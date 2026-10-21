mod integration;
mod rpc;

use anyhow::{Context, Result, bail, ensure};
use dial::{
    benchmark::{
        AssistanceOverrides, BenchmarkProfile, BenchmarkReport, BenchmarkSuite, CommandCheck,
        EfficiencyOverrides, MachineBenchmarkPolicy, MachineBenchmarkReport, MachineConfiguration,
        MachineConfigurationReport, MachineIdentity, PolicyOverrides, RunMetrics, clone_project,
        compare_baseline, named_invalid_runs, rank_machine_configurations, score_report,
    },
    config::paths::Layout,
    config::types::{Approval, Config, EditMode, LEAN_AUTO_MAX_WINDOW, LeanLocal, SandboxMode},
    runtime::tool_output::ToolOutput,
    tools::{self, Workspace},
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profiles {
    #[serde(default)]
    profiles: Vec<BenchmarkProfile>,
    #[serde(default)]
    configurations: Vec<MachineConfiguration>,
    #[serde(default)]
    machine: MachineBenchmarkPolicy,
}

pub fn run(
    layout: &Layout,
    config: &Config,
    manifest: &Path,
    baseline: Option<&Path>,
    report_path: Option<&Path>,
    profiles_path: Option<&Path>,
    checkpoint_path: Option<&Path>,
    resume: bool,
) -> Result<()> {
    ensure!(
        checkpoint_path.is_none() || profiles_path.is_none(),
        "--checkpoint currently records one configured run; select a model profile through config"
    );
    if profiles_path.is_none() {
        config.require_model()?;
    }
    let manifest = if manifest.is_absolute() {
        manifest.to_owned()
    } else {
        layout.project_root.join(manifest)
    };
    let source = fs::read_to_string(&manifest)
        .with_context(|| format!("reading benchmark suite {}", manifest.display()))?;
    let suite = BenchmarkSuite::parse_toml(&source)?;
    ensure!(
        !suite
            .cases
            .iter()
            .any(|case| case.categories.iter().any(|tag| tag == "integration"))
            || profiles_path.is_some(),
        "integration evaluation unavailable: explicit machine configuration with integration mode is required (--profiles)"
    );
    let user_home = tempfile::tempdir().context("creating isolated benchmark state")?;
    prepare_user_home(layout, config, &user_home)?;
    let executable = std::env::current_exe().context("locating dial executable")?;
    let bench_config = isolated_config(config);

    let mut regressed = false;
    // Runs that never measured a model: the report is still written, then the command fails.
    let mut invalid = Vec::new();
    let payload = if let Some(path) = profiles_path {
        ensure!(
            baseline.is_none(),
            "--baseline applies to a single report, not --profiles"
        );
        let text = fs::read_to_string(resolve_output(layout, path))?;
        let profiles: Profiles = toml::from_str(&text).context("parsing benchmark profiles")?;
        ensure!(
            !suite
                .cases
                .iter()
                .any(|case| case.categories.iter().any(|tag| tag == "integration"))
                || !profiles.configurations.is_empty(),
            "integration evaluation unavailable: explicit machine configurations are required, not legacy named profiles"
        );
        ensure!(
            profiles.profiles.is_empty() || profiles.configurations.is_empty(),
            "use profiles or configurations, not both"
        );
        if !profiles.configurations.is_empty() {
            let report = run_machine(
                layout,
                config,
                &suite,
                &source,
                profiles.configurations,
                profiles.machine,
                &executable,
            )?;
            regressed = report.recommended_configuration.is_none();
            invalid.extend(report.invalid_runs.iter().cloned());
            json!({"machine_benchmark": report})
        } else {
            let frozen = clone_project(&layout.project_root)?;
            let mut contracts = std::collections::BTreeMap::new();
            let mut comparison = suite.run_profiles(
                frozen.path(),
                &profiles.profiles,
                |profile, case, workspace| {
                    let mut selected = bench_config.clone();
                    ensure!(
                        selected.model_profiles.contains_key(&profile.name),
                        "unknown configured model profile {:?}",
                        profile.name
                    );
                    let configured = selected.model_profiles[&profile.name].clone();
                    if configured.provider != selected.selection().provider {
                        if configured.provider == "mlx" {
                            selected.endpoint = None;
                        } else {
                            let provider = selected
                                .providers
                                .get(&configured.provider)
                                .context("benchmark profile provider is not configured")?;
                            selected.endpoint = Some(dial::config::types::EndpointConfig {
                                headers: provider.headers.clone(),
                                base_url: provider.base_url.clone(),
                                api_key_env: provider.api_key_env.clone(),
                                allow_insecure: provider.allow_insecure,
                                protocol: provider.protocol,
                                account_id: None,
                            });
                        }
                    }
                    selected.provider = Some(configured.provider.clone());
                    selected.model = configured.model;
                    selected.model_profile = Some(profile.name.clone());
                    for ablation in &profile.ablations {
                        match ablation.as_str() {
                            "no_integrations" => {
                                selected.integrations.clear();
                                selected.mcp.clear();
                                selected.dap.clear();
                            }
                            "replace_edit" => {
                                selected.edit.mode = dial::config::types::EditMode::Replace;
                                selected
                                    .model_profiles
                                    .get_mut(&profile.name)
                                    .unwrap()
                                    .edit_mode = Some(selected.edit.mode);
                            }
                            "hashline_edit" => {
                                selected.edit.mode = dial::config::types::EditMode::Hashline;
                                selected
                                    .model_profiles
                                    .get_mut(&profile.name)
                                    .unwrap()
                                    .edit_mode = Some(selected.edit.mode);
                            }
                            _ => bail!("unknown benchmark ablation {ablation:?}"),
                        }
                    }
                    selected.validate()?;
                    if !contracts.contains_key(&profile.name) {
                        contracts.insert(
                            profile.name.clone(),
                            dial::benchmark::records::Contract::new(
                                &suite,
                                frozen.path(),
                                &serde_json::to_value(&selected)?,
                            )?,
                        );
                    }
                    let isolated = tempfile::tempdir()?;
                    prepare_user_home(layout, &selected, &isolated)?;
                    run_agent(&executable, isolated.path(), &selected, case, workspace)
                },
                |check, workspace| run_command_check(&user_home, &bench_config, workspace, check),
            )?;
            for profile in &mut comparison.profiles {
                profile.report.contract = contracts.remove(&profile.profile.name);
            }
            invalid.extend(comparison.invalid_runs.iter().cloned());
            json!({"profile_comparison": comparison})
        }
    } else {
        let frozen_source = clone_project(&layout.project_root)?;
        let contract = dial::benchmark::records::Contract::new(
            &suite,
            frozen_source.path(),
            &serde_json::to_value(&bench_config)?,
        )?;
        let mut recorder = checkpoint_path
            .map(|path| {
                let path = resolve_output(layout, path);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                dial::benchmark::records::Recorder::open(&path, contract.clone(), resume)
            })
            .transpose()?;
        let mut report = suite.run_recorded(
            frozen_source.path(),
            |case, workspace| {
                run_agent(
                    &executable,
                    user_home.path(),
                    &bench_config,
                    case,
                    workspace,
                )
            },
            |check, workspace| run_command_check(&user_home, &bench_config, workspace, check),
            recorder.as_mut(),
        )?;
        report.contract = Some(contract);
        invalid.extend(report.invalid_run.iter().cloned());
        let comparison = if let Some(path) = baseline {
            let bytes = fs::read(resolve_output(layout, path))?;
            let prior: serde_json::Value = serde_json::from_slice(&bytes)?;
            let prior: BenchmarkReport =
                serde_json::from_value(prior.get("report").cloned().unwrap_or(prior))?;
            if let Some(reason) = prior.invalid_reason() {
                invalid.push(format!(
                    "baseline {} is not comparable: {reason}",
                    path.display()
                ));
            }
            Some(compare_baseline(&report, &prior))
        } else {
            None
        };
        regressed = comparison.is_some_and(|c| c.regressed);
        json!({"report": report, "comparison": comparison})
    };
    let bytes = serde_json::to_vec_pretty(&payload)?;
    if let Some(path) = report_path {
        let path = resolve_output(layout, path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_private(&path, &bytes)?;
        println!("{}", path.display());
    } else {
        println!("{}", String::from_utf8(bytes)?);
    }
    if !invalid.is_empty() {
        return Err(invalid_run_error(layout, &invalid));
    }
    if regressed {
        bail!(
            "benchmark regressed or no machine configuration met the correctness/regression floor"
        );
    }
    Ok(())
}

/// Subscription sign-ins hold rotating refresh tokens, so isolated benchmark homes never copy them.
const UNCOPIED_SIGN_INS: [&str; 2] = ["anthropic-subscription.json", "chatgpt-accounts.json"];

fn invalid_run_error(layout: &Layout, reasons: &[String]) -> anyhow::Error {
    let mut message = format!(
        "benchmark run is invalid; the report was still emitted but its scores are not measurements:\n  {}",
        reasons.join("\n  ")
    );
    if reasons
        .iter()
        .any(|reason| reason.contains("credentials are missing"))
        && UNCOPIED_SIGN_INS
            .iter()
            .any(|file| layout.user_dir.join(file).is_file())
    {
        message.push_str(
            "\nhint: benchmark agents run in an isolated DIAL_HOME that receives only config.toml and credentials.json; subscription sign-ins are not copied because refreshing them there could rotate the real login. Store a provider API key (/providers) or export the provider's API key environment variable.",
        );
    }
    anyhow::anyhow!(message)
}

/// Isolation shared by suite, profile and machine runs: no improvement, approvals, network, or user tools.
fn isolated_config(config: &Config) -> Config {
    let mut bench_config = config.clone();
    bench_config.improve.enabled = false;
    bench_config.solve.enabled = false;
    bench_config.escalation.enabled = false;
    bench_config.approval = Approval::Auto;
    bench_config.sandbox.mode = SandboxMode::Dev;
    bench_config.sandbox.allow_network = false;
    bench_config.sandbox.extra_write.clear();
    bench_config
        .integrations
        .retain(|name| matches!(name.as_str(), "ast_search" | "ast_edit"));
    bench_config.mcp.clear();
    bench_config.tools.clear();
    bench_config.subagents.clear();
    bench_config.verification.commands.clear();
    bench_config.verification.behavioral_checks.clear();
    bench_config.verification.requirements.clear();
    bench_config.verification.runners.clear();
    bench_config
}

const ASSIST_CASE_TIMEOUT_SECS: u64 = 600;

/// One single-feature variant of the shipped `dial` baseline. `enable-*` turns on a feature that
/// ships off; `ablate-*` turns off or changes a feature that ships on. Either way exactly one
/// lever differs from `dial`.
type FeatureVariant = fn(&mut AssistanceOverrides, EditMode);
const ASSIST_VARIANTS: [(&str, FeatureVariant); 8] = [
    ("enable-compact_tools", |o, _| o.compact_tools = Some(true)),
    ("enable-repair_hints", |o, _| o.repair_hints = Some(true)),
    // 0 means unlimited: the initial tool budget is removed.
    ("ablate-initial_tool_limit", |o, _| {
        o.initial_tool_limit = Some(0)
    }),
    ("ablate-tool_grammar", |o, _| o.tool_grammar = Some(false)),
    ("ablate-edit_mode", |o, shipped| {
        o.edit_mode = Some(if shipped == EditMode::Replace {
            EditMode::Hashline
        } else {
            EditMode::Replace
        })
    }),
    ("ablate-reliability_gate", |o, _| {
        o.reliability_gate = Some(false)
    }),
    ("ablate-lean_local", |o, _| {
        o.lean_local = Some(LeanLocal::Off)
    }),
    ("ablate-local_profile", |o, _| o.local_profile = Some(false)),
];

/// The shipped assistance and context-economy values, read from `Config` rather than restated, so
/// the `dial` baseline cannot drift from what an untouched config runs.
fn shipped_overrides(shipped: &Config) -> (AssistanceOverrides, EfficiencyOverrides) {
    let assistance = &shipped.assistance;
    (
        AssistanceOverrides {
            enabled: Some(assistance.enabled),
            compact_tools: Some(assistance.compact_tools),
            initial_tool_limit: Some(assistance.initial_tool_limit),
            tool_grammar: Some(assistance.tool_grammar),
            repair_hints: Some(assistance.repair_hints),
            syntax_guard: Some(assistance.syntax_guard),
            selective_verification: Some(assistance.selective_verification),
            edit_mode: Some(shipped.edit.mode),
            reliability_gate: Some(shipped.reliability.enabled),
            constrained_repair: Some(assistance.constrained_repair),
            lean_local: Some(assistance.lean_local),
            local_profile: Some(assistance.local.enabled),
        },
        EfficiencyOverrides {
            tool_preview_bytes: Some(shipped.efficiency.tool_preview_bytes),
            history_token_cap: Some(shipped.efficiency.history_token_cap),
            compaction_enabled: Some(shipped.compaction.enabled),
            keep_recent_tokens: Some(shipped.compaction.keep_recent_tokens),
        },
    )
}

/// `raw` turns assistance off, `dial` is the shipped defaults (not the user's config), `lean` turns on
/// the lean local profile that ships on `auto`, and each variant changes exactly one lever from `dial`.
fn assist_configurations(
    config: &Config,
    provider: &str,
    model: &str,
) -> Vec<MachineConfiguration> {
    let shipped = Config::default();
    let default_edit = shipped.edit.mode;
    let (dial, efficiency) = shipped_overrides(&shipped);
    let raw = AssistanceOverrides {
        enabled: Some(false),
        compact_tools: Some(false),
        initial_tool_limit: Some(0),
        tool_grammar: Some(false),
        repair_hints: Some(false),
        syntax_guard: Some(false),
        selective_verification: Some(false),
        edit_mode: Some(default_edit),
        reliability_gate: Some(false),
        constrained_repair: Some(false),
        lean_local: Some(LeanLocal::Off),
        local_profile: Some(false),
    };
    let machine = |name: String, assistance: AssistanceOverrides| MachineConfiguration {
        name,
        provider: provider.to_owned(),
        model: model.to_owned(),
        context_window: config.context_window,
        quantization_bits: None,
        adapter: None,
        assistance,
        efficiency: efficiency.clone(),
        policy: shipped_policy(&shipped),
        integration: Default::default(),
        draft_model: None,
        num_draft_tokens: None,
        kv_bits: None,
        kv_group_size: None,
        quantized_kv_start: None,
    };
    let mut configurations = vec![
        machine("raw".into(), raw),
        machine("dial".into(), dial.clone()),
    ];
    for (name, variant) in ASSIST_VARIANTS {
        let mut overrides = dial.clone();
        variant(&mut overrides, default_edit);
        configurations.push(machine(name.into(), overrides));
    }
    for (name, strict) in [
        ("enable-strict_requirements", true),
        ("enable-independent_review", false),
    ] {
        let mut variant = machine(name.into(), dial.clone());
        if strict {
            variant.policy.strict_requirements = Some(true);
        } else {
            variant.policy.review_enabled = Some(true);
        }
        configurations.push(variant);
    }
    configurations
}

/// Machine benchmark over `raw`, `dial`, `lean` and one ablation per assistance feature.
pub fn run_assist(
    layout: &Layout,
    config: &Config,
    manifest: &Path,
    report_path: Option<&Path>,
    allow_remote: bool,
) -> Result<()> {
    config.require_model()?;
    let manifest = if manifest.is_absolute() {
        manifest.to_owned()
    } else {
        layout.project_root.join(manifest)
    };
    let source = fs::read_to_string(&manifest)
        .with_context(|| format!("reading benchmark suite {}", manifest.display()))?;
    let suite = BenchmarkSuite::parse_toml(&source)?;
    let selection = config.selection();
    let configurations = assist_configurations(config, &selection.provider, &selection.model);
    let policy = MachineBenchmarkPolicy {
        case_timeout_secs: ASSIST_CASE_TIMEOUT_SECS,
        minimum_correctness: 0.0,
        allow_remote,
        ..MachineBenchmarkPolicy::default()
    };
    let executable = std::env::current_exe().context("locating dial executable")?;
    let report = run_machine(
        layout,
        &isolated_config(config),
        &suite,
        &source,
        configurations,
        policy,
        &executable,
    )?;
    print_assist_table(&report);
    let bytes = serde_json::to_vec_pretty(&json!({"assistance_ablation": report}))?;
    if let Some(path) = report_path {
        let path = resolve_output(layout, path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_private(&path, &bytes)?;
        println!("{}", path.display());
    } else {
        println!("{}", String::from_utf8(bytes)?);
    }
    if !report.invalid_runs.is_empty() {
        return Err(invalid_run_error(layout, &report.invalid_runs));
    }
    Ok(())
}

/// Per-configuration measurements shown in the ablation table.
struct AssistRow {
    correct_pct: f64,
    tool_errors: u64,
    malformed: u64,
    repairs: Option<u64>,
    tokens: Option<u64>,
    wall_secs: f64,
}

impl AssistRow {
    fn of(row: &MachineConfigurationReport) -> Self {
        Self {
            correct_pct: row.report.pass_rate * 100.0,
            tool_errors: row.report.total_tool_errors,
            malformed: row
                .report
                .cases
                .iter()
                .map(|case| case.metrics.tool_contract_errors)
                .sum(),
            repairs: row.repair_attempts,
            tokens: row
                .observed_prompt_tokens
                .zip(row.observed_output_tokens)
                .map(|(prompt, output)| prompt.saturating_add(output)),
            wall_secs: row.wall_time_ms as f64 / 1000.0,
        }
    }
}

fn print_assist_table(report: &MachineBenchmarkReport) {
    let opt = |value: Option<u64>| value.map_or("-".to_owned(), |v| v.to_string());
    let dial = report
        .configurations
        .iter()
        .find(|row| row.configuration.name == "dial")
        .map(AssistRow::of);
    println!(
        "assistance ablation: suite {:?}, {} cases per configuration; delta = configuration - dial",
        report.suite,
        report
            .configurations
            .first()
            .map_or(0, |row| row.report.cases.len())
    );
    println!(
        "{:<28} {:>8} {:>9} {:>10} {:>8} {:>10} {:>8} | {:>9} {:>9} {:>10} {:>8}",
        "configuration",
        "correct%",
        "tool_err",
        "malformed",
        "repairs",
        "tokens",
        "wall_s",
        "d_correct",
        "d_toolerr",
        "d_tokens",
        "d_wall_s"
    );
    // Machine reports rank configurations; the ablation reads best as raw, dial, then each ablation.
    let mut ordered: Vec<_> = report.configurations.iter().collect();
    ordered.sort_by_key(|row| match row.configuration.name.as_str() {
        "raw" => (0, ""),
        "dial" => (1, ""),
        name => (2, name),
    });
    for configuration in ordered {
        let row = AssistRow::of(configuration);
        let (d_correct, d_tool, d_tokens, d_wall) = match &dial {
            Some(base) => (
                format!("{:+.1}", row.correct_pct - base.correct_pct),
                format!("{:+}", row.tool_errors as i128 - base.tool_errors as i128),
                match (row.tokens, base.tokens) {
                    (Some(a), Some(b)) => format!("{:+}", a as i128 - b as i128),
                    _ => "-".to_owned(),
                },
                format!("{:+.1}", row.wall_secs - base.wall_secs),
            ),
            None => ("-".into(), "-".into(), "-".into(), "-".into()),
        };
        println!(
            "{:<28} {:>8.1} {:>9} {:>10} {:>8} {:>10} {:>8.1} | {:>9} {:>9} {:>10} {:>8}",
            configuration.configuration.name,
            row.correct_pct,
            row.tool_errors,
            row.malformed,
            opt(row.repairs),
            opt(row.tokens),
            row.wall_secs,
            d_correct,
            d_tool,
            d_tokens,
            d_wall
        );
    }
}

fn resolve_output(layout: &Layout, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        layout.project_root.join(path)
    }
}

fn prepare_user_home(layout: &Layout, config: &Config, user_home: &TempDir) -> Result<()> {
    prepare_integration_home(layout, config, user_home, &Default::default())
}

fn prepare_integration_home(
    layout: &Layout,
    config: &Config,
    user_home: &TempDir,
    lane: &dial::benchmark::IntegrationOverrides,
) -> Result<()> {
    let isolated = integration::isolate(config, lane)?;
    write_benchmark_home(layout, &isolated, user_home)
}

fn write_benchmark_home(layout: &Layout, isolated: &Config, user_home: &TempDir) -> Result<()> {
    let contents = toml::to_string(isolated).context("serializing benchmark config")?;
    write_private(&user_home.path().join("config.toml"), contents.as_bytes())?;
    copy_private_if_present(
        &layout.user_dir.join("credentials.json"),
        &user_home.path().join("credentials.json"),
    )?;
    Ok(())
}

/// Apply identical suite-owned evidence mappings in strict and non-strict machine variants.
/// Final-only assertions cannot be required of every intermediate request in a journey.
fn case_contract(config: &Config, case: &dial::benchmark::BenchmarkCase) -> Result<Config> {
    let mut selected = config.clone();
    if case.turns.is_empty() {
        for (index, assertion) in case.checks.iter().enumerate() {
            if let dial::benchmark::Assertion::Command {
                command,
                expect_exit: 0,
                expect_stdout: None,
            } = assertion
            {
                let name = format!("suite_check_{}", index + 1);
                selected
                    .verification
                    .commands
                    .push(dial::config::types::VerificationCommand {
                        name: name.clone(),
                        command: command.clone(),
                        file_extensions: vec![],
                        timeout_secs: 120,
                    });
                selected.verification.requirements.insert(
                    name.clone(),
                    dial::config::types::VerificationRequirement {
                        checks: vec![name],
                        visual: false,
                    },
                );
            }
        }
    }
    if selected.verification.strict_requirements && selected.verification.requirements.is_empty() {
        return Err(dial::benchmark::AgentUnavailable("strict-contract evaluation unavailable: requires single-turn zero-exit command assertions; journey final assertions are not intermediate obligations".into()).into());
    }
    selected.validate()?;
    Ok(selected)
}

fn run_agent(
    executable: &Path,
    user_home: &Path,
    _config: &Config,
    case: &dial::benchmark::BenchmarkCase,
    workspace: &Path,
) -> Result<RunMetrics> {
    rpc::run(executable, user_home, case, workspace)
}

fn run_command_check(
    user_home: &TempDir,
    config: &Config,
    workspace: &Path,
    check: &CommandCheck<'_>,
) -> Result<bool> {
    let layout = Layout::with_user_dir(workspace, user_home.path().to_owned())?;
    let ws = Workspace::new(workspace)?;
    let ws = tools::apply_sandbox_config(
        ws,
        &config.sandbox,
        Some((&layout.state_dir, &layout.user_dir)),
    )?
    .with_shell_backend(config.shell.backend);
    let output: ToolOutput = tools::execute(
        &ws,
        "bash",
        &json!({"command":check.command,"timeout":120}),
        120,
    );
    let process = output
        .process_outcome()
        .context("command check did not return typed process termination")?;
    ensure!(
        process.started && !process.cancelled && !process.timed_out && process.output_complete,
        "command check has incomplete execution or capture"
    );
    let stdout = output
        .text
        .rsplit_once("\n[exit code: ")
        .map_or(output.text.as_str(), |(stdout, _)| stdout)
        .trim();
    Ok(process.exit_code == Some(check.expect_exit)
        && check
            .expect_stdout
            .is_none_or(|expected| stdout == expected.trim()))
}

fn private_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    dial::platform::fs::open_owner_only(&options, path)
        .with_context(|| format!("creating {}", path.display()))
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = private_file(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn copy_private_if_present(source: &Path, target: &Path) -> Result<()> {
    match fs::read(source) {
        Ok(bytes) => write_private(target, &bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("reading {}", source.display())),
    }
}

fn machine_identity() -> MachineIdentity {
    // A local OS query; no network discovery or hardware fitness inference.
    let hostname = dial::platform::sys::hostname().unwrap_or_else(|_| "unavailable".into());
    MachineIdentity {
        hostname,
        operating_system: std::env::consts::OS.into(),
        architecture: std::env::consts::ARCH.into(),
        logical_cpus: std::thread::available_parallelism().ok().map(usize::from),
        dial_version: env!("CARGO_PKG_VERSION").into(),
    }
}

fn bounded_json(path: &Path) -> Result<serde_json::Value> {
    use std::io::Read;
    let mut bytes = Vec::new();
    File::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "model/adapter metadata exceeds 1 MiB"
    );
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

/// Applies per-configuration ablation switches on top of the user's config; unset fields keep it.
fn apply_assistance(config: &mut Config, overrides: &AssistanceOverrides) {
    if let Some(value) = overrides.enabled {
        config.assistance.enabled = value;
    }
    if let Some(value) = overrides.compact_tools {
        config.assistance.compact_tools = value;
    }
    if let Some(value) = overrides.initial_tool_limit {
        config.assistance.initial_tool_limit = value;
    }
    if let Some(value) = overrides.tool_grammar {
        config.assistance.tool_grammar = value;
    }
    if let Some(value) = overrides.repair_hints {
        config.assistance.repair_hints = value;
    }
    if let Some(value) = overrides.syntax_guard {
        config.assistance.syntax_guard = value;
    }
    if let Some(value) = overrides.selective_verification {
        config.assistance.selective_verification = value;
    }
    if let Some(mode) = overrides.edit_mode {
        config.edit.mode = mode;
        config.harness.edit_mode = Some(mode);
    }
    if let Some(value) = overrides.reliability_gate {
        config.reliability.enabled = value;
    }
    if let Some(value) = overrides.constrained_repair {
        config.assistance.constrained_repair = value;
    }
    if let Some(value) = overrides.lean_local {
        config.assistance.lean_local = value;
    }
    if let Some(value) = overrides.local_profile {
        config.assistance.local.enabled = value;
    }
}

/// Applies per-configuration context-economy overrides on top of the user's config; unset fields keep it.
fn apply_efficiency(config: &mut Config, overrides: &EfficiencyOverrides) {
    if let Some(bytes) = overrides.tool_preview_bytes {
        config.efficiency.tool_preview_bytes = bytes;
    }
    if let Some(cap) = overrides.history_token_cap {
        config.efficiency.history_token_cap = cap;
    }
    if let Some(enabled) = overrides.compaction_enabled {
        config.compaction.enabled = enabled;
    }
    if let Some(tokens) = overrides.keep_recent_tokens {
        config.compaction.keep_recent_tokens = tokens;
    }
}

/// Use the shared config serializer/validator as the authority for policy semantics. This overlay
/// also keeps the benchmark independent of the concrete review type's module location.
fn apply_policy(config: &mut Config, overrides: &PolicyOverrides) -> Result<()> {
    overrides.validate()?;
    let mut value = serde_json::to_value(&config.verification)?;
    let verification = value
        .as_object_mut()
        .context("verification config object")?;
    if let Some(strict) = overrides.strict_requirements {
        verification.insert("strict_requirements".into(), json!(strict));
    }
    if overrides.review_enabled.is_some()
        || overrides.review_min_changed_files.is_some()
        || overrides.review_sensitive_paths.is_some()
        || overrides.review_max_rounds.is_some()
    {
        let review = verification.entry("review").or_insert_with(|| json!({}));
        let review = review.as_object_mut().context("review config object")?;
        for (key, value) in [
            ("enabled", overrides.review_enabled.map(|v| json!(v))),
            (
                "min_changed_files",
                overrides.review_min_changed_files.map(|v| json!(v)),
            ),
            (
                "sensitive_paths",
                overrides.review_sensitive_paths.as_ref().map(|v| json!(v)),
            ),
            ("max_rounds", overrides.review_max_rounds.map(|v| json!(v))),
        ] {
            if let Some(value) = value {
                review.insert(key.into(), value);
            }
        }
    }
    config.verification =
        serde_json::from_value(value).context("applying benchmark policy overrides")?;
    Ok(())
}

fn shipped_policy(config: &Config) -> PolicyOverrides {
    let v = serde_json::to_value(&config.verification).expect("serializable config");
    PolicyOverrides {
        strict_requirements: Some(v["strict_requirements"].as_bool().unwrap_or(false)),
        review_enabled: Some(v["review"]["enabled"].as_bool().unwrap_or(false)),
        review_min_changed_files: Some(
            v["review"]["min_changed_files"].as_u64().unwrap_or(4) as usize
        ),
        review_sensitive_paths: Some(
            serde_json::from_value(v["review"]["sensitive_paths"].clone()).unwrap_or_default(),
        ),
        review_max_rounds: Some(v["review"]["max_rounds"].as_u64().unwrap_or(1) as u8),
    }
}

/// Per-configuration MLX runtime overrides; unset fields keep the user's `local_runtime`.
fn apply_local_runtime(
    config: &mut Config,
    requested: &MachineConfiguration,
) -> anyhow::Result<()> {
    let overridden = requested.draft_model.is_some()
        || requested.num_draft_tokens.is_some()
        || requested.kv_bits.is_some()
        || requested.kv_group_size.is_some()
        || requested.quantized_kv_start.is_some();
    anyhow::ensure!(
        !overridden || requested.provider == "mlx",
        "configuration {} sets local runtime overrides; they apply only to the mlx provider",
        requested.name
    );
    if let Some(draft) = &requested.draft_model {
        config.local_runtime.draft_model = Some(draft.clone());
    }
    if let Some(value) = requested.num_draft_tokens {
        config.local_runtime.num_draft_tokens = Some(value);
    }
    if let Some(value) = requested.kv_bits {
        config.local_runtime.kv_bits = Some(value);
    }
    if let Some(value) = requested.kv_group_size {
        config.local_runtime.kv_group_size = Some(value);
    }
    if let Some(value) = requested.quantized_kv_start {
        config.local_runtime.quantized_kv_start = Some(value);
    }
    config.local_runtime.validate()
}

/// Effective feature values recorded per configuration so reports are self-describing.
fn assistance_settings(config: &Config) -> serde_json::Value {
    json!({
        "enabled": config.assistance.enabled,
        "compact_tools": config.assistance.compact_tools,
        "initial_tool_limit": config.assistance.initial_tool_limit,
        "tool_grammar": config.assistance.tool_grammar,
        "repair_hints": config.assistance.repair_hints,
        "syntax_guard": config.assistance.syntax_guard,
        "selective_verification": config.assistance.selective_verification,
        "edit_mode": config.edit.mode,
        "reliability_gate": config.reliability.enabled,
        "constrained_repair": config.assistance.constrained_repair,
        "lean_local": config.assistance.lean_local,
        "local_profile": config.assistance.local.enabled,
        "lean_local_within_auto_window": config.context_window <= LEAN_AUTO_MAX_WINDOW,
    })
}

/// Effective context-economy values recorded per configuration, after overrides and defaults.
fn efficiency_settings(config: &Config) -> serde_json::Value {
    json!({
        "tool_preview_bytes": config.efficiency.tool_preview_bytes,
        "history_token_cap": config.efficiency.history_token_cap,
        "compaction_enabled": config.compaction.enabled,
        "keep_recent_tokens": config.compaction.keep_recent_tokens,
    })
}

fn select_configuration(
    layout: &Layout,
    base: &Config,
    requested: &mut MachineConfiguration,
    policy: &MachineBenchmarkPolicy,
) -> Result<(Config, serde_json::Value)> {
    let mut selected = integration::isolate(base, &requested.integration)?;
    selected.model_profile = None;
    selected.model_profiles.clear();
    selected.model_roles.clear();
    selected.provider = Some(requested.provider.clone());
    selected.model = requested.model.clone();
    selected.context_window = requested.context_window;
    selected.worker_timeout_secs = policy.case_timeout_secs;
    selected.harness.max_steps = selected.harness.capped_steps(24);
    apply_assistance(&mut selected, &requested.assistance);
    apply_efficiency(&mut selected, &requested.efficiency);
    apply_policy(&mut selected, &requested.policy)?;
    apply_local_runtime(&mut selected, requested)?;
    let mut quantization = serde_json::Value::Null;
    let mut adapter_metadata = serde_json::Value::Null;
    if requested.provider == "mlx" {
        selected.endpoint = None;
        // Never download or guess a model from its name during a machine benchmark.
        let model = fs::canonicalize(resolve_output(layout, Path::new(&requested.model)))
            .context("machine benchmark requires an existing local model directory")?;
        ensure!(
            model.is_dir(),
            "machine benchmark model must be a directory"
        );
        let metadata = bounded_json(&model.join("config.json"))?;
        quantization = metadata
            .get("quantization")
            .or_else(|| metadata.get("quantization_config"))
            .cloned()
            .unwrap_or_default();
        if let Some(bits) = requested.quantization_bits {
            ensure!(
                quantization["bits"].as_u64() == Some(u64::from(bits)),
                "requested quantization_bits do not match local model metadata"
            );
        }
        if let Some(path) = &requested.adapter {
            let adapter = fs::canonicalize(resolve_output(layout, path))
                .context("resolving benchmark adapter")?;
            ensure!(
                adapter.is_dir() && adapter.join("adapters.safetensors").is_file(),
                "benchmark adapter needs adapters.safetensors"
            );
            adapter_metadata = bounded_json(&adapter.join("adapter_config.json"))?;
            let identity = adapter_metadata
                .get("model")
                .or_else(|| adapter_metadata.get("model_name_or_path"))
                .and_then(serde_json::Value::as_str)
                .context("adapter metadata must declare model or model_name_or_path")?;
            let matches = identity == requested.model
                || fs::canonicalize(resolve_output(layout, Path::new(identity)))
                    .is_ok_and(|path| path == model);
            ensure!(
                matches,
                "adapter base model does not match benchmark configuration"
            );
            requested.adapter = Some(adapter);
        }
        requested.model = model.to_string_lossy().into_owned();
        selected.model = requested.model.clone();
    } else {
        ensure!(
            requested.adapter.is_none() && requested.quantization_bits.is_none(),
            "adapter and quantization_bits require a local MLX model with verifiable metadata; endpoint model IDs may name already-quantized variants"
        );
        // A named provider entry wins; otherwise the active session's own endpoint serves its provider,
        // so `--ablate-assistance` works with a plain `[endpoint]` configuration.
        let endpoint = match selected.providers.get(&requested.provider) {
            Some(provider) => dial::config::types::EndpointConfig {
                headers: provider.headers.clone(),
                base_url: provider.base_url.clone(),
                api_key_env: provider.api_key_env.clone(),
                allow_insecure: provider.allow_insecure,
                protocol: provider.protocol,
                account_id: None,
            },
            None if base.provider.as_deref() == Some(requested.provider.as_str()) => base
                .endpoint
                .clone()
                .context("machine benchmark provider is not configured")?,
            None => anyhow::bail!("machine benchmark provider is not configured"),
        };
        dial::llm::provider::endpoint_url(&endpoint.base_url, endpoint.allow_insecure)?;
        let uri: ureq::http::Uri = endpoint.base_url.parse()?;
        let host = uri.host().unwrap_or("").trim_matches(['[', ']']);
        let loopback = host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        ensure!(
            policy.allow_remote || loopback,
            "remote benchmark inference requires [machine] allow_remote = true"
        );
        selected.endpoint = Some(endpoint);
    }
    if selected.verification.review.enabled {
        selected.escalation.allow_remote = policy.allow_remote;
    }
    integration::configure_review(&mut selected);
    selected.validate()?;
    let settings = json!({
        "context_window": selected.context_window,
        "context_window_semantics": "configured harness budget; runtime may cap to loaded model context",
        "endpoint": selected.endpoint.as_ref().map(|endpoint| &endpoint.base_url),
        "quantization_metadata": quantization,
        "adapter_metadata": adapter_metadata,
        "max_steps": selected.harness.max_steps,
        "max_tokens": selected.harness.max_tokens,
        "temperature": selected.harness.temperature,
        "verification": selected.verification,
        "case_timeout_secs": policy.case_timeout_secs,
        "tool_network_allowed": selected.sandbox.allow_network,
        "integration": requested.integration,
        "automatic_improvement": false,
        "model_roles": if selected.verification.review.enabled { "coach:selected_model_only" } else { "disabled" },
        "assistance": assistance_settings(&selected),
        "efficiency": efficiency_settings(&selected),
        "policy_contract_version": 1,
        "policy": shipped_policy(&selected),
        "policy_sha256": format!("{:x}", Sha256::digest(serde_json::to_vec(&shipped_policy(&selected))?)),
        "case_requirement_mapping": "single_turn_zero_exit_commands_v1",
        "local_runtime": selected.local_runtime,
    });
    Ok((selected, settings))
}

fn run_machine(
    layout: &Layout,
    base: &Config,
    suite: &BenchmarkSuite,
    source: &str,
    mut configurations: Vec<MachineConfiguration>,
    policy: MachineBenchmarkPolicy,
    executable: &Path,
) -> Result<MachineBenchmarkReport> {
    policy.validate(suite, &configurations)?;
    // Preflight every entry before inference. Misspellings must not become failed task scores.
    let selected = configurations
        .iter_mut()
        .map(|configuration| select_configuration(layout, base, configuration, &policy))
        .collect::<Result<Vec<_>>>()?;
    let baseline_configuration = configurations[0].name.clone();
    // All configurations and repetitions see exactly one snapshot of the source tree.
    let snapshot = clone_project(&layout.project_root)?;
    let mut reports = Vec::with_capacity(configurations.len());
    for (configuration, (selected, settings)) in configurations.into_iter().zip(selected) {
        let contract = dial::benchmark::records::Contract::new(
            suite,
            snapshot.path(),
            &json!({"config": selected, "integration": configuration.integration, "policy_contract_version": 1}),
        )?;
        let started = std::time::Instant::now();
        let mut cases = Vec::with_capacity(suite.cases.len() * policy.repetitions);
        for repetition in 0..policy.repetitions {
            let check_home = tempfile::tempdir()?;
            prepare_user_home(layout, &selected, &check_home)?;
            let report = suite.run(
                snapshot.path(),
                |case, workspace| {
                    // No session, provenance memory, adapter state, or learned configuration leaks
                    // from another case. The executable/python paths still reference the installation.
                    let home = tempfile::tempdir()?;
                    let expected = match configuration.integration.mode {
                        dial::benchmark::IntegrationMode::Core => "core",
                        dial::benchmark::IntegrationMode::Mcp => "mcp",
                        dial::benchmark::IntegrationMode::Browser => "browser",
                        dial::benchmark::IntegrationMode::Delegation => "delegation",
                    };
                    if case.categories.iter().any(|tag| tag == "integration")
                        && (expected == "core" || !case.categories.iter().any(|tag| tag == expected)) {
                        return Err(dial::benchmark::AgentUnavailable("integration unavailable: case requires its explicit matching integration mode".into()).into());
                    }
                    integration::prerequisite(&selected, &configuration.integration, &layout.project_root)?;
                    let child = case_contract(&integration::isolate(&selected, &configuration.integration)?, case)?;
                    write_benchmark_home(layout, &child, &home)?;
                    rpc::run_configured(
                        executable,
                        home.path(),
                        case,
                        workspace,
                        policy.case_timeout_secs,
                        configuration.adapter.as_deref(),
                    )
                },
                |check, workspace| run_command_check(&check_home, &isolated_config(&selected), workspace, check),
            )?;
            cases.extend(report.cases.into_iter().map(|mut case| {
                case.id = format!("{repetition}:{}", case.id);
                case
            }));
        }
        let wall_time_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        let mut aggregate = score_report(&suite.name, cases);
        aggregate.contract = Some(contract);
        reports.push(MachineConfigurationReport::measured(
            configuration,
            settings,
            aggregate,
            wall_time_ms,
        ));
    }
    let recommended_configuration = rank_machine_configurations(&mut reports, &policy)?;
    let invalid_runs = named_invalid_runs(
        reports
            .iter()
            .map(|report| (report.configuration.name.as_str(), &report.report)),
    );
    Ok(MachineBenchmarkReport {
        machine: machine_identity(),
        suite: suite.name.clone(),
        suite_sha256: format!("{:x}", Sha256::digest(source.as_bytes())),
        measured_at_unix_secs: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs(),
        policy,
        baseline_configuration,
        recommended_configuration,
        configurations: reports,
        invalid_runs,
    })
}

#[cfg(test)]
mod tests;

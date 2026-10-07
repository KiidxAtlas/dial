mod rpc;

use anyhow::{Context, Result, bail, ensure};
use dial::{
    benchmark::{
        BenchmarkProfile, BenchmarkReport, BenchmarkSuite, CommandCheck, MachineBenchmarkPolicy,
        MachineBenchmarkReport, MachineConfiguration, MachineConfigurationReport, MachineIdentity,
        RunMetrics, clone_project, compare_baseline, rank_machine_configurations, score_report,
    },
    config::types::{Approval, Config, SandboxMode},
    storage::paths::Layout,
    tools::{self, ToolOutput, Workspace},
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
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
) -> Result<()> {
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
    let user_home = tempfile::tempdir().context("creating isolated benchmark state")?;
    prepare_user_home(layout, config, &user_home)?;
    let executable = std::env::current_exe().context("locating dial executable")?;
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

    let mut regressed = false;
    let payload = if let Some(path) = profiles_path {
        ensure!(
            baseline.is_none(),
            "--baseline applies to a single report, not --profiles"
        );
        let text = fs::read_to_string(resolve_output(layout, path))?;
        let profiles: Profiles = toml::from_str(&text).context("parsing benchmark profiles")?;
        ensure!(
            profiles.profiles.is_empty() || profiles.configurations.is_empty(),
            "use profiles or configurations, not both"
        );
        if !profiles.configurations.is_empty() {
            let report = run_machine(
                layout,
                &bench_config,
                &suite,
                &source,
                profiles.configurations,
                profiles.machine,
                &executable,
            )?;
            regressed = report.recommended_configuration.is_none();
            json!({"machine_benchmark": report})
        } else {
            let comparison = suite.run_profiles(
                &layout.project_root,
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
                                base_url: provider.base_url.clone(),
                                api_key_env: provider.api_key_env.clone(),
                                allow_insecure: provider.allow_insecure,
                            });
                        }
                    }
                    selected.provider = Some(configured.provider.clone());
                    selected.model = configured.model.clone();
                    selected.model_profile = Some(profile.name.clone());
                    for ablation in &profile.ablations {
                        match ablation.as_str() {
                            "no_integrations" => {
                                selected.integrations.clear();
                                selected.mcp.clear();
                                selected.dap.clear();
                            }
                            "replace_edit" => {
                                selected.edit_mode = dial::config::types::EditModeConfig::Replace;
                                selected
                                    .model_profiles
                                    .get_mut(&profile.name)
                                    .unwrap()
                                    .edit_mode = Some(selected.edit_mode);
                            }
                            "hashline_edit" => {
                                selected.edit_mode = dial::config::types::EditModeConfig::Hashline;
                                selected
                                    .model_profiles
                                    .get_mut(&profile.name)
                                    .unwrap()
                                    .edit_mode = Some(selected.edit_mode);
                            }
                            _ => bail!("unknown benchmark ablation {ablation:?}"),
                        }
                    }
                    selected.validate()?;
                    let isolated = tempfile::tempdir()?;
                    prepare_user_home(layout, &selected, &isolated)?;
                    run_agent(&executable, isolated.path(), &selected, case, workspace)
                },
                |check, workspace| run_command_check(&user_home, &bench_config, workspace, check),
            )?;
            json!({"profile_comparison": comparison})
        }
    } else {
        let report = suite.run(
            &layout.project_root,
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
        )?;
        let comparison = if let Some(path) = baseline {
            let bytes = fs::read(resolve_output(layout, path))?;
            let prior: serde_json::Value = serde_json::from_slice(&bytes)?;
            let prior: BenchmarkReport =
                serde_json::from_value(prior.get("report").cloned().unwrap_or(prior))?;
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
    if regressed {
        bail!(
            "benchmark regressed or no machine configuration met the correctness/regression floor"
        );
    }
    Ok(())
}

fn resolve_output(layout: &Layout, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        layout.project_root.join(path)
    }
}

fn prepare_user_home(layout: &Layout, config: &Config, user_home: &TempDir) -> Result<()> {
    let mut isolated = config.clone();
    isolated.improve.enabled = false;
    isolated.solve.enabled = false;
    isolated.escalation.enabled = false;
    isolated.approval = Approval::Auto;
    isolated.sandbox.mode = SandboxMode::Dev;
    isolated.sandbox.allow_network = false;
    isolated.sandbox.extra_write.clear();
    isolated
        .integrations
        .retain(|name| matches!(name.as_str(), "ast_search" | "ast_edit"));
    isolated.mcp.clear();
    isolated.tools.clear();
    isolated.subagents.clear();
    isolated.verification.commands.clear();
    isolated.verification.behavioral_checks.clear();
    let contents = toml::to_string(&isolated).context("serializing benchmark config")?;
    write_private(&user_home.path().join("config.toml"), contents.as_bytes())?;
    copy_private_if_present(
        &layout.user_dir.join("credentials.json"),
        &user_home.path().join("credentials.json"),
    )?;
    Ok(())
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
    )?;
    let output: ToolOutput = tools::execute(
        &ws,
        "bash",
        &json!({"command":check.command,"timeout":120}),
        120,
    );
    let Some(at) = output.text.rfind("[exit code: ") else {
        return Ok(false);
    };
    let status = output.text[at + 12..]
        .trim_end()
        .trim_end_matches(']')
        .parse::<i32>()
        .ok();
    let stdout = output.text[..at].trim();
    Ok(!output.is_error
        && status == Some(check.expect_exit)
        && check
            .expect_stdout
            .is_none_or(|expected| stdout == expected.trim()))
}

fn private_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
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
    let mut hostname = [0u8; 256];
    // gethostname is a local OS query; no network discovery or hardware fitness inference.
    let hostname =
        if unsafe { libc::gethostname(hostname.as_mut_ptr().cast(), hostname.len()) } == 0 {
            let end = hostname
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(hostname.len());
            String::from_utf8_lossy(&hostname[..end]).into_owned()
        } else {
            "unavailable".into()
        };
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

fn select_configuration(
    layout: &Layout,
    base: &Config,
    requested: &mut MachineConfiguration,
    policy: &MachineBenchmarkPolicy,
) -> Result<(Config, serde_json::Value)> {
    let mut selected = base.clone();
    selected.model_profile = None;
    selected.model_profiles.clear();
    selected.model_roles.clear();
    selected.provider = Some(requested.provider.clone());
    selected.model = requested.model.clone();
    selected.context_window = requested.context_window;
    selected.worker_timeout_secs = policy.case_timeout_secs;
    selected.harness.max_steps = selected.harness.capped_steps(24);
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
        let provider = selected
            .providers
            .get(&requested.provider)
            .context("machine benchmark provider is not configured")?;
        dial::llm::provider::endpoint_url(&provider.base_url, provider.allow_insecure)?;
        let uri: ureq::http::Uri = provider.base_url.parse()?;
        let host = uri.host().unwrap_or("").trim_matches(['[', ']']);
        let loopback = host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        ensure!(
            policy.allow_remote || loopback,
            "remote benchmark inference requires [machine] allow_remote = true"
        );
        selected.endpoint = Some(dial::config::types::EndpointConfig {
            base_url: provider.base_url.clone(),
            api_key_env: provider.api_key_env.clone(),
            allow_insecure: provider.allow_insecure,
        });
    }
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
        "tool_network_allowed": false,
        "automatic_improvement": false,
        "model_roles": "disabled",
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
                    prepare_user_home(layout, &selected, &home)?;
                    rpc::run_configured(
                        executable,
                        home.path(),
                        case,
                        workspace,
                        policy.case_timeout_secs,
                        configuration.adapter.as_deref(),
                    )
                },
                |check, workspace| run_command_check(&check_home, &selected, workspace, check),
            )?;
            cases.extend(report.cases.into_iter().map(|mut case| {
                case.id = format!("{repetition}:{}", case.id);
                case
            }));
        }
        let wall_time_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        reports.push(MachineConfigurationReport::measured(
            configuration,
            settings,
            score_report(&suite.name, cases),
            wall_time_ms,
        ));
    }
    let recommended_configuration = rank_machine_configurations(&mut reports, &policy)?;
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requested(model: &Path) -> MachineConfiguration {
        MachineConfiguration {
            name: "candidate".into(),
            provider: "mlx".into(),
            model: model.to_string_lossy().into_owned(),
            context_window: 8192,
            quantization_bits: Some(4),
            adapter: None,
        }
    }

    #[test]
    fn configuration_preflight_checks_quantization_and_adapter_identity() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(root.path(), home.path().to_owned()).unwrap();
        let model = root.path().join("model");
        fs::create_dir(&model).unwrap();
        fs::write(
            model.join("config.json"),
            r#"{"quantization":{"bits":4,"group_size":64}}"#,
        )
        .unwrap();
        let mut configuration = requested(&model);
        let (selected, settings) = select_configuration(
            &layout,
            &Config::default(),
            &mut configuration,
            &MachineBenchmarkPolicy::default(),
        )
        .unwrap();
        assert_eq!(selected.context_window, 8192);
        assert_eq!(settings["quantization_metadata"]["bits"], 4);
        configuration.quantization_bits = Some(8);
        assert!(
            select_configuration(
                &layout,
                &selected,
                &mut configuration,
                &MachineBenchmarkPolicy::default()
            )
            .is_err()
        );
        configuration.quantization_bits = Some(4);
        let adapter = root.path().join("adapter");
        fs::create_dir(&adapter).unwrap();
        fs::write(adapter.join("adapters.safetensors"), []).unwrap();
        fs::write(
            adapter.join("adapter_config.json"),
            r#"{"model":"/different/model"}"#,
        )
        .unwrap();
        configuration.adapter = Some(adapter.clone());
        assert!(
            select_configuration(
                &layout,
                &selected,
                &mut configuration,
                &MachineBenchmarkPolicy::default()
            )
            .is_err()
        );
        fs::write(
            adapter.join("adapter_config.json"),
            serde_json::to_vec(&json!({"model":model})).unwrap(),
        )
        .unwrap();
        let (_, settings) = select_configuration(
            &layout,
            &selected,
            &mut configuration,
            &MachineBenchmarkPolicy::default(),
        )
        .unwrap();
        assert!(settings["adapter_metadata"]["model"].is_string());
    }

    #[test]
    fn remote_inference_needs_explicit_opt_in() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(root.path(), home.path().to_owned()).unwrap();
        let base: Config = toml::from_str(
            r#"
            [providers.remote]
            base_url = "https://example.com/v1"
        "#,
        )
        .unwrap();
        let mut configuration = MachineConfiguration {
            name: "remote".into(),
            provider: "remote".into(),
            model: "operator-model".into(),
            context_window: 8192,
            quantization_bits: None,
            adapter: None,
        };
        let policy = MachineBenchmarkPolicy::default();
        assert!(
            select_configuration(&layout, &base, &mut configuration, &policy)
                .unwrap_err()
                .to_string()
                .contains("allow_remote")
        );
        let opted_in = MachineBenchmarkPolicy {
            allow_remote: true,
            ..policy
        };
        let (selected, _) =
            select_configuration(&layout, &base, &mut configuration, &opted_in).unwrap();
        assert_eq!(selected.model, "operator-model");
    }
}

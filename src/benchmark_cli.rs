use anyhow::{Context, Result, bail, ensure};
use dial::{
    benchmark::{BenchmarkProfile, BenchmarkReport, BenchmarkSuite, CommandCheck, RunMetrics, compare_baseline},
    config::types::{Approval, Config, SandboxMode},
    storage::paths::Layout,
    tools::{self, ToolOutput, Workspace},
};
use serde_json::json;
use serde::Deserialize;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
use tempfile::TempDir;


pub fn run(
    layout: &Layout,
    config: &Config,
    manifest: &Path,
    baseline: Option<&Path>,
    report_path: Option<&Path>,
    profiles_path: Option<&Path>,
) -> Result<()> {
    config.require_model()?;
    let manifest = if manifest.is_absolute() { manifest.to_owned() } else { layout.project_root.join(manifest) };
    let source = fs::read_to_string(&manifest).with_context(|| format!("reading benchmark suite {}", manifest.display()))?;
    let suite = BenchmarkSuite::parse_toml(&source)?;
    let user_home = tempfile::tempdir().context("creating isolated benchmark state")?;
    prepare_user_home(layout, config, &user_home)?;
    let executable = std::env::current_exe().context("locating dial executable")?;
    let mut bench_config = config.clone();
    bench_config.improve.enabled = false;
    bench_config.approval = Approval::Auto;
    bench_config.sandbox.mode = SandboxMode::Dev;
    bench_config.sandbox.allow_network = false;
    bench_config.sandbox.extra_write.clear();
    bench_config.integrations.retain(|name| matches!(name.as_str(), "ast_search" | "ast_edit"));
    bench_config.mcp.clear();
    bench_config.tools.clear();
    bench_config.subagents.clear();
    bench_config.verification.commands.clear();

    let mut regressed = false;
    let payload = if let Some(path) = profiles_path {
        ensure!(baseline.is_none(), "--baseline applies to a single report, not --profiles");
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Profiles { profiles: Vec<BenchmarkProfile> }
        let text = fs::read_to_string(resolve_output(layout, path))?;
        let profiles: Profiles = toml::from_str(&text).context("parsing benchmark profiles")?;
        let comparison = suite.run_profiles(
            &layout.project_root, &profiles.profiles,
            |profile, case, workspace| {
                let mut selected = bench_config.clone();
                ensure!(selected.model_profiles.contains_key(&profile.name), "unknown configured model profile {:?}", profile.name);
                let configured = selected.model_profiles[&profile.name].clone();
                if configured.provider != selected.selection().provider {
                    if configured.provider == "mlx" { selected.endpoint = None; }
                    else {
                        let provider = selected.providers.get(&configured.provider).context("benchmark profile provider is not configured")?;
                        selected.endpoint = Some(dial::config::types::EndpointConfig {
                            base_url:provider.base_url.clone(), api_key_env:provider.api_key_env.clone(), allow_insecure:provider.allow_insecure,
                        });
                    }
                }
                selected.provider = Some(configured.provider.clone());
                selected.model = configured.model.clone();
                selected.model_profile = Some(profile.name.clone());
                for ablation in &profile.ablations {
                    match ablation.as_str() {
                        "no_integrations" => { selected.integrations.clear(); selected.mcp.clear(); selected.dap.clear(); },
                        "replace_edit" => { selected.edit_mode = dial::config::types::EditModeConfig::Replace; selected.model_profiles.get_mut(&profile.name).unwrap().edit_mode = Some(selected.edit_mode); },
                        "hashline_edit" => { selected.edit_mode = dial::config::types::EditModeConfig::Hashline; selected.model_profiles.get_mut(&profile.name).unwrap().edit_mode = Some(selected.edit_mode); },
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
    } else {
        let report = suite.run(
            &layout.project_root,
            |case, workspace| run_agent(&executable, user_home.path(), &bench_config, case, workspace),
            |check, workspace| run_command_check(&user_home, &bench_config, workspace, check),
        )?;
        let comparison = if let Some(path) = baseline {
            let bytes = fs::read(resolve_output(layout, path))?;
            let prior: serde_json::Value = serde_json::from_slice(&bytes)?;
            let prior: BenchmarkReport = serde_json::from_value(prior.get("report").cloned().unwrap_or(prior))?;
            Some(compare_baseline(&report, &prior))
        } else { None };
        regressed = comparison.is_some_and(|c| c.regressed);
        json!({"report": report, "comparison": comparison})
    };
    let bytes = serde_json::to_vec_pretty(&payload)?;
    if let Some(path) = report_path {
        let path = resolve_output(layout, path);
        if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; }
        write_private(&path, &bytes)?;
        println!("{}", path.display());
    } else {
        println!("{}", String::from_utf8(bytes)?);
    }
    if regressed { bail!("benchmark regressed against its baseline"); }
    Ok(())
}

fn resolve_output(layout: &Layout, path: &Path) -> PathBuf {
    if path.is_absolute() { path.to_owned() } else { layout.project_root.join(path) }
}

fn prepare_user_home(layout: &Layout, config: &Config, user_home: &TempDir) -> Result<()> {
    let mut isolated = config.clone();
    isolated.improve.enabled = false;
    isolated.approval = Approval::Auto;
    isolated.sandbox.mode = SandboxMode::Dev;
    isolated.sandbox.allow_network = false;
    isolated.sandbox.extra_write.clear();
    isolated.integrations.retain(|name| matches!(name.as_str(), "ast_search" | "ast_edit"));
    isolated.mcp.clear();
    isolated.tools.clear();
    isolated.subagents.clear();
    isolated.verification.commands.clear();
    let contents = toml::to_string(&isolated).context("serializing benchmark config")?;
    write_private(&user_home.path().join("config.toml"), contents.as_bytes())?;
    copy_private_if_present(&layout.user_dir.join("credentials.json"), &user_home.path().join("credentials.json"))?;
    Ok(())
}

fn run_agent(
    executable: &Path,
    user_home: &Path,
    _config: &Config,
    case: &dial::benchmark::BenchmarkCase,
    workspace: &Path,
) -> Result<RunMetrics> {
    crate::benchmark_rpc::run(executable, user_home, case, workspace)
}

fn run_command_check(user_home: &TempDir, config: &Config, workspace: &Path, check: &CommandCheck<'_>) -> Result<bool> {
    let layout = Layout::with_user_dir(workspace, user_home.path().to_owned())?;
    let ws = Workspace::new(workspace)?;
    let ws = tools::apply_sandbox_config(ws, &config.sandbox, Some((&layout.state_dir, &layout.user_dir)))?;
    let output: ToolOutput = tools::execute(&ws, "bash", &json!({"command":check.command,"timeout":120}), 120);
    let Some(at) = output.text.rfind("[exit code: ") else { return Ok(false) };
    let status = output.text[at + 12..].trim_end().trim_end_matches(']').parse::<i32>().ok();
    let stdout = output.text[..at].trim();
    Ok(!output.is_error && status == Some(check.expect_exit) && check.expect_stdout.is_none_or(|expected| stdout == expected.trim()))
}

fn private_file(path: &Path) -> Result<File> {
    OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).with_context(|| format!("creating {}", path.display()))
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


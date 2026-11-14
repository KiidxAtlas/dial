//! Lifecycle of the local comparison workspace. Both the terminal and RPC call this owner.
//!
//! The versioned checkout's Python runner owns grading and progress; no terminal output is parsed
//! for scores. Launching the workspace never starts inference. Starting a run requires confirmation
//! in its loopback API. An installed Dial can target a checkout with `DIAL_BENCHMARK_ROOT`.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::Duration;

static WORKSPACES: OnceLock<Mutex<HashMap<PathBuf, Workspace>>> = OnceLock::new();

#[derive(Debug)]
struct Workspace {
    child: Child,
    url: String,
}

fn checkout(project: &Path) -> Result<PathBuf> {
    let configured = std::env::var_os("DIAL_BENCHMARK_ROOT").map(PathBuf::from);
    let root = if let Some(root) = configured {
        root
    } else {
        project
            .ancestors()
            .find(|path| {
                path.join("examples/benchmarks/benchmark_dashboard.py")
                    .is_file()
            })
            .context("benchmark tooling not found; set DIAL_BENCHMARK_ROOT to a Dial checkout")?
            .to_owned()
    };
    ensure!(
        root.join("examples/benchmarks/benchmark_dashboard.py")
            .is_file(),
        "DIAL_BENCHMARK_ROOT must contain examples/benchmarks/benchmark_dashboard.py"
    );
    root.canonicalize().context("resolving benchmark checkout")
}

/// Open, query or stop this process's comparison workspace, without changing personal settings.
///
/// # Errors
/// Refuses unknown actions, missing tooling/Python 3.11+, failed startup and poisoned state.
pub fn handle(project: &Path, action: &str) -> Result<Value> {
    ensure!(
        matches!(action, "open" | "status" | "stop"),
        "expected open, status or stop"
    );
    let root = checkout(project)?;
    let mut workspaces = WORKSPACES
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|_| anyhow::anyhow!("benchmark workspace state is poisoned"))?;
    if let Some(workspace) = workspaces.get_mut(&root)
        && workspace.child.try_wait()?.is_some()
    {
        workspaces.remove(&root);
    }
    if action == "stop" {
        let Some(mut workspace) = workspaces.remove(&root) else {
            return Ok(json!({"status":"closed"}));
        };
        // Graceful shutdown cancels runs and lets the runner finalize its evidence. A workspace
        // that cannot answer is killed instead of being left as an unreachable process.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(3)))
            .build()
            .into();
        let requested = agent
            .post(format!("{}api/shutdown", workspace.url))
            .header("X-Dial-Benchmark", "1")
            .send_json(json!({}));
        if requested.is_err() {
            let _ = workspace.child.kill();
            let _ = workspace.child.wait();
            return Ok(json!({"status":"stopped"}));
        }
        // The runner exits on its own after cancelling; reap it off this thread.
        let _ = std::thread::Builder::new()
            .name("dial-benchmark-reap".into())
            .spawn(move || {
                let _ = workspace.child.wait();
            });
        return Ok(json!({"status":"stopping"}));
    }
    if !workspaces.contains_key(&root) && action == "open" {
        let workspace = start(&root)?;
        workspaces.insert(root.clone(), workspace);
    }
    Ok(match workspaces.get(&root) {
        Some(workspace) => json!({"status":"ready", "url":workspace.url}),
        None => json!({"status":"closed"}),
    })
}

fn start(root: &Path) -> Result<Workspace> {
    let mut child = Command::new("python3")
        .arg(root.join("examples/benchmarks/benchmark_dashboard.py"))
        .arg("--root")
        .arg(root)
        .arg("--dial-executable")
        .arg(std::env::current_exe()?)
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("starting benchmark workspace (requires Python 3.11+)")?;
    let stdout = child
        .stdout
        .take()
        .context("benchmark startup pipe is missing")?;
    let (tx, rx) = mpsc::sync_channel(1);
    let reader = std::thread::Builder::new()
        .name("dial-benchmark-ready".into())
        .spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
            let _ = tx.send(result);
        });
    let ready = (|| -> Result<String> {
        reader.context("starting benchmark readiness reader")?;
        let line = rx.recv_timeout(Duration::from_secs(5)).context(
            "benchmark workspace did not confirm readiness (check Python 3.11+ and tooling)",
        )??;
        let value: Value =
            serde_json::from_str(&line).context("benchmark workspace failed before readiness")?;
        let url = value["url"]
            .as_str()
            .context("benchmark workspace returned no URL")?;
        ensure!(
            url.starts_with("http://127.0.0.1:"),
            "benchmark workspace returned a non-loopback URL"
        );
        Ok(url.to_owned())
    })();
    match ready {
        Ok(url) => Ok(Workspace { child, url }),
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            bail!(error)
        }
    }
}

#[cfg(test)]
mod tests;

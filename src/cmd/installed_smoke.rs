//! Internal installed-artifact probe: no model, network, desktop capture or input.
use anyhow::{Context, Result, bail, ensure};
use dial::runtime::process;
use dial::setup::deps::bundled_node;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Bound for the Node version probe. The x86_64 macOS artifact is checked on Apple silicon, where
/// Rosetta translates the bundled Node on its first launch; that alone exceeded 10 s in CI.
const TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) fn run(arguments: impl Iterator<Item = std::ffi::OsString>) -> Result<()> {
    let arguments: Vec<_> = arguments.collect();
    let require_bundle = match arguments.as_slice() {
        [] => false,
        [flag] if flag == "--require-bundled-node" => true,
        _ => bail!("usage: dial __installed-smoke [--require-bundled-node]"),
    };
    ensure!(
        !require_bundle || bundled_node::available(),
        "installed artifact has no bundled Node runtime"
    );
    let cwd = std::env::current_dir().context("finding probe workspace")?;
    let node = bundled_node::resolve(&cwd, "node", "computer.node").map_err(anyhow::Error::msg)?;
    let mut command = Command::new(node);
    command
        .arg("--version")
        .env_clear()
        .envs(dial::tools::exec::sandbox::windows_system_env());
    let result = process::run(
        command,
        None,
        TIMEOUT,
        4096,
        &AtomicBool::new(false),
        &mut |_, _| {},
    )
    .map_err(anyhow::Error::msg)?;
    ensure!(
        result.success(),
        "{}",
        result.failure("Node version probe", TIMEOUT)
    );
    let version = result.stdout_text();
    let version = version.trim();
    check_version(version, bundled_node::version())?;
    dial::computer::supervisor::smoke_test(&cwd)
        .map_err(|error| anyhow::anyhow!("computer worker transport probe: {}", error.message))?;
    let mut checks = vec!["node-version", "computer-worker"];
    if bundled_node::available() {
        checks.insert(0, "bundled-node");
    }
    println!(
        "{}",
        serde_json::json!({
            "schema_version": 1,
            "bundled": bundled_node::available(),
            "node_version": version,
            "checks": checks,
        })
    );
    Ok(())
}

fn check_version(actual: &str, bundled: Option<&str>) -> Result<()> {
    if let Some(expected) = bundled {
        ensure!(
            actual == format!("v{expected}"),
            "bundled Node version mismatch: expected v{expected}, got {actual:?}"
        );
    } else {
        let major = actual
            .strip_prefix('v')
            .and_then(|v| v.split('.').next())
            .and_then(|v| v.parse::<u64>().ok());
        ensure!(
            major.is_some_and(|major| major >= 22),
            "Node 22+ required, got {actual:?}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;

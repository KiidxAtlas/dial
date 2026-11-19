//! Internal installed-artifact probe: no model, network, desktop capture or input.
use anyhow::{Context, Result, bail, ensure};
use dial::runtime::process;
use dial::setup::deps::{bundled_node, bundled_python};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Bound for the Node version probe. The x86_64 macOS artifact is checked on Apple silicon, where
/// Rosetta translates the bundled Node on its first launch; that alone exceeded 10 s in CI.
const TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) fn run(arguments: impl Iterator<Item = std::ffi::OsString>) -> Result<()> {
    let (mut node, mut python) = (false, false);
    for flag in arguments {
        match flag.to_str() {
            Some("--require-bundled-node") if !node => node = true,
            Some("--require-bundled-python") if !python => python = true,
            _ => bail!(
                "usage: dial __installed-smoke [--require-bundled-node] [--require-bundled-python]"
            ),
        }
    }
    ensure!(
        !node || bundled_node::available(),
        "installed artifact has no bundled Node runtime"
    );
    let cwd = std::env::current_dir().context("finding probe workspace")?;
    let mut checks = Vec::new();
    let mut report = serde_json::json!({"schema_version": 1});
    if node || !python {
        report["bundled"] = bundled_node::available().into();
        report["node_version"] = probe_node(&cwd)?.into();
        if bundled_node::available() {
            checks.push("bundled-node");
        }
        checks.extend(["node-version", "computer-worker"]);
    }
    if python {
        report["python_version"] = probe_python(&cwd)?.into();
        checks.push("bundled-python");
    }
    report["checks"] = serde_json::json!(checks);
    println!("{report}");
    Ok(())
}

fn probe_node(cwd: &std::path::Path) -> Result<String> {
    let node = bundled_node::resolve(cwd, "node", "computer.node").map_err(anyhow::Error::msg)?;
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
    dial::computer::supervisor::smoke_test(cwd)
        .map_err(|error| anyhow::anyhow!("computer worker transport probe: {}", error.message))?;
    Ok(version.to_owned())
}

/// Only controller-owned source is run. Go through the real tool so a launch or driver failure
/// cannot pass as a working interpreter. No virtualenv, host fallback or user config is needed.
fn probe_python(cwd: &std::path::Path) -> Result<&'static str> {
    use dial::runtime::tool_output::ToolOutput;
    use dial::tools::{
        self, Workspace, apply_sandbox_config,
        exec::eval,
        ext::{ToolCallback, ToolContext},
    };
    let expected =
        bundled_python::version().context("installed artifact has no bundled Python runtime")?;
    let mut config = dial::config::types::Config::default();
    config.kernel.venv = "off".into();
    // Exercise a saved artifact, not an in-memory pattern. Keep the caller's files untouched.
    let fixture = tempfile::tempdir_in(cwd).context("creating Python probe workspace")?;
    std::fs::write(fixture.path().join("pattern.txt"), r"^id=(\d+)$")?;
    let workspace = apply_sandbox_config(Workspace::new(fixture.path())?, &config.sandbox, None)?;
    let ctx = ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let tool = eval::tools(&config).pop().context("missing eval tool")?;
    let code = r#"import platform, re
assert platform.python_implementation() == 'CPython'
source = dial.read('pattern.txt')
pattern = source['content']
text = 'id=17\nnot-id=88\nid=23'
expected = ['17', '23']
dial.check('matched IDs', re.findall(pattern, text, re.MULTILINE) == expected, source)
assert re.findall(pattern, 'id=bad', re.MULTILINE) == []
# An extra empty capture preserves the match count but breaks the return contract.
assert re.findall(pattern + '()', text, re.MULTILINE) == [('17', ''), ('23', '')]
print(platform.python_version())"#;
    let out = tool.call_with_dispatch(
        &ctx,
        &serde_json::json!({"language":"py", "code":code, "timeout":60, "background":false}),
        &|_| {},
        &AtomicBool::new(false),
        &|_| Err("probe must run in the foreground".into()),
        &mut |callback| match callback {
            ToolCallback::Call { name, arguments }
                if name == "read"
                    && arguments["path"] == "pattern.txt"
                    && matches!(arguments["format"].as_str(), Some("text" | "base64")) =>
            {
                tools::execute(&workspace, "read", &arguments, 60)
            }
            _ => ToolOutput::err("probe permits only a data read of its fixture"),
        },
    );
    ensure!(!out.is_error, "bundled Python Eval: {}", out.text);
    let data = out.data.as_ref().context("Python probe returned no data")?;
    let digest = eval::check_source_digest(&workspace, "pattern.txt")
        .context("probe fixture unavailable")?;
    let check = &data["checks"][0];
    ensure!(
        check["passed"] == true
            && check["name"] == "matched IDs"
            && check["runtime"] == "python"
            && check["source"] == serde_json::json!({"path":"pattern.txt","sha256":digest}),
        "Python probe did not bind its check to the saved fixture"
    );
    let actual = data["stdout"].as_str().unwrap_or("").trim();
    ensure!(
        actual == expected,
        "bundled Python version mismatch: expected {expected}, got {actual:?}"
    );
    Ok(expected)
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

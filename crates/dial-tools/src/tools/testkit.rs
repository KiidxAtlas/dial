//! Shared fixtures for the tool tests.
use super::{EditMode, ToolOutput, Workspace, execute};
use serde_json::Value;

/// The directory is created under the canonical temp root so `dir.path()` equals `ws.root`
/// (on macOS the default `/var/folders/...` is a symlink into `/private/var`).
pub(crate) fn workspace() -> (tempfile::TempDir, Workspace) {
    let base = std::env::temp_dir().canonicalize().unwrap();
    let dir = tempfile::tempdir_in(base).unwrap();
    let ws = Workspace::new(dir.path())
        .unwrap()
        .with_edit_mode(EditMode::Replace);
    (dir, ws)
}

pub(crate) fn run(ws: &Workspace, name: &str, args: Value) -> ToolOutput {
    execute(ws, name, &args, 20)
}

/// Keep ordinary Git fixtures independent of executable filters in the operator's
/// configuration. Security tests may install their own filters after this baseline.
pub(crate) fn disable_inherited_git_filters(root: &std::path::Path) {
    let filters = std::process::Command::new("git")
        .args([
            "config",
            "--null",
            "--name-only",
            "--get-regexp",
            r"^filter\..*\.(clean|smudge|process)$",
        ])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(filters.status.success() || filters.status.code() == Some(1));
    for key in filters
        .stdout
        .split(|byte| *byte == 0)
        .filter(|key| !key.is_empty())
    {
        let result = std::process::Command::new("git")
            .args(["config", "--local", std::str::from_utf8(key).unwrap(), ""])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(result.status.success(), "{result:?}");
    }
}

/// Test authority for native mutations: the named tool may touch absolute paths under the
/// workspace root or an added root, with no permission rules. This is the containment check
/// the controller applies, reduced to what a tool-level fixture needs.
pub(crate) fn authorize_in_workspace<'a>(
    ws: &'a Workspace,
    tool: &'a str,
) -> impl Fn(&str, &[std::path::PathBuf], bool) -> Result<(), String> + 'a {
    move |name, paths, unbounded| {
        if name != tool {
            return Err("native mutation authority does not match the executing tool".into());
        }
        if unbounded {
            return Ok(());
        }
        let contained = |path: &std::path::PathBuf| {
            path.is_absolute()
                && (path.starts_with(ws.root())
                    || ws
                        .additional_roots()
                        .iter()
                        .any(|root| path.starts_with(root)))
                && !path.components().any(|part| {
                    matches!(
                        part,
                        std::path::Component::ParentDir | std::path::Component::CurDir
                    )
                })
        };
        if paths.is_empty() || !paths.iter().all(contained) {
            return Err("native mutation must declare canonical workspace targets".into());
        }
        Ok(())
    }
}

/// Run the named test in a fresh process with its environment set before any threads start.
/// Returns true only in that child; the parent checks its result and skips the test body.
pub(crate) fn in_env_subprocess(test: &str, env: &[(&str, &str)]) -> bool {
    const MARKER: &str = "DIAL_ENV_TEST_CHILD";
    if std::env::var(MARKER).as_deref() == Ok(test) {
        return true;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(MARKER, test)
        .envs(env.iter().copied())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{test} subprocess failed: {}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

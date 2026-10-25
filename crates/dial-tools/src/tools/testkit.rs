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

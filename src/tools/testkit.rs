//! Shared fixtures for the tool tests.
use super::{EditMode, ToolOutput, Workspace, execute};
use serde_json::Value;

pub(crate) fn workspace() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path())
        .unwrap()
        .with_edit_mode(EditMode::Replace);
    (dir, ws)
}

pub(crate) fn run(ws: &Workspace, name: &str, args: Value) -> ToolOutput {
    execute(ws, name, &args, 20)
}

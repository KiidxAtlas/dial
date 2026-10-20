//! Commit a controller-planned single-file restoration (the agent's `revert_edit`).
//!
//! The controller decides what to restore from its checkpoint journal; this module only performs
//! the compare-and-swap through the ordinary edit transaction, so protected-path checks, checkpoint
//! observation (the revert itself is undoable), LSP write-through and committed-path facts all
//! apply exactly as for `edit`. Read witnesses for the file are dropped: the model re-reads it
//! before editing again.
use crate::runtime::tool_output::ToolOutput;
use crate::tools::Workspace;
use crate::tools::edit::edit_transaction::{Change, commit};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::atomic::AtomicBool;

/// Restore `path` (canonical, absolute) to `content` (`None` deletes it) only if its bytes still
/// hash to `expected` (`None`: it must be absent). `label` names the restored state in the result.
pub fn restore(
    ws: &Workspace,
    path: &Path,
    expected: Option<[u8; 32]>,
    content: Option<Vec<u8>>,
    label: &str,
) -> ToolOutput {
    let cancel = AtomicBool::new(false);
    crate::tools::exec::execution::run(&cancel, || {
        match commit_restore(ws, path, expected, content, label) {
            Ok(text) => ToolOutput {
                text,
                is_error: false,
                data: None,
                execution: Default::default(),
            },
            Err(error) => ToolOutput::err(error),
        }
    })
}

fn commit_restore(
    ws: &Workspace,
    path: &Path,
    expected: Option<[u8; 32]>,
    content: Option<Vec<u8>>,
    label: &str,
) -> Result<String, String> {
    let shown = ws.rel(path);
    let after = content
        .map(String::from_utf8)
        .transpose()
        .map_err(|_| format!("saved content of {shown} is not UTF-8 text; use /undo instead"))?;
    let change = Change::load(ws, path.to_path_buf(), after)?;
    let current = change
        .before
        .as_ref()
        .map(|original| <[u8; 32]>::from(Sha256::digest(original.text.as_bytes())));
    if current != expected {
        return Err(format!(
            "{shown} changed since the revert was planned; nothing restored. Read it and edit instead."
        ));
    }
    let deleted = change.after.is_none();
    let warnings = commit(ws, std::slice::from_ref(&change))?;
    ws.store().forget(path);
    let mut text = if deleted {
        format!("Removed {shown}: it did not exist {label}. Checkpointed; /undo restores it.")
    } else {
        format!(
            "Restored {shown} to its content {label}. Checkpointed; /undo applies. Read it before editing again."
        )
    };
    for warning in warnings {
        text.push('\n');
        text.push_str(&warning);
    }
    Ok(text)
}

#[cfg(test)]
mod tests;

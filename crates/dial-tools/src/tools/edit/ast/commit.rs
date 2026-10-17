//! All-or-nothing application of rewritten files.
//!
//! Order: tell the workspace observer about every file (so /undo checkpoints exist before
//! anything changes), stage every new content as a temp file next to its target, then rename
//! them into place. A failure before the first rename leaves the tree untouched; a failure
//! during the renames puts the already replaced files back.

use crate::tools::Workspace;
use std::fs;
use std::path::PathBuf;

/// One file's new content.
pub(super) struct Change {
    pub abs: PathBuf,
    pub rel: String,
    pub original: String,
    pub updated: String,
    pub perms: fs::Permissions,
}

/// Write every change, or none. On success the workspace's hashline snapshots of the files are
/// dropped so older `[path#TAG]` headers cannot be used to edit them.
pub(super) fn commit(ws: &Workspace, changes: &[Change]) -> Result<Vec<String>, String> {
    let mut transaction = Vec::with_capacity(changes.len());
    for change in changes {
        crate::tools::edit::editing::generated_guard(ws, &change.abs, Some(&change.original))?;
        let mut current = crate::tools::edit::edit_transaction::Change::load(
            ws,
            change.abs.clone(),
            Some(change.updated.clone()),
        )?;
        if current.before.as_ref().map(|before| before.text.as_str())
            != Some(change.original.as_str())
        {
            return Err(format!(
                "{} changed after AST planning; re-run ast_edit",
                change.rel
            ));
        }
        let now = fs::metadata(&change.abs)
            .map_err(|e| format!("{}: {e}", change.rel))?
            .permissions();
        if now != change.perms {
            return Err(format!(
                "{} changed after AST planning; re-run ast_edit",
                change.rel
            ));
        }
        current.authored_whole = true;
        transaction.push(current);
    }
    let notes = crate::tools::edit::edit_transaction::commit(ws, &transaction)
        .map_err(|e| format!("{e}; nothing was written"))?;
    let mut store = ws.store();
    for change in changes {
        store.forget(&change.abs);
    }
    Ok(notes)
}

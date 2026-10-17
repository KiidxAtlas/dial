use std::path::PathBuf;

use crate::tools::Workspace;
use crate::tools::code::lsp;

#[cfg(unix)]
#[path = "edit_transaction_unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "edit_transaction_windows.rs"]
mod imp;

pub(crate) use imp::{Change, MAX_BYTES, MAX_FILE, Original, inherit_permissions, open_read, read};

/// Commits every change atomically, then notifies LSP servers (watched files, format-on-write,
/// diagnostics). LSP output is appended to the warnings. It never fails the commit, because the
/// files are already written when it runs.
pub(crate) fn commit(ws: &Workspace, changes: &[Change]) -> Result<Vec<String>, String> {
    commit_as(ws, changes, lsp::WriteOrigin::Edit)
}

/// Commits like [`commit`]. `origin` says why the change is published: a format-on-write commit
/// must not format again (see [`lsp::WriteOrigin`]). Checkpoint observation is the same for all.
pub(crate) fn commit_as(
    ws: &Workspace,
    changes: &[Change],
    origin: lsp::WriteOrigin,
) -> Result<Vec<String>, String> {
    let _lease = super::document_transaction::Lease::acquire(
        changes.iter().map(|change| change.path.clone()),
    )?;
    crate::tools::exec::execution::before_commit()?;
    let mut warnings = imp::commit(ws, changes)?;
    crate::tools::exec::execution::committed(
        changes
            .iter()
            .filter(|change| match (&change.before, &change.after) {
                (Some(before), Some(after)) => before.text != *after,
                (None, None) => false,
                _ => true,
            })
            .map(|change| change.path.clone()),
    );
    let paths: Vec<(PathBuf, u8)> = changes
        .iter()
        .filter_map(|change| match (&change.before, &change.after) {
            (None, None) => None,
            (None, Some(_)) => Some((change.path.clone(), 1)),
            (Some(_), None) => Some((change.path.clone(), 3)),
            (Some(_), Some(_)) => Some((change.path.clone(), 2)),
        })
        .collect();
    if !paths.is_empty() {
        let notes = lsp::writethrough(ws, &paths, origin);
        if !notes.trim().is_empty() {
            warnings.push(notes.trim_end().to_owned());
        }
    }
    Ok(warnings)
}

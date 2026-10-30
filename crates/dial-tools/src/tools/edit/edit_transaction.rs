use std::path::PathBuf;

use crate::tools::Workspace;
use crate::tools::code::lsp;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use self::unix as imp;
#[cfg(windows)]
use self::windows as imp;

pub(crate) use imp::{Change, MAX_BYTES, MAX_FILE, Original, inherit_permissions, open_read, read};

/// A new binary file staged and published relative to a pinned, no-follow directory handle.
/// Unlike text edits, this streams arbitrary bytes and never replaces an existing destination.
pub(crate) struct NewBinary {
    dir: imp::Directory,
    temporary: imp::EntryName,
    path: PathBuf,
    file: std::fs::File,
}

impl NewBinary {
    pub(crate) fn create(ws: &Workspace, path: &std::path::Path) -> Result<Self, String> {
        let (dir, temporary, file) = imp::stage_binary(ws, path)?;
        Ok(Self {
            dir,
            temporary,
            path: path.to_owned(),
            file,
        })
    }

    pub(crate) fn file_mut(&mut self) -> &mut std::fs::File {
        &mut self.file
    }

    pub(crate) fn publish(
        self,
        ws: &Workspace,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<(), String> {
        self.file.sync_all().map_err(|e| e.to_string())?;
        let _lease = super::document_transaction::Lease::acquire([self.path.clone()])?;
        if ws.resolve(&ws.rel(&self.path))? != self.path {
            return Err("binary destination physical alias changed".into());
        }
        ws.observe_write(&self.path)?;
        self.dir.validate_location(ws, &self.path)?;
        // Sync and observers may take time; turn cancellation must still prevent publication.
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("binary publication interrupted".into());
        }
        crate::tools::exec::execution::before_commit()?;
        // The exclusive link uses the pinned directory, never a re-resolved pathname.
        self.dir
            .link(&self.temporary, &imp::entry_name(&self.path)?)?;
        crate::tools::exec::execution::committed([self.path.clone()]);
        Ok(())
    }
}

impl Drop for NewBinary {
    fn drop(&mut self) {
        let _ = self.dir.unlink(&self.temporary);
    }
}

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

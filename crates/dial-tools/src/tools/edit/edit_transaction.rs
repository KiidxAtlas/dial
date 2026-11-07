use std::path::{Path, PathBuf};

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

/// The editor-client target for `path`: a plain text file inside a workspace root. Rich documents,
/// images, notebooks and paths outside the roots keep their local readers and writers.
fn client_target<'a>(ws: &'a Workspace, path: &Path) -> Option<&'a dyn crate::tools::ClientFileIo> {
    let client = ws.client_files()?;
    let shown = path.to_str()?;
    (ws.root_containing(path).is_some()
        && crate::tools::fs::documents::kind(shown).is_none()
        && !crate::llm::images::is_image(path)
        && !crate::tools::edit::edit_source::notebook_path(path))
    .then_some(client)
}

/// Client used for reads of `path`. Reads are bridged only when the client also takes writes:
/// a read from the editor buffer must be committed back through the same client, or the local
/// stage would compare the buffer against disk and refuse the edit.
pub(crate) fn read_bridge<'a>(
    ws: &'a Workspace,
    path: &Path,
) -> Option<&'a dyn crate::tools::ClientFileIo> {
    client_target(ws, path).filter(|c| c.reads() && c.writes())
}

/// Text of `path` for a streaming read: the editor's buffer when the bridge serves it, otherwise
/// the local file opened through its pinned directory.
pub(crate) fn open_text(
    ws: &Workspace,
    path: &std::path::Path,
) -> Result<Box<dyn std::io::BufRead>, String> {
    if let Some(client) = read_bridge(ws, path)
        && path.is_file()
    {
        let text = client.read_text(path)?;
        return Ok(Box::new(std::io::Cursor::new(text.into_bytes())));
    }
    Ok(Box::new(std::io::BufReader::new(open_read(ws, path)?)))
}

/// Publishes a change set through the editor client. Runs after every check and after the
/// checkpoint observer, so approvals, confinement and the seen-line guard are unchanged. `None`
/// keeps the local transaction (no bridged target). A bridged read of a deleted file, or a batch
/// that mixes bridged and local targets, is refused before any write. A client error stops the
/// batch as a tool error; no local write follows.
pub(crate) fn client_commit(
    ws: &Workspace,
    changes: &[Change],
) -> Option<Result<Vec<String>, String>> {
    let mut targets: Vec<Option<&dyn crate::tools::ClientFileIo>> =
        Vec::with_capacity(changes.len());
    for change in changes {
        let write =
            client_target(ws, &change.path).filter(|c| c.writes() && change.after.is_some());
        if write.is_none() && read_bridge(ws, &change.path).is_some() {
            return Some(Err(format!(
                "{} is read through the editor client, so it cannot be deleted locally; no change was made",
                ws.rel(&change.path)
            )));
        }
        targets.push(write);
    }
    if targets.iter().all(Option::is_none) {
        return None;
    }
    if targets.iter().any(Option::is_none) {
        return Some(Err(
            "edit mixes editor-client files with local files; split it so each batch is entirely one or the other; no change was made"
                .into(),
        ));
    }
    let mut written: Vec<String> = Vec::new();
    for (client, change) in targets.into_iter().flatten().zip(changes) {
        let text = change.after.as_deref().unwrap_or_default();
        if let Err(error) = client.write_text_confirmed(&change.path, text) {
            let certainty = match &error {
                crate::tools::ClientWriteError::NotWritten(_) => "was not written",
                crate::tools::ClientWriteError::Unknown(_) => {
                    crate::tools::exec::execution::unsettled();
                    "write was not confirmed; reconcile the editor state before retrying"
                }
            };
            let already = if written.is_empty() {
                String::new()
            } else {
                format!(
                    "; already written by the editor client: {}",
                    written.join(", ")
                )
            };
            return Some(Err(format!(
                "{} {certainty} ({error}); no local write was made{already}",
                ws.rel(&change.path)
            )));
        }
        written.push(ws.rel(&change.path));
        crate::tools::exec::execution::committed([change.path.clone()]);
    }
    Some(Ok(Vec::new()))
}
#[cfg(test)]
mod tests;

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
        crate::platform::fs::sync_file(&self.file).map_err(|e| e.to_string())?;
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

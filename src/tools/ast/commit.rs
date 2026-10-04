//! All-or-nothing application of rewritten files.
//!
//! Order: tell the workspace observer about every file (so /undo checkpoints exist before
//! anything changes), stage every new content as a temp file next to its target, then rename
//! them into place. A failure before the first rename leaves the tree untouched; a failure
//! during the renames puts the already replaced files back.

use crate::tools::Workspace;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// One file's new content.
pub(super) struct Change {
    pub abs: PathBuf,
    pub rel: String,
    pub original: String,
    pub updated: String,
    pub perms: fs::Permissions,
}

static STAGE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write `content` to a fresh temp file beside `target` with `perms`.
fn stage(target: &Path, content: &str, perms: &fs::Permissions) -> io::Result<PathBuf> {
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let n = STAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = target.with_file_name(format!(".{name}.dial-ast.{}.{n}.tmp", std::process::id()));
    let mut file: File = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    let written = file.write_all(content.as_bytes()).and_then(|()| file.set_permissions(perms.clone())).and_then(|()| file.sync_all());
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(tmp)
}

fn discard(staged: &[PathBuf]) {
    for tmp in staged {
        let _ = fs::remove_file(tmp);
    }
}

/// Put the original content back for `done` (already replaced) files; returns files that could not be restored.
fn restore(done: &[Change]) -> Vec<String> {
    let mut stuck = Vec::new();
    for change in done {
        let back = stage(&change.abs, &change.original, &change.perms).and_then(|tmp| {
            let renamed = fs::rename(&tmp, &change.abs);
            if renamed.is_err() {
                discard(&[tmp]);
            }
            renamed
        });
        if back.is_err() {
            stuck.push(change.rel.clone());
        }
    }
    stuck
}

/// Write every change, or none. On success the workspace's hashline snapshots of the files are
/// dropped so older `[path#TAG]` headers cannot be used to edit them.
pub(super) fn commit(ws: &Workspace, changes: &[Change]) -> Result<(), String> {
    for change in changes {
        ws.observe_write(&change.abs)?;
    }
    let mut staged: Vec<PathBuf> = Vec::with_capacity(changes.len());
    for change in changes {
        match stage(&change.abs, &change.updated, &change.perms) {
            Ok(tmp) => staged.push(tmp),
            Err(e) => {
                discard(&staged);
                return Err(format!("staging {:?}: {e}; nothing was written", change.rel));
            }
        }
    }
    for (i, change) in changes.iter().enumerate() {
        if let Err(e) = fs::rename(&staged[i], &change.abs) {
            discard(&staged[i..]);
            let stuck = restore(&changes[..i]);
            let tail = if stuck.is_empty() {
                "earlier files were restored; nothing was written".to_owned()
            } else {
                format!("COULD NOT RESTORE {} (use /undo)", stuck.join(", "))
            };
            return Err(format!("replacing {:?}: {e}; {tail}", change.rel));
        }
    }
    let mut store = ws.store();
    for change in changes {
        store.forget(&change.abs);
    }
    Ok(())
}

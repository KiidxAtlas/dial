//! Session-scoped Cargo build output for commands Dial runs.
//!
//! Each session gets one target directory under the workspace scratch dir, so concurrent sessions
//! stop contending for one `target/` lock. The directory is fixed per session, never per command.
//! An explicit `CARGO_TARGET_DIR` in the inherited environment is left alone, and a `--target-dir`
//! flag still wins because Cargo applies it after the environment. Copied workers have their own
//! root, so they get their own directory; shared-checkout workers share their session's.
//!
//! Each export touches a marker in the directory. Directories of other sessions whose marker is
//! older than [`STALE_AFTER`] are deleted by [`prune_stale`], so finished sessions do not leave
//! build output behind indefinitely.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

const TARGET_ROOT: &str = "cargo-target";
const MAX_SESSION_LEN: usize = 128;
/// Records when Dial last exported the directory for a command.
const USED_MARKER: &str = ".dial-last-used";
/// Session target directories unused for this long are removed.
pub const STALE_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Whether a session name is safe to use as one path component.
pub(crate) fn valid_session(session: &str) -> bool {
    !session.is_empty()
        && session.len() <= MAX_SESSION_LEN
        && session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// The per-session target directory for a workspace root.
pub(crate) fn dir_for(root: &Path, session: &str) -> PathBuf {
    root.join(crate::workspace_copy::SCRATCH_DIR)
        .join(TARGET_ROOT)
        .join(session)
}

/// Chooses the directory to export: none when the inherited environment already names one.
fn chosen<'a>(inherited: Option<&OsStr>, dir: Option<&'a Path>) -> Option<&'a Path> {
    match inherited {
        Some(_) => None,
        None => dir,
    }
}

/// Exports `CARGO_TARGET_DIR` on `command` unless the process environment already sets one.
pub(crate) fn apply(command: &mut Command, dir: Option<PathBuf>) {
    // Named-shell state and callers such as edit preview may already set an override on
    // this command; checking only Dial's inherited environment would overwrite it.
    if command.get_envs().any(|(key, _)| {
        if cfg!(windows) {
            key.to_string_lossy()
                .eq_ignore_ascii_case("CARGO_TARGET_DIR")
        } else {
            key == OsStr::new("CARGO_TARGET_DIR")
        }
    }) {
        return;
    }
    let inherited = std::env::var_os("CARGO_TARGET_DIR");
    if let Some(dir) = chosen(inherited.as_deref(), dir.as_deref()) {
        mark_used(dir);
        command.env("CARGO_TARGET_DIR", dir);
    }
}

/// Refreshes the last-used marker of an existing target directory. Cargo creates the directory on
/// its first build; a command that never runs Cargo leaves nothing behind.
pub(crate) fn mark_used(dir: &Path) {
    if dir.is_dir() {
        let _ = std::fs::write(dir.join(USED_MARKER), b"");
    }
}

/// Deletes other sessions' target directories under `root` that have not been used for `stale`.
/// Returns the removed directories. Errors are ignored: pruning is best effort.
pub fn prune_stale(root: &Path, keep_session: &str, stale: Duration) -> Vec<PathBuf> {
    let base = root
        .join(crate::workspace_copy::SCRATCH_DIR)
        .join(TARGET_ROOT);
    let Ok(entries) = std::fs::read_dir(&base) else {
        return Vec::new();
    };
    let now = SystemTime::now();
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == keep_session || !valid_session(name) {
            continue;
        }
        let path = entry.path();
        // Never follow a link out of the scratch directory.
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let used = std::fs::metadata(path.join(USED_MARKER))
            .or_else(|_| std::fs::metadata(&path))
            .and_then(|meta| meta.modified());
        let Ok(used) = used else { continue };
        if now.duration_since(used).is_ok_and(|age| age >= stale)
            && std::fs::remove_dir_all(&path).is_ok()
        {
            removed.push(path);
        }
    }
    removed
}

#[cfg(test)]
mod tests;

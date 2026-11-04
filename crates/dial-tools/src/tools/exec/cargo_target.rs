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
//! build output behind indefinitely. A new session first adopts the most recently used idle
//! directory ([`adopt_idle`]) so its first build is incremental instead of cold.

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
/// Another session's directory unused for this long, with no Cargo build holding it, may be
/// taken over by a new session.
pub const ADOPT_AFTER: Duration = Duration::from_secs(30 * 60);

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

/// When `session` has no target directory yet, renames the most recently used directory of another
/// session that has been idle for at least `idle` and is not locked by a running Cargo build, so
/// the new session starts with a warm cache. Returns the directory that was adopted.
pub fn adopt_idle(root: &Path, session: &str, idle: Duration) -> Option<PathBuf> {
    if !valid_session(session) {
        return None;
    }
    let own = dir_for(root, session);
    if own.exists() {
        return None;
    }
    let candidate = idle_directories(root, session, idle)
        .into_iter()
        .filter(|(path, _)| !cargo_build_running(path))
        .max_by_key(|(_, used)| *used)?
        .0;
    std::fs::rename(&candidate, &own).ok()?;
    mark_used(&own);
    Some(candidate)
}

/// Whether Cargo holds the build lock of any profile directory in `dir`.
fn cargo_build_running(dir: &Path) -> bool {
    let Ok(profiles) = std::fs::read_dir(dir) else {
        return true;
    };
    profiles.flatten().any(|profile| {
        let lock = profile.path().join(".cargo-lock");
        std::fs::File::open(&lock).is_ok_and(|file| file.try_lock().is_err())
    })
}

/// Other sessions' target directories unused for at least `idle`, with their last-use time.
fn idle_directories(root: &Path, keep_session: &str, idle: Duration) -> Vec<(PathBuf, SystemTime)> {
    let base = root
        .join(crate::workspace_copy::SCRATCH_DIR)
        .join(TARGET_ROOT);
    let Ok(entries) = std::fs::read_dir(&base) else {
        return Vec::new();
    };
    let now = SystemTime::now();
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            // Never follow a link out of the scratch directory.
            if name == keep_session
                || !valid_session(name)
                || !entry.file_type().is_ok_and(|kind| kind.is_dir())
            {
                return None;
            }
            let path = entry.path();
            let used = std::fs::metadata(path.join(USED_MARKER))
                .or_else(|_| std::fs::metadata(&path))
                .and_then(|meta| meta.modified())
                .ok()?;
            now.duration_since(used)
                .is_ok_and(|age| age >= idle)
                .then_some((path, used))
        })
        .collect()
}

/// Deletes other sessions' target directories under `root` that have not been used for `stale`.
/// Returns the removed directories. Errors are ignored: pruning is best effort.
pub fn prune_stale(root: &Path, keep_session: &str, stale: Duration) -> Vec<PathBuf> {
    idle_directories(root, keep_session, stale)
        .into_iter()
        .filter(|(path, _)| !cargo_build_running(path) && std::fs::remove_dir_all(path).is_ok())
        .map(|(path, _)| path)
        .collect()
}

#[cfg(test)]
mod tests;

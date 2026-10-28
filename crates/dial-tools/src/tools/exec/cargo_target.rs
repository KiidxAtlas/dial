//! Session-scoped Cargo build output for commands Dial runs.
//!
//! Each session gets one target directory under the workspace scratch dir, so concurrent sessions
//! stop contending for one `target/` lock. The directory is fixed per session, never per command.
//! An explicit `CARGO_TARGET_DIR` in the inherited environment is left alone, and a `--target-dir`
//! flag still wins because Cargo applies it after the environment. Copied workers have their own
//! root, so they get their own directory; shared-checkout workers share their session's.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

const TARGET_ROOT: &str = "cargo-target";
const MAX_SESSION_LEN: usize = 128;

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
        command.env("CARGO_TARGET_DIR", dir);
    }
}

#[cfg(test)]
mod tests;

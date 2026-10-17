use std::{
    fs,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use tokio::runtime::{Builder, Runtime};

use crate::computer::desktop::error::{CoreResult, DesktopError};

/// Process-wide Tokio runtime shared by every xdg-desktop-portal call.
///
/// `ashpd` caches a process-global D-Bus connection whose I/O tasks are bound
/// to the runtime that first creates it. A long-lived multi-thread runtime
/// keeps that connection alive and drives it while portal callers block.
static PORTAL_RUNTIME: LazyLock<Result<Runtime, String>> = LazyLock::new(|| {
    Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|err| err.to_string())
});

/// Borrow the shared portal runtime, surfacing a one-time build failure.
pub(super) fn portal_runtime() -> CoreResult<&'static Runtime> {
    PORTAL_RUNTIME
        .as_ref()
        .map_err(|err| DesktopError::internal(format!("xdg-desktop-portal runtime: {err}")))
}

/// File name of the `RemoteDesktop` restore token that pre-#7884 builds wrote
/// (world-readable) during read-only `computer` calls. Nothing reads it after
/// #7884 dropped the restore-token path.
const ORPHANED_REMOTE_DESKTOP_TOKEN: &str = "remote-desktop-token";

/// Resolves the `dial` state directory (`$XDG_STATE_HOME/dial` or
/// `~/.local/state/dial`) that holds portal tokens.
fn dial_state_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    Some(base.join("dial"))
}

fn remove_token_in(dir: &Path) {
    let _ = fs::remove_file(dir.join(ORPHANED_REMOTE_DESKTOP_TOKEN));
}

/// Best-effort removal of the orphaned `RemoteDesktop` restore token left
/// behind by pre-#7884 builds. Runs on Wayland backend construction; a missing
/// file is success, so it is safe to call on every session.
pub(super) fn remove_orphaned_remote_desktop_token() {
    if let Some(dir) = dial_state_dir() {
        remove_token_in(&dir);
    }
}

#[cfg(feature = "wayland-pipewire")]
fn token_path(name: &str) -> Option<PathBuf> {
    Some(dial_state_dir()?.join(name))
}

#[cfg(feature = "wayland-pipewire")]
pub(super) fn read_token(name: &str) -> Option<String> {
    fs::read_to_string(token_path(name)?)
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

#[cfg(feature = "wayland-pipewire")]
pub(super) fn store_token(name: &str, token: Option<&str>) {
    let (Some(path), Some(token)) = (token_path(name), token) else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_ok() {
        let _ = write_private(&path, token);
    }
}

/// Restore tokens grant screen capture without a prompt, so they are written
/// owner-only; the default umask would leave them world-readable.
#[cfg(feature = "wayland-pipewire")]
fn write_private(path: &Path, token: &str) -> std::io::Result<()> {
    use std::{
        io::Write,
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
    };
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(token.as_bytes())?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(test)]
mod tests;

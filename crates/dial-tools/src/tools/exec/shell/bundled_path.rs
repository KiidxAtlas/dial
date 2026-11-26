//! Shell PATH fallback to Dial's bundled CPython when the environment provides no `python3`.
//!
//! An image or host Python always wins: the bundled launcher directory is appended after every
//! existing PATH entry, and only when no `python3` resolves there. The interpreter is extracted
//! once per process under the host temp directory and removed when the process exits.

use crate::setup::deps::bundled_python::{self, Runtime};
use crate::tools::Workspace;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Once, OnceLock};

struct Shared {
    runtime: Runtime,
    launchers: PathBuf,
}

static SHARED: OnceLock<Option<Shared>> = OnceLock::new();
static CLEANUP: Once = Once::new();

/// Whether `program` resolves to a visible executable on `path` (relative entries are taken
/// against the workspace root, as the shell would from there).
pub(super) fn resolves(ws: &Workspace, path: Option<&OsStr>, program: &str) -> bool {
    let names: &[String] = &if cfg!(windows) {
        vec![format!("{program}.exe"), program.to_owned()]
    } else {
        vec![program.to_owned()]
    };
    path.map(std::env::split_paths)
        .into_iter()
        .flatten()
        .map(|dir| ws.root().join(dir))
        .any(|dir| {
            names.iter().any(|name| {
                crate::platform::fs::executable_candidate(&dir.join(name))
                    .is_some_and(|p| !ws.sandbox_hides(&p))
            })
        })
}

/// Whether this build could supply `python3` to a shell whose PATH resolves none.
pub(super) fn offered(ws: &Workspace, path: Option<&OsStr>) -> bool {
    bundled_python::available()
        && !resolves(ws, path, "python3")
        && super::super::kernel::bundled_runtime_parent(ws).is_ok()
}

/// The bundled runtime and the PATH to give the shell, when `path` resolves no `python3`.
/// Extraction failure is reported once on stderr and leaves the PATH unchanged.
pub(super) fn fallback(
    ws: &Workspace,
    path: Option<&OsStr>,
) -> Option<(&'static Runtime, OsString)> {
    if !offered(ws, path) {
        return None;
    }
    let shared = SHARED
        .get_or_init(|| {
            let provisioned = super::super::kernel::bundled_runtime_parent(ws)
                .and_then(|parent| {
                    bundled_python::provision_in(&parent).map_err(|e| format!("{e:#}"))
                })
                .and_then(|runtime| {
                    let launchers = runtime.shell_launchers().map_err(|e| format!("{e:#}"))?;
                    Ok(Shared { runtime, launchers })
                });
            match provisioned {
                Ok(shared) => {
                    register_cleanup();
                    Some(shared)
                }
                Err(error) => {
                    eprintln!("dial: bundled Python is unavailable to bash: {error}");
                    None
                }
            }
        })
        .as_ref()?;
    Some((&shared.runtime, appended(path, &shared.launchers)?))
}

/// `path` with `dir` as its last entry.
fn appended(path: Option<&OsStr>, dir: &Path) -> Option<OsString> {
    let mut entries: Vec<PathBuf> = path
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .collect();
    entries.push(dir.to_owned());
    std::env::join_paths(entries).ok()
}

/// Statics are never dropped, so the extracted tree is removed by an exit hook instead.
fn register_cleanup() {
    extern "C" fn remove_at_exit() {
        let _ = std::panic::catch_unwind(|| {
            if let Some(Some(shared)) = SHARED.get() {
                let _ = std::fs::remove_dir_all(shared.runtime.root());
            }
        });
    }
    // SAFETY: remove_at_exit has the required extern "C", no-argument ABI, lives for the process
    // lifetime, and catches Rust unwinding before returning to the C runtime.
    CLEANUP.call_once(|| unsafe {
        libc::atexit(remove_at_exit);
    });
}

#[cfg(test)]
mod tests;

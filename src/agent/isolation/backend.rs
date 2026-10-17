//! Chooses the backend that gives a `task` worker its private view of the project.
//!
//! Every backend is judged on the same contract: the worker sees the project's files, and only the
//! changes it makes are merged back. `auto` picks the first backend that the platform supports, in
//! a fixed per-platform order, and records why each earlier candidate was skipped. Probes are cheap
//! and cached for the life of the process. See `docs/subagents.md` for each backend's guarantees.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Preference {
    #[default]
    Auto,
    Clone,
    Overlay,
    Worktree,
    Copy,
    ProjFs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Backend {
    /// macOS `clonefile` copy-on-write clone of the project.
    Clone,
    /// Linux overlayfs mount over the project; changes are the upper directory.
    Overlay,
    /// Detached `git worktree` checkout of HEAD plus the project's uncommitted and ignored files.
    Worktree,
    /// Plain file-by-file copy of the project.
    Copy,
    /// Windows Projected File System: the project is projected read-only into the worker's dir.
    ProjFs,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Self::Clone => "clone",
            Self::Overlay => "overlay",
            Self::Worktree => "worktree",
            Self::Copy => "copy",
            Self::ProjFs => "projfs",
        }
    }
}

impl Preference {
    fn named(self) -> Option<Backend> {
        match self {
            Self::Auto => None,
            Self::Clone => Some(Backend::Clone),
            Self::Overlay => Some(Backend::Overlay),
            Self::Worktree => Some(Backend::Worktree),
            Self::Copy => Some(Backend::Copy),
            Self::ProjFs => Some(Backend::ProjFs),
        }
    }
}

/// `[isolation]` settings. Deserialized from the user's config file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IsolationConfig {
    pub backend: Preference,
}

/// The chosen backend plus every automatic candidate skipped before it, with the reason.
#[derive(Debug, PartialEq, Eq)]
pub struct Selection {
    pub backend: Backend,
    pub skipped: Vec<(Backend, String)>,
}

impl Selection {
    /// One-line status: the backend in use, then any candidates skipped and why.
    pub fn label(&self) -> String {
        let mut label = self.backend.name().to_owned();
        for (backend, reason) in &self.skipped {
            label.push_str(&format!("; {} skipped ({reason})", backend.name()));
        }
        label
    }
}

/// Automatic order for an operating system name (`std::env::consts::OS`). Copy is the universal
/// last resort. Overlay on Linux hides secret paths (for example `.env`) and leaves dependency
/// directories visible, as `docs/subagents.md` documents.
fn auto_order_for(os: &str) -> &'static [Backend] {
    match os {
        "linux" => &[Backend::Overlay, Backend::Worktree, Backend::Copy],
        "windows" => &[Backend::ProjFs, Backend::Worktree, Backend::Copy],
        "macos" => &[Backend::Clone, Backend::Worktree, Backend::Copy],
        _ => &[Backend::Copy],
    }
}

/// Pick a backend. `probe` reports whether a backend can run here; an `Err` carries the reason.
/// A named preference never falls back silently: an unavailable backend is an error.
pub fn select(
    preference: Preference,
    probe: impl Fn(Backend) -> Result<(), String>,
) -> Result<Selection, String> {
    select_from(preference, auto_order_for(std::env::consts::OS), probe)
}

fn select_from(
    preference: Preference,
    order: &[Backend],
    probe: impl Fn(Backend) -> Result<(), String>,
) -> Result<Selection, String> {
    if let Some(backend) = preference.named() {
        probe(backend).map_err(|reason| {
            format!(
                "isolation backend `{}` is unavailable: {reason}. Set [isolation] backend = \"auto\" to use an available backend",
                backend.name()
            )
        })?;
        return Ok(Selection {
            backend,
            skipped: Vec::new(),
        });
    }
    let mut skipped = Vec::new();
    for &backend in order {
        match probe(backend) {
            Ok(()) => return Ok(Selection { backend, skipped }),
            Err(reason) => skipped.push((backend, reason)),
        }
    }
    Err("no isolation backend is available on this platform".into())
}

/// Process-wide probe results, keyed by backend, project root, and entry budget. Probing worktree
/// runs git and overlay scans the project, so each answer is computed once per project and budget
/// for the life of the process.
static PROBES: Mutex<BTreeMap<ProbeKey, Result<(), String>>> = Mutex::new(BTreeMap::new());

/// Backend, project root, and entry budget a probe result was computed for.
type ProbeKey = (Backend, PathBuf, usize);

/// Whether `backend` can run for `project` on this platform. `max_entries` is the
/// `isolation_source_files` budget that overlay's secret scan must fit in. Cached per process.
pub fn probe(backend: Backend, project: &Path, max_entries: usize) -> Result<(), String> {
    cached(backend, project, max_entries, || {
        probe_uncached(backend, project, max_entries)
    })
}

fn cached(
    backend: Backend,
    project: &Path,
    max_entries: usize,
    probe: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let key = (backend, project.to_path_buf(), max_entries);
    let hit = PROBES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
        .cloned();
    if let Some(result) = hit {
        return result;
    }
    let result = probe();
    PROBES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key, result.clone());
    result
}

fn probe_uncached(backend: Backend, project: &Path, max_entries: usize) -> Result<(), String> {
    match backend {
        Backend::Clone => {
            if cfg!(target_os = "macos") {
                Ok(())
            } else {
                Err("clone uses macOS clonefile".into())
            }
        }
        Backend::Copy => Ok(()),
        Backend::Worktree => super::worktree::probe(project),
        Backend::Overlay => super::overlay::probe(project, max_entries),
        #[cfg(windows)]
        Backend::ProjFs => super::projfs::probe(),
        #[cfg(not(windows))]
        Backend::ProjFs => Err("ProjFS requires Windows".into()),
    }
}

#[cfg(test)]
mod tests;

//! Resource discovery and change detection (skills, commands, settings) for the live UI.

use crate::config::paths::Layout;
use crate::config::settings_store;
use anyhow::{Context, Result, ensure};
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, Instant};

/// Poll discovery metadata on the UI heartbeat, never read private TOML every frame.
pub(super) const RESOURCE_TTL: Duration = Duration::from_secs(2);

const RESOURCE_SCAN_CAP: usize = 4096;

const RESOURCE_SCAN_TIME: Duration = Duration::from_millis(100);

/// Heartbeat fallback when `notify` covers every resource input; the watcher marks changes immediately.
pub(super) const RESOURCE_WATCHED_FALLBACK: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// The shell: everything visible and interactive
// ---------------------------------------------------------------------------

/// Includes inode and change time, so atomic replacement and same-sized edits are observable.
fn hash_resource(path: &Path, hash: &mut DefaultHasher) -> Result<bool> {
    path.hash(hash);
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            #[cfg(unix)]
            (
                meta.dev(),
                meta.ino(),
                meta.len(),
                meta.mtime(),
                meta.mtime_nsec(),
                meta.ctime(),
                meta.ctime_nsec(),
                meta.mode(),
                meta.uid(),
                meta.nlink(),
            )
                .hash(hash);
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                (
                    meta.len(),
                    meta.last_write_time(),
                    meta.creation_time(),
                    meta.file_attributes(),
                )
                    .hash(hash);
                if !meta.file_type().is_symlink() {
                    crate::platform::fs::path_identity(path)?.hash(hash);
                }
            }
            if meta.file_type().is_symlink() {
                fs::read_link(path)?.hash(hash);
                if let Ok(target) = fs::metadata(path) {
                    #[cfg(unix)]
                    (
                        target.dev(),
                        target.ino(),
                        target.len(),
                        target.mtime(),
                        target.mtime_nsec(),
                        target.ctime(),
                        target.ctime_nsec(),
                    )
                        .hash(hash);
                    #[cfg(windows)]
                    {
                        use std::os::windows::fs::MetadataExt;
                        (
                            target.len(),
                            target.last_write_time(),
                            target.creation_time(),
                            target.file_attributes(),
                        )
                            .hash(hash);
                        let file = std::fs::File::open(path)?;
                        crate::platform::fs::identity(&file)?.hash(hash);
                    }
                }
            }
            Ok(meta.is_dir())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            0u8.hash(hash);
            Ok(false)
        }
        Err(error) => Err(error).with_context(|| format!("cannot inspect {}", path.display())),
    }
}

pub(super) fn settings_fingerprint(layout: &Layout) -> Result<u64> {
    let mut hash = DefaultHasher::new();
    for path in layout.config_files() {
        hash_resource(&path, &mut hash)?;
    }
    Ok(hash.finish())
}

/// Files hashed by `resources_fingerprint` and watched by `resource_events`.
fn resource_files(layout: &Layout) -> [std::path::PathBuf; 10] {
    [
        layout.user_dir.join("keybindings.toml"),
        layout.user_dir.join("skills.json"),
        layout.state_dir.join("skills.json"),
        layout.user_dir.join("SOUL.md"),
        layout.project_dir.join("SYSTEM.md"),
        layout.project_dir.join("APPEND_SYSTEM.md"),
        layout.project_dir.join("AGENTS.md"),
        layout.project_root.join("AGENTS.md"),
        layout.project_root.join("CLAUDE.md"),
        layout.project_root.join(".cursorrules"),
    ]
}

/// Directories hashed by `resources_fingerprint`; the flag marks skill roots (their `SKILL.md` counts).
fn resource_dirs(layout: &Layout) -> [(std::path::PathBuf, bool); 5] {
    [
        (layout.project_dir.join("commands"), false),
        (layout.user_dir.join("commands"), false),
        (layout.user_dir.join("themes"), false),
        (layout.project_dir.join("skills"), true),
        (layout.user_dir.join("skills"), true),
    ]
}

/// Poll discovery inputs, not the entire workspace or bundled skill trees.
pub(super) fn resources_fingerprint(layout: &Layout) -> Result<u64> {
    let started = Instant::now();
    let mut hash = DefaultHasher::new();
    let mut count = 0;
    for path in resource_files(layout) {
        hash_resource(&path, &mut hash)?;
    }
    for (dir, skill_root) in resource_dirs(layout) {
        if !hash_resource(&dir, &mut hash)? {
            continue;
        }
        let mut paths = Vec::new();
        for entry in fs::read_dir(&dir).with_context(|| format!("cannot scan {}", dir.display()))? {
            count += 1;
            ensure!(
                count <= RESOURCE_SCAN_CAP && started.elapsed() < RESOURCE_SCAN_TIME,
                "resource change scan exceeded its bounded budget; automatic refresh is incomplete"
            );
            paths.push(entry?.path());
        }
        paths.sort();
        for path in paths {
            ensure!(
                started.elapsed() < RESOURCE_SCAN_TIME,
                "resource change scan exceeded its bounded budget; automatic refresh is incomplete"
            );
            if hash_resource(&path, &mut hash)? && skill_root {
                hash_resource(&path.join("SKILL.md"), &mut hash)?;
            }
        }
    }
    Ok(hash.finish())
}

pub(super) struct ResourceWatch {
    pub(super) at: Instant,
    pub(super) settings: Option<u64>,
    pub(super) resources: Option<u64>,
    pub(super) diagnostic: Option<String>,
    pub(super) reload: settings_store::SettingsReload,
    pub(super) events: Option<ResourceEvents>,
}

/// Disk-change signal from `notify`. `complete` means every resource input has a live watch, so the
/// heartbeat poll drops to `RESOURCE_WATCHED_FALLBACK`; otherwise it stays at `RESOURCE_TTL`.
pub(in crate::app) struct ResourceEvents {
    pub(super) dirty: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub(super) complete: bool,
    _watcher: notify::RecommendedWatcher,
}

/// Watch the same inputs that `settings_fingerprint` and `resources_fingerprint` hash. Returns `None`
/// only when the platform watcher cannot start; partial coverage still signals and keeps TTL polling.
pub(super) fn resource_events(layout: &Layout) -> Option<ResourceEvents> {
    use notify::{RecursiveMode, Watcher};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let dirty = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&dirty);
    let mut watcher = notify::recommended_watcher(move |_: notify::Result<notify::Event>| {
        flag.store(true, Ordering::Release);
    })
    .ok()?;
    let mut complete = true;
    let mut paths: Vec<(std::path::PathBuf, bool)> = layout
        .config_files()
        .into_iter()
        .chain(resource_files(layout))
        .map(|path| (path, false))
        .collect();
    paths.extend(
        resource_dirs(layout)
            .into_iter()
            .map(|(dir, _)| (dir, true)),
    );
    for (path, is_dir) in paths {
        let (target, mode) = if is_dir && path.is_dir() {
            (path, RecursiveMode::Recursive)
        } else {
            // Missing directories are seen through their parent; their contents stay on the TTL fallback.
            complete &= !is_dir;
            let Some(parent) = path.parent() else {
                complete = false;
                continue;
            };
            (parent.to_path_buf(), RecursiveMode::NonRecursive)
        };
        if !target.is_dir() || watcher.watch(&target, mode).is_err() {
            complete = false;
        }
    }
    Some(ResourceEvents {
        dirty,
        complete,
        _watcher: watcher,
    })
}

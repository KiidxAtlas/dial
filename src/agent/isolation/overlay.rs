//! Linux overlayfs isolation backend.
//!
//! The worker sees the project through a copy-on-write mount: reads fall through to the project,
//! writes land in an upper directory. That upper directory is the change set. Whiteouts (deleted
//! names) and opaque directories (replaced directories) are read from it, so a merge applies
//! exactly what the worker did. The kernel mount is tried first; `fuse-overlayfs` is the fallback
//! for unprivileged users.
//!
//! Secret and private entries (`.env*`, keys, credentials, `.git`, `.dial`) are hidden: before
//! mounting, a whiteout is created in the upper directory for each one, so the worker sees no
//! trace of them. Build outputs and installed dependencies (`target`, `node_modules`, venvs, and
//! the like) stay visible through the lower layer; writes to them land in the upper directory and
//! are never merged back, as the copy backends exclude them from their change sets. Overlay copies
//! nothing, so it is the first choice on Linux.

use super::{Snapshot, stamp};
use crate::agent::agent_defs::TaskConfig;
use anyhow::{Context, Result, ensure};
use std::{
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Whether an overlay mount can work for `root` here: Linux with overlayfs in the kernel or
/// `fuse-overlayfs` on PATH, whiteouts that hide excluded files can be created for the driver
/// chosen, and the project's secret scan fits in `max_entries`.
pub(super) fn probe(root: &Path, max_entries: usize) -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Err("overlay isolation needs Linux".into());
    }
    let kernel = kernel_present();
    let fuse = on_path("fuse-overlayfs");
    if !kernel && !fuse {
        return Err("needs overlayfs in the kernel or fuse-overlayfs on PATH".into());
    }
    select_driver(kernel, fuse, can_hide_in_scratch)?;
    scan_for_probe(root, max_entries)
}

/// The probe's view of the secret scan. A budget overflow is a skip reason, not a mount error.
fn scan_for_probe(root: &Path, max_entries: usize) -> Result<(), String> {
    excluded_entries(root, max_entries)
        .map(|_| ())
        .map_err(|error| match error {
            ScanError::TooMany(_) => "too many entries to scan for secrets".to_owned(),
            ScanError::Failed(message) => message,
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Driver {
    Kernel,
    Fuse,
}

/// A mounted overlay view. Unmounts on drop.
pub(super) struct Mount {
    merged: PathBuf,
    upper: PathBuf,
    driver: Driver,
    mounted: bool,
}

impl Mount {
    pub(super) fn upper(&self) -> &Path {
        &self.upper
    }

    /// Unmount the view. Calling this again after success does nothing.
    pub(super) fn unmount(&mut self) -> Result<(), String> {
        if !self.mounted {
            return Ok(());
        }
        match self.driver {
            Driver::Kernel => kernel_unmount(&self.merged)?,
            Driver::Fuse => fuse_unmount(&self.merged)?,
        }
        self.mounted = false;
        Ok(())
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        if let Err(error) = self.unmount() {
            eprintln!("dial: could not unmount isolated view: {error}");
        }
    }
}

/// Mount an overlay of `lower` under `base`. `base` gets `upper`, `work` and `merged` directories.
/// Entries of `lower` that the copy policy excludes are hidden first: a whiteout for each one is
/// created in `upper` before the mount, so the worker sees no trace of them.
pub(super) fn mount(lower: &Path, base: &Path, max_entries: usize) -> Result<Mount, String> {
    let upper = base.join("upper");
    let work = base.join("work");
    let merged = base.join("merged");
    for dir in [&upper, &work, &merged] {
        fs::create_dir(dir).map_err(|error| format!("creating {}: {error}", dir.display()))?;
    }
    let excluded = excluded_entries(lower, max_entries).map_err(|error| error.to_string())?;
    let options = format!(
        "lowerdir={},upperdir={},workdir={}",
        mount_path(lower)?,
        mount_path(&upper)?,
        mount_path(&work)?
    );
    let mut driver = select_driver(
        kernel_present(),
        on_path("fuse-overlayfs"),
        can_hide_in_scratch,
    )?;
    if let Err(first) = mount_with(driver, &options, &merged, &upper, &excluded) {
        if driver != Driver::Kernel {
            return Err(first);
        }
        if !on_path("fuse-overlayfs") {
            return Err(format!(
                "kernel overlay mount failed ({first}) and fuse-overlayfs is not on PATH"
            ));
        }
        remove_masks(&upper, &excluded, Driver::Kernel)
            .map_err(|error| format!("removing kernel whiteouts: {error}"))?;
        driver = Driver::Fuse;
        mount_with(driver, &options, &merged, &upper, &excluded)
            .map_err(|fuse| format!("kernel overlay mount failed ({first}); {fuse}"))?;
    }
    Ok(Mount {
        merged,
        upper,
        driver,
        mounted: true,
    })
}

/// Overlay mount options split on `,` and `:`, so paths containing either cannot be expressed.
fn mount_path(path: &Path) -> Result<&str, String> {
    let text = path.to_str().ok_or_else(|| {
        format!(
            "{} is not UTF-8, which overlay mount options require",
            path.display()
        )
    })?;
    if text.contains([',', ':']) {
        return Err(format!(
            "{} contains ',' or ':', which overlay mount options cannot represent",
            path.display()
        ));
    }
    Ok(text)
}

#[cfg(target_os = "linux")]
fn kernel_mount(options: &str, merged: &Path) -> Result<(), String> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let target = CString::new(merged.as_os_str().as_bytes())
        .map_err(|_| "mount point contains a NUL byte".to_owned())?;
    let data = CString::new(options).map_err(|_| "mount options contain a NUL byte".to_owned())?;
    // SAFETY: all pointers are valid NUL-terminated strings for the duration of the call.
    let rc = unsafe {
        libc::mount(
            c"overlay".as_ptr(),
            target.as_ptr(),
            c"overlay".as_ptr(),
            0,
            data.as_ptr().cast(),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

#[cfg(not(target_os = "linux"))]
fn kernel_mount(_options: &str, _merged: &Path) -> Result<(), String> {
    Err("kernel overlayfs needs Linux".into())
}

#[cfg(target_os = "linux")]
fn kernel_unmount(merged: &Path) -> Result<(), String> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let target = CString::new(merged.as_os_str().as_bytes())
        .map_err(|_| "mount point contains a NUL byte".to_owned())?;
    // SAFETY: `target` is a valid NUL-terminated string for the duration of the call.
    let rc = unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) };
    if rc == 0 {
        Ok(())
    } else {
        Err(format!(
            "unmounting {}: {}",
            merged.display(),
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(not(target_os = "linux"))]
fn kernel_unmount(_merged: &Path) -> Result<(), String> {
    Err("kernel overlayfs needs Linux".into())
}

fn fuse_mount(options: &str, merged: &Path) -> Result<(), String> {
    let output = Command::new("fuse-overlayfs")
        .arg("-o")
        .arg(options)
        .arg(merged)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("fuse-overlayfs failed to start: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "fuse-overlayfs failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn fuse_unmount(merged: &Path) -> Result<(), String> {
    let mut last_error = String::new();
    for tool in ["fusermount3", "fusermount"] {
        match Command::new(tool)
            .arg("-u")
            .arg(merged)
            .stdin(Stdio::null())
            .output()
        {
            Ok(output) if output.status.success() => return Ok(()),
            Ok(output) => {
                last_error = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            }
            Err(error) => last_error = error.to_string(),
        }
    }
    Err(format!("unmounting {}: {last_error}", merged.display()))
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// Whether the kernel lists `overlay` among its filesystems.
fn kernel_present() -> bool {
    fs::read_to_string("/proc/filesystems").is_ok_and(|list| {
        list.lines()
            .any(|line| line.split_whitespace().last() == Some("overlay"))
    })
}

/// The mount driver to use, in preference order: kernel overlayfs, then fuse-overlayfs. A driver
/// is chosen only when it is present and whiteouts can be created for it; otherwise its reason is
/// reported.
fn select_driver(
    kernel: bool,
    fuse: bool,
    can_hide: impl Fn(Driver) -> Result<(), String>,
) -> Result<Driver, String> {
    let mut reasons = Vec::new();
    for (driver, present, name) in [
        (Driver::Kernel, kernel, "kernel overlayfs"),
        (Driver::Fuse, fuse, "fuse-overlayfs"),
    ] {
        if !present {
            reasons.push(format!("{name} is not available"));
        } else {
            match can_hide(driver) {
                Ok(()) => return Ok(driver),
                Err(reason) => reasons.push(format!("{name}: {reason}")),
            }
        }
    }
    Err(reasons.join("; "))
}

/// Whether whiteouts can be created for `driver`, checked in a scratch directory. Failure means
/// this mount type cannot hide excluded files.
fn can_hide_in_scratch(driver: Driver) -> Result<(), String> {
    let scratch =
        tempfile::tempdir().map_err(|error| format!("cannot hide excluded files: {error}"))?;
    hide_entries(scratch.path(), &[PathBuf::from(".env")], driver)
        .map_err(|error| format!("cannot hide excluded files: {error}"))
}

/// Why the secret scan of a project did not finish.
#[derive(Debug)]
enum ScanError {
    /// The project has more entries than `isolation_source_files` allows.
    TooMany(usize),
    /// A directory could not be read.
    Failed(String),
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooMany(max) => write!(
                f,
                "project has more than {max} entries; increase task.isolation_source_files"
            ),
            Self::Failed(message) => f.write_str(message),
        }
    }
}

/// Top-most secret entries of `root` (`.env*`, keys, credentials, `.git`, `.dial` and the like),
/// relative to `root`. Neither secret nor dependency directories are entered. Dependency
/// directories (`target`, `node_modules`, and the like) are not walked because the copy backends do
/// not walk them either, and walking them would make overlay slow on the large trees it exists for.
/// `max_entries` bounds the entries listed.
fn excluded_entries(root: &Path, max_entries: usize) -> Result<Vec<PathBuf>, ScanError> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = pending.pop() {
        let listing = fs::read_dir(&dir)
            .map_err(|error| ScanError::Failed(format!("reading {}: {error}", dir.display())))?;
        for entry in listing {
            let entry = entry.map_err(|error| {
                ScanError::Failed(format!("reading {}: {error}", dir.display()))
            })?;
            visited += 1;
            if visited > max_entries {
                return Err(ScanError::TooMany(max_entries));
            }
            let path = entry.path();
            if crate::benchmark::is_secret_source_path(root, &path) {
                let relative = path.strip_prefix(root).map_err(|_| {
                    ScanError::Failed("project entry outside the project root".to_owned())
                })?;
                found.push(relative.to_path_buf());
                continue;
            }
            if crate::benchmark::is_dependency_source_path(root, &path) {
                continue;
            }
            let file_type = entry.file_type().map_err(|error| {
                ScanError::Failed(format!("reading {}: {error}", path.display()))
            })?;
            if file_type.is_dir() {
                pending.push(path);
            }
        }
    }
    Ok(found)
}

/// Where the whiteout hiding `relative` goes inside `upper`: the entry's own name for a kernel
/// whiteout, or `.wh.<name>` beside it for fuse-overlayfs.
fn mask_path(upper: &Path, relative: &Path, driver: Driver) -> PathBuf {
    let path = upper.join(relative);
    match driver {
        Driver::Kernel => path,
        Driver::Fuse => {
            let mut name = OsString::from(FUSE_WHITEOUT_PREFIX);
            name.push(relative.file_name().unwrap_or_default());
            path.with_file_name(name)
        }
    }
}

/// Create one whiteout per excluded entry in `upper`, so the mount hides each one from `lower`.
fn hide_entries(upper: &Path, entries: &[PathBuf], driver: Driver) -> std::io::Result<()> {
    for relative in entries {
        let path = mask_path(upper, relative, driver);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        match driver {
            Driver::Kernel => make_whiteout(&path)?,
            Driver::Fuse => {
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
            }
        }
    }
    Ok(())
}

/// Remove the whiteouts `hide_entries` made for `driver`. Missing ones are not an error.
fn remove_masks(upper: &Path, entries: &[PathBuf], driver: Driver) -> std::io::Result<()> {
    for relative in entries {
        match fs::remove_file(mask_path(upper, relative, driver)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// A kernel whiteout: a character device 0:0, which needs `CAP_MKNOD`.
#[cfg(target_os = "linux")]
fn make_whiteout(path: &Path) -> std::io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let target = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains a NUL byte")
    })?;
    // SAFETY: `target` is a valid NUL-terminated path for the duration of the call; device 0 is 0:0.
    let rc = unsafe { libc::mknod(target.as_ptr(), libc::S_IFCHR | 0o600, 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn make_whiteout(_path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "kernel whiteouts need Linux",
    ))
}

/// Hide the excluded entries for `driver`, then mount. Shared by the first attempt and the
/// fuse-overlayfs fallback.
fn mount_with(
    driver: Driver,
    options: &str,
    merged: &Path,
    upper: &Path,
    excluded: &[PathBuf],
) -> Result<(), String> {
    hide_entries(upper, excluded, driver)
        .map_err(|error| format!("cannot hide excluded files: {error}"))?;
    match driver {
        Driver::Kernel => kernel_mount(options, merged),
        Driver::Fuse => fuse_mount(options, merged),
    }
}

/// The change set of an overlay view, as a snapshot comparable with `before`.
///
/// Starts from `before` and applies the upper directory: a regular file is stamped with its
/// current content; a whiteout removes that name and everything under it; an opaque directory
/// hides every `before` entry below it.
pub(super) fn changed_snapshot(
    upper: &Path,
    before: &Snapshot,
    limits: &TaskConfig,
) -> Result<Snapshot> {
    let mut after = before.clone();
    let mut pending = vec![upper.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
            let entry = entry?;
            visited += 1;
            ensure!(
                visited <= limits.isolation_source_files,
                "isolated copy has more than {} changed entries",
                limits.isolation_source_files
            );
            let path = entry.path();
            if crate::benchmark::is_excluded_source_path(upper, &path) {
                continue;
            }
            let relative = path
                .strip_prefix(upper)
                .context("overlay upper entry outside the upper directory")?
                .to_path_buf();
            let meta = fs::symlink_metadata(&path)?;
            if is_whiteout(&meta) {
                remove_subtree(&mut after, &relative);
            } else if meta.is_dir() {
                if is_opaque(&path) || is_fuse_opaque(&path) {
                    remove_subtree(&mut after, &relative);
                }
                pending.push(path);
            } else if meta.is_file() {
                let name = entry.file_name();
                if let Some(hidden) = fuse_whiteout_target(&name) {
                    let hidden = relative.with_file_name(hidden);
                    // Masks hide excluded entries before the mount; they never delete anything.
                    if !crate::benchmark::is_excluded_source_path(upper, &upper.join(&hidden)) {
                        remove_subtree(&mut after, &hidden);
                    }
                } else if name.as_os_str() != FUSE_OPAQUE_MARKER {
                    match stamp(&path)? {
                        Some(stamp) => {
                            after.insert(relative, stamp);
                        }
                        None => {
                            after.remove(&relative);
                        }
                    }
                }
            }
        }
    }
    Ok(after)
}

/// Drop `relative` and every entry below it. `Path::starts_with` compares whole components, so
/// `a` does not match `ab`.
fn remove_subtree(snapshot: &mut Snapshot, relative: &Path) {
    snapshot.retain(|key, _| !key.starts_with(relative));
}

/// A whiteout is a character device 0:0 in the upper directory.
#[cfg(unix)]
fn is_whiteout(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    meta.file_type().is_char_device() && meta.rdev() == 0
}

#[cfg(not(unix))]
fn is_whiteout(_meta: &fs::Metadata) -> bool {
    false
}

/// Kernel overlayfs marks replaced directories with the `trusted.overlay.opaque` xattr set to `y`.
#[cfg(target_os = "linux")]
fn is_opaque(dir: &Path) -> bool {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let Ok(path) = CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    let mut value = [0u8; 1];
    // SAFETY: `path` is NUL-terminated and `value` has room for the one byte requested.
    let read = unsafe {
        libc::getxattr(
            path.as_ptr(),
            c"trusted.overlay.opaque".as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    read == 1 && value[0] == b'y'
}

#[cfg(not(target_os = "linux"))]
fn is_opaque(_dir: &Path) -> bool {
    false
}

/// fuse-overlayfs records a deleted name as a regular file `.wh.<name>` and a replaced directory
/// as a `.wh..wh..opq` file inside it. Both are reserved names, never part of the change set.
const FUSE_WHITEOUT_PREFIX: &str = ".wh.";
const FUSE_OPAQUE_MARKER: &str = ".wh..wh..opq";

/// The entry hidden by a fuse-overlayfs whiteout file, or `None` when `name` is not one.
#[cfg(unix)]
fn fuse_whiteout_target(name: &OsStr) -> Option<OsString> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let rest = name
        .as_bytes()
        .strip_prefix(FUSE_WHITEOUT_PREFIX.as_bytes())?;
    // `.wh..wh..opq` and other reserved `.wh.` names are markers, not whiteouts.
    if rest.is_empty() || rest.starts_with(FUSE_WHITEOUT_PREFIX.as_bytes()) {
        return None;
    }
    Some(OsString::from_vec(rest.to_vec()))
}

#[cfg(not(unix))]
fn fuse_whiteout_target(_name: &OsStr) -> Option<OsString> {
    None
}

/// Whether a fuse-overlayfs directory hides every `before` entry below it.
fn is_fuse_opaque(dir: &Path) -> bool {
    fs::symlink_metadata(dir.join(FUSE_OPAQUE_MARKER)).is_ok_and(|meta| meta.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp_of(path: &Path) -> crate::agent::isolation::FileStamp {
        stamp(path).unwrap().unwrap()
    }

    #[test]
    fn remove_subtree_drops_the_name_and_its_children_only() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        fs::write(&file, "x").unwrap();
        let mut snapshot = Snapshot::new();
        for key in ["a", "a/x.txt", "ab/y.txt", "b.txt"] {
            snapshot.insert(PathBuf::from(key), stamp_of(&file));
        }
        remove_subtree(&mut snapshot, Path::new("a"));
        let keys: Vec<_> = snapshot
            .keys()
            .map(|key| key.display().to_string())
            .collect();
        assert_eq!(keys, vec!["ab/y.txt", "b.txt"]);
    }

    #[test]
    fn changed_snapshot_layers_upper_files_over_the_base() {
        let upper = tempfile::tempdir().unwrap();
        fs::write(upper.path().join("new.txt"), "new").unwrap();
        let base = tempfile::tempdir().unwrap();
        let kept = base.path().join("kept.txt");
        fs::write(&kept, "kept").unwrap();
        let mut before = Snapshot::new();
        before.insert(PathBuf::from("kept.txt"), stamp_of(&kept));
        let after = changed_snapshot(upper.path(), &before, &TaskConfig::default()).unwrap();
        let keys: Vec<_> = after.keys().map(|key| key.display().to_string()).collect();
        assert_eq!(keys, vec!["kept.txt", "new.txt"]);
    }

    #[test]
    fn changed_snapshot_is_bounded_by_the_entry_budget() {
        let upper = tempfile::tempdir().unwrap();
        fs::write(upper.path().join("a.txt"), "a").unwrap();
        fs::write(upper.path().join("b.txt"), "b").unwrap();
        let limits = TaskConfig {
            isolation_source_files: 1,
            ..TaskConfig::default()
        };
        let error = changed_snapshot(upper.path(), &Snapshot::new(), &limits).unwrap_err();
        assert!(format!("{error:#}").contains("more than 1 changed entries"));
    }

    #[cfg(unix)]
    #[test]
    fn fuse_whiteout_names_map_back_to_the_hidden_entry() {
        assert_eq!(
            fuse_whiteout_target(OsStr::new(".wh.gone.txt")),
            Some(OsString::from("gone.txt"))
        );
        assert_eq!(
            fuse_whiteout_target(OsStr::new(".wh.dir")),
            Some(OsString::from("dir"))
        );
        assert_eq!(fuse_whiteout_target(OsStr::new(FUSE_OPAQUE_MARKER)), None);
        assert_eq!(fuse_whiteout_target(OsStr::new(".wh.")), None);
        assert_eq!(fuse_whiteout_target(OsStr::new(".whiteboard")), None);
        assert_eq!(fuse_whiteout_target(OsStr::new("plain.txt")), None);
    }

    #[cfg(unix)]
    #[test]
    fn fuse_whiteouts_and_opaque_markers_shape_the_change_set() {
        let upper = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let kept = base.path().join("kept.txt");
        fs::write(&kept, "kept").unwrap();
        fs::write(upper.path().join(".wh.gone.txt"), "").unwrap();
        fs::write(upper.path().join(".wh.olddir"), "").unwrap();
        fs::create_dir(upper.path().join("replaced")).unwrap();
        fs::write(upper.path().join("replaced").join(FUSE_OPAQUE_MARKER), "").unwrap();
        fs::write(upper.path().join("replaced/new.txt"), "new").unwrap();
        let mut before = Snapshot::new();
        for key in ["gone.txt", "olddir/x.txt", "replaced/old.txt", "kept.txt"] {
            before.insert(PathBuf::from(key), stamp_of(&kept));
        }
        let after = changed_snapshot(upper.path(), &before, &TaskConfig::default()).unwrap();
        let keys: Vec<_> = after.keys().map(|key| key.display().to_string()).collect();
        assert_eq!(keys, vec!["kept.txt", "replaced/new.txt"]);
    }

    #[test]
    fn mount_path_rejects_separators_that_mount_options_cannot_carry() {
        assert!(mount_path(Path::new("/tmp/project")).is_ok());
        assert!(
            mount_path(Path::new("/tmp/a,b"))
                .unwrap_err()
                .contains("cannot represent")
        );
        assert!(
            mount_path(Path::new("/tmp/a:b"))
                .unwrap_err()
                .contains("cannot represent")
        );
    }

    #[test]
    fn excluded_entries_are_the_top_most_secret_matches() {
        let lower = tempfile::tempdir().unwrap();
        let root = lower.path();
        fs::write(root.join(".env"), "secret").unwrap();
        fs::write(root.join(".env.example"), "ok").unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/.env.local"), "secret").unwrap();
        fs::write(root.join("sub/keep.txt"), "keep").unwrap();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir(root.join(".dial")).unwrap();
        let mut found = excluded_entries(root, 1000).unwrap();
        found.sort();
        assert_eq!(
            found,
            vec![
                PathBuf::from(".dial"),
                PathBuf::from(".env"),
                PathBuf::from(".git"),
                PathBuf::from("sub/.env.local"),
            ]
        );
    }

    #[test]
    fn excluded_entries_do_not_enter_dependency_dirs() {
        let lower = tempfile::tempdir().unwrap();
        let root = lower.path();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("node_modules/pkg/index.js"), "dep").unwrap();
        fs::write(root.join("node_modules/.env"), "not masked: not entered").unwrap();
        fs::create_dir_all(root.join("target/deep")).unwrap();
        fs::write(root.join("target/deep/out.bin"), "bin").unwrap();
        // Two top-level entries fit the budget; walking into either directory would not.
        assert_eq!(excluded_entries(root, 2).unwrap(), Vec::<PathBuf>::new());
    }

    #[test]
    fn excluded_entries_are_bounded_by_the_entry_budget() {
        let lower = tempfile::tempdir().unwrap();
        fs::write(lower.path().join("a.txt"), "a").unwrap();
        fs::write(lower.path().join("b.txt"), "b").unwrap();
        let error = excluded_entries(lower.path(), 1).unwrap_err();
        assert!(matches!(error, ScanError::TooMany(1)), "{error}");
    }

    #[test]
    fn probe_scan_skips_overlay_when_the_project_is_over_budget() {
        let lower = tempfile::tempdir().unwrap();
        fs::write(lower.path().join("a.txt"), "a").unwrap();
        fs::write(lower.path().join("b.txt"), "b").unwrap();
        assert_eq!(
            scan_for_probe(lower.path(), 1),
            Err("too many entries to scan for secrets".to_owned())
        );
        assert_eq!(scan_for_probe(lower.path(), 2), Ok(()));
    }

    #[test]
    fn fuse_masks_are_wh_files_beside_each_excluded_entry() {
        let upper = tempfile::tempdir().unwrap();
        let entries = [
            PathBuf::from(".env"),
            PathBuf::from("sub/.env.local"),
            PathBuf::from(".ssh"),
        ];
        hide_entries(upper.path(), &entries, Driver::Fuse).unwrap();
        assert!(upper.path().join(".wh..env").is_file());
        assert!(upper.path().join("sub/.wh..env.local").is_file());
        assert!(upper.path().join(".wh..ssh").is_file());
        assert!(!upper.path().join(".env").exists());
    }

    #[test]
    fn mask_paths_match_each_encoding() {
        assert_eq!(
            mask_path(Path::new("/u"), Path::new("sub/.env"), Driver::Kernel),
            PathBuf::from("/u/sub/.env")
        );
        assert_eq!(
            mask_path(Path::new("/u"), Path::new("sub/.env"), Driver::Fuse),
            PathBuf::from("/u/sub/.wh..env")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn kernel_masks_are_char_devices_when_mknod_is_allowed() {
        let upper = tempfile::tempdir().unwrap();
        match hide_entries(upper.path(), &[PathBuf::from("sub/.env")], Driver::Kernel) {
            Ok(()) => assert!(is_whiteout(
                &fs::symlink_metadata(upper.path().join("sub/.env")).unwrap()
            )),
            Err(error) => assert_eq!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied,
                "unexpected error: {error}"
            ),
        }
    }

    #[test]
    fn select_driver_moves_past_a_driver_that_cannot_hide_entries() {
        let chosen = select_driver(true, true, |driver| match driver {
            Driver::Kernel => Err("cannot hide excluded files: mknod denied".into()),
            Driver::Fuse => Ok(()),
        });
        assert_eq!(chosen, Ok(Driver::Fuse));
    }

    #[test]
    fn select_driver_reports_the_mask_failure_when_no_driver_can_hide() {
        let error = select_driver(true, true, |_| {
            Err("cannot hide excluded files: denied".into())
        })
        .unwrap_err();
        assert!(error.contains("cannot hide excluded files"), "{error}");
        assert!(error.contains("kernel overlayfs"), "{error}");
        assert!(error.contains("fuse-overlayfs"), "{error}");
    }

    #[test]
    fn select_driver_skips_absent_drivers_without_testing_them() {
        let chosen = select_driver(false, true, |driver| {
            assert_eq!(
                driver,
                Driver::Fuse,
                "an absent kernel overlay is not tested"
            );
            Ok(())
        });
        assert_eq!(chosen, Ok(Driver::Fuse));
    }

    #[test]
    fn select_driver_reports_absence_when_nothing_is_present() {
        let error = select_driver(false, false, |_| Ok(())).unwrap_err();
        assert!(
            error.contains("kernel overlayfs is not available"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn merge_back_ignores_masks_and_dependency_writes() {
        let upper = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let kept = base.path().join("kept.txt");
        fs::write(&kept, "kept").unwrap();
        let masks = [PathBuf::from(".env"), PathBuf::from("sub/.env.local")];
        hide_entries(upper.path(), &masks, Driver::Fuse).unwrap();
        fs::write(upper.path().join(".wh.gone.txt"), "").unwrap();
        fs::create_dir_all(upper.path().join("node_modules")).unwrap();
        fs::write(upper.path().join("node_modules/new.js"), "dep").unwrap();
        fs::create_dir_all(upper.path().join("sub")).unwrap();
        fs::write(upper.path().join("sub/new.txt"), "new").unwrap();
        let mut before = Snapshot::new();
        for key in ["gone.txt", "kept.txt", "sub/keep.txt", "target/out.bin"] {
            before.insert(PathBuf::from(key), stamp_of(&kept));
        }
        let after = changed_snapshot(upper.path(), &before, &TaskConfig::default()).unwrap();
        let keys: Vec<_> = after.keys().map(|key| key.display().to_string()).collect();
        assert_eq!(
            keys,
            vec!["kept.txt", "sub/keep.txt", "sub/new.txt", "target/out.bin"]
        );
    }
}

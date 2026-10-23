//! Isolated working copies for `task` workers (`isolation = "copy"`).
//!
//! The worker runs against a private copy of the project (the same copier benchmarks use, so dirty
//! and untracked files are included and secrets, `.git`, `.dial` and build caches are not). When it
//! finishes, only the files it changed are merged back, each one only if the project's version is
//! still the one the copy started from. Files the user or another worker changed in the meantime are
//! reported as conflicts and never overwritten; the copy is then kept so nothing is lost. Merges go
//! through the project's checkpoint observer, so `/undo` reverts them like any other edit.
//! Failed/interrupted workers never merge. Their changed or unreadable copies remain recoverable;
//! lifecycle metadata reports applied files, parent conflicts, I/O errors and recovery locations
//! independently of the worker's schema-validated answer.
use crate::agent::workflow::solve::{FileState, confined_destination, install};
use crate::{platform::fs as platform_fs, tools::Workspace};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

pub(crate) mod backend;
mod overlay;
mod projfs;
mod worktree;

use crate::config::isolation::Preference;
use backend::Backend;

const MAX_BYTES: usize = 64 * 1024 * 1024;

/// The per-file limit is fixed, so the error says where the file is and how to get it out of the
/// copy: build outputs live in excluded directories, and the rest must move out of the project.
fn oversized_file(relative: &Path, bytes: u64) -> String {
    format!(
        "{} is {bytes} bytes, above the {MAX_BYTES} byte per-file isolation limit ({} MiB); this limit is not configurable, so move the file out of the project or into a build-output directory (bin/, target/, .cargo-work/, .tmp/) that isolation does not copy",
        relative.display(),
        MAX_BYTES >> 20
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    file: platform_fs::FileIdentity,
    size: u64,
    modified: std::time::SystemTime,
}

impl Identity {
    fn of(meta: &fs::Metadata, file: platform_fs::FileIdentity) -> Self {
        Self {
            file,
            size: meta.len(),
            modified: meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
        }
    }
}

#[derive(Clone, Debug)]
struct FileStamp {
    identity: Identity,
    hash: [u8; 32],
    mode: u32,
}

impl FileStamp {
    fn same_content(&self, other: &Self) -> bool {
        self.mode == other.mode
            && self.identity.size == other.identity.size
            && self.hash == other.hash
    }
}

type Snapshot = BTreeMap<PathBuf, FileStamp>;

pub struct Isolated {
    // Drop order is declaration order: unmount, then worktree metadata, then the directory.
    _mount: Option<overlay::Mount>,
    _link: Option<worktree::Link>,
    _projection: Option<projfs::Projection>,
    dir: tempfile::TempDir,
    root: PathBuf,
    label: String,
    before: Snapshot,
    limits: crate::config::task::TaskConfig,
    workspace: Workspace,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct Merge {
    pub changed: Vec<PathBuf>,
    pub applied: Vec<PathBuf>,
    pub already_applied: Vec<PathBuf>,
    pub conflicts: Vec<PathBuf>,
    pub errors: Vec<MergeError>,
    /// Full copy retained after worker failure, cancellation, conflicts or merge errors.
    #[serde(rename = "recovery_location")]
    pub kept: Option<PathBuf>,
    pub recovery_reason: Option<String>,
    pub interrupted: bool,
    /// Backend that gave the worker its view, with any skipped candidates and why.
    pub backend: String,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct MergeError {
    pub path: Option<PathBuf>,
    /// snapshot, read, confinement, observer, install or cleanup; never a parent-edit conflict.
    pub stage: &'static str,
    pub message: String,
}

impl Merge {
    pub fn describe(&self) -> String {
        let list = |paths: &[PathBuf]| {
            paths
                .iter()
                .take(20)
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut text = if self.applied.is_empty() {
            format!(
                "isolated copy: {} changed file(s), none merged",
                self.changed.len()
            )
        } else {
            format!(
                "isolated copy: merged {} file(s): {}",
                self.applied.len(),
                list(&self.applied)
            )
        };
        if !self.backend.is_empty() {
            text.insert_str(0, &format!("isolation backend: {}\n", self.backend));
        }
        if !self.already_applied.is_empty() {
            text.push_str(&format!(
                "\n{} file(s) already contain the worker's changes: {}",
                self.already_applied.len(),
                list(&self.already_applied)
            ));
        }
        if !self.conflicts.is_empty() {
            text.push_str(&format!("\nCONFLICT: {} file(s) changed in the project while the worker ran and were NOT overwritten: {}", self.conflicts.len(), list(&self.conflicts)));
        }
        for error in self.errors.iter().take(20) {
            text.push_str(&format!(
                "\nMERGE ERROR ({}{}): {}",
                error.stage,
                error
                    .path
                    .as_ref()
                    .map(|path| format!(": {}", path.display()))
                    .unwrap_or_default(),
                error.message
            ));
        }
        if let Some(kept) = &self.kept {
            text.push_str(&format!(
                "\nThe worker's full copy is kept at {} for recovery{}.",
                kept.display(),
                self.recovery_reason
                    .as_ref()
                    .map(|reason| format!(" ({reason})"))
                    .unwrap_or_default()
            ));
        }
        text
    }
}

/// A shared metadata walk keeps preflight and hashed snapshots on the same exclusion and
/// size policies. Preflight retains only names: copied inodes cannot reuse source hashes.
fn scan<T, P>(
    root: &Path,
    preserved: Option<&BTreeMap<PathBuf, P>>,
    limits: &crate::config::task::TaskConfig,
    mut visit: impl FnMut(&Path, &Path, Identity) -> Result<T>,
) -> Result<BTreeMap<PathBuf, T>> {
    fn walk<T, P>(
        root: &Path,
        dir: &Path,
        out: &mut BTreeMap<PathBuf, T>,
        preserved: Option<&BTreeMap<PathBuf, P>>,
        remaining: &mut usize,
        limits: &crate::config::task::TaskConfig,
        visit: &mut impl FnMut(&Path, &Path, Identity) -> Result<T>,
    ) -> Result<()> {
        for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let path = entry?.path();
            // Preserve copied source that was tracked under an ignored bin directory, even
            // before the private Git baseline exists (or when isolation_git is disabled).
            let preserved_source = preserved.is_some_and(|tree| {
                path.strip_prefix(root).ok().is_some_and(|relative| {
                    tree.range(relative.to_path_buf()..)
                        .next()
                        .is_some_and(|(source, _)| source.starts_with(relative))
                })
            });
            if !preserved_source && crate::workspace_copy::is_excluded_source_path(root, &path)
                || path
                    .file_name()
                    .is_some_and(|n| n == crate::workspace_copy::SCRATCH_DIR)
            {
                continue;
            }
            let meta = fs::symlink_metadata(&path)
                .with_context(|| format!("reading metadata for {}", path.display()))?;
            if meta.is_dir() {
                walk(root, &path, out, preserved, remaining, limits, visit)?;
            } else if meta.is_file() {
                let relative = path.strip_prefix(root)?.to_path_buf();
                ensure!(
                    out.len() < limits.isolation_source_files,
                    "project has more than {} files, the isolation file limit (task.isolation_source_files); next file: {}",
                    limits.isolation_source_files,
                    relative.display()
                );
                ensure!(
                    meta.len() <= MAX_BYTES as u64,
                    "{}",
                    oversized_file(&relative, meta.len())
                );
                ensure!(
                    meta.len() <= *remaining as u64,
                    "{} is {} bytes, but the copied source is limited to {} bytes in total (task.isolation_source_bytes; {} bytes already counted)",
                    relative.display(),
                    meta.len(),
                    limits.isolation_source_bytes,
                    limits.isolation_source_bytes - *remaining
                );
                let identity = Identity::of(&meta, platform_fs::path_identity(&path)?);
                let size = identity.size as usize;
                let value = visit(&path, &relative, identity)?;
                *remaining -= size;
                out.insert(relative, value);
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    let mut remaining = limits.isolation_source_bytes;
    walk(
        root,
        root,
        &mut out,
        preserved,
        &mut remaining,
        limits,
        &mut visit,
    )?;
    Ok(out)
}

/// Metadata-only preflight bounds the source before a backend copies or mounts it.
fn preflight(
    root: &Path,
    limits: &crate::config::task::TaskConfig,
) -> Result<BTreeMap<PathBuf, ()>> {
    scan::<_, ()>(root, None, limits, |_, _, _| Ok(()))
}

/// Identity and streamed hashes, not two complete in-memory copies of project contents.
fn snapshot(
    root: &Path,
    baseline: Option<&Snapshot>,
    limits: &crate::config::task::TaskConfig,
) -> Result<Snapshot> {
    snapshot_preserving(root, baseline, baseline, limits)
}

fn snapshot_preserving<P>(
    root: &Path,
    baseline: Option<&Snapshot>,
    preserved: Option<&BTreeMap<PathBuf, P>>,
    limits: &crate::config::task::TaskConfig,
) -> Result<Snapshot> {
    scan(root, preserved, limits, |path, relative, identity| {
        let value = match baseline.and_then(|tree| tree.get(relative)) {
            Some(previous) if previous.identity == identity => previous.clone(),
            _ => stamp(path)?.context("snapshot file disappeared")?,
        };
        ensure!(
            value.identity == identity,
            "file changed while snapshotting {}",
            path.display()
        );
        Ok(value)
    })
}

fn stamp(path: &Path) -> Result<Option<FileStamp>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(meta.is_file(), "source destination is not a regular file");
    ensure!(
        meta.len() <= MAX_BYTES as u64,
        "{} is {} bytes, above the {} byte per-file isolation limit ({} MiB)",
        path.display(),
        meta.len(),
        MAX_BYTES,
        MAX_BYTES >> 20
    );
    let mut file = platform_fs::open_nofollow(fs::OpenOptions::new().read(true), path)?;
    let identity = Identity::of(&file.metadata()?, platform_fs::identity(&file)?);
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 8192];
    let mut bytes = 0u64;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes += count as u64;
        ensure!(
            bytes <= MAX_BYTES as u64,
            "{} grew past the {} byte per-file isolation limit while it was hashed (at least {bytes} bytes)",
            path.display(),
            MAX_BYTES
        );
        digest.update(&buffer[..count]);
    }
    ensure!(
        bytes == identity.size
            && Identity::of(&file.metadata()?, platform_fs::identity(&file)?) == identity
            && Identity::of(
                &fs::symlink_metadata(path)?,
                platform_fs::path_identity(path)?
            ) == identity,
        "file changed while hashing"
    );
    #[cfg(test)]
    startup_tests::record_hash(bytes);
    Ok(Some(FileStamp {
        identity,
        hash: digest.finalize().into(),
        mode: platform_fs::mode(&meta) & 0o777,
    }))
}

fn same(left: Option<&FileStamp>, right: Option<&FileStamp>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left.same_content(right),
        (None, None) => true,
        _ => false,
    }
}

fn changed(before: &Snapshot, after: &Snapshot) -> Vec<PathBuf> {
    before
        .keys()
        .chain(after.keys())
        .filter(|path| !same(before.get(*path), after.get(*path)))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .cloned()
        .collect()
}

/// `whiteouts`: the worker tree is an overlay upper layer, where a deleted name is recorded as a
/// whiteout (character device 0:0) under that same name rather than being absent.
fn materialize(
    path: &Path,
    expected: Option<&FileStamp>,
    whiteouts: bool,
) -> Result<Option<FileState>> {
    let Some(expected) = expected else {
        return match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
            Ok(meta) if whiteouts && overlay::is_whiteout(&meta) => Ok(None),
            Ok(_) => anyhow::bail!("worker deleted file reappeared after snapshot"),
        };
    };
    let mut file = platform_fs::open_nofollow(fs::OpenOptions::new().read(true), path)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && Identity::of(&meta, platform_fs::identity(&file)?) == expected.identity,
        "worker file changed after snapshot"
    );
    let mut bytes = Vec::with_capacity(expected.identity.size as usize);
    file.by_ref()
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == expected.identity.size
            && <[u8; 32]>::from(Sha256::digest(&bytes)) == expected.hash
            && Identity::of(&file.metadata()?, platform_fs::identity(&file)?) == expected.identity
            && Identity::of(
                &fs::symlink_metadata(path)?,
                platform_fs::path_identity(path)?
            ) == expected.identity,
        "worker file changed while reading"
    );
    Ok(Some(FileState {
        bytes,
        mode: expected.mode,
    }))
}

impl Isolated {
    #[cfg(test)]
    pub fn create(project: &Workspace) -> Result<Self> {
        Self::with_config(
            project,
            &crate::config::task::TaskConfig::default(),
            Preference::Auto,
        )
    }

    /// Build the private copy with the backend `preference` selects for this platform.
    pub fn with_config(
        project: &Workspace,
        config: &crate::config::task::TaskConfig,
        preference: Preference,
    ) -> Result<Self> {
        // Bound source scanning before copying any tree; a failure here leaves nothing copied.
        let source_paths = preflight(project.root(), config).context(
            "isolation preflight failed before any copy was made; no files were copied and the project is unchanged",
        )?;
        let selection = backend::select(preference, |candidate| {
            backend::probe(candidate, project.root(), config.isolation_source_files)
        })
        .map_err(anyhow::Error::msg)?;
        let kind = selection.backend;
        let label = selection.label();
        // Declaration order is drop order for the locals below: the overlay is unmounted and the
        // worktree metadata removed before the directory holding them is deleted.
        let dir;
        let root;
        let mut link = None;
        let mut mount = None;
        let mut projection = None;
        match kind {
            Backend::Clone => {
                dir = crate::workspace_copy::clone_project(project.root())?;
                root = dir.path().canonicalize()?;
            }
            Backend::Copy => {
                dir = crate::workspace_copy::copy_project_plain(project.root())?;
                root = dir.path().canonicalize()?;
            }
            Backend::Worktree => {
                dir = tempfile::tempdir().context("creating isolation directory")?;
                let work = dir.path().join("work");
                link = Some(
                    worktree::populate(
                        project.root(),
                        dir.path(),
                        &work,
                        config.isolation_dependency_bytes,
                    )
                    .map_err(anyhow::Error::msg)?,
                );
                root = work.canonicalize()?;
            }
            Backend::Overlay => {
                dir = tempfile::tempdir().context("creating isolation directory")?;
                mount = Some(
                    overlay::mount(project.root(), dir.path(), config.isolation_source_files)
                        .map_err(anyhow::Error::msg)?,
                );
                root = dir.path().join("merged").canonicalize()?;
            }
            Backend::ProjFs => {
                dir = tempfile::tempdir().context("creating isolation directory")?;
                let work = dir.path().join("work");
                projection =
                    Some(projfs::start(project.root(), &work).map_err(anyhow::Error::msg)?);
                root = work;
            }
        }
        // Hash the actual worker baseline once. ProjFS must hash the source instead: walking
        // its instance would hydrate every placeholder. Overlay still hashes its mounted view
        // so concurrent source changes cannot silently reuse stale preflight content.
        let before = match kind {
            Backend::ProjFs => snapshot(project.root(), None, config)?,
            _ => snapshot_preserving(&root, None, Some(&source_paths), config)?,
        };
        // The overlay's lower layer and the worktree's copied ignored files already provide
        // dependencies, so only the plain copies need the dependency copy.
        if config.isolation_dependencies && matches!(kind, Backend::Clone | Backend::Copy) {
            crate::agent::workers::development_copy::dependencies(
                project.root(),
                &root,
                config.isolation_dependency_bytes,
            )?;
        }
        // A git snapshot would read every file of the instance and hydrate the whole projection.
        if config.isolation_git
            && !matches!(kind, Backend::ProjFs)
            && project.root().join(".git").exists()
        {
            crate::tools::vcs::git::initialize_private_snapshot(&root)
                .map_err(anyhow::Error::msg)?;
        }
        let workspace = project.rebased(&root)?;
        Ok(Self {
            _mount: mount,
            _link: link,
            _projection: projection,
            dir,
            root,
            label,
            before,
            limits: config.clone(),
            workspace,
        })
    }

    /// The worker's changes as a snapshot comparable with `before`.
    fn current(&self) -> Result<Snapshot> {
        match (&self._mount, &self._projection) {
            (Some(mount), _) => {
                overlay::changed_snapshot(mount.upper(), &self.before, &self.limits)
            }
            (None, Some(projection)) => projection.changed_snapshot(&self.before, &self.limits),
            (None, None) => snapshot(&self.root, Some(&self.before), &self.limits),
        }
    }

    /// Directory whose files hold the worker's current content, as `current` describes them.
    fn source_root(&self) -> &Path {
        match &self._mount {
            Some(mount) => mount.upper(),
            None => &self.root,
        }
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Never merge failed/cancelled workers, but retain any changed or unreadable copy.
    pub fn recover(mut self, reason: &str) -> Merge {
        let link = self._link.take();
        let mut merge = Merge {
            backend: self.label.clone(),
            ..Merge::default()
        };
        match self.current() {
            Ok(after) => merge.changed = changed(&self.before, &after),
            Err(error) => merge.errors.push(MergeError {
                path: None,
                stage: "snapshot",
                message: format!("{error:#}"),
            }),
        }
        if !merge.changed.is_empty() || !merge.errors.is_empty() {
            merge.recovery_reason = Some(reason.to_owned());
            let sub = self.kept_subdir();
            merge.kept = Some(self.dir.keep().join(sub));
        }
        release_link(link, &mut merge);
        merge
    }

    /// Merge the copy's changes into `project`, recording I/O failures separately
    /// from real parent edits. Each changed file alone is materialized for installation.
    pub fn merge(mut self, project: &Workspace, cancel: &AtomicBool) -> Merge {
        let link = self._link.take();
        let mut merge = self.merge_changes(project, cancel);
        release_link(link, &mut merge);
        merge
    }

    fn merge_changes(self, project: &Workspace, cancel: &AtomicBool) -> Merge {
        let mut merge = Merge {
            backend: self.label.clone(),
            ..Merge::default()
        };
        let after = match self.current() {
            Ok(after) => after,
            Err(error) => {
                merge.errors.push(MergeError {
                    path: None,
                    stage: "snapshot",
                    message: format!("{error:#}"),
                });
                merge.recovery_reason = Some("could not inspect worker changes".into());
                merge.interrupted = cancel.load(Ordering::SeqCst);
                let sub = self.kept_subdir();
                merge.kept = Some(self.dir.keep().join(sub));
                return merge;
            }
        };
        merge.changed = changed(&self.before, &after);
        let _serial = project.serial_operation();
        // Cancellation before this commit boundary preserves the original project.
        // Once installation begins, finish the per-file merge rather than cancelling
        // half a commit and claiming the original workspace is unchanged.
        if cancel.load(Ordering::SeqCst) {
            merge.interrupted = true;
            merge.recovery_reason = Some("worker interrupted before merge".into());
            if !merge.changed.is_empty() {
                let sub = self.kept_subdir();
                merge.kept = Some(self.dir.keep().join(sub));
            }
            return merge;
        }
        for relative in &merge.changed {
            let mut stage = "confinement";
            let mut already_applied = false;
            let result = (|| -> Result<bool> {
                let path = confined_destination(project.root(), relative)?;
                stage = "read";
                let current = stamp(&path)?;
                if same(current.as_ref(), after.get(relative)) {
                    already_applied = true;
                    return Ok(true);
                }
                if !same(current.as_ref(), self.before.get(relative)) {
                    return Ok(false);
                }
                stage = "confinement";
                let worker_path = confined_destination(self.source_root(), relative)?;
                stage = "read";
                let state = materialize(&worker_path, after.get(relative), self._mount.is_some())?;
                stage = "observer";
                if state.is_some() {
                    project.observe_write(&path)
                } else {
                    project.observe_delete(&path)
                }
                .map_err(anyhow::Error::msg)?;
                // Observers can block while a user changes the project. Recheck the
                // content before installation; do not turn a read error into a conflict.
                stage = "read";
                if !same(stamp(&path)?.as_ref(), self.before.get(relative)) {
                    return Ok(false);
                }
                stage = "confinement";
                ensure!(
                    confined_destination(project.root(), relative)? == path,
                    "merge destination changed"
                );
                stage = "install";
                install(&path, state.as_ref())?;
                Ok(true)
            })();
            match result {
                Ok(true) if already_applied => merge.already_applied.push(relative.clone()),
                Ok(true) => merge.applied.push(relative.clone()),
                Ok(false) => merge.conflicts.push(relative.clone()),
                Err(error) => merge.errors.push(MergeError {
                    path: Some(relative.clone()),
                    stage,
                    message: format!("{error:#}"),
                }),
            }
        }
        if !merge.conflicts.is_empty() || !merge.errors.is_empty() {
            merge.recovery_reason = Some("unmerged changes".into());
            let sub = self.kept_subdir();
            merge.kept = Some(self.dir.keep().join(sub));
        }
        merge
    }

    /// Where the worker's files sit inside the kept directory: the copy root, the worktree's
    /// `work`, or an overlay's `upper` layer (the view itself is unmounted on release).
    fn kept_subdir(&self) -> PathBuf {
        let dir = fs::canonicalize(self.dir.path()).unwrap_or_else(|_| self.dir.path().into());
        let files =
            fs::canonicalize(self.source_root()).unwrap_or_else(|_| self.source_root().into());
        files
            .strip_prefix(&dir)
            .map(Path::to_path_buf)
            .unwrap_or_default()
    }
}

/// Removes a worktree's git metadata; a failure is reported in the merge rather than dropped.
fn release_link(link: Option<worktree::Link>, merge: &mut Merge) {
    let Some(mut link) = link else {
        return;
    };
    if let Err(message) = link.remove() {
        merge.errors.push(MergeError {
            path: None,
            stage: "cleanup",
            message,
        });
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod startup_tests;

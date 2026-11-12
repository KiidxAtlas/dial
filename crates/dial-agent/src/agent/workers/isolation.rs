//! Isolated working copies for `task` workers (`isolation = "copy"`).
//!
//! The worker runs against a private copy of the project (the same copier benchmarks use, so dirty
//! and untracked files are included and secrets, `.git`, `.dial` and build caches are not).
//! Gitignored, untracked paths are left out of the copy, its size limits and its merge: they are
//! local data, not source, and a large ignored directory must not block every worker. When it
//! finishes, its complete patch is preflighted before any project file is installed. A conflict
//! blocks the entire patch; an installation failure rolls back earlier writes without replacing
//! concurrent user edits. Recovery retains the worker copy and any unrestored preimages. Merges
//! go through the project's checkpoint observer, so `/undo` reverts them like any other edit.
//! Failed/interrupted workers never merge. Their changed or unreadable copies remain recoverable;
//! lifecycle metadata reports applied files, parent conflicts, I/O errors and recovery locations
//! independently of the worker's schema-validated answer.
use crate::agent::workflow::solve::{FileState, confined_destination, install, read_state};
use crate::workspace_copy::IgnoredPaths;
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
    /// Gitignored, untracked paths of the project, left out of the copy and its change set.
    ignored: IgnoredPaths,
    workspace: Workspace,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct Merge {
    /// True only after the complete candidate patch is present in the parent.
    pub integrated: bool,
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
    /// Preimages retained only when rollback could not restore every installed file.
    pub recovery_backup: Option<PathBuf>,
    pub rolled_back: Vec<PathBuf>,
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
        if let Some(backup) = &self.recovery_backup {
            text.push_str(&format!(
                "\nUnrestored project preimages are kept at {}.",
                backup.display()
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
/// `ignored` paths are skipped entirely, so they count against no limit.
fn scan<T, P>(
    root: &Path,
    preserved: Option<&BTreeMap<PathBuf, P>>,
    ignored: &IgnoredPaths,
    limits: &crate::config::task::TaskConfig,
    mut visit: impl FnMut(&Path, &Path, Identity) -> Result<T>,
) -> Result<BTreeMap<PathBuf, T>> {
    #[allow(clippy::too_many_arguments)]
    fn walk<T, P>(
        root: &Path,
        dir: &Path,
        out: &mut BTreeMap<PathBuf, T>,
        preserved: Option<&BTreeMap<PathBuf, P>>,
        ignored: &IgnoredPaths,
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
                || path
                    .strip_prefix(root)
                    .is_ok_and(|relative| ignored.covers(relative))
            {
                continue;
            }
            let meta = fs::symlink_metadata(&path)
                .with_context(|| format!("reading metadata for {}", path.display()))?;
            if meta.is_dir() {
                walk(
                    root, &path, out, preserved, ignored, remaining, limits, visit,
                )?;
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
        ignored,
        &mut remaining,
        limits,
        &mut visit,
    )?;
    Ok(out)
}

/// Metadata-only preflight bounds the source before a backend copies or mounts it.
fn preflight(
    root: &Path,
    ignored: &IgnoredPaths,
    limits: &crate::config::task::TaskConfig,
) -> Result<BTreeMap<PathBuf, ()>> {
    scan::<_, ()>(root, None, ignored, limits, |_, _, _| Ok(()))
}

/// Identity and streamed hashes, not two complete in-memory copies of project contents.
fn snapshot(
    root: &Path,
    baseline: Option<&Snapshot>,
    ignored: &IgnoredPaths,
    limits: &crate::config::task::TaskConfig,
) -> Result<Snapshot> {
    snapshot_preserving(root, baseline, baseline, ignored, limits)
}

fn snapshot_preserving<P>(
    root: &Path,
    baseline: Option<&Snapshot>,
    preserved: Option<&BTreeMap<PathBuf, P>>,
    ignored: &IgnoredPaths,
    limits: &crate::config::task::TaskConfig,
) -> Result<Snapshot> {
    scan(
        root,
        preserved,
        ignored,
        limits,
        |path, relative, identity| {
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
        },
    )
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

/// One backend's private view of the project. Field order is drop order: unmount, then worktree
/// metadata, then the directory holding them.
struct Materialized {
    mount: Option<overlay::Mount>,
    link: Option<worktree::Link>,
    projection: Option<projfs::Projection>,
    dir: tempfile::TempDir,
    root: PathBuf,
}

/// Sets up `kind`'s view of `project`. On failure everything it created is dropped.
fn set_up_backend(
    project: &Workspace,
    config: &crate::config::task::TaskConfig,
    ignored: &IgnoredPaths,
    kind: Backend,
) -> Result<Materialized> {
    // Declaration order is drop order for the locals below: the overlay is unmounted and the
    // worktree metadata removed before the directory holding them is deleted.
    let dir;
    let root;
    let mut link = None;
    let mut mount = None;
    let mut projection = None;
    match kind {
        Backend::Clone => {
            dir = crate::workspace_copy::clone_project_except(project.root(), ignored)?;
            root = dir.path().canonicalize()?;
        }
        Backend::Copy => {
            dir = crate::workspace_copy::copy_project_plain_except(project.root(), ignored)?;
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
            projection = Some(projfs::start(project.root(), &work).map_err(anyhow::Error::msg)?);
            root = work;
        }
    }
    Ok(Materialized {
        mount,
        link,
        projection,
        dir,
        root,
    })
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
        let ignored = IgnoredPaths::of(project.root());
        // Bound source scanning before copying any tree; a failure here leaves nothing copied.
        let source_paths = preflight(project.root(), &ignored, config).context(
            "isolation preflight failed before any copy was made; no files were copied and the project is unchanged",
        )?;
        // `auto` takes the first backend that both probes and sets up: a probe cannot prove a
        // mount will succeed (an unprivileged kernel overlay mount fails with EPERM, as does one in
        // a container without CAP_SYS_ADMIN), so a setup failure moves on to the next candidate
        // and is reported as the reason that backend was skipped. A named backend still fails.
        let mut setup_failures: Vec<(Backend, String)> = Vec::new();
        let (selection, parts) = loop {
            let failed: Vec<Backend> = setup_failures.iter().map(|(backend, _)| *backend).collect();
            let mut selection = backend::select(preference, |candidate| {
                if failed.contains(&candidate) {
                    return Err("setup failed".into());
                }
                backend::probe(candidate, project.root(), config.isolation_source_files)
            })
            .map_err(anyhow::Error::msg)?;
            match set_up_backend(project, config, &ignored, selection.backend) {
                Ok(parts) => {
                    selection
                        .skipped
                        .retain(|(backend, _)| !failed.contains(backend));
                    selection.skipped.splice(0..0, setup_failures);
                    break (selection, parts);
                }
                Err(error) if matches!(preference, Preference::Auto) => {
                    setup_failures.push((selection.backend, format!("{error:#}")));
                }
                Err(error) => return Err(error),
            }
        };
        let kind = selection.backend;
        let label = selection.label();
        let Materialized {
            mount,
            link,
            projection,
            dir,
            root,
        } = parts;
        // Hash the actual worker baseline once. ProjFS must hash the source instead: walking
        // its instance would hydrate every placeholder. Overlay still hashes its mounted view
        // so concurrent source changes cannot silently reuse stale preflight content.
        let before = match kind {
            Backend::ProjFs => snapshot(project.root(), None, &ignored, config)?,
            _ => snapshot_preserving(&root, None, Some(&source_paths), &ignored, config)?,
        };
        // The overlay's lower layer already provides dependencies. The worktree checkout holds only
        // tracked files (its sync skips `node_modules`, `.venv`, `vendor` and other dependency
        // directories), so it needs the dependency copy like the plain copies do.
        if config.isolation_dependencies
            && matches!(kind, Backend::Clone | Backend::Copy | Backend::Worktree)
        {
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
            ignored,
            workspace,
        })
    }

    /// The worker's changes as a snapshot comparable with `before`. Ignored paths are dropped for
    /// every backend, including overlay and ProjFS views that still show them to the worker.
    fn current(&self) -> Result<Snapshot> {
        let mut after = match (&self._mount, &self._projection) {
            (Some(mount), _) => {
                overlay::changed_snapshot(mount.upper(), &self.before, &self.limits)?
            }
            (None, Some(projection)) => projection.changed_snapshot(&self.before, &self.limits)?,
            (None, None) => snapshot(&self.root, Some(&self.before), &self.ignored, &self.limits)?,
        };
        after.retain(|path, _| !self.ignored.covers(path));
        Ok(after)
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

    /// Test fixture for an uncontracted patch; production always supplies its acceptance guard.
    #[cfg(test)]
    fn merge(self, project: &Workspace, cancel: &AtomicBool) -> Merge {
        self.merge_checked(project, cancel, |_| Ok(()))
    }

    pub(in crate::agent) fn merge_checked(
        mut self,
        project: &Workspace,
        cancel: &AtomicBool,
        validate: impl FnMut(&Workspace) -> Result<()>,
    ) -> Merge {
        let link = self._link.take();
        let mut merge = self.merge_changes(project, cancel, install, validate);
        release_link(link, &mut merge);
        merge
    }

    fn merge_changes(
        self,
        project: &Workspace,
        cancel: &AtomicBool,
        mut install_file: impl FnMut(&Path, Option<&FileState>) -> Result<()>,
        mut validate: impl FnMut(&Workspace) -> Result<()>,
    ) -> Merge {
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
        // Cancellation before the commit boundary preserves the original project.
        if cancel.load(Ordering::SeqCst) {
            merge.interrupted = true;
            merge.recovery_reason = Some("worker interrupted before merge".into());
            if !merge.changed.is_empty() {
                let sub = self.kept_subdir();
                merge.kept = Some(self.dir.keep().join(sub));
            }
            return merge;
        }
        // Stage candidate bytes and preimages on disk, retaining bounded per-file memory.
        // No observer or write is allowed until every path has passed preflight.
        let staging = tempfile::tempdir();
        let mut prepared = Vec::new();
        match &staging {
            Err(error) => merge.errors.push(MergeError {
                path: None,
                stage: "stage",
                message: error.to_string(),
            }),
            Ok(staging) => {
                for (index, relative) in merge.changed.iter().enumerate() {
                    let mut stage = "confinement";
                    let result = (|| -> Result<bool> {
                        let path = confined_destination(project.root(), relative)?;
                        stage = "read";
                        let current = stamp(&path)?;
                        if same(current.as_ref(), after.get(relative)) {
                            merge.already_applied.push(relative.clone());
                            return Ok(true);
                        }
                        if !same(current.as_ref(), self.before.get(relative)) {
                            return Ok(false);
                        }
                        stage = "confinement";
                        let worker_path = confined_destination(self.source_root(), relative)?;
                        stage = "stage";
                        let candidate =
                            materialize(&worker_path, after.get(relative), self._mount.is_some())?;
                        let original = materialize(&path, current.as_ref(), false)?;
                        let candidate_path = staging.path().join(format!("{index}.candidate"));
                        let original_path = staging.path().join(format!("{index}.original"));
                        if let Some(state) = &candidate {
                            install(&candidate_path, Some(state))?;
                        }
                        if let Some(state) = &original {
                            install(&original_path, Some(state))?;
                        }
                        prepared.push((relative.clone(), candidate_path, original_path));
                        Ok(true)
                    })();
                    match result {
                        Ok(true) => {}
                        Ok(false) => merge.conflicts.push(relative.clone()),
                        Err(error) => merge.errors.push(MergeError {
                            path: Some(relative.clone()),
                            stage,
                            message: format!("{error:#}"),
                        }),
                    }
                }
            }
        }
        if merge.conflicts.is_empty() && merge.errors.is_empty() {
            let mut stage = "observer";
            let mut active = None;
            let result = (|| -> Result<()> {
                stage = "stage";
                if let Ok(staging) = &staging {
                    // Numeric stage names avoid path collisions; this manifest maps recovery files.
                    fs::write(
                        staging.path().join("paths.json"),
                        serde_json::to_vec(&merge.changed)?,
                    )?;
                }
                stage = "observer";
                for (relative, _, _) in &prepared {
                    active = Some(relative.clone());
                    let path = confined_destination(project.root(), relative)?;
                    if after.contains_key(relative) {
                        project.observe_write(&path)
                    } else {
                        project.observe_delete(&path)
                    }
                    .map_err(anyhow::Error::msg)?;
                }
                stage = "read";
                // An observer can block while another writer changes any patch input.
                for relative in &merge.changed {
                    active = Some(relative.clone());
                    let path = confined_destination(project.root(), relative)?;
                    let expected = if merge.already_applied.contains(relative) {
                        after.get(relative)
                    } else {
                        self.before.get(relative)
                    };
                    if !same(stamp(&path)?.as_ref(), expected) {
                        merge.conflicts.push(relative.clone());
                    }
                }
                if !merge.conflicts.is_empty() {
                    return Ok(());
                }
                if cancel.load(Ordering::SeqCst) {
                    merge.interrupted = true;
                    return Ok(());
                }
                stage = "acceptance";
                validate(self.workspace())?;
                for (relative, candidate, _) in &prepared {
                    active = Some(relative.clone());
                    stage = "read";
                    let path = confined_destination(project.root(), relative)?;
                    if !same(stamp(&path)?.as_ref(), self.before.get(relative)) {
                        merge.conflicts.push(relative.clone());
                        anyhow::bail!("project changed during patch installation");
                    }
                    let state = read_state(candidate)?;
                    stage = "install";
                    install_file(&path, state.as_ref())?;
                    merge.applied.push(relative.clone());
                }
                // Parent dependencies outside the patch may differ from the checked candidate.
                // Reject inside the transaction so those differences roll the whole patch back.
                stage = "acceptance";
                validate(project)?;
                Ok(())
            })();
            if let Err(error) = result {
                merge.errors.push(MergeError {
                    path: active,
                    stage,
                    message: format!("{error:#}"),
                });
                for (relative, _, original) in prepared.iter().rev() {
                    if !merge.applied.contains(relative) {
                        continue;
                    }
                    let restored = (|| -> Result<()> {
                        let path = confined_destination(project.root(), relative)?;
                        ensure!(
                            same(stamp(&path)?.as_ref(), after.get(relative)),
                            "concurrent project edit prevents rollback; user work preserved"
                        );
                        let state = read_state(original)?;
                        install(&path, state.as_ref())
                    })();
                    match restored {
                        Ok(()) => {
                            merge.applied.retain(|path| path != relative);
                            merge.rolled_back.push(relative.clone());
                        }
                        Err(error) => merge.errors.push(MergeError {
                            path: Some(relative.clone()),
                            stage: "rollback",
                            message: format!("{error:#}"),
                        }),
                    }
                }
                if !merge.applied.is_empty()
                    && let Ok(staging) = staging
                {
                    merge.recovery_backup = Some(staging.keep());
                }
            }
        }
        merge.integrated =
            !merge.interrupted && merge.conflicts.is_empty() && merge.errors.is_empty();
        if merge.interrupted || !merge.conflicts.is_empty() || !merge.errors.is_empty() {
            merge.recovery_reason = Some("patch not coherently integrated".into());
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

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
use super::solve::{FileState, confined_destination, install};
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

use backend::{Backend, Preference};

const MAX_BYTES: usize = 64 * 1024 * 1024;

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
    limits: super::agent_defs::TaskConfig,
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

/// Identity and streamed hashes, not two complete in-memory copies of project contents.
/// Symlinks and excluded paths retain the copier's policy; limits are unchanged.
fn snapshot(
    root: &Path,
    baseline: Option<&Snapshot>,
    limits: &super::agent_defs::TaskConfig,
) -> Result<Snapshot> {
    fn walk(
        root: &Path,
        dir: &Path,
        out: &mut Snapshot,
        baseline: Option<&Snapshot>,
        remaining: &mut usize,
        limits: &super::agent_defs::TaskConfig,
    ) -> Result<()> {
        for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let path = entry?.path();
            if crate::benchmark::is_excluded_source_path(root, &path)
                || path
                    .file_name()
                    .is_some_and(|n| n == crate::tools::SCRATCH_DIR)
            {
                continue;
            }
            let meta = fs::symlink_metadata(&path)
                .with_context(|| format!("reading metadata for {}", path.display()))?;
            if meta.is_dir() {
                walk(root, &path, out, baseline, remaining, limits)?;
            } else if meta.is_file() {
                ensure!(
                    out.len() < limits.isolation_source_files && meta.len() <= *remaining as u64,
                    "project is too large to isolate (limit {} files, {} MiB)",
                    limits.isolation_source_files,
                    limits.isolation_source_bytes >> 20
                );
                let relative = path.strip_prefix(root)?.to_path_buf();
                let identity = Identity::of(&meta, platform_fs::path_identity(&path)?);
                let stamp = match baseline.and_then(|tree| tree.get(&relative)) {
                    Some(previous) if previous.identity == identity => previous.clone(),
                    _ => stamp(&path)?.context("snapshot file disappeared")?,
                };
                ensure!(
                    stamp.identity == identity,
                    "file changed while snapshotting {}",
                    path.display()
                );
                *remaining -= stamp.identity.size as usize;
                out.insert(relative, stamp);
            }
        }
        Ok(())
    }
    let mut out = Snapshot::new();
    let mut remaining = limits.isolation_source_bytes;
    walk(root, root, &mut out, baseline, &mut remaining, limits)?;
    Ok(out)
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
        "file exceeds isolation byte limit"
    );
    let mut file = platform_fs::open_nofollow(&fs::OpenOptions::new().read(true), path)?;
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
            "file exceeds isolation byte limit"
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

fn materialize(path: &Path, expected: Option<&FileStamp>) -> Result<Option<FileState>> {
    let Some(expected) = expected else {
        return match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
            Ok(_) => anyhow::bail!("worker deleted file reappeared after snapshot"),
        };
    };
    let mut file = platform_fs::open_nofollow(&fs::OpenOptions::new().read(true), path)?;
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
            &super::agent_defs::TaskConfig::default(),
            Preference::Auto,
        )
    }

    /// Build the private copy with the backend `preference` selects for this platform.
    pub fn with_config(
        project: &Workspace,
        config: &super::agent_defs::TaskConfig,
        preference: Preference,
    ) -> Result<Self> {
        // Bound source scanning before copying any tree.
        snapshot(project.root(), None, config)?;
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
                dir = crate::benchmark::clone_project(project.root())?;
                root = dir.path().canonicalize()?;
            }
            Backend::Copy => {
                dir = crate::benchmark::copy_project_plain(project.root())?;
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
        // The source snapshot is the baseline for ProjFS: walking the instance would read every placeholder.
        let before = match kind {
            Backend::ProjFs => snapshot(project.root(), None, config)?,
            _ => snapshot(&root, None, config)?,
        };
        // The overlay's lower layer and the worktree's copied ignored files already provide
        // dependencies, so only the plain copies need the dependency copy.
        if config.isolation_dependencies && matches!(kind, Backend::Clone | Backend::Copy) {
            super::development_copy::dependencies(
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
            crate::tools::git::initialize_private_snapshot(&root).map_err(anyhow::Error::msg)?;
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
                let state = materialize(&worker_path, after.get(relative))?;
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
mod tests {
    use super::*;

    fn project() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "a1").unwrap();
        fs::write(dir.path().join("b.txt"), "b1").unwrap();
        fs::write(dir.path().join("gone.txt"), "x").unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        (dir, ws)
    }

    #[test]
    fn development_copy_has_private_git_and_dependencies_and_obeys_source_limits() {
        let (dir, ws) = project();
        crate::tools::git::initialize_private_snapshot(dir.path()).unwrap();
        fs::create_dir_all(dir.path().join("node_modules/pkg/node_modules/nested")).unwrap();
        fs::write(
            dir.path()
                .join("node_modules/pkg/node_modules/nested/index.js"),
            "module.exports=7;",
        )
        .unwrap();
        let iso = Isolated::create(&ws).unwrap();
        assert!(iso.root().join(".git/HEAD").exists());
        let status = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(iso.root())
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "{}",
            String::from_utf8_lossy(&status.stderr)
        );
        assert!(
            status.stdout.is_empty(),
            "{}",
            String::from_utf8_lossy(&status.stdout)
        );
        let dep = "node_modules/pkg/node_modules/nested/index.js";
        fs::write(iso.root().join(dep), "changed").unwrap();
        let merge = iso.merge(&ws, &AtomicBool::new(false));
        assert!(merge.applied.is_empty());
        assert_eq!(
            fs::read_to_string(dir.path().join(dep)).unwrap(),
            "module.exports=7;"
        );
        let limits = super::super::agent_defs::TaskConfig {
            isolation_source_files: 1,
            ..Default::default()
        };
        assert!(Isolated::with_config(&ws, &limits, Preference::Auto).is_err());
    }

    #[test]
    fn changes_merge_back_and_the_copy_is_discarded() {
        let (dir, ws) = project();
        let iso = Isolated::create(&ws).unwrap();
        assert_ne!(iso.root(), ws.root());
        fs::write(iso.root().join("a.txt"), "a2").unwrap();
        fs::write(iso.root().join("new.txt"), "n").unwrap();
        fs::remove_file(iso.root().join("gone.txt")).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "a1",
            "project untouched while the worker runs"
        );
        let root = iso.root().to_path_buf();
        let merge = iso.merge(&ws, &AtomicBool::new(false));
        assert_eq!(merge.applied.len(), 3);
        assert!(merge.conflicts.is_empty() && merge.kept.is_none());
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "a2");
        assert_eq!(fs::read_to_string(dir.path().join("new.txt")).unwrap(), "n");
        assert!(!dir.path().join("gone.txt").exists());
        assert!(!root.exists(), "copy removed");
    }

    #[test]
    fn concurrent_project_edits_are_never_overwritten() {
        let (dir, ws) = project();
        let iso = Isolated::create(&ws).unwrap();
        fs::write(iso.root().join("a.txt"), "worker").unwrap();
        fs::write(iso.root().join("b.txt"), "worker").unwrap();
        fs::write(dir.path().join("a.txt"), "user").unwrap();
        let merge = iso.merge(&ws, &AtomicBool::new(false));
        assert_eq!(merge.applied, vec![PathBuf::from("b.txt")]);
        assert_eq!(merge.conflicts, vec![PathBuf::from("a.txt")]);
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "user"
        );
        let kept = merge.kept.clone().expect("copy kept for manual merge");
        assert_eq!(fs::read_to_string(kept.join("a.txt")).unwrap(), "worker");
        assert!(merge.describe().contains("CONFLICT"));
        fs::remove_dir_all(kept).unwrap();
    }

    #[test]
    fn two_workers_on_disjoint_files_both_land_and_identical_edits_are_not_conflicts() {
        let (dir, ws) = project();
        let (one, two) = (
            Isolated::create(&ws).unwrap(),
            Isolated::create(&ws).unwrap(),
        );
        fs::write(one.root().join("a.txt"), "one").unwrap();
        fs::write(two.root().join("b.txt"), "two").unwrap();
        fs::write(two.root().join("a.txt"), "one").unwrap();
        assert!(one.merge(&ws, &AtomicBool::new(false)).conflicts.is_empty());
        let merge = two.merge(&ws, &AtomicBool::new(false));
        assert!(merge.conflicts.is_empty(), "{merge:?}");
        assert_eq!(fs::read_to_string(dir.path().join("b.txt")).unwrap(), "two");
    }

    #[test]
    fn build_output_and_secrets_in_the_copy_never_merge() {
        let (dir, ws) = project();
        fs::write(dir.path().join(".env"), "SECRET").unwrap();
        let iso = Isolated::create(&ws).unwrap();
        assert!(!iso.root().join(".env").exists(), "secrets are not copied");
        fs::create_dir_all(iso.root().join("target")).unwrap();
        fs::write(iso.root().join("target/out.bin"), "junk").unwrap();
        fs::write(iso.root().join(".env"), "PLANTED").unwrap();
        let merge = iso.merge(&ws, &AtomicBool::new(false));
        assert!(
            merge.applied.is_empty() && merge.conflicts.is_empty(),
            "{merge:?}"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(".env")).unwrap(),
            "SECRET"
        );
    }

    fn git_project() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "a1").unwrap();
        fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=dial",
            "-c",
            "user.email=dial@example.invalid",
            "commit",
            "-q",
            "-m",
            "init",
        ]);
        let ws = Workspace::new(dir.path()).unwrap();
        (dir, ws)
    }

    fn round_trip(preference: Preference, expected_backend: &str) {
        let (dir, ws) = git_project();
        let iso = Isolated::with_config(
            &ws,
            &super::super::agent_defs::TaskConfig::default(),
            preference,
        )
        .unwrap();
        let root = iso.root().to_path_buf();
        assert_ne!(root, ws.root());
        fs::write(root.join("a.txt"), "a2").unwrap();
        fs::write(root.join("new.txt"), "n").unwrap();
        let merge = iso.merge(&ws, &AtomicBool::new(false));
        assert!(
            merge.conflicts.is_empty() && merge.errors.is_empty(),
            "{merge:?}"
        );
        assert!(
            merge.backend.starts_with(expected_backend),
            "{}",
            merge.backend
        );
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "a2");
        assert_eq!(fs::read_to_string(dir.path().join("new.txt")).unwrap(), "n");
        assert!(!root.exists(), "copy removed");
        let list = std::process::Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let listed = String::from_utf8_lossy(&list.stdout);
        assert_eq!(listed.matches("worktree ").count(), 1, "{listed}");
    }

    #[test]
    fn worktree_backend_merges_back_and_removes_its_metadata() {
        round_trip(Preference::Worktree, "worktree");
    }

    #[test]
    fn copy_backend_merges_back_and_is_removed() {
        round_trip(Preference::Copy, "copy");
    }
}

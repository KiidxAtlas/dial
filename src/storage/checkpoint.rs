//! Per-session file checkpoints so a bad turn can be undone (`/rollback`).
//!
//! Before the first mutation of a file in a turn, the tool layer calls `before_write`; we store the
//! file's original bytes (or the fact that it did not exist) under
//! `<dir>/<turn>/{manifest.json,files/<n>}`. `rollback(n)` puts the files of the most recent `n`
//! turns back, newest first, so the oldest recorded state wins. Everything is on disk, so a rollback
//! works after dial restarts. Nothing outside the project root and the directories attached to the
//! workspace (`/add-dir`) is ever read or written.
//!
//! Only turns that touched files are materialised: a read-only turn leaves no trace and does not
//! count toward `rollback(n)`.
use super::atomic::write_checkpoint as atomic_write;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use similar::TextDiff;
use std::{
    fs,
    path::{Component, Path, PathBuf},
    sync::Mutex,
};

use crate::platform::fs as platform_fs;

/// Files larger than this are not recorded (they are listed in the warnings instead).
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointInfo {
    pub turn: u64,
    pub label: String,
    pub files: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RollbackReport {
    /// Files whose earlier content was written back (paths relative to the project root).
    pub restored: Vec<String>,
    /// Files that did not exist before the turn and were deleted.
    pub removed: Vec<String>,
    /// Things that could not be undone (files too large to checkpoint, restore failures).
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    /// Path relative to its workspace root, `/`-separated.
    rel: String,
    /// The added workspace directory holding the file; `None` is the project root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root: Option<PathBuf>,
    existed: bool,
    /// Unix permission bits of the original file.
    mode: u32,
}

impl Entry {
    /// The path shown to users and used to select the entry: project-relative for the project
    /// root, absolute for an added directory.
    fn display(&self) -> String {
        match &self.root {
            None => self.rel.clone(),
            Some(root) => root.join(&self.rel).to_string_lossy().into_owned(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Manifest {
    turn: u64,
    label: String,
    entries: Vec<Entry>,
    /// Files that were too large to record.
    skipped: Vec<String>,
}

struct Inner {
    next_turn: u64,
    /// The turn `before_write` currently records into.
    current: Option<Manifest>,
}

pub struct Checkpoints {
    dir: PathBuf,
    root: PathBuf,
    /// Directories attached to the workspace; files under them are recorded and restored too.
    added: Mutex<Vec<PathBuf>>,
    inner: Mutex<Inner>,
}

fn turn_dir(dir: &Path, turn: u64) -> PathBuf {
    dir.join(format!("{turn:010}"))
}

/// Turn numbers of the turn directories on disk, ascending.
fn disk_turns(dir: &Path) -> Vec<u64> {
    let mut turns: Vec<u64> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse().ok()))
        .collect();
    turns.sort_unstable();
    turns
}

fn read_manifest(dir: &Path, turn: u64) -> Result<Manifest> {
    let path = turn_dir(dir, turn).join("manifest.json");
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

fn write_manifest(dir: &Path, manifest: &Manifest) -> Result<()> {
    let path = turn_dir(dir, manifest.turn).join("manifest.json");
    atomic_write(&path, &serde_json::to_vec(manifest)?, 0o600)
}

impl Checkpoints {
    /// `dir` is this session's checkpoint directory (created if needed); `project_root` is the only
    /// tree files may be recorded from or restored into.
    pub fn new(dir: PathBuf, project_root: PathBuf) -> Result<Self> {
        let root = fs::canonicalize(&project_root)
            .with_context(|| format!("project root {}", project_root.display()))?;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        match platform_fs::create_dir_owner_only(&builder, &dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).with_context(|| format!("creating {}", dir.display())),
        }
        ensure!(
            platform_fs::open_dir_nofollow(&dir)?.metadata()?.is_dir(),
            "checkpoint directory must be a regular directory"
        );
        platform_fs::set_owner_only(&dir)?;
        let dir = fs::canonicalize(&dir)?;
        ensure!(
            !root.starts_with(&dir),
            "project directory must not be inside its checkpoint storage"
        );
        let next_turn = disk_turns(&dir).last().map_or(1, |t| t + 1);
        Ok(Self {
            dir,
            root,
            added: Mutex::new(Vec::new()),
            inner: Mutex::new(Inner {
                next_turn,
                current: None,
            }),
        })
    }

    /// Replace the directories attached to the workspace (canonical paths, as the workspace keeps
    /// them). Entries recorded under a directory that is later detached are not restored.
    pub fn set_added_roots(&self, roots: &[PathBuf]) {
        *self.added.lock().unwrap_or_else(|p| p.into_inner()) = roots.to_vec();
    }

    fn added_roots(&self) -> Vec<PathBuf> {
        self.added.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Start a new turn; later `before_write` calls record into it. Returns the turn number.
    pub fn begin_turn(&self, label: &str) -> Result<u64> {
        let mut inner = self.lock();
        let turn = inner.next_turn;
        inner.next_turn += 1;
        inner.current = Some(Manifest {
            turn,
            label: one_line(label),
            ..Manifest::default()
        });
        Ok(turn)
    }
    /// The number the next `begin_turn` returns; turns in `[before, after)` of two reads ran
    /// between them.
    pub fn next_turn(&self) -> u64 {
        self.lock().next_turn
    }
    /// Workspace-relative paths changed or skipped in `turn`, whether it is active or completed;
    /// an unknown turn id is empty.
    pub fn changed_files(&self, turn: u64) -> Vec<String> {
        let Ok(manifest) = self.manifest(turn) else {
            return Vec::new();
        };
        let mut paths: Vec<_> = manifest.entries.iter().map(Entry::display).collect();
        paths.extend(manifest.skipped.iter().cloned());
        paths.sort();
        paths.dedup();
        paths
    }
    /// Render the source delta captured by this turn's first-write checkpoints.
    /// The output is bounded and reports paths that need manual inspection.
    fn manifest(&self, turn: u64) -> Result<Manifest> {
        if let Some(manifest) = self.lock().current.as_ref().filter(|m| m.turn == turn) {
            return Ok(manifest.clone());
        }
        read_manifest(&self.dir, turn)
    }

    /// Preview a single file. The token binds both saved content and current bytes/mode.
    /// All current changes are shown, including user edits; caller must obtain explicit consent.
    pub fn review_file(&self, turn: u64, path: &str) -> Result<(String, String)> {
        use sha2::{Digest, Sha256};
        ensure!(
            crate::storage::experience::safe_path(path),
            "private path cannot be reviewed here"
        );
        let manifest = self.manifest(turn)?;
        let (index, entry) = manifest
            .entries
            .iter()
            .enumerate()
            .find(|(_, entry)| entry.display() == path)
            .context("file is not in this checkpoint")?;
        let before = if entry.existed {
            fs::read(
                turn_dir(&self.dir, turn)
                    .join("files")
                    .join(index.to_string()),
            )?
        } else {
            Vec::new()
        };
        let (target, _) = self.target_of(entry)?;
        let (after, mode, exists) = match fs::symlink_metadata(&target) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_file() && metadata.len() <= MAX_FILE_BYTES,
                    "file cannot be safely reviewed"
                );
                (fs::read(&target)?, platform_fs::mode(&metadata), true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Vec::new(), 0, false),
            Err(error) => return Err(error.into()),
        };
        ensure!(
            before.len() as u64 <= MAX_FILE_BYTES && after.len() as u64 <= MAX_FILE_BYTES,
            "file grew beyond review limit"
        );
        let before_hash: [u8; 32] = Sha256::digest(&before).into();
        let after_hash: [u8; 32] = Sha256::digest(&after).into();
        let identity = serde_json::to_vec(&(
            turn,
            path,
            entry.existed,
            entry.mode,
            mode,
            exists,
            before_hash,
            after_hash,
        ))?;
        let token = format!("{:x}", Sha256::digest(identity));
        let before_text =
            std::str::from_utf8(&before).context("binary file requires manual review")?;
        let after_text =
            std::str::from_utf8(&after).context("binary file requires manual review")?;
        let diff = TextDiff::from_lines(before_text, after_text)
            .unified_diff()
            .header("checkpoint", path)
            .to_string();
        ensure!(
            diff.len() <= 24 * 1024,
            "diff exceeds interactive review limit; inspect and restore manually"
        );
        Ok((diff, token))
    }

    /// Compare-before-restore; other checkpoint files and later unrelated work are untouched.
    /// The revert itself gets a checkpoint, so the operator can undo it.
    pub fn restore_file(&self, turn: u64, path: &str, expected: &str) -> Result<()> {
        let manifest = self.manifest(turn)?;
        let (index, entry) = manifest
            .entries
            .iter()
            .enumerate()
            .find(|(_, entry)| entry.display() == path)
            .context("file is not in this checkpoint")?;
        ensure!(
            self.review_file(turn, path)?.1 == expected,
            "file or saved checkpoint changed after review; nothing restored"
        );
        let (target, _) = self.target_of(entry)?;
        self.begin_turn(&format!("Selective revert: {path}"))?;
        self.before_write(&target)?;
        ensure!(
            self.review_file(turn, path)?.1 == expected,
            "file changed during review; nothing restored"
        );
        self.restore_entry(turn, index, entry)?;
        Ok(())
    }

    /// Capture exact scoped source states for executable repair evidence. Unlike `changed_files`,
    /// this fails closed on skipped/oversized files. Content-addressed snapshots live outside
    /// numbered rollback turns, so both failed and repaired source survive undo.
    pub fn repair_changes(
        &self,
        turn: u64,
    ) -> Result<Vec<crate::agent::repair_evidence::FileChange>> {
        use crate::agent::repair_evidence::FileChange;
        use sha2::{Digest, Sha256};
        let inner = self.lock();
        let manifest = inner
            .current
            .as_ref()
            .filter(|m| m.turn == turn)
            .context("no matching checkpoint turn")?;
        ensure!(
            manifest.skipped.is_empty(),
            "repair evidence cannot cover skipped checkpoint files"
        );
        ensure!(
            manifest.entries.len() <= 128,
            "repair evidence exceeds 128 scoped files"
        );
        let base = turn_dir(&self.dir, turn);
        let snapshots = self.dir.join("repair-evidence");
        let mut changes = Vec::with_capacity(manifest.entries.len());
        for (index, entry) in manifest.entries.iter().enumerate() {
            let before_path = base.join("files").join(index.to_string());
            let (before_sha256, before_ref) = if entry.existed {
                let bytes = fs::read(&before_path)?;
                let digest = format!("{:x}", Sha256::digest(&bytes));
                let saved = snapshots.join(&digest);
                if !saved.exists() {
                    atomic_write(&saved, &bytes, 0o600)?;
                }
                (Some(digest), Some(saved.to_string_lossy().into_owned()))
            } else {
                (None, None)
            };
            let (target, _) = self.target_of(entry)?;
            let (after_sha256, after_ref) = match fs::metadata(&target) {
                Ok(metadata) => {
                    ensure!(
                        metadata.is_file() && metadata.len() <= MAX_FILE_BYTES,
                        "repair evidence cannot snapshot {}",
                        entry.display()
                    );
                    let bytes = fs::read(&target)?;
                    ensure!(
                        bytes.len() as u64 <= MAX_FILE_BYTES,
                        "repair snapshot grew beyond file limit"
                    );
                    let digest = format!("{:x}", Sha256::digest(&bytes));
                    let saved = snapshots.join(&digest);
                    if !saved.exists() {
                        atomic_write(&saved, &bytes, 0o600)?;
                    }
                    (Some(digest), Some(saved.to_string_lossy().into_owned()))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (None, None),
                Err(error) => return Err(error.into()),
            };
            changes.push(FileChange {
                path: entry.display(),
                before_sha256,
                after_sha256,
                before_ref,
                after_ref,
            });
        }
        Ok(changes)
    }

    /// Record `abs` as it is now, unless this turn already did. Must be called BEFORE the file is
    /// changed or deleted. A path outside the project root and the added directories is an error
    /// (the write must not proceed); a file over `MAX_FILE_BYTES` is not recorded and is listed as
    /// skipped.
    pub fn before_write(&self, abs: &Path) -> Result<()> {
        let (resolved, root, rel) = self.resolve(abs)?;
        let shown = Entry {
            rel: rel.clone(),
            root: root.clone(),
            existed: false,
            mode: 0,
        }
        .display();
        let mut inner = self.lock();
        if inner.current.is_none() {
            let turn = inner.next_turn;
            inner.next_turn += 1;
            inner.current = Some(Manifest {
                turn,
                label: "(no turn started)".into(),
                ..Manifest::default()
            });
        }
        let Some(manifest) = inner.current.as_ref() else {
            bail!("no active checkpoint turn")
        };
        if manifest
            .entries
            .iter()
            .any(|e| e.rel == rel && e.root == root)
            || manifest.skipped.contains(&shown)
        {
            return Ok(());
        }
        let mut manifest = manifest.clone();

        match fs::symlink_metadata(&resolved) {
            Ok(meta) if meta.is_file() => {
                if meta.len() > MAX_FILE_BYTES {
                    manifest.skipped.push(shown);
                    write_manifest(&self.dir, &manifest)?;
                    inner.current = Some(manifest);
                    return Ok(());
                }
                let bytes = fs::read(&resolved)
                    .with_context(|| format!("reading {shown} for checkpoint"))?;
                let blob = turn_dir(&self.dir, manifest.turn)
                    .join("files")
                    .join(manifest.entries.len().to_string());
                atomic_write(&blob, &bytes, 0o600)?;
                manifest.entries.push(Entry {
                    rel,
                    root,
                    existed: true,
                    mode: platform_fs::mode(&meta) & 0o7777,
                });
            }
            Ok(_) => bail!("{shown} is not a regular file; refusing to checkpoint it"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                manifest.entries.push(Entry {
                    rel,
                    root,
                    existed: false,
                    mode: 0,
                });
            }
            Err(e) => return Err(e).with_context(|| format!("inspecting {shown}")),
        }
        write_manifest(&self.dir, &manifest)?;
        inner.current = Some(manifest);
        Ok(())
    }

    /// Checkpointed turns, newest first.
    pub fn list(&self) -> Vec<CheckpointInfo> {
        let _inner = self.lock();
        let mut out: Vec<CheckpointInfo> = disk_turns(&self.dir)
            .into_iter()
            .rev()
            .filter_map(|t| read_manifest(&self.dir, t).ok())
            .map(|m| CheckpointInfo {
                turn: m.turn,
                label: m.label,
                files: m.entries.len() + m.skipped.len(),
            })
            .collect();
        out.retain(|c| c.files > 0);
        out
    }

    /// Undo the most recent `turns` checkpointed turns, newest first. Consumed checkpoints are
    /// deleted, so the next rollback goes further back. Edits made outside dial since are
    /// overwritten without a check.
    pub fn rollback(&self, turns: usize) -> Result<RollbackReport> {
        ensure!(turns > 0, "rollback needs at least one turn");
        let mut inner = self.lock();
        let mut report = RollbackReport::default();
        let mut manifests: Vec<Manifest> = Vec::new();
        for t in disk_turns(&self.dir).into_iter().rev() {
            if manifests.len() == turns {
                break;
            }
            match read_manifest(&self.dir, t) {
                Ok(m) if !m.entries.is_empty() || !m.skipped.is_empty() => manifests.push(m),
                Ok(_) => {}
                Err(e) => report
                    .warnings
                    .push(format!("turn {t}: unreadable checkpoint skipped: {e:#}")),
            }
        }
        ensure!(
            !manifests.is_empty(),
            "nothing to roll back: no checkpointed turns"
        );
        for manifest in &manifests {
            let mut failed = false;
            for skipped in &manifest.skipped {
                report.warnings.push(format!(
                    "turn {}: {skipped} was too large to checkpoint and was not restored",
                    manifest.turn
                ));
            }
            for (index, entry) in manifest.entries.iter().enumerate().rev() {
                match self.restore_entry(manifest.turn, index, entry) {
                    Ok(true) => report.restored.push(entry.display()),
                    Ok(false) => report.removed.push(entry.display()),
                    Err(e) => {
                        failed = true;
                        report.warnings.push(format!(
                            "turn {}: {}: {e:#}",
                            manifest.turn,
                            entry.display()
                        ));
                    }
                }
            }
            // Only discard a checkpoint whose files all came back; otherwise keep it for a retry.
            if !failed {
                fs::remove_dir_all(turn_dir(&self.dir, manifest.turn)).ok();
            }
        }
        inner.current = None;
        Ok(report)
    }

    /// Map a path the tools hand us to (resolved absolute path, added root holding it or `None`
    /// for the project root, root-relative `/` path), rejecting anything that resolves outside
    /// every root. Symlinks are resolved before checking; the deepest containing root wins.
    fn resolve(&self, abs: &Path) -> Result<(PathBuf, Option<PathBuf>, String)> {
        ensure!(
            abs.is_absolute(),
            "checkpoint path must be absolute: {}",
            abs.display()
        );
        ensure!(
            !abs.components().any(|c| c == Component::ParentDir),
            "path contains '..': {}",
            abs.display()
        );
        // Resolve the deepest existing ancestor, then re-append the not-yet-existing tail.
        let mut existing = abs;
        let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
        let base = loop {
            match fs::canonicalize(existing) {
                Ok(p) => break p,
                Err(_) => {
                    let (Some(parent), Some(name)) = (existing.parent(), existing.file_name())
                    else {
                        bail!("cannot resolve {}", abs.display())
                    };
                    tail.push(name);
                    existing = parent;
                }
            }
        };
        let mut resolved = base;
        for name in tail.iter().rev() {
            resolved.push(name);
        }
        ensure!(
            !resolved.starts_with(&self.dir) && !self.dir.starts_with(&resolved),
            "cannot checkpoint private checkpoint storage or its ancestors"
        );
        let added = self.added_roots();
        let root = std::iter::once((None, self.root.as_path()))
            .chain(added.iter().map(|root| (Some(root), root.as_path())))
            .filter(|(_, root)| resolved.starts_with(root))
            .max_by_key(|(_, root)| root.components().count())
            .map(|(added, root)| (added.cloned(), root))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{} is outside the project root and the added workspace directories",
                    abs.display()
                )
            })?;
        let rel = resolved.strip_prefix(root.1)?;
        let rel = rel.to_str().context("path is not valid UTF-8")?.to_string();
        ensure!(!rel.is_empty(), "cannot checkpoint a workspace root itself");
        Ok((resolved, root.0, rel))
    }

    /// Where `entry` lives now: (target path, the root it belongs to). Refuses unsafe relative
    /// paths, directories no longer attached, and paths that now resolve into another root.
    fn target_of(&self, entry: &Entry) -> Result<(PathBuf, PathBuf)> {
        let rel = Path::new(&entry.rel);
        ensure!(
            rel.components().all(|c| matches!(c, Component::Normal(_))),
            "unsafe path in checkpoint"
        );
        let base = match &entry.root {
            None => self.root.clone(),
            Some(root) => {
                ensure!(
                    self.added_roots().contains(root),
                    "{} is no longer attached to the workspace; not touching it",
                    root.display()
                );
                root.clone()
            }
        };
        let (target, root, _) = self.resolve(&base.join(rel))?;
        ensure!(
            root == entry.root,
            "checkpoint path now resolves into another workspace directory"
        );
        Ok((target, base))
    }

    /// Restore one entry. `Ok(true)` = content written back, `Ok(false)` = created file removed.
    fn restore_entry(&self, turn: u64, index: usize, entry: &Entry) -> Result<bool> {
        let (target, base) = self.target_of(entry)?;
        if entry.existed {
            let blob = turn_dir(&self.dir, turn)
                .join("files")
                .join(index.to_string());
            let bytes = fs::read(&blob)
                .with_context(|| format!("reading saved content {}", blob.display()))?;
            if let Ok(meta) = fs::symlink_metadata(&target) {
                ensure!(
                    meta.is_file(),
                    "the path is now a directory or special file; not overwriting"
                );
            }
            atomic_write(&target, &bytes, entry.mode & 0o7777)?;
            Ok(true)
        } else {
            match fs::symlink_metadata(&target) {
                Ok(meta) if meta.is_file() => fs::remove_file(&target)?,
                Ok(_) => bail!("the path is now a directory or special file; not deleting"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            // Drop directories the turn created and left empty, never the root itself.
            let mut parent = target.parent();
            while let Some(dir) = parent.filter(|d| *d != base && d.starts_with(&base)) {
                if fs::remove_dir(dir).is_err() {
                    break;
                }
                parent = dir.parent();
            }
            Ok(false)
        }
    }
}

fn one_line(label: &str) -> String {
    let flat: String = label
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    flat.chars().take(120).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _state: tempfile::TempDir,
        project: tempfile::TempDir,
        dir: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let state = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            let dir = state.path().join("session-1");
            Self {
                _state: state,
                project,
                dir,
            }
        }
        fn root(&self) -> PathBuf {
            fs::canonicalize(self.project.path()).unwrap()
        }
        fn open(&self) -> Checkpoints {
            Checkpoints::new(self.dir.clone(), self.root()).unwrap()
        }
        fn path(&self, rel: &str) -> PathBuf {
            self.root().join(rel)
        }
        fn write(&self, rel: &str, text: &str) {
            let p = self.path(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
        }
        fn read(&self, rel: &str) -> Option<String> {
            fs::read_to_string(self.path(rel)).ok()
        }
    }
    #[test]
    fn changed_paths_are_relative_and_scoped_to_their_turn() {
        let fixture = Fixture::new();
        let checkpoint = fixture.open();
        fixture.write("src/lib.rs", "before");
        let first = checkpoint.begin_turn("edit").unwrap();
        checkpoint
            .before_write(&fixture.path("src/lib.rs"))
            .unwrap();
        assert_eq!(checkpoint.changed_files(first), vec!["src/lib.rs"]);
        assert!(checkpoint.changed_files(first + 1).is_empty());

        let second = checkpoint.begin_turn("next").unwrap();
        fixture.write("README.md", "new");
        checkpoint.before_write(&fixture.path("README.md")).unwrap();
        // A completed turn keeps its inventory: `app.rs` lets the user review any earlier turn.
        assert_eq!(checkpoint.changed_files(first), vec!["src/lib.rs"]);
        assert_eq!(checkpoint.changed_files(second), vec!["README.md"]);
    }

    #[test]
    fn repair_sources_are_exact_and_survive_further_edits_and_rollback() {
        let fixture = Fixture::new();
        let checkpoint = fixture.open();
        fixture.write("src/lib.rs", "original");
        let turn = checkpoint.begin_turn("repair").unwrap();
        modify(&checkpoint, &fixture, "src/lib.rs", "failing");
        let failure = checkpoint.repair_changes(turn).unwrap();
        modify(&checkpoint, &fixture, "src/lib.rs", "passing");
        let pass = checkpoint.repair_changes(turn).unwrap();
        assert_ne!(failure[0].after_sha256, pass[0].after_sha256);
        assert_eq!(failure[0].before_sha256, pass[0].before_sha256);
        checkpoint.rollback(1).unwrap();
        assert_eq!(
            fs::read_to_string(failure[0].before_ref.as_ref().unwrap()).unwrap(),
            "original"
        );
        assert_eq!(
            fs::read_to_string(failure[0].after_ref.as_ref().unwrap()).unwrap(),
            "failing"
        );
        assert_eq!(
            fs::read_to_string(pass[0].after_ref.as_ref().unwrap()).unwrap(),
            "passing"
        );
        assert!(checkpoint.list().is_empty());
    }

    #[test]
    fn repair_sources_represent_creation_and_deletion() {
        let fixture = Fixture::new();
        let checkpoint = fixture.open();
        let turn = checkpoint.begin_turn("create").unwrap();
        modify(&checkpoint, &fixture, "new.rs", "new");
        let created = checkpoint.repair_changes(turn).unwrap();
        assert_eq!(created[0].before_sha256, None);
        assert_eq!(created[0].before_ref, None);
        assert!(created[0].after_ref.is_some());
        fs::remove_file(fixture.path("new.rs")).unwrap();
        let removed = checkpoint.repair_changes(turn).unwrap();
        assert_eq!(removed[0].after_sha256, None);
        assert_eq!(removed[0].after_ref, None);
        assert!(checkpoint.repair_changes(turn + 1).is_err());
    }

    /// Mimic a tool: record, then mutate.
    fn modify(cp: &Checkpoints, fx: &Fixture, rel: &str, text: &str) {
        cp.before_write(&fx.path(rel)).unwrap();
        fx.write(rel, text);
    }

    #[test]
    fn rolls_back_modified_created_and_deleted_files() {
        let fx = Fixture::new();
        fx.write("a.txt", "A0");
        fx.write("b.txt", "B0");
        let cp = fx.open();
        cp.begin_turn("turn one").unwrap();
        modify(&cp, &fx, "a.txt", "A1");
        modify(&cp, &fx, "new/deep/c.txt", "C1");
        cp.before_write(&fx.path("b.txt")).unwrap();
        fs::remove_file(fx.path("b.txt")).unwrap();

        let report = cp.rollback(1).unwrap();
        assert_eq!(fx.read("a.txt").as_deref(), Some("A0"));
        assert_eq!(fx.read("b.txt").as_deref(), Some("B0"));
        assert!(!fx.path("new/deep/c.txt").exists());
        assert!(
            !fx.path("new").exists(),
            "directories created by the turn are cleaned up"
        );
        let mut restored = report.restored.clone();
        restored.sort();
        assert_eq!(restored, ["a.txt", "b.txt"]);
        assert_eq!(report.removed, ["new/deep/c.txt"]);
        assert!(report.warnings.is_empty());
        assert!(cp.list().is_empty(), "consumed checkpoints are gone");
        assert!(cp.rollback(1).is_err(), "nothing left to roll back");
    }

    #[test]
    fn only_the_first_write_per_turn_is_recorded() {
        let fx = Fixture::new();
        fx.write("a.txt", "A0");
        let cp = fx.open();
        cp.begin_turn("t").unwrap();
        modify(&cp, &fx, "a.txt", "A1");
        modify(&cp, &fx, "a.txt", "A2");
        modify(&cp, &fx, "a.txt", "A3");
        assert_eq!(
            cp.list(),
            [CheckpointInfo {
                turn: 1,
                label: "t".into(),
                files: 1
            }]
        );
        cp.rollback(1).unwrap();
        assert_eq!(fx.read("a.txt").as_deref(), Some("A0"));
    }

    #[test]
    fn multiple_turns_roll_back_newest_first_and_partially() {
        let fx = Fixture::new();
        fx.write("a.txt", "A0");
        let cp = fx.open();
        cp.begin_turn("one").unwrap();
        modify(&cp, &fx, "a.txt", "A1");
        modify(&cp, &fx, "x.txt", "X1");
        cp.begin_turn("two").unwrap();
        modify(&cp, &fx, "a.txt", "A2");
        cp.begin_turn("read only").unwrap();
        cp.begin_turn("three").unwrap();
        modify(&cp, &fx, "a.txt", "A3");
        modify(&cp, &fx, "x.txt", "X3");

        let labels: Vec<_> = cp
            .list()
            .into_iter()
            .map(|c| (c.turn, c.label, c.files))
            .collect();
        assert_eq!(
            labels,
            [
                (4, "three".to_string(), 2),
                (2, "two".to_string(), 1),
                (1, "one".to_string(), 2)
            ]
        );

        cp.rollback(1).unwrap();
        assert_eq!(
            (fx.read("a.txt").as_deref(), fx.read("x.txt").as_deref()),
            (Some("A2"), Some("X1"))
        );
        // Two more turns: the oldest recorded state (A0, no x.txt) must win.
        cp.rollback(2).unwrap();
        assert_eq!(fx.read("a.txt").as_deref(), Some("A0"));
        assert!(!fx.path("x.txt").exists());
    }

    #[test]
    fn rollback_survives_restart() {
        let fx = Fixture::new();
        fx.write("a.txt", "A0");
        {
            let cp = fx.open();
            cp.begin_turn("before restart").unwrap();
            modify(&cp, &fx, "a.txt", "A1");
            modify(&cp, &fx, "n.txt", "N1");
        }
        let cp = fx.open();
        assert_eq!(cp.list().len(), 1);
        // New turns after reopen get fresh numbers and stack on top.
        assert_eq!(cp.begin_turn("after restart").unwrap(), 2);
        modify(&cp, &fx, "a.txt", "A2");
        cp.rollback(2).unwrap();
        assert_eq!(fx.read("a.txt").as_deref(), Some("A0"));
        assert!(!fx.path("n.txt").exists());
    }

    #[test]
    fn restores_binary_content_and_permissions() {
        let fx = Fixture::new();
        let bin: Vec<u8> = (0..=255u8).collect();
        fs::write(fx.path("blob.bin"), &bin).unwrap();
        #[cfg(unix)]
        platform_fs::set_mode(&fx.path("blob.bin"), 0o755).unwrap();
        let cp = fx.open();
        cp.begin_turn("t").unwrap();
        cp.before_write(&fx.path("blob.bin")).unwrap();
        fs::write(fx.path("blob.bin"), b"x").unwrap();
        #[cfg(unix)]
        platform_fs::set_mode(&fx.path("blob.bin"), 0o600).unwrap();
        cp.rollback(1).unwrap();
        assert_eq!(fs::read(fx.path("blob.bin")).unwrap(), bin);
        #[cfg(unix)]
        assert_eq!(
            platform_fs::mode(&fs::metadata(fx.path("blob.bin")).unwrap()) & 0o777,
            0o755
        );
    }

    #[test]
    fn refuses_paths_outside_the_project() {
        let fx = Fixture::new();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "s").unwrap();
        let cp = fx.open();
        cp.begin_turn("t").unwrap();
        assert!(cp.before_write(&outside.path().join("secret")).is_err());
        assert!(cp.before_write(&outside.path().join("not-yet")).is_err());
        assert!(cp.before_write(&fx.root().join("../escape.txt")).is_err());
        assert!(cp.before_write(Path::new("relative.txt")).is_err());
        assert!(
            cp.before_write(&fx.root()).is_err(),
            "the root itself is not a file"
        );
        // A symlink inside the project that points out is judged by where it leads.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path().join("secret"), fx.path("link")).unwrap();
            assert!(cp.before_write(&fx.path("link")).is_err());
            std::os::unix::fs::symlink(outside.path(), fx.path("linkdir")).unwrap();
            assert!(cp.before_write(&fx.path("linkdir/new.txt")).is_err());
        }
        assert!(cp.list().is_empty());
    }

    #[test]
    fn tampered_manifest_cannot_restore_outside_the_root() {
        let fx = Fixture::new();
        fx.write("a.txt", "A0");
        let cp = fx.open();
        cp.begin_turn("t").unwrap();
        modify(&cp, &fx, "a.txt", "A1");
        let manifest_path = turn_dir(&cp.dir, 1).join("manifest.json");
        let evil = tempfile::tempdir().unwrap();
        let target = evil.path().join("pwned");
        let text = fs::read_to_string(&manifest_path)
            .unwrap()
            .replace("a.txt", &format!("../../..{}", target.display()));
        fs::write(&manifest_path, text).unwrap();
        let report = cp.rollback(1).unwrap();
        assert!(report.restored.is_empty() && !report.warnings.is_empty());
        assert!(!target.exists());
        assert!(
            turn_dir(&cp.dir, 1).exists(),
            "a failed restore keeps the checkpoint for another try"
        );
    }

    #[test]
    fn oversized_files_are_skipped_with_a_warning() {
        let fx = Fixture::new();
        let big = fx.path("big.bin");
        fs::File::create(&big)
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        fx.write("a.txt", "A0");
        let cp = fx.open();
        cp.begin_turn("t").unwrap();
        cp.before_write(&big).unwrap();
        cp.before_write(&big).unwrap();
        modify(&cp, &fx, "a.txt", "A1");
        let report = cp.rollback(1).unwrap();
        assert_eq!(report.restored, ["a.txt"]);
        assert!(report.warnings.iter().any(|w| w.contains("big.bin")));
    }

    #[test]
    fn implicit_turn_and_nested_checkpoint_storage_stay_separate() {
        let fx = Fixture::new();
        fx.write("a.txt", "A0");
        let cp = fx.open();
        modify(&cp, &fx, "a.txt", "A1");
        assert_eq!(cp.list().len(), 1);
        let nested_dir = fx.root().join(".cp");
        let nested = Checkpoints::new(nested_dir.clone(), fx.root()).unwrap();
        assert!(
            nested
                .before_write(&nested_dir.join("manifest.json"))
                .is_err()
        );
        nested.before_write(&fx.path("a.txt")).unwrap();
        fs::write(fx.path("a.txt"), "A2").unwrap();
        assert_eq!(nested.rollback(1).unwrap().restored, ["a.txt"]);
        assert_eq!(fs::read_to_string(fx.path("a.txt")).unwrap(), "A1");
        assert!(cp.rollback(0).is_err());
    }

    /// Files under a directory attached with `/add-dir` are recorded and rolled back like project
    /// files; created files are removed together with the directories the turn made.
    #[test]
    fn added_directories_are_recorded_and_rolled_back() {
        let fx = Fixture::new();
        let extra = tempfile::tempdir().unwrap();
        let extra_root = fs::canonicalize(extra.path()).unwrap();
        fs::write(extra_root.join("page.vue"), "old").unwrap();
        let cp = fx.open();
        assert!(
            cp.before_write(&extra_root.join("page.vue")).is_err(),
            "not attached yet"
        );
        cp.set_added_roots(std::slice::from_ref(&extra_root));
        cp.begin_turn("t").unwrap();
        cp.before_write(&extra_root.join("page.vue")).unwrap();
        fs::write(extra_root.join("page.vue"), "new").unwrap();
        let created = extra_root.join("public/install.sh");
        cp.before_write(&created).unwrap();
        fs::create_dir_all(created.parent().unwrap()).unwrap();
        fs::write(&created, "#!/bin/sh").unwrap();

        let shown = extra_root.join("page.vue").to_string_lossy().into_owned();
        assert!(
            cp.changed_files(1).contains(&shown),
            "{:?}",
            cp.changed_files(1)
        );
        let report = cp.rollback(1).unwrap();
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(report.restored, [shown]);
        assert_eq!(
            fs::read_to_string(extra_root.join("page.vue")).unwrap(),
            "old"
        );
        assert!(!created.exists() && !extra_root.join("public").exists());
        assert!(
            extra_root.exists(),
            "the added root itself is never removed"
        );
    }

    /// Detaching a directory stops restores into it; a manifest naming a root that is not
    /// attached cannot write there.
    #[test]
    fn detached_or_forged_added_roots_are_not_restored_into() {
        let fx = Fixture::new();
        let extra = tempfile::tempdir().unwrap();
        let extra_root = fs::canonicalize(extra.path()).unwrap();
        fs::write(extra_root.join("f.txt"), "old").unwrap();
        let cp = fx.open();
        cp.set_added_roots(std::slice::from_ref(&extra_root));
        cp.begin_turn("t").unwrap();
        cp.before_write(&extra_root.join("f.txt")).unwrap();
        fs::write(extra_root.join("f.txt"), "new").unwrap();
        cp.set_added_roots(&[]);
        let report = cp.rollback(1).unwrap();
        assert!(report.restored.is_empty());
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("no longer attached"))
        );
        assert_eq!(fs::read_to_string(extra_root.join("f.txt")).unwrap(), "new");

        let evil = tempfile::tempdir().unwrap();
        let evil_root = fs::canonicalize(evil.path()).unwrap();
        let manifest_path = turn_dir(&cp.dir, 1).join("manifest.json");
        let text = fs::read_to_string(&manifest_path).unwrap().replace(
            &serde_json::to_string(&extra_root).unwrap(),
            &serde_json::to_string(&evil_root).unwrap(),
        );
        fs::write(&manifest_path, text).unwrap();
        cp.set_added_roots(std::slice::from_ref(&extra_root));
        let report = cp.rollback(1).unwrap();
        assert!(report.restored.is_empty() && !report.warnings.is_empty());
        assert!(!evil_root.join("f.txt").exists());
    }
}

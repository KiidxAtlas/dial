//! Per-session file checkpoints so a bad turn can be undone (`/rollback`).
//!
//! Before the first mutation of a file in a turn, the tool layer calls `before_write`; we store the
//! file's original bytes (or the fact that it did not exist) under
//! `<dir>/<turn>/{manifest.json,files/<n>}`. `rollback(n)` puts the files of the most recent `n`
//! turns back, newest first, so the oldest recorded state wins. Everything is on disk, so a rollback
//! works after dial restarts. Nothing outside the project root and the directories attached to the
//! workspace (`/add-dir`) is ever read or written.
//!
//! Standalone turns materialize only on writes. Conversation-linked turns also persist empty
//! manifests so `/undo` can distinguish a read-only turn from missing history; these do not
//! count toward `rollback(n)`.
use crate::platform::atomic::{write_checkpoint as atomic_write, write_checkpoint_deferred};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
/// Longest a review diff may search for a minimal edit script before settling for a coarser one.
const DIFF_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);
use std::{
    fs,
    path::{Component, Path, PathBuf},
    sync::Mutex,
};

use crate::platform::fs as platform_fs;
mod preserved;
pub use preserved::PreservedInput;

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

/// One file a turn changed: its content before the turn's first write and its content now.
/// `None` means the file did not exist at that point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnVersion {
    /// Project-relative for the project root, absolute for an added directory.
    pub path: String,
    /// Whether the file lives under the project root rather than an added directory.
    pub in_project: bool,
    pub before: Option<Vec<u8>>,
    pub after: Option<Vec<u8>>,
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
    /// Who wrote the file this turn: `main` or a worker label. Empty when the checkpoint predates
    /// attribution, so old checkpoints are shown as unattributed rather than guessed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    writers: Vec<String>,
    /// Byte length of the saved content (`files/<n>`). Saved content is flushed in the
    /// background, so a power loss can truncate it; restores refuse a mismatch. `None` in
    /// checkpoints written before the length was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    len: Option<u64>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session: Option<String>,
    /// Restoration completed, but the linked conversation rewind has not committed yet.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    restored: bool,
}

#[derive(Clone, Copy)]
enum RestoreMode {
    Consume,
    Conversation,
}

struct Inner {
    next_turn: u64,
    /// The turn `before_write` currently records into.
    current: Option<Manifest>,
    /// Latest change per file in `current`, for the agent's single-file revert.
    journal: revisions::Journal,
}

mod deferred;
mod review;
mod revisions;
pub use review::{GroupPlan, ReviewFile, TurnReview, UNATTRIBUTED};
pub use revisions::RevertPlan;

pub struct Checkpoints {
    dir: PathBuf,
    root: PathBuf,
    /// Directories attached to the workspace; files under them are recorded and restored too.
    added: Mutex<Vec<PathBuf>>,
    inner: Mutex<Inner>,
    /// Background flushes of records published on the write path.
    flusher: deferred::Flusher,
}

fn turn_dir(dir: &Path, turn: u64) -> PathBuf {
    dir.join(format!("{turn:010}"))
}

/// Remove a restored turn. The manifest goes last, so a failure partway leaves a checkpoint that
/// is still listed and can be retried rather than orphaned saved files.
fn discard_turn(dir: &Path, turn: u64) -> std::io::Result<()> {
    let base = turn_dir(dir, turn);
    match fs::remove_dir_all(base.join("files")) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    fs::remove_dir_all(base)
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

/// Read a turn's saved content of `entry` (`files/<index>`), refusing a truncated record.
fn read_saved(dir: &Path, turn: u64, index: usize, entry: &Entry) -> Result<Vec<u8>> {
    let blob = turn_dir(dir, turn).join("files").join(index.to_string());
    let bytes =
        fs::read(&blob).with_context(|| format!("reading saved content {}", blob.display()))?;
    if let Some(len) = entry.len {
        ensure!(
            bytes.len() as u64 == len,
            "saved content {} is incomplete ({} of {len} bytes); not restoring",
            blob.display(),
            bytes.len()
        );
    }
    Ok(bytes)
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
        let recorded_next = match fs::read(dir.join("next_turn")) {
            Ok(bytes) => Some(
                serde_json::from_slice::<u64>(&bytes).context("invalid checkpoint turn counter")?,
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("reading checkpoint turn counter"),
        };
        let next_turn = disk_turns(&dir)
            .last()
            .map_or(1, |t| t + 1)
            .max(recorded_next.unwrap_or(1));
        Ok(Self {
            dir,
            root,
            added: Mutex::new(Vec::new()),
            inner: Mutex::new(Inner {
                next_turn,
                current: None,
                journal: Default::default(),
            }),
            flusher: deferred::Flusher::default(),
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
    /// Start a new standalone turn; later writes record into it. Returns its unique turn number.
    pub fn begin_turn(&self, label: &str) -> Result<u64> {
        self.begin_linked_turn(label, None)
    }

    /// Start a durable conversation-linked turn, including turns that never write a file.
    /// The session identity prevents copied conversation records from addressing another journal.
    pub fn begin_conversation_turn(&self, label: &str, session: &str) -> Result<u64> {
        self.begin_linked_turn(label, Some(session))
    }

    fn begin_linked_turn(&self, label: &str, session: Option<&str>) -> Result<u64> {
        // Earlier turns' records must be durable, and a failed flush reported, before a new turn.
        self.flusher.drain()?;
        let mut inner = self.lock();
        let turn = inner.next_turn;
        let next = turn
            .checked_add(1)
            .context("checkpoint turn counter exhausted")?;
        // Never reuse a durable link, even after every checkpoint has been rolled back.
        atomic_write(
            &self.dir.join("next_turn"),
            &serde_json::to_vec(&next)?,
            0o600,
        )?;
        inner.next_turn = next;
        inner.journal.reset(&self.dir, None);
        let manifest = Manifest {
            turn,
            label: one_line(label),
            session: session.map(str::to_owned),
            ..Manifest::default()
        };
        if session.is_some() {
            write_manifest(&self.dir, &manifest)?;
        }
        inner.current = Some(manifest);
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
        let manifest = self.manifest(turn)?;
        self.review_with(&manifest, path)
    }

    fn review_with(&self, manifest: &Manifest, path: &str) -> Result<(String, String)> {
        let (diff, token) = self.review_parts(manifest, path, true)?;
        Ok((diff.unwrap_or_default(), token))
    }

    /// The review token alone, for compare-before-restore. It hashes the current content, so any
    /// change since the operator reviewed the diff is still detected without rebuilding the diff
    /// while the checkpoint lock is held.
    fn review_token(&self, manifest: &Manifest, path: &str) -> Result<String> {
        self.review_parts(manifest, path, false)
            .map(|(_, token)| token)
    }

    fn review_parts(
        &self,
        manifest: &Manifest,
        path: &str,
        with_diff: bool,
    ) -> Result<(Option<String>, String)> {
        use sha2::{Digest, Sha256};
        let turn = manifest.turn;
        ensure!(
            crate::storage::experience::safe_path(path),
            "private path cannot be reviewed here"
        );
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
        if !with_diff {
            return Ok((None, token));
        }
        // Myers diff is O((N+M)·D); a dense rewrite of a multi-megabyte file could take minutes.
        // Past the deadline `similar` returns a coarser but still correct diff.
        let diff = similar::TextDiff::configure()
            .timeout(DIFF_TIMEOUT)
            .diff_lines(before_text, after_text)
            .unified_diff()
            .header("checkpoint", path)
            .to_string();
        ensure!(
            diff.len() <= 24 * 1024,
            "diff exceeds interactive review limit; inspect and restore manually"
        );
        Ok((Some(diff), token))
    }

    /// Compare-before-restore; other checkpoint files and later unrelated work are untouched.
    /// The revert itself gets a checkpoint, so the operator can undo it. The caller has shown the
    /// overlap signals (see [`ReviewFile::overlap`]) and obtained consent for this one file.
    pub fn restore_file(&self, turn: u64, path: &str, expected: &str) -> Result<()> {
        self.restore_files(turn, &[(path.to_owned(), expected.to_owned())], true)
            .map(|_| ())
    }

    /// Capture exact scoped source states for executable repair evidence. Unlike `changed_files`,
    /// this fails closed on skipped/oversized files. Content-addressed snapshots live outside
    /// numbered rollback turns, so both failed and repaired source survive undo.
    pub fn repair_changes(
        &self,
        turn: u64,
    ) -> Result<Vec<crate::storage::repair_evidence::FileChange>> {
        use crate::storage::repair_evidence::FileChange;
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

    /// Before and current content of every file `turn` recorded. Files skipped as oversized, no
    /// longer resolvable, or now larger than [`MAX_FILE_BYTES`] are left out; this is a best-effort
    /// view for history, unlike [`Self::repair_changes`], which fails closed.
    pub fn turn_versions(&self, turn: u64) -> Result<Vec<TurnVersion>> {
        let manifest = self.manifest(turn)?;
        let base = turn_dir(&self.dir, turn);
        let mut versions = Vec::with_capacity(manifest.entries.len());
        for (index, entry) in manifest.entries.iter().enumerate() {
            let before = if entry.existed {
                match fs::read(base.join("files").join(index.to_string())) {
                    Ok(bytes) => Some(bytes),
                    Err(_) => continue,
                }
            } else {
                None
            };
            let Ok((target, _)) = self.target_of(entry) else {
                continue;
            };
            let after = match fs::symlink_metadata(&target) {
                Ok(meta) if meta.is_file() && meta.len() <= MAX_FILE_BYTES => {
                    match fs::read(&target) {
                        Ok(bytes) => Some(bytes),
                        Err(_) => continue,
                    }
                }
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => continue,
            };
            versions.push(TurnVersion {
                path: entry.display(),
                in_project: entry.root.is_none(),
                before,
                after,
            });
        }
        Ok(versions)
    }

    /// Record `abs` as it is now, unless this turn already did. Must be called BEFORE the file is
    /// changed or deleted. A path outside the project root and the added directories is an error
    /// (the write must not proceed); a file over `MAX_FILE_BYTES` is not recorded and is listed as
    /// skipped. Every call also opens a per-change revision (see [`revisions`]).
    pub fn before_write(&self, abs: &Path) -> Result<()> {
        self.before_write_as(abs, "main")
    }

    /// [`Self::before_write`] that also records `owner` (`main` or a worker label) as a writer of
    /// the file in this turn, so review can group changes and see files several writers touched.
    pub fn before_write_as(&self, abs: &Path, owner: &str) -> Result<()> {
        let (resolved, root, rel) = self.resolve(abs)?;
        let mut inner = self.lock();
        self.record_first_write(&mut inner, &resolved, root.clone(), rel.clone())?;
        self.record_revision(&mut inner, &resolved, root.clone(), rel.clone());
        let owner = one_line(owner);
        if let Some(manifest) = inner.current.as_mut()
            && let Some(index) = manifest
                .entries
                .iter()
                .position(|entry| entry.rel == rel && entry.root == root)
            && !manifest.entries[index].writers.contains(&owner)
        {
            // Persist first: a failed write must leave the attribution unrecorded so a retry
            // writes it again, as `record_first_write` does.
            let mut saved = manifest.clone();
            saved.entries[index].writers.push(owner);
            self.write_manifest_deferred(&saved)?;
            *manifest = saved;
        }
        Ok(())
    }

    fn record_first_write(
        &self,
        inner: &mut Inner,
        resolved: &Path,
        root: Option<PathBuf>,
        rel: String,
    ) -> Result<()> {
        let shown = Entry {
            rel: rel.clone(),
            root: root.clone(),
            existed: false,
            mode: 0,
            writers: Vec::new(),
            len: None,
        }
        .display();
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

        match fs::symlink_metadata(resolved) {
            Ok(meta) if meta.is_file() => {
                if meta.len() > MAX_FILE_BYTES {
                    manifest.skipped.push(shown);
                    self.write_manifest_deferred(&manifest)?;
                    inner.current = Some(manifest);
                    return Ok(());
                }
                let bytes = fs::read(resolved)
                    .with_context(|| format!("reading {shown} for checkpoint"))?;
                let blob = turn_dir(&self.dir, manifest.turn)
                    .join("files")
                    .join(manifest.entries.len().to_string());
                self.write_deferred(&blob, &bytes)?;
                manifest.entries.push(Entry {
                    rel,
                    root,
                    existed: true,
                    mode: platform_fs::mode(&meta) & 0o7777,
                    writers: Vec::new(),
                    len: Some(bytes.len() as u64),
                });
            }
            Ok(_) => bail!("{shown} is not a regular file; refusing to checkpoint it"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                manifest.entries.push(Entry {
                    rel,
                    root,
                    existed: false,
                    mode: 0,
                    writers: Vec::new(),
                    len: None,
                });
            }
            Err(e) => return Err(e).with_context(|| format!("inspecting {shown}")),
        }
        self.write_manifest_deferred(&manifest)?;
        inner.current = Some(manifest);
        Ok(())
    }

    /// Publish a checkpoint record now and flush it in the background.
    fn write_deferred(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        self.flusher
            .submit(write_checkpoint_deferred(path, bytes, 0o600)?);
        Ok(())
    }

    fn write_manifest_deferred(&self, manifest: &Manifest) -> Result<()> {
        let path = turn_dir(&self.dir, manifest.turn).join("manifest.json");
        self.write_deferred(&path, &serde_json::to_vec(manifest)?)
    }

    /// A failed background flush cannot block restoring readable records; it is reported.
    fn drain_into(&self, report: &mut RollbackReport) {
        if let Err(error) = self.flusher.drain() {
            report.warnings.push(format!("{error:#}"));
        }
    }

    /// Checkpointed turns, newest first.
    pub fn list(&self) -> Vec<CheckpointInfo> {
        let _inner = self.lock();
        let mut out: Vec<CheckpointInfo> = disk_turns(&self.dir)
            .into_iter()
            .rev()
            .filter_map(|t| read_manifest(&self.dir, t).ok().filter(|m| !m.restored))
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
        self.drain_into(&mut report);
        let mut manifests: Vec<Manifest> = Vec::new();
        for t in disk_turns(&self.dir).into_iter().rev() {
            if manifests.len() == turns {
                break;
            }
            match read_manifest(&self.dir, t) {
                Ok(m) if !m.restored && (!m.entries.is_empty() || !m.skipped.is_empty()) => {
                    manifests.push(m)
                }
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
        self.restore_manifests(&mut inner, &manifests, report, RestoreMode::Consume)
    }

    /// Restore exactly the checkpoint turns linked to a selected conversation range.
    /// Missing links and intervening file-changing turns fail before touching any file.
    pub fn rollback_conversation(&self, session: &str, turns: &[u64]) -> Result<RollbackReport> {
        let mut report = RollbackReport::default();
        self.drain_into(&mut report);
        let mut inner = self.lock();
        ensure!(
            turns.windows(2).all(|pair| pair[0] < pair[1]),
            "checkpoint links must be chronological"
        );
        let mut manifests = Vec::new();
        for &turn in turns.iter().rev() {
            let manifest = read_manifest(&self.dir, turn).with_context(|| {
                format!("turn {turn}: linked checkpoint unavailable; conversation kept")
            })?;
            ensure!(
                manifest.session.as_deref() == Some(session),
                "turn {turn}: checkpoint has no matching session link; conversation kept"
            );
            manifests.push(manifest);
        }
        if let Some(oldest) = turns.iter().min() {
            for turn in disk_turns(&self.dir)
                .into_iter()
                .filter(|turn| turn >= oldest)
            {
                let manifest = read_manifest(&self.dir, turn)?;
                ensure!(
                    turns.contains(&turn)
                        || manifest.restored
                        || (manifest.entries.is_empty() && manifest.skipped.is_empty()),
                    "turn {turn}: unrelated newer file checkpoint blocks undo; conversation kept"
                );
            }
        }
        self.restore_manifests(&mut inner, &manifests, report, RestoreMode::Conversation)
    }

    /// Remove completed restoration markers only after the conversation rewind commits.
    /// Cleanup failures leave harmless markers, never checkpoints that replay restored files.
    pub fn finalize_conversation_rollback(&self, session: &str, turns: &[u64]) -> Result<()> {
        self.flusher.drain()?;
        let _inner = self.lock();
        for &turn in turns {
            let manifest = read_manifest(&self.dir, turn)?;
            ensure!(
                manifest.session.as_deref() == Some(session) && manifest.restored,
                "turn {turn}: conversation restoration has not completed"
            );
        }
        for &turn in turns {
            discard_turn(&self.dir, turn)
                .with_context(|| format!("cleaning restored checkpoint {turn}"))?;
        }
        Ok(())
    }

    fn restore_manifests(
        &self,
        inner: &mut Inner,
        manifests: &[Manifest],
        mut report: RollbackReport,
        mode: RestoreMode,
    ) -> Result<RollbackReport> {
        for manifest in manifests {
            if manifest.restored {
                continue;
            }
            let mut failed = !manifest.skipped.is_empty();
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
            // Older checkpoints are never reached past a failed newer one: a retry would replay
            // this manifest's saved content after an older manifest had already been restored.
            if failed {
                report.warnings.push(format!(
                    "turn {}: rollback stopped; older checkpoints were not restored",
                    manifest.turn
                ));
                break;
            }
            if matches!(mode, RestoreMode::Conversation) {
                let mut completed = manifest.clone();
                completed.restored = true;
                if let Err(error) = write_manifest(&self.dir, &completed) {
                    report.warnings.push(format!("turn {}: restoration progress could not be saved; rollback stopped: {error:#}", manifest.turn));
                    break;
                }
                continue;
            }
            if let Err(e) = discard_turn(&self.dir, manifest.turn)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                report.warnings.push(format!(
                    "turn {}: checkpoint could not be discarded, rollback stopped: {e:#}",
                    manifest.turn
                ));
                break;
            }
        }
        inner.current = None;
        inner.journal.reset(&self.dir, None);
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
    /// paths, detached directories, and any redirection from the recorded canonical path.
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
        let (target, root, resolved_rel) = self.resolve(&base.join(rel))?;
        ensure!(
            root == entry.root,
            "checkpoint path now resolves into another workspace directory"
        );
        ensure!(
            resolved_rel == entry.rel,
            "checkpoint path now resolves to a different file; not touching it"
        );
        Ok((target, base))
    }

    /// Restore one entry. `Ok(true)` = content written back, `Ok(false)` = created file removed.
    fn restore_entry(&self, turn: u64, index: usize, entry: &Entry) -> Result<bool> {
        let (target, base) = self.target_of(entry)?;
        if entry.existed {
            let bytes = read_saved(&self.dir, turn, index, entry)?;
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
mod tests;

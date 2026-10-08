//! Per-session file checkpoints so a bad turn can be undone (`/rollback`).
//!
//! Before the first mutation of a file in a turn, the tool layer calls `before_write`; we store the
//! file's original bytes (or the fact that it did not exist) under
//! `<dir>/<turn>/{manifest.json,files/<n>}`. `rollback(n)` puts the files of the most recent `n`
//! turns back, newest first, so the oldest recorded state wins. Everything is on disk, so a rollback
//! works after dial restarts. Nothing outside the project root is ever read or written.
//!
//! Only turns that touched files are materialised: a read-only turn leaves no trace and does not
//! count toward `rollback(n)`.
use super::memory::atomic_write;
use crate::platform::fs::PermissionsExt;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use similar::{Algorithm, ChangeTag, TextDiff};
use std::{
    fs,
    path::{Component, Path, PathBuf},
    sync::Mutex,
};

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
    /// Path relative to the project root, `/`-separated.
    rel: String,
    existed: bool,
    /// Unix permission bits of the original file.
    mode: u32,
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
    warnings: Vec<String>,
}

pub struct Checkpoints {
    dir: PathBuf,
    root: PathBuf,
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
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        crate::platform::fs::private(&dir)?;
        let dir = fs::canonicalize(&dir)?;
        if dir.starts_with(&root) {
            fs::remove_dir(&dir).ok();
            bail!(
                "checkpoint directory {} must be outside the project {}",
                dir.display(),
                root.display()
            );
        }
        let next_turn = disk_turns(&dir).last().map_or(1, |t| t + 1);
        Ok(Self {
            dir,
            root,
            inner: Mutex::new(Inner {
                next_turn,
                current: None,
                warnings: Vec::new(),
            }),
        })
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
    /// Workspace-relative paths changed or skipped in the active turn; an unrelated turn id is empty.
    pub fn changed_files(&self, turn: u64) -> Vec<String> {
        let inner = self.lock();
        let Some(manifest) = inner
            .current
            .as_ref()
            .filter(|manifest| manifest.turn == turn)
        else {
            return Vec::new();
        };
        let mut paths: Vec<_> = manifest
            .entries
            .iter()
            .map(|entry| entry.rel.clone())
            .collect();
        paths.extend(manifest.skipped.iter().cloned());
        paths.sort();
        paths.dedup();
        paths
    }
    /// Render the source delta captured by this turn's first-write checkpoints.
    /// The output is bounded and reports paths that need manual inspection.
    pub fn review_diff(&self, turn: u64) -> Result<String> {
        use std::fmt::Write as _;
        const MAX_REVIEW_BYTES: usize = 24 * 1024;
        const MAX_PATH_LIST_BYTES: usize = 4 * 1024;
        const TRUNCATED: &str =
            "\n[Diff review truncated; inspect remaining changed files manually.]\n";
        let inner = self.lock();
        let manifest = inner
            .current
            .as_ref()
            .filter(|manifest| manifest.turn == turn)
            .context("no matching checkpoint turn")?;
        let base = turn_dir(&self.dir, turn);
        let mut output = String::new();
        let mut entries = manifest.entries.iter().enumerate().collect::<Vec<_>>();
        entries.sort_by(|(_, left), (_, right)| left.rel.cmp(&right.rel));
        output.push_str("Changed files in this turn:\n");
        let mut omitted_paths = 0;
        for (_, entry) in &entries {
            if output.len() + entry.rel.len() + 3 <= MAX_PATH_LIST_BYTES - 64 {
                let _ = writeln!(output, "- {}", entry.rel);
            } else {
                omitted_paths += 1;
            }
        }
        let skipped_suffix = " (checkpoint unavailable; inspect manually)";
        for path in &manifest.skipped {
            if output.len() + path.len() + skipped_suffix.len() + 3 <= MAX_PATH_LIST_BYTES - 64 {
                let _ = writeln!(output, "- {path}{skipped_suffix}");
            } else {
                omitted_paths += 1;
            }
        }
        if omitted_paths > 0 {
            let _ = writeln!(
                output,
                "[{omitted_paths} additional paths omitted from list]"
            );
        }
        for (index, entry) in entries {
            if output.len() + TRUNCATED.len() + entry.rel.len() + 12 > MAX_REVIEW_BYTES {
                output.push_str(TRUNCATED);
                break;
            }
            const PRIVATE_NOTICE: &str = "[sensitive or private path; content omitted]\n";
            if !crate::storage::experience::safe_path(&entry.rel) {
                if output.len() + PRIVATE_NOTICE.len() + TRUNCATED.len() > MAX_REVIEW_BYTES {
                    output.push_str(TRUNCATED);
                    break;
                }
                output.push_str(PRIVATE_NOTICE);
                continue;
            }
            let before = if entry.existed {
                Some(fs::read(base.join("files").join(index.to_string()))?)
            } else {
                None
            };
            let (target, _) = self.resolve(&self.root.join(&entry.rel))?;
            let after = match fs::read(&target) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            if before == after {
                continue;
            }
            let _ = writeln!(output, "\n--- {} ---", entry.rel);
            let text_before = before.as_deref().unwrap_or_default();
            let text_after = after.as_deref().unwrap_or_default();
            let (Ok(text_before), Ok(text_after)) = (
                std::str::from_utf8(text_before),
                std::str::from_utf8(text_after),
            ) else {
                const BINARY_NOTICE: &str = "[binary file change; inspect with read]\n";
                if output.len() + BINARY_NOTICE.len() + TRUNCATED.len() > MAX_REVIEW_BYTES {
                    output.push_str(TRUNCATED);
                    return Ok(output);
                }
                output.push_str(BINARY_NOTICE);
                continue;
            };
            let diff = TextDiff::configure()
                .algorithm(Algorithm::Myers)
                .timeout(std::time::Duration::from_millis(200))
                .diff_lines(text_before, text_after);
            for change in diff.iter_all_changes() {
                if change.tag() == ChangeTag::Equal {
                    continue;
                }
                let prefix = if change.tag() == ChangeTag::Delete {
                    '-'
                } else {
                    '+'
                };
                let line = change.value();
                let added_bytes = 1 + line.len() + usize::from(!line.ends_with('\n'));
                if output.len() + added_bytes + TRUNCATED.len() > MAX_REVIEW_BYTES {
                    output.push_str(TRUNCATED);
                    return Ok(output);
                }
                output.push(prefix);
                output.push_str(line);
                if !line.ends_with('\n') {
                    output.push('\n');
                }
            }
        }
        Ok(output)
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
            let (target, _) = self.resolve(&self.root.join(&entry.rel))?;
            let (after_sha256, after_ref) = match fs::metadata(&target) {
                Ok(metadata) => {
                    ensure!(
                        metadata.is_file() && metadata.len() <= MAX_FILE_BYTES,
                        "repair evidence cannot snapshot {}",
                        entry.rel
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
                path: entry.rel.clone(),
                before_sha256,
                after_sha256,
                before_ref,
                after_ref,
            });
        }
        Ok(changes)
    }

    /// Record `abs` as it is now, unless this turn already did. Must be called BEFORE the file is
    /// changed or deleted. A path outside the project root is an error (the write must not
    /// proceed); a file over `MAX_FILE_BYTES` is not recorded but only warned about, see `warnings`.
    pub fn before_write(&self, abs: &Path) -> Result<()> {
        let (resolved, rel) = self.resolve(abs)?;
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
        if manifest.entries.iter().any(|e| e.rel == rel) || manifest.skipped.contains(&rel) {
            return Ok(());
        }
        let mut manifest = manifest.clone();

        match fs::symlink_metadata(&resolved) {
            Ok(meta) if meta.is_file() => {
                if meta.len() > MAX_FILE_BYTES {
                    let warning = format!(
                        "{rel}: {} bytes is over the {MAX_FILE_BYTES} byte checkpoint limit; this change cannot be rolled back",
                        meta.len()
                    );
                    manifest.skipped.push(rel);
                    write_manifest(&self.dir, &manifest)?;
                    inner.warnings.push(warning);
                    inner.current = Some(manifest);
                    return Ok(());
                }
                let bytes =
                    fs::read(&resolved).with_context(|| format!("reading {rel} for checkpoint"))?;
                let blob = turn_dir(&self.dir, manifest.turn)
                    .join("files")
                    .join(manifest.entries.len().to_string());
                atomic_write(&blob, &bytes, 0o600)?;
                manifest.entries.push(Entry {
                    rel,
                    existed: true,
                    mode: meta.permissions().mode() & 0o7777,
                });
            }
            Ok(_) => bail!("{rel} is not a regular file; refusing to checkpoint it"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                manifest.entries.push(Entry {
                    rel,
                    existed: false,
                    mode: 0,
                });
            }
            Err(e) => return Err(e).with_context(|| format!("inspecting {rel}")),
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
                files: m.entries.len(),
            })
            .collect();
        out.retain(|c| c.files > 0);
        out
    }

    /// Notes about writes that could not be checkpointed, in the order they happened.
    pub fn warnings(&self) -> Vec<String> {
        self.lock().warnings.clone()
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
                    Ok(true) => report.restored.push(entry.rel.clone()),
                    Ok(false) => report.removed.push(entry.rel.clone()),
                    Err(e) => {
                        failed = true;
                        report
                            .warnings
                            .push(format!("turn {}: {}: {e:#}", manifest.turn, entry.rel));
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

    /// Map a path the tools hand us to (resolved absolute path, project-relative `/` path),
    /// rejecting anything that resolves outside the root. Symlinks are resolved before checking.
    fn resolve(&self, abs: &Path) -> Result<(PathBuf, String)> {
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
        let rel = resolved
            .strip_prefix(&self.root)
            .map_err(|_| anyhow::anyhow!("{} is outside the project root", abs.display()))?;
        rel.to_str().context("path is not valid UTF-8")?;
        let rel = crate::platform::shell_path(rel);
        ensure!(!rel.is_empty(), "cannot checkpoint the project root itself");
        Ok((resolved, rel))
    }

    /// Restore one entry. `Ok(true)` = content written back, `Ok(false)` = created file removed.
    fn restore_entry(&self, turn: u64, index: usize, entry: &Entry) -> Result<bool> {
        let rel = Path::new(&entry.rel);
        ensure!(
            rel.components().all(|c| matches!(c, Component::Normal(_))),
            "unsafe path in checkpoint"
        );
        let (target, _) = self.resolve(&self.root.join(rel))?;
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
            // Drop directories the turn created and left empty.
            let mut parent = target.parent();
            while let Some(dir) = parent.filter(|d| *d != self.root && d.starts_with(&self.root)) {
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
    fn changed_paths_are_relative_and_scoped_to_the_active_turn() {
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
        assert!(checkpoint.changed_files(first).is_empty());
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

    #[test]
    fn review_diff_covers_modified_created_and_deleted_files() {
        let fixture = Fixture::new();
        fixture.write("change.rs", "old line\n");
        fixture.write("remove.rs", "remove me\n");
        let checkpoint = fixture.open();
        let turn = checkpoint.begin_turn("review").unwrap();
        modify(&checkpoint, &fixture, "change.rs", "new line\n");
        modify(&checkpoint, &fixture, "create.rs", "created\n");
        checkpoint.before_write(&fixture.path("remove.rs")).unwrap();
        fs::remove_file(fixture.path("remove.rs")).unwrap();

        let review = checkpoint.review_diff(turn).unwrap();
        assert!(review.contains("- change.rs\n- create.rs\n- remove.rs\n"));
        assert!(review.contains("-old line\n+new line\n"));
        assert!(
            review.contains("--- create.rs ---\n+created\n"),
            "{review:?}"
        );
        assert!(review.contains("--- remove.rs ---\n-remove me\n"));
    }

    #[test]
    fn review_diff_never_includes_private_file_contents() {
        let fixture = Fixture::new();
        fixture.write(".env.local", "old-secret");
        let checkpoint = fixture.open();
        let turn = checkpoint.begin_turn("private edit").unwrap();
        modify(&checkpoint, &fixture, ".env.local", "new-secret");

        let review = checkpoint.review_diff(turn).unwrap();
        assert!(review.contains(".env.local"));
        assert!(review.contains("content omitted"));
        assert!(!review.contains("old-secret") && !review.contains("new-secret"));
    }

    #[test]
    fn review_diff_output_stays_within_its_byte_budget() {
        let fixture = Fixture::new();
        fixture.write("large.txt", "old\n");
        let checkpoint = fixture.open();
        let turn = checkpoint.begin_turn("large diff").unwrap();
        modify(
            &checkpoint,
            &fixture,
            "large.txt",
            &"new line\n".repeat(4000),
        );

        let review = checkpoint.review_diff(turn).unwrap();
        assert!(
            review.len() <= 24 * 1024,
            "review grew to {} bytes",
            review.len()
        );
        assert!(review.contains("[Diff review truncated;"));
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
        crate::platform::fs::set_mode(fx.path("blob.bin"), 0o755).unwrap();
        let cp = fx.open();
        cp.begin_turn("t").unwrap();
        cp.before_write(&fx.path("blob.bin")).unwrap();
        fs::write(fx.path("blob.bin"), b"x").unwrap();
        crate::platform::fs::set_mode(fx.path("blob.bin"), 0o600).unwrap();
        cp.rollback(1).unwrap();
        assert_eq!(fs::read(fx.path("blob.bin")).unwrap(), bin);
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(fx.path("blob.bin"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }

    #[test]
    #[cfg(unix)]
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
        std::os::unix::fs::symlink(outside.path().join("secret"), fx.path("link")).unwrap();
        assert!(cp.before_write(&fx.path("link")).is_err());
        std::os::unix::fs::symlink(outside.path(), fx.path("linkdir")).unwrap();
        assert!(cp.before_write(&fx.path("linkdir/new.txt")).is_err());
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
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["entries"][0]["rel"] = serde_json::json!(format!("../../..{}", target.display()));
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
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
        assert_eq!(cp.warnings().len(), 1);
        assert!(cp.warnings()[0].contains("big.bin"));
        let report = cp.rollback(1).unwrap();
        assert_eq!(report.restored, ["a.txt"]);
        assert!(report.warnings.iter().any(|w| w.contains("big.bin")));
    }

    #[test]
    fn implicit_turn_when_none_begun_and_new_rejects_dir_inside_project() {
        let fx = Fixture::new();
        fx.write("a.txt", "A0");
        let cp = fx.open();
        modify(&cp, &fx, "a.txt", "A1");
        assert_eq!(cp.list().len(), 1);
        assert!(Checkpoints::new(fx.root().join(".cp"), fx.root()).is_err());
        assert!(cp.rollback(0).is_err());
    }
}

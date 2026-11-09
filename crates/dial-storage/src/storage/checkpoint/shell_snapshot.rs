//! Opt-in pre-images of the project root, so files a shell command changes can be undone.
//!
//! Tools call `before_write` before they change a file; a shell command cannot. With
//! `shell.undo_snapshot` the agent copies the project's source files once per turn, before the
//! first shell command that may change files, and compares after each command. A file the command
//! changed, created or deleted becomes an ordinary entry of the turn's manifest, attributed to
//! [`SHELL_WRITER`], so `/undo`, `/rollback` and review handle it like a tool edit.
//!
//! Only the project root is covered. Ignored files, dependency and build directories,
//! credential-like files and symlinks are left out, as are effects outside the project.
use super::*;
use dial_core::workspace_copy::{
    DEPENDENCY_DIRS, ROOT_GENERATED_DIRS, SECRET_DIRS, is_secret_file,
};
use std::collections::{BTreeMap, BTreeSet};

/// Writer recorded for entries a shell command produced.
pub const SHELL_WRITER: &str = "shell";
/// A project with more source files than this is not snapshotted.
pub const SHELL_SNAPSHOT_MAX_FILES: usize = 20_000;
/// A project whose copied source files exceed this is not snapshotted.
pub const SHELL_SNAPSHOT_MAX_BYTES: u64 = 512 * 1024 * 1024;
const STORE: &str = "shell-snapshot";

/// Whether this turn's shell commands are compared against a pre-image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellCoverage {
    /// A pre-image of this many files exists for the turn.
    Covered { files: usize },
    /// No pre-image: shell effects stay outside checkpoints this turn. The text says why.
    Unavailable(String),
}

/// Cheap identity of a file's content and mode; a difference is confirmed by comparing bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
    mode: u32,
    /// Unix inode and status-change time, which also move when a file is replaced in place.
    identity: (u64, i64, i64),
}

impl Stamp {
    fn of(meta: &fs::Metadata) -> Self {
        #[cfg(unix)]
        let identity = {
            use std::os::unix::fs::MetadataExt;
            (meta.ino(), meta.ctime(), meta.ctime_nsec())
        };
        #[cfg(not(unix))]
        let identity = (0, 0, 0);
        Self {
            len: meta.len(),
            modified: meta.modified().ok(),
            mode: platform_fs::mode(meta) & 0o7777,
            identity,
        }
    }
}

struct Saved {
    stamp: Stamp,
    /// File name of the copy in the snapshot store; `None` for a file over `MAX_FILE_BYTES`,
    /// which is tracked by its stamp only.
    blob: Option<usize>,
}

pub(super) struct ShellSnapshot {
    turn: u64,
    /// Pre-image by project-relative path, or why none could be taken this turn.
    files: Result<BTreeMap<String, Saved>, String>,
}

impl ShellSnapshot {
    fn coverage(&self) -> ShellCoverage {
        match &self.files {
            Ok(files) => ShellCoverage::Covered { files: files.len() },
            Err(reason) => ShellCoverage::Unavailable(reason.clone()),
        }
    }
}

impl Checkpoints {
    /// Take this turn's pre-image of the project root unless it already exists. Call before a
    /// shell command that may change files. Never fails the command: a project that is too large
    /// or unreadable reports [`ShellCoverage::Unavailable`] for the rest of the turn.
    pub fn snapshot_for_shell(&self) -> ShellCoverage {
        let mut inner = self.lock();
        let Some(turn) = inner.current.as_ref().map(|manifest| manifest.turn) else {
            return ShellCoverage::Unavailable("no checkpoint turn is active".into());
        };
        if let Some(snapshot) = inner.shell.as_ref().filter(|shell| shell.turn == turn) {
            return snapshot.coverage();
        }
        let files = self.copy_sources().map_err(|error| format!("{error:#}"));
        if files.is_err() {
            self.remove_shell_store();
        }
        let snapshot = ShellSnapshot { turn, files };
        let coverage = snapshot.coverage();
        inner.shell = Some(snapshot);
        coverage
    }

    /// Record every file that differs from this turn's pre-image and is not yet part of the turn
    /// as a checkpoint entry holding its earlier content. Returns the recorded paths. Without a
    /// pre-image for the active turn this records nothing.
    pub fn record_shell_changes(&self) -> Result<Vec<String>> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let Some(mut manifest) = inner.current.clone() else {
            return Ok(Vec::new());
        };
        let Some(files) = inner
            .shell
            .as_ref()
            .filter(|shell| shell.turn == manifest.turn)
            .and_then(|shell| shell.files.as_ref().ok())
        else {
            return Ok(Vec::new());
        };
        let known: BTreeSet<String> = manifest
            .entries
            .iter()
            .filter(|entry| entry.root.is_none())
            .map(|entry| entry.rel.clone())
            .chain(manifest.skipped.iter().cloned())
            .collect();
        let store = self.dir.join(STORE);
        let mut recorded = Vec::new();
        for (rel, saved) in files {
            if known.contains(rel) {
                continue;
            }
            let path = self.root.join(rel);
            let current = fs::symlink_metadata(&path)
                .ok()
                .filter(fs::Metadata::is_file);
            if current
                .as_ref()
                .is_some_and(|meta| Stamp::of(meta) == saved.stamp)
            {
                continue;
            }
            let Some(blob) = saved.blob else {
                manifest.skipped.push(rel.clone());
                recorded.push(rel.clone());
                continue;
            };
            let before = fs::read(store.join(blob.to_string()))
                .with_context(|| format!("reading the shell pre-image of {rel}"))?;
            // A moved stamp with the same bytes and mode (a `touch`, a rewrite in place) is not
            // a change.
            if current.as_ref().is_some_and(|meta| {
                meta.len() == saved.stamp.len
                    && platform_fs::mode(meta) & 0o7777 == saved.stamp.mode
                    && fs::read(&path).is_ok_and(|now| now == before)
            }) {
                continue;
            }
            let target = turn_dir(&self.dir, manifest.turn)
                .join("files")
                .join(manifest.entries.len().to_string());
            self.write_deferred(&target, &before)?;
            manifest.entries.push(Entry {
                rel: rel.clone(),
                root: None,
                existed: true,
                mode: saved.stamp.mode,
                writers: vec![SHELL_WRITER.to_owned()],
                len: Some(before.len() as u64),
            });
            recorded.push(rel.clone());
        }
        for (rel, _) in self.source_files()? {
            if files.contains_key(&rel) || known.contains(&rel) {
                continue;
            }
            manifest.entries.push(Entry {
                rel: rel.clone(),
                root: None,
                existed: false,
                mode: 0,
                writers: vec![SHELL_WRITER.to_owned()],
                len: None,
            });
            recorded.push(rel);
        }
        if !recorded.is_empty() {
            self.write_manifest_deferred(&manifest)?;
            inner.current = Some(manifest);
        }
        Ok(recorded)
    }

    /// Forget the pre-image; the next shell command of a turn takes a fresh one.
    pub(super) fn drop_shell_snapshot(&self, inner: &mut Inner) {
        if inner.shell.take().is_some() {
            self.remove_shell_store();
        }
    }

    /// Best effort: a leftover store is replaced by the next snapshot and never read otherwise.
    pub(super) fn remove_shell_store(&self) {
        let _ = fs::remove_dir_all(self.dir.join(STORE));
    }

    fn copy_sources(&self) -> Result<BTreeMap<String, Saved>> {
        let store = self.dir.join(STORE);
        match fs::remove_dir_all(&store) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("clearing the previous shell pre-image"),
        }
        platform_fs::create_dir_owner_only(&fs::DirBuilder::new(), &store)
            .context("creating the shell pre-image store")?;
        let sources = self.source_files()?;
        ensure!(
            sources.len() <= SHELL_SNAPSHOT_MAX_FILES,
            "the project has more than {SHELL_SNAPSHOT_MAX_FILES} source files"
        );
        let mut files = BTreeMap::new();
        let mut bytes = 0u64;
        for (index, (rel, meta)) in sources.into_iter().enumerate() {
            let path = self.root.join(&rel);
            let blob = if meta.len() > MAX_FILE_BYTES {
                None
            } else {
                bytes += meta.len();
                ensure!(
                    bytes <= SHELL_SNAPSHOT_MAX_BYTES,
                    "the project's source files exceed {} MiB",
                    SHELL_SNAPSHOT_MAX_BYTES / (1024 * 1024)
                );
                platform_fs::copy_on_write(&path, &store.join(index.to_string()))
                    .with_context(|| format!("copying {rel} for the shell pre-image"))?;
                Some(index)
            };
            // Stamp after the copy, so cloning cannot make the source look changed.
            let meta = fs::symlink_metadata(&path).with_context(|| format!("inspecting {rel}"))?;
            files.insert(
                rel,
                Saved {
                    stamp: Stamp::of(&meta),
                    blob,
                },
            );
        }
        Ok(files)
    }

    /// Regular files of the project root that a pre-image covers, sorted by relative path. Stops
    /// one past `SHELL_SNAPSHOT_MAX_FILES`, so a caller can tell an oversized project.
    fn source_files(&self) -> Result<Vec<(String, fs::Metadata)>> {
        let mut builder = ignore::WalkBuilder::new(&self.root);
        builder
            .hidden(false)
            .follow_links(false)
            .require_git(false)
            .git_global(false);
        let private = self.dir.clone();
        builder.filter_entry(move |entry| {
            let name = entry.file_name().to_string_lossy();
            let name = name.as_ref();
            entry.depth() == 0
                || !(SECRET_DIRS.contains(&name)
                    || DEPENDENCY_DIRS.contains(&name)
                    || is_secret_file(name)
                    || (entry.depth() == 1 && ROOT_GENERATED_DIRS.contains(&name))
                    || entry.path().starts_with(&private))
        });
        let mut files = Vec::new();
        for entry in builder.build() {
            // An unreadable directory is left out; a shell command running as the same user
            // cannot change what this walk cannot read.
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let Some(rel) = entry
                .path()
                .strip_prefix(&self.root)
                .ok()
                .and_then(Path::to_str)
            else {
                continue;
            };
            let Ok(meta) = entry.metadata() else { continue };
            files.push((rel.to_owned(), meta));
            if files.len() > SHELL_SNAPSHOT_MAX_FILES {
                break;
            }
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(files)
    }
}

#[cfg(test)]
mod tests;

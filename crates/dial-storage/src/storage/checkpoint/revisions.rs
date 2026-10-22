//! Latest change per file within the active checkpoint turn, for the agent's `revert_edit`.
//!
//! `before_write` runs before every observed mutation, not only the first one in a turn. Each
//! call opens a revision holding the file's state at that moment unless one is already open for
//! the path, so a tool that writes a file twice in one call still yields one revision. The agent
//! settles open revisions after every tool call: a revision whose file did not change is dropped,
//! otherwise it replaces the path's previous revision and records the committed state. That
//! committed state is how a later revert detects edits made outside this journal (shell commands,
//! external editors, tools without checkpoints).
//!
//! Only the latest settled revision per path is kept; the turn's first-write manifest remains the
//! source for `turn_start`. Saved bytes live under `<turn>/revisions/`, are bounded like other
//! checkpoint files and are removed when the turn ends, rolls back or is replaced.
use super::*;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Root-relative identity used by manifest entries: `None` is the project root.
type Key = (Option<PathBuf>, String);

/// A file's observed state: missing, present with this SHA-256, or not restorable (oversized,
/// not a regular file, unreadable).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Absent,
    Present([u8; 32]),
    Unavailable,
}

#[derive(Debug)]
struct Revision {
    before: State,
    /// Saved bytes of `before` when it is `Present`.
    blob: Option<PathBuf>,
    /// Committed state; `None` while the revision is still open.
    after: Option<State>,
}

#[derive(Debug, Default)]
struct PathRevisions {
    settled: Option<Revision>,
    open: Option<Revision>,
}

#[derive(Debug, Default)]
pub(super) struct Journal {
    turn: Option<u64>,
    next_blob: u64,
    paths: BTreeMap<Key, PathRevisions>,
}

/// What `revert_edit` must commit: compare-and-swap from `current` to `content`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevertPlan {
    /// Canonical absolute target.
    pub path: PathBuf,
    /// Path as shown to users: project-relative, or absolute for an added directory.
    pub shown: String,
    /// SHA-256 the file must still have when the restore commits; `None` means it must be absent.
    pub current: Option<[u8; 32]>,
    /// Content to restore; `None` deletes the file (it did not exist before).
    pub content: Option<Vec<u8>>,
}

fn remove_blob(revision: Option<Revision>) {
    if let Some(blob) = revision.and_then(|revision| revision.blob) {
        let _ = fs::remove_file(blob);
    }
}

fn state_of(path: &Path) -> (State, Option<Vec<u8>>) {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() && meta.len() <= MAX_FILE_BYTES => match fs::read(path) {
            Ok(bytes) if bytes.len() as u64 <= MAX_FILE_BYTES => {
                (State::Present(Sha256::digest(&bytes).into()), Some(bytes))
            }
            _ => (State::Unavailable, None),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (State::Absent, None),
        _ => (State::Unavailable, None),
    }
}

impl Journal {
    /// Forget every revision and its saved bytes; the next record starts `turn`'s journal.
    pub(super) fn reset(&mut self, dir: &Path, turn: Option<u64>) {
        if let Some(old) = self.turn {
            let _ = fs::remove_dir_all(turn_dir(dir, old).join("revisions"));
        }
        *self = Journal {
            turn,
            ..Journal::default()
        };
    }
}

impl Checkpoints {
    /// Open a revision for `resolved` unless one is open. Journal failures never block the write:
    /// the revision is recorded as unrestorable instead.
    pub(super) fn record_revision(
        &self,
        inner: &mut Inner,
        resolved: &Path,
        root: Option<PathBuf>,
        rel: String,
    ) {
        let Some(turn) = inner.current.as_ref().map(|manifest| manifest.turn) else {
            return;
        };
        if inner.journal.turn != Some(turn) {
            inner.journal.reset(&self.dir, Some(turn));
        }
        let journal = &mut inner.journal;
        let slot = journal.paths.entry((root, rel)).or_default();
        if slot.open.is_some() {
            return;
        }
        let (before, bytes) = state_of(resolved);
        let blob = bytes.and_then(|bytes| {
            let blob = turn_dir(&self.dir, turn)
                .join("revisions")
                .join(journal.next_blob.to_string());
            journal.next_blob += 1;
            atomic_write(&blob, &bytes, 0o600).ok().map(|()| blob)
        });
        let before = if matches!(before, State::Present(_)) && blob.is_none() {
            State::Unavailable
        } else {
            before
        };
        slot.open = Some(Revision {
            before,
            blob,
            after: None,
        });
    }

    fn settle_locked(&self, inner: &mut Inner) {
        let root = self.root.clone();
        for ((added, rel), slot) in inner.journal.paths.iter_mut() {
            let Some(mut open) = slot.open.take() else {
                continue;
            };
            let (after, _) = state_of(&added.as_ref().unwrap_or(&root).join(rel));
            if after == open.before && after != State::Unavailable {
                remove_blob(Some(open));
                continue;
            }
            open.after = Some(after);
            remove_blob(slot.settled.replace(open));
        }
        inner.journal.paths.retain(|_, slot| slot.settled.is_some());
    }

    /// Close revisions opened since the last settle, recording what each committed. Call after
    /// every tool call that may have written through the checkpoint observer.
    pub fn settle_changes(&self) {
        let mut inner = self.lock();
        self.settle_locked(&mut inner);
    }

    /// Plan restoring `abs` to its state before the latest journaled change this turn, or before
    /// the turn's first change when `turn_start` is set. Refuses when the file has no journaled
    /// change, cannot be restored, already has that content, or changed outside the journal since
    /// its latest journaled change. Planning never writes the workspace.
    pub fn revert_plan(&self, abs: &Path, turn_start: bool) -> Result<RevertPlan> {
        let (resolved, root, rel) = self.resolve(abs)?;
        let shown = Entry {
            rel: rel.clone(),
            root: root.clone(),
            existed: false,
            mode: 0,
            writers: Vec::new(),
        }
        .display();
        let mut inner = self.lock();
        self.settle_locked(&mut inner);
        let turn = inner
            .current
            .as_ref()
            .map(|manifest| manifest.turn)
            .filter(|turn| inner.journal.turn == Some(*turn));
        let key = (root, rel);
        let Some((turn, revision)) = turn.and_then(|turn| {
            inner
                .journal
                .paths
                .get(&key)
                .and_then(|slot| slot.settled.as_ref())
                .map(|revision| (turn, revision))
        }) else {
            bail!("{shown} has no checkpointed change by Dial in this turn; nothing to revert");
        };
        let (current, _) = state_of(&resolved);
        if revision.after != Some(current) || current == State::Unavailable {
            bail!(
                "{shown} changed outside Dial's checkpoint journal since its last change (shell command, external editor or an unjournaled tool); nothing restored. Read it and edit instead."
            );
        }
        let content = if turn_start {
            let manifest = inner
                .current
                .as_ref()
                .context("no active checkpoint turn")?;
            let (index, entry) = manifest
                .entries
                .iter()
                .enumerate()
                .find(|(_, entry)| entry.root == key.0 && entry.rel == key.1)
                .with_context(|| {
                    format!("{shown} was too large to checkpoint at turn start; use /undo")
                })?;
            if entry.existed {
                Some(
                    fs::read(
                        turn_dir(&self.dir, turn)
                            .join("files")
                            .join(index.to_string()),
                    )
                    .with_context(|| format!("reading saved content of {shown}"))?,
                )
            } else {
                None
            }
        } else {
            match (revision.before, &revision.blob) {
                (State::Absent, _) => None,
                (State::Present(_), Some(blob)) => Some(
                    fs::read(blob).with_context(|| format!("reading saved content of {shown}"))?,
                ),
                _ => bail!(
                    "{shown}: the content before its last change was not saved (oversized or not a regular file); use /undo"
                ),
            }
        };
        let target = match &content {
            Some(bytes) => State::Present(Sha256::digest(bytes).into()),
            None => State::Absent,
        };
        ensure!(
            target != current,
            "{shown} already has that content; nothing to revert"
        );
        Ok(RevertPlan {
            path: resolved,
            shown,
            current: match current {
                State::Present(digest) => Some(digest),
                _ => None,
            },
            content,
        })
    }
}

#[cfg(test)]
mod tests;

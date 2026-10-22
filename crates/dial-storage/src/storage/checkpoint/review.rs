//! Grouped checkpoint review: which writer changed which file, which files overlap with other
//! writers or later checkpointed turns, and a token-checked multi-file revert (not a transaction).
//!
//! Overlap is a *risk signal*, never proof of independence or of dependency: two writers touching
//! one file, or a later turn recording the same path, means reverting may discard work that built on
//! the change. Reverting a file with such a signal needs explicit confirmation from the caller;
//! files without a signal can still be semantically coupled to other files (a renamed function and
//! its callers), which this module cannot see.
use super::*;

/// Group name for checkpoints recorded before writers were attributed.
pub const UNATTRIBUTED: &str = "unattributed";

/// One file a turn changed, with the signals review needs before offering a revert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewFile {
    pub path: String,
    /// `main`, worker labels, or [`UNATTRIBUTED`]; never empty.
    pub writers: Vec<String>,
    /// Later checkpointed turns that recorded a change to the same path.
    pub later_turns: Vec<u64>,
}

impl ReviewFile {
    /// More than one writer changed this file in the turn.
    pub fn shared(&self) -> bool {
        self.writers.len() > 1
    }

    /// Why a revert of this file may discard other work, or `None` when no signal was recorded.
    pub fn overlap(&self) -> Option<String> {
        let mut reasons = Vec::new();
        if self.shared() {
            reasons.push(format!("changed by {}", self.writers.join(" and ")));
        }
        if !self.later_turns.is_empty() {
            let turns: Vec<_> = self.later_turns.iter().map(u64::to_string).collect();
            reasons.push(format!("changed again in later turn {}", turns.join(", ")));
        }
        (!reasons.is_empty()).then(|| reasons.join("; "))
    }
}

/// Inventory of one checkpointed turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnReview {
    pub turn: u64,
    pub label: String,
    pub files: Vec<ReviewFile>,
    /// Files too large to record: they cannot be reviewed or restored here.
    pub skipped: Vec<String>,
}

impl TurnReview {
    /// Writer groups in display order (`main` first, then alphabetical) with each group's file
    /// indices. A shared file appears in every group that wrote it.
    pub fn groups(&self) -> Vec<(String, Vec<usize>)> {
        let mut groups: std::collections::BTreeMap<String, Vec<usize>> = Default::default();
        for (index, file) in self.files.iter().enumerate() {
            for writer in &file.writers {
                groups.entry(writer.clone()).or_default().push(index);
            }
        }
        let mut groups: Vec<_> = groups.into_iter().collect();
        groups.sort_by_key(|(name, _)| (name != "main", name.clone()));
        groups
    }
}

/// What a group revert may touch without extra confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupPlan {
    /// Files only this writer changed and no later turn recorded.
    pub safe: Vec<String>,
    /// Files held back for individual review, with the reason.
    pub held: Vec<(String, String)>,
}

/// A file this revert wrote: path, manifest index, entry and the hash of what was written.
type Written<'a> = (&'a str, usize, &'a Entry, Option<[u8; 32]>);

impl Checkpoints {
    /// Undo files this revert already wrote, newest first, from the revert's own checkpoint. A file
    /// is put back only if it still holds exactly what the revert wrote, so edits made since are
    /// never overwritten. Returns the paths left reverted.
    fn put_back<'a>(&self, inner: &Inner, revert: u64, done: &[Written<'a>]) -> Vec<&'a str> {
        let undo = inner.current.clone().unwrap_or_default();
        let mut left = Vec::new();
        for (written, _, entry, expected) in done.iter().rev() {
            let restored = undo
                .entries
                .iter()
                .enumerate()
                .find(|(_, e)| e.display() == *written)
                .filter(|_| self.disk_hash(entry).ok().as_ref() == Some(expected))
                .map(|(i, e)| self.restore_entry(revert, i, e));
            if !matches!(restored, Some(Ok(_))) {
                left.push(*written);
            }
        }
        left
    }

    /// SHA-256 of the file as it is on disk now; `None` when it does not exist.
    fn disk_hash(&self, entry: &Entry) -> Result<Option<[u8; 32]>> {
        use sha2::{Digest, Sha256};
        let (target, _) = self.target_of(entry)?;
        match fs::symlink_metadata(&target) {
            Ok(meta) if meta.is_file() && meta.len() <= MAX_FILE_BYTES => {
                Ok(Some(Sha256::digest(fs::read(&target)?).into()))
            }
            Ok(_) => bail!("not a regular file within the review limit"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Hash of what restoring `entry` writes: the saved bytes, or `None` for a created file.
    fn saved_hash(&self, turn: u64, index: usize, entry: &Entry) -> Result<Option<[u8; 32]>> {
        use sha2::{Digest, Sha256};
        if !entry.existed {
            return Ok(None);
        }
        let blob = turn_dir(&self.dir, turn)
            .join("files")
            .join(index.to_string());
        Ok(Some(Sha256::digest(fs::read(blob)?).into()))
    }

    fn manifest_in(&self, inner: &Inner, turn: u64) -> Result<Manifest> {
        if let Some(manifest) = inner.current.as_ref().filter(|m| m.turn == turn) {
            return Ok(manifest.clone());
        }
        read_manifest(&self.dir, turn)
    }

    /// Manifests of checkpointed turns after `turn`, oldest first.
    fn later_manifests(&self, inner: &Inner, turn: u64) -> Vec<Manifest> {
        disk_turns(&self.dir)
            .into_iter()
            .filter(|t| *t > turn)
            .filter_map(|t| self.manifest_in(inner, t).ok())
            .collect()
    }

    /// Inventory of `turn` with writer attribution and overlap signals.
    pub fn review_turn(&self, turn: u64) -> Result<TurnReview> {
        let inner = self.lock();
        let manifest = self.manifest_in(&inner, turn)?;
        let later = self.later_manifests(&inner, turn);
        let mut files: Vec<ReviewFile> = manifest
            .entries
            .iter()
            .map(|entry| {
                let path = entry.display();
                ReviewFile {
                    writers: if entry.writers.is_empty() {
                        vec![UNATTRIBUTED.to_owned()]
                    } else {
                        entry.writers.clone()
                    },
                    later_turns: later
                        .iter()
                        .filter(|m| m.entries.iter().any(|e| e.display() == path))
                        .map(|m| m.turn)
                        .collect(),
                    path,
                }
            })
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let mut skipped = manifest.skipped.clone();
        skipped.sort();
        Ok(TurnReview {
            turn,
            label: manifest.label,
            files,
            skipped,
        })
    }

    /// Split a writer's files into those without recorded overlap and those held for individual
    /// review. Held: other writers, later checkpointed changes, or private paths.
    pub fn group_plan(&self, turn: u64, owner: &str) -> Result<GroupPlan> {
        let review = self.review_turn(turn)?;
        let mut plan = GroupPlan {
            safe: Vec::new(),
            held: Vec::new(),
        };
        for file in review
            .files
            .iter()
            .filter(|f| f.writers.iter().any(|w| w == owner))
        {
            if !crate::storage::experience::safe_path(&file.path) {
                plan.held
                    .push((file.path.clone(), "private path; use /undo".into()));
            } else if let Some(reason) = file.overlap() {
                plan.held.push((file.path.clone(), reason));
            } else {
                plan.safe.push(file.path.clone());
            }
        }
        Ok(plan)
    }

    /// Revert several files of `turn`. Every `(path, token)` must match the current review token
    /// up front (nothing is written otherwise) and again immediately before that file's own write.
    /// The checkpoint lock only keeps other checkpoint recording out; it does not exclude edit
    /// tools or other programs, so a change can still land between a compare and its write.
    ///
    /// This is not a transaction. If a later file fails, files already written are put back only
    /// when they still hold exactly what this revert wrote; any that changed since are left as
    /// they are and the revert checkpoint is kept (`/undo` would overwrite them).
    ///
    /// A file with an overlap signal ([`ReviewFile::overlap`]) is refused unless
    /// `overlap_confirmed`. The revert gets its own checkpoint turn, so `/undo` reverses it.
    pub fn restore_files(
        &self,
        turn: u64,
        items: &[(String, String)],
        overlap_confirmed: bool,
    ) -> Result<Vec<String>> {
        ensure!(!items.is_empty(), "no files selected; nothing restored");
        let mut inner = self.lock();
        let manifest = self.manifest_in(&inner, turn)?;
        let later = self.later_manifests(&inner, turn);
        let mut picked: Vec<(usize, &Entry, &str, &str)> = Vec::new();
        for (path, token) in items {
            if picked.iter().any(|(_, _, p, _)| p == path) {
                continue;
            }
            ensure!(
                crate::storage::experience::safe_path(path),
                "private path cannot be reverted here"
            );
            let (index, entry) = manifest
                .entries
                .iter()
                .enumerate()
                .find(|(_, entry)| entry.display() == *path)
                .with_context(|| format!("{path} is not in this checkpoint"))?;
            let later_turns: Vec<u64> = later
                .iter()
                .filter(|m| m.entries.iter().any(|e| e.display() == *path))
                .map(|m| m.turn)
                .collect();
            let file = ReviewFile {
                path: path.clone(),
                writers: entry.writers.clone(),
                later_turns,
            };
            if let Some(reason) = file.overlap()
                && !overlap_confirmed
            {
                bail!(
                    "{path} overlaps other work ({reason}); nothing restored. Review it on its own and confirm."
                );
            }
            ensure!(
                self.review_with(&manifest, path)?.1 == *token,
                "{path} or its saved checkpoint changed after review; nothing restored"
            );
            picked.push((index, entry, path, token));
        }

        let revert = inner.next_turn;
        inner.next_turn += 1;
        inner.journal.reset(&self.dir, None);
        inner.current = Some(Manifest {
            turn: revert,
            label: one_line(&match picked.as_slice() {
                [(_, _, path, _)] => format!("Selective revert: {path}"),
                _ => format!("Selective revert: {} files of turn {turn}", picked.len()),
            }),
            ..Manifest::default()
        });
        let abandon = |inner: &mut Inner| {
            let _ = discard_turn(&self.dir, revert);
            inner.current = None;
            inner.journal.reset(&self.dir, None);
        };
        for (_, entry, _, _) in &picked {
            let prepared = self.target_of(entry).and_then(|(target, _)| {
                let (resolved, root, rel) = self.resolve(&target)?;
                self.record_first_write(&mut inner, &resolved, root.clone(), rel.clone())?;
                self.record_revision(&mut inner, &resolved, root, rel);
                Ok(())
            });
            if let Err(error) = prepared {
                abandon(&mut inner);
                return Err(error.context("could not checkpoint the revert; nothing restored"));
            }
        }
        // Each file is compared with its reviewed token immediately before its own write, and
        // each write is remembered by content hash so a later failure only puts back files that
        // still hold exactly what this revert wrote.
        let mut done: Vec<Written<'_>> = Vec::new();
        for (index, entry, path, token) in &picked {
            let current = self.review_with(&manifest, path).map(|(_, now)| now).ok();
            let failure = if current.as_deref() != Some(*token) {
                Some(anyhow::anyhow!(
                    "{path} changed after review; it was not written"
                ))
            } else {
                self.saved_hash(turn, *index, entry)
                    .and_then(|written| {
                        self.restore_entry(turn, *index, entry)
                            .map(|_| done.push((path, *index, entry, written)))
                    })
                    .err()
            };
            let Some(error) = failure else { continue };
            let left = self.put_back(&inner, revert, &done);
            if left.is_empty() {
                abandon(&mut inner);
                return Err(error.context(
                    "revert stopped; files it had already written were put back because they still held what it wrote",
                ));
            }
            // Retain the revert checkpoint: it holds the pre-revert content for /undo.
            return Err(error.context(format!(
                "revert stopped part-way. Left reverted (changed since the revert wrote them, or could not be put back): {}. The revert checkpoint is kept for /undo; review those files before using it, because /undo overwrites them.",
                left.join(", ")
            )));
        }
        Ok(done.into_iter().map(|(p, ..)| p.to_owned()).collect())
    }
}

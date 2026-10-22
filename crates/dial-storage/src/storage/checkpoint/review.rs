//! Grouped checkpoint review: which writer changed which file, which files overlap with other
//! writers or later checkpointed turns, and an all-or-nothing multi-file revert.
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

impl Checkpoints {
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

    /// Split a writer's files into those safe to revert together and those held for individual
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

    /// Revert several files of `turn` all-or-nothing. Every `(path, token)` must match the
    /// current review token; the checkpoint lock is held from verification through the writes, so
    /// no other Dial writer can record into the checkpoint in between. A failure part-way restores
    /// the files already written from the revert's own checkpoint. Edits by programs outside Dial
    /// cannot be locked out: each file is compared again immediately before it is written.
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
        for (_, _, path, token) in &picked {
            if self
                .review_with(&manifest, path)
                .map(|(_, now)| now)
                .ok()
                .as_deref()
                != Some(*token)
            {
                abandon(&mut inner);
                bail!("{path} changed during review; nothing restored");
            }
        }
        let mut done: Vec<&str> = Vec::new();
        for (index, entry, path, _) in &picked {
            match self.restore_entry(turn, *index, entry) {
                Ok(_) => done.push(path),
                Err(error) => {
                    let undo = inner.current.clone().unwrap_or_default();
                    let mut failed = Vec::new();
                    for written in &done {
                        let reverted = undo
                            .entries
                            .iter()
                            .enumerate()
                            .find(|(_, e)| e.display() == *written)
                            .map(|(i, e)| self.restore_entry(revert, i, e));
                        if !matches!(reverted, Some(Ok(_))) {
                            failed.push(*written);
                        }
                    }
                    if failed.is_empty() {
                        abandon(&mut inner);
                        return Err(error.context(format!(
                            "could not restore {path}; files already written were put back"
                        )));
                    }
                    // Keep the revert checkpoint so /undo can finish the job.
                    return Err(error.context(format!(
                        "could not restore {path}; {} could not be put back and remain reverted (see /undo)",
                        failed.join(", ")
                    )));
                }
            }
        }
        Ok(done.into_iter().map(str::to_owned).collect())
    }
}

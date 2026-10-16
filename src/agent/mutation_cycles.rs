//! Committed-content cycle detection for successful native mutations.
//!
//! Failure and no-progress guards count failed or unchanged calls. A model can still alternate
//! successful edits that keep returning a file to earlier content (inserting and removing the same
//! lines), and every success used to clear recovery state. This tracks the actual committed content
//! of controller-reported changed paths (`Execution::changed_paths`), never model text or diffs.
//!
//! A cycle never ends the turn. It breaks the loop instead: the exact edit calls that produced the
//! alternating versions are refused for the rest of the turn, while different edits, other files
//! and every other tool stay available. Returning to an earlier version is also not progress, so it
//! cannot clear other recovery restrictions.
//!
//! Policy (conservative, not a measured optimum): one return to an earlier version is a legitimate
//! revert; a version committed for the third time with other versions in between is a cycle.
//! Consecutive identical versions are one visit. History is bounded per file and per turn, so
//! arbitrarily many novel versions stay allowed without a hidden step limit.
//!
//! Coverage: only post-commit content is observed, so a file's pre-edit content counts once it is
//! committed again. Deleted, unreadable, non-UTF-8-named, directory, protected or out-of-workspace
//! targets and files over 16 MiB are skipped (see [`Workspace::file_identity`]).
use crate::tools::Workspace;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

/// Files tracked per turn; the least recently changed file is forgotten first.
const MAX_FILES: usize = 64;
/// Recent distinct consecutive versions remembered per file.
const MAX_VERSIONS: usize = 16;
/// Commits of one version, with other versions in between, that constitute a cycle.
const CYCLE_VISITS: usize = 3;
/// Refused cycle calls retained per turn; the oldest is released first.
const MAX_BLOCKED: usize = 64;

/// One committed version and the normalized identity of the call that produced it.
struct Version {
    digest: [u8; 32],
    call: String,
}

/// What one tool result's committed paths mean for recovery.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Observation {
    /// At least one changed path reached content not seen earlier in this turn (or untracked).
    pub novel: bool,
    /// A file that returned to an earlier version for the third time, and how many distinct
    /// calls are now refused because of it.
    pub cycle: Option<(PathBuf, usize)>,
}

#[derive(Default)]
pub(super) struct MutationCycles {
    files: VecDeque<(PathBuf, VecDeque<Version>)>,
    /// Normalized call identity and the canonical file whose cycle it belongs to.
    blocked: VecDeque<(String, PathBuf)>,
}

impl MutationCycles {
    /// Record the committed content of one tool result's changed paths, produced by the call with
    /// normalized identity `call`.
    pub fn observe(
        &mut self,
        workspace: &Workspace,
        changed: &[PathBuf],
        call: &str,
    ) -> Observation {
        let mut observation = Observation::default();
        let mut seen: Vec<PathBuf> = Vec::new();
        for path in changed {
            let Some((path, digest)) = workspace.file_identity(path) else {
                // Deleted or untracked targets are real changes this guard cannot judge.
                observation.novel = true;
                continue;
            };
            if seen.contains(&path) {
                continue;
            }
            seen.push(path.clone());
            let (novel, cycle) = self.record(path.clone(), digest, call);
            observation.novel |= novel;
            if let Some(blocked) = cycle
                && observation.cycle.is_none()
            {
                observation.cycle = Some((path, blocked));
            }
        }
        observation
    }

    /// The canonical file whose edit cycle refuses this exact call, if any.
    pub fn blocked(&self, call: &str) -> Option<&PathBuf> {
        self.blocked
            .iter()
            .find(|(known, _)| known == call)
            .map(|(_, path)| path)
    }

    /// Returns whether the version is new to this file and, on a cycle, how many calls it refuses.
    fn record(&mut self, path: PathBuf, digest: [u8; 32], call: &str) -> (bool, Option<usize>) {
        let mut versions = self
            .files
            .iter()
            .position(|(known, _)| *known == path)
            .and_then(|index| self.files.remove(index))
            .map(|(_, versions)| versions)
            .unwrap_or_default();
        let earlier = versions
            .iter()
            .filter(|known| known.digest == digest)
            .count();
        let mut cycle = None;
        if versions.back().map(|last| last.digest) != Some(digest) {
            versions.push_back(Version {
                digest,
                call: call.to_owned(),
            });
            if versions.len() > MAX_VERSIONS {
                versions.pop_front();
            }
            if earlier + 1 >= CYCLE_VISITS {
                cycle = Some(self.block_cycle(&path, &versions));
            }
        }
        self.files.push_back((path, versions));
        if self.files.len() > MAX_FILES {
            self.files.pop_front();
        }
        (earlier == 0, cycle)
    }

    /// Refuse every call that produced a version visited more than once: the alternating edits.
    /// Versions reached only once (setup before the loop) stay available.
    fn block_cycle(&mut self, path: &Path, versions: &VecDeque<Version>) -> usize {
        let mut calls: Vec<&str> = Vec::new();
        for version in versions {
            let visits = versions
                .iter()
                .filter(|other| other.digest == version.digest)
                .count();
            if visits > 1 && !calls.contains(&version.call.as_str()) {
                calls.push(&version.call);
            }
        }
        for call in &calls {
            if self.blocked(call).is_none() {
                self.blocked
                    .push_back(((*call).to_owned(), path.to_path_buf()));
                if self.blocked.len() > MAX_BLOCKED {
                    self.blocked.pop_front();
                }
            }
        }
        calls.len()
    }

    #[cfg(test)]
    pub fn retained(&self) -> (usize, usize, usize) {
        (
            self.files.len(),
            self.files
                .iter()
                .map(|(_, versions)| versions.len())
                .max()
                .unwrap_or(0),
            self.blocked.len(),
        )
    }
}

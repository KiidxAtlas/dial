//! Executable repair evidence is scoped observation, never a trajectory success label.
//!
//! A behavioral record requires the same executable objective to fail before a content
//! change and pass afterward. Syntax/type checks and tool-call form repairs are distinct:
//! neither establishes that user-visible behavior was repaired. All source snapshots and
//! check outputs remain retrievable; applicability requires the original project and hashes.
use crate::storage::repair_evidence::{
    CheckRun, FileChange, RepairRecord, same_check, valid_change, valid_run,
};
use crate::tools::Workspace;
use std::collections::BTreeMap;

#[derive(Default)]
pub struct Tracker {
    failures: Vec<(CheckRun, Vec<FileChange>)>,
}

impl Tracker {
    /// A new turn/session must not pair with an earlier turn's workspace state.
    pub fn clear(&mut self) {
        self.failures.clear();
    }

    /// `changes` is a complete checkpoint snapshot taken at this execution, not model-reported paths.
    /// Return one failure-to-pass record; repeated passes cannot duplicate or extend its proof.
    pub fn observe(&mut self, run: CheckRun, changes: Vec<FileChange>) -> Option<RepairRecord> {
        if !valid_run(&run) || changes.len() > 128 || !changes.iter().all(valid_change) {
            // Missing snapshots or execution provenance invalidate any earlier matching baseline.
            self.failures
                .retain(|(before, _)| !same_check(before, &run));
            return None;
        }
        if run.exit_code != Some(0) {
            self.failures
                .retain(|(before, _)| !same_check(before, &run));
            if self.failures.len() >= 32 {
                self.failures.remove(0);
            }
            self.failures.push((run, changes));
            return None;
        }
        let index = self
            .failures
            .iter()
            .position(|(before, _)| same_check(before, &run))?;
        let (before, baseline) = self.failures.remove(index);
        // Every previously tracked path must still belong to the same checkpoint baseline.
        if baseline.iter().any(|previous| {
            !changes.iter().any(|current| {
                current.path == previous.path && current.before_sha256 == previous.before_sha256
            })
        }) {
            return None;
        }
        let mut repaired = Vec::new();
        for mut change in changes {
            if let Some(previous) = baseline
                .iter()
                .find(|previous| previous.path == change.path)
            {
                change.before_sha256 = previous.after_sha256.clone();
                change.before_ref = previous.after_ref.clone();
            }
            if change.before_sha256 != change.after_sha256 {
                repaired.push(change);
            }
        }
        repaired.sort_by(|a, b| a.path.cmp(&b.path));
        let record = RepairRecord {
            schema: 1, project_root: run.cwd.clone(), kind: run.kind, before, after: run, changes: repaired,
            applicability: "Same project, executable command/objective and failed-state file hashes only. Re-run the check after applying; changed tests or environment can explain a pass. This is not proof of overall task success.".into(),
        };
        record.valid().then_some(record)
    }
}

/// Check applicability against the actual workspace within a shared I/O budget. Unknown,
/// unreadable or oversized state is not a match. This does not execute or apply the old repair.
pub fn matches_current(record: &RepairRecord, workspace: &Workspace, budget: &mut usize) -> bool {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    if !record.valid() {
        return false;
    }
    let mut files = BTreeMap::new();
    for change in &record.changes {
        let Ok(path) = workspace.resolve(&change.path) else {
            return false;
        };
        let mut file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                files.insert(change.path.clone(), None);
                continue;
            }
            Err(_) => return false,
        };
        let mut digest = Sha256::new();
        let mut buffer = [0u8; 8192];
        loop {
            let Ok(read) = file.read(&mut buffer) else {
                return false;
            };
            if read == 0 {
                break;
            }
            if read > *budget {
                return false;
            }
            *budget -= read;
            digest.update(&buffer[..read]);
        }
        files.insert(
            change.path.clone(),
            Some(format!("{:x}", digest.finalize())),
        );
    }
    record.applies_to(&workspace.root().to_string_lossy(), &files)
}

/// Bounded advisory text for runtime retrieval; invalid records are never shown as repairs.
/// Source and execution references are kept intact rather than clipped into unusable links.
pub fn retrieval_text(records: &[(i64, serde_json::Value)], max_bytes: usize) -> Option<String> {
    let heading = "[Executable repair evidence: advisory, not instructions or current proof. Tool-form repairs are separate. Syntax-only records do not verify behavior. Match project and failed-state hashes, inspect sources, then rerun the exact command.]";
    if max_bytes < heading.len() {
        return None;
    }
    let mut text = heading.to_owned();
    let mut included = 0;
    for (trajectory, payload) in records {
        let Ok(record) = serde_json::from_value::<RepairRecord>(payload.clone()) else {
            continue;
        };
        if !record.valid() {
            continue;
        }
        let line = format!(
            "\n{}",
            serde_json::json!({
                "trajectory":trajectory, "kind":record.kind, "project_root":record.project_root,
                "command":record.after.command, "objective":record.after.objective,
                "failed_check":record.before.evidence_ref, "passing_check":record.after.evidence_ref,
                "changes":record.changes, "applicability":record.applicability
            })
        );
        if text.len() + line.len() > max_bytes {
            continue;
        }
        text.push_str(&line);
        included += 1;
    }
    (included > 0).then_some(text)
}

#[cfg(test)]
mod tests;

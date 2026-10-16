//! Executable repair evidence is scoped observation, never a trajectory success label.
//!
//! A behavioral record requires the same executable objective to fail before a content
//! change and pass afterward. Syntax/type checks and tool-call form repairs are distinct:
//! neither establishes that user-visible behavior was repaired. All source snapshots and
//! check outputs remain retrievable; applicability requires the original project and hashes.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Component, Path};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    Behavioral,
    SyntaxOnly,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileChange {
    pub path: String,
    pub before_sha256: Option<String>,
    pub after_sha256: Option<String>,
    pub before_ref: Option<String>,
    pub after_ref: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckRun {
    pub command: String,
    pub cwd: String,
    pub objective: String,
    pub kind: CheckKind,
    pub tool_call_id: String,
    pub evidence_ref: String,
    /// Typed exit status only for a classified pass/code failure; unknown runs invalidate baselines.
    pub exit_code: Option<i32>,
    /// Strictly increasing execution sequence within the current checkpoint turn.
    pub sequence: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepairRecord {
    pub schema: u32,
    pub project_root: String,
    pub kind: CheckKind,
    pub before: CheckRun,
    pub after: CheckRun,
    pub changes: Vec<FileChange>,
    pub applicability: String,
}

fn valid_digest(digest: &str) -> bool {
    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_change(change: &FileChange) -> bool {
    !change.path.is_empty()
        && Path::new(&change.path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        && [&change.before_sha256, &change.after_sha256]
            .iter()
            .all(|hash| hash.as_deref().is_none_or(valid_digest))
        && change.before_sha256.is_some()
            == change.before_ref.as_ref().is_some_and(|s| !s.is_empty())
        && change.after_sha256.is_some() == change.after_ref.as_ref().is_some_and(|s| !s.is_empty())
}

fn valid_run(run: &CheckRun) -> bool {
    !run.command.trim().is_empty()
        && run.command.len() <= 16 * 1024
        && Path::new(&run.cwd).is_absolute()
        && !run.objective.trim().is_empty()
        && run.objective.len() <= 4096
        && !run.tool_call_id.is_empty()
        && !run.evidence_ref.is_empty()
        && run.exit_code.is_some_and(|code| (0..124).contains(&code))
}

impl RepairRecord {
    /// Validate persisted evidence without trusting a model-provided "verified" flag.
    pub fn valid(&self) -> bool {
        self.schema == 1
            && self.project_root == self.before.cwd
            && self.kind == self.before.kind
            && valid_run(&self.before)
            && valid_run(&self.after)
            && same_check(&self.before, &self.after)
            && self.before.exit_code.is_some_and(|code| code > 0)
            && self.after.exit_code == Some(0)
            && self.before.sequence < self.after.sequence
            && self.before.tool_call_id != self.after.tool_call_id
            && !self.changes.is_empty()
            && self.changes.len() <= 128
            && self
                .changes
                .iter()
                .all(|change| valid_change(change) && change.before_sha256 != change.after_sha256)
            && self
                .changes
                .iter()
                .map(|c| &c.path)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == self.changes.len()
    }

    /// A retrieved repair is advice only. Exact failed-state hashes are its applicability boundary.
    pub fn applies_to(&self, project_root: &str, files: &BTreeMap<String, Option<String>>) -> bool {
        self.valid()
            && self.project_root == project_root
            && self
                .changes
                .iter()
                .all(|change| files.get(&change.path) == Some(&change.before_sha256))
    }

    /// Check applicability against the actual workspace within a shared I/O budget. Unknown,
    /// unreadable or oversized state is not a match. This does not execute or apply the old repair.
    pub fn matches_current(&self, workspace: &crate::tools::Workspace, budget: &mut usize) -> bool {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        if !self.valid() {
            return false;
        }
        let mut files = BTreeMap::new();
        for change in &self.changes {
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
        self.applies_to(&workspace.root().to_string_lossy(), &files)
    }
}

fn same_check(a: &CheckRun, b: &CheckRun) -> bool {
    a.command == b.command && a.cwd == b.cwd && a.objective == b.objective && a.kind == b.kind
}

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
mod tests {
    use super::*;

    fn run(sequence: u64, code: i32, kind: CheckKind) -> CheckRun {
        CheckRun {
            command: "cargo test regression".into(),
            cwd: "/workspace".into(),
            objective: "reject duplicate keys".into(),
            kind,
            tool_call_id: format!("check-{sequence}"),
            evidence_ref: format!("artifact://check-{sequence}"),
            exit_code: Some(code),
            sequence,
        }
    }
    fn change(before: char, after: char) -> FileChange {
        FileChange {
            path: "src/lib.rs".into(),
            before_sha256: Some(before.to_string().repeat(64)),
            after_sha256: Some(after.to_string().repeat(64)),
            before_ref: Some("/state/before".into()),
            after_ref: Some("/state/after".into()),
        }
    }

    #[test]
    fn records_reproducible_failure_change_pass_and_exact_applicability() {
        let mut tracker = Tracker::default();
        assert!(
            tracker
                .observe(run(1, 1, CheckKind::Behavioral), vec![])
                .is_none()
        );
        let record = tracker
            .observe(run(2, 0, CheckKind::Behavioral), vec![change('a', 'b')])
            .unwrap();
        assert!(record.valid());
        assert!(record.applies_to(
            "/workspace",
            &BTreeMap::from([("src/lib.rs".into(), Some("a".repeat(64)))])
        ));
        assert!(!record.applies_to(
            "/other",
            &BTreeMap::from([("src/lib.rs".into(), Some("a".repeat(64)))])
        ));
        assert!(!record.applies_to(
            "/workspace",
            &BTreeMap::from([("src/lib.rs".into(), Some("b".repeat(64)))])
        ));
        assert!(
            tracker
                .observe(run(3, 0, CheckKind::Behavioral), vec![change('a', 'b')])
                .is_none()
        );
    }

    #[test]
    fn applicability_checks_current_bytes_and_refuses_exhausted_budget() {
        use sha2::{Digest, Sha256};
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "broken").unwrap();
        let workspace = crate::tools::Workspace::new(dir.path()).unwrap();
        let mut tracker = Tracker::default();
        tracker.observe(run(1, 1, CheckKind::Behavioral), vec![]);
        let mut record = tracker
            .observe(run(2, 0, CheckKind::Behavioral), vec![change('a', 'b')])
            .unwrap();
        record.project_root = workspace.root().to_string_lossy().into_owned();
        record.before.cwd = record.project_root.clone();
        record.after.cwd = record.project_root.clone();
        record.changes[0].before_sha256 = Some(format!("{:x}", Sha256::digest(b"broken")));
        assert!(record.matches_current(&workspace, &mut 100));
        assert!(!record.matches_current(&workspace, &mut 2));
        std::fs::write(dir.path().join("src/lib.rs"), "different").unwrap();
        assert!(!record.matches_current(&workspace, &mut 100));
    }

    #[test]
    fn syntax_proof_stays_syntax_and_changed_objective_cannot_pair() {
        let mut tracker = Tracker::default();
        tracker.observe(run(1, 1, CheckKind::SyntaxOnly), vec![]);
        assert!(
            tracker
                .observe(run(2, 0, CheckKind::Behavioral), vec![change('a', 'b')])
                .is_none()
        );
        let record = tracker
            .observe(run(3, 0, CheckKind::SyntaxOnly), vec![change('a', 'b')])
            .unwrap();
        assert_eq!(record.kind, CheckKind::SyntaxOnly);
        tracker.observe(run(4, 1, CheckKind::Behavioral), vec![]);
        let mut other = run(5, 0, CheckKind::Behavioral);
        other.command = "cargo test unrelated".into();
        assert!(tracker.observe(other, vec![change('a', 'b')]).is_none());
    }

    #[test]
    fn rejects_flaky_pass_timeout_and_missing_evidence() {
        let mut tracker = Tracker::default();
        tracker.observe(run(1, 1, CheckKind::Behavioral), vec![change('a', 'b')]);
        assert!(
            tracker
                .observe(run(2, 0, CheckKind::Behavioral), vec![change('a', 'b')])
                .is_none()
        );
        tracker.observe(run(3, 124, CheckKind::Behavioral), vec![]);
        assert!(
            tracker
                .observe(run(4, 0, CheckKind::Behavioral), vec![change('a', 'b')])
                .is_none()
        );
        tracker.observe(run(5, 1, CheckKind::Behavioral), vec![change('a', 'b')]);
        let mut pass = run(6, 0, CheckKind::Behavioral);
        pass.evidence_ref.clear();
        assert!(tracker.observe(pass, vec![change('a', 'c')]).is_none());
    }

    #[test]
    fn unknown_run_invalidates_an_earlier_failure_baseline() {
        let mut tracker = Tracker::default();
        tracker.observe(run(1, 1, CheckKind::Behavioral), vec![change('a', 'b')]);
        let mut unknown = run(2, 0, CheckKind::Behavioral);
        unknown.exit_code = None;
        assert!(tracker.observe(unknown, vec![change('a', 'c')]).is_none());
        assert!(
            tracker
                .observe(run(3, 0, CheckKind::Behavioral), vec![change('a', 'd')])
                .is_none()
        );
    }

    #[test]
    fn failure_snapshot_not_turn_start_is_the_precondition() {
        let mut tracker = Tracker::default();
        tracker.observe(run(1, 1, CheckKind::Behavioral), vec![change('a', 'b')]);
        let record = tracker
            .observe(run(2, 0, CheckKind::Behavioral), vec![change('a', 'c')])
            .unwrap();
        assert_eq!(record.changes[0].before_sha256, Some("b".repeat(64)));
        assert_eq!(
            record.changes[0].before_ref.as_deref(),
            Some("/state/after")
        );
    }

    #[test]
    fn incomplete_checkpoint_cannot_silently_drop_failure_paths() {
        let mut tracker = Tracker::default();
        tracker.observe(run(1, 1, CheckKind::Behavioral), vec![change('a', 'b')]);
        let mut unrelated = change('a', 'c');
        unrelated.path = "src/other.rs".into();
        assert!(
            tracker
                .observe(run(2, 0, CheckKind::Behavioral), vec![unrelated])
                .is_none()
        );
    }

    #[test]
    fn retrieval_keeps_complete_provenance_within_budget_and_rejects_forged_pass() {
        let mut tracker = Tracker::default();
        tracker.observe(run(1, 1, CheckKind::SyntaxOnly), vec![]);
        let record = tracker
            .observe(run(2, 0, CheckKind::SyntaxOnly), vec![change('a', 'b')])
            .unwrap();
        let payload = serde_json::to_value(record).unwrap();
        let text = retrieval_text(&[(7, payload.clone())], 4096).unwrap();
        assert!(text.len() <= 4096);
        assert!(text.contains("syntax_only"));
        assert!(text.contains("artifact://check-1"));
        assert!(text.contains("/state/before"));
        assert!(retrieval_text(&[(7, payload.clone())], text.len() - 1).is_none());
        let mut forged = payload;
        forged["before"]["exit_code"] = serde_json::json!(0);
        assert!(retrieval_text(&[(7, forged)], 4096).is_none());
    }
}

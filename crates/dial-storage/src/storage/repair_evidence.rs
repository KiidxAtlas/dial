//! Executable repair evidence records: the persisted, self-validating shape of a behavioral or
//! syntax check pair with the file snapshots it covers. Storage keeps and re-validates them;
//! the agent decides when to record one and how to retrieve it.
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

pub fn valid_change(change: &FileChange) -> bool {
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

pub fn valid_run(run: &CheckRun) -> bool {
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
}

pub fn same_check(a: &CheckRun, b: &CheckRun) -> bool {
    a.command == b.command && a.cwd == b.cwd && a.objective == b.objective && a.kind == b.kind
}

//! Turn-local observations from Eval's host check callbacks. A source match identifies only
//! the named file's bytes, not the condition's dependencies or any configured requirement.
use crate::runtime::{execution::ObservationKind, tool_output::ToolOutput};
use crate::tools::{Workspace, exec::eval};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};

const LIMIT: usize = 32;

struct Receipt {
    runtime: String,
    name: String,
    passed: bool,
    path: String,
    sha256: String,
}

impl Receipt {
    fn from_check(check: &Value, passed: bool) -> Option<Self> {
        let runtime = check["runtime"].as_str()?;
        let name = check["name"].as_str()?;
        let path = check["source"]["path"].as_str()?;
        let sha256 = check["source"]["sha256"].as_str()?;
        if !matches!(runtime, "python" | "node" | "quickjs")
            || name.trim().is_empty()
            || name.len() > 200
            || name.chars().any(char::is_control)
            || !eval::is_check_source_path(path)
            || !crate::storage::image_refs::is_sha256_hex(sha256)
        {
            return None;
        }
        Some(Self {
            runtime: runtime.into(),
            name: name.into(),
            passed,
            path: path.into(),
            sha256: sha256.into(),
        })
    }
}

#[derive(Default)]
pub(in crate::agent) struct History {
    recent: VecDeque<Receipt>,
    omitted: usize,
    /// Trailing source-bound receipts from one successful cell since the latest launch failure.
    recovery_receipts: usize,
}

impl History {
    pub(in crate::agent) fn forget_recovery(&mut self) {
        self.recovery_receipts = 0;
    }

    /// Only called for the Eval tool. A completed protocol reply can carry an exception:
    /// callbacks that executed before it (including a failing assertion) still happened.
    pub(in crate::agent) fn observe(&mut self, args: &Value, output: &ToolOutput) -> (u64, u64) {
        let completed = output
            .execution
            .observation
            .as_ref()
            .is_some_and(|wait| wait.kind == ObservationKind::Wait && wait.satisfied);
        if !(completed && (args["action"].is_null() || args["action"] == "eval")) {
            return (0, 0);
        }
        let mut bound = 0;
        let mut counts = (0, 0);
        for check in output
            .data
            .as_ref()
            .and_then(|data| data["checks"].as_array())
            .into_iter()
            .flatten()
        {
            let Some(passed) = check["passed"].as_bool() else {
                continue;
            };
            // Two-argument checks still count as runtime callbacks. Only host-bound source
            // receipts have a file identity to revalidate and surface in verification reports.
            if check.get("source").is_some() {
                let Some(receipt) = Receipt::from_check(check, passed) else {
                    continue;
                };
                if self.recent.len() == LIMIT {
                    self.recent.pop_front();
                    self.omitted += 1;
                }
                self.recent.push_back(receipt);
                bound += 1;
            }
            if passed {
                counts.0 += 1;
            } else {
                counts.1 += 1;
            }
        }
        if output.is_error || counts.1 != 0 || bound > LIMIT {
            self.forget_recovery();
        } else if counts.0 != 0 {
            self.recovery_receipts = bound;
        }
        counts
    }

    /// Artifact-only fallback for an unavailable shell check, never general verification.
    /// Every changed path must have been asserted on its current bytes in the same clean cell.
    pub(in crate::agent) fn covers_artifacts(
        &self,
        workspace: &Workspace,
        paths: &[String],
    ) -> bool {
        if paths.is_empty() || paths.len() > self.recovery_receipts {
            return false;
        }
        paths.iter().all(|path| {
            let mut receipts = self
                .recent
                .iter()
                .rev()
                .take(self.recovery_receipts)
                .filter(|r| &r.path == path)
                .peekable();
            receipts.peek().is_some()
                && eval::check_source_digest(workspace, path)
                    .is_some_and(|sha| receipts.all(|r| r.passed && r.sha256 == sha))
        })
    }

    /// Some source's most recent Eval receipt passed and still matches that file's current bytes.
    pub(in crate::agent) fn current_pass(&self, workspace: &Workspace) -> bool {
        let mut seen = std::collections::BTreeSet::new();
        self.recent.iter().rev().any(|r| {
            seen.insert(r.path.as_str())
                && r.passed
                && eval::check_source_digest(workspace, &r.path).is_some_and(|sha| sha == r.sha256)
        })
    }

    /// Called to render a report; read each distinct source at most once per report.
    pub(in crate::agent) fn snapshot(&self, workspace: &Workspace) -> Value {
        if self.recent.is_empty() {
            return Value::Null;
        }
        let mut current = BTreeMap::new();
        let receipts: Vec<_> = self
            .recent
            .iter()
            .map(|r| {
                let digest = current
                    .entry(r.path.as_str())
                    .or_insert_with(|| eval::check_source_digest(workspace, &r.path));
                let freshness = match digest.as_deref() {
                    Some(now) if now == r.sha256 => "current",
                    Some(_) => "stale",
                    None => "unavailable",
                };
                json!({"runtime":r.runtime,"name":r.name,"passed":r.passed,
                "source":{"path":r.path,"sha256":r.sha256},"freshness":freshness})
            })
            .collect();
        json!({"receipts":receipts,"omitted":self.omitted,"scope":"artifact_local_only",
            "note":"Freshness compares only the named file's bytes at report time. These Eval observations do not verify commands, other inputs or task requirements."})
    }
}

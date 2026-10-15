//! Turn-local enforcement of corrective feedback. Notices alone are not a control plane.
use crate::llm::ToolCall;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Default)]
pub(super) struct RecoveryCheckpoint {
    withheld: BTreeMap<String, String>,
    cached_observations: BTreeMap<String, u64>,
    source_versions: BTreeMap<String, [u8; 32]>,
}

fn control(name: &str) -> bool {
    matches!(
        name,
        "todo" | "finish" | "working_state" | "tool_search" | "tool_load" | "artifact_read"
    )
}

impl RecoveryCheckpoint {
    pub fn observe_at(
        &mut self,
        ws: &crate::tools::Workspace,
        call: &ToolCall,
        output: &crate::tools::ToolOutput,
        investigation: &mut super::investigation::InvestigationGuard,
        repeats: &mut super::guard::RepeatGuard,
    ) -> (Option<String>, Option<String>, Option<String>) {
        let result = self.observe(call, output, investigation, repeats);
        let key = Self::key(call);
        if call.name == "read"
            && !output.is_error
            && self.withheld.contains_key(&key)
            && let Some(version) = call.arguments["path"]
                .as_str()
                .and_then(|path| ws.read_identity(path))
        {
            self.source_versions.insert(key, version);
        }
        result
    }

    pub fn rejection_at(
        &mut self,
        ws: &crate::tools::Workspace,
        call: &ToolCall,
    ) -> Option<String> {
        let key = Self::key(call);
        if let Some(previous) = self.source_versions.get(&key)
            && call.arguments["path"]
                .as_str()
                .and_then(|path| ws.read_identity(path))
                .as_ref()
                != Some(previous)
        {
            self.withheld.remove(&key);
            self.cached_observations.remove(&key);
            self.source_versions.remove(&key);
        }
        self.rejection(call)
    }
    /// Shared main/child decision: normalize once, preserve observation history,
    /// withhold only this call, and leave corrected arguments eligible.
    pub fn observe(
        &mut self,
        call: &ToolCall,
        output: &crate::tools::ToolOutput,
        investigation: &mut super::investigation::InvestigationGuard,
        repeats: &mut super::guard::RepeatGuard,
    ) -> (Option<String>, Option<String>, Option<String>) {
        let key = Self::key(call);
        let recovery_error = output
            .text
            .strip_prefix("invalid tool call: ")
            .unwrap_or(&output.text);
        let duplicate = if output.is_error
            && recovery_error.starts_with("Recovery checkpoint: this")
        {
            self.cached_observations
                .get(&key)
                .and_then(|key| investigation.repeat_known(*key))
                .map(|advice| format!("This call was not executed. Reuse the earlier evidence; corrected arguments and changed sources remain available.\n{advice}"))
        } else {
            investigation.observe_output(call, output)
        };
        if duplicate.is_some() && !output.is_error {
            self.cached_observations.insert(
                key,
                super::investigation::observation_key(call, &output.text),
            );
        }
        let repeat = if duplicate.is_some() {
            None
        } else {
            repeats.observe(call, output.is_error)
        };
        // Withhold the exact repeated observation consistently. Corrected ranges, other files,
        // and changed sources remain available through their own call/source identities.
        for advice in [duplicate.as_deref(), repeat.as_deref()]
            .into_iter()
            .flatten()
        {
            self.arm(call, advice);
        }
        let stop = investigation
            .stop_message()
            .or_else(|| repeats.stop_message(call));
        (duplicate, repeat, stop)
    }

    pub fn arm(&mut self, call: &ToolCall, advice: &str) {
        if !control(&call.name) {
            if self.withheld.len() >= 32
                && let Some((key, _)) = self.withheld.pop_first()
            {
                self.cached_observations.remove(&key);
                self.source_versions.remove(&key);
            }
            self.withheld.insert(
                Self::key(call),
                format!(
                    "`{}`: {}",
                    call.name,
                    advice.chars().take(700).collect::<String>()
                ),
            );
        }
    }

    fn key(call: &ToolCall) -> String {
        format!(
            "{}\0{}",
            call.name,
            super::guard::normalized_arguments(call)
        )
    }

    pub fn rejection(&self, call: &ToolCall) -> Option<String> {
        self.withheld.get(&Self::key(call)).map(|reason| format!(
            "Recovery checkpoint: this `{}` call is temporarily withheld after repeated unsuccessful or unchanged actions. This call was not executed. Other arguments and narrower observations remain available; repair the known problem, reuse existing evidence, or report a blocker. Previous feedback: {reason}", call.name
        ))
    }

    pub fn apply(
        &self,
        _schemas: &mut Vec<Value>,
        _names: &mut Vec<String>,
        messages: &mut Vec<Value>,
    ) {
        if self.withheld.is_empty() {
            return;
        }
        // Keep tools discoverable: only the unsuccessful call is withheld, not
        // legitimate corrected arguments, another file, or a narrower check.
        let reasons = self
            .withheld
            .values()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        messages.push(json!({"role":"user","dial_transient":true,"content":format!(
            "[Recovery checkpoint from Dial]\n{reasons}\nDo not repeat the unchanged calls. The tools remain available with corrected arguments or for genuinely different evidence. Resume the existing task; do not create a new plan or edit merely to restore tool access. If no useful action is available, call finish with a specific blocker or question."
        )}));
    }

    pub fn progress(&mut self, call: &ToolCall, success: bool, todo_changed: bool, mutated: bool) {
        let task_resolved = call.name == "todo"
            && todo_changed
            && matches!(call.arguments["op"].as_str(), Some("done" | "block"));
        if success && (mutated || task_resolved) {
            self.withheld.clear();
            self.cached_observations.clear();
            self.source_versions.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "test".into(),
            name: name.into(),
            arguments: json!({}),
            malformed_arguments: false,
        }
    }
    #[test]
    fn corrected_calls_keep_the_tool_schema_and_normalized_repeats_stay_withheld() {
        let mut checkpoint = RecoveryCheckpoint::default();
        let mut failed = call("bash");
        failed.arguments = json!({"command":"cargo check", "timeout":10});
        checkpoint.arm(&failed, "failed check");
        let mut retry = failed.clone();
        retry.arguments = json!({"command":"cargo   'check'", "timeout":20});
        assert!(checkpoint.rejection(&retry).is_some());
        retry.arguments = json!({"command":"cargo test"});
        assert!(checkpoint.rejection(&retry).is_none());
        let mut schemas = vec![json!({"function":{"name":"bash"}})];
        let mut names = vec!["bash".into()];
        let mut messages = vec![];
        checkpoint.apply(&mut schemas, &mut names, &mut messages);
        assert_eq!(schemas.len(), 1);
        assert_eq!(names, ["bash"]);
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn externally_changed_source_releases_only_its_read_feedback() {
        let dir = tempfile::tempdir().unwrap();
        let ws = crate::tools::Workspace::new(dir.path()).unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "original").unwrap();
        let mut checkpoint = RecoveryCheckpoint::default();
        let mut read = call("read");
        read.arguments = json!({"path":"a.txt:1-1"});
        let mut investigation = super::super::investigation::InvestigationGuard::default();
        let mut repeats = super::super::guard::RepeatGuard::default();
        checkpoint.arm(&read, "unchanged evidence");
        checkpoint.arm(&call("bash"), "failed command");
        checkpoint.observe_at(
            &ws,
            &read,
            &crate::tools::ToolOutput {
                execution: Default::default(),
                data: None,
                text: "original".into(),
                is_error: false,
            },
            &mut investigation,
            &mut repeats,
        );
        assert!(checkpoint.rejection_at(&ws, &read).is_some());
        std::fs::write(dir.path().join("unrelated.txt"), "other").unwrap();
        assert!(checkpoint.rejection_at(&ws, &read).is_some());
        std::fs::write(&path, "changed").unwrap();
        assert!(checkpoint.rejection_at(&ws, &read).is_none());
        assert!(checkpoint.rejection_at(&ws, &call("bash")).is_some());
    }

    #[test]
    fn only_meaningful_recovery_restores_tools() {
        let mut checkpoint = RecoveryCheckpoint::default();
        checkpoint.arm(&call("read"), "unchanged evidence");
        checkpoint.progress(&call("bash"), false, false, false);
        checkpoint.progress(&call("todo"), true, false, false);
        checkpoint.progress(&call("grep"), true, false, false);
        checkpoint.progress(&call("artifact_read"), true, false, false);
        checkpoint.progress(&call("edit"), true, false, false);
        let mut task = call("todo");
        task.arguments = json!({"op":"set"});
        checkpoint.progress(&task, true, true, false);
        assert!(checkpoint.rejection(&call("read")).is_some());
        let mut other = call("read");
        other.arguments = json!({"path":"another.rs"});
        assert!(checkpoint.rejection(&other).is_none());
        checkpoint.progress(&call("write"), true, false, true);
        assert!(checkpoint.rejection(&call("read")).is_none());
        checkpoint.arm(&call("edit"), "wrong arguments");
        task.arguments = json!({"op":"done"});
        checkpoint.progress(&task, true, true, false);
        assert!(checkpoint.rejection(&call("edit")).is_none());
        checkpoint.arm(&call("read"), "unchanged evidence");
        task.arguments = json!({"op":"block"});
        checkpoint.progress(&task, true, true, false);
        assert!(checkpoint.rejection(&call("read")).is_none());
        checkpoint.arm(&call("finish"), "invalid outcome");
        assert!(checkpoint.rejection(&call("finish")).is_none());
    }
}

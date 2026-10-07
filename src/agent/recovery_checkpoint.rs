//! Turn-local enforcement of corrective feedback. Notices alone are not a control plane.
use crate::llm::ToolCall;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Default)]
pub(super) struct RecoveryCheckpoint {
    withheld: BTreeMap<String, String>,
}

fn control(name: &str) -> bool {
    matches!(
        name,
        "todo" | "finish" | "working_state" | "tool_search" | "tool_load"
    )
}

impl RecoveryCheckpoint {
    pub fn arm(&mut self, name: &str, advice: &str) {
        // Preserve the ability to record a finding, correct a plan, discover another
        // tool, or report a concrete blocker. A user turn starts with a fresh checkpoint.
        if !control(name) {
            self.withheld
                .insert(name.into(), advice.chars().take(700).collect());
        }
    }

    pub fn rejection(&self, name: &str) -> Option<String> {
        self.withheld.get(name).map(|reason| format!(
            "Recovery checkpoint: `{name}` is temporarily withheld after repeated unsuccessful or unchanged actions. This call was not executed. Reuse existing evidence and choose a different tool, record a concrete investigation finding, or use finish to report a specific blocker. A successful different action restores the tool. Previous feedback: {reason}"
        ))
    }

    pub fn apply(
        &self,
        schemas: &mut Vec<Value>,
        names: &mut Vec<String>,
        messages: &mut Vec<Value>,
    ) {
        if self.withheld.is_empty() {
            return;
        }
        schemas.retain(|s| {
            !self
                .withheld
                .contains_key(s["function"]["name"].as_str().unwrap_or(""))
        });
        names.retain(|name| !self.withheld.contains_key(name));
        let reasons = self
            .withheld
            .iter()
            .map(|(name, reason)| format!("- `{name}`: {reason}"))
            .collect::<Vec<_>>()
            .join("\n");
        messages.push(json!({"role":"user","content":format!(
            "[Recovery checkpoint from Dial]\n{reasons}\nThese tools are temporarily absent from the offered tools. Do not call them or repeat the same promise. Use the evidence already available to choose a different concrete action: fix the known problem, perform a distinguishing check with an available tool, or record the investigation finding. A successful different action restores the tools; viewing a todo or another failed guess does not. If no useful action is available, call finish with a specific blocker or question."
        )}));
    }

    pub fn progress(&mut self, call: &ToolCall, success: bool, todo_changed: bool) {
        if success
            && !self.withheld.contains_key(&call.name)
            && (!control(&call.name) || (call.name == "todo" && todo_changed))
        {
            self.withheld.clear();
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
    fn only_successful_different_work_restores_tools() {
        let mut checkpoint = RecoveryCheckpoint::default();
        checkpoint.arm("read", "unchanged evidence");
        checkpoint.progress(&call("bash"), false, false);
        checkpoint.progress(&call("todo"), true, false);
        assert!(checkpoint.rejection("read").is_some());
        checkpoint.progress(&call("grep"), true, false);
        assert!(checkpoint.rejection("read").is_none());
        checkpoint.arm("edit", "wrong arguments");
        checkpoint.progress(&call("todo"), true, true);
        assert!(checkpoint.rejection("edit").is_none());
        checkpoint.arm("finish", "invalid outcome");
        assert!(checkpoint.rejection("finish").is_none());
    }
}

//! Keep rejected todo updates at the request tail until the plan actually changes.
use super::{guard, todo::TodoList};
use crate::llm::ToolCall;
use serde_json::{Value, json};

const FAILURE_LIMIT: u32 = 4;

#[derive(Default)]
pub(super) struct TodoRecovery {
    failures: u32,
    reminder: Option<String>,
    /// Arguments of the last rejected update and how many times they were resent unchanged.
    repeated: (String, u32),
}

impl TodoRecovery {
    pub fn observe(
        &mut self,
        call: &ToolCall,
        failed: bool,
        changed: bool,
        diagnostic: &str,
        todo: &TodoList,
    ) -> Option<String> {
        if call.name != "todo" {
            return None;
        }
        if !failed {
            // Polling the plan, or resending it unchanged, is not recovery.
            if changed {
                *self = Self::default();
            }
            return None;
        }
        self.failures += 1;
        let key = call.arguments.to_string();
        self.repeated = match &self.repeated {
            (last, count) if *last == key => (key, count + 1),
            _ => (key, 1),
        };
        let correction = if let Some(hint) = todo.completion_hint() {
            format!(
                "Keep the existing plan. After the work, close its active task with {hint}; no finding or evidence is required. Optional notes must be valid strings/references. Use append for new work and a current 1-based index for start. For a blocker, use block with a concrete reason."
            )
        } else if todo.is_empty() {
            "The plan is empty. For substantial work use {\"op\":\"set\",\"tasks\":[{\"content\":\"<next step>\"}]}; otherwise omit todo. done cannot close an empty plan.".to_owned()
        } else {
            "No task is active. Use a current 1-based index with start, or append necessary new tasks. done needs an active task or a valid explicit index.".to_owned()
        };
        let advice = format!(
            "[Todo recovery from Dial]\nLast rejected update: {}\n{correction}\nTask numbers are assigned automatically; index belongs at the top level for start/done/drop/block, never inside tasks. Omit index for the current task; explicit indices are 1-based.\nThis correction takes precedence over generic todo planning examples. Changing the operation or wording and calling view do not reset the failure count. Stop and report the specific blocker if you cannot correct the update; the turn stops after {FAILURE_LIMIT} unsuccessful todo updates without a plan change.",
            guard::clip_middle(diagnostic.lines().next().unwrap_or(diagnostic), 800)
        );
        self.reminder = Some(advice.clone());
        Some(advice)
    }

    pub fn apply(&self, messages: &mut Vec<Value>) {
        if let Some(reminder) = &self.reminder {
            messages.push(json!({"role":"user","dial_transient":true,"content":reminder}));
        }
    }

    pub fn stop_message(&self) -> Option<String> {
        (self.failures.max(self.repeated.1) >= FAILURE_LIMIT).then(|| format!(
            "Stopped: todo updates failed {} times without a successful plan change. Switching operations, rewording claims, reading files, or viewing the plan is not recovery. The existing tasks are preserved; correct the reported arguments before continuing.", self.failures
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(op: &str) -> ToolCall {
        ToolCall {
            id: "test".into(),
            name: "todo".into(),
            arguments: json!({"op":op}),
            malformed_arguments: false,
        }
    }

    #[test]
    fn empty_plan_recovery_does_not_recommend_done() {
        let mut recovery = TodoRecovery::default();
        let advice = recovery
            .observe(
                &call("set"),
                true,
                false,
                "investigations must start open",
                &TodoList::new(),
            )
            .unwrap();
        assert!(advice.contains("\"op\":\"set\""));
        assert!(advice.contains("otherwise omit todo"));
        assert!(!advice.contains("\"op\":\"done\""));
    }

    #[test]
    fn optional_notes_recovery_keeps_the_simple_completion_contract() {
        let mut todo = TodoList::new();
        todo.call(
            &serde_json::json!({"op":"set","tasks":[{"content":"Inspect","investigation":true}]}),
        );
        let mut recovery = TodoRecovery::default();
        let advice = recovery
            .observe(
                &call("done"),
                true,
                false,
                "finding must be a string",
                &todo,
            )
            .unwrap();
        assert!(advice.contains("{\"op\":\"done\"}"));
        assert!(advice.contains("no finding or evidence is required"));
        assert!(!advice.contains("fill placeholders"));
    }

    #[test]
    fn only_a_successful_plan_change_clears_recovery() {
        let mut recovery = TodoRecovery::default();
        let todo = TodoList::new();
        recovery.observe(&call("done"), true, false, "no active task", &todo);
        recovery.observe(&call("view"), false, false, "", &todo);
        assert!(recovery.reminder.is_some());
        recovery.observe(&call("set"), false, false, "", &todo);
        assert_eq!(recovery.failures, 1);
        recovery.observe(&call("set"), false, true, "", &todo);
        assert!(recovery.reminder.is_none());
        assert_eq!(recovery.failures, 0);
    }

    #[test]
    fn identical_rejected_updates_stop_even_across_plan_changes() {
        let mut recovery = TodoRecovery::default();
        let todo = TodoList::new();
        for _ in 0..FAILURE_LIMIT {
            recovery.observe(&call("done"), true, false, "no active task", &todo);
            // A successful plan change in between resets the failure count, not the repeat count.
            recovery.observe(&call("set"), false, true, "", &todo);
        }
        assert!(recovery.stop_message().is_none());
        for _ in 0..FAILURE_LIMIT {
            recovery.observe(&call("done"), true, false, "no active task", &todo);
        }
        assert!(recovery.stop_message().is_some());
    }
}

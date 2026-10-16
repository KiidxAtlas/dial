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
        // Repeated rejections escalate the same reminder; the turn is never ended for them.
        let attempts = self.failures.max(self.repeated.1);
        let escalation = if attempts >= FAILURE_LIMIT && attempts % FAILURE_LIMIT == 0 {
            format!(
                "\nEscalation: {attempts} todo updates have been rejected without a successful plan change. Resending or rewording them is not recovery. Send the single update the diagnostic describes with its operation and indices corrected, or block the step with a concrete reason. The turn continues."
            )
        } else {
            String::new()
        };
        let advice = format!(
            "[Todo recovery from Dial]\nLast rejected update: {}\n{correction}\nTask numbers are assigned automatically; index belongs at the top level for start/done/drop/block, never inside tasks. Omit index for the current task; explicit indices are 1-based.\nThis correction takes precedence over generic todo planning examples. Changing the operation or wording and calling view do not reset the failure count. If you cannot correct the update, block the step with a concrete reason instead of resending it.{escalation}",
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
}

/// A response whose only calls are `todo` spends a full request (the whole context re-sent)
/// on bookkeeping. Say so once per turn, in the todo result, so later updates ride along with
/// real work. Not when only `todo` was offered.
pub(super) fn solo_update_note(
    calls: &[ToolCall],
    planning_required: bool,
    already_noted: bool,
) -> Option<&'static str> {
    (!already_noted
        && !planning_required
        && !calls.is_empty()
        && calls.iter().all(|call| call.name == "todo"))
    .then_some(
        "[Dial] This response held only `todo`, which costs a full request. Send todo updates in the same response as real work: set with the first reads, done with the next step's first call. Skip closing updates before the final answer; an accepted answer closes only the in-progress task and leaves pending tasks open.",
    )
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
    fn todo_only_responses_are_noted_once_and_not_when_forced() {
        let read = ToolCall {
            id: "r".into(),
            name: "read".into(),
            arguments: json!({"path":"a.rs"}),
            malformed_arguments: false,
        };
        assert!(solo_update_note(&[call("done")], false, false).is_some());
        assert!(solo_update_note(&[call("done"), call("start")], false, false).is_some());
        assert!(solo_update_note(&[call("done"), read], false, false).is_none());
        assert!(solo_update_note(&[call("done")], false, true).is_none());
        assert!(solo_update_note(&[call("set")], true, false).is_none());
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
    fn identical_rejected_updates_escalate_even_across_plan_changes() {
        let mut recovery = TodoRecovery::default();
        let todo = TodoList::new();
        // A successful plan change resets the whole rejection history, so each run counts from zero.
        for _ in 0..FAILURE_LIMIT {
            recovery.observe(&call("done"), true, false, "no active task", &todo);
            recovery.observe(&call("set"), false, true, "", &todo);
        }
        assert_eq!(recovery.failures, 0);
        let mut last = None;
        for _ in 0..FAILURE_LIMIT {
            last = recovery.observe(&call("done"), true, false, "no active task", &todo);
        }
        assert!(last.is_some_and(|text| text.contains("Escalation")));
    }
}

//! Keep rejected todo updates at the request tail until the plan actually changes.
use crate::agent::{safety::guard, workflow::todo::TodoList};
use crate::llm::ToolCall;
use serde_json::Value;

const FAILURE_LIMIT: u32 = 4;

#[derive(Default)]
pub(in crate::agent) struct TodoRecovery {
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
        let escalation = if attempts >= FAILURE_LIMIT && attempts.is_multiple_of(FAILURE_LIMIT) {
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
            messages.push(crate::agent::run::guidance::note(
                crate::agent::run::guidance::Priority::Failure,
                reminder.as_str(),
            ));
        }
    }
}

/// A response whose only calls are `todo` spends a full request (the whole context re-sent)
/// on bookkeeping. Say so once per turn, in the todo result, so later updates ride along with
/// real work. Not when only `todo` was offered.
pub(in crate::agent) fn solo_update_note(
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
mod tests;

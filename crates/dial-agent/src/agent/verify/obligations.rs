//! Canonical snapshot of open and blocked execution obligations across `TodoList` and
//! `working_state`.
//!
//! Completion gating, rejection-outcome selection (`Blocked` vs `Incomplete`), outstanding-work
//! notices and completion evidence all read from this single snapshot so they cannot disagree.

use serde_json::Value;

use crate::agent::verify::completion::TurnOutcome;
use crate::agent::workflow::todo::{Kind, TodoList};
use crate::agent::workflow::working_state;

/// Unified view of open/blocked steps, optional follow-ups, and declared deliverables.
pub(in crate::agent) struct Obligations<'a> {
    todo: &'a TodoList,
    working: &'a Value,
    open_steps: Vec<&'a str>,
    blocked_steps: Vec<String>,
    open_follow_ups: usize,
    open_deliverables: Vec<Value>,
    blocked_deliverables: bool,
}

impl<'a> Obligations<'a> {
    pub(in crate::agent) fn collect(todo: &'a TodoList, working: &'a Value) -> Self {
        let mut open_steps = Vec::new();
        let mut open_follow_ups = 0usize;
        for task in todo.tasks() {
            if !task.status.is_open() {
                continue;
            }
            match task.kind {
                Kind::Step => open_steps.push(task.content.as_str()),
                Kind::FollowUp => open_follow_ups += 1,
            }
        }
        let blocked_steps = todo
            .blocked_steps()
            .map(|task| {
                format!(
                    "{}: {}",
                    task.content,
                    task.note
                        .as_deref()
                        .unwrap_or("No blocking reason recorded")
                )
            })
            .collect();
        let open_deliverables = working_state::open_deliverables(working);
        let blocked_deliverables = working["deliverables"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["status"] == "blocked"));
        Self {
            todo,
            working,
            open_steps,
            blocked_steps,
            open_follow_ups,
            open_deliverables,
            blocked_deliverables,
        }
    }

    pub(in crate::agent) fn has_open_steps(&self) -> bool {
        !self.open_steps.is_empty()
    }

    pub(in crate::agent) fn has_open_deliverables(&self) -> bool {
        !self.open_deliverables.is_empty()
    }

    #[allow(dead_code)]
    pub(in crate::agent) fn open_deliverables(&self) -> &[Value] {
        &self.open_deliverables
    }

    /// True when at least one execution step or declared deliverable is explicitly blocked.
    pub(in crate::agent) fn has_external_blocker(&self) -> bool {
        !self.blocked_steps.is_empty() || self.blocked_deliverables
    }

    /// Rejection outcome for an unaccepted delivery: `Blocked` when an explicit blocker is
    /// recorded on a step or deliverable, otherwise `Incomplete`.
    pub(in crate::agent) fn rejection_outcome(&self) -> TurnOutcome {
        if self.has_external_blocker() {
            TurnOutcome::Blocked
        } else {
            TurnOutcome::Incomplete
        }
    }

    /// Blocking diagnostic for blocked steps, open execution steps, or incomplete deliverables.
    pub(in crate::agent) fn completion_blocker(&self) -> Option<String> {
        if !self.blocked_steps.is_empty() {
            return Some(format!(
                "Explicitly blocked tasks remain. Resolve their prerequisites or report outcome=blocked.\n{}",
                self.blocked_steps.join("\n")
            ));
        }
        if !self.open_steps.is_empty() {
            return Some(format!(
                "Execution steps remain open: {}. Complete or explicitly block them; a final reply does not close them.",
                self.open_steps.join("; ")
            ));
        }
        working_state::completion_blocker(self.working)
    }

    /// A bounded, separate notice of remaining obligations, never a rewrite of the answer.
    pub(in crate::agent) fn outstanding_notice(&self, outcome: TurnOutcome) -> Option<String> {
        if self.open_steps.is_empty()
            && self.open_follow_ups == 0
            && self.open_deliverables.is_empty()
        {
            return None;
        }
        let mut notice = format!(
            "Outstanding work retained: {} execution step(s), {} required deliverable(s), {} optional follow-up(s).",
            self.open_steps.len(),
            self.open_deliverables.len(),
            self.open_follow_ups,
        );
        if outcome == TurnOutcome::Answered {
            notice.push_str(" This answer does not mark that work complete.");
        }
        for (index, task) in self
            .todo
            .tasks()
            .iter()
            .enumerate()
            .filter(|(_, task)| task.status.is_open())
            .take(3)
        {
            notice.push_str(&format!(
                "\n{}",
                crate::agent::base::helpers::byte_prefix(&task.label(index + 1), 300)
            ));
        }
        Some(notice)
    }
}

#[cfg(test)]
mod tests;

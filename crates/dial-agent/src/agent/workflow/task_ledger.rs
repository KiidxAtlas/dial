//! Canonical task-ledger read/write coordination over `TodoList`, `working_state`, and
//! `Obligations`.
//!
//! Keeps execution obligations, completion blockers, outstanding notices, and session control
//! synchronization behind one authoritative facade so completion gating, verification checks,
//! and projection never interpret task state through diverging paths.
use crate::agent::verify::completion::TurnOutcome;
use crate::agent::verify::obligations::Obligations;
use crate::agent::workers::task_packet::PacketCache;
use crate::agent::workflow::todo::TodoList;
use serde_json::Value;

#[cfg(test)]
mod tests;

/// Stateless coordinator over the session's durable task stores (`TodoList` + `control_state`).
pub(in crate::agent) struct TaskLedger;

impl TaskLedger {
    /// Collect the canonical obligation snapshot across `todo` and `control["working_state"]`.
    pub(in crate::agent) fn obligations<'a>(
        todo: &'a TodoList,
        control: &'a Value,
    ) -> Obligations<'a> {
        Obligations::collect(todo, &control["working_state"])
    }

    /// Side-effect-free completion blocker from open/blocked execution steps and deliverables.
    pub(in crate::agent) fn completion_blocker(todo: &TodoList, control: &Value) -> Option<String> {
        Self::obligations(todo, control).completion_blocker()
    }

    /// Outcome classification (`Blocked` vs `Incomplete`) when obligations reject completion.
    pub(in crate::agent) fn rejection_outcome(todo: &TodoList, control: &Value) -> TurnOutcome {
        Self::obligations(todo, control).rejection_outcome()
    }

    /// Bounded outstanding-work notice for non-completed turn reports.
    pub(in crate::agent) fn outstanding_notice(
        todo: &TodoList,
        working: &Value,
        outcome: TurnOutcome,
    ) -> Option<String> {
        Obligations::collect(todo, working).outstanding_notice(outcome)
    }

    /// Synchronize the persisted `todo` snapshot inside `control` and invalidate any cached
    /// task packet so the next projected request reflects the updated ledger.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(in crate::agent) fn sync_todos(
        todo: &TodoList,
        control: &mut Value,
        packet_cache: &mut PacketCache,
    ) {
        control["todo"] = todo.snapshot();
        packet_cache.clear();
    }
}

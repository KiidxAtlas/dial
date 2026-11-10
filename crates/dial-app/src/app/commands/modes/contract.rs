//! `/contract <task>`: start a turn that first turns the request into a bounded task contract.
//! The contract lives in the existing working state (`todo` state ops: goal, constraints,
//! deliverables), so the completion gates that already read deliverables apply; nothing here adds a new gate.

use crate::agent::Agent;
use crate::app::{App, Shell, Step};
use anyhow::Result;

const USAGE: &str = "Usage: /contract <task>";

pub(super) fn run(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    let task = args.trim();
    if task.is_empty() {
        shell.note("Contract", USAGE);
        return Ok(Step::Done);
    }
    shell.run_turn(app, agent, &kickoff(task))
}

/// The turn text: the user's task verbatim, then the contract procedure.
fn kickoff(task: &str) -> String {
    format!(
        "{task}\n\n\
Work this as a task contract. Before changing anything:\n\
1. Record the contract with `todo` op `state_update` (load `todo` with `tool_search` names:[\"todo\"] if it is not offered); in \
`state`, `goal` is the task in one sentence; `constraints` lists what must not change; each \
\"done when\" condition is one deliverable with status `open`, phrased as an observable check \
(a test, command or behavior), not as a claim.\n\
2. If a condition, constraint or the deliverable itself is ambiguous in a way that changes the \
work, ask before starting instead of choosing an easier version of the task.\n\
Then do the work. Mark a deliverable `satisfied` only with evidence from a check that actually \
ran and passed. If you are blocked, stop: record what you tried and what remains with \
`todo` `state_patch`, mark the deliverable `blocked`, and report it. Do not guess."
    )
}

#[cfg(test)]
mod tests;

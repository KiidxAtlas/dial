//! `/loop`: repeat a prompt after each turn, bounded by a count or duration and optionally gated by
//! a `--while` / `--until` shell condition. Engine: `agent::modes::loop_runner`.

use crate::agent::Agent;
use crate::agent::modes::loop_runner;
use crate::app::{App, Shell, Step};
use anyhow::Result;
use std::time::Instant;

pub(super) fn run(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    if shell.loop_mode.disable() {
        shell.note("Loop", "Loop mode disabled.");
        return Ok(Step::Done);
    }
    let parsed = match loop_runner::parse_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            shell.note("Loop", message);
            return Ok(Step::Done);
        }
    };
    let message = shell.loop_mode.enable(&parsed, Instant::now());
    shell.note("Loop", message);
    match parsed.prompt {
        Some(prompt) => shell.run_turn(app, agent, &prompt),
        None => Ok(Step::Done),
    }
}

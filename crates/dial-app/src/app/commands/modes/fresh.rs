//! `/fresh`: reset provider stream state without changing the local transcript.

use crate::agent::Agent;
use crate::app::{App, Shell, Step};
use anyhow::Result;

pub(super) fn run(shell: &mut Shell, app: &App, agent: &mut Agent<'_>) -> Result<Step> {
    if !shell.wait_compute(app)? {
        return Ok(Step::Done);
    }
    if agent.reset_provider_stream() {
        shell.note(
            "Fresh",
            "provider stream state reset; the transcript is unchanged and the next request starts a new chain",
        );
    } else {
        shell.note(
            "Fresh",
            "no provider backend is active yet; nothing to reset",
        );
    }
    Ok(Step::Done)
}

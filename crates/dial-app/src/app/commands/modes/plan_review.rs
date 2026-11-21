//! `/plan-review`: re-surfaces the stored plan for approval while plan mode is active.

use crate::agent::Agent;
use crate::app::{App, Bridge, Shell, Step};
use anyhow::Result;

pub(super) fn run(shell: &mut Shell, app: &App, agent: &mut Agent<'_>) -> Result<Step> {
    if !shell.wait_compute(app)? {
        return Ok(Step::Done);
    }
    let result = {
        let mut bridge = Bridge::new(app, shell);
        bridge.repaint(true);
        agent.review_plan(&mut bridge)
    };
    if let Err(error) = result {
        shell.note("Plan review", format!("{error:#}"));
    }
    Ok(Step::Done)
}

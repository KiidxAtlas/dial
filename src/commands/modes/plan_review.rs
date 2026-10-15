//! `/plan-review`: re-surfaces the stored plan for approval while plan mode is active.

use crate::agent::Agent;
use crate::app::{App, Bridge, PAINT_EVERY, Shell, Step};
use anyhow::Result;
use std::time::Instant;

pub(super) fn run(shell: &mut Shell, app: &App, agent: &mut Agent<'_>) -> Result<Step> {
    if !shell.wait_compute(app)? {
        return Ok(Step::Done);
    }
    let result = {
        let mut bridge = Bridge {
            app,
            shell: &mut *shell,
            interrupt: app.interrupt.clone(),
            started: Instant::now(),
            last_paint: Instant::now() - PAINT_EVERY,
            failed: false,
            pending: Vec::new(),
            next_id: 0,
        };
        bridge.repaint(true);
        agent.review_plan(&mut bridge)
    };
    if let Err(error) = result {
        shell.note("Plan review", format!("{error:#}"));
    }
    Ok(Step::Done)
}

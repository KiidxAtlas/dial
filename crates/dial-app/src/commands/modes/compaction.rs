//! Manual compaction run shared by `/handoff` and `/shake`. Mirrors the `/compact` arm in app.rs:
//! wait for the current compute, show the compacting phase, and drive `compact_with_options`.

use crate::agent::Agent;
use crate::app::{App, Bridge, PAINT_EVERY, Phase, Shell, Step};
use crate::config::compaction::CompactionOptions;
use anyhow::Result;
use std::time::Instant;

pub(super) fn run(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    mut options: CompactionOptions,
) -> Result<Step> {
    options.suppress_continuation = true;
    if !shell.wait_compute(app)? {
        return Ok(Step::Done);
    }
    shell.composer.set_busy(true);
    shell.status.phase = Phase::Compacting;
    shell.status.activity = "compacting".into();
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
        agent.compact_with_options(&mut bridge, options)
    };
    shell.composer.set_busy(false);
    if let Err(error) = result {
        shell.transcript.error(&format!("{error:#}"));
    }
    Ok(Step::Done)
}

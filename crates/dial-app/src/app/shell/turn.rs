//! Running one agent turn from the shell and rating the last turn.

use crate::agent::Agent;
use crate::app::bridge::Bridge;
use crate::app::core::App;
use crate::app::display::local_time;
use crate::app::drive::Step;
use crate::app::shell::teach::TEACH_OFF;
use crate::app::shell::{PAINT_EVERY, Shell};
use crate::config::types::Outcome;
use crate::platform;
use crate::ui::status::Phase;
use crate::ui::transcript::TurnStats;
use anyhow::Result;
use std::sync::atomic::Ordering;
use std::time::Instant;

impl Shell {
    /// Alt+Up / Alt+Down: rate the last finished turn for "Teach this model".
    pub(super) fn rate_last_turn(&mut self, app: &App, good: bool) {
        let Some(store) = app.store.as_ref() else {
            let message = if !app.config.improve.teach {
                TEACH_OFF.to_owned()
            } else if !app.config.teaching() {
                format!(
                    "Teach this model works with local models; {} is hosted, so this turn was not recorded.",
                    app.config.model
                )
            } else if let Some(error) = &app.store_error {
                format!("Teaching is on, but the learning store could not open: {error}")
            } else {
                TEACH_OFF.to_owned()
            };
            self.note("Teaching", message);
            return;
        };
        let Some(trajectory) = self.last_trajectory else {
            self.note("Teaching", "No finished turn to rate yet.");
            return;
        };
        let outcome = Outcome {
            trajectory,
            score: if good { 100.0 } else { 0.0 },
            source: "user:rating".into(),
            reference: None,
        };
        match store.outcome(&outcome) {
            Ok(()) => {
                self.teach_pending
                    .retain(|change| change.trajectory != trajectory);
                self.note(
                    "Teaching",
                    if good {
                        "Marked the last turn good: saved as a training example."
                    } else {
                        "Marked the last turn bad: it will not be used as an example."
                    },
                );
                self.start_live_monitor("Teaching", app.config.clone());
            }
            Err(error) => self.transcript.error(&format!("{error:#}")),
        }
    }

    pub(in crate::app) fn run_turn(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        text: &str,
    ) -> Result<Step> {
        // A joined guest's prompts belong to the host agent: send them there or refuse. The local
        // agent never runs them.
        if let Some(routed) = crate::collab::room::route_guest_prompt(text) {
            let message = match routed {
                Ok(()) => "Sent to the host agent; its replies arrive as collab entries.",
                Err(message) => message,
            };
            self.note("Collab", message);
            return Ok(Step::Done);
        }
        self.loop_mode.turn_started(text);
        self.retry_noted = false;
        platform::interrupt::clear_interrupt();
        app.interrupt.store(false, Ordering::SeqCst);
        self.stop_speech();
        self.live_tasks = Some(agent.live_task_control());
        self.background_control = agent.background_control();
        let started = Instant::now();
        self.status.elapsed_secs = 0.0;
        self.transcript.push_user(text);
        self.composer.set_busy(true);
        self.status.phase = Phase::Working;
        let ready = self.wait_compute(app).and_then(|ready| {
            if ready {
                self.prepare_local(app)
            } else {
                Ok(false)
            }
        });
        match ready {
            Ok(true) => {}
            Ok(false) => {
                self.composer.set_busy(false);
                self.loop_mode.pause();
                return Ok(Step::Done);
            }
            Err(error) => {
                self.transcript.error(&format!("{error:#}"));
                self.composer.set_busy(false);
                self.loop_mode.pause();
                return Ok(Step::Done);
            }
        }
        self.status.phase = Phase::Working;
        self.turn_output_tokens = 0;
        self.status.output_tokens = 0;
        self.status.tokens_per_second = None;
        self.status.activity = "thinking".into();
        let teach = self.teach_turn_start(app, agent);
        let (result, failed, streamed_tokens) = {
            let mut bridge = Bridge {
                app,
                shell: self,
                interrupt: app.interrupt.clone(),
                started,
                last_paint: Instant::now() - PAINT_EVERY,
                failed: false,
                pending: Vec::new(),
                next_id: 0,
            };
            bridge.repaint(true);
            let result = agent.turn(text, &mut bridge);
            (result, bridge.failed, bridge.shell.turn_output_tokens)
        };
        app.interrupt.store(false, Ordering::SeqCst);
        crate::collab::room::sync_host(agent.session());
        self.composer.set_busy(false);
        self.save_history(app);
        self.status.activity.clear();
        self.status.model_progress = None;
        self.status.model_silence_secs = 0;
        let elapsed = started.elapsed().as_secs_f64();
        self.refresh_status(app, agent);
        match &result {
            Ok(report) => {
                if report.interrupted {
                    self.note("Interrupted", "the turn stopped at the next safe point");
                }
                if report.trajectory.is_some() {
                    self.last_trajectory = report.trajectory;
                }
                if let (Some(trajectory), Some(start)) = (report.trajectory, teach) {
                    self.teach_turn_end(app, agent, trajectory, start);
                }
            }
            Err(error) => self.transcript.error(&format!("{error:#}")),
        }
        // Notify from the agent's outcome, not from transport success: failed, blocked and
        // incomplete turns are errors, and an interrupted turn is silent (see `turn_end_notice`).
        let outcome = match &result {
            Ok(report) if report.interrupted => crate::agent::TurnOutcome::Interrupted,
            Ok(report) => report.outcome,
            Err(_) => crate::agent::TurnOutcome::Failed,
        };
        if let Some(notice) =
            crate::ui::termcaps::turn_end_notice(outcome, self.ui.notify, self.ui.notify_errors)
        {
            crate::ui::termcaps::notify(notice.kind, "Dial", notice.body, &self.session_id);
        }
        self.transcript.finish_turn();
        if matches!(&result, Ok(report) if !report.interrupted) {
            self.auto_speak(app, agent);
        }
        if result.is_ok() {
            self.transcript.turn_stats(TurnStats {
                clock: local_time("%Y-%m-%d %H:%M:%S"),
                elapsed_secs: elapsed,
                prompt_tokens: self.status.prompt_tokens,
                output_tokens: (streamed_tokens > 0).then_some(streamed_tokens),
                tokens_per_second: self.status.tokens_per_second,
                outcome: None,
            });
        }
        let halted = result.as_ref().map_or(true, |report| report.interrupted);
        self.loop_mode.turn_settled(Instant::now(), halted);
        if failed {
            anyhow::bail!("terminal draw failed; the screen was restored");
        }
        Ok(Step::Done)
    }
}

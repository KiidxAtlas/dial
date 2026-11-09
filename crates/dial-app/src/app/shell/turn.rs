//! Running one agent turn from the shell and rating the last turn.

use crate::agent::Agent;
use crate::app::bridge::Bridge;
use crate::app::core::App;
use crate::app::display::local_time;
use crate::app::drive::Step;
use crate::app::shell::teach::TEACH_OFF;
use crate::app::shell::{PAINT_EVERY, Shell};
use crate::cli::watch::state::ChatRole;
use crate::config::types::Outcome;
use crate::platform;
use crate::ui::status::Phase;
use crate::ui::transcript::TurnStats;
use anyhow::Result;
use serde_json::Value;
use std::sync::atomic::Ordering;
use std::time::Instant;

/// Provider and transport failures are worth resending as-is; `/retry` does exactly that.
fn retryable_class(class: &str) -> bool {
    matches!(class, "transient" | "rate_limited")
}

/// The completion footer for a turn that ended in an error. A summary recorded by this turn
/// (`fresh`) is kept, so changed files and checks still show; only a turn that failed before
/// recording one gets the generic handoff. Provider/transport errors point at `/retry`.
pub(super) fn failed_turn_outcome(
    fresh: Option<&Value>,
    latency: &Value,
    error_class: &str,
) -> crate::ui::transcript::TurnOutcome {
    let retry = retryable_class(error_class);
    let Some(summary) = fresh else {
        let next = if retry {
            "Next: /retry resends the request once the provider recovers."
        } else {
            "Next: inspect /review-file and /evidence before continuing; shell effects may be outside /undo."
        };
        return crate::ui::transcript::TurnOutcome {
            status: "failed".into(),
            handoff: vec![
                "Full handoff unavailable: the turn ended before completion was recorded.".into(),
                next.into(),
            ],
            ..Default::default()
        };
    };
    let mut outcome = crate::app::shell::turn_report::outcome(summary, latency);
    if retry {
        outcome.handoff.retain(|line| !line.starts_with("Next:"));
        outcome
            .handoff
            .push("Next: /retry resends the request once the provider recovers.".into());
    }
    outcome
}

enum TurnInput<'a> {
    User(&'a str),
    Peer,
    Monitor,
}

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

    /// A message whose turn never started (the compute/preparation wait was cancelled or failed)
    /// goes back to the composer, ahead of anything typed meanwhile, instead of being lost.
    fn restore_unsent(&mut self, text: &str, cancelled: bool) {
        let draft = self.composer.draft();
        if draft.trim().is_empty() {
            self.composer.set_draft(text);
        } else if !draft.starts_with(text) {
            self.composer.set_draft(&format!("{text}\n\n{draft}"));
        }
        self.note(
            "Not sent",
            if cancelled {
                "Cancelled while waiting for compute; your message is back in the composer."
            } else {
                "The turn could not start; your message is back in the composer."
            },
        );
    }

    pub(in crate::app) fn run_turn(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        text: &str,
    ) -> Result<Step> {
        self.run_turn_input(app, agent, TurnInput::User(text))
    }

    pub(super) fn run_peer_turn(&mut self, app: &App, agent: &mut Agent<'_>) -> Result<Step> {
        self.run_turn_input(app, agent, TurnInput::Peer)
    }

    pub(super) fn run_monitor_turn(&mut self, app: &App, agent: &mut Agent<'_>) -> Result<Step> {
        self.run_turn_input(app, agent, TurnInput::Monitor)
    }

    fn run_turn_input(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        input: TurnInput<'_>,
    ) -> Result<Step> {
        let automatic = !matches!(input, TurnInput::User(_));
        let text = match input {
            TurnInput::User(text) => text,
            TurnInput::Peer => "Continuing the current conversation for peer context",
            TurnInput::Monitor => "Continuing the current task for an approved application watch",
        };
        // A joined guest's prompts belong to the host agent: send them there or refuse. The local
        // agent never runs them.
        if !automatic && let Some(routed) = crate::collab::room::route_guest_prompt(text) {
            let message = match routed {
                Ok(()) => "Sent to the host agent; its replies arrive as collab entries.",
                Err(message) => message,
            };
            self.note("Collab", message);
            return Ok(Step::Done);
        }
        if !automatic {
            // A new user submission supersedes an older wake even if preflight never reaches Agent::turn.
            agent.cancel_peer_wake();
            agent.cancel_monitor_wake();
            self.loop_mode.turn_started(text);
            platform::interrupt::clear_interrupt();
            app.interrupt.store(false, Ordering::SeqCst);
        }
        self.retry_noted = false;
        self.retry_attempt = None;
        self.stop_speech();
        self.live_tasks = Some(agent.live_task_control());
        self.background_control = agent.background_control();
        // Joins the project registry now, so even the first turn can send peer letters at once.
        self.peer_sender = agent.peer_sender();
        let started = Instant::now();
        self.status.elapsed_secs = 0.0;
        if automatic {
            self.note("Automatic continuation", text);
            self.live_pane.chat(
                ChatRole::Event,
                match input {
                    TurnInput::Peer => "Woken by a project peer message",
                    _ => "Woken by an application watch",
                },
            );
        }
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
                if automatic {
                    agent.cancel_peer_wake();
                    agent.cancel_monitor_wake();
                } else {
                    self.restore_unsent(text, true);
                }
                self.composer.set_busy(false);
                self.loop_mode.pause();
                return Ok(Step::Done);
            }
            Err(error) => {
                if automatic {
                    agent.cancel_peer_wake();
                    agent.cancel_monitor_wake();
                }
                self.transcript.error(&format!("{error:#}"));
                if !automatic {
                    self.restore_unsent(text, false);
                }
                self.composer.set_busy(false);
                self.loop_mode.pause();
                return Ok(Step::Done);
            }
        }
        // Shown once the turn really starts, so a cancelled wait never leaves an unsent message.
        if !automatic {
            self.transcript
                .push_user_at(text, &crate::storage::session::timestamp_now());
            self.live_pane.chat(ChatRole::User, text);
        }
        // Distinguishes this turn's recorded summary from an older one when the turn errors.
        let summary_before = agent.session().control_state()["turn_summary"].clone();
        self.status.phase = Phase::Working;
        self.turn_output_tokens = 0;
        self.status.output_tokens = 0;
        self.status.tokens_per_second = None;
        self.status.turn_cost = None;
        self.live_cost = Some(super::paint::LiveCost::start(agent));
        self.status.activity = "thinking".into();
        let teach = self.teach_turn_start(app, agent);
        if !automatic && let Err(error) = self.side.prepare_turn(agent, text) {
            self.note("BTW", format!("error: {error:#}"));
        }
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
            let result = match input {
                TurnInput::Peer => agent.resume_from_peer(&mut bridge),
                TurnInput::Monitor => agent.resume_from_monitor(&mut bridge),
                TurnInput::User(_) => agent.turn(text, &mut bridge),
            };
            (result, bridge.failed, bridge.shell.turn_output_tokens)
        };
        app.interrupt.store(false, Ordering::SeqCst);
        self.peer_sender = None;
        self.side.end_turn();
        crate::collab::room::sync_host(agent.session());
        self.composer.set_busy(false);
        self.save_history(app);
        self.status.activity.clear();
        self.status.model_progress = None;
        self.status.model_silence_secs = 0;
        self.status.background_wait = None;
        self.status.provider_retry = None;
        self.status.tool_started = None;
        self.retry_attempt = None;
        self.request_phase = Default::default();
        self.retry_control = None;
        let elapsed = started.elapsed().as_secs_f64();
        self.live_cost = None;
        self.refresh_status(app, agent);
        match &result {
            Ok(report) => {
                self.live_pane.chat(ChatRole::Model, &report.text);
                if report.interrupted {
                    self.note("Interrupted", "the turn stopped at the next safe point");
                    self.live_pane.chat(ChatRole::Event, "Interrupted");
                }
                if report.trajectory.is_some() {
                    self.last_trajectory = report.trajectory;
                }
                if let (Some(trajectory), Some(start)) = (report.trajectory, teach) {
                    self.teach_turn_end(app, agent, trajectory, start);
                }
            }
            Err(error) => {
                self.transcript.error(&format!("{error:#}"));
                self.live_pane.chat(
                    ChatRole::Event,
                    &format!(
                        "Turn failed: {}",
                        crate::ui::text::one_line(&format!("{error:#}"))
                    ),
                );
            }
        }
        // Notify from the agent's outcome, not from transport success: failed, blocked and
        // incomplete turns are errors, and an interrupted turn is silent (see `turn_end_notice`).
        let outcome = match &result {
            Ok(report) if report.interrupted => crate::agent::TurnOutcome::Interrupted,
            Ok(report) => report.outcome,
            Err(_) => crate::agent::TurnOutcome::Failed,
        };
        if crate::cli::watch::enabled(&app.layout, &self.session_id)
            && let Err(error) = crate::cli::watch::append(
                &app.layout,
                &self.session_id,
                crate::cli::watch::Event::Text {
                    title: if outcome == crate::agent::TurnOutcome::Answered {
                        "Reply delivered"
                    } else {
                        "Turn finished"
                    }
                    .into(),
                    body: outcome.label().to_owned(),
                },
            )
        {
            self.note(
                "Live feed",
                format!("Could not record turn outcome: {error:#}"),
            );
        }
        // Inbox availability is independent of terminal notification preferences. Use the
        // authoritative typed outcome, not message text, to attach its settings destination.
        if let Some(notice) = crate::ui::termcaps::turn_end_notice(outcome, true, true) {
            use crate::ui::overlay::inbox::Category;
            use crate::ui::termcaps::NotifyKind;
            let category = match notice.kind {
                NotifyKind::Error => Category::Error,
                NotifyKind::Completion => Category::Completion,
                _ => Category::General,
            };
            self.inbox.push_category("Turn", notice.body, category);
        }
        if let Some(notice) =
            crate::ui::termcaps::turn_end_notice(outcome, self.ui.notify, self.ui.notify_errors)
        {
            crate::ui::termcaps::notify(notice.kind, "Dial", notice.body, &self.session_id);
        }
        self.term_activity = crate::ui::term_state::Activity::after_turn(outcome);
        // The first message names an untitled session, so the tab learns its scope here.
        self.set_scope(&agent.session().title());
        self.transcript.finish_turn();
        if matches!(&result, Ok(report) if !report.interrupted) {
            self.auto_speak(app, agent);
        }
        let control = agent.session().control_state();
        let latency = &control["efficiency"]["inference_latency"];
        match &result {
            Ok(_) => self
                .transcript
                .completion(crate::app::shell::turn_report::outcome(
                    &control["turn_summary"],
                    latency,
                )),
            Err(error) => {
                // A fatal error may precede durable completion. Never reuse an older turn's summary.
                let recorded = &control["turn_summary"];
                let fresh =
                    (recorded.is_object() && *recorded != summary_before).then_some(recorded);
                let class = fresh
                    .and_then(|summary| summary["error_class"].as_str())
                    .map_or_else(
                        || crate::llm::errors::classify(error).label(),
                        |class| class,
                    )
                    .to_owned();
                self.transcript
                    .completion(failed_turn_outcome(fresh, latency, &class));
            }
        }
        self.transcript.turn_stats(TurnStats {
            clock: local_time("%Y-%m-%d %H:%M:%S"),
            elapsed_secs: elapsed,
            prompt_tokens: self.status.prompt_tokens,
            output_tokens: (streamed_tokens > 0).then_some(streamed_tokens),
            tokens_per_second: self.status.tokens_per_second,
            outcome: None,
        });
        // Questions, blockers and failures wait for the human instead of re-arming the loop.
        let halted = result
            .as_ref()
            .map_or(true, |report| report.halts_automation());
        if !automatic {
            self.loop_mode.turn_settled(Instant::now(), halted);
        }
        if failed {
            anyhow::bail!("terminal draw failed; the screen was restored");
        }
        Ok(Step::Done)
    }
}

#[cfg(test)]
mod tests;

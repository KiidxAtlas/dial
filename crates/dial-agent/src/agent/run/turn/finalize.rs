//! Unified completion delivery pipeline shared by plain replies and typed `finish(...)` calls.
//!
//! Both completion surfaces must enforce identical post-acceptance boundaries: live steering and
//! interruption checks, external-thinking scratchpad stripping *before* review, stop-boundary
//! gating for `quality_review` and `coach_completion`, completion notes, change disclosure, visual
//! summaries, and training-request capture.

use super::*;
use crate::agent::run::engine::Tick;

/// Which surface produced the accepted turn answer.
pub(super) enum DeliverySurface {
    /// A plain assistant reply already persisted and streamed (except any newly appended suffix).
    Plain { outcome: TurnOutcome },
    /// A typed `finish(...)` tool call whose final text has not yet been persisted or streamed.
    Finish(crate::agent::verify::completion::Finish),
}

/// Request facts needed by the shared delivery tail.
pub(super) struct DeliveryContext<'a> {
    pub step: u32,
    pub reply: &'a llm::Reply,
    pub context_before: &'a [Value],
    pub request_tools: &'a [Value],
}

impl Agent<'_> {
    /// Finalize an accepted plain reply or `finish(...)` call through one shared delivery path.
    pub(super) fn finalize_turn_delivery(
        &mut self,
        turn: &mut TurnState<'_>,
        backend: &mut Backend,
        ui: &mut dyn Ui,
        delivery: DeliveryContext<'_>,
        surface: DeliverySurface,
    ) -> Result<Tick<TurnReport>> {
        let DeliveryContext {
            step,
            reply,
            context_before,
            request_tools,
        } = delivery;

        let (mut text, outcome, persist_and_stream_all) = match surface {
            DeliverySurface::Plain { outcome } => (reply.text.clone(), outcome, false),
            DeliverySurface::Finish(mut finish) => {
                let accepted_live = self.flush_accepted_live_steering(ui)?;
                if self.take_steering(ui)? || accepted_live {
                    turn.steering_pending = true;
                    return Ok(Tick::Again);
                }
                if self.interrupt.load(Ordering::SeqCst) {
                    return Ok(Tick::Finished(TurnReport::interrupted(step + 1)));
                }
                // Strip private `<scratchpad>` blocks before completion review so raw internal
                // reasoning never leaks into coach prompts or final reports.
                self.normalize_finish(&mut finish);
                (finish.text, finish.outcome, true)
            }
        };

        // Past a deadline or cost cap the answer is delivered without another review or coach call.
        let reviewable = self.stop_boundary().is_none();
        if outcome == TurnOutcome::Completed {
            if reviewable && let Some(message) = self.quality_review(ui)? {
                turn.notices.push_raw(message);
                return Ok(Tick::Again);
            }
            if reviewable
                && let Some(message) = self.coach_completion(
                    &mut turn.coach_turn,
                    backend,
                    turn.user_goal,
                    &text,
                    step,
                    ui,
                )?
            {
                turn.notices.push_raw(message);
                return Ok(Tick::Again);
            }
            let note = turn.coach_turn.completion_note();
            if !note.is_empty() {
                text.push_str(&note);
                if !persist_and_stream_all {
                    ui.text_delta(&note);
                }
            }
            self.disclose_turn_changes(ui);
            if self.capture_training_requests() {
                self.training_capture.record(
                    context_before,
                    request_tools,
                    llm::assistant_message(reply),
                    reply.wire_form,
                );
            }
            let before_visual = text.len();
            text = self
                .step_checks
                .visual
                .summary(text, self.step_checks.revision());
            if !persist_and_stream_all && text.len() > before_visual {
                ui.text_delta(&text[before_visual..]);
            }
        }

        if persist_and_stream_all {
            self.push(json!({"role": "assistant", "content": text}))?;
            ui.text_delta(&text);
        }

        Ok(Tick::Finished(TurnReport {
            text,
            outcome,
            steps: step + 1,
            interrupted: false,
            trajectory: None,
        }))
    }
}

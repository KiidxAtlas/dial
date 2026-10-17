//! User steering between steps and live steering during generation.

use super::*;

/// Server acknowledgement deadline for live Codex steering; afterwards the receipt is uncertain.
pub(super) const LIVE_STEER_ACK: Duration = Duration::from_secs(10);
pub(super) struct PendingSteer {
    pub(super) message: Value,
    pub(super) text: String,
    pub(super) ticket: Option<llm::codex_ws::SteerTicket>,
    pub(super) deadline: Instant,
}
/// Steering submitted during one generation. Journaled strictly in submission order.
#[derive(Default)]
pub(super) struct LiveSteering {
    pub(super) pending: std::collections::VecDeque<PendingSteer>,
    pub(super) replanning: bool,
}

impl<'a> Agent<'a> {
    pub(super) fn drain_steering(&mut self, ui: &mut dyn Ui) -> Vec<String> {
        let mut messages = std::mem::take(&mut self.pending_tool_steering);
        messages.extend(ui.take_steering());
        messages
    }

    pub(super) fn take_steering(&mut self, ui: &mut dyn Ui) -> Result<bool> {
        let messages = self.drain_steering(ui);
        self.accept_steering(messages, ui)
    }

    pub(super) fn accept_steering(
        &mut self,
        messages: Vec<String>,
        ui: &mut dyn Ui,
    ) -> Result<bool> {
        let mut received = false;
        for text in messages {
            let Some(message) = self.prepare_steering(&text, ui) else {
                continue;
            };
            self.commit_steering(message, &text)?;
            received = true;
        }
        if received {
            self.finish_steering(
                ui,
                "Steering received; continuing with the new instruction.",
            )?;
        }
        Ok(received)
    }

    /// Expand one steering text into the exact message that will be journaled; nothing is persisted.
    pub(super) fn prepare_steering(&self, text: &str, ui: &mut dyn Ui) -> Option<Value> {
        if text.trim().is_empty() {
            return None;
        }
        let mut message = if let Some(layout) = &self.layout {
            let (message, warnings) =
                context::expand_message(&layout.project_root, text, &self.interrupt);
            for warning in warnings {
                ui.notice(&warning);
            }
            message
        } else {
            json!({"role":"user","content":text})
        };
        for reference in message["dial_images"].as_array().into_iter().flatten() {
            ui.image(reference);
        }
        message["dial_context_request"] = json!(true);
        Some(message)
    }

    /// Journal a prepared steering message exactly once and make it the current instruction.
    pub(super) fn commit_steering(&mut self, message: Value, text: &str) -> Result<()> {
        self.push(message)?;
        self.task_policy = crate::learn::task_policy::Policy::for_prompt(
            text,
            self.config.reasoning_guard.max_pre_action_secs,
        );
        self.request_phase = crate::adaptive::State::begin(self.task_policy.kind, text);
        let mut control = self.session.control_state();
        control["current_instruction_entry_id"] = json!(self.session.branch_leaf());
        control["current_instruction"] = json!(prompt::truncate_chars(text, 4096));
        control["steering_active"] = json!(true);
        self.session.set_control_state(control)
    }

    pub(super) fn finish_steering(&mut self, ui: &mut dyn Ui, notice: &str) -> Result<()> {
        self.invalidate_exploration(
            "new external user steering superseded the retained checkpoint",
            ui,
        )?;
        self.clear_context_rollover();
        self.task_packet.clear();
        self.reasoning_recovery = Default::default();
        self.pending_plan = None;
        ui.notice(notice);
        Ok(())
    }

    /// One poll of user steering while a response streams. Returns `true` when the generation must
    /// be discarded and replanned locally. Live Codex steering is journaled only after the server
    /// acknowledges it, in submission order; any rejection, uncertain receipt, missing live
    /// session or later local message journals every outstanding message once and replans.
    pub(super) fn steer_during_generation(
        &mut self,
        ui: &mut dyn Ui,
        live_key: Option<&str>,
        live: &mut LiveSteering,
    ) -> Result<bool> {
        if live.replanning {
            // Already discarding this generation; keep journaling new input for the replay.
            return self.take_steering(ui);
        }
        for text in self.drain_steering(ui) {
            let Some(message) = self.prepare_steering(&text, ui) else {
                continue;
            };
            let ticket = match live_key {
                Some(key) => llm::responses::input_items(std::slice::from_ref(&message))
                    .and_then(|input| llm::codex_ws::submit(key, input)),
                None => Ok(None),
            };
            let ticket = ticket.ok().flatten();
            let local = ticket.is_none();
            live.pending.push_back(PendingSteer {
                message,
                text,
                ticket,
                deadline: Instant::now() + LIVE_STEER_ACK,
            });
            if local {
                return self.replan_live_steering(ui, live);
            }
        }
        self.poll_live_steering(ui, live, false)
    }

    /// Move the acknowledged prefix to `accepted_live_steering`; any failed or expired receipt
    /// switches to local replay. Accepted input is journaled after the reply it steered (and that
    /// reply's tool results), which is where the server placed it.
    pub(super) fn poll_live_steering(
        &mut self,
        ui: &mut dyn Ui,
        live: &mut LiveSteering,
        wait: bool,
    ) -> Result<bool> {
        while let Some(front) = live.pending.front() {
            let Some(ticket) = front.ticket.as_ref() else {
                break;
            };
            let outcome = if wait {
                Some(ticket.wait_until(front.deadline))
            } else {
                ticket.try_outcome().or_else(|| {
                    (Instant::now() >= front.deadline)
                        .then_some(llm::codex_ws::SteerOutcome::Uncertain)
                })
            };
            match outcome {
                None => break,
                Some(llm::codex_ws::SteerOutcome::Accepted { id }) => {
                    let steer = live.pending.pop_front().expect("front exists");
                    self.record(
                        self.cur_step,
                        "steering_accepted_live",
                        json!({"steer_id":id}),
                    )?;
                    ui.event(&crate::agent::ui_event::UiEvent::new(
                        crate::agent::ui_event::EventKind::Steering,
                        "Steering delivered to the running response.",
                    ));
                    self.accepted_live_steering
                        .push((steer.message, steer.text));
                }
                Some(_) => return self.replan_live_steering(ui, live),
            }
        }
        Ok(false)
    }

    /// Journal accepted and outstanding steering once, in submission order, and discard the generation.
    pub(super) fn replan_live_steering(
        &mut self,
        ui: &mut dyn Ui,
        live: &mut LiveSteering,
    ) -> Result<bool> {
        live.replanning = true;
        let mut received = !self.accepted_live_steering.is_empty();
        for (message, text) in std::mem::take(&mut self.accepted_live_steering) {
            self.commit_steering(message, &text)?;
        }
        while let Some(steer) = live.pending.pop_front() {
            self.commit_steering(steer.message, &steer.text)?;
            received = true;
        }
        if received {
            self.finish_steering(
                ui,
                "Steering received; continuing with the new instruction.",
            )?;
        }
        Ok(true)
    }

    /// Journal steering the server accepted for an already-journaled reply. Returns `true` when the
    /// turn must continue so the next request lines up with the server's steered successor.
    pub(super) fn flush_accepted_live_steering(&mut self, ui: &mut dyn Ui) -> Result<bool> {
        if self.accepted_live_steering.is_empty() {
            return Ok(false);
        }
        for (message, text) in std::mem::take(&mut self.accepted_live_steering) {
            self.commit_steering(message, &text)?;
        }
        self.finish_steering(
            ui,
            "Continuing with the steering the running response accepted.",
        )?;
        Ok(true)
    }

    /// After the stream ends, resolve outstanding receipts (bounded by each ACK deadline).
    pub(super) fn settle_live_steering(
        &mut self,
        ui: &mut dyn Ui,
        live: &mut LiveSteering,
    ) -> Result<bool> {
        if live.pending.is_empty() {
            return Ok(false);
        }
        if self.interrupt.load(Ordering::SeqCst) {
            return self.replan_live_steering(ui, live);
        }
        self.poll_live_steering(ui, live, true)
    }
}

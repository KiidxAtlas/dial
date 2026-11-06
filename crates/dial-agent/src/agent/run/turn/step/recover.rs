//! Classify provider failures and retain only replies that actually answered this request.
use super::*;

impl Agent<'_> {
    pub(super) fn recover_generation(
        &mut self,
        context: &mut StepContext<'_, '_>,
        generation: Generation,
    ) -> Result<Phase<ReceivedReply>> {
        let StepContext {
            turn,
            engine,
            backend,
            schemas: _,
            ui,
        } = context;
        let turn = &mut **turn;
        let engine = &mut **engine;
        let backend = &mut **backend;
        let ui = &mut **ui;
        let Generation {
            result,
            rule_deferred,
            sent,
        } = generation;
        let reply = match result {
            Err(error)
                if error.downcast_ref::<SilentRequest>().is_some()
                    && !self.interrupt.load(Ordering::SeqCst) =>
            {
                let stalled = error.downcast_ref::<SilentRequest>().unwrap();
                if let Some(direction) = stalled.direction.clone() {
                    // Same local resets as ordinary steering: the new direction must not
                    // inherit the old task's restrictions, guidance or recovery counters.
                    if self.accept_steering(vec![direction], ui)? {
                        turn.steering_pending = true;
                    }
                    turn.budget.give_back();
                    return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
                }
                if !stalled.partial.trim().is_empty()
                    && turn.stall_continues
                        < crate::agent::safety::stream_guards::STREAM_STALL_CONTINUE_MAX_RETRIES
                {
                    turn.stall_continues += 1;
                    let partial = if self.external_thinking_instruction().is_some() {
                        crate::agent::modes::external_thinking::strip_scratchpad(&stalled.partial)
                    } else {
                        stalled.partial.clone()
                    };
                    self.request_phase
                        .recover("the stream stalled after partial text; continuing");
                    ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                        crate::agent::ui::ui_event::EventKind::Recovery,
                        format!(
                            "Stream stalled after partial text; continuing ({}/{})",
                            turn.stall_continues,
                            crate::agent::safety::stream_guards::STREAM_STALL_CONTINUE_MAX_RETRIES
                        ),
                    ));
                    let history_len = self.session.message_count();
                    self.push(json!({"role":"assistant","content":partial}))?;
                    // The partial answer kept in history responded to this request's notes.
                    turn.reply_received(history_len);
                    // Charge shared recovery after retaining the partial answer.
                    if let Some(report) = self.setback(
                        turn,
                        engine,
                        super::super::progress::Setback::StreamStall,
                        sent.step,
                        ui,
                    )? {
                        return Ok(ControlFlow::Break(
                            crate::agent::run::engine::Tick::Finished(report),
                        ));
                    }
                    turn.pending_nudges.push(json!({"role":"user","content":crate::agent::safety::stream_guards::render_retry_prompt(crate::agent::safety::stream_guards::STREAM_STALL_CONTINUE, turn.stall_continues, crate::agent::safety::stream_guards::STREAM_STALL_CONTINUE_MAX_RETRIES),"dial_transient":true}));
                    turn.refund_unusable(ui);
                    return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
                }
                self.save_request_policy(None, json!({"state":"request needs recovery","elapsed_ms":sent.started.elapsed().as_millis()}), ui)?;
                return self
                    .stop_turn(
                        ui,
                        format!("{error} Send a direction or retry to resume the unfinished task."),
                        sent.step + 1,
                    )
                    .map(crate::agent::run::engine::Tick::Finished)
                    .map(ControlFlow::Break);
            }
            Ok(reply) => {
                turn.reply_received(self.session.message_count());
                self.admit_rule_triggers(turn, &reply, rule_deferred, ui)?;
                reply
            }
            Err(error)
                if error
                    .downcast_ref::<crate::agent::workflow::reasoning::GateHit>()
                    .is_some()
                    && !self.interrupt.load(Ordering::SeqCst) =>
            {
                turn.reasoning_budget = Some(sent.requested_tokens);
                self.request_phase
                    .recover("reasoning gate intercepted the previous request");
                self.save_request_policy(
                    None,
                    json!({"state":"reasoning gate","elapsed_ms":sent.started.elapsed().as_millis()}),
                    ui,
                )?;
                self.metrics.reasoning_interceptions += 1;
                self.record(
                    sent.step,
                    "reasoning_gate",
                    json!({"reason":error.to_string(),"elapsed_ms":sent.started.elapsed().as_millis()}),
                )?;
                let plan_gate = self.config.reasoning_guard.require_plan
                    && !self.todo.has_open()
                    && !error
                        .downcast_ref::<crate::agent::workflow::reasoning::GateHit>()
                        .is_some_and(|hit| hit.is_repetition());
                let recovered = if plan_gate {
                    self.reasoning_recovery
                        .intercept_planned(&error.to_string())
                } else {
                    self.reasoning_recovery.intercept(&error.to_string())
                };
                turn.plan_after_gate = plan_gate && recovered.is_ok();
                match recovered {
                    Ok(message) => {
                        ui.notice(&message);
                        self.push(json!({"role":"user","content":message}))?;
                    }
                    Err(message) => {
                        return self
                            .stop_turn(ui, message, sent.step + 1)
                            .map(crate::agent::run::engine::Tick::Finished)
                            .map(ControlFlow::Break);
                    }
                }
                return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
            }
            Err(error)
                if error.downcast_ref::<llm::Interrupted>().is_some()
                    || self.interrupt.load(Ordering::SeqCst) =>
            {
                self.save_request_policy(
                    None,
                    json!({"state":"interrupted","elapsed_ms":sent.started.elapsed().as_millis()}),
                    ui,
                )?;
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(TurnReport::interrupted(
                        sent.step + 1,
                    )),
                ));
            }
            // Only a fallback that actually took over retries here. Without one, the error
            // continues to the shared failure arm below, so every provider failure is recorded
            // and carries the same recovery guidance whichever class it had.
            Err(error)
                if !self.hermetic
                    && error.downcast_ref::<llm::errors::ThinkingLoop>().is_none()
                    && crate::llm::fallback::should_fallback(
                        &error,
                        self.effective_config().retry.fallback_on,
                    )
                    && self.fail_over(backend, &error, ui) =>
            {
                turn.max_tokens = backend.output_budget(turn.harness.max_tokens);
                turn.budget.give_back();
                return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
            }
            Err(error)
                if !self.hermetic
                    && looks_like_overflow(&error)
                    && turn.overflow_retries < 3
                    && self.config.compaction.enabled
                    && self.config.compaction.strategy
                        != crate::config::compaction::CompactionStrategy::Off =>
            {
                if let Some(report) = self.setback(
                    turn,
                    engine,
                    super::super::progress::Setback::Overflow,
                    sent.step,
                    ui,
                )? {
                    return Ok(ControlFlow::Break(
                        crate::agent::run::engine::Tick::Finished(report),
                    ));
                }
                self.request_phase
                    .recover("context overflow; bounded compaction retry");
                // Consecutive overflows without a reply; a received reply restarts the count.
                turn.overflow_retries += 1;
                let promote_output = backend.output_budget(sent.requested_tokens);
                if self.try_promote_context(backend, sent.request_context, promote_output, true, ui)
                {
                    turn.max_tokens = backend.output_budget(turn.harness.max_tokens);
                    turn.compaction_failed = false;
                    turn.compaction_retry_floor = None;
                    turn.budget.give_back();
                    return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
                }
                // The provider measured the prompt it rejected. Calibrate on that real count so
                // the compaction budget checks below stop trusting the estimate that let it through.
                if let Some(actual) = crate::llm::errors::overflow_prompt_tokens(&error) {
                    self.context_meter.complete(
                        sent.request_estimate,
                        sent.request_context,
                        Some(actual),
                        sent.sent_messages,
                    );
                }
                ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                    crate::agent::ui::ui_event::EventKind::Recovery,
                    "context window exceeded; compacting and retrying",
                ));
                let overflow_request = Chat {
                    messages: sent.context_before.clone(),
                    tools: sent.offer.clone(),
                    temperature: turn.harness.temperature,
                    max_tokens: sent.requested_tokens,
                    seed: None,
                };
                let options = crate::config::compaction::CompactionOptions {
                    reason: crate::config::compaction::CompactionReason::Overflow,
                    ..Default::default()
                };
                self.poll_background_compaction(ui);
                let claimed = match self.claim_background_compaction(
                    ui,
                    crate::config::compaction::CompactionReason::Overflow,
                    Some(&overflow_request),
                    Some(backend),
                ) {
                    Ok(Some(outcome)) => Ok(outcome),
                    Ok(None) => self.compact_using(backend, ui, Some(&overflow_request), &options),
                    Err(error) => {
                        ui.diagnostic(&format!("Prepared compaction was not claimed during overflow ({error:#}); using blocking maintenance."));
                        self.compact_using(backend, ui, Some(&overflow_request), &options)
                    }
                };
                // Same degradation as proactive compaction: record the failure and stop with the
                // session intact instead of failing the turn with a bare error.
                let outcome = match claimed {
                    Ok(outcome) => outcome,
                    Err(compaction_error) => {
                        turn.compaction_failed_at(sent.request_context);
                        self.record(sent.step, "generation_error", json!({"error":format!("{error:#}"),"compaction_error":format!("{compaction_error:#}"),"elapsed_ms":sent.started.elapsed().as_millis()}))?;
                        if self.interrupt.load(Ordering::SeqCst) {
                            return Ok(ControlFlow::Break(
                                crate::agent::run::engine::Tick::Finished(TurnReport::interrupted(
                                    sent.step + 1,
                                )),
                            ));
                        }
                        return self
                            .stop_turn(
                                ui,
                                format!("Stopped: the request exceeded the model's context window and compaction could not make room ({compaction_error:#}). The session and completed work are kept; select a larger-window model, compact manually, or say \"continue\" to retry."),
                                sent.step + 1,
                            )
                            .map(crate::agent::run::engine::Tick::Finished).map(ControlFlow::Break);
                    }
                };
                if !outcome.continue_allowed
                    && outcome.kind
                        != crate::agent::compact::compaction::CompactionOutcomeKind::Compaction
                {
                    return self
                        .pause_after_compaction(ui, sent.step)
                        .map(crate::agent::run::engine::Tick::Finished)
                        .map(ControlFlow::Break);
                }
                turn.compaction_succeeded();
                turn.context_rewritten(engine);
                turn.budget.give_back();
                return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
            }
            // The constrained retry is an optional aid. A local server that rejects its schema
            // (grammar compilation, unsupported `response_format`) must not end the session: drop
            // the aid for this turn and resend an ordinary request.
            Err(error) if sent.repairing && !self.hermetic => {
                turn.repair.reject();
                self.record(
                    sent.step,
                    "constrained_repair",
                    json!({"phase":"result","outcome":"provider_rejected","error":format!("{error:#}")}),
                )?;
                ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                    crate::agent::ui::ui_event::EventKind::Recovery,
                    "The server rejected the constrained tool-call retry; continuing without it",
                ));
                turn.budget.give_back();
                return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
            }
            Err(error) => {
                self.save_request_policy(
                    None,
                    json!({"state":"generation failed","elapsed_ms":sent.started.elapsed().as_millis()}),
                    ui,
                )?;
                self.record(sent.step, "generation_error", json!({"error":format!("{error:#}"),"elapsed_ms":sent.started.elapsed().as_millis()}))?;
                crate::telemetry::request_failed(
                    self.config,
                    self.hermetic,
                    self.session.id(),
                    backend.model_label(),
                    sent.started.elapsed(),
                    &error,
                );
                return Err(error.context(crate::agent::run::failure_guidance::PROVIDER_RECOVERY))
                    .map(crate::agent::run::engine::Tick::Finished)
                    .map(ControlFlow::Break);
            }
        };
        Ok(ControlFlow::Continue(ReceivedReply { reply, sent }))
    }
}

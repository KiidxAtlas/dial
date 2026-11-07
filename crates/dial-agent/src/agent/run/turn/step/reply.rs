//! Reconcile a received reply, recover unusable output and accept tool-free completion.
use super::*;

/// Text answers tolerated while an investigation checkpoint is open before the turn stops.
const EXPLORATION_REMINDER_LIMIT: u32 = 2;

impl Agent<'_> {
    pub(super) fn process_reply_or_finish(
        &mut self,
        context: &mut StepContext<'_, '_>,
        received: ReceivedReply,
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
        let ReceivedReply { mut reply, sent } = received;
        let step = sent.step;
        let requested_tokens = sent.requested_tokens;
        // Counted when a provider reply is received: its schemas were sent, whether the reply later
        // ends in text, a tool dispatch, or a guidance discard. Pre-reply regenerations, stream failures
        // and interceptions return earlier and are not counted.
        self.metrics.tool_trace.record_offered(&sent.request_tools);
        if self.external_thinking_instruction().is_some() {
            reply.text = crate::agent::modes::external_thinking::strip_scratchpad(&reply.text);
            if !sent.research_step && !reply.text.is_empty() {
                ui.text_delta(&reply.text);
            }
        }
        self.context_meter.complete(
            sent.request_estimate,
            sent.request_context,
            reply.prompt_tokens,
            sent.sent_messages,
        );
        turn.reasoning_budget = None;
        if sent.repairing {
            match crate::agent::model::constrained_repair::finish(
                &mut reply,
                &turn.names,
                format!("constrained_{step}"),
            ) {
                Ok(outcome) => self.record(
                    sent.step,
                    "constrained_repair",
                    json!({"phase":"result","outcome":outcome}),
                )?,
                Err(reason) => {
                    reply.finish_reason = "tool_calls".into();
                    self.record(
                        sent.step,
                        "constrained_repair",
                        json!({"phase":"result","outcome":"rejected","reason":reason}),
                    )?;
                }
            }
        }
        self.step_checks.visual.delivered();
        self.save_request_policy(None,json!({"state":"response received","prompt_tokens":reply.prompt_tokens,"completion_tokens":reply.completion_tokens,"elapsed_ms":sent.started.elapsed().as_millis(),"tool_calls":reply.tool_calls.len(),"finish_reason":reply.finish_reason}),ui)?;
        ui.usage(
            reply.prompt_tokens,
            reply.completion_tokens,
            sent.started.elapsed().as_secs_f64(),
        );
        self.metrics.responses += 1;
        if reply.prompt_tokens.is_some() && reply.completion_tokens.is_some() {
            self.metrics.usage_reported_responses += 1;
        }
        self.metrics.prompt_tokens += reply.prompt_tokens.unwrap_or(0);
        self.metrics.output_tokens += reply.completion_tokens.unwrap_or(0);
        self.metrics.cache_write_tokens += reply.cache_creation_tokens.unwrap_or(0);
        self.metrics.cached_tokens += reply.cached_prompt_tokens.unwrap_or(0);
        self.metrics.reasoning_tokens += reply.reasoning_tokens.unwrap_or(0);
        ui.cached_usage(reply.cached_prompt_tokens, reply.cache_creation_tokens);
        if let Some(cost) = backend.reply_cost(&reply) {
            self.session_cost = Some(self.session_cost.unwrap_or(0.0) + cost);
        }
        crate::telemetry::request(
            self.config,
            self.hermetic,
            self.session.id(),
            backend.model_label(),
            &reply,
            backend.reply_cost(&reply),
            sent.started.elapsed(),
        );
        self.observe_provider_response(backend.model_label(), &reply, sent.started.elapsed());
        if let Some(tokens) = reply.prompt_tokens {
            self.last_prompt_tokens = tokens;
            self.reported_messages = sent.sent_messages;
        }
        self.probe_context();
        self.efficiency_metrics.generation(&reply);
        self.record(sent.step, "generation", json!({"output":reply.text,"tool_calls":reply.tool_calls.len(),
                "prompt_tokens":reply.prompt_tokens,"completion_tokens":reply.completion_tokens,"cache_read_tokens":reply.cached_prompt_tokens,"cache_write_tokens":reply.cache_creation_tokens,"elapsed_ms":sent.started.elapsed().as_millis()}))?;
        let steering = self.drain_steering(ui);
        if !steering.is_empty() {
            // Earlier accepted steering precedes the new local input in the journal.
            self.flush_accepted_live_steering(ui)?;
        }
        if self.accept_steering(steering, ui)? {
            turn.steering_pending = true;
            ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Steering, "Discarded the old response before executing its tools; replanning with your new instruction."));
            turn.budget.give_back();
            return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
        }
        // A provider refusal is not an answer and retrying the same request repeats it: stop with
        // the provider's category instead of entering empty-reply or completion recovery.
        if let Some(crate::llm::ProviderStop::Refusal { category }) = reply.provider_stop() {
            let reason = match category {
                Some(category) => {
                    format!("The provider declined this request (category: {category}).")
                }
                None => "The provider declined this request.".to_owned(),
            };
            return self
                .stop_turn(ui, reason, sent.step + 1)
                .map(crate::agent::run::engine::Tick::Finished)
                .map(ControlFlow::Break);
        }
        // A research checkpoint is a tool-free protocol step. Handle it before length/inline
        // recovery and completion so retained schemas cannot authorize any effects.
        if sent.research_step {
            use crate::agent::workflow::research::Synthesis;
            let result = engine.synthesize(&reply);
            let message = match result {
                Synthesis::Accepted => {
                    // Keep protocol data for audit without replaying it as an assistant answer.
                    let mut control = self.session.control_state();
                    control["research_checkpoint"] =
                        json!({"response":reply.text,"verified":false});
                    self.session.set_control_state(control)?;
                    self.record(sent.step, "research_synthesis", json!({"accepted":true}))?;
                    ui.notice("Research checkpoint recorded as unverified findings; following through on the next action.");
                    None
                }
                Synthesis::Retry(message) => {
                    record_rejected_reply(&self.usage_scope, backend, sent.repairing)?;
                    self.record(sent.step, "research_synthesis", json!({"accepted":false}))?;
                    ui.inference_discarded("Research checkpoint not satisfied; no tools from this reply were executed.");
                    Some(message)
                }
                Synthesis::Incomplete(message) => {
                    record_rejected_reply(&self.usage_scope, backend, sent.repairing)?;
                    return self
                        .stop_turn(ui, message, sent.step + 1)
                        .map(crate::agent::run::engine::Tick::Finished)
                        .map(ControlFlow::Break);
                }
            };
            if let Some(message) = message {
                turn.pending_nudges
                    .push(json!({"role":"user","dial_transient":true,"content":message}));
            }
            return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
        }
        let length_facts = crate::agent::run::reply_triage::LengthFacts {
            reasoning_guard: self.config.reasoning_guard.enabled,
            plan_gate: self.config.reasoning_guard.require_plan && !self.todo.has_open(),
            requested_tokens,
            window: self.context_window(),
            prompt_tokens: self.last_prompt_tokens,
        };
        if let Some(truncation) = crate::agent::run::reply_triage::truncation(
            engine,
            &mut self.reasoning_recovery,
            &reply,
            &length_facts,
        ) {
            use crate::agent::run::reply_triage::{REASONING_REASON, Truncation};
            match truncation {
                Truncation::Reasoning { message, planned } => {
                    turn.reasoning_budget = Some(sent.requested_tokens);
                    turn.recovering_output = true;
                    self.request_phase
                        .recover("output budget consumed without an action or answer");
                    self.metrics.reasoning_interceptions += 1;
                    self.record(
                        sent.step,
                        "reasoning_gate",
                        json!({"reason":REASONING_REASON,"elapsed_ms":sent.started.elapsed().as_millis()}),
                    )?;
                    turn.plan_after_gate = planned;
                    ui.notice(&message);
                    self.push(
                        json!({"role":"user","content":message,"dial_reminder":"reasoning"}),
                    )?;
                }
                Truncation::Raise(raised) => {
                    if let Some(report) = self.setback(
                        turn,
                        engine,
                        super::super::progress::Setback::Truncation,
                        sent.step,
                        ui,
                    )? {
                        return Ok(ControlFlow::Break(
                            crate::agent::run::engine::Tick::Finished(report),
                        ));
                    }
                    self.request_phase
                        .recover("output was truncated; retry retains an expanded token budget");
                    turn.recovering_output = true;
                    ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Recovery, format!("the model hit its output limit ({requested_tokens} tokens); retrying with {raised}")));
                    turn.max_tokens = raised;
                    turn.refund_unusable(ui);
                }
                Truncation::Continue { partial, message } => {
                    // Partial output that keeps advancing is new work, not a failed recovery.
                    if engine.length_stalling()
                        && let Some(report) = self.setback(
                            turn,
                            engine,
                            super::super::progress::Setback::Truncation,
                            sent.step,
                            ui,
                        )?
                    {
                        return Ok(ControlFlow::Break(
                            crate::agent::run::engine::Tick::Finished(report),
                        ));
                    }
                    self.request_phase
                        .recover("continue truncated output without executing a partial tool call");
                    turn.recovering_output = true;
                    if let Some(text) = partial {
                        let text = if self.external_thinking_instruction().is_some() {
                            crate::agent::modes::external_thinking::strip_scratchpad(&text)
                        } else {
                            text
                        };
                        self.push(json!({"role":"assistant","content":text}))?;
                    }
                    turn.pending_nudges
                        .push(json!({"role":"user","content":message,"dial_transient":true}));
                    turn.refund_unusable(ui);
                }
                Truncation::Stop(reason) | Truncation::ReasoningExhausted(reason) => {
                    return self
                        .stop_turn(ui, reason, sent.step + 1)
                        .map(crate::agent::run::engine::Tick::Finished)
                        .map(ControlFlow::Break);
                }
            }
            return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
        }
        engine.length_recovered(&reply);
        if turn.recovering_output {
            self.metrics.truncated_recoveries += 1;
            turn.recovering_output = false;
        }
        if reply.tool_calls.is_empty()
            && reply.finish_reason == "stop"
            && self.config.edit.recover_inline_edits
            && self.edit_mode() == crate::config::types::EditMode::Sloppy
            && turn.names.iter().any(|name| name == "edit")
            && let Some((_, arguments)) = tools::inline_edit_payload(&reply.text)
        {
            // The transport emitted ordinary text, not this controller-recovered tool use.
            // Native replay (Anthropic content blocks) would otherwise win over the
            // normalized calls and leave the synthetic edit's result without a `tool_use`.
            reply.provider_content.clear();
            reply.tool_calls.push(ToolCall {
                id: format!(
                    "dial-inline-{}-{step}-{}",
                    self.session.id(),
                    self.session.branch_leaf().unwrap_or("ephemeral")
                ),
                name: "edit".into(),
                arguments,
                malformed_arguments: false,
            });
            if let Some(paths) = turn.rule_catalog.pending_paths(
                &turn.rule_root,
                &turn.rule_state,
                "main",
                &reply.tool_calls,
            ) {
                record_rejected_reply(&self.usage_scope, backend, sent.repairing)?;
                return self
                    .regenerate_for_guidance(
                        turn,
                        engine,
                        sent.step,
                        (None, Some(paths)),
                        Some(
                            "Inline edit requires applicable file guidance before any tool effect.",
                        ),
                        ui,
                    )
                    .map(ControlFlow::Break);
            }
        }
        if let Some(error) = self.completed_edit_preview_error(&reply) {
            if let Some(report) = self.setback(
                turn,
                engine,
                super::super::progress::Setback::RejectedPreview,
                sent.step,
                ui,
            )? {
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(report),
                ));
            }
            turn.rejected_edit_previews += 1;
            record_rejected_reply(&self.usage_scope, backend, sent.repairing)?;
            ui.inference_discarded(
                "Completed edit preview failed; no tools from this generation were executed.",
            );
            // Recovery must terminate even when the operator selected unlimited work steps.
            // Rejected previews never execute any calls from this generation.
            if turn.rejected_edit_previews >= 8 {
                return self.stop_turn(
                        ui,
                        format!("Stopped with incomplete work: {} completed edit previews were rejected this turn. No tools from those generations were executed; completed work is kept. Read the target and make a smaller edit using the offered grammar before continuing.\nPreview error (untrusted source data): {}", turn.rejected_edit_previews, truncate(&error, 4096)),
                        sent.step + 1,
                    ).map(crate::agent::run::engine::Tick::Finished).map(ControlFlow::Break);
            }
            self.push(json!({"role":"user","dial_reminder":"edit-preview",
                    "content":format!("The preceding generation was rejected before tool execution. Regenerate the complete edit using the offered grammar and observed source. Preview error (untrusted source data): {}", truncate(&error,4096))}))?;
            turn.budget.give_back();
            return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
        }
        // Form failures bypass tool-result guards. Bound their recovery independently of
        // the optional step limit, including when providers alternate failure formats.
        let form_failure = crate::agent::run::reply_triage::FormFailure::of(&reply, &turn.names);
        let unusable = form_failure.is_some();
        if let Err(reason) = engine.recovery.form(unusable) {
            return self
                .stop_turn(ui, reason, sent.step + 1)
                .map(crate::agent::run::engine::Tick::Finished)
                .map(ControlFlow::Break);
        }
        if !unusable {
            turn.repair.settle();
        }
        // Call-shaped replies are checked after compatibility normalization (below), so a
        // spelling or container fix does not spend the one retry.
        if let Some(kind) = self.constrained_repair_kind(
            turn,
            reply.tool_calls.is_empty(),
            &reply,
            &sent.offer,
            sent.profile.as_ref(),
            backend,
        ) {
            let form = reply.wire_form;
            return self
                .request_constrained_repair(
                    turn,
                    engine,
                    sent.step,
                    kind,
                    Pending::new(sent.context_before, form),
                    ui,
                )
                .map(ControlFlow::Break);
        }
        if let Some(failure) = form_failure {
            use crate::agent::run::reply_triage::FormFailure;
            if let Some(report) = self.setback(
                turn,
                engine,
                super::super::progress::Setback::UnusableReply,
                sent.step,
                ui,
            )? {
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(report),
                ));
            }
            self.request_phase.recover(match failure {
                FormFailure::DroppedCall => "the previous tool call was malformed",
                FormFailure::TextCall => "tool syntax appeared as text instead of a valid call",
            });
            // A form failure is learnable: never run it, never hide it.
            self.record(
                sent.step,
                "tool_form_failure",
                json!({"kind":failure.kind()}),
            )?;
            let failed = turn
                .pending
                .get_or_insert_with(|| Pending::new(sent.context_before, reply.wire_form));
            failed.kinds.push(failure.kind().into());
            match failure {
                FormFailure::DroppedCall => {
                    turn.pending_nudges.push(
                        json!({"role":"user","content":failure.correction(),"dial_transient":true}),
                    );
                }
                FormFailure::TextCall => {
                    failed.reject(json!({"role":"assistant","content":reply.text.clone()}));
                    self.push(json!({"role":"assistant","content":reply.text}))?;
                    self.push(json!({"role":"user","content":failure.correction()}))?;
                }
            }
            turn.refund_unusable(ui);
            return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
        }
        // A tool-free yield with outstanding owned work is a runtime wait, not a reason to
        // spend another inference asking the model to poll. Explicit blockers/questions still
        // end normally; uncertain effects must stop rather than enter another continuation.
        if reply.tool_calls.is_empty()
            && !self.hermetic
            && (turn.budget.limit() == 0 || turn.budget.spent() < turn.budget.limit())
            && engine.pending_refusal().is_none()
            && !crate::agent::verify::completion::awaiting_input(&reply.text)
            && crate::agent::verify::completion::declared_outcome(&reply.text)
                .is_none_or(|outcome| outcome == TurnOutcome::Completed)
            && self.wait_for_background_event(ui)?
        {
            if !reply.text.trim().is_empty() {
                self.push(json!({"role":"assistant","content":reply.text}))?;
            }
            return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
        }
        match engine
            .recovery
            .stopped(&reply.text, !reply.tool_calls.is_empty())
        {
            crate::agent::run::turn_recovery::StopAction::Accept => {}
            crate::agent::run::turn_recovery::StopAction::Continue { reason, message } => {
                if let Some(report) = self.setback(
                    turn,
                    engine,
                    super::super::progress::Setback::NoAnswer,
                    sent.step,
                    ui,
                )? {
                    return Ok(ControlFlow::Break(
                        crate::agent::run::engine::Tick::Finished(report),
                    ));
                }
                self.request_phase.recover(reason);
                ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                    crate::agent::ui::ui_event::EventKind::Recovery,
                    format!("Recovering: {reason}."),
                ));
                if !reply.text.trim().is_empty() {
                    self.push(json!({"role":"assistant","content":reply.text}))?;
                }
                turn.pending_nudges
                    .push(json!({"role":"user","content":message,"dial_transient":true}));
                turn.refund_unusable(ui);
                return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
            }
            crate::agent::run::turn_recovery::StopAction::Incomplete(reason) => {
                return self
                    .stop_turn(ui, reason, sent.step + 1)
                    .map(crate::agent::run::engine::Tick::Finished)
                    .map(ControlFlow::Break);
            }
        }
        turn.stall_continues = 0;
        if reply.tool_calls.is_empty() {
            if let (Some(warm), Some(sent)) = (&self.cache_warm, &sent.warm_request) {
                warm.settled(sent, &reply, &self.usage_scope);
            }
            self.push(json!({"role":"assistant","content":reply.text}))?;
            self.observe_assistant_message(&reply.text, sent.step, ui);
            let accepted_live = self.flush_accepted_live_steering(ui)?;
            if self.take_steering(ui)? || accepted_live {
                turn.steering_pending = true;
                return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
            }
            if self.interrupt.load(Ordering::SeqCst) {
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(TurnReport::interrupted(
                        sent.step + 1,
                    )),
                ));
            }
            if self.exploration.active.is_some() {
                // Each reminder is a paid request and the default step limit is unbounded: a model
                // that keeps answering in text (or cannot see `rewind`) must not loop forever.
                if turn.exploration_reminders >= EXPLORATION_REMINDER_LIMIT {
                    return Ok(ControlFlow::Break(
                        crate::agent::run::engine::Tick::Finished(TurnReport {
                            text: format!(
                                "{}\n\n[Dial] Stopped: an investigation checkpoint is still open and the model answered without calling rewind after {EXPLORATION_REMINDER_LIMIT} reminders.",
                                reply.text
                            ),
                            outcome: TurnOutcome::Incomplete,
                            steps: sent.step + 1,
                            interrupted: false,
                            trajectory: None,
                        }),
                    ));
                }
                turn.exploration_reminders += 1;
                self.push(json!({"role":"user","dial_exploration_reminder":true,
                        "content":"Investigation checkpoint is active. Call rewind with your findings before finishing; the report replaces intermediate context, not actual effects or current user instructions."}))?;
                return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
            }
            if self.plan_mode && !crate::agent::verify::completion::open_work(&self.todo).is_empty()
            {
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(TurnReport {
                        text: reply.text,
                        outcome: TurnOutcome::NeedsInput,
                        steps: sent.step + 1,
                        interrupted: false,
                        trajectory: None,
                    }),
                ));
            }
            let requested = crate::agent::verify::completion::declared_outcome(&reply.text)
                .unwrap_or_else(|| {
                    if crate::agent::verify::completion::awaiting_input(&reply.text) {
                        TurnOutcome::NeedsInput
                    } else {
                        TurnOutcome::Completed
                    }
                });
            if requested == TurnOutcome::Completed && self.await_background_jobs(ui) {
                return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
            }
            match self.completion_decision(requested, &reply.text, ui) {
                crate::agent::verify::completion::Decision::Continue(note) => {
                    turn.completion_candidate = Some(reply.text.clone());
                    turn.pending_nudges.push(crate::agent::run::guidance::note(
                        crate::agent::run::guidance::Priority::Review,
                        note,
                    ));
                    return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
                }
                crate::agent::verify::completion::Decision::Incomplete(reason) => {
                    let text = crate::agent::verify::completion::rejected_report(
                        &reply.text,
                        &reason,
                        TurnOutcome::Incomplete,
                    );
                    return self.stop_turn(ui, text, sent.step + 1).map(|mut report| {
                        report.outcome = TurnOutcome::Incomplete;
                        ControlFlow::Break(crate::agent::run::engine::Tick::Finished(report))
                    });
                }
                crate::agent::verify::completion::Decision::Accept(outcome)
                    if outcome != TurnOutcome::Completed =>
                {
                    return Ok(ControlFlow::Break(
                        crate::agent::run::engine::Tick::Finished(TurnReport {
                            text: reply.text,
                            outcome,
                            steps: sent.step + 1,
                            interrupted: false,
                            trajectory: None,
                        }),
                    ));
                }
                crate::agent::verify::completion::Decision::Accept(_) => {}
            }
            let route = self.route.generation();
            if self.before_yield(backend, &turn.names, &mut turn.pending_nudges, ui)? {
                if self.route.generation() != route {
                    turn.max_tokens = backend.output_budget(turn.harness.max_tokens);
                }
                return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
            }
            // A small model that denies a capability without looking for a tool gets one
            // reminder; the catalog often holds it (an interactive terminal is `bg pty`).
            let loadable_tools = !self.hermetic
                && self.lean_completion_route()
                && self.stop_boundary().is_none()
                && self
                    .catalog()
                    .iter()
                    .any(|name| !turn.names.contains(name) && self.tool_eligible(name));
            if turn.capability_search.remind(&reply.text, loadable_tools) {
                ui.notice("Answer denies a capability without a tool search; asking once to search the catalog.");
                turn.pending_nudges.push(crate::agent::run::guidance::note(
                    crate::agent::run::guidance::Priority::Review,
                    crate::agent::verify::completion::CAPABILITY_SEARCH_REMINDER,
                ));
                return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
            }
            if let Some(message) = self.coach_completion(
                &mut turn.coach_turn,
                backend,
                turn.user_goal,
                &reply.text,
                sent.step,
                ui,
            )? {
                turn.pending_nudges.push(message);
                return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
            }
            let note = turn.coach_turn.completion_note();
            if !note.is_empty() {
                reply.text.push_str(&note);
                ui.text_delta(&note);
            }
            self.disclose_turn_changes(ui);
            if self.capture_training_requests() {
                self.training_capture.record(
                    &sent.context_before,
                    &sent.request_tools,
                    llm::assistant_message(&reply),
                    reply.wire_form,
                );
            }
            let streamed_len = reply.text.len();
            let text = self
                .step_checks
                .visual
                .summary(reply.text, self.step_checks.revision());
            if text.len() > streamed_len {
                ui.text_delta(&text[streamed_len..]);
            }
            return Ok(ControlFlow::Break(
                crate::agent::run::engine::Tick::Finished(TurnReport {
                    text,
                    outcome: TurnOutcome::Completed,
                    steps: sent.step + 1,
                    interrupted: false,
                    trajectory: None,
                }),
            ));
        }
        Ok(ControlFlow::Continue(ReceivedReply { reply, sent }))
    }
}

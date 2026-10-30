//! Round settlement: refusal, steering, guidance nudges, coaching, stop/finish handling and
//! tool-recovery capture after every call of one reply has run.
use super::*;

impl Agent<'_> {
    pub(super) fn settle_round(
        &mut self,
        turn: &mut TurnState<'_>,
        engine: &mut crate::agent::run::engine::Engine,
        backend: &mut Backend,
        schemas: &mut Vec<Value>,
        ui: &mut dyn Ui,
        round: Round,
    ) -> Result<crate::agent::run::engine::Tick<TurnReport>> {
        let Round {
            step,
            single_call,
            reply,
            context_before,
            request_tools,
            temperature,
            max_tokens,
            seed,
            outcome,
            ..
        } = round;
        let ToolOutcome {
            narration_warning,
            short_circuited,
            form_kinds,
            rejected_calls,
            execution_failed,
            observed_check_failure,
            checked: round_checked,
            failing_tests: round_failing_tests,
            unnamed_failures: round_unnamed_failures,
            stop,
            succeeded: round_succeeded,
            last_tool_failure,
            edit_failed,
            finished,
            steering: round_steering,
            portable_batch_intercepted,
        } = outcome;
        if let Some(reason) = engine.pending_refusal() {
            self.accept_steering(round_steering, ui)?;
            return self
                .stop_turn(ui, stop.unwrap_or_else(|| reason.to_owned()), step + 1)
                .map(crate::agent::run::engine::Tick::Finished);
        }
        // A deadline or cost cap reached during this round: stop before settlement spends more
        // (rollover, automatic escalation, coach review) and before another request. A final
        // answer from this round is still delivered, without further model calls.
        let boundary = self.stop_boundary();
        if finished.is_none()
            && let Some(reason) = boundary.clone()
        {
            self.accept_steering(round_steering, ui)?;
            return self
                .stop_turn(ui, reason, step + 1)
                .map(crate::agent::run::engine::Tick::Finished);
        }
        if portable_batch_intercepted && turn.rule_state.callback_retry() {
            // One inference retry per intercepted cell/batch, charged to the shared
            // portable-rule budget. Already completed/prefetched reads retain their results.
            turn.budget.give_back();
        }
        if self.accept_steering(round_steering, ui)? || self.take_steering(ui)? {
            turn.steering_pending = true;
            ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Steering, "Kept completed tool results and skipped unstarted old actions; replanning with your new instruction."));
            return Ok(crate::agent::run::engine::Tick::Again);
        }
        if portable_batch_intercepted {
            if !turn.rule_state.callback_retry() {
                let reason = if turn.rule_state.callback_pending() {
                    "Portable guidance regeneration limit reached; the intercepted callback and remaining unstarted batch actions were refused. Completed work is preserved."
                } else {
                    "Portable callback guidance could not be journaled; remaining unstarted actions were refused. Restore writable session storage before continuing."
                };
                return self
                    .stop_turn(ui, reason.into(), step + 1)
                    .map(crate::agent::run::engine::Tick::Finished);
            }
            turn.names = self.offered();
            *schemas = self.schemas(&turn.names)?;
            return Ok(crate::agent::run::engine::Tick::Again);
        }
        match self.settle_exploration(backend, ui) {
                Ok(true) => {
                    turn.names = self.offered();
                    *schemas = self.schemas(&turn.names)?;
                    turn.compaction_failed = true;
                    turn.compaction_retry_floor = None;
                    turn.context_rewritten(engine);
                    return Ok(crate::agent::run::engine::Tick::Again);
                }
                Ok(false) => {}
                Err(error) => return self.stop_turn(ui, format!(
                    "Investigation transition did not receive a successful durable acknowledgement: {error:#}. Automatic continuation stopped; no rollback or retry is implied."),
                    step + 1).map(crate::agent::run::engine::Tick::Finished),
            }
        if boundary.is_none()
            && let Some(requested) = self.pending_context_rollover.take()
        {
            let request = Chat {
                messages: context_before,
                tools: std::mem::take(schemas),
                temperature,
                max_tokens,
                seed,
            };
            match self.rollover_context_using(backend, ui, requested, Some(&request)) {
                Ok(_) => {
                    turn.compaction_failed = true;
                    turn.compaction_retry_floor = None;
                    turn.context_rewritten(engine);
                    turn.names = self.offered();
                    *schemas = self.schemas(&turn.names)?;
                    return Ok(crate::agent::run::engine::Tick::Again);
                }
                Err(error) => {
                    return self.stop_turn(ui, format!(
                            "New-context processing stopped: {error:#}. Automatic continuation stopped; recorded work and original history are preserved."),
                            step + 1).map(crate::agent::run::engine::Tick::Finished);
                }
            }
        }
        if let Some(advice) = narration_warning {
            ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                crate::agent::ui::ui_event::EventKind::Recovery,
                advice.as_str(),
            ));
            turn.pending_nudges.push(crate::agent::run::guidance::note(
                crate::agent::run::guidance::Priority::Loop,
                advice,
            ));
        }
        // A short-circuited round neither failed nor succeeded, so it must not advance failure streaks.
        let stall_round = if short_circuited {
            crate::agent::safety::guard::Stall::None
        } else {
            turn.stall.round(!round_succeeded)
        };
        let warning = match stall_round {
            crate::agent::safety::guard::Stall::Warn(advice) => Some(advice),
            crate::agent::safety::guard::Stall::None => None,
        };
        if let Some(advice) = warning {
            turn.pending_nudges.push(crate::agent::run::guidance::note(
                crate::agent::run::guidance::Priority::Failure,
                advice,
            ));
        }
        if !short_circuited && let Some(advice) = turn.pace.round(&reply.tool_calls) {
            turn.pending_nudges.push(crate::agent::run::guidance::note(
                crate::agent::run::guidance::Priority::Loop,
                advice,
            ));
        }
        let gradient_rounds = self.config.experimental.outcome_gradient_rounds;
        if round_checked && self.step_checks.mutations > 0 {
            let score = self.step_checks.failed_checks.len()
                + round_failing_tests.len()
                + round_unnamed_failures;
            if let Some(advice) = self
                .step_checks
                .outcome_gradient
                .observe(score, gradient_rounds)
            {
                turn.pending_nudges.push(crate::agent::run::guidance::note(
                    crate::agent::run::guidance::Priority::Loop,
                    advice,
                ));
            }
        }
        if let Some(plan) = self.pending_plan.take() {
            self.push(json!({"role":"user","content":format!("Approved plan:\n{plan}\nProceed with implementation.")}))?;
        }
        turn.reliability
            .round(!form_kinds.is_empty(), round_succeeded);
        // A reached escalation step takes effect at the next turn, so this turn's prompt and
        // offer stay as they are.
        self.observe_local_round(!form_kinds.is_empty() || edit_failed, ui);
        let tool_round_failed = !form_kinds.is_empty()
            || execution_failed
            || observed_check_failure
            || self.request_phase.phase == crate::config::adaptive::Phase::Recovery;
        if !self.hermetic {
            let mut control = self.session.control_state();
            settle_reliability(
                &mut control["reliability"],
                &turn.reliability,
                &turn.measured,
                single_call,
                &self.step_checks,
            );
            self.session.set_control_state(control.clone())?;
            ui.request_policy(&control);
            // Repeated contract failures escalate the guidance instead of ending the turn; the step
            // budget still bounds the retries.
            let failures = turn.reliability.consecutive_contract_failures;
            let limit = self.config.reliability.max_contract_failures;
            if self.config.reliability.enabled
                && limit > 0
                && failures >= limit
                && failures.is_multiple_of(limit)
            {
                turn.pending_nudges.push(crate::agent::run::guidance::note(crate::agent::run::guidance::Priority::Failure, format!(
                        "[Escalation from dial] {failures} tool calls in a row broke the tool contract, and none of the rejected calls ran. Call only the offered tools, with arguments that match their schemas exactly. Do not restate the rules or explain the failure at length: make the next call now."
                    )));
            }
            let mut result = control["adaptive_policy"]["result"].clone();
            result["tool_round_failed"] = json!(tool_round_failed);
            result["next_observed_phase"] = json!(self.request_phase.phase);
            self.save_request_policy(None, result, ui)?;
        }
        if tool_round_failed {
            self.step_checks.auto_escalation.remember(
                last_tool_failure
                    .as_deref()
                    .unwrap_or("A tool round failed without a captured error."),
            );
        }
        if turn.automatic_coach_pending {
            self.record(
                step,
                "auto_coach_follow_up",
                json!({
                    "tool_round_failed": tool_round_failed,
                    "observed_check_failure": observed_check_failure,
                    "verified": false,
                    "causal_improvement_claimed": false
                }),
            )?;
            turn.automatic_coach_pending = false;
        }
        let role_configured = self.config.model_roles.contains_key("coach");
        // An accepted finish ends the round. A turn already ending is not charged for a
        // coach-model call; completion review below still applies to completed outcomes.
        let continuing = stop.is_none() && boundary.is_none() && finished.is_none();
        if continuing
            && self.step_checks.auto_escalation.round(
                tool_round_failed,
                &self.config.escalation,
                role_configured,
            )
        {
            let mut question = self.step_checks.auto_escalation.question(turn.user_goal);
            let observations = turn.coach_turn.evidence();
            if !observations.is_empty() {
                question["evidence"] = json!(observations);
            }
            let call = ToolCall {
                id: format!("auto-escalation-{step}"),
                name: "escalate".into(),
                arguments: question.clone(),
                malformed_arguments: false,
            };
            let before = self.session_cost;
            // Same approval decision as the escalate tool: Ask prompts, Readonly, plan mode and
            // rules refuse. A refusal never reaches the coach model.
            let outcome = match self.authorize_call(&call, ui) {
                Ok(()) => {
                    backend.release();
                    let env = crate::agent::workers::subagents::Env {
                        config: &self.auth_config,
                        workspace: &self.workspace,
                        ext: &self.ext,
                        layout: self.layout.as_ref(),
                        harness: &self.version.harness,
                        interrupt: &self.interrupt,
                        usage: Some(&self.usage_scope),
                        context_cap: Some(self.context_window()),
                        agents_messages: &self.agents_messages,
                        plan_mode: self.plan_mode,
                        memory: self.memory_runtime.as_ref(),
                    };
                    crate::agent::workflow::solve::ask(
                        env,
                        &question,
                        &mut self.session_cost,
                        ui,
                        self.session.id(),
                        self.hermetic,
                    )
                }
                Err(denial) => Err(anyhow::anyhow!("approval {}", denial.message())),
            };
            match outcome {
                Ok(answer) => {
                    turn.automatic_coach_pending = true;
                    turn.pending_nudges.push(
                        crate::agent::verify::auto_escalation::advice_message(
                            answer["advice"].as_str().unwrap_or_default(),
                        ),
                    );
                    ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Coach, "Automatic escalation: the coach model's unverified advice is added to the next turn.",));
                    let cost = self.session_cost.unwrap_or(0.0) - before.unwrap_or(0.0);
                    self.record(
                        step,
                        "auto_escalation",
                        json!({"status":"advised","cost":cost}),
                    )?;
                }
                Err(error) => {
                    let reason = format!("{error:#}");
                    ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                        crate::agent::ui::ui_event::EventKind::Coach,
                        format!("Automatic escalation skipped: {reason}"),
                    ));
                    self.record(
                        step,
                        "auto_escalation",
                        json!({"status":"skipped","reason":reason}),
                    )?;
                }
            }
        }
        // Live advisor: the coach reviews this round; its note reaches the next request. An
        // ending turn has no next request, so it is not charged for a live review.
        if continuing
            && let Some(note) =
                self.coach_round(&mut turn.coach_turn, backend, turn.user_goal, step, ui)?
        {
            turn.pending_nudges.push(note);
        }
        if let Some(mut message) = stop {
            if self.hermetic {
                bail!("{message}");
            }
            if let Some(failure) = last_tool_failure {
                message.push_str("\n\n");
                message.push_str(&failure);
            }
            if engine.pending_refusal().is_some() {
                return self
                    .stop_turn(ui, message, step + 1)
                    .map(crate::agent::run::engine::Tick::Finished);
            }
            // First resume the model on its own:
            // most stalls end with one fresh instruction and nobody has to type "continue".
            if turn.auto_continues < self.config.reliability.auto_continue_rounds
                && !self.interrupt.load(Ordering::SeqCst)
            {
                if let Some(report) =
                    self.setback(turn, super::progress::Setback::AutomaticContinue, step, ui)?
                {
                    return Ok(crate::agent::run::engine::Tick::Finished(report));
                }
                turn.auto_continues += 1;
                let headline = message.lines().next().unwrap_or_default().to_owned();
                ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                    crate::agent::ui::ui_event::EventKind::Continue,
                    format!(
                        "Auto-continue {}/{}: {headline}",
                        turn.auto_continues, self.config.reliability.auto_continue_rounds
                    ),
                ));
                self.record(
                    step,
                    "auto_continue",
                    json!({"attempt":turn.auto_continues,"reason":headline}),
                )?;
                self.push(json!({"role":"user","content":format!(
                        "<system-notice>\n{message}\n\nContinue. MUST resume the most recent intent with a different concrete action and complete the unfinished work. NEVER repeat the unchanged call, pause to summarize progress, or ask whether to proceed.\n</system-notice>"
                    )}))?;

                self.transition_turn(
                    turn,
                    engine,
                    super::transition::Transition::AutomaticContinue,
                    backend,
                    schemas,
                )?;
                return Ok(crate::agent::run::engine::Tick::Again);
            }
            // Warnings and call-specific withholding come first. Interactive users can
            // redirect recovery in this same turn instead of losing their active task.
            // Noninteractive UIs return None, preserving finite evaluation behavior.
            let options = vec![
                "Continue with a different approach".into(),
                "Pause and keep completed work".into(),
            ];
            if !self.interrupt.load(Ordering::SeqCst)
                && let Some(answer) = ui.ask_user("Recovery needs direction", &message, &options)
                && !answer.trim().is_empty()
                && answer != options[1]
            {
                self.push(json!({"role":"user","content":format!(
                        "Recovery direction from the user: {answer}\nResume the existing task using the evidence already returned. Correct the failed call or choose a genuinely different approach; do not repeat unchanged calls or fabricate success."
                    )}))?;

                self.transition_turn(
                    turn,
                    engine,
                    super::transition::Transition::DirectedContinue,
                    backend,
                    schemas,
                )?;
                return Ok(crate::agent::run::engine::Tick::Again);
            }
            message.push_str("\n\nThe chat and completed work are kept. Send a corrected instruction or say \"continue\" to resume.");
            return self
                .stop_turn(ui, message, step + 1)
                .map(crate::agent::run::engine::Tick::Finished);
        }
        if !form_kinds.is_empty() || execution_failed || observed_check_failure {
            self.request_phase
                .recover("tool round failed; correct its observed diagnostics");
        }
        turn.names = self.offered();
        *schemas = self.schemas(&turn.names)?;
        if let Some(mut finish) = finished {
            let accepted_live = self.flush_accepted_live_steering(ui)?;
            if self.take_steering(ui)? || accepted_live {
                turn.steering_pending = true;
                return Ok(crate::agent::run::engine::Tick::Again);
            }
            if self.interrupt.load(Ordering::SeqCst) {
                return Ok(crate::agent::run::engine::Tick::Finished(
                    TurnReport::interrupted(step + 1),
                ));
            }
            // Past a deadline or cost cap the answer is delivered without another coach call.
            let reviewable = self.stop_boundary().is_none();
            if finish.outcome == TurnOutcome::Completed {
                let run = reviewable && !turn.completion.reminder_spent();
                let (unverified, failures) = self.gate_unverified_edits(run, ui);
                match turn.completion.evidence(unverified.as_deref(), reviewable) {
                    crate::agent::verify::completion::EvidenceGate::Accept => {}
                    crate::agent::verify::completion::EvidenceGate::Remind(note) => {
                        turn.pending_nudges.push(crate::agent::run::guidance::note(
                            crate::agent::run::guidance::Priority::Review,
                            crate::agent::verify::completion::with_gate_failures(note, &failures),
                        ));
                        return Ok(crate::agent::run::engine::Tick::Again);
                    }
                    crate::agent::verify::completion::EvidenceGate::Disclose(disclosure) => {
                        finish.text.push_str(&disclosure);
                    }
                }
                if reviewable
                    && let Some(message) = self.coach_completion(
                        &mut turn.coach_turn,
                        backend,
                        turn.user_goal,
                        &finish.text,
                        step,
                        ui,
                    )?
                {
                    turn.pending_nudges.push(message);
                    return Ok(crate::agent::run::engine::Tick::Again);
                }
                finish.text.push_str(&turn.coach_turn.completion_note());
                finish.text = self
                    .step_checks
                    .visual
                    .summary(finish.text, self.step_checks.revision());
                if self.capture_training_requests() {
                    self.training_capture.record(
                        &context_before,
                        &request_tools,
                        llm::assistant_message(&reply),
                    );
                }
            }
            if self.external_thinking_instruction().is_some() {
                finish.text =
                    crate::agent::modes::external_thinking::strip_scratchpad(&finish.text);
            }
            self.push(json!({"role":"assistant","content":finish.text}))?;
            ui.text_delta(&finish.text);
            return Ok(crate::agent::run::engine::Tick::Finished(TurnReport {
                text: finish.text,
                outcome: finish.outcome,
                steps: step + 1,
                interrupted: false,
                trajectory: None,
            }));
        }
        if self.interrupt.load(Ordering::SeqCst) {
            return Ok(crate::agent::run::engine::Tick::Finished(
                TurnReport::interrupted(step + 1),
            ));
        }
        if self.take_steering(ui)? {
            turn.steering_pending = true;
        }
        if form_kinds.is_empty() && !execution_failed && self.capture_training_requests() {
            self.training_capture.record(
                &context_before,
                &request_tools,
                llm::assistant_message(&reply),
            );
        }
        if !form_kinds.is_empty() {
            let failed = turn
                .pending
                .get_or_insert_with(|| Pending::new(context_before));
            failed.kinds.extend(form_kinds);
            for rejected in rejected_calls {
                failed.reject(rejected);
            }
        } else if !execution_failed
            && let (Some(failed), Some(store), Some((id, _, _))) =
                (turn.pending.take(), self.store, &self.trajectory)
        {
            // Corrected turn: every call validated and ran without error.
            match store.tool_recovery(*id, self.version.id, &json!({
                        "context": failed.context, "tools": request_tools, "corrected": llm::assistant_message(&reply),
                        "failure_kinds": failed.kinds, "rejected": failed.rejected,
                        "project_root": self.workspace.root().to_string_lossy(),
                    })) {
                    Ok(true) => {
                        if self.record(step, "tool_recovery", json!({"failure_kinds":failed.kinds})).is_err() {
                            ui.diagnostic("Tool recovery was captured, but its optional learning event could not be recorded; continuing.");
                        }
                    }
                    Ok(false) => {} // Duplicate or oversized learning sample, not a failed tool call.
                    Err(_) => ui.diagnostic("Optional tool-recovery capture unavailable; continuing the task."),
                }
        }

        Ok(crate::agent::run::engine::Tick::Again)
    }
}

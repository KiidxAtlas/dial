//! Settle committed effects, evidence and recovery before history publication.
use super::*;

impl Agent<'_> {
    pub(super) fn settle_call<'reply>(
        &mut self,
        turn: &mut TurnState<'_>,
        context: &mut DispatchContext<'_>,
        request: &RoundRequest<'_>,
        batch: &mut BatchState,
        executed: ExecutedCall<'reply>,
        outcome: &mut ToolOutcome,
    ) -> Result<SettledCall<'reply>> {
        let step = request.step;
        let single_call = request.single_call;
        let planning_required = request.planning_required;
        let reply = request.reply;
        let engine = &mut *context.engine;
        let ui = &mut *context.ui;
        let BatchState {
            parallel,
            prerequisites,
            ..
        } = batch;
        let ExecutedCall {
            call,
            mut output,
            image,
            contract_failed,
            recovery_blocked,
            controller_blocked,
            tool_seconds,
            todo_before,
            observed_sources,
            target_sources,
        } = executed;
        let status = if controller_blocked {
            tools::ToolStatus::Blocked
        } else {
            tools::ToolStatus::from_output(&output)
        };
        if matches!(call.name.as_str(), "bash" | "eval") {
            self.note_preserved_input_changes(&mut output)?;
        }
        if !contract_failed
            && !controller_blocked
            && !recovery_blocked
            && !matches!(call.name.as_str(), "todo" | "finish" | "ask" | "tool_load")
        {
            self.step_checks.observations += 1;
        }
        if controller_blocked {
            output.execution.status = Some(status);
            output.execution.effects = tools::surface::outcome::Effects::None;
        }
        // A controller retry/loop refusal did not attempt a native mutation.
        if !recovery_blocked {
            prerequisites.observe(&self.workspace, self.edit_mode(), call, &output, &self.ext);
        }
        self.record(step, "tool_outcome", json!({"name":call.name,"id":call.id,"status":status,"effects":tools::surface::outcome::effects(&output),"duration_ms":tool_seconds*1000.0}))?;
        let callback_intercepted = std::mem::take(&mut self.portable_callback_intercepted);
        if callback_intercepted || self.ext.contains(&call.name) {
            turn.rule_state =
                crate::agent::safety::rules::State::restore(&self.session.control_state());
            outcome.portable_batch_intercepted |=
                turn.rule_state.callback_pending() || callback_intercepted;
        }
        if call.name == "new_context" && output.is_error {
            self.clear_context_rollover();
        }
        if crate::agent::workflow::exploration::TOOL_NAMES.contains(&call.name.as_str())
            && output.is_error
        {
            self.exploration.reject_result(call);
        }
        turn.reliability.observe(
            contract_failed,
            output.is_error,
            !matches!(
                call.name.as_str(),
                "todo"
                    | "finish"
                    | "tool_search"
                    | "tool_load"
                    | "context_notes"
                    | "new_context"
                    | "checkpoint"
                    | "rewind"
            ),
            turn.turn_started
                .elapsed()
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
        );
        self.observe_experience(call, &output, observed_sources, ui);
        if output.is_error {
            outcome.last_tool_failure = Some(format!(
                "Most recent tool error (`{}`):\n{}",
                call.name,
                crate::agent::safety::guard::clip_middle(&output.text, 1200)
            ));
        }
        outcome.edit_failed |= output.is_error && matches!(call.name.as_str(), "edit" | "write");
        if matches!(call.name.as_str(), "edit" | "write") {
            self.observe_local_edit(!output.is_error);
            turn.last_edit_refused = output.is_error;
        }
        if call.name == "todo" && !output.is_error {
            self.observe_local_plan_change();
        }
        let is_check = crate::agent::memory::selfplay::is_check_call(call);
        if call.name == "bash"
            && let Some(command) = call.arguments["command"].as_str()
        {
            if output
                .execution
                .process
                .as_ref()
                .is_some_and(|process| process.started)
            {
                self.efficiency_metrics.full_builds +=
                    u64::from(crate::agent::workflow::efficiency::broad_check(command));
            }
            if is_check && tool_seconds >= 5.0 {
                let key = format!(
                    "{}\0{command}",
                    call.arguments["cwd"].as_str().unwrap_or_default()
                );
                let repeated =
                    self.step_checks.slow_checks.get(&key) == Some(&self.step_checks.mutations);
                if self.step_checks.slow_checks.len() < 8
                    || self.step_checks.slow_checks.contains_key(&key)
                {
                    self.step_checks
                        .slow_checks
                        .insert(key, self.step_checks.mutations);
                }
                let note = if repeated {
                    "The same slow check was repeated with no intervening native source edit observed. Reuse its retained output for diagnostics; rerun only for changed inputs or a new verification need. This hint is not proof that external inputs stayed unchanged."
                } else {
                    "This check took more than five seconds. Use its retained output for diagnostics and prefer a scoped check during iteration; do not rerun a build just to filter or recover its output."
                };
                output.text.push_str(&format!("\n[Check cost] {note}"));
            }
        }
        // Runtime check receipts record calls that actually executed. They are local algorithm
        // checks, not blanket verification of all workspace edits at the cell's final revision.
        if call.name == "eval" {
            for passed in crate::agent::verify::completion::eval_checks(&output) {
                if passed {
                    turn.reliability.passing_checks += 1;
                } else {
                    turn.reliability.failing_checks += 1;
                }
            }
        }
        let mut shell_check_passed = false;
        let mut shell_check_run = None;
        let check_failed = is_check && crate::agent::verify::verification::check_failed(&output);
        if is_check
            && let Some(process) = output.process_outcome()
            && process.started
            && !process.cancelled
            && (process.exit_code.is_some() || process.timed_out || process.signal.is_some())
        {
            let passed = !(output.is_error
                || check_failed
                || process.exit_code != Some(0)
                || !process.output_complete);
            if passed {
                turn.reliability.passing_checks += 1;
                shell_check_passed = true;
            } else {
                turn.reliability.failing_checks += 1;
            }
            shell_check_run = Some(passed);
        }
        outcome.observed_check_failure |= check_failed;
        self.step_checks.reproduced |= check_failed;
        if self.config.experimental.outcome_gradient_rounds > 0
            && (is_check || call.name == crate::agent::verify::verify_tool::NAME)
        {
            outcome.checked = true;
            let failing = crate::agent::run::outcome_gradient::failing_tests(&output.text);
            if failing.is_empty() && check_failed {
                outcome.unnamed_failures += 1;
            }
            outcome.failing_tests.extend(failing);
        }
        if self.completion_review_active() || self.config.escalation.enabled {
            let label = if call.name == "bash" {
                format!(
                    "bash {}",
                    crate::agent::safety::guard::clip_middle(
                        call.arguments["command"].as_str().unwrap_or(""),
                        200
                    )
                )
            } else {
                call.name.clone()
            };
            turn.coach_turn.observe(
                &label,
                &output.text,
                output.is_error || check_failed,
                is_check,
            );
        }
        self.request_phase
            .observe(&call.name, !output.is_error && !check_failed, is_check);
        if output.is_error && outcome.form_kinds.is_empty() {
            outcome.execution_failed = true;
        }
        if !output.is_error
            && call.name == "read"
            && let Some(path) = call.arguments["path"].as_str()
            && turn.prechecked_paths.contains(path)
        {
            turn.revalidated_paths.insert(path.to_owned());
        }
        if !output.is_error {
            outcome.succeeded = true;
        }
        // A mutation that ran the batch's `verify` already settled before running it.
        // Do not immediately stale that evidence by advancing the revision a second time.
        let repro_advice = self.settle_tool_mutation(call, &mut output);
        let attached_verify = matches!(call.name.as_str(), "edit" | "write")
            .then(|| crate::agent::verify::verify_tool::post_mutation_verify(&output))
            .flatten();
        let committed = !output.execution.changed_paths.is_empty();
        if let Some(check) = attached_verify {
            outcome.checked = true;
            if check["is_error"] == true {
                outcome.execution_failed = true;
                outcome.observed_check_failure |=
                    check["data"]["results"].as_array().is_some_and(|results| {
                        results
                            .iter()
                            .any(|result| result["status"] == "code_failed")
                    });
                outcome.last_tool_failure = Some(format!(
                    "verify after {} failed; the change remains applied:\n{}",
                    call.name,
                    check["text"].as_str().unwrap_or_default()
                ));
                turn.reliability.failing_checks += 1;
            } else if check["data"]["results"]
                .as_array()
                .is_some_and(|results| !results.is_empty())
            {
                turn.reliability.passing_checks += 1;
            }
        }
        // A call the controller refused (retry barrier, contract or preflight) never ran.
        if call.name == "bash" && !controller_blocked {
            if call.arguments["command"]
                .as_str()
                .is_some_and(crate::agent::safety::approval::opaque_shell_effect)
            {
                self.step_checks.uncheckpointed_shell_commands += 1;
                self.record_shell_changes(ui);
            }
        } else if !controller_blocked
            && call.name != "bash"
            && let Some(tool) = self.ext.get(&call.name)
            && tool.may_modify_workspace(&call.arguments)
        {
            self.step_checks.uncheckpointed_extension_calls += 1;
            self.step_checks
                .uncheckpointed_extension_tools
                .insert(call.name.clone());
        }
        let search_note = if self.hermetic {
            None
        } else {
            if call.name == "read" && !output.is_error {
                let targets = crate::agent::verify::caller_obligations::read_targets(
                    &self.workspace,
                    &call.arguments,
                );
                self.step_checks.visited_paths.extend(targets);
            }
            let revision = self.step_checks.revision();
            let mut effective = call.clone();
            if crate::agent::window::file_handles::expand(&self.workspace, &mut effective).is_ok() {
                self.step_checks.empty_searches.observe(
                    &effective.name,
                    &effective.arguments,
                    &output.text,
                    output.is_error,
                    revision,
                )
            } else {
                None
            }
        };
        self.step_checks
            .evidence
            .settle_call(&call.id, self.step_checks.revision());
        self.step_checks.targets.observe(
            &self.workspace,
            &call.name,
            &call.arguments,
            &output,
            self.step_checks.effect_epoch(),
            target_sources,
        );
        if committed && parallel.is_empty() {
            self.start_background_check(ui);
        }
        // Bound after this call's own shell effect, like the ledger's observations.
        if shell_check_passed {
            self.step_checks.shell_check_passed = Some(self.step_checks.revision());
        }
        if let Some(passed) = shell_check_run {
            let command = call.arguments["command"]
                .as_str()
                .unwrap_or(&call.name)
                .to_owned();
            let revision = self.step_checks.revision();
            self.step_checks
                .shell_check_runs
                .push((command, passed, revision));
        }
        let review_stamp = crate::agent::run::enrichment::Stamp {
            revision: self.step_checks.revision(),
            instruction: turn.enrichment_epoch,
        };
        turn.enrichment.invalidate(review_stamp);
        if call.name == "finish" && !output.is_error && self.step_checks.finish_accepted {
            let mut finish = crate::agent::verify::completion::Finish::parse(&call.arguments)
                .map_err(anyhow::Error::msg)?;
            if let Some(data) = output.data.as_ref()
                && matches!(
                    data["accepted_outcome"].as_str(),
                    Some("incomplete" | "blocked")
                )
            {
                let reason = data["reason"]
                    .as_str()
                    .unwrap_or("Completion evidence is missing.");
                finish.outcome = if data["accepted_outcome"] == "blocked" {
                    TurnOutcome::Blocked
                } else {
                    TurnOutcome::Incomplete
                };
                finish.text = crate::agent::verify::completion::rejected_report(
                    &finish.summary,
                    reason,
                    finish.outcome,
                );
            }
            outcome.finished = Some(finish);
        }
        // The catalog identifies the name; shell prose alone cannot invent a tool or
        // authorize a replacement action. Shared foreground/worker settlement keeps the
        // original failure and unchanged-retry barrier intact.
        if call.name == "bash" {
            let advice = crate::agent::run::failure_guidance::shell_tool_recovery(
                call,
                &mut output,
                |name| self.recovery_capability(name, &turn.names),
            )
            .or_else(|| {
                crate::agent::run::failure_guidance::missing_program_recovery(call, &output)
            });
            if let Some(advice) = advice {
                output.text.push_str(&advice);
            }
        }
        if let Some(hint) = crate::agent::run::failure_guidance::command_shape_hint(call, &output) {
            output.text.push_str(&hint);
        }
        if let Some(notice) = crate::agent::run::failure_guidance::directory_request_unavailable(
            &output,
            !self.hermetic && ui.interactive_input(),
        ) {
            output.text.push_str(&notice);
        }
        // A command that outlived its bash call is only reachable through `bg`. Load it for the
        // next request (the stable offer through the `tool_load` owner) instead of asking the
        // model to spend a round on `tool_load`.
        if call.name == "bash"
            && output
                .data
                .as_ref()
                .is_some_and(|data| data["background_job"].is_string())
            && output
                .text
                .contains(crate::tools::exec::shell::BG_LOAD_HINT)
            && self.load_bg_for_background_job()
        {
            output.text = output.text.replacen(
                crate::tools::exec::shell::BG_LOAD_HINT,
                crate::tools::exec::shell::BG_OFFERED_HINT,
                1,
            );
        }
        if matches!(call.name.as_str(), "bash" | "bg") {
            let capabilities = self.launch_recovery_capabilities(&turn.names);
            if let Some(advice) = crate::agent::run::failure_guidance::shell_launch_recovery(
                call,
                &mut output,
                &capabilities,
            ) {
                output.text.push_str(&advice);
            }
        }
        let (duplicate, repeat_advice, recovery_stop) = engine.settle(
            &self.workspace,
            call,
            &mut output,
            crate::agent::run::engine::Settlement {
                check_failed,
                task_changed: todo_before != self.todo.snapshot(),
                // A recovery-checkpoint refusal is a controller retry barrier, not a call
                // contract mistake; it keeps its boundary guidance.
                contract_failed: contract_failed && !recovery_blocked,
                tool: self.ext.get(&call.name).map(|tool| tool.as_ref()),
            },
            |id| self.holds_full_result(id),
        );
        // Only admit optional work after committed facts have settled. The cache owns a
        // single best-effort worker; admission, polling and turn drop never await it.
        if !self.hermetic
            && !output.is_error
            && matches!(call.name.as_str(), "write" | "edit" | "ast_edit")
            && let (Some(cache), Some(path)) =
                (self.codebase.as_ref(), call.arguments["path"].as_str())
            && turn.reviewed_paths.len() < 4
            && turn.reviewed_paths.insert(path.to_owned())
        {
            turn.enrichment
                .submit(cache, vec![path.to_owned()], review_stamp);
        }
        let is_duplicate = duplicate.is_some();
        // A read pointer re-presents evidence the model already holds: it is neither a
        // withheld call nor a new observation.
        let is_pointer = duplicate
            .as_deref()
            .is_some_and(crate::agent::workflow::investigation::is_unchanged_read);
        if is_pointer {
            // An unchanged re-read is not an intervention, but it is still a repeated call.
            self.record(step, "repeated_call", json!({"name":call.name}))?;
        }
        if is_duplicate && !is_pointer {
            self.record(step, "investigation_gate", json!({"name":call.name}))?;
            ui.diagnostic(&format!("Recovery checkpoint: unchanged observation in this `{}` call; it is withheld while corrected arguments and narrower evidence remain available.", call.name));
        }
        if outcome.stop.is_none() {
            outcome.stop = recovery_stop;
        }
        let todo_advice = turn.todo_recovery.observe(
            call,
            output.is_error,
            todo_before != self.todo.snapshot(),
            &output.text,
            &self.todo,
        );
        if output.is_error {
            self.efficiency_metrics.failed_tool_results += 1;
            // The first failure speaks for itself; coaching starts when the same tool
            // fails again in a row, so the model is not lectured on every error. Patch
            // syntax errors are the exception: the fix is to the call, never the file.
            let advice = todo_advice.as_deref().or_else(|| {
                (turn.stall.streak(&call.name) >= 1
                    || matches!(call.name.as_str(), "edit" | "ast_edit"))
                .then(|| crate::learn::task_policy::recovery_hint(&call.name, &output.text))
                .flatten()
            });
            if let Some(advice) = advice {
                output.text.push_str("\n\n");
                output.text.push_str(advice);
            }
        }
        if call.name == "todo"
            && !output.is_error
            && let Some(note) = crate::agent::workflow::todo_recovery::solo_update_note(
                &reply.tool_calls,
                planning_required,
                single_call,
                turn.solo_todo_noted,
            )
        {
            turn.solo_todo_noted = true;
            output.text.push_str("\n\n");
            output.text.push_str(note);
        }
        if let Some(note) = crate::agent::workflow::todo_recovery::refused_edit_done_note(
            call,
            !output.is_error,
            turn.last_edit_refused,
        ) {
            output.text.push_str("\n\n");
            output.text.push_str(note);
        }
        if let Some(advice) = repeat_advice {
            ui.diagnostic(&format!("Recovery checkpoint: `{}` failed; correct its arguments or choose a different concrete action.", call.name));
            output.text.push_str("\n\n");
            output.text.push_str(&advice);
        }
        if let Some(advice) = turn.pace.call(call, &output) {
            output.text.push_str("\n\n");
            output.text.push_str(&advice);
        }
        let changed: Vec<String> = output
            .execution
            .changed_paths
            .iter()
            .map(|path| {
                path.strip_prefix(self.workspace.root())
                    .unwrap_or(path)
                    .display()
                    .to_string()
            })
            .collect();
        if shell_check_passed {
            let paths: Vec<String> = call.arguments["check_paths"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter_map(|path| self.workspace.resolve(path).ok())
                .map(|path| self.workspace.rel(&path))
                .collect();
            turn.edit_churn.checked_paths(&paths);
        }
        let check_passed = call.name == crate::agent::verify::verify_tool::NAME
            && !output.is_error
            && output.data.as_ref().is_some_and(|data| {
                data["results"].as_array().is_some_and(|results| {
                    !results.is_empty()
                        && results.iter().all(|result| {
                            result["status"] == "passed"
                                && result["ran"] == true
                                && result["reused"] == false
                        })
                })
            });
        if let Some(advice) = turn.edit_churn.observe(&changed, check_passed) {
            ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                crate::agent::ui::ui_event::EventKind::Recovery,
                "Repeated edits to one file without a passing check; asking the model to reconsider",
            ));
            output.text.push_str("\n\n");
            output.text.push_str(&advice);
        }
        if output.is_error {
            turn.stall.note_error(&call.name, &output.text);
        }
        match turn.stall.call(call, output.is_error) {
            crate::agent::safety::guard::Stall::Warn(advice) => {
                if self.config.reliability.enabled {
                    engine.arm(call, &advice);
                    ui.diagnostic(&format!("Recovery checkpoint: `{}` keeps failing; correct its arguments or choose a different concrete action.", call.name));
                } else {
                    ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                        crate::agent::ui::ui_event::EventKind::Recovery,
                        advice.as_str(),
                    ));
                }
                output.text.push_str("\n\n");
                output.text.push_str(&advice);
            }
            crate::agent::safety::guard::Stall::None => {}
        }
        Ok(SettledCall {
            call,
            output,
            image,
            tool_seconds,
            status,
            duplicate,
            is_duplicate,
            search_note,
            repro_advice,
        })
    }

    /// Close committed effects exactly once, before any verification consumes their revision.
    pub(in crate::agent) fn settle_tool_mutation(
        &mut self,
        call: &ToolCall,
        output: &mut ToolOutput,
    ) -> Option<&'static str> {
        if let Some(checkpoints) = &self.checkpoints {
            checkpoints.settle_changes();
        }
        // Committed paths count even after partial failure. The name list covers writers
        // that commit without reporting paths.
        if !output.execution.changed_paths.is_empty()
            || (!output.is_error
                && output.execution.effects != tools::surface::outcome::Effects::None
                && matches!(call.name.as_str(), "write" | "edit" | "ast_edit" | "solve"))
        {
            if !self.verify_batch.settle_once(&mut output.execution) {
                return None;
            }
            self.step_checks.mutations += 1;
            self.behavioral_checks.clear();
            if !self.hermetic
                && let Some(path) = call.arguments["path"].as_str()
            {
                self.step_checks.visual.changed(Path::new(path));
            }
            if self.repro_first_fires(call, crate::config::types::ReproFirst::Advise) {
                return Some(crate::agent::verify::repro_first::ADVICE);
            }
        }
        None
    }
}

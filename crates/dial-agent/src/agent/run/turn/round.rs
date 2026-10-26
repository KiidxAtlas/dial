//! One reply's tool round: run every admitted call in order, then settle the round.
use super::*;
use std::ops::ControlFlow;

impl Agent<'_> {
    pub(super) fn execute_round(
        &mut self,
        turn: &mut TurnState<'_>,
        engine: &mut crate::agent::run::engine::Engine,
        backend: &mut Backend,
        schemas: &mut Vec<Value>,
        ui: &mut dyn Ui,
        mut round: Round,
    ) -> Result<crate::agent::run::engine::Tick<TurnReport>> {
        // Unconsumed speculative reads are dropped when this round ends, on every exit path.
        let _speculative_batch = self.workspace.speculation.batch();
        if let ControlFlow::Break(tick) =
            self.dispatch_calls(turn, engine, backend, schemas, ui, &mut round)?
        {
            return Ok(tick);
        }
        engine.research_round();
        self.settle_round(turn, engine, backend, schemas, ui, round)
    }

    /// Runs the reply's calls in order and records what they produced in `round.outcome`.
    fn dispatch_calls(
        &mut self,
        turn: &mut TurnState<'_>,
        engine: &mut crate::agent::run::engine::Engine,
        backend: &mut Backend,
        schemas: &mut [Value],
        ui: &mut dyn Ui,
        round: &mut Round,
    ) -> Result<ControlFlow<crate::agent::run::engine::Tick<TurnReport>>> {
        let Round {
            step,
            single_call,
            planning_required,
            ..
        } = *round;
        let reply = &round.reply;
        let compatibility_loaded = &round.compatibility_loaded;
        let repeat_streak = &round.repeat_streak;
        let streamed = &mut round.streamed;
        let narration_warning = engine.narration.observe(reply);
        self.edit_preview_intercepted = false;
        self.push(crate::tools::surface::contracts::assistant_message(
            reply, schemas, &self.ext,
        ))?;
        let (mut form_kinds, mut rejected_calls, mut execution_failed) =
            (Vec::new(), Vec::new(), false);
        let mut observed_check_failure = false;
        // `experimental.outcome_gradient_rounds`: checks observed this round and their failures.
        let mut round_checked = false;
        let mut round_failing_tests = std::collections::BTreeSet::new();
        let mut round_unnamed_failures = 0usize;
        let mut parallel = std::collections::VecDeque::new();
        let mut stop: Option<String> = None;
        let mut round_succeeded = false;
        let mut last_tool_failure = None;
        let mut edit_failed = false;
        let mut finished = None;
        let mut round_steering = Vec::new();
        let mut portable_batch_intercepted = false;
        let mut boundary: Option<String> = None;
        if (!single_call || reply.tool_calls.len() <= 1) && engine.pending_refusal().is_none() {
            for call in reply.tool_calls.iter().skip(1) {
                if call_form_error(call, schemas, &turn.names).is_none()
                    && engine.rejection_at(&self.workspace, call).is_none()
                {
                    self.warm_speculative_read(call);
                }
            }
        }
        // Past the stronger guidance, an identical repeat is answered with its earlier result
        // instead of running again. Only the repeated call set qualifies; anything else runs.
        let duplicates = repeat_streak
            .as_ref()
            .filter(|repeat| {
                repeat.count > crate::agent::safety::stream_guards::TOOL_CALL_LOOP_THRESHOLD
            })
            .and_then(|repeat| repeat.short_circuit(&reply.tool_calls));
        if duplicates.is_some() {
            ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Recovery, "Repeated tool call not executed; the earlier identical result is already in context"));
        }
        for (index, call) in reply.tool_calls.iter().enumerate() {
            if parallel.is_empty()
                && let Some(reason) = engine.pending_refusal()
            {
                self.push(crate::tools::surface::outcome::skipped(&call.id, reason))?;
                continue;
            }
            if self.interrupt.load(Ordering::SeqCst) {
                self.close_interrupted_batch(&reply.tool_calls[index..], &mut parallel)?;
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(TurnReport::interrupted(step + 1)),
                ));
            }
            // A deadline or cost cap reached during generation or an earlier call: unstarted calls
            // are answered without running and `settle_round` stops the turn. Results of a group
            // that already ran are still delivered.
            if parallel.is_empty()
                && let Some(reason) = boundary.clone().or_else(|| self.stop_boundary())
            {
                self.push(crate::tools::surface::outcome::skipped(
                    &call.id,
                    &format!("This call was not executed. {reason}"),
                ))?;
                boundary = Some(reason);
                continue;
            }
            if let Some(results) = &duplicates {
                let output = ToolOutput::ok(results[index].clone());
                ui.tool_call_with_args(&call.id, &call.name, &summarize(call), &call.arguments);
                ui.tool_result_with_id(&call.id, &call.name, &output);
                self.push(crate::tools::surface::outcome::message(&call.id, &output))?;
                continue;
            }
            if self.edit_preview_intercepted && parallel.is_empty() {
                let output = ToolOutput::err(
                    "This call was not executed: a completed, authorized edit preview failed earlier in this assistant batch. Regenerate the edit before starting further tools; completed effects and prefetched results remain intact.",
                );
                ui.tool_call_with_args(&call.id, &call.name, &summarize(call), &call.arguments);
                ui.tool_result_with_id(&call.id, &call.name, &output);
                self.push(crate::tools::surface::outcome::message(&call.id, &output))?;
                crate::telemetry::tool(
                    self.config,
                    self.hermetic,
                    self.session.id(),
                    &call.name,
                    0.0,
                    tools::ToolStatus::Skipped,
                );
                continue;
            }
            if portable_batch_intercepted && parallel.is_empty() {
                let output = ToolOutput::err(
                    "This call was not executed: portable guidance intercepted an earlier native-code callback in this assistant batch. Regenerate with the staged guidance before any further tool action.",
                );
                ui.tool_call_with_args(&call.id, &call.name, &summarize(call), &call.arguments);
                ui.tool_result_with_id(&call.id, &call.name, &output);
                self.push(crate::tools::surface::outcome::message(&call.id, &output))?;
                crate::telemetry::tool(
                    self.config,
                    self.hermetic,
                    self.session.id(),
                    &call.name,
                    0.0,
                    tools::ToolStatus::Skipped,
                );
                continue;
            }
            round_steering.extend(
                self.drain_steering(ui)
                    .into_iter()
                    .filter(|text| !text.trim().is_empty()),
            );
            if !round_steering.is_empty() && parallel.is_empty() {
                // Close the old call/result pair before appending the new user
                // message. Already-prefetched reads keep their actual results.
                self.push(llm::tool_message(&call.id, "This call was not executed: a newer user instruction superseded this pending action. Replan from the latest user message."))?;
                crate::telemetry::tool(
                    self.config,
                    self.hermetic,
                    self.session.id(),
                    &call.name,
                    0.0,
                    tools::ToolStatus::Skipped,
                );
                continue;
            }
            if self.exploration.requested_rewind() && parallel.is_empty() {
                let output = ToolOutput::err(
                    "This call was not executed: rewind was requested earlier in this assistant batch. Completed sibling effects and their actual results are preserved; continue after the owner commits or rejects the anchored report.",
                );
                ui.tool_call_with_args(&call.id, &call.name, &summarize(call), &call.arguments);
                ui.tool_result_with_id(&call.id, &call.name, &output);
                self.push(crate::tools::surface::outcome::message(&call.id, &output))?;
                crate::telemetry::tool(
                    self.config,
                    self.hermetic,
                    self.session.id(),
                    &call.name,
                    0.0,
                    tools::ToolStatus::Skipped,
                );
                continue;
            }
            if self.pending_context_rollover.is_some() && parallel.is_empty() {
                let output = ToolOutput::err(
                    "This call was not executed: a new context window was requested earlier in this assistant batch. Continue after the owner commits or rejects that request; previously completed effects and results are preserved.",
                );
                ui.tool_call_with_args(&call.id, &call.name, &summarize(call), &call.arguments);
                ui.tool_result_with_id(&call.id, &call.name, &output);
                self.push(crate::tools::surface::outcome::message(&call.id, &output))?;
                crate::telemetry::tool(
                    self.config,
                    self.hermetic,
                    self.session.id(),
                    &call.name,
                    0.0,
                    tools::ToolStatus::Skipped,
                );
                continue;
            }
            let todo_before = self.todo.snapshot();
            let observed_sources = self.experience_sources(call, ui);
            // Read-only batch results may already have finished before a prior
            // result arms recovery. Consume those results honestly; never label
            // a prefetched operation as unexecuted or shift its result to another call.
            let form = (if parallel.is_empty() && let Some(reason) = engine.rejection_at(&self.workspace, call) {
                    Some(("recovery_checkpoint", reason))
                } else if single_call && reply.tool_calls.len() > 1 {
                    Some(("batch_contract", "This request permits one tool call. The entire batch is rejected without executing it; choose one action and await its result.".into()))
                } else { call_form_error(call, schemas, &turn.names) }).map(|(kind, why)| {
                    if planning_required && call.name != "todo" && kind == "unknown_tool" {
                        (kind, format!("{} is temporarily withheld until a plan exists. This request offers only todo and finish. Create a plan with todo before requesting read, grep, bash or other tools; they return in the next round after a valid plan. If the request is already answered, call finish.", call.name))
                    } else if kind == "unknown_tool" && compatibility_loaded.contains(&call.name) {
                        (kind, format!("{} is available and has been loaded for the next request. This call was not executed. Retry using its newly supplied schema; normal approvals and sandbox restrictions still apply.", call.name))
                    } else {
                        (kind, why)
                    }
                }).or_else(|| {
                    (call.name == "finish" && index + 1 != reply.tool_calls.len()).then(|| {
                        (
                            "invalid_arguments",
                            "finish must be the last call in a batch".to_owned(),
                        )
                    })
                });
            let contract_failed = form.is_some();
            let mut image = None;
            let mut preflight = None;
            if form.is_none()
                && !self.hermetic
                && matches!(call.name.as_str(), "write" | "edit" | "ast_edit")
                && let (Some(cache), Some(path)) =
                    (self.codebase.clone(), call.arguments["path"].as_str())
            {
                if turn.prechecked_paths.insert(path.to_owned()) {
                    let path = path.to_owned();
                    let check = thread::scope(|scope| -> Result<_> {
                        let (tx, rx) = mpsc::sync_channel(1);
                        scope.spawn(move || {
                            let _ = tx.send(cache.pre_edit_context(&path));
                        });
                        receive_with_heartbeat(&rx, ui)?
                    });
                    match check {
                        Ok(Some(text)) => {
                            preflight = Some(ToolOutput {
                                execution: Default::default(),
                                data: None,
                                text,
                                is_error: true,
                            });
                        }
                        Ok(None) => {
                            turn.revalidated_paths
                                .insert(call.arguments["path"].as_str().unwrap().to_owned());
                        }
                        Err(error) => {
                            preflight = Some(ToolOutput {
                                execution: Default::default(),
                                data: None,
                                text: format!(
                                    "Could not check saved assumptions: {error}. Read the current target before retrying."
                                ),
                                is_error: true,
                            });
                        }
                    }
                } else if !turn.revalidated_paths.contains(path) {
                    preflight = Some(ToolOutput {
                        execution: Default::default(),
                        data: None,
                        text: format!(
                            "Read current {path} before retrying this edit; its saved assumptions required review."
                        ),
                        is_error: true,
                    });
                }
            }
            let controller_blocked = preflight.is_some() || form.is_some();
            let mut tool_seconds = 0.0;
            let mut output = if let Some(output) = preflight {
                ui.tool_call_with_args(&call.id, &call.name, &summarize(call), &call.arguments);
                ui.tool_result_with_id(&call.id, &call.name, &output);
                output
            } else if let Some((kind, why)) = form {
                self.metrics.tool_contract_errors += 1;
                self.record(
                    step,
                    "tool_form_failure",
                    json!({"kind":kind,"name":call.name}),
                )?;
                form_kinds.push(kind.to_owned());
                if !call.malformed_arguments {
                    rejected_calls.push(rejected_tool_call_message(call));
                }
                let output = ToolOutput {
                    execution: Default::default(),
                    data: None,
                    text: format!("invalid tool call: {why}"),
                    is_error: true,
                };
                ui.tool_call_with_args(&call.id, &call.name, &summarize(call), &call.arguments);
                ui.tool_timing(&call.name, 0.0);
                ui.tool_result_with_id(&call.id, &call.name, &output);
                output
            } else {
                ui.tool_call_with_args(&call.id, &call.name, &summarize(call), &call.arguments);
                self.record(
                    step,
                    "tool_call",
                    json!({"name":call.name,"input":call.arguments}),
                )?;
                if self.enabled_tools.contains(&call.name)
                    || self.discovered_tools().contains(&call.name)
                    || self.tool_routing.selected.contains(&call.name)
                {
                    self.metrics.dynamic_tools_used += 1;
                }
                if !single_call
                        && parallel.is_empty()
                        // Rewrites must pass the serial controller's schema and approval checks.
                        && !crate::agent::safety::hooks::wanted_for_tool(
                            self.tool_hooks(),
                            self.hook_plugins().as_deref(),
                            "pre_tool_use",
                            &call.name,
                        )
                        && crate::agent::safety::approval::parallel_read(&self.ext, &call.name, &call.arguments)
                        && !crate::agent::safety::approval::denied_by_rule(&self.always, call, self.workspace.root())
                {
                    let count = reply.tool_calls[index..]
                        .iter()
                        .take(self.config.task.max_concurrency)
                        .take_while(|c| {
                            !crate::agent::safety::hooks::wanted_for_tool(
                                self.tool_hooks(),
                                self.hook_plugins().as_deref(),
                                "pre_tool_use",
                                &c.name,
                            ) && crate::agent::safety::approval::parallel_read(
                                &self.ext,
                                &c.name,
                                &c.arguments,
                            ) && !crate::agent::safety::approval::denied_by_rule(
                                &self.always,
                                c,
                                self.workspace.root(),
                            ) && call_form_error(c, schemas, &turn.names).is_none()
                                && !self.speculative_pending(c)
                                && engine.rejection_at(&self.workspace, c).is_none()
                        })
                        .count();
                    let group = &reply.tool_calls[index..index + count];
                    let mut early: Vec<Option<(ToolOutput, f64)>> = match streamed.as_mut() {
                        Some(streamed) => group.iter().map(|c| streamed.take(c, ui)).collect(),
                        None => vec![None; count],
                    };
                    let reused = early.iter().flatten().count();
                    if count > 1 || reused > 0 {
                        let rest: Vec<ToolCall> = group
                            .iter()
                            .zip(&early)
                            .filter(|(_, result)| result.is_none())
                            .map(|(c, _)| c.clone())
                            .collect();
                        let mut steering = Vec::new();
                        let mut ran = if rest.is_empty() {
                            Vec::new()
                        } else {
                            parallel_tools(
                                &self.workspace,
                                self.effective_config(),
                                &self.ext,
                                &rest,
                                &self.interrupt,
                                ui,
                                &mut steering,
                            )
                        }
                        .into_iter();
                        self.pending_tool_steering.extend(steering);
                        parallel = early
                            .iter_mut()
                            .map(|result| {
                                result
                                    .take()
                                    .or_else(|| ran.next())
                                    .expect("one result per grouped call")
                            })
                            .collect();
                        if reused > 0 {
                            ui.diagnostic(&format!("Streaming dispatch: {reused} of {count} grouped read(s) reused results started while the reply streamed."));
                        }
                    }
                }
                let (mut out, seconds, executed) = match parallel.pop_front() {
                        Some(result) => {
                            // Batched bash skips `dispatch`; record verification evidence as the serial path does.
                            if call.name == "bash" {
                                self.note_verification(call, &result.0, ui);
                            }
                            (result.0, result.1, true)
                        }
                        None if call.name == "finish" && call.arguments["outcome"] == "completed" && (execution_failed || !form_kinds.is_empty()) => {
                            (ToolOutput { execution: Default::default(),  data: None,
text: "finish completed cannot follow a failed call in the same batch; address its error first".into(), is_error: true }, 0.0, false)
                        }
                        None => {
                            // Anything executed serially may change what an early read observed.
                            if let Some(streamed) = streamed.as_mut() {
                                streamed.invalidate();
                            }
                            let started = Instant::now(); let out = self.dispatch(backend, call, ui); (out, started.elapsed().as_secs_f64(), true)
                        }
                    };
                // Only admitted calls that reached execution count; rejected, skipped and blocked forms do not.
                if executed {
                    self.metrics.tool_trace.record_invoked(&call.name);
                    self.efficiency_metrics.tool_result(
                        call,
                        !out.execution.changed_paths.is_empty(),
                        self.workflow_started.elapsed(),
                    );
                }
                if (call.name == "browser" || call.name == "computer") && !out.is_error {
                    match crate::storage::artifact::capture_image(
                        self.workspace.artifacts(),
                        &mut out,
                    ) {
                        Ok(attachment) => image = attachment,
                        Err(e) => {
                            out = ToolOutput {
                                execution: Default::default(),
                                data: None,
                                text: format!("retaining browser screenshot: {e:#}"),
                                is_error: true,
                            }
                        }
                    }
                }
                ui.tool_timing(&call.name, seconds);
                tool_seconds = seconds;
                self.settled(call, &out);
                if call.name == "task"
                    && let Some(data) = &out.data
                {
                    self.efficiency_metrics.worker_results(data);
                }
                self.observe_tool_routing(call, &out);
                ui.tool_result_with_id(&call.id, &call.name, &out);
                out
            };
            let status = if controller_blocked {
                tools::ToolStatus::Blocked
            } else {
                tools::ToolStatus::from_output(&output)
            };
            self.record(step, "tool_outcome", json!({"name":call.name,"id":call.id,"status":status,"effects":tools::surface::outcome::effects(&output),"duration_ms":tool_seconds*1000.0}))?;
            let callback_intercepted = std::mem::take(&mut self.portable_callback_intercepted);
            if callback_intercepted || self.ext.contains(&call.name) {
                turn.rule_state =
                    crate::agent::safety::rules::State::restore(&self.session.control_state());
                portable_batch_intercepted |=
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
                        | "working_state"
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
                last_tool_failure = Some(format!(
                    "Most recent tool error (`{}`):\n{}",
                    call.name,
                    crate::agent::safety::guard::clip_middle(&output.text, 1200)
                ));
            }
            edit_failed |= output.is_error && matches!(call.name.as_str(), "edit" | "write");
            if matches!(call.name.as_str(), "edit" | "write") {
                self.observe_local_edit(!output.is_error);
            }
            if call.name == "todo" && !output.is_error {
                self.observe_local_plan_change();
            }
            let is_check = call.name == "bash"
                && call.arguments["command"]
                    .as_str()
                    .is_some_and(is_check_command);
            let mut shell_check_passed = false;
            if is_check {
                if output.is_error
                    || output.process_outcome().is_none_or(|process| {
                        process.exit_code != Some(0)
                            || !process.output_complete
                            || process.cancelled
                            || process.timed_out
                    })
                {
                    turn.reliability.failing_checks += 1;
                } else {
                    turn.reliability.passing_checks += 1;
                    shell_check_passed = true;
                }
            }
            let check_failed =
                is_check && crate::agent::verify::verification::check_failed(&output);
            observed_check_failure |= check_failed;
            self.step_checks.reproduced |= check_failed;
            if self.config.experimental.outcome_gradient_rounds > 0
                && (is_check || call.name == crate::agent::verify::check_tool::NAME)
            {
                round_checked = true;
                let failing = crate::agent::run::outcome_gradient::failing_tests(&output.text);
                if failing.is_empty() && check_failed {
                    round_unnamed_failures += 1;
                }
                round_failing_tests.extend(failing);
            }
            if self.advisor_enabled() || self.config.escalation.enabled {
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
            if output.is_error && form_kinds.is_empty() {
                execution_failed = true;
            }
            if !output.is_error
                && call.name == "read"
                && let Some(path) = call.arguments["path"].as_str()
                && turn.prechecked_paths.contains(path)
            {
                turn.revalidated_paths.insert(path.to_owned());
            }
            if !output.is_error {
                round_succeeded = true;
            }
            // Close this call's per-file revisions so `revert_edit` can detect later outside edits.
            if let Some(checkpoints) = &self.checkpoints {
                checkpoints.settle_changes();
            }
            // Committed facts decide: any tool whose transaction committed paths (LSP rename, code
            // actions, checkpointed edits) advances the mutation epoch, even when it failed after
            // a partial commit. The name list covers writers that commit without reporting paths.
            let mut repro_advice = None;
            let committed = !output.execution.changed_paths.is_empty();
            if committed
                || (!output.is_error
                    && matches!(call.name.as_str(), "write" | "edit" | "ast_edit" | "solve"))
            {
                self.step_checks.mutations += 1;
                if self.repro_first_fires(call, crate::config::types::ReproFirst::Advise) {
                    repro_advice = Some(crate::agent::verify::repro_first::ADVICE);
                }
                self.behavioral_checks.clear();
                if !self.hermetic
                    && let Some(path) = call.arguments["path"].as_str()
                {
                    self.step_checks.visual.changed(Path::new(path));
                }
            }
            if call.name == "bash" {
                if call.arguments["command"].as_str().is_some_and(|command| {
                    !crate::agent::safety::approval::safe_bash(command)
                        && !crate::agent::safety::approval::safe_review_batch(command)
                }) {
                    self.step_checks.uncheckpointed_shell_commands += 1;
                }
            } else if let Some(tool) = self.ext.get(&call.name)
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
                self.step_checks.empty_searches.observe(
                    &call.name,
                    &call.arguments,
                    &output.text,
                    output.is_error,
                    revision,
                )
            };
            self.step_checks
                .evidence
                .settle_call(&call.id, self.step_checks.revision());
            // Bound after this call's own shell effect, like the ledger's observations.
            if shell_check_passed {
                self.step_checks.shell_check_passed = Some(self.step_checks.revision());
            }
            let review_stamp = crate::agent::run::enrichment::Stamp {
                revision: self.step_checks.revision(),
                instruction: turn.enrichment_epoch,
            };
            turn.enrichment.invalidate(review_stamp);
            if call.name == "finish" && !output.is_error && self.step_checks.finish_accepted {
                finished = Some(
                    crate::agent::verify::completion::Finish::parse(&call.arguments)
                        .map_err(anyhow::Error::msg)?,
                );
            }
            let (duplicate, repeat_advice, recovery_stop) = engine.settle(
                &self.workspace,
                call,
                &mut output,
                check_failed,
                todo_before != self.todo.snapshot(),
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
            if is_duplicate && !is_pointer {
                self.record(step, "investigation_gate", json!({"name":call.name}))?;
                ui.diagnostic(&format!("Recovery checkpoint: unchanged observation in this `{}` call; it is withheld while corrected arguments and narrower evidence remain available.", call.name));
            }
            // A new, successful result is progress: the recovery ladder starts over.
            if !output.is_error
                && !check_failed
                && !is_duplicate
                && repeat_advice.is_none()
                && recovery_stop.is_none()
            {
                turn.progress.progressed();
            }
            if stop.is_none() {
                stop = recovery_stop;
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
                    turn.solo_todo_noted,
                )
            {
                turn.solo_todo_noted = true;
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
            self.record(
                step,
                if output.is_error {
                    "tool_error"
                } else {
                    "tool_result"
                },
                json!({"name":call.name,"output":truncate(&output.text, 4096)}),
            )?;
            let (artifact, retain_notice) = match self.workspace.artifacts() {
                Some(store) => match store.retain(&output.text) {
                    Ok(uri) => (Some(uri), None),
                    Err(error) => (
                        None,
                        Some(format!(
                            "\n[Complete tool evidence could not be retained: {error:#}]"
                        )),
                    ),
                },
                None => (None, None),
            };
            let limit = crate::agent::workflow::efficiency::preview_limit(
                &self.config.efficiency,
                self.context_window(),
                &call.name,
                output.is_error,
                artifact.is_some(),
            );
            let (mut body, dropped) =
                crate::agent::workflow::efficiency::visible_result(&output.text, limit, duplicate);
            let mut presentation_changed = body != output.text;
            if call.name == "bash"
                && !output.is_error
                && call.arguments["command"]
                    .as_str()
                    .is_some_and(is_check_command)
                && let Some(uri) = &artifact
                && let Some(compact) =
                    crate::agent::verify::verification::compact_success(&output.text, uri)
            {
                body = compact;
                presentation_changed = true;
            }
            if presentation_changed {
                let integrity = output.execution.output.get_or_insert_with(Default::default);
                integrity.mark_presentation_partial();
            }
            crate::telemetry::tool_output(
                self.config,
                self.hermetic,
                self.session.id(),
                &call.name,
                tool_seconds,
                status,
                &output.execution,
            );
            if let Some(uri) = &artifact {
                crate::agent::workflow::efficiency::evidence_link(&mut body, uri, dropped);
            }
            if let Some(notice) = &retain_notice {
                body.push_str(notice);
            }
            if let Some(note) = &search_note {
                body.push_str(note);
            }
            if let Some(note) = repro_advice {
                body.push_str(note);
            }
            self.efficiency_metrics.result(&output.text, &body);
            let mut message = llm::tool_message(&call.id, &body);
            crate::tools::surface::outcome::annotate(&mut message, &output);
            message["dial_evidence"] = json!({"artifact":artifact,"is_error":output.is_error});
            message["is_error"] = json!(output.is_error);
            if call.name == "ask"
                && let Some(data) = &output.data
            {
                message["dial_question_result"] = data.clone();
            }
            if call.name == "task"
                && let Some(note) =
                    crate::agent::workers::subagents::structured_result_note(&output)
            {
                turn.pending_nudges.push(note);
            }
            let clipped = output.text.len() > limit
                || output.text.contains("output omitted")
                || output.text.contains("bytes clipped");
            if clipped {
                let mut state = self.session.control_state();
                let mut uris = state["truncated_artifacts"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if let Some(uri) = &artifact
                    && !uris.contains(&json!(uri))
                {
                    uris.push(json!(uri));
                }
                for uri in crate::storage::artifact::uris(&output.text) {
                    if !uris.contains(&json!(uri)) {
                        uris.push(json!(uri));
                    }
                }
                state["truncated_artifacts"] = json!(uris);
                self.session.set_control_state(state)?;
            }
            if call.name == "read" && !output.is_error && !is_duplicate {
                let mut observation = call.arguments.clone();
                if let Some(args) = observation.as_object_mut() {
                    args.remove("i");
                }
                message["dial_evidence"]["observation"] = json!(observation.to_string());
            }
            if let Some(image) = image {
                message["dial_images"] = json!([image]);
            }
            if call.name == "ask"
                && let Some(images) = output
                    .data
                    .as_ref()
                    .and_then(|data| data["dial_images"].as_array())
                && !images.is_empty()
            {
                message["dial_images"] = json!(images);
            }
            crate::agent::workflow::questions::project_callbacks(
                &mut message,
                std::mem::take(&mut self.callback_question_results),
                limit,
            );
            if call.name == "read"
                && !output.is_error
                && !self.hermetic
                && let Some(path) = call.arguments["path"].as_str()
            {
                let path = Path::new(path);
                if llm::images::is_image(path) {
                    let authorized =
                        llm::images::authorized_user_reference(&self.session.messages(), path);
                    let attachment = if let Some(image) = authorized {
                        Ok(image)
                    } else {
                        self.workspace
                            .resolve(path.to_string_lossy().as_ref())
                            .map_err(anyhow::Error::msg)
                            .and_then(|absolute| {
                                let relative = absolute
                                    .strip_prefix(self.workspace.root())
                                    .context("image resolves outside the project")?;
                                llm::images::attachment(self.workspace.root(), relative)
                            })
                    };
                    match attachment {
                        Ok(image) => message["dial_images"] = json!([image]),
                        Err(error) => {
                            ui.notice(&format!("Image attachment unavailable: {error:#}"))
                        }
                    }
                }
            }
            if !self.hermetic
                && message.get("dial_images").is_some()
                && backend.accepts_image_input()
            {
                self.step_checks.visual.captured(
                    self.step_checks.revision(),
                    call.name == "browser" || call.name == "computer",
                );
            }
            for reference in message["dial_images"].as_array().into_iter().flatten() {
                ui.image(reference);
            }
            if crate::agent::workflow::exploration::TOOL_NAMES.contains(&call.name.as_str())
                && !output.is_error
            {
                message["dial_exploration_result"] = json!(true);
            }
            self.push(message)?;
            // A changed history snapshot may make a previously dead-ended compaction fit.
            turn.compaction_failed = false;
            let (tokens, estimated) = self.context_usage();
            ui.context_usage(tokens, estimated);
        }
        round.outcome = ToolOutcome {
            narration_warning,
            short_circuited: duplicates.is_some(),
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
        };
        Ok(ControlFlow::Continue(()))
    }

    /// Answers every call an interrupt left without a result. Reads that already finished in a
    /// parallel group keep their actual output; every other call is recorded as not run, with no
    /// effects, so the next turn does not have to re-inspect state it never touched.
    /// `repair_incomplete_tools` stays the backstop for anything still unanswered.
    fn close_interrupted_batch(
        &mut self,
        pending: &[ToolCall],
        parallel: &mut std::collections::VecDeque<(ToolOutput, f64)>,
    ) -> Result<()> {
        for call in pending {
            let message = match parallel.pop_front() {
                Some((output, _)) => {
                    let limit = crate::agent::workflow::efficiency::preview_limit(
                        &self.config.efficiency,
                        self.context_window(),
                        &call.name,
                        output.is_error,
                        false,
                    );
                    let (body, _) = crate::agent::workflow::efficiency::visible_result(
                        &output.text,
                        limit,
                        None,
                    );
                    let mut message = llm::tool_message(&call.id, &body);
                    crate::tools::surface::outcome::annotate(&mut message, &output);
                    message
                }
                None => crate::tools::surface::outcome::skipped(&call.id, INTERRUPTED_BEFORE_START),
            };
            self.push(message)?;
        }
        self.session.repair_incomplete_tools()?;
        Ok(())
    }
}

/// The result of a call the user interrupted before it started or before its result was used.
/// Every such call is read-only or never ran, so it changed nothing.
const INTERRUPTED_BEFORE_START: &str = "This call was not run to completion: the user interrupted the turn before its result was delivered. It made no changes.";

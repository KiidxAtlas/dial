//! Execute one admitted call, retaining launch-time and controller facts for settlement.
use super::*;

impl Agent<'_> {
    pub(super) fn dispatch_one<'reply>(
        &mut self,
        turn: &mut TurnState<'_>,
        context: &mut DispatchContext<'_>,
        request: &RoundRequest<'reply>,
        index: usize,
        batch: &mut BatchState,
        outcome: &mut ToolOutcome,
    ) -> Result<ExecutedCall<'reply>> {
        let RoundRequest {
            step,
            single_call,
            planning_required,
            reply,
            compatibility_loaded,
            argument_routes,
        } = *request;
        let call = &reply.tool_calls[index];
        let provenance = crate::agent::safety::approval::CallProvenance::from_reply(reply);
        let engine = &mut *context.engine;
        let backend = &mut *context.backend;
        let schemas = context.schemas;
        let ui = &mut *context.ui;
        let streamed = &mut *context.streamed;
        let BatchState {
            parallel,
            prerequisites,
            launch_sources,
            ..
        } = batch;
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
                } else if kind == "schema" && let Some((keys, owners)) = argument_routes.get(&call.id) {
                    let keys = keys.iter().map(|key| format!("`{key}`")).collect::<Vec<_>>().join(", ");
                    let owners = owners.iter().map(|name| format!("`{name}`")).collect::<Vec<_>>().join(" or ");
                    (kind, format!("`{}` does not accept {keys} ({why}). {owners} accepts {keys} and has been loaded for the next request; this call was not executed. Call {owners} directly with its own schema; normal approvals and sandbox restrictions still apply.", call.name))
                } else if kind == "schema" && compatibility_loaded.contains(&call.name) {
                    let schema = schemas
                        .iter()
                        .find(|s| s["function"]["name"] == call.name)
                        .and_then(|s| s.pointer("/function/parameters"))
                        .map(|p| p.to_string())
                        .unwrap_or_default();
                    (kind, format!("{} was loaded this round, but these arguments do not match its schema ({why}); this call was not executed. Retry with arguments for this schema: {schema}", call.name))
                } else if kind == "schema" && let Some(help) = self.parameter_help(&call.name) {
                    (kind, format!("{why}\nThis call was not executed. {help}"))
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
        let recovery_blocked = form
            .as_ref()
            .is_some_and(|(kind, _)| *kind == "recovery_checkpoint");
        let mut image = None;
        let mut preflight = None;
        if form.is_none() && parallel.is_empty() {
            preflight = self.native_edit_preflight(turn, call, ui);
        }
        let controller_blocked = preflight.is_some() || form.is_some();
        let mut tool_seconds = 0.0;
        // Batched launches retain the snapshot captured before their group was dispatched.
        // A hook may rewrite the command, so no source identity is credited for that path.
        let target_sources = launch_sources.remove(&call.id).unwrap_or_else(|| {
            if crate::agent::safety::hooks::wanted_for_tool(
                self.tool_hooks(),
                "pre_tool_use",
                &call.name,
            ) {
                Default::default()
            } else {
                self.step_checks
                    .targets
                    .prepare(&self.workspace, &call.name, &call.arguments)
            }
        });
        let output = if let Some(output) = preflight {
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
            outcome.form_kinds.push(kind.to_owned());
            if !call.malformed_arguments {
                outcome
                    .rejected_calls
                    .push(rejected_tool_call_message(call));
            }
            let output = ToolOutput {
                execution: Default::default(),
                data: None,
                text: format!("invalid tool call: {why}"),
                is_error: true,
            };
            ui.tool_call_with_args(&call.id, &call.name, &summarize(call), &call.arguments);
            ui.tool_timing(&call.id, &call.name, 0.0);
            ui.tool_result_with_id(&call.id, &call.name, &output);
            output
        } else {
            if parallel.is_empty() {
                ui.tool_call_with_args(&call.id, &call.name, &summarize(call), &call.arguments);
            }
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
            if self.config.experimental.dependency_dispatch
                && !reply.text_calls_lifted
                && !self.hermetic
                && !single_call
                && parallel.is_empty()
            {
                let mut group = self.dependency_wave(&reply.tool_calls[index..]);
                let mut admitted = 0;
                for candidate in &group {
                    if call_form_error(candidate, schemas, &turn.names).is_some()
                        || engine.rejection_at(&self.workspace, candidate).is_some()
                        || prerequisites
                            .rejection(&self.workspace, self.edit_mode(), &self.ext, candidate)
                            .is_some()
                        || (admitted > 0
                            && self.native_edit_preflight(turn, candidate, ui).is_some())
                    {
                        break;
                    }
                    admitted += 1;
                }
                group.truncate(admitted);
                if group.len() > 1 {
                    self.drain_background_check(ui, true);
                    if let Some(streamed) = streamed.as_mut() {
                        streamed.invalidate();
                    }
                    for candidate in group.iter().skip(1) {
                        ui.tool_call_with_args(
                            &candidate.id,
                            &candidate.name,
                            &summarize(candidate),
                            &candidate.arguments,
                        );
                    }
                    for candidate in group.iter().skip(1) {
                        let sources = if crate::agent::safety::hooks::wanted_for_tool(
                            self.tool_hooks(),
                            "pre_tool_use",
                            &candidate.name,
                        ) {
                            Default::default()
                        } else {
                            self.step_checks.targets.prepare(
                                &self.workspace,
                                &candidate.name,
                                &candidate.arguments,
                            )
                        };
                        launch_sources.insert(candidate.id.clone(), sources);
                    }
                    let read_reuse = engine.reusable_reads(|id| self.holds_full_result(id));
                    let outputs =
                        self.dispatch_dependency_wave(backend, &group, ui, Some(&read_reuse));
                    for (candidate, (output, seconds)) in group.iter().zip(&outputs) {
                        ui.tool_timing(&candidate.id, &candidate.name, *seconds);
                        ui.tool_result_with_id(&candidate.id, &candidate.name, output);
                    }
                    *parallel = outputs.into();
                }
            }
            if !single_call
                    && !reply.text_calls_lifted
                    && parallel.is_empty()
                    // Rewrites must pass the serial controller's schema and approval checks.
                    && !crate::agent::safety::hooks::wanted_for_tool(
                        self.tool_hooks(),
                        "pre_tool_use",
                        &call.name,
                    )
                    && crate::agent::safety::approval::parallel_read(&self.ext, &call.name, &call.arguments)
                    && !crate::agent::safety::approval::denied_by_rule(&self.always, call, self.workspace.root())
            {
                let count = reply.tool_calls[index..]
                    .iter()
                    .take(self.config.task.parallel_read_limit())
                    .take_while(|c| {
                        !crate::agent::safety::hooks::wanted_for_tool(
                            self.tool_hooks(),
                            "pre_tool_use",
                            &c.name,
                        ) && (self.pending_check.is_none() || c.name != "bash")
                            && crate::agent::safety::approval::parallel_read(
                                &self.ext,
                                &c.name,
                                &c.arguments,
                            )
                            && !crate::agent::safety::approval::denied_by_rule(
                                &self.always,
                                c,
                                self.workspace.root(),
                            )
                            && call_form_error(c, schemas, &turn.names).is_none()
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
                    for (position, (batched, result)) in group.iter().zip(&early).enumerate() {
                        if let Some((output, seconds)) = result {
                            if position > 0 {
                                ui.tool_call_with_args(
                                    &batched.id,
                                    &batched.name,
                                    &summarize(batched),
                                    &batched.arguments,
                                );
                            }
                            ui.tool_timing(&batched.id, &batched.name, *seconds);
                            ui.tool_result_with_id(&batched.id, &batched.name, output);
                        }
                    }
                    let rest: Vec<ToolCall> = group
                        .iter()
                        .zip(&early)
                        .filter(|(_, result)| result.is_none())
                        .map(|(c, _)| c.clone())
                        .collect();
                    let mut steering = Vec::new();
                    let read_reuse = engine.reusable_reads(|id| self.holds_full_result(id));
                    let mut ran = if rest.is_empty() {
                        Vec::new()
                    } else {
                        parallel_tools(
                            ParallelToolContext {
                                workspace: &self.workspace,
                                config: self.effective_config(),
                                ext: &self.ext,
                                interrupt: &self.interrupt,
                                ui,
                                pending_steering: &mut steering,
                            },
                            &rest,
                            Some(&call.id),
                            Some(&read_reuse),
                        )
                    }
                    .into_iter();
                    self.pending_tool_steering.extend(steering);
                    *parallel = early
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
            let result_presented = !parallel.is_empty();
            let (mut out, seconds, executed) = match parallel.pop_front() {
                Some(result) => {
                    // Batched bash skips `dispatch`; record verification evidence as the serial path does.
                    if call.name == "bash" {
                        self.note_verification(call, &result.0, ui);
                    }
                    // Batched and streamed-reuse calls also skip the mode hooks `dispatch` runs:
                    // without this, goal mode saw a turn of batched reads as no activity and
                    // stopped its automation as stalled.
                    crate::agent::modes::lifecycle::Lifecycle::dispatched(self, call, &result.0);
                    (result.0, result.1, true)
                }
                None if call.name == "finish"
                    && call.arguments["outcome"] == "completed"
                    && (outcome.execution_failed || !outcome.form_kinds.is_empty()) =>
                {
                    self.step_checks.finish_accepted = true;
                    (
                        ToolOutput::structured(json!({
                            "accepted_outcome": self.completion_rejection_outcome(),
                            "reason": "Completion cannot follow a failed or refused call in the same batch.",
                        })),
                        0.0,
                        false,
                    )
                }
                None => {
                    // Anything executed serially may change what an early read observed.
                    if let Some(streamed) = streamed.as_mut() {
                        streamed.invalidate();
                    }
                    let started = Instant::now();
                    let reuse = engine.reusable_reads(|id| self.holds_full_result(id));
                    let out =
                        self.dispatch_from_reusing(backend, call, provenance, ui, Some(&reuse));
                    (out, started.elapsed().as_secs_f64(), true)
                }
            };
            let executed = executed
                && !out
                    .data
                    .as_ref()
                    .is_some_and(|data| data["read_reuse"]["fully_reused"] == true);
            // Only admitted calls that reached execution count; rejected, skipped and blocked forms do not.
            if executed {
                self.metrics.tool_trace.record_invoked(&call.name);
                if call.name == "read"
                    && !out.is_error
                    && let Some(path) = call.arguments["path"].as_str()
                    && let Ok((uri, _)) =
                        crate::tools::integrations::resources::artifact_target(path)
                {
                    self.metrics.artifacts_retrieved += 1;
                    if self.session.control_state()["truncated_artifacts"]
                        .as_array()
                        .is_some_and(|uris| uris.iter().any(|known| known.as_str() == Some(&uri)))
                    {
                        self.metrics.truncated_recoveries += 1;
                    }
                }
                if matches!(call.name.as_str(), "tool_search" | "tool_load") {
                    turn.capability_search.searched();
                }
                self.efficiency_metrics.tool_result(
                    call,
                    !out.execution.changed_paths.is_empty(),
                    self.workflow_started.elapsed(),
                );
            }
            if !out.is_error {
                match crate::agent::run::dispatch::capture_output_image(
                    &self.workspace,
                    call,
                    &mut out,
                ) {
                    Ok(attachment) => image = attachment,
                    Err(e) if call.name == "browser" || call.name == "computer" => {
                        out = ToolOutput {
                            execution: Default::default(),
                            data: None,
                            text: format!("retaining browser screenshot: {e:#}"),
                            is_error: true,
                        }
                    }
                    Err(e)
                        if call.name == "session_search"
                            && call.arguments["view"] == "layers"
                            && call.arguments["format"] == "image" =>
                    {
                        out = ToolOutput::err(format!(
                            "retaining Snapcompact view: {e:#}; use format:text"
                        ));
                    }
                    // The text result stands on its own; only the attachment is lost.
                    Err(e) => out
                        .text
                        .push_str(&format!("\n[Image attachment unavailable: {e:#}]")),
                }
            }
            if !result_presented {
                ui.tool_timing(&call.id, &call.name, seconds);
            }
            tool_seconds = seconds;
            self.settled(call, &out);
            if call.name == "task"
                && let Some(data) = &out.data
            {
                self.efficiency_metrics.worker_results(data);
            }
            self.observe_tool_routing(call, &out);
            if !result_presented {
                ui.tool_result_with_id(&call.id, &call.name, &out);
            }
            out
        };
        Ok(ExecutedCall {
            call,
            output,
            image,
            contract_failed,
            recovery_blocked,
            controller_blocked,
            tool_seconds,
            todo_before,
            observed_sources,
            target_sources,
        })
    }
}

//! One reply's tool round: run every admitted call in order, then settle the round.
use super::*;
use std::ops::ControlFlow;

mod dispatch;
mod input_preservation;
mod prepare;
mod publish;
mod settle;
mod shell_undo;

/// Runtime services borrowed for dispatch only; execution authority stays with their owners.
struct DispatchContext<'a> {
    engine: &'a mut crate::agent::run::engine::Engine,
    backend: &'a mut Backend,
    schemas: &'a [Value],
    ui: &'a mut dyn Ui,
    streamed: &'a mut Option<crate::agent::run::streaming_dispatch::StreamingDispatch>,
}

/// Immutable request facts, split from the reply's mutable streamed-result queue.
struct RoundRequest<'a> {
    step: u32,
    single_call: bool,
    planning_required: bool,
    reply: &'a llm::Reply,
    compatibility_loaded: &'a HashSet<String>,
    argument_routes: &'a std::collections::HashMap<String, (Vec<String>, Vec<String>)>,
}

/// Operational batch state, not a second copy of the round's outcome.
#[derive(Default)]
struct BatchState {
    narration_warning: Option<String>,
    duplicates: Option<Vec<String>>,
    parallel: std::collections::VecDeque<(ToolOutput, f64)>,
    prerequisites: crate::agent::run::dependency_dispatch::BatchPrerequisites,
    launch_sources:
        std::collections::BTreeMap<String, crate::agent::verify::targets::LaunchSnapshot>,
    boundary: Option<String>,
}

/// Facts captured before or during execution, consumed once by settlement.
struct ExecutedCall<'a> {
    call: &'a ToolCall,
    output: ToolOutput,
    image: Option<Value>,
    contract_failed: bool,
    recovery_blocked: bool,
    controller_blocked: bool,
    tool_seconds: f64,
    todo_before: Value,
    observed_sources: std::collections::BTreeMap<String, Option<String>>,
    target_sources: crate::agent::verify::targets::LaunchSnapshot,
}

/// Publication can change presentation, but cannot re-decide execution or recovery.
struct SettledCall<'a> {
    call: &'a ToolCall,
    output: ToolOutput,
    image: Option<Value>,
    tool_seconds: f64,
    status: tools::ToolStatus,
    duplicate: Option<String>,
    is_duplicate: bool,
    search_note: Option<String>,
    repro_advice: Option<&'static str>,
}

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
        self.pending_batch_approvals.clear();
        let _speculative_batch = self.workspace.speculation.batch();
        let dispatched = self.dispatch_calls(turn, engine, backend, schemas, ui, &mut round);
        // Tickets belong to this exact reply, including when dispatch exits with an error.
        self.pending_batch_approvals.clear();
        if let ControlFlow::Break(tick) = dispatched? {
            return Ok(tick);
        }
        engine.research_round();
        self.settle_round(turn, engine, backend, schemas, ui, round)
    }

    /// Pre-edit context for one native write/edit. The lookup runs on a scoped thread only so the
    /// controller can keep pumping `ui.heartbeat()` while it waits; the call is serial by design,
    /// because the result gates this call's admission and ordering of `prechecked_paths`.
    fn native_edit_preflight(
        &mut self,
        turn: &mut TurnState<'_>,
        call: &ToolCall,
        ui: &mut dyn Ui,
    ) -> Option<ToolOutput> {
        if call.name == "write" {
            if self.config.efficiency.small_case_guard
                && let (Some(path), Some(content)) = (
                    call.arguments["path"].as_str(),
                    call.arguments["content"].as_str(),
                )
                && let Some(note) = turn.edit_churn.first_large_write_notice(
                    path,
                    content.len(),
                    turn.reliability.passing_checks + turn.reliability.failing_checks,
                )
            {
                turn.pending_nudges.push(crate::agent::run::guidance::note(
                    crate::agent::run::guidance::Priority::Review,
                    note,
                ));
            }
            let mut expanded = call.clone();
            if crate::agent::window::file_handles::expand(&self.workspace, &mut expanded).is_ok()
                && let (Some(path), Some(content)) = (
                    expanded.arguments["path"].as_str(),
                    expanded.arguments["content"].as_str(),
                )
                && let Ok(path) = self.workspace.resolve(path)
                && let Some(reason) = turn
                    .edit_churn
                    .rewrite_rejection(&self.workspace.rel(&path), content.len())
            {
                return Some(
                    ToolOutput::err(reason)
                        .with_status(tools::ToolStatus::Blocked)
                        .with_effects(tools::surface::outcome::Effects::None),
                );
            }
        }
        let mut preflight = None;
        if !self.hermetic
            && matches!(call.name.as_str(), "write" | "edit" | "ast_edit")
            && let (Some(cache), Some(path)) =
                (self.codebase.clone(), call.arguments["path"].as_str())
        {
            if turn.prechecked_paths.insert(path.to_owned()) {
                let path = path.to_owned();
                let started = Instant::now();
                let check = thread::scope(|scope| -> Result<_> {
                    let (tx, rx) = mpsc::sync_channel(1);
                    scope.spawn(move || {
                        let _ = tx.send(cache.pre_edit_context(&path));
                    });
                    receive_with_heartbeat(&rx, ui)?
                });
                self.efficiency_metrics
                    .backend_operation("native_edit_preflight", started.elapsed());
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
        preflight
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
        let mut batch = self.prepare_batch(turn, engine, schemas, ui, round)?;
        let mut outcome = ToolOutcome {
            narration_warning: batch.narration_warning.take(),
            short_circuited: batch.duplicates.is_some(),
            ..Default::default()
        };
        let request = RoundRequest {
            step: round.step,
            single_call: round.single_call,
            planning_required: round.planning_required,
            reply: &round.reply,
            compatibility_loaded: &round.compatibility_loaded,
            argument_routes: &round.argument_routes,
        };
        let mut context = DispatchContext {
            engine,
            backend,
            schemas,
            ui,
            streamed: &mut round.streamed,
        };
        let step = request.step;
        let reply = request.reply;
        let mut budget = publish::ResultBudget::new(
            if self.hermetic {
                0
            } else {
                publish::ResultBudget::percent(&self.config.efficiency)
            },
            self.budget_window(),
            self.context_usage().0,
        );
        self.verify_batch.begin(&reply.tool_calls);
        self.snapshot_before_shell(&reply.tool_calls, &mut *context.ui);
        for (index, call) in reply.tool_calls.iter().enumerate() {
            let engine = &mut *context.engine;
            let ui = &mut *context.ui;
            let BatchState {
                parallel,
                prerequisites,
                boundary,
                duplicates,
                ..
            } = &mut batch;
            let read_overlap = call.name != "bash"
                && crate::agent::safety::approval::parallel_read(
                    &self.ext,
                    &call.name,
                    &call.arguments,
                )
                && !crate::agent::safety::hooks::wanted_for_tool(
                    self.tool_hooks(),
                    "pre_tool_use",
                    &call.name,
                )
                && !crate::agent::safety::hooks::wanted_for_tool(
                    self.tool_hooks(),
                    "post_tool_use",
                    &call.name,
                );
            self.drain_background_check(ui, !read_overlap);
            if parallel.is_empty()
                && let Some(reason) = engine.pending_refusal()
            {
                self.push(crate::tools::surface::outcome::skipped(&call.id, reason))?;
                continue;
            }
            if self.interrupt.load(Ordering::SeqCst) {
                self.close_interrupted_batch(&reply.tool_calls[index..], parallel)?;
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
                *boundary = Some(reason);
                continue;
            }
            if let Some(results) = duplicates.as_ref() {
                // The loop guard answered this call from the earlier identical round.
                self.record(step, "repeated_call", json!({"name":call.name}))?;
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
            if outcome.portable_batch_intercepted && parallel.is_empty() {
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
            outcome.steering.extend(
                self.drain_steering(ui)
                    .into_iter()
                    .filter(|text| !text.trim().is_empty()),
            );
            if !outcome.steering.is_empty() && parallel.is_empty() {
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
            if parallel.is_empty()
                && let Some(output) =
                    prerequisites.rejection(&self.workspace, self.edit_mode(), &self.ext, call)
            {
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
            let executed = self.dispatch_one(
                turn,
                &mut context,
                &request,
                index,
                &mut batch,
                &mut outcome,
            )?;
            let settled = self.settle_call(
                turn,
                &mut context,
                &request,
                &mut batch,
                executed,
                &mut outcome,
            )?;
            // Results already produced by this call's group are known; water-fill across them.
            let result_cap = crate::tools::model_output_limit(self.context_window());
            let pending = crate::agent::run::dispatch::result_budget::pending_sizes(
                &reply.tool_calls[index + 1..],
                batch.parallel.iter().map(|(output, _)| output),
                result_cap,
            );
            self.publish_call_result(turn, &mut context, step, settled, &mut budget, &pending)?;
        }
        self.verify_batch.end();
        if let Some(stream) = round.streamed.take() {
            let (started, reused) = stream.statistics();
            self.efficiency_metrics.scheduling.stream(started, reused);
        }
        round.outcome = outcome;
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

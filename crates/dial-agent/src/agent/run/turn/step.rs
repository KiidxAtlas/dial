//! One engine tick of the parent turn: admit the step, build and send one request, recover or
//! complete from the reply, execute its tool calls and settle the round.
use super::*;

impl Agent<'_> {
    /// Runs one request/response round. Turn-scoped state lives in [`TurnState`].
    pub(super) fn turn_step(
        &mut self,
        turn: &mut TurnState<'_>,
        engine: &mut crate::agent::run::engine::Engine,
        backend: &mut Backend,
        schemas: &mut Vec<Value>,
        ui: &mut dyn Ui,
    ) -> Result<crate::agent::run::engine::Tick<TurnReport>> {
        let step = match self.begin_step(turn, engine, backend, schemas, ui)? {
            std::ops::ControlFlow::Continue(step) => step,
            std::ops::ControlFlow::Break(tick) => return Ok(tick),
        };
        // `schemas` is the turn's full offer. A request narrows its own copy (planning gate,
        // single-call simplification) and the next request starts from the full offer again.
        turn.names = offered_names(schemas);
        let prep_started = Instant::now();
        if let Some(hints) = turn.enrichment.poll(crate::agent::run::enrichment::Stamp {
            revision: self.step_checks.revision(),
            instruction: turn.enrichment_epoch,
        }) {
            turn.pending_nudges.push(crate::agent::run::guidance::note(
                crate::agent::run::guidance::Priority::Review,
                hints,
            ));
        }
        let budget_window = self.budget_window();
        self.before_request(backend, &turn.names, &mut turn.pending_nudges, ui)?;
        let mut messages = self.system_messages();
        self.session.extend_messages(&mut messages);
        let pre_action_secs = if step == 0 {
            self.task_policy.initial_reasoning_secs
        } else {
            self.config.reasoning_guard.max_pre_action_secs
        };
        let mut selection = crate::adaptive::select(
            self.config,
            &self.request_phase,
            crate::adaptive::Request {
                kind: self.task_policy.kind,
                temperature: turn.harness.temperature,
                tokens: turn.reasoning_budget.unwrap_or(turn.max_tokens),
                pre_action_secs,
                base_profile: backend.model_profile(),
                recovering_output: turn.recovering_output || turn.reasoning_budget.is_some(),
                hermetic: self.hermetic,
            },
        );
        if let Some(source) =
            self.apply_local_sampling(&mut selection.profile, turn.harness.temperature)
        {
            selection.sampling_source = format!("local profile: {source}");
        }
        let mut request_control = self.session.control_state();
        let mut packet_cache = std::mem::take(&mut self.task_packet);
        let mut superseded = std::mem::take(&mut self.superseded);
        let sent_messages = self.project_history(
            &mut messages,
            schemas,
            selection.context_bytes,
            &mut packet_cache,
            &mut superseded,
            Some(&mut turn.rule_state),
        )?;
        self.task_packet = packet_cache;
        self.superseded = superseded;
        // Opt-in (`reasoning_guard.require_plan`): after a reasoning-gate hit with no plan,
        // only `todo` is offered until one exists.
        if self.todo.has_open() {
            turn.plan_after_gate = false;
        }
        let mut offer = schemas.clone();
        let planning_required = !self.hermetic
            && turn.plan_after_gate
            && !self.todo.has_open()
            && offer.iter().any(|s| s["function"]["name"] == "todo");
        // Harness notes for this request start here; they are coalesced into one message below.
        let notes_start = messages.len();
        if planning_required {
            if !turn.planning_notice {
                ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Plan, "Plan required: create a todo plan to resume tools. The next request offers only todo."));
            }
            messages.push(json!({"role":"user","dial_transient":true,"content":
                    "The previous generation stalled without an active plan. If the request is already answered, answer directly. Otherwise your next tool call must create actual todos: use todo set for an empty list, or append to preserve existing tasks. Add 1-3 concrete unresolved questions with investigation=true, then the known implementation and verification steps. Do not invent a large speculative plan. A prose plan is not a todo update."}));
            offer.retain(|s| matches!(s["function"]["name"].as_str(), Some("todo" | "finish")));
            turn.names.retain(|name| name == "todo" || name == "finish");
        }
        turn.planning_notice = planning_required;
        engine.apply(&mut offer, &mut turn.names, &mut messages);
        let measured_single = !self.hermetic
            && crate::reliability::single_call(
                self.config,
                turn.measured.as_ref(),
                &turn.reliability,
            );
        // The small local tier defaults to one call per response unless its saved record passed
        // the batch fixture; the system prompt states the same mode for the whole session. Its
        // offer is already minimal and listed in that prompt, so it is not simplified further.
        let single_call = measured_single || (!self.hermetic && self.local_one_call());
        let mut contract = crate::reliability::Contract::new(offer)?;
        if measured_single {
            let loaded = self
                .enabled_tools
                .iter()
                .cloned()
                .chain(self.discovered_tools())
                .collect();
            crate::reliability::simplify(&mut contract.schemas, &loaded);
            // Simplification can drop offered schemas, so the offered contract is validated again.
            contract = crate::reliability::Contract::new(contract.schemas)?;
        }
        turn.names = contract.names.clone();
        let lightweight = self.config.efficiency.enabled
            && !self.hermetic
            && self.task_policy.lightweight
            && step < self.config.efficiency.lightweight_requests
            && !self.todo.has_open()
            && self.efficiency_metrics.failed_tool_results == 0;
        // The tool contract is restated only when the model is constrained to one call or has
        // just broken the contract; the offered schemas already carry it otherwise.
        if !self.hermetic
            && self.config.reliability.enabled
            && (single_call || turn.reliability.consecutive_contract_failures > 0)
        {
            let guidance = contract.instruction(
                single_call,
                self.config.reliability.examples && !lightweight,
            );
            messages.push(json!({"role":"user","content":guidance,"dial_transient":true}));
        }
        turn.todo_recovery.apply(&mut messages);
        turn.pending_nudges.extend(
            std::mem::take(&mut self.background_notices)
                .into_iter()
                .map(|text| json!({"role":"user","content":text,"dial_transient":true})),
        );
        // Lifecycle hooks' `additional_context` is delivered once, like background notices.
        turn.pending_nudges.extend(self.take_hook_context());
        turn.pending_nudges.extend(self.poll_coordination(ui));
        messages.extend(turn.pending_nudges.iter().cloned());
        // The redirect is stated once; if the identical repeat continues, the stronger
        // guidance replaces it. Neither stage ends the turn.
        let repeat_streak = crate::agent::safety::guard::RepeatedToolCall::detect(
            &messages,
            crate::agent::safety::stream_guards::TOOL_CALL_LOOP_THRESHOLD,
            crate::agent::safety::stream_guards::TOOL_CALL_LOOP_EXEMPT_TOOLS,
        );
        if let Some(repeat) = &repeat_streak {
            ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                crate::agent::ui::ui_event::EventKind::Recovery,
                format!(
                    "Repeated tool call {}x; redirecting the model",
                    repeat.count
                ),
            ));
            let guidance =
                if repeat.count > crate::agent::safety::stream_guards::TOOL_CALL_LOOP_THRESHOLD {
                    crate::agent::safety::stream_guards::render_tool_call_loop_escalation(
                        &repeat.tool_name,
                        repeat.count,
                        &repeat.arguments_summary,
                        &repeat.result_summary,
                    )
                } else {
                    crate::agent::safety::stream_guards::render_tool_call_loop_redirect(
                        &repeat.tool_name,
                        repeat.count,
                        &repeat.arguments_summary,
                        &repeat.result_summary,
                    )
                };
            messages.push(crate::agent::run::guidance::note(
                crate::agent::run::guidance::Priority::Repetition,
                guidance,
            ));
        } else if let Some(cycle) = crate::agent::safety::guard::CallCycle::detect(
            &messages,
            crate::agent::safety::stream_guards::TOOL_CALL_LOOP_EXEMPT_TOOLS,
        ) {
            ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                crate::agent::ui::ui_event::EventKind::Recovery,
                format!(
                    "Tool calls cycling with period {}; redirecting the model",
                    cycle.period
                ),
            ));
            messages.push(crate::agent::run::guidance::note(
                crate::agent::run::guidance::Priority::Repetition,
                cycle.guidance(),
            ));
        }
        let research_step = engine.research_required();
        if let Some(note) = engine.research_prompt() {
            messages.push(note);
        }
        // One advisory note per request, by priority; protocol notes and gates are unaffected.
        // A previous request that was discarded before any reply delivered nothing.
        turn.emissions.retract();
        crate::agent::run::guidance::curate(&mut messages, &mut turn.emissions);
        coalesce_harness_notes(&mut messages, notes_start);
        if !self.experimental_context_owner()
            && let Some(guidance) = turn.rule_catalog.guidance(&mut turn.rule_state, "main")
        {
            messages.push(guidance);
        }
        turn.rule_state.save(&mut request_control);
        let mut request = Chat {
            messages,
            tools: contract.schemas,
            temperature: selection.temperature.unwrap_or(turn.harness.temperature),
            max_tokens: selection.max_tokens,
            seed: if self.hermetic {
                Some(42 + u64::from(step))
            } else {
                None
            },
        };
        let local_control = self.prepare_local_request(backend, &mut selection.profile);
        if let Some(local) = &local_control {
            self.record(step, "local_request", local.clone())?;
        }
        backend.next_request_profile(crate::llm::profiles::request_tiers_for(
            selection.profile.clone(),
            &self.auth_config.selection(),
            self.auth_config.effective_service_tier(),
            self.auth_config.extended_context,
        )?);
        let compaction_selection = self.auth_config.selection();
        let compaction_identity = crate::config::compaction::CompactionIdentity {
            provider: &compaction_selection.provider,
            model: &compaction_selection.model,
            agent: None,
        };
        let mut request_estimate = crate::agent::compact::compaction::request_tokens_with_policy(
            &request,
            &self.config.compaction,
            compaction_identity,
        );
        if self.experimental_context_owner()
            && self.config.compaction.enabled
            && crate::agent::window::context_notes::TOOL_NAMES
                .iter()
                .all(|name| {
                    request
                        .tools
                        .iter()
                        .any(|schema| schema["function"]["name"] == *name)
                })
        {
            let threshold = self
                .config
                .compaction
                .effective_threshold(compaction_identity, budget_window)?;
            let current = self
                .context_meter
                .predict(request_estimate)
                .max(request_estimate);
            if let Some(guidance) = crate::agent::window::context_notes::pressure_guidance(
                &self.session,
                current,
                threshold,
                crate::config::compaction::speculation_lead(threshold),
                &mut self.context_notes_reminder_boundary,
            ) {
                // Price only the appended message; re-serializing the whole history again
                // per step is pure overhead at large context sizes.
                request_estimate =
                    request_estimate.saturating_add(crate::llm::wire::estimate(&guidance));
                request.messages.push(guidance);
            }
        }
        let mut request_overhead = crate::agent::window::context_meter::overhead(&request);
        self.context_meter
            .record_breakdown(std::mem::take(&mut request_overhead.breakdown));
        let request_context = self
            .context_meter
            .predict(request_estimate)
            .max(request_estimate);
        if turn.harness.max_tokens == 0
            && selection
                .profile
                .as_ref()
                .is_none_or(|profile| profile.max_tokens.is_none())
        {
            // Unlimited output resolves to a model ceiling, not an allocation that must fit
            // beside every prompt. Bound that automatic ceiling by this request's remaining
            // context; the policy reserve below still cannot be consumed by the prompt.
            let remaining = u64::from(self.context_window()).saturating_sub(request_context);
            if remaining > 0 {
                request.max_tokens = request.max_tokens.min(remaining as u32);
            }
        }
        let reason = if step == 0 {
            crate::config::compaction::CompactionReason::Threshold
        } else {
            crate::config::compaction::CompactionReason::MidTurn
        };
        self.poll_background_compaction(ui);
        // Apply the same failed-attempt backoff to speculation and blocking maintenance.
        // Otherwise each small tool result can relaunch the method chain in the background.
        let safe_prompt_budget = crate::agent::compact::compaction::prompt_budget(
            &self.config.compaction,
            self.context_window(),
            backend.output_budget(request.max_tokens),
        );
        let retry_ready = turn
            .compaction_retry_floor
            .is_none_or(|floor| request_context >= floor || request_context > safe_prompt_budget);
        // Switch to a configured larger-window model before any compaction (background claim
        // included) once the request is over the compaction threshold.
        if !self.config.compaction.experimental_context_management
            && self.config.compaction.promotion.enabled
            && self.config.compaction.should_compact(
                compaction_identity,
                budget_window,
                request_context,
                step != 0,
            )?
            && {
                let output = backend.output_budget(request.max_tokens);
                self.try_promote_context(backend, request_context, output, false, ui)
            }
        {
            turn.max_tokens = backend.output_budget(turn.harness.max_tokens);
            turn.compaction_failed = false;
            turn.compaction_retry_floor = None;
            turn.budget.give_back();
            return Ok(crate::agent::run::engine::Tick::Again);
        }
        let mut deferred_compaction = false;
        if !turn.compaction_failed && retry_ready {
            let action = self.compaction_runtime.speculation_action(
                crate::agent::compact::compaction_runtime::SpeculationContext {
                    policy: &self.config.compaction,
                    identity: compaction_identity,
                    session_id: self.session.id(),
                    window: budget_window,
                    fixed_tokens: crate::agent::compact::compaction::request_tokens_with_policy(
                        &crate::agent::compact::compaction::replacement_request(&request, &[]),
                        &self.config.compaction,
                        compaction_identity,
                    ),
                    tokens: request_context,
                    mid_turn: step != 0,
                    image_input: backend.accepts_image_input(),
                    parallel_supported: !backend.local_inference(),
                },
            )?;
            match action {
                crate::agent::compact::compaction_runtime::SpeculationAction::Claim => {
                    match self.claim_background_compaction(ui, reason, Some(&request), Some(backend)) {
                            Ok(Some(outcome)) => {
                                if !outcome.continue_allowed { return self.pause_after_compaction(ui, step).map(crate::agent::run::engine::Tick::Finished); }
                                turn.compaction_succeeded();
                                turn.context_rewritten(engine);
                                turn.budget.give_back();
                                return Ok(crate::agent::run::engine::Tick::Again);
                            }
                            Ok(None) => {}
                            Err(error) => ui.diagnostic(&format!("Prepared compaction was not claimed ({error:#}); considering blocking maintenance.")),
                        }
                }
                crate::agent::compact::compaction_runtime::SpeculationAction::Defer => {
                    deferred_compaction = true
                }
                crate::agent::compact::compaction_runtime::SpeculationAction::Start(method)
                | crate::agent::compact::compaction_runtime::SpeculationAction::Refresh(method) => {
                    if action
                        == crate::agent::compact::compaction_runtime::SpeculationAction::Refresh(
                            method,
                        )
                    {
                        self.compaction_runtime.cancel();
                    }
                    let native_profile = backend.compaction_profile().cloned();
                    match self.start_background_compaction(
                        ui,
                        crate::agent::compact::compaction_runtime::BackgroundCompactionRequest {
                            template: Some(&request),
                            kind: crate::agent::compact::compaction_runtime::WorkKind::Speculative,
                            method: Some(method),
                            context_tokens: request_context,
                            native_profile: native_profile.as_ref(),
                        },
                        Some(backend),
                    ) {
                            Ok(crate::agent::compact::compaction_runtime::Launch::Started) => deferred_compaction = true,
                            Ok(_) => {}
                            Err(error) => ui.diagnostic(&format!("Background compaction was not started ({error:#}); considering blocking maintenance.")),
                        }
                }
                crate::agent::compact::compaction_runtime::SpeculationAction::Blocking
                | crate::agent::compact::compaction_runtime::SpeculationAction::None => {}
            }
        }
        // A deferred background summary may finish later, but never let a request through that
        // leaves less than the output reserve: past that budget, compact in the foreground now.
        if !turn.compaction_failed
            && retry_ready
            && (!deferred_compaction || request_context > safe_prompt_budget)
            && !self.config.compaction.experimental_context_management
            && self.config.compaction.should_compact(
                compaction_identity,
                budget_window,
                request_context,
                step != 0,
            )?
        {
            let options = crate::config::compaction::CompactionOptions {
                reason,
                ..Default::default()
            };
            // A compaction since the last reply that left the request over its threshold shows
            // that compacting again cannot make room; repeating it would loop without a request.
            let compacted = if turn.compacted_without_reply {
                Err(anyhow::anyhow!(
                    "the previous compaction left the request over the compaction threshold"
                ))
            } else {
                self.compact_using(backend, ui, Some(&request), &options)
            };
            match compacted {
                Ok(outcome) => {
                    if !outcome.continue_allowed {
                        return self
                            .pause_after_compaction(ui, step)
                            .map(crate::agent::run::engine::Tick::Finished);
                    }
                    // The compacted context no longer holds the results duplicate memory cited.
                    // Re-check at the next request boundary; compaction may be needed again
                    // after subsequent tool results in this same autonomous turn.
                    turn.compaction_succeeded();
                    turn.context_rewritten(engine);
                    turn.budget.give_back();
                    return Ok(crate::agent::run::engine::Tick::Again);
                }
                Err(error) => {
                    turn.compaction_failed_at(request_context);
                    ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                        crate::agent::ui::ui_event::EventKind::Recovery,
                        format!("context compaction could not make enough room ({error:#})"),
                    ));
                    if self.interrupt.load(Ordering::SeqCst) {
                        return Ok(crate::agent::run::engine::Tick::Finished(
                            TurnReport::interrupted(step + 1),
                        ));
                    }
                }
            }
        }
        if request_context > safe_prompt_budget && !self.lean_tools {
            self.lean_tools = true;
            self.rebuild_system();
            turn.names = self.offered();
            *schemas = self.schemas(&turn.names)?;
            ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Model, format!(
                    "Context window {} needs response headroom; offering lean instructions and tools (others load on demand)",
                    self.context_window()
                )));
            turn.budget.give_back();
            return Ok(crate::agent::run::engine::Tick::Again);
        }
        ensure!(
            request_context <= safe_prompt_budget
                && request_context < u64::from(self.context_window()),
            "the projected request needs about {request_context} tokens, exceeding the usable prompt budget of {safe_prompt_budget} tokens in the {}-token context window{} (the remainder is reserved for a useful response). Estimated wire bytes: system={}, tools={}, results={}, task={}, conversation={}. Compaction cannot safely make room; reduce system/project instructions or tool schemas, retrieve smaller evidence slices, or select a larger-window model. The session and unfinished work are preserved",
            self.context_window(),
            if self.lean_tools {
                " even with the lean tool set"
            } else {
                ""
            },
            request_overhead.system_bytes,
            request_overhead.tool_schema_bytes,
            request_overhead.tool_result_bytes,
            request_overhead.task_context_bytes,
            request_overhead.conversation_bytes
        );
        // One-shot notes stay queued until a reply proves delivery (`TurnState::reply_received`):
        // every path below that discards this request before a reply resends them.
        self.efficiency_metrics
            .request(&request_overhead, lightweight);
        let requested_tokens = request.max_tokens;
        let repairing = turn.repair.take_request();
        let mut request_selection = None;
        if !self.hermetic {
            let mut profile = selection.profile.clone();
            if single_call
                && let Some(profile) = profile.as_mut()
                && crate::llm::model_config::ApiProtocol::builtin(&profile.provider)
                    .accepts_parallel_tool_calls()
            {
                profile.capabilities.parallel_tool_calls = Some(false);
            }
            self.judge_turn_effort(turn, profile.as_ref(), ui);
            if let (Some(effort), Some(profile)) = (turn.turn_effort, profile.as_mut())
                && let Some(clamped) =
                    crate::judgment::clamp_effort(effort, &profile.capabilities.reasoning_efforts)
            {
                profile.reasoning_effort = Some(clamped);
            }
            // Applied last: external thinking keeps provider reasoning off whatever else chose an effort.
            if self.external_thinking_instruction().is_some()
                && let Some(profile) = profile.as_mut()
            {
                profile.reasoning_effort = Some(crate::llm::profiles::ReasoningEffort::None);
            }
            if repairing {
                let model = self.auth_config.selection();
                let profile = profile.get_or_insert_with(|| {
                    crate::llm::profiles::ModelProfile::minimal(&model.provider, &model.model)
                });
                profile.tool_repair = true;
            }
            let effective = local_control.as_ref().map(|local| {
                crate::adaptive::local_request_settings(
                    &request,
                    profile.as_ref(),
                    local,
                    &selection.sampling_source,
                )
            });
            backend.next_request_profile(crate::llm::profiles::request_tiers_for(
                profile,
                &self.auth_config.selection(),
                self.auth_config.effective_service_tier(),
                self.auth_config.extended_context,
            )?);
            let signature = json!([
                selection.enabled,
                selection.phase,
                selection.temperature,
                selection.top_p,
                selection.max_tokens,
                selection.pre_action_secs,
                selection.reasoning_effort
            ])
            .to_string();
            if signature != turn.policy_signature {
                turn.policy_signature = signature;
                if selection.enabled {
                    ui.diagnostic(&format!("Policy: {} → {} · temp {} · output {} · pre-action {}s · /policy shows why",selection.task,selection.phase.label(),selection.temperature.map(|t|t.to_string()).unwrap_or("provider-controlled".into()),selection.max_tokens,selection.pre_action_secs));
                }
            }
            let mut control = request_control;
            control["reliability"]["interface"] = json!(if single_call {
                "compact single call"
            } else {
                "standard"
            });
            control["reliability"]["current_tools"] = json!(turn.names);
            self.session.set_control_state(control)?;
            let mut selected = serde_json::to_value(&selection)?;
            if let Some(effective) = effective {
                selected["temperature"] = effective["temperature"].clone();
                selected["top_p"] = effective["top_p"].clone();
                selected["effective"] = effective;
            }
            request_selection = Some(selected);
        }
        // The headroom gate used this exact projected request, including transient guidance.
        self.record(
            step,
            "request_overhead",
            serde_json::to_value(&request_overhead)?,
        )?;
        let started = Instant::now();
        let timeout = self.request_timeout();
        let mut reasoning_policy = self.config.reasoning_guard.clone();
        reasoning_policy.max_pre_action_secs = selection.pre_action_secs;
        if reasoning_policy.max_pre_action_secs == 0
            && let Some(secs) = self.local_time_budget(
                request_estimate,
                self.local_reasoning_cap()
                    .map_or(u64::from(selection.max_tokens), |bytes| (bytes / 4) as u64),
            )
        {
            reasoning_policy.max_pre_action_secs = secs;
        }
        if let Some(cap) = self.local_reasoning_cap()
            && (reasoning_policy.max_reasoning_bytes == 0
                || reasoning_policy.max_reasoning_bytes > cap)
        {
            reasoning_policy.max_reasoning_bytes = cap;
        }
        if let Some(mut selected) = request_selection {
            if selected["effective"].is_object() {
                selected["effective"]["max_reasoning_bytes"] = json!(if reasoning_policy.enabled {
                    reasoning_policy.max_reasoning_bytes
                } else {
                    0
                });
                selected["effective"]["max_pre_action_secs"] = json!(if reasoning_policy.enabled {
                    reasoning_policy.max_pre_action_secs
                } else {
                    0
                });
            }
            self.record(step, "request_policy", selected.clone())?;
            self.save_request_policy(Some(selected), json!({"state":"awaiting response"}), ui)?;
        }
        let interrupt = self.interrupt.clone();
        // Steering cancels only this generation, never the whole user turn or
        // independent workers that share the ordinary interrupt flag.
        let generation_scope = turn.cancel.inference();
        let generation_cancel = generation_scope.flag();
        let mut steered_during_generation = false;
        // `--max-time` passing mid-generation cancels only this generation; the turn then stops.
        let mut deadline_cancelled = false;
        let mut steering_error = None;
        let live_steering_key = backend.live_steering_key();
        let mut live_steering = LiveSteering::default();
        let mut rule_monitor = turn
            .rule_catalog
            .monitor(&turn.rule_root, &turn.rule_state, "main");
        let rewritten = self.provider_request_with(&request, ui)?;
        let provider_request = rewritten.as_ref().unwrap_or(&request);
        let prefix = self.request_prefix.observe(
            &self.auth_config.selection().display_name(),
            provider_request,
        );
        self.record(step, "request_prefix", serde_json::to_value(&prefix)?)?;
        // Wall time before primary inference, including synchronous maintenance and waits.
        self.record(
            step,
            "request_prep",
            json!({"elapsed_ms": prep_started.elapsed().as_millis()}),
        )?;
        ui.diagnostic(&format!(
            "Request prefix: {}; {} stable message bytes",
            prefix.invalidation, prefix.stable_message_bytes
        ));
        // A constrained repair request is tagged as repair; the agent scope is restored after it.
        let previous_usage = repairing.then(|| {
            backend.set_usage_scope(Some(
                self.usage_scope
                    .for_purpose(crate::llm::usage::Purpose::Repair),
            ))
        });
        // Experimental: admitted reads start while the reply still streams (`run/streaming_dispatch.rs`).
        let mut streamed = (!self.hermetic
            && !single_call
            && self.effective_config().experimental.streaming_dispatch)
            .then(|| {
                crate::agent::run::streaming_dispatch::StreamingDispatch::new(
                    self.workspace.clone(),
                    self.effective_config().clone(),
                    self.ext.clone(),
                    self.interrupt.clone(),
                    crate::agent::run::streaming_dispatch::Admission {
                        hooks: self.tool_hooks().to_vec(),
                        plugins: self.hook_plugins(),
                        always: self.always.clone(),
                        schemas: provider_request.tools.clone(),
                        names: turn.names.clone(),
                        failed_calls: engine.failed_calls(),
                    },
                )
            });
        // With a configured fallback still untried, failures it accepts come back after a brief
        // retry so the turn fails over instead of waiting out every provider retry first.
        backend.next_request_failover(
            (!self.hermetic && self.fallback_ready())
                .then_some(self.effective_config().retry.fallback_on),
        );
        ui.message_started();
        self.emit_event(
            "message_start",
            || json!({"session_id": self.session.id(), "step": step}),
        );
        let result = if self.hermetic {
            crate::learn::compute::background_call(self.config, backend, |backend, cancel| {
                chat_with_ui(
                    backend,
                    turn.adapter.as_deref(),
                    provider_request,
                    ChatOptions {
                        timeout,
                        render_text: false,
                        context_tokens: Some(request_context),
                        policy: Some(&reasoning_policy),
                        recovery: Some(&self.config.request_recovery),
                    },
                    if crate::learn::compute::background_active() {
                        cancel
                    } else {
                        interrupt.clone()
                    },
                    &mut NullUi,
                    &mut |_| {},
                )
            })
        } else {
            chat_with_ui_streaming(
                backend,
                turn.adapter.as_deref(),
                provider_request,
                ChatOptions {
                    timeout,
                    render_text: self.external_thinking_instruction().is_none() && !repairing,
                    context_tokens: Some(request_context),
                    policy: Some(&reasoning_policy),
                    recovery: Some(&self.config.request_recovery),
                },
                generation_cancel.clone(),
                crate::agent::ChatSinks {
                    ui,
                    on_idle: &mut |ui| {
                        self.pump_subagents(ui);
                        if self.modes.launch.expired() {
                            deadline_cancelled = true;
                            generation_cancel.store(true, Ordering::SeqCst);
                        }
                        generation_scope.sync();
                        let result = self.steer_during_generation(
                            ui,
                            live_steering_key.as_deref(),
                            &mut live_steering,
                        );
                        match result {
                            Ok(true) => {
                                steered_during_generation = true;
                                generation_cancel.store(true, Ordering::SeqCst);
                            }
                            Ok(false) => {}
                            Err(error) => {
                                steering_error = Some(error);
                                generation_cancel.store(true, Ordering::SeqCst);
                            }
                        }
                    },
                },
                crate::agent::ChatMonitors {
                    rules: Some(&mut rule_monitor),
                    stream: streamed.as_mut().map(|stream| {
                        stream as &mut dyn crate::agent::run::streaming_dispatch::ToolStream
                    }),
                },
            )
        };
        if let Some(previous) = previous_usage {
            backend.set_usage_scope(previous);
        }
        self.session
            .runtime()
            .check_persistence()
            .map_err(anyhow::Error::msg)?;
        // Also clear a profile if the model worker failed before entering the provider.
        if let Some(observation) = backend.wire_observation() {
            self.record(step, "provider_wire", serde_json::to_value(observation)?)?;
        }
        backend.next_request_profile(None);
        backend.next_request_failover(None);
        match &result {
            Ok(reply) => {
                self.observe_local_reply(reply, backend);
                self.note_fallback_served(ui);
            }
            Err(error) => self.observe_local_failure(error, backend),
        }
        self.observe_cost_warnings(&mut turn.turn_cost_warn, ui);
        // The projected request is not reused after inference: move its history instead of
        // deep-copying it each iteration. Recovery and training still see the exact bytes sent.
        let (context_before, request_tools) = match rewritten {
            Some(rewritten) => (rewritten.messages, rewritten.tools),
            None => (std::mem::take(&mut request.messages), request.tools.clone()),
        };
        // What this request offered: calls are normalized and repaired against it.
        let offer = request.tools;
        // Receipts still outstanding after the stream ends: wait to their deadline, then commit.
        match self.settle_live_steering(ui, &mut live_steering) {
            Ok(true) => steered_during_generation = true,
            Ok(false) => {}
            Err(error) => {
                steering_error.get_or_insert(error);
            }
        }
        // Undecided input never cancels a live generation or a running tool. Classify only at
        // this safe boundary, before admitting any calls from the completed response.
        if !steered_during_generation
            && !self.interrupt.load(Ordering::SeqCst)
            && self.route_pending_messages(backend, ui)?
        {
            if result.is_ok() {
                record_rejected_reply(&self.usage_scope, backend, repairing)?;
            }
            steered_during_generation = true;
        }
        // A reply that completed before the cancellation took effect is kept: the round skips its
        // unstarted tools at the boundary and a final answer is still delivered.
        if deadline_cancelled && result.is_err() && !self.interrupt.load(Ordering::SeqCst) {
            self.record(
                step,
                "deadline_cancelled_generation",
                json!({"elapsed_ms":started.elapsed().as_millis()}),
            )?;
            return self
                .stop_turn(
                    ui,
                    "Stopped: --max-time deadline reached during a model request; the unfinished generation was discarded.".into(),
                    step + 1,
                )
                .map(crate::agent::run::engine::Tick::Finished);
        }
        if let Some(error) = steering_error {
            return Err(error).map(crate::agent::run::engine::Tick::Finished);
        }
        if steered_during_generation && !self.interrupt.load(Ordering::SeqCst) {
            turn.steering_pending = true;
            self.record(
                step,
                "steering_superseded_generation",
                json!({"elapsed_ms":started.elapsed().as_millis()}),
            )?;
            ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                crate::agent::ui::ui_event::EventKind::Steering,
                "Stopped the old generation; replanning with your new instruction.",
            ));
            turn.budget.give_back();
            return Ok(crate::agent::run::engine::Tick::Again);
        }
        let rule_hit = result
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<crate::agent::safety::rules::Hit>())
            .cloned();
        let scoped_paths = result.as_ref().ok().and_then(|reply| {
            turn.rule_catalog.pending_paths(
                &turn.rule_root,
                &turn.rule_state,
                "main",
                &reply.tool_calls,
            )
        });
        if !self.interrupt.load(Ordering::SeqCst) && (rule_hit.is_some() || scoped_paths.is_some())
        {
            if result.is_ok() {
                record_rejected_reply(&self.usage_scope, backend, repairing)?;
            }
            let discarded = scoped_paths.is_some().then_some("New file-scoped guidance must be delivered before these tools; discarded the entire tool attempt.");
            return self.regenerate_for_guidance(turn, step, rule_hit, scoped_paths, discarded, ui);
        }
        let mut reply = match result {
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
                    return Ok(crate::agent::run::engine::Tick::Again);
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
                    self.push(json!({"role":"assistant","content":partial}))?;
                    // The partial answer kept in history responded to this request's notes.
                    turn.reply_received();
                    // After the reply is recorded, so a redirecting note reaches the next request.
                    if let Some(report) =
                        self.setback(turn, super::progress::Setback::StreamStall, step, ui)?
                    {
                        return Ok(crate::agent::run::engine::Tick::Finished(report));
                    }
                    turn.pending_nudges.push(json!({"role":"user","content":crate::agent::safety::stream_guards::render_retry_prompt(crate::agent::safety::stream_guards::STREAM_STALL_CONTINUE, turn.stall_continues, crate::agent::safety::stream_guards::STREAM_STALL_CONTINUE_MAX_RETRIES),"dial_transient":true}));
                    turn.refund_unusable(ui);
                    return Ok(crate::agent::run::engine::Tick::Again);
                }
                self.save_request_policy(None, json!({"state":"request needs recovery","elapsed_ms":started.elapsed().as_millis()}), ui)?;
                return self
                    .stop_turn(
                        ui,
                        format!("{error} Send a direction or retry to resume the unfinished task."),
                        step + 1,
                    )
                    .map(crate::agent::run::engine::Tick::Finished);
            }
            Ok(reply) => {
                turn.reply_received();
                reply
            }
            Err(error)
                if error
                    .downcast_ref::<crate::agent::workflow::reasoning::GateHit>()
                    .is_some()
                    && !self.interrupt.load(Ordering::SeqCst) =>
            {
                turn.reasoning_budget = Some(requested_tokens);
                self.request_phase
                    .recover("reasoning gate intercepted the previous request");
                self.save_request_policy(
                    None,
                    json!({"state":"reasoning gate","elapsed_ms":started.elapsed().as_millis()}),
                    ui,
                )?;
                self.metrics.reasoning_interceptions += 1;
                self.record(
                    step,
                    "reasoning_gate",
                    json!({"reason":error.to_string(),"elapsed_ms":started.elapsed().as_millis()}),
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
                            .stop_turn(ui, message, step + 1)
                            .map(crate::agent::run::engine::Tick::Finished);
                    }
                }
                return Ok(crate::agent::run::engine::Tick::Again);
            }
            Err(error)
                if error.downcast_ref::<llm::Interrupted>().is_some()
                    || self.interrupt.load(Ordering::SeqCst) =>
            {
                self.save_request_policy(
                    None,
                    json!({"state":"interrupted","elapsed_ms":started.elapsed().as_millis()}),
                    ui,
                )?;
                return Ok(crate::agent::run::engine::Tick::Finished(
                    TurnReport::interrupted(step + 1),
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
                return Ok(crate::agent::run::engine::Tick::Again);
            }
            Err(error)
                if !self.hermetic
                    && looks_like_overflow(&error)
                    && turn.overflow_retries < 3
                    && self.config.compaction.enabled
                    && self.config.compaction.strategy
                        != crate::config::compaction::CompactionStrategy::Off =>
            {
                if let Some(report) =
                    self.setback(turn, super::progress::Setback::Overflow, step, ui)?
                {
                    return Ok(crate::agent::run::engine::Tick::Finished(report));
                }
                self.request_phase
                    .recover("context overflow; bounded compaction retry");
                // Consecutive overflows without a reply; a received reply restarts the count.
                turn.overflow_retries += 1;
                let promote_output = backend.output_budget(requested_tokens);
                if self.try_promote_context(backend, request_context, promote_output, true, ui) {
                    turn.max_tokens = backend.output_budget(turn.harness.max_tokens);
                    turn.compaction_failed = false;
                    turn.compaction_retry_floor = None;
                    turn.budget.give_back();
                    return Ok(crate::agent::run::engine::Tick::Again);
                }
                // The provider measured the prompt it rejected. Calibrate on that real count so
                // the compaction budget checks below stop trusting the estimate that let it through.
                if let Some(actual) = crate::llm::errors::overflow_prompt_tokens(&error) {
                    self.context_meter.complete(
                        request_estimate,
                        request_context,
                        Some(actual),
                        sent_messages,
                    );
                }
                ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                    crate::agent::ui::ui_event::EventKind::Recovery,
                    "context window exceeded; compacting and retrying",
                ));
                let overflow_request = Chat {
                    messages: context_before.clone(),
                    tools: offer.clone(),
                    temperature: turn.harness.temperature,
                    max_tokens: requested_tokens,
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
                        turn.compaction_failed_at(request_context);
                        self.record(step, "generation_error", json!({"error":format!("{error:#}"),"compaction_error":format!("{compaction_error:#}"),"elapsed_ms":started.elapsed().as_millis()}))?;
                        if self.interrupt.load(Ordering::SeqCst) {
                            return Ok(crate::agent::run::engine::Tick::Finished(
                                TurnReport::interrupted(step + 1),
                            ));
                        }
                        return self
                            .stop_turn(
                                ui,
                                format!("Stopped: the request exceeded the model's context window and compaction could not make room ({compaction_error:#}). The session and completed work are kept; select a larger-window model, compact manually, or say \"continue\" to retry."),
                                step + 1,
                            )
                            .map(crate::agent::run::engine::Tick::Finished);
                    }
                };
                if !outcome.continue_allowed
                    && outcome.kind
                        != crate::agent::compact::compaction::CompactionOutcomeKind::Compaction
                {
                    return self
                        .pause_after_compaction(ui, step)
                        .map(crate::agent::run::engine::Tick::Finished);
                }
                turn.compaction_succeeded();
                turn.context_rewritten(engine);
                turn.budget.give_back();
                return Ok(crate::agent::run::engine::Tick::Again);
            }
            Err(error) => {
                self.save_request_policy(
                    None,
                    json!({"state":"generation failed","elapsed_ms":started.elapsed().as_millis()}),
                    ui,
                )?;
                self.record(step, "generation_error", json!({"error":format!("{error:#}"),"elapsed_ms":started.elapsed().as_millis()}))?;
                crate::telemetry::request_failed(
                    self.config,
                    self.hermetic,
                    self.session.id(),
                    backend.model_label(),
                    started.elapsed(),
                    &error,
                );
                return Err(error.context(crate::agent::run::failure_guidance::PROVIDER_RECOVERY))
                    .map(crate::agent::run::engine::Tick::Finished);
            }
        };
        // Counted when a provider reply is received: its schemas were sent, whether the reply later
        // ends in text, a tool dispatch, or a guidance discard. Pre-reply regenerations, stream failures
        // and interceptions return earlier and are not counted.
        self.metrics.tool_trace.record_offered(&request_tools);
        if self.external_thinking_instruction().is_some() {
            reply.text = crate::agent::modes::external_thinking::strip_scratchpad(&reply.text);
            if !reply.text.is_empty() {
                ui.text_delta(&reply.text);
            }
        }
        self.context_meter.complete(
            request_estimate,
            request_context,
            reply.prompt_tokens,
            sent_messages,
        );
        turn.reasoning_budget = None;
        if repairing {
            match crate::agent::model::constrained_repair::finish(
                &mut reply,
                &turn.names,
                format!("constrained_{step}"),
            ) {
                Ok(outcome) => self.record(
                    step,
                    "constrained_repair",
                    json!({"phase":"result","outcome":outcome}),
                )?,
                Err(reason) => {
                    reply.finish_reason = "tool_calls".into();
                    self.record(
                        step,
                        "constrained_repair",
                        json!({"phase":"result","outcome":"rejected","reason":reason}),
                    )?;
                }
            }
        }
        self.step_checks.visual.delivered();
        self.save_request_policy(None,json!({"state":"response received","prompt_tokens":reply.prompt_tokens,"completion_tokens":reply.completion_tokens,"elapsed_ms":started.elapsed().as_millis(),"tool_calls":reply.tool_calls.len(),"finish_reason":reply.finish_reason}),ui)?;
        ui.usage(
            reply.prompt_tokens,
            reply.completion_tokens,
            started.elapsed().as_secs_f64(),
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
            started.elapsed(),
        );
        self.observe_provider_response(backend.model_label(), &reply, started.elapsed());
        if let Some(tokens) = reply.prompt_tokens {
            self.last_prompt_tokens = tokens;
            self.reported_messages = sent_messages;
        }
        self.probe_context();
        self.record(step, "generation", json!({"output":reply.text,"tool_calls":reply.tool_calls.len(),
                "prompt_tokens":reply.prompt_tokens,"completion_tokens":reply.completion_tokens,"cache_read_tokens":reply.cached_prompt_tokens,"cache_write_tokens":reply.cache_creation_tokens,"elapsed_ms":started.elapsed().as_millis()}))?;
        let steering = self.drain_steering(ui);
        if !steering.is_empty() {
            // Earlier accepted steering precedes the new local input in the journal.
            self.flush_accepted_live_steering(ui)?;
        }
        if self.accept_steering(steering, ui)? {
            turn.steering_pending = true;
            ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Steering, "Discarded the old response before executing its tools; replanning with your new instruction."));
            turn.budget.give_back();
            return Ok(crate::agent::run::engine::Tick::Again);
        }
        // A research checkpoint is a tool-free protocol step. Handle it before length/inline
        // recovery and completion so retained schemas cannot authorize any effects.
        if research_step {
            use crate::agent::workflow::research::Synthesis;
            let result = engine.synthesize(&reply);
            let message = match result {
                Synthesis::Accepted => {
                    self.push(llm::assistant_message(&reply))?;
                    self.record(step, "research_synthesis", json!({"accepted":true}))?;
                    "Research checkpoint recorded as unverified model findings. Resume the existing task: act on the supported path, deliver the requested report, or investigate the stated remaining question. Tools, scope, permissions and completion checks are unchanged.".to_owned()
                }
                Synthesis::Retry(message) => {
                    record_rejected_reply(&self.usage_scope, backend, repairing)?;
                    self.record(step, "research_synthesis", json!({"accepted":false}))?;
                    ui.inference_discarded("Research checkpoint not satisfied; no tools from this reply were executed.");
                    message
                }
                Synthesis::Incomplete(message) => {
                    record_rejected_reply(&self.usage_scope, backend, repairing)?;
                    return self
                        .stop_turn(ui, message, step + 1)
                        .map(crate::agent::run::engine::Tick::Finished);
                }
            };
            turn.pending_nudges
                .push(json!({"role":"user","dial_transient":true,"content":message}));
            return Ok(crate::agent::run::engine::Tick::Again);
        }
        if reply.finish_reason == "length" {
            if self.config.reasoning_guard.enabled
                && reply.text.trim().is_empty()
                && reply.tool_calls.is_empty()
            {
                turn.reasoning_budget = Some(requested_tokens);
                turn.recovering_output = true;
                self.request_phase
                    .recover("output budget consumed without an action or answer");
                self.metrics.reasoning_interceptions += 1;
                let reason = "the output token limit was spent without any answer or tool call";
                self.record(
                    step,
                    "reasoning_gate",
                    json!({"reason":reason,"elapsed_ms":started.elapsed().as_millis()}),
                )?;
                let plan_gate = self.config.reasoning_guard.require_plan && !self.todo.has_open();
                let recovered = if plan_gate {
                    self.reasoning_recovery.intercept_planned(reason)
                } else {
                    self.reasoning_recovery.intercept(reason)
                };
                turn.plan_after_gate = plan_gate && recovered.is_ok();
                match recovered {
                    Ok(message) => {
                        ui.notice(&message);
                        self.push(
                            json!({"role":"user","content":message,"dial_reminder":"reasoning"}),
                        )?;
                    }
                    Err(message) => {
                        return self
                            .stop_turn(ui, message, step + 1)
                            .map(crate::agent::run::engine::Tick::Finished);
                    }
                }
                return Ok(crate::agent::run::engine::Tick::Again);
            }
            match engine.length(
                requested_tokens,
                self.context_window(),
                self.last_prompt_tokens,
                &reply,
            ) {
                crate::agent::safety::guard::LengthAction::Retry { max_tokens: raised } => {
                    if let Some(report) =
                        self.setback(turn, super::progress::Setback::Truncation, step, ui)?
                    {
                        return Ok(crate::agent::run::engine::Tick::Finished(report));
                    }
                    self.request_phase
                        .recover("output was truncated; retry retains an expanded token budget");
                    turn.recovering_output = true;
                    ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Recovery, format!("the model hit its output limit ({requested_tokens} tokens); retrying with {raised}")));
                    turn.max_tokens = raised;
                    turn.refund_unusable(ui);
                    return Ok(crate::agent::run::engine::Tick::Again);
                }
                crate::agent::safety::guard::LengthAction::Nudge { partial, message } => {
                    // Partial output that keeps advancing is new work, not a failed recovery.
                    if engine.length_stalling()
                        && let Some(report) =
                            self.setback(turn, super::progress::Setback::Truncation, step, ui)?
                    {
                        return Ok(crate::agent::run::engine::Tick::Finished(report));
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
                    return Ok(crate::agent::run::engine::Tick::Again);
                }
                crate::agent::safety::guard::LengthAction::Incomplete(message) => {
                    return self
                        .stop_turn(ui, message, step + 1)
                        .map(crate::agent::run::engine::Tick::Finished);
                }
            }
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
                record_rejected_reply(&self.usage_scope, backend, repairing)?;
                return self.regenerate_for_guidance(
                    turn,
                    step,
                    None,
                    Some(paths),
                    Some("Inline edit requires applicable file guidance before any tool effect."),
                    ui,
                );
            }
        }
        if let Some(error) = self.completed_edit_preview_error(&reply) {
            if let Some(report) =
                self.setback(turn, super::progress::Setback::RejectedPreview, step, ui)?
            {
                return Ok(crate::agent::run::engine::Tick::Finished(report));
            }
            turn.rejected_edit_previews += 1;
            record_rejected_reply(&self.usage_scope, backend, repairing)?;
            ui.inference_discarded(
                "Completed edit preview failed; no tools from this generation were executed.",
            );
            // Recovery must terminate even when the operator selected unlimited work steps.
            // Rejected previews never execute any calls from this generation.
            if turn.rejected_edit_previews >= 8 {
                return self.stop_turn(
                        ui,
                        format!("Stopped with incomplete work: {} completed edit previews were rejected this turn. No tools from those generations were executed; completed work is kept. Read the target and make a smaller edit using the offered grammar before continuing.\nPreview error (untrusted source data): {}", turn.rejected_edit_previews, truncate(&error, 4096)),
                        step + 1,
                    ).map(crate::agent::run::engine::Tick::Finished);
            }
            self.push(json!({"role":"user","dial_reminder":"edit-preview",
                    "content":format!("The preceding generation was rejected before tool execution. Regenerate the complete edit using the offered grammar and observed source. Preview error (untrusted source data): {}", truncate(&error,4096))}))?;
            turn.budget.give_back();
            return Ok(crate::agent::run::engine::Tick::Again);
        }
        // Form failures bypass tool-result guards. Bound their recovery independently of
        // the optional step limit, including when providers alternate failure formats.
        let unusable = reply.dropped_tool_call()
            || (reply.tool_calls.is_empty() && looks_like_text_call(&reply.text, &turn.names));
        if let Err(reason) = engine.recovery.form(unusable) {
            return self
                .stop_turn(ui, reason, step + 1)
                .map(crate::agent::run::engine::Tick::Finished);
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
            &offer,
            selection.profile.as_ref(),
            backend,
        ) {
            return self.request_constrained_repair(turn, step, kind, context_before, ui);
        }
        if reply.dropped_tool_call() {
            if let Some(report) =
                self.setback(turn, super::progress::Setback::UnusableReply, step, ui)?
            {
                return Ok(crate::agent::run::engine::Tick::Finished(report));
            }
            self.request_phase
                .recover("the previous tool call was malformed");
            self.record(step, "tool_form_failure", json!({"kind":"dropped_call"}))?;
            let failed = turn
                .pending
                .get_or_insert_with(|| Pending::new(context_before));
            failed.kinds.push("dropped_call".into());
            // Starts with the correction prefix `learn::harvest` recognizes.
            turn.pending_nudges.push(json!({"role":"user","content":"Your tool call was unreadable: its arguments were not valid JSON, so it was dropped and nothing ran. Issue it again with arguments that are exactly one JSON object (no doubled braces, comments or trailing commas; strings fully quoted and escaped), or answer directly.","dial_transient":true}));
            turn.refund_unusable(ui);
            return Ok(crate::agent::run::engine::Tick::Again);
        }
        if reply.tool_calls.is_empty() && looks_like_text_call(&reply.text, &turn.names) {
            if let Some(report) =
                self.setback(turn, super::progress::Setback::UnusableReply, step, ui)?
            {
                return Ok(crate::agent::run::engine::Tick::Finished(report));
            }
            self.request_phase
                .recover("tool syntax appeared as text instead of a valid call");
            // The model wrote a tool call as plain text instead of using the tool-call channel.
            // That is a form failure (and a learnable one): do not run it, do not hide it.
            self.record(step, "tool_form_failure", json!({"kind":"text_call"}))?;
            let failed = turn
                .pending
                .get_or_insert_with(|| Pending::new(context_before));
            failed.kinds.push("text_call".into());
            failed.reject(json!({"role":"assistant","content":reply.text.clone()}));
            self.push(json!({"role":"assistant","content":reply.text}))?;
            self.push(json!({"role":"user","content":"That looked like a tool call written as plain text, so nothing ran. Call the tool through the tool-calling interface, or answer in words."}))?;
            turn.refund_unusable(ui);
            return Ok(crate::agent::run::engine::Tick::Again);
        }
        match engine
            .recovery
            .stopped(&reply.text, !reply.tool_calls.is_empty())
        {
            crate::agent::run::turn_recovery::StopAction::Accept => {}
            crate::agent::run::turn_recovery::StopAction::Continue { reason, message } => {
                if let Some(report) =
                    self.setback(turn, super::progress::Setback::NoAnswer, step, ui)?
                {
                    return Ok(crate::agent::run::engine::Tick::Finished(report));
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
                return Ok(crate::agent::run::engine::Tick::Again);
            }
            crate::agent::run::turn_recovery::StopAction::Incomplete(reason) => {
                return self
                    .stop_turn(ui, reason, step + 1)
                    .map(crate::agent::run::engine::Tick::Finished);
            }
        }
        turn.stall_continues = 0;
        ui.message_settled();
        if reply.tool_calls.is_empty() {
            self.push(json!({"role":"assistant","content":reply.text}))?;
            self.observe_assistant_message(&reply.text, step, ui);
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
            if self.exploration.active.is_some() {
                self.push(json!({"role":"user","dial_exploration_reminder":true,
                        "content":"Investigation checkpoint is active. Call rewind with your findings before finishing; the report replaces intermediate context, not actual effects or current user instructions."}))?;
                return Ok(crate::agent::run::engine::Tick::Again);
            }
            if self.plan_mode && !crate::agent::verify::completion::open_work(&self.todo).is_empty()
            {
                return Ok(crate::agent::run::engine::Tick::Finished(TurnReport {
                    text: reply.text,
                    outcome: TurnOutcome::NeedsInput,
                    steps: step + 1,
                    interrupted: false,
                    trajectory: None,
                }));
            }
            if let Some(outcome) = crate::agent::verify::completion::declared_outcome(&reply.text)
                && (outcome != TurnOutcome::Completed || !self.advisor_enabled())
            {
                self.disclose_turn_changes(ui);
                return Ok(crate::agent::run::engine::Tick::Finished(TurnReport {
                    text: reply.text,
                    outcome,
                    steps: step + 1,
                    interrupted: false,
                    trajectory: None,
                }));
            }
            if !self.hermetic && crate::agent::verify::completion::awaiting_input(&reply.text) {
                ui.notice("Waiting for your answer; unfinished todos are preserved.");
                return Ok(crate::agent::run::engine::Tick::Finished(TurnReport {
                    text: reply.text,
                    outcome: TurnOutcome::NeedsInput,
                    steps: step + 1,
                    interrupted: false,
                    trajectory: None,
                }));
            }
            if self.await_background_jobs(ui) {
                return Ok(crate::agent::run::engine::Tick::Again);
            }
            if self.before_yield(backend, &turn.names, &mut turn.pending_nudges, ui)? {
                return Ok(crate::agent::run::engine::Tick::Again);
            }
            match turn.completion.stopped(&self.todo) {
                crate::agent::verify::completion::StopAction::Accept => {}
                crate::agent::verify::completion::StopAction::Blocked(blockers) => {
                    ui.notice(&blockers);
                    self.disclose_turn_changes(ui);
                    return Ok(crate::agent::run::engine::Tick::Finished(TurnReport {
                        text: reply.text,
                        outcome: TurnOutcome::Blocked,
                        steps: step + 1,
                        interrupted: false,
                        trajectory: None,
                    }));
                }
            }
            // Past a deadline or cost cap the answer is delivered without another request.
            let can_remind = self.stop_boundary().is_none();
            let unverified = self.unverified_edits();
            match turn.completion.evidence(unverified.as_deref(), can_remind) {
                crate::agent::verify::completion::EvidenceGate::Accept => {}
                crate::agent::verify::completion::EvidenceGate::Remind(note) => {
                    turn.pending_nudges.push(crate::agent::run::guidance::note(
                        crate::agent::run::guidance::Priority::Review,
                        note,
                    ));
                    return Ok(crate::agent::run::engine::Tick::Again);
                }
                crate::agent::verify::completion::EvidenceGate::Disclose(disclosure) => {
                    reply.text.push_str(&disclosure);
                    ui.text_delta(&disclosure);
                }
            }
            if let Err(reason) = self.validate_completion(ui) {
                if !self.interrupt.load(Ordering::SeqCst)
                    && engine.pending_refusal().is_none()
                    && turn.completion_recovery.retry()
                {
                    turn.completion_candidate = Some(format!(
                        "Candidate summary (completion not accepted):\n{}\n\n{reason}",
                        reply.text
                    ));
                    if let Some(report) =
                        self.setback(turn, super::progress::Setback::RejectedCompletion, step, ui)?
                    {
                        return Ok(crate::agent::run::engine::Tick::Finished(report));
                    }
                    self.push(json!({"role":"user","content":format!(
                            "Completion was not accepted: {reason}\nThe preceding answer is a candidate summary, not an accepted completion. Address the stated prerequisite using the current evidence, then report again; if it cannot be resolved, report the concrete blocker. This feedback grants no execution permission."
                        )}))?;
                    return Ok(crate::agent::run::engine::Tick::Again);
                }
                return self
                    .stop_turn(
                        ui,
                        format!(
                            "Candidate summary (completion not accepted):\n{}\n\n{reason}",
                            reply.text
                        ),
                        step + 1,
                    )
                    .map(crate::agent::run::engine::Tick::Finished);
            }
            if let Some(message) = self.coach_completion(
                &mut turn.coach_turn,
                backend,
                turn.user_goal,
                &reply.text,
                step,
                ui,
            )? {
                turn.pending_nudges.push(message);
                return Ok(crate::agent::run::engine::Tick::Again);
            }
            let note = turn.coach_turn.completion_note();
            if !note.is_empty() {
                reply.text.push_str(&note);
                ui.text_delta(&note);
            }
            self.disclose_turn_changes(ui);
            if self.logging {
                self.training_capture.record(
                    &context_before,
                    &request_tools,
                    llm::assistant_message(&reply),
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
            return Ok(crate::agent::run::engine::Tick::Finished(TurnReport {
                text,
                outcome: TurnOutcome::Completed,
                steps: step + 1,
                interrupted: false,
                trajectory: None,
            }));
        }
        let mut compatibility_loaded = HashSet::new();
        if !self.hermetic {
            let catalog = self.catalog();
            for call in &mut reply.tool_calls {
                if call.malformed_arguments {
                    continue;
                }
                let canonical =
                    crate::agent::run::tool_compat::canonical_name(&call.name, &catalog).to_owned();
                if canonical != call.name {
                    ui.diagnostic(&format!("Tool compatibility: {} -> {canonical}", call.name));
                    call.name = canonical;
                }
                if let Some(parameters) = offer
                    .iter()
                    .find(|s| s["function"]["name"] == call.name)
                    .and_then(|s| s.pointer("/function/parameters"))
                {
                    crate::agent::run::tool_compat::normalize(
                        &call.name,
                        parameters,
                        &mut call.arguments,
                    );
                } else if !planning_required
                    && !self.plan_mode
                    && engine.rejection_at(&self.workspace, call).is_none()
                    && catalog.contains(&call.name)
                {
                    // Capability discovery is not permission to execute a call whose schema
                    // was absent. Enable the real tool, then require a fresh call next round.
                    compatibility_loaded.insert(call.name.clone());
                }
            }
            if !compatibility_loaded.is_empty() {
                let mut loaded: Vec<_> = compatibility_loaded.iter().cloned().collect();
                loaded.sort();
                self.activate_tools(&loaded)?;
                ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                    crate::agent::ui::ui_event::EventKind::Tools,
                    format!("Enabled {} for the next round", loaded.join(", ")),
                ));
            }
        }
        // Hermetic turns disable repair; its live endpoint probe is not evaluated either.
        if let Some(kind) = self.constrained_repair_kind(
            turn,
            true,
            &reply,
            &offer,
            selection.profile.as_ref(),
            backend,
        ) {
            return self.request_constrained_repair(turn, step, kind, context_before, ui);
        }
        // Compatibility can decode JSON containers and rename arguments. Recheck that exact
        // executable form before journaling any assistant/tool pair or prefetching a read.
        let mut normalized_rules =
            turn.rule_catalog
                .monitor(&turn.rule_root, &turn.rule_state, "main");
        let normalized_hit = normalized_rules.completed(&reply);
        if let Some(failure) = normalized_rules.failure {
            bail!("{failure}");
        }
        let scoped_paths = turn.rule_catalog.pending_paths(
            &turn.rule_root,
            &turn.rule_state,
            "main",
            &reply.tool_calls,
        );
        if normalized_hit.is_some() || scoped_paths.is_some() {
            record_rejected_reply(&self.usage_scope, backend, repairing)?;
            return self.regenerate_for_guidance(
                turn,
                step,
                normalized_hit,
                scoped_paths,
                Some("Applicable portable guidance must be delivered before the normalized tool attempt; no tools executed."),
                ui,
            );
        }
        if turn.final_step {
            // The wrap-up step answers; tools it still asks for would outlive the step limit.
            self.record(
                step,
                "final_step_tools_withheld",
                json!({"calls":reply.tool_calls.len()}),
            )?;
            let mut text = reply.text.trim().to_owned();
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&format!(
                "The {} tool call(s) requested on the final step were not run.\n\n{}",
                reply.tool_calls.len(),
                super::step_limit_reason(turn.budget.limit())
            ));
            return self
                .stop_turn(ui, text, step + 1)
                .map(crate::agent::run::engine::Tick::Finished);
        }
        if turn
            .rule_catalog
            .observe_paths(&turn.rule_root, &mut turn.rule_state, &reply.tool_calls)
            .map_err(anyhow::Error::msg)?
        {
            let mut control = self.session.control_state();
            turn.rule_state.save(&mut control);
            self.session.set_control_state(control)?;
        }
        if let Some(warm) = &self.cache_warm {
            // The settled prefix is the exact bytes sent: the warmer replays it near expiry.
            let settled = llm::Chat {
                messages: context_before.clone(),
                tools: request_tools.clone(),
                temperature: request.temperature,
                max_tokens: request.max_tokens,
                seed: request.seed,
            };
            warm.settled(&settled, reply.prompt_tokens, &self.usage_scope);
        }
        self.execute_round(
            turn,
            engine,
            backend,
            schemas,
            ui,
            Round {
                step,
                single_call,
                planning_required,
                reply,
                context_before,
                request_tools,
                temperature: request.temperature,
                max_tokens: request.max_tokens,
                seed: request.seed,
                compatibility_loaded,
                repeat_streak,
                streamed,
                outcome: ToolOutcome::default(),
            },
        )
    }

    /// Discard a generation that portable guidance must precede and request it again with the
    /// guidance delivered: a fired rule is remembered, file-scoped rules are activated for
    /// `paths`. Bounded by `portable_rules.max_regenerations`; at the limit the turn stops
    /// gracefully, and nothing from the discarded generation ran.
    pub(super) fn regenerate_for_guidance(
        &mut self,
        turn: &mut TurnState<'_>,
        step: u32,
        hit: Option<crate::agent::safety::rules::Hit>,
        paths: Option<std::collections::BTreeSet<String>>,
        discarded: Option<&str>,
        ui: &mut dyn Ui,
    ) -> Result<crate::agent::run::engine::Tick<TurnReport>> {
        if turn.rule_regenerations >= self.config.portable_rules.max_regenerations {
            return self
                .stop_turn(
                    ui,
                    format!(
                        "Stopped: portable guidance required {} regenerations this turn, its limit; no tools from the discarded generation were executed. Completed work is kept; say \"continue\" to resume, or raise portable_rules.max_regenerations.",
                        turn.rule_regenerations
                    ),
                    step + 1,
                )
                .map(crate::agent::run::engine::Tick::Finished);
        }
        if let Some(report) = self.setback(
            turn,
            super::progress::Setback::GuidanceRegeneration,
            step,
            ui,
        )? {
            return Ok(crate::agent::run::engine::Tick::Finished(report));
        }
        turn.rule_regenerations += 1;
        turn.rule_state.set_regenerations(turn.rule_regenerations);
        if let Some(hit) = hit {
            turn.rule_state
                .remember(&hit, "main")
                .map_err(anyhow::Error::msg)?;
            ui.notice(&hit.to_string());
        }
        if let Some(paths) = paths {
            turn.rule_state
                .activate(paths)
                .map_err(anyhow::Error::msg)?;
        }
        if let Some(discarded) = discarded {
            ui.inference_discarded(discarded);
        }
        let mut control = self.session.control_state();
        turn.rule_state.save(&mut control);
        self.session.set_control_state(control)?;
        self.record(
            step,
            "portable_rule_regeneration",
            json!({"count":turn.rule_regenerations}),
        )?;
        turn.budget.give_back();
        Ok(crate::agent::run::engine::Tick::Again)
    }

    /// The malformed tool-call form that justifies the turn's one constrained retry, when
    /// `eligible`, the session is not hermetic and the live route enforces structured output.
    fn constrained_repair_kind(
        &self,
        turn: &TurnState<'_>,
        eligible: bool,
        reply: &crate::llm::Reply,
        offer: &[Value],
        profile: Option<&crate::llm::profiles::ModelProfile>,
        backend: &Backend,
    ) -> Option<&'static str> {
        let model = self.auth_config.selection();
        turn.repair.trigger(
            eligible
                && !self.hermetic
                && crate::agent::model::constrained_repair::enabled(
                    &self.config.assistance,
                    self.hermetic,
                    &model.provider,
                    &model.model,
                    profile,
                    backend.endpoint_structured_output(),
                ),
            reply,
            offer,
            &turn.names,
        )
    }

    /// Discards the malformed generation and requests the constrained retry for `kind`. The
    /// retry is an unusable reply for the progress ledger, which may end the turn instead.
    fn request_constrained_repair(
        &mut self,
        turn: &mut TurnState<'_>,
        step: u32,
        kind: &'static str,
        context_before: Vec<Value>,
        ui: &mut dyn Ui,
    ) -> Result<crate::agent::run::engine::Tick<TurnReport>> {
        if let Some(report) =
            self.setback(turn, super::progress::Setback::UnusableReply, step, ui)?
        {
            return Ok(crate::agent::run::engine::Tick::Finished(report));
        }
        let model = self.auth_config.selection();
        self.record(step, "constrained_repair", json!({"phase":"requested","trigger":kind,"provider":model.provider,"model":model.model}))?;
        self.request_phase
            .recover("the previous tool call was malformed");
        let failed = turn
            .pending
            .get_or_insert_with(|| Pending::new(context_before));
        failed.kinds.push(kind.into());
        turn.repair.request();
        turn.refund_unusable(ui);
        Ok(crate::agent::run::engine::Tick::Again)
    }
}

/// Tool names in offer order; the turn's names and schemas are built one-to-one.
fn offered_names(schemas: &[Value]) -> Vec<String> {
    schemas
        .iter()
        .filter_map(|schema| schema.pointer("/function/name").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

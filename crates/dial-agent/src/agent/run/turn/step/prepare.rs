//! Build the exact request and enforce context headroom before any inference.
use super::*;

impl Agent<'_> {
    pub(super) fn prepare_step_request(
        &mut self,
        context: &mut StepContext<'_, '_>,
        step: u32,
    ) -> Result<Phase<PreparedRequest>> {
        let StepContext {
            turn,
            engine,
            backend,
            schemas,
            ui,
        } = context;
        let turn = &mut **turn;
        let engine = &mut **engine;
        let backend = &mut **backend;
        let schemas = &mut **schemas;
        let ui = &mut **ui;
        for note in turn.settle_delivered(|mark| {
            self.session
                .weigh_from(mark, |message| usize::from(message["role"] == "assistant"))
        }) {
            if crate::agent::run::search_frontier::EmptySearches::is_advice(&note) {
                self.step_checks.empty_searches.delivered_advice(&note);
            }
        }
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
        let route = self.route.generation();
        self.before_request(backend, &turn.names, &mut turn.pending_nudges, ui)?;
        // A mode hook (prewalk handoff) may have installed another model: size for it.
        if self.route.generation() != route {
            turn.max_tokens = backend.output_budget(turn.harness.max_tokens);
        }
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
        // Recovery budgets remain explicit; ordinary unlimited requests inherit the host's limit.
        if !turn.recovering_output && turn.reasoning_budget.is_none() {
            self.apply_local_output_limit(&mut selection.profile, turn.harness.max_tokens);
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
                    "<dial-notice kind=\"planning_required\">\nThe previous generation stalled without an active plan, so this request offers only `todo` and `finish`. If the request is already answered, answer directly. Otherwise create a short todo plan (todo set for an empty list, append to keep existing tasks): 1-3 concrete open questions, then the known implementation and verification steps. A prose plan is not a todo update.\n</dial-notice>"}));
            offer.retain(|s| matches!(s["function"]["name"].as_str(), Some("todo" | "finish")));
            turn.names.retain(|name| name == "todo" || name == "finish");
        }
        turn.planning_notice = planning_required;
        let mut call_reasons = if self.hermetic {
            Vec::new()
        } else {
            crate::reliability::single_call_reasons(
                self.config,
                turn.measured.as_ref(),
                &turn.reliability,
            )
        };
        let measured_single = !call_reasons.is_empty();
        // Local and measured constraints compose; record every applicable cause.
        if !self.hermetic && self.local_one_call() {
            call_reasons.push("small_local_tier_without_batch_evidence");
        }
        let single_call = !call_reasons.is_empty();
        request_control["reliability"]["call_policy"] = json!({
            "mode": if single_call { "one_call" } else { "batch" },
            "reasons": call_reasons,
        });
        // Re-render deferred advice, rather than replaying a batch recommendation after the
        // request policy changes. Keep the target pending if higher-priority guidance wins.
        turn.pending_nudges
            .retain(|note| !crate::agent::run::search_frontier::EmptySearches::is_advice(note));
        let discovery_advice = (!self.hermetic)
            .then(|| self.step_checks.empty_searches.advice(single_call))
            .flatten();
        if let Some(advice) = &discovery_advice {
            turn.pending_nudges.push(crate::agent::run::guidance::note(
                crate::agent::run::guidance::Priority::Loop,
                advice.clone(),
            ));
        }
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
        turn.pending_nudges.extend(self.take_monitor_context());
        messages.extend(turn.pending_nudges.iter().cloned());
        // The redirect is stated once; if the identical repeat continues, the stronger
        // guidance replaces it. Neither stage ends the turn.
        let repeat_streak = crate::agent::safety::guard::RepeatedToolCall::detect(
            &messages,
            crate::agent::safety::stream_guards::TOOL_CALL_LOOP_THRESHOLD,
            crate::agent::safety::stream_guards::TOOL_CALL_LOOP_EXEMPT_TOOLS,
        );
        if let Some((event, guidance)) =
            crate::agent::safety::stream_guards::repetition_guidance_with_streak(
                &messages,
                repeat_streak.as_ref(),
            )
        {
            ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                crate::agent::ui::ui_event::EventKind::Recovery,
                event,
            ));
            messages.push(crate::agent::run::guidance::note(
                crate::agent::run::guidance::Priority::Repetition,
                guidance,
            ));
        }
        // Only lean local routes get the forced tool-free checkpoint; elsewhere a long
        // observation run receives the advisory note and keeps its tools.
        let research_forced = self.lean_completion_route();
        let research_step = research_forced && engine.research_required();
        if let Some(note) = engine.research_prompt(research_forced) {
            messages.push(note);
        }
        // One advisory note per request, by priority, then one merged harness message; protocol
        // notes and gates are unaffected. A previous request that was discarded before any reply
        // delivered nothing.
        turn.emissions.retract();
        crate::agent::run::guidance::assemble(&mut messages, notes_start, &mut turn.emissions);
        if discovery_advice.as_ref().is_some_and(|advice| {
            !messages.iter().any(|message| {
                message["content"]
                    .as_str()
                    .is_some_and(|text| text.contains(advice))
            })
        }) {
            turn.pending_nudges
                .retain(|note| !crate::agent::run::search_frontier::EmptySearches::is_advice(note));
        }
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
        let mut accounting = crate::agent::compact::compaction::request_accounting_with_policy(
            &request,
            &self.config.compaction,
            compaction_identity,
        );
        let mut request_estimate = accounting.tokens;
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
                request_estimate = accounting.push_message(
                    &guidance,
                    &self.config.compaction,
                    compaction_identity,
                );
                request.messages.push(guidance);
            }
        }
        let mut request_overhead =
            crate::agent::window::context_meter::overhead(&request, &accounting);
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
            return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
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
                    native_supported: crate::agent::compact::compaction::native_supported(
                        backend,
                        &self.config.compaction,
                        compaction_identity,
                    ),
                },
            )?;
            match action {
                crate::agent::compact::compaction_runtime::SpeculationAction::Claim => {
                    match self.claim_background_compaction(ui, reason, Some(&request), Some(backend)) {
                            Ok(Some(outcome)) => {
                                if !outcome.continue_allowed { return self.pause_after_compaction(ui, step).map(crate::agent::run::engine::Tick::Finished).map(ControlFlow::Break); }
                                turn.compaction_succeeded();
                                turn.context_rewritten(engine);
                                turn.budget.give_back();
                                return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
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
                            .map(crate::agent::run::engine::Tick::Finished)
                            .map(ControlFlow::Break);
                    }
                    // The compacted context no longer holds the results duplicate memory cited.
                    // Re-check at the next request boundary; compaction may be needed again
                    // after subsequent tool results in this same autonomous turn.
                    turn.compaction_succeeded();
                    self.compaction_retry_carry = None;
                    turn.context_rewritten(engine);
                    turn.budget.give_back();
                    return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
                }
                Err(error) => {
                    turn.compaction_failed_at(request_context);
                    if error.is::<crate::agent::compact::compaction::Unprofitable>()
                        || error.is::<crate::agent::compact::compaction::NothingToCompact>()
                    {
                        self.compaction_retry_carry = turn.compaction_retry_floor;
                    }
                    ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                        crate::agent::ui::ui_event::EventKind::Recovery,
                        format!("context compaction could not make enough room ({error:#})"),
                    ));
                    if self.interrupt.load(Ordering::SeqCst) {
                        return Ok(ControlFlow::Break(
                            crate::agent::run::engine::Tick::Finished(TurnReport::interrupted(
                                step + 1,
                            )),
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
            return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
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
        // Where the route keeps `tools` byte-stable, the narrowing above (planning gate,
        // single-call simplification) and mode state change only which tools are callable, and
        // loads change no definition. Accounting, compaction and admission used the callable set.
        self.wire_tool_surface(backend.tool_exposure(), schemas, &mut request)?;
        Ok(ControlFlow::Continue(PreparedRequest {
            step,
            prep_started,
            request,
            selection,
            request_control,
            local_control,
            request_overhead,
            request_estimate,
            request_context,
            sent_messages,
            lightweight,
            single_call,
            planning_required,
            research_step,
            repeat_streak,
        }))
    }
}

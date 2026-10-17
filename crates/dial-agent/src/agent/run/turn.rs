//! Parent-turn orchestration: live steering, durable session state and learning policy.
//! Model transport, result recovery and native execution are shared with worker controllers.
use crate::agent::modes::lifecycle::Lifecycle;
use crate::agent::*;

impl Agent<'_> {
    pub(in crate::agent) fn run_loop(
        &mut self,
        backend: &mut Backend,
        names: &[String],
        schemas: &mut Vec<Value>,
        max_steps: u32,
        user_goal: &str,
        ui: &mut dyn Ui,
    ) -> Result<TurnReport> {
        let mut names = names.to_vec();
        let rule_root = self.workspace.root().to_path_buf();
        let mut rule_catalog = Arc::new(if !self.hermetic && self.config.portable_rules.enabled {
            self.layout
                .as_ref()
                .map(|layout| crate::agent::safety::rules::Catalog::discover(layout, &rule_root))
                .unwrap_or_default()
        } else {
            crate::agent::safety::rules::Catalog::default()
        });
        self.context_rule_catalog = Some(rule_catalog.clone());
        for warning in &rule_catalog.warnings {
            ui.notice(warning);
        }
        let mut rule_state =
            crate::agent::safety::rules::State::restore(&self.session.control_state());
        let instruction = self.session.control_state()["current_instruction"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        rule_state.begin(&rule_root, &instruction);
        let mut rule_regenerations: u32 = rule_state.regenerations();
        rule_state.set_regenerations(0);
        rule_state.clear_callback();
        let mut control = self.session.control_state();
        rule_state.save(&mut control);
        self.session.set_control_state(control)?;
        let mut reviewed_paths = std::collections::BTreeSet::new();
        let mut prechecked_paths = std::collections::BTreeSet::new();
        let mut revalidated_paths = std::collections::BTreeSet::new();
        let h = self.config.effective_harness(&self.version.harness)?;
        let adapter = self.version.adapter.clone();
        let mut pending: Option<Pending> = None;
        let mut overflow_retries = 0u32;
        let mut compaction_failed = false;
        // After a failed blocking compaction, retry only once the request has grown by a real
        // amount (or would overflow): retrying every tool step re-ran the whole method chain.
        let mut compaction_retry_floor: Option<u64> = None;
        let mut max_tokens = backend.output_budget(h.max_tokens);
        self.report_retained_fallback(ui);
        let mut recovering_output = false;
        let mut engine = crate::agent::run::engine::Engine::default();
        let turn_cancel = self.cancellation.turn(self.interrupt.clone());
        let mut repair = crate::agent::model::constrained_repair::Repair::default();
        let mut stall_continues = 0u32;
        let mut rejected_edit_previews = 0u32;
        let mut reasoning_budget = None;
        // `thinking = "auto"`: judge this user turn's effort once from its prompt, not per step.
        let mut turn_effort: Option<crate::llm::profiles::ReasoningEffort> = None;
        if self.config.thinking_auto && !self.hermetic {
            match crate::judgment::judge_turn_thinking(
                &self.auth_config,
                &self.config.user_dir,
                &instruction,
                Some(&self.usage_scope),
                self.interrupt.clone(),
            ) {
                Ok(judged) => {
                    turn_effort = crate::judgment::effort_for_choice(&judged.choice);
                    self.last_auto_thinking = Some(judged.choice);
                }
                Err(error) => ui.diagnostic(&format!(
                    "Auto thinking unavailable ({error:#}); using the model's default effort."
                )),
            }
        }
        let mut todo_recovery = crate::agent::workflow::todo_recovery::TodoRecovery::default();
        let mut solo_todo_noted = false;
        let mut pending_nudges: Vec<Value> = Vec::new();
        let mut coach_turn = modes::advisor::CoachTurn::default();
        let mut automatic_coach_pending = false;
        let mut auto_continues = 0u32;
        let mut planning_notice = false;
        let mut plan_after_gate = false;
        let measured = if self.hermetic
            || adapter.is_some()
            || self.config.steering.enabled
            || crate::llm::steering::specification(self.config, &self.config.model)
                .ok()
                .flatten()
                .is_some()
        {
            None
        } else {
            crate::reliability::measured(self.config)
        };
        let mut reliability: crate::reliability::Stats =
            serde_json::from_value(self.session.control_state()["reliability"]["stats"].clone())
                .unwrap_or_default();
        let turn_started = self.workflow_started;
        if !self.hermetic {
            let mut control = self.session.control_state();
            control["reliability"]["measured"] = json!(measured);
            self.session.set_control_state(control.clone())?;
            ui.request_policy(&control);
        }
        let mut policy_signature = String::new();
        let mut stall = crate::agent::safety::guard::StallGuard::default();
        let mut pace = crate::agent::safety::guard::PaceGuard::default();
        let mut completion = crate::agent::verify::completion::CompletionGuard;
        let mut steering_pending = false;
        let mut budget = crate::agent::safety::guard::StepBudget::new(
            max_steps,
            if self.hermetic {
                0
            } else {
                crate::agent::safety::guard::StepBudget::FREE_RETRIES
            },
        );
        let mut iteration = 0u32;
        let turn_cost_start = self.session_cost().unwrap_or(0.0);
        let driven = engine.drive(|engine| {
            if !budget.begin() { return Ok(crate::agent::run::engine::Tick::Exhausted); }

            rule_state = crate::agent::safety::rules::State::restore(&self.session.control_state());
            rule_regenerations = rule_state.regenerations();
            if rule_state.callback_pending() {
                rule_catalog = Arc::new(if !self.hermetic && self.config.portable_rules.enabled {
                    self.layout
                        .as_ref()
                        .map(|layout| crate::agent::safety::rules::Catalog::discover(layout, &rule_root))
                        .unwrap_or_default()
                } else {
                    crate::agent::safety::rules::Catalog::default()
                });
                self.context_rule_catalog = Some(rule_catalog.clone());
                for warning in &rule_catalog.warnings {
                    ui.notice(warning);
                }
                rule_state.clear_callback();
            }
            let step = iteration;
            iteration = iteration.saturating_add(1);
            if !self.hermetic {
                let spent = self.session_cost().unwrap_or(0.0);
                let caps = &self.config.budget;
                let hit = match (caps.max_session_cost_usd, caps.max_turn_cost_usd) {
                    (Some(cap), _) if cap > 0.0 && spent >= cap => Some(format!(
                        "observed session API cost ${spent:.4} reached the ${cap:.4} stop boundary"
                    )),
                    (_, Some(cap)) if cap > 0.0 && spent - turn_cost_start >= cap => Some(format!(
                        "observed API cost this turn is ${:.4}, reaching its ${cap:.4} stop boundary",
                        spent - turn_cost_start
                    )),
                    _ => None,
                };
                if let Some(why) = hit {
                    return self.stop_turn(
                        ui,
                        format!("Stopped: {why}. This is not an invoice ceiling: missing prices/usage, discarded attempts and in-flight workers may add charges. Raise [budget] in the configuration to continue."),
                        step,
                    ).map(crate::agent::run::engine::Tick::Finished);
                }
            }
            if self.modes.launch.expired() {
                return self.stop_turn(ui, "Stopped: --max-time deadline reached.".into(), step).map(crate::agent::run::engine::Tick::Finished);
            }
            let accepted_live = self.flush_accepted_live_steering(ui)?;
            if self.take_steering(ui)? || accepted_live {
                steering_pending = true;
            }
            if std::mem::take(&mut steering_pending) {
                let instruction = self.session.control_state()["current_instruction"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned();
                rule_state.begin(&rule_root, &instruction);
                let mut control = self.session.control_state();
                rule_state.save(&mut control);
                self.session.set_control_state(control)?;
                completion.reset_for_steering();
                engine.reset();

                todo_recovery = Default::default();
                stall = Default::default();

                pending = None;
                engine.recovery.reset();
                rejected_edit_previews = 0;
                recovering_output = false;
                max_tokens = backend.output_budget(h.max_tokens);
                reasoning_budget = None;
                planning_notice = false;
                plan_after_gate = false;
                reliability.consecutive_contract_failures = 0;
                names = self.offered();
                *schemas = self.schemas(&names)?;
            }
            self.pump_subagents(ui);
            self.cur_step = step;
            if self.interrupt.load(Ordering::SeqCst) {
                return Ok(crate::agent::run::engine::Tick::Finished(TurnReport {
                    text: String::new(),
                    outcome: TurnOutcome::Interrupted,
                    steps: step,
                    interrupted: true,
                    trajectory: None,
                }));
            }
            // Worker results are persisted before the request is built, so they stay in context and survive
            // an overflow retry; each one is delivered exactly once.
            self.deliver_worker_results()?;
            // A plan approved inside this turn ends planner-model use at this boundary, so implementation
            // runs on the main model. The loop owns the backend, so it is swapped in place.
            if !self.plan_mode && self.plan_model_prior.is_some() {
                self.plan_model_resume()?;
                if let Some(mut main) = self.backend.take() {
                    main.set_usage_scope(Some(self.usage_scope.clone()));
                    let mut planner = std::mem::replace(backend, main);
                    planner.release();
                }
            }
            // A fallback whose cooldown elapsed returns to the primary before anything is sized for it.
            if self.restore_primary_if_due(backend, ui)? {
                max_tokens = backend.output_budget(h.max_tokens);
            }
            let prep_started = Instant::now();
            let budget_window = self.budget_window();
            self.before_request(backend, &names, &mut pending_nudges, ui)?;
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
                    temperature: h.temperature,
                    tokens: reasoning_budget.unwrap_or(max_tokens),
                    pre_action_secs,
                    base_profile: backend.model_profile(),
                    recovering_output: recovering_output || reasoning_budget.is_some(),
                    hermetic: self.hermetic,
                },
            );
            if let Some(source) = self.apply_local_sampling(&mut selection.profile, h.temperature) {
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
                Some(&mut rule_state),
            )?;
            self.task_packet = packet_cache;
            self.superseded = superseded;
            // Opt-in (`reasoning_guard.require_plan`): after a reasoning-gate hit with no plan,
            // only `todo` is offered until one exists.
            if self.todo.has_open() {
                plan_after_gate = false;
            }
            let planning_required = !self.hermetic
                && plan_after_gate
                && !self.todo.has_open()
                && schemas.iter().any(|s| s["function"]["name"] == "todo");
            // Harness notes for this request start here; they are coalesced into one message below.
            let notes_start = messages.len();
            if planning_required {
                if !planning_notice {
                    ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Plan, "Plan required: create a todo plan to resume tools. The next request offers only todo."));
                }
                messages.push(json!({"role":"user","dial_transient":true,"content":
                    "The previous generation stalled without an active plan. If the request is already answered, answer directly. Otherwise your next tool call must create actual todos: use todo set for an empty list, or append to preserve existing tasks. Add 1-3 concrete unresolved questions with investigation=true, then the known implementation and verification steps. Do not invent a large speculative plan. A prose plan is not a todo update."}));
                schemas
                    .retain(|s| matches!(s["function"]["name"].as_str(), Some("todo" | "finish")));
                names.retain(|name| name == "todo" || name == "finish");
            }
            planning_notice = planning_required;
            engine.apply(schemas, &mut names, &mut messages);
            let single_call = !self.hermetic
                && crate::reliability::single_call(self.config, measured.as_ref(), &reliability);
            let mut contract = crate::reliability::Contract::new(schemas.clone())?;
            if single_call {
                let loaded = self.enabled_tools.iter().cloned().chain(self.discovered_tools()).collect();
                crate::reliability::simplify(&mut contract.schemas, &loaded);
                // Simplification can drop offered schemas, so the offered contract is validated again.
                contract = crate::reliability::Contract::new(contract.schemas)?;
            }
            names = contract.names.clone();
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
                && (single_call || reliability.consecutive_contract_failures > 0)
            {
                let guidance = contract.instruction(
                    single_call,
                    self.config.reliability.examples && !lightweight,
                );
                messages.push(json!({"role":"user","content":guidance,"dial_transient":true}));
            }
            todo_recovery.apply(&mut messages);
            pending_nudges.extend(
                std::mem::take(&mut self.background_notices)
                    .into_iter()
                    .map(|text| json!({"role":"user","content":text,"dial_transient":true})),
            );
            messages.extend(pending_nudges.iter().cloned());
            // The redirect is stated once; if the identical repeat continues, the stronger
            // guidance replaces it. Neither stage ends the turn.
            let repeat_streak = crate::agent::safety::guard::RepeatedToolCall::detect(
                &messages,
                crate::agent::safety::stream_guards::TOOL_CALL_LOOP_THRESHOLD,
                crate::agent::safety::stream_guards::TOOL_CALL_LOOP_EXEMPT_TOOLS,
            );
            if let Some(repeat) = &repeat_streak {
                ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Recovery, format!(
                    "Repeated tool call {}x; redirecting the model",
                    repeat.count
                )));
                let guidance = if repeat.count > crate::agent::safety::stream_guards::TOOL_CALL_LOOP_THRESHOLD {
                    crate::agent::safety::stream_guards::render_tool_call_loop_escalation(&repeat.tool_name, repeat.count, &repeat.arguments_summary, &repeat.result_summary)
                } else {
                    crate::agent::safety::stream_guards::render_tool_call_loop_redirect(&repeat.tool_name, repeat.count, &repeat.arguments_summary, &repeat.result_summary)
                };
                messages.push(json!({"role":"user","dial_transient":true,"content":guidance}));
            }
            coalesce_harness_notes(&mut messages, notes_start);
            if !self.experimental_context_owner()
                && let Some(guidance) = rule_catalog.guidance(&mut rule_state, "main")
            {
                messages.push(guidance);
            }
            rule_state.save(&mut request_control);
            let mut request = Chat {
                messages,
                tools: contract.schemas,
                temperature: selection.temperature.unwrap_or(h.temperature),
                max_tokens: selection.max_tokens,
                seed: if self.hermetic {
                    Some(42 + u64::from(step))
                } else {
                    None
                },
            };
            if let Some(local) = self.prepare_local_request(backend, &mut selection.profile) {
                self.record(step, "local_request", local)?;
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
                && crate::agent::window::context_notes::TOOL_NAMES.iter().all(|name| {
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
                    request_estimate = request_estimate.saturating_add(crate::llm::wire::estimate(&guidance));
                    request.messages.push(guidance);
                }
            }
            let request_overhead = crate::agent::window::context_meter::overhead(&request);
            let request_context = self
                .context_meter
                .predict(request_estimate)
                .max(request_estimate);
            let reason = if step == 0 {
                crate::config::compaction::CompactionReason::Threshold
            } else {
                crate::config::compaction::CompactionReason::MidTurn
            };
            self.poll_background_compaction(ui);
            let mut deferred_compaction = false;
            if !compaction_failed {
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
                                compaction_failed = false;
                                budget.give_back();
                                return Ok(crate::agent::run::engine::Tick::Again);
                            }
                            Ok(None) => {}
                            Err(error) => ui.diagnostic(&format!("Prepared compaction was not claimed ({error:#}); considering blocking maintenance.")),
                        }
                    }
                    crate::agent::compact::compaction_runtime::SpeculationAction::Defer => deferred_compaction = true,
                    crate::agent::compact::compaction_runtime::SpeculationAction::Start(method)
                    | crate::agent::compact::compaction_runtime::SpeculationAction::Refresh(method) => {
                        if action == crate::agent::compact::compaction_runtime::SpeculationAction::Refresh(method) {
                            self.compaction_runtime.cancel();
                        }
                        let native_profile = backend.compaction_profile().cloned();
                        match self.start_background_compaction(ui, Some(&request), crate::agent::compact::compaction_runtime::WorkKind::Speculative,
                            Some(method), request_context, native_profile.as_ref(), Some(backend)) {
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
            let safe_prompt_budget = crate::agent::compact::compaction::prompt_budget(
                &self.config.compaction,
                self.context_window(),
                backend.output_budget(request.max_tokens),
            );
            let retry_ready = compaction_retry_floor
                .is_none_or(|floor| request_context >= floor || request_context > safe_prompt_budget);
            if !compaction_failed
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
                match self.compact_using(backend, ui, Some(&request), &options) {
                    Ok(outcome) => {
                        if !outcome.continue_allowed {
                            return self.pause_after_compaction(ui, step).map(crate::agent::run::engine::Tick::Finished);
                        }
                        // The compacted context no longer holds the results duplicate memory cited.
                        engine.evidence_compacted();
                        // Re-check at the next request boundary; compaction may be needed again
                        // after subsequent tool results in this same autonomous turn.
                        compaction_failed = false;
                        compaction_retry_floor = None;
                        budget.give_back();
                        return Ok(crate::agent::run::engine::Tick::Again);
                    }
                    Err(error) => {
                        compaction_failed = true;
                        compaction_retry_floor = Some(request_context.saturating_add(
                            crate::config::compaction::SPECULATION_LEAD_MIN_TOKENS,
                        ));
                        ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Recovery, format!(
                            "context compaction could not make enough room ({error:#})"
                        )));
                        if self.interrupt.load(Ordering::SeqCst) {
                            return Ok(crate::agent::run::engine::Tick::Finished(TurnReport {
                                text: String::new(),
                                outcome: TurnOutcome::Interrupted,
                                steps: step + 1,
                                interrupted: true,
                                trajectory: None,
                            }));
                        }
                    }
                }
            }
            if request_context >= u64::from(self.context_window()) && !self.lean_tools {
                self.lean_tools = true;
                self.rebuild_system();
                names = self.offered();
                *schemas = self.schemas(&names)?;
                ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Model, format!(
                    "Context window {} is small; offering a lean tool set (others load on demand)",
                    self.context_window()
                )));
                budget.give_back();
                return Ok(crate::agent::run::engine::Tick::Again);
            }
            ensure!(
                request_context < u64::from(self.context_window()),
                "the projected request needs about {request_context} tokens, exceeding the {}-token context window{}. Compaction cannot safely make room; reduce system/project instructions or tool schemas, retrieve smaller evidence slices, or select a larger-window model. The session and unfinished work are preserved",
                self.context_window(),
                if self.lean_tools {
                    " even with the lean tool set"
                } else {
                    ""
                }
            );
            pending_nudges.clear();
            self.efficiency_metrics
                .request(&request_overhead, lightweight);
            let requested_tokens = request.max_tokens;
            let repairing = repair.take_request();
            if !self.hermetic {
                let mut profile = selection.profile.clone();
                if single_call
                    && let Some(profile) = profile.as_mut()
                    && crate::llm::model_config::ApiProtocol::builtin(&profile.provider)
                        .accepts_parallel_tool_calls()
                {
                    profile.capabilities.parallel_tool_calls = Some(false);
                }
                if self.external_thinking_instruction().is_some()
                    && let Some(profile) = profile.as_mut()
                {
                    profile.reasoning_effort = Some(crate::llm::profiles::ReasoningEffort::None);
                }
                if let (Some(effort), Some(profile)) = (turn_effort, profile.as_mut())
                    && let Some(clamped) =
                        crate::judgment::clamp_effort(effort, &profile.capabilities.reasoning_efforts)
                {
                    profile.reasoning_effort = Some(clamped);
                }
                if repairing {
                    let model = self.auth_config.selection();
                    let profile = profile.get_or_insert_with(|| {
                        crate::llm::profiles::ModelProfile::minimal(&model.provider, &model.model)
                    });
                    profile.tool_repair = true;
                }
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
                if signature != policy_signature {
                    policy_signature = signature;
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
                control["reliability"]["current_tools"] = json!(names);
                self.session.set_control_state(control)?;
                self.record(step, "request_policy", serde_json::to_value(&selection)?)?;
                self.save_request_policy(
                    Some(serde_json::to_value(&selection)?),
                    json!({"state":"awaiting response"}),
                    ui,
                )?;
            }
            // The headroom gate used this exact projected request, including transient guidance.
            self.record(
                step,
                "request_overhead",
                serde_json::to_value(&request_overhead)?,
            )?;
            let started = Instant::now();
            let timeout = self.config.worker_timeout_secs;
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
            let interrupt = self.interrupt.clone();
            // Steering cancels only this generation, never the whole user turn or
            // independent workers that share the ordinary interrupt flag.
            let generation_scope = turn_cancel.inference();
            let generation_cancel = generation_scope.flag();
            let mut steered_during_generation = false;
            let mut steering_error = None;
            let live_steering_key = backend.live_steering_key();
            let mut live_steering = LiveSteering::default();
            let mut rule_monitor = rule_catalog.monitor(&rule_root, &rule_state, "main");
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
                    self.usage_scope.for_purpose(crate::llm::usage::Purpose::Repair),
                ))
            });
            let result = if self.hermetic {
                crate::learn::compute::background_call(self.config, backend, |backend, cancel| {
                    chat_with_ui(
                        backend,
                        adapter.as_deref(),
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
                chat_with_ui_inner(
                    backend,
                    adapter.as_deref(),
                    provider_request,
                    ChatOptions {
                        timeout,
                        render_text: self.external_thinking_instruction().is_none() && !repairing,
                        context_tokens: Some(request_context),
                        policy: Some(&reasoning_policy),
                        recovery: Some(&self.config.request_recovery),
                    },
                    generation_cancel.clone(),
                    ui,
                    &mut |ui| {
                        self.pump_subagents(ui);
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
                    Some(&mut rule_monitor),
                )
            };
            if let Some(previous) = previous_usage {
                backend.set_usage_scope(previous);
            }
            self.session.runtime().check_persistence().map_err(anyhow::Error::msg)?;
            // Also clear a profile if the model worker failed before entering the provider.
            if let Some(observation) = backend.wire_observation() {
                self.record(step, "provider_wire", serde_json::to_value(observation)?)?;
            }
            backend.next_request_profile(None);
            match &result {
                Ok(reply) => self.observe_local_reply(reply, backend),
                Err(error) => self.observe_local_failure(error, backend),
            }
            // The projected request is not reused after inference: move its history instead of
            // deep-copying it each iteration. Recovery and training still see the exact bytes sent.
            let (context_before, request_tools) = match rewritten {
                Some(rewritten) => (rewritten.messages, rewritten.tools),
                None => (std::mem::take(&mut request.messages), request.tools.clone()),
            };
            *schemas = request.tools;
            // Receipts still outstanding after the stream ends: wait to their deadline, then commit.
            match self.settle_live_steering(ui, &mut live_steering) {
                Ok(true) => steered_during_generation = true,
                Ok(false) => {}
                Err(error) => {
                    steering_error.get_or_insert(error);
                }
            }
            if let Some(error) = steering_error {
                return Err(error).map(crate::agent::run::engine::Tick::Finished);
            }
            if steered_during_generation && !self.interrupt.load(Ordering::SeqCst) {
                steering_pending = true;
                self.record(
                    step,
                    "steering_superseded_generation",
                    json!({"elapsed_ms":started.elapsed().as_millis()}),
                )?;
                ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Steering, "Stopped the old generation; replanning with your new instruction."));
                budget.give_back();
                return Ok(crate::agent::run::engine::Tick::Again);
            }
            let rule_hit = result
                .as_ref()
                .err()
                .and_then(|e| e.downcast_ref::<crate::agent::safety::rules::Hit>())
                .cloned();
            let scoped_paths = result.as_ref().ok().and_then(|reply| {
                rule_catalog.pending_paths(&rule_root, &rule_state, "main", &reply.tool_calls)
            });
            if !self.interrupt.load(Ordering::SeqCst)
                && (rule_hit.is_some() || scoped_paths.is_some())
            {
                if result.is_ok() {
                    record_rejected_reply(&self.usage_scope, backend, repairing)?;
                }
                ensure!(
                    rule_regenerations < self.config.portable_rules.max_regenerations,
                    "portable guidance regeneration limit reached; stale tools were not executed"
                );
                rule_regenerations += 1;
                rule_state.set_regenerations(rule_regenerations);
                if let Some(hit) = rule_hit {
                    rule_state
                        .remember(&hit, "main")
                        .map_err(anyhow::Error::msg)?;
                    ui.notice(&hit.to_string());
                }
                if let Some(paths) = scoped_paths {
                    rule_state.activate(paths).map_err(anyhow::Error::msg)?;
                    ui.inference_discarded("New file-scoped guidance must be delivered before these tools; discarded the entire tool attempt.");
                }
                let mut control = self.session.control_state();
                rule_state.save(&mut control);
                self.session.set_control_state(control)?;
                self.record(
                    step,
                    "portable_rule_regeneration",
                    json!({"count":rule_regenerations}),
                )?;
                budget.give_back();
                return Ok(crate::agent::run::engine::Tick::Again);
            }
            let mut reply = match result {
                Err(error)
                    if error.downcast_ref::<SilentRequest>().is_some()
                        && !self.interrupt.load(Ordering::SeqCst) =>
                {
                    let stalled = error.downcast_ref::<SilentRequest>().unwrap();
                    if let Some(direction) = stalled.direction.clone() {
                        self.accept_steering(vec![direction], ui)?;
                        budget.give_back();
                        return Ok(crate::agent::run::engine::Tick::Again);
                    }
                    if !stalled.partial.trim().is_empty()
                        && stall_continues < crate::agent::safety::stream_guards::STREAM_STALL_CONTINUE_MAX_RETRIES
                    {
                        stall_continues += 1;
                        let partial = if self.external_thinking_instruction().is_some() {
                            crate::agent::modes::external_thinking::strip_scratchpad(
                                &stalled.partial,
                            )
                        } else {
                            stalled.partial.clone()
                        };
                        self.request_phase
                            .recover("the stream stalled after partial text; continuing");
                        ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Recovery, format!(
                            "Stream stalled after partial text; continuing ({stall_continues}/{})",
                            crate::agent::safety::stream_guards::STREAM_STALL_CONTINUE_MAX_RETRIES
                        )));
                        self.push(json!({"role":"assistant","content":partial}))?;
                        pending_nudges.push(json!({"role":"user","content":crate::agent::safety::stream_guards::render_retry_prompt(crate::agent::safety::stream_guards::STREAM_STALL_CONTINUE, stall_continues, crate::agent::safety::stream_guards::STREAM_STALL_CONTINUE_MAX_RETRIES),"dial_transient":true}));
                        budget.refund();
                        return Ok(crate::agent::run::engine::Tick::Again);
                    }
                    self.save_request_policy(None, json!({"state":"request needs recovery","elapsed_ms":started.elapsed().as_millis()}), ui)?;
                    return self.stop_turn(
                        ui,
                        format!("{error} Send a direction or retry to resume the unfinished task."),
                        step + 1,
                    ).map(crate::agent::run::engine::Tick::Finished);
                }
                Ok(reply) => reply,
                Err(error)
                    if error.downcast_ref::<crate::agent::workflow::reasoning::GateHit>().is_some()
                        && !self.interrupt.load(Ordering::SeqCst) =>
                {
                    reasoning_budget = Some(requested_tokens);
                    self.request_phase
                        .recover("reasoning gate intercepted the previous request");
                    self.save_request_policy(None,json!({"state":"reasoning gate","elapsed_ms":started.elapsed().as_millis()}),ui)?;
                    self.metrics.reasoning_interceptions += 1;
                    self.record(step, "reasoning_gate", json!({"reason":error.to_string(),"elapsed_ms":started.elapsed().as_millis()}))?;
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
                    plan_after_gate = plan_gate && recovered.is_ok();
                    match recovered {
                        Ok(message) => {
                            ui.notice(&message);
                            self.push(json!({"role":"user","content":message}))?;
                        }
                        Err(message) => return self.stop_turn(ui, message, step + 1).map(crate::agent::run::engine::Tick::Finished),
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
                    return Ok(crate::agent::run::engine::Tick::Finished(TurnReport {
                        text: String::new(),
                        outcome: TurnOutcome::Interrupted,
                        steps: step,
                        interrupted: true,
                        trajectory: None,
                    }));
                }
                Err(error)
                    if !self.hermetic
                        && error.downcast_ref::<llm::errors::ThinkingLoop>().is_none()
                        && crate::llm::fallback::should_fallback(&error) =>
                {
                    if self.fail_over(backend, &error, ui) {
                        max_tokens = backend.output_budget(h.max_tokens);
                        budget.give_back();
                        return Ok(crate::agent::run::engine::Tick::Again);
                    }
                    self.save_request_policy(None,json!({"state":"generation failed","elapsed_ms":started.elapsed().as_millis()}),ui)?;
                    crate::telemetry::request_failed(
                        self.config,
                        self.hermetic,
                        self.session.id(),
                        backend.model_label(),
                        started.elapsed(),
                        &error,
                    );
                    return Err(error).map(crate::agent::run::engine::Tick::Finished);
                }
                Err(error)
                    if !self.hermetic
                        && looks_like_overflow(&error)
                        && overflow_retries < 3
                        && self.config.compaction.enabled
                        && self.config.compaction.strategy
                            != crate::config::compaction::CompactionStrategy::Off =>
                {
                    self.request_phase
                        .recover("context overflow; bounded compaction retry");
                    overflow_retries += 1;
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
                    ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Recovery, "context window exceeded; compacting and retrying"));
                    let overflow_request = Chat {
                        messages: context_before.clone(),
                        tools: schemas.clone(),
                        temperature: h.temperature,
                        max_tokens: requested_tokens,
                        seed: None,
                    };
                    let options = crate::config::compaction::CompactionOptions {
                        reason: crate::config::compaction::CompactionReason::Overflow,
                        ..Default::default()
                    };
                    self.poll_background_compaction(ui);
                    let outcome = match self.claim_background_compaction(
                        ui,
                        crate::config::compaction::CompactionReason::Overflow,
                        Some(&overflow_request),
                        Some(backend),
                    ) {
                        Ok(Some(outcome)) => outcome,
                        Ok(None) => {
                            self.compact_using(backend, ui, Some(&overflow_request), &options)?
                        }
                        Err(error) => {
                            ui.diagnostic(&format!("Prepared compaction was not claimed during overflow ({error:#}); using blocking maintenance."));
                            self.compact_using(backend, ui, Some(&overflow_request), &options)?
                        }
                    };
                    if !outcome.continue_allowed
                        && outcome.kind != crate::agent::compact::compaction::CompactionOutcomeKind::Compaction
                    {
                        return self.pause_after_compaction(ui, step).map(crate::agent::run::engine::Tick::Finished);
                    }
                    budget.give_back();
                    return Ok(crate::agent::run::engine::Tick::Again);
                }
                Err(error) => {
                    self.save_request_policy(None,json!({"state":"generation failed","elapsed_ms":started.elapsed().as_millis()}),ui)?;
                    self.record(step, "generation_error", json!({"error":format!("{error:#}"),"elapsed_ms":started.elapsed().as_millis()}))?;
                    crate::telemetry::request_failed(
                        self.config,
                        self.hermetic,
                        self.session.id(),
                        backend.model_label(),
                        started.elapsed(),
                        &error,
                    );
                    return Err(error).map(crate::agent::run::engine::Tick::Finished);
                }
            };
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
            reasoning_budget = None;
            if repairing {
                match crate::agent::model::constrained_repair::finish(&mut reply, &names, format!("constrained_{step}"))
                {
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
                steering_pending = true;
                ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Steering, "Discarded the old response before executing its tools; replanning with your new instruction."));
                budget.give_back();
                return Ok(crate::agent::run::engine::Tick::Again);
            }
            if reply.finish_reason == "length" {
                if self.config.reasoning_guard.enabled
                    && reply.text.trim().is_empty()
                    && reply.tool_calls.is_empty()
                {
                    reasoning_budget = Some(requested_tokens);
                    recovering_output = true;
                    self.request_phase
                        .recover("output budget consumed without an action or answer");
                    self.metrics.reasoning_interceptions += 1;
                    let reason = "the output token limit was spent without any answer or tool call";
                    self.record(
                        step,
                        "reasoning_gate",
                        json!({"reason":reason,"elapsed_ms":started.elapsed().as_millis()}),
                    )?;
                    let plan_gate =
                        self.config.reasoning_guard.require_plan && !self.todo.has_open();
                    let recovered = if plan_gate {
                        self.reasoning_recovery.intercept_planned(reason)
                    } else {
                        self.reasoning_recovery.intercept(reason)
                    };
                    plan_after_gate = plan_gate && recovered.is_ok();
                    match recovered {
                        Ok(message) => {
                            ui.notice(&message);
                            self.push(json!({"role":"user","content":message,"dial_reminder":"reasoning"}))?;
                        }
                        Err(message) => return self.stop_turn(ui, message, step + 1).map(crate::agent::run::engine::Tick::Finished),
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
                        self.request_phase.recover(
                            "output was truncated; retry retains an expanded token budget",
                        );
                        recovering_output = true;
                        ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Recovery, format!("the model hit its output limit ({requested_tokens} tokens); retrying with {raised}")));
                        max_tokens = raised;
                        budget.refund();
                        return Ok(crate::agent::run::engine::Tick::Again);
                    }
                    crate::agent::safety::guard::LengthAction::Nudge { partial, message } => {
                        self.request_phase.recover(
                            "continue truncated output without executing a partial tool call",
                        );
                        recovering_output = true;
                        if let Some(text) = partial {
                            let text = if self.external_thinking_instruction().is_some() {
                                crate::agent::modes::external_thinking::strip_scratchpad(&text)
                            } else {
                                text
                            };
                            self.push(json!({"role":"assistant","content":text}))?;
                        }
                        pending_nudges
                            .push(json!({"role":"user","content":message,"dial_transient":true}));
                        budget.refund();
                        return Ok(crate::agent::run::engine::Tick::Again);
                    }
                }
            }
            if recovering_output {
                self.metrics.truncated_recoveries += 1;
                recovering_output = false;
            }
            if reply.tool_calls.is_empty()
                && reply.finish_reason == "stop"
                && self.config.edit.recover_inline_edits
                && self.edit_mode() == crate::config::types::EditMode::Sloppy
                && names.iter().any(|name| name == "edit")
                && let Some((_, arguments)) = tools::inline_edit_payload(&reply.text)
            {
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
                if let Some(paths) =
                    rule_catalog.pending_paths(&rule_root, &rule_state, "main", &reply.tool_calls)
                {
                    record_rejected_reply(&self.usage_scope, backend, repairing)?;
                    ensure!(
                        rule_regenerations < self.config.portable_rules.max_regenerations,
                        "portable guidance regeneration limit reached; inline edit was not executed"
                    );
                    rule_regenerations += 1;
                    rule_state.set_regenerations(rule_regenerations);
                    rule_state.activate(paths).map_err(anyhow::Error::msg)?;
                    let mut control = self.session.control_state();
                    rule_state.save(&mut control);
                    self.session.set_control_state(control)?;
                    ui.inference_discarded(
                        "Inline edit requires applicable file guidance before any tool effect.",
                    );
                    budget.give_back();
                    return Ok(crate::agent::run::engine::Tick::Again);
                }
            }
            if let Some(error) = self.completed_edit_preview_error(&reply) {
                rejected_edit_previews += 1;
                record_rejected_reply(&self.usage_scope, backend, repairing)?;
                ui.inference_discarded(
                    "Completed edit preview failed; no tools from this generation were executed.",
                );
                // From the eighth rejection on, generations are charged to the step budget instead of
                // refunded, so `harness.max_steps` still bounds this loop. The guidance escalates; no turn end.
                let escalation = if rejected_edit_previews >= 8 {
                    format!("Escalation: {rejected_edit_previews} completed edit previews have been rejected this turn, and none of their edits ran. Stop regenerating the whole edit: read the target once, then make one smaller edit that quotes it exactly, using the offered grammar. ")
                } else {
                    String::new()
                };
                self.push(json!({"role":"user","dial_reminder":"edit-preview",
                    "content":format!("{escalation}The preceding generation was rejected before tool execution. Regenerate the complete edit using the offered grammar and observed source. Preview error (untrusted source data): {}", truncate(&error,4096))}))?;
                if rejected_edit_previews < 8 {
                    budget.give_back();
                }
                return Ok(crate::agent::run::engine::Tick::Again);
            }
            // Form failures bypass tool-result guards. Bound their recovery independently of
            // the optional step limit, including when providers alternate failure formats.
            let unusable = reply.dropped_tool_call()
                || (reply.tool_calls.is_empty() && looks_like_text_call(&reply.text, &names));
            if let Err(reason) = engine.recovery.form(unusable) {
                return self.stop_turn(ui, reason, step + 1).map(crate::agent::run::engine::Tick::Finished);
            }
            if !unusable {
                repair.settle();
            }
            // Call-shaped replies are checked after compatibility normalization (below), so a
            // spelling or container fix does not spend the one retry.
            let model = self.auth_config.selection();
            if let Some(kind) = repair.trigger(
                reply.tool_calls.is_empty()
                    && !self.hermetic
                    && crate::agent::model::constrained_repair::enabled(
                        &self.config.assistance,
                        self.hermetic,
                        &model.provider,
                        &model.model,
                        selection.profile.as_ref(),
                        backend.endpoint_structured_output(),
                    ),
                &reply,
                schemas.as_slice(),
                &names,
            ) {
                self.record(step, "constrained_repair", json!({"phase":"requested","trigger":kind,"provider":model.provider,"model":model.model}))?;
                self.request_phase
                    .recover("the previous tool call was malformed");
                let failed = pending.get_or_insert_with(|| Pending::new(context_before));
                failed.kinds.push(kind.into());
                repair.request();
                budget.refund();
                return Ok(crate::agent::run::engine::Tick::Again);
            }
            if reply.dropped_tool_call() {
                self.request_phase
                    .recover("the previous tool call was malformed");
                self.record(step, "tool_form_failure", json!({"kind":"dropped_call"}))?;
                let failed = pending.get_or_insert_with(|| Pending::new(context_before));
                failed.kinds.push("dropped_call".into());
                pending_nudges.push(json!({"role":"user","content":"Your tool call was unreadable JSON. Issue it again with valid arguments, or answer directly.","dial_transient":true}));
                budget.refund();
                return Ok(crate::agent::run::engine::Tick::Again);
            }
            if reply.tool_calls.is_empty() && looks_like_text_call(&reply.text, &names) {
                self.request_phase
                    .recover("tool syntax appeared as text instead of a valid call");
                // The model wrote a tool call as plain text instead of using the tool-call channel.
                // That is a form failure (and a learnable one): do not run it, do not hide it.
                self.record(step, "tool_form_failure", json!({"kind":"text_call"}))?;
                let failed = pending.get_or_insert_with(|| Pending::new(context_before));
                failed.kinds.push("text_call".into());
                failed.reject(json!({"role":"assistant","content":reply.text.clone()}));
                self.push(json!({"role":"assistant","content":reply.text}))?;
                self.push(json!({"role":"user","content":"That looked like a tool call written as plain text, so nothing ran. Call the tool through the tool-calling interface, or answer in words."}))?;
                budget.refund();
                return Ok(crate::agent::run::engine::Tick::Again);
            }
            match engine
                .recovery
                .stopped(&reply.text, !reply.tool_calls.is_empty())
            {
                crate::agent::run::turn_recovery::StopAction::Accept => {}
                crate::agent::run::turn_recovery::StopAction::Continue { reason, message } => {
                    self.request_phase.recover(reason);
                    ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Recovery, format!("Recovering: {reason}.")));
                    if !reply.text.trim().is_empty() {
                        self.push(json!({"role":"assistant","content":reply.text}))?;
                    }
                    pending_nudges
                        .push(json!({"role":"user","content":message,"dial_transient":true}));
                    budget.refund();
                    return Ok(crate::agent::run::engine::Tick::Again);
                }
                crate::agent::run::turn_recovery::StopAction::Incomplete(reason) => {
                    return self.stop_turn(ui, reason, step + 1).map(crate::agent::run::engine::Tick::Finished);
                }
            }
            stall_continues = 0;
            if reply.tool_calls.is_empty() {
                self.push(json!({"role":"assistant","content":reply.text}))?;
                let accepted_live = self.flush_accepted_live_steering(ui)?;
                if self.take_steering(ui)? || accepted_live {
                    steering_pending = true;
                    return Ok(crate::agent::run::engine::Tick::Again);
                }
                if self.interrupt.load(Ordering::SeqCst) {
                    return Ok(crate::agent::run::engine::Tick::Finished(TurnReport {
                        text: String::new(),
                        outcome: TurnOutcome::Interrupted,
                        steps: step + 1,
                        interrupted: true,
                        trajectory: None,
                    }));
                }
                if self.exploration.active.is_some() {
                    self.push(json!({"role":"user","dial_exploration_reminder":true,
                        "content":"Investigation checkpoint is active. Call rewind with your findings before finishing; the report replaces intermediate context, not actual effects or current user instructions."}))?;
                    return Ok(crate::agent::run::engine::Tick::Again);
                }
                if self.plan_mode && !crate::agent::verify::completion::open_work(&self.todo).is_empty() {
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
                if self.before_yield(backend, &names, &mut pending_nudges, ui)? {
                    return Ok(crate::agent::run::engine::Tick::Again);
                }
                match completion.stopped(&self.todo) {
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
                if let Err(reason) = self.validate_completion(ui) {
                    return self.stop_turn(ui, reason, step + 1).map(crate::agent::run::engine::Tick::Finished);
                }
                if let Some(message) = self.coach_completion(
                    &mut coach_turn,
                    backend,
                    user_goal,
                    &reply.text,
                    step,
                    ui,
                )? {
                    pending_nudges.push(message);
                    return Ok(crate::agent::run::engine::Tick::Again);
                }
                let note = coach_turn.completion_note();
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
                let text = self.step_checks.visual.summary(
                    reply.text,
                    (
                        self.step_checks.mutations,
                        self.step_checks.uncheckpointed_shell_commands,
                    ),
                );
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
                    let canonical = crate::agent::run::tool_compat::canonical_name(&call.name, &catalog).to_owned();
                    if canonical != call.name {
                        ui.diagnostic(&format!("Tool compatibility: {} -> {canonical}", call.name));
                        call.name = canonical;
                    }
                    if let Some(parameters) = schemas
                        .iter()
                        .find(|s| s["function"]["name"] == call.name)
                        .and_then(|s| s.pointer("/function/parameters"))
                    {
                        crate::agent::run::tool_compat::normalize(&call.name, parameters, &mut call.arguments);
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
            let model = self.auth_config.selection();
            if let Some(kind) = repair.trigger(
                // Hermetic turns disable repair; do not evaluate its live endpoint probe either.
                !self.hermetic
                    && crate::agent::model::constrained_repair::enabled(
                        &self.config.assistance,
                        self.hermetic,
                        &model.provider,
                        &model.model,
                        selection.profile.as_ref(),
                        backend.endpoint_structured_output(),
                    ),
                &reply,
                schemas.as_slice(),
                &names,
            ) {
                self.record(step, "constrained_repair", json!({"phase":"requested","trigger":kind,"provider":model.provider,"model":model.model}))?;
                self.request_phase
                    .recover("the previous tool call was malformed");
                let failed = pending.get_or_insert_with(|| Pending::new(context_before));
                failed.kinds.push(kind.into());
                repair.request();
                budget.refund();
                return Ok(crate::agent::run::engine::Tick::Again);
            }
            // Compatibility can decode JSON containers and rename arguments. Recheck that exact
            // executable form before journaling any assistant/tool pair or prefetching a read.
            let mut normalized_rules = rule_catalog.monitor(&rule_root, &rule_state, "main");
            let normalized_hit = normalized_rules.completed(&reply);
            if let Some(failure) = normalized_rules.failure {
                bail!("{failure}");
            }
            let scoped_paths =
                rule_catalog.pending_paths(&rule_root, &rule_state, "main", &reply.tool_calls);
            if normalized_hit.is_some() || scoped_paths.is_some() {
                record_rejected_reply(&self.usage_scope, backend, repairing)?;
                ensure!(
                    rule_regenerations < self.config.portable_rules.max_regenerations,
                    "portable guidance regeneration limit reached; stale tools were not executed"
                );
                rule_regenerations += 1;
                rule_state.set_regenerations(rule_regenerations);
                if let Some(hit) = normalized_hit {
                    rule_state
                        .remember(&hit, "main")
                        .map_err(anyhow::Error::msg)?;
                    ui.notice(&hit.to_string());
                }
                if let Some(paths) = scoped_paths {
                    rule_state.activate(paths).map_err(anyhow::Error::msg)?;
                }
                let mut control = self.session.control_state();
                rule_state.save(&mut control);
                self.session.set_control_state(control)?;
                ui.inference_discarded("Applicable portable guidance must be delivered before the normalized tool attempt; no tools executed.");
                budget.give_back();
                return Ok(crate::agent::run::engine::Tick::Again);
            }
            if rule_catalog
                .observe_paths(&rule_root, &mut rule_state, &reply.tool_calls)
                .map_err(anyhow::Error::msg)?
            {
                let mut control = self.session.control_state();
                rule_state.save(&mut control);
                self.session.set_control_state(control)?;
            }
            let narration_warning = engine.narration.observe(&reply);
            self.edit_preview_intercepted = false;
            self.push(crate::tools::surface::contracts::assistant_message(
                &reply, schemas, &self.ext,
            ))?;
            let (mut form_kinds, mut rejected_calls, mut execution_failed) =
                (Vec::new(), Vec::new(), false);
            let mut observed_check_failure = false;
            let mut parallel = std::collections::VecDeque::new();
            let mut stop: Option<String> = None;
            let mut round_succeeded = false;
            let mut last_tool_failure = None;
            let mut edit_failed = false;
            let mut finished = None;
            let mut round_steering = Vec::new();
            let mut portable_batch_intercepted = false;
            let _speculative_batch = self.workspace.speculation.batch();
            if (!single_call || reply.tool_calls.len() <= 1) && engine.pending_refusal().is_none() {
                for call in reply.tool_calls.iter().skip(1) {
                    if call_form_error(call,schemas,&names).is_none() && engine.rejection_at(&self.workspace,call).is_none() {
                        self.warm_speculative_read(call);
                    }
                }
            }
            // Past the stronger guidance, an identical repeat is answered with its earlier result
            // instead of running again. Only the repeated call set qualifies; anything else runs.
            let duplicates = repeat_streak
                .as_ref()
                .filter(|repeat| repeat.count > crate::agent::safety::stream_guards::TOOL_CALL_LOOP_THRESHOLD)
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
                    self.session.repair_incomplete_tools()?;
                    return Ok(crate::agent::run::engine::Tick::Finished(TurnReport {
                        text: String::new(),
                        outcome: TurnOutcome::Interrupted,
                        steps: step + 1,
                        interrupted: true,
                        trajectory: None,
                    }));
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
                } else { call_form_error(call, schemas, &names) }).map(|(kind, why)| {
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
                    if prechecked_paths.insert(path.to_owned()) {
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
                                preflight = Some(ToolOutput { execution: Default::default(),
                                    data: None,
                                    text,
                                    is_error: true,
                                });
                            }
                            Ok(None) => {
                                revalidated_paths
                                    .insert(call.arguments["path"].as_str().unwrap().to_owned());
                            }
                            Err(error) => {
                                preflight = Some(ToolOutput { execution: Default::default(),
                                    data: None,
                                    text: format!(
                                        "Could not check saved assumptions: {error}. Read the current target before retrying."
                                    ),
                                    is_error: true,
                                });
                            }
                        }
                    } else if !revalidated_paths.contains(path) {
                        preflight = Some(ToolOutput { execution: Default::default(),
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
                    let output = ToolOutput { execution: Default::default(),
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
                                ) &&
                                crate::agent::safety::approval::parallel_read(&self.ext, &c.name, &c.arguments)
                                    && !crate::agent::safety::approval::denied_by_rule(
                                        &self.always,
                                        c,
                                        self.workspace.root(),
                                    )
                                    && call_form_error(c, schemas, &names).is_none()
                                    && !self.speculative_pending(c)
                                    && engine.rejection_at(&self.workspace, c).is_none()
                            })
                            .count();
                        if count > 1 {
                            let mut steering = Vec::new();
                            parallel = parallel_tools(
                                &self.workspace,
                                self.effective_config(),
                                &self.ext,
                                &reply.tool_calls[index..index + count],
                                &self.interrupt,
                                ui,
                                &mut steering,
                            )
                            .into();
                            self.pending_tool_steering.extend(steering);
                        }
                    }
                    let (mut out, seconds) = match parallel.pop_front() {
                        Some(result) => {
                            // Batched bash skips `dispatch`; record verification evidence as the serial path does.
                            if call.name == "bash" {
                                self.note_verification(call, &result.0, ui);
                            }
                            result
                        }
                        None if call.name == "finish" && call.arguments["outcome"] == "completed" && (execution_failed || !form_kinds.is_empty()) => {
                            (ToolOutput { execution: Default::default(),  data: None,
text: "finish completed cannot follow a failed call in the same batch; address its error first".into(), is_error: true }, 0.0)
                        }
                        None => { let started = Instant::now(); let out = self.dispatch(backend, call, ui); (out, started.elapsed().as_secs_f64()) }
                    };
                    if (call.name == "browser" || call.name == "computer") && !out.is_error {
                        match crate::storage::artifact::capture_image(self.workspace.artifacts(), &mut out) {
                            Ok(attachment) => image = attachment,
                            Err(e) => {
                                out = ToolOutput { execution: Default::default(),
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
                    ui.tool_result_with_id(&call.id, &call.name, &out);
                    out
                };
                let status = if controller_blocked {
                    tools::ToolStatus::Blocked
                } else {
                    tools::ToolStatus::from_output(&output)
                };
                crate::telemetry::tool_with_effects(
                    self.config,
                    self.hermetic,
                    self.session.id(),
                    &call.name,
                    tool_seconds,
                    status,
                    crate::tools::surface::outcome::effects(&output),
                );
                self.record(step, "tool_outcome", json!({"name":call.name,"id":call.id,"status":status,"effects":tools::surface::outcome::effects(&output),"duration_ms":tool_seconds*1000.0}))?;
                let callback_intercepted = std::mem::take(&mut self.portable_callback_intercepted);
                if callback_intercepted || self.ext.contains(&call.name) {
                    rule_state = crate::agent::safety::rules::State::restore(&self.session.control_state());
                    portable_batch_intercepted |=
                        rule_state.callback_pending() || callback_intercepted;
                }
                if call.name == "new_context" && output.is_error {
                    self.clear_context_rollover();
                }
                if crate::agent::workflow::exploration::TOOL_NAMES.contains(&call.name.as_str()) && output.is_error {
                    self.exploration.reject_result(call);
                }
                reliability.observe(
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
                    turn_started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
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
                if is_check {
                    if output.is_error || output.process_outcome().is_none_or(|process| process.exit_code != Some(0) || !process.output_complete || process.cancelled || process.timed_out) {
                        reliability.failing_checks += 1;
                    } else {
                        reliability.passing_checks += 1;
                    }
                }
                let check_failed = is_check
                    && (output.process_outcome().is_some_and(|process| process.exit_code.is_some_and(|code| code != 0))
                        || crate::agent::verify::verification::zero_tests(&output.text));
                observed_check_failure |= check_failed;
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
                    coach_turn.observe(
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
                    && prechecked_paths.contains(path)
                {
                    revalidated_paths.insert(path.to_owned());
                }
                if !output.is_error {
                    round_succeeded = true;
                }
                // Committed facts decide: any tool whose transaction committed paths (LSP rename, code
                // actions, checkpointed edits) advances the mutation epoch, even when it failed after
                // a partial commit. The name list covers writers that commit without reporting paths.
                let committed = !output.execution.changed_paths.is_empty();
                if committed
                    || (!output.is_error
                        && matches!(call.name.as_str(), "write" | "edit" | "ast_edit" | "solve"))
                {
                    self.step_checks.mutations += 1;
                    self.behavioral_checks.clear();
                    if !self.hermetic
                        && let Some(path) = call.arguments["path"].as_str()
                    {
                        self.step_checks.visual.changed(Path::new(path));
                    }
                }
                if call.name == "bash" {
                    if call.arguments["command"].as_str().is_some_and(|command| {
                        !crate::agent::safety::approval::safe_bash(command) && !crate::agent::safety::approval::safe_review_batch(command)
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
                if !self.hermetic
                    && !output.is_error
                    && matches!(call.name.as_str(), "write" | "edit" | "ast_edit")
                    && let (Some(cache), Some(path)) =
                        (self.codebase.clone(), call.arguments["path"].as_str())
                    && reviewed_paths.len() < 4
                    && reviewed_paths.insert(path.to_owned())
                {
                    let paths = vec![path.to_owned()];
                    let context = thread::scope(|scope| -> Result<_> {
                        let (tx, rx) = mpsc::sync_channel(1);
                        scope.spawn(move || {
                            let _ = tx.send(cache.impact_context(&paths, 3000));
                        });
                        receive_with_heartbeat(&rx, ui)?
                    });
                    if let Ok(context) = context {
                        output.text.push_str("\n\n");
                        output.text.push_str(&context);
                        self.record(step, "change_review", json!({"path":path}))?;
                    }
                }
                if call.name == "finish" && !output.is_error && self.step_checks.finish_accepted {
                    finished = Some(
                        crate::agent::verify::completion::Finish::parse(&call.arguments).map_err(anyhow::Error::msg)?,
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
                if stop.is_none() {
                    stop = recovery_stop;
                }
                let todo_advice = todo_recovery.observe(
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
                        (stall.streak(&call.name) >= 1
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
                        solo_todo_noted,
                    )
                {
                    solo_todo_noted = true;
                    output.text.push_str("\n\n");
                    output.text.push_str(note);
                }
                if let Some(advice) = repeat_advice {
                    ui.diagnostic(&format!("Recovery checkpoint: `{}` failed; correct its arguments or choose a different concrete action.", call.name));
                    output.text.push_str("\n\n");
                    output.text.push_str(&advice);
                }
                if let Some(advice) = pace.call(call, &output) {
                    output.text.push_str("\n\n");
                    output.text.push_str(&advice);
                }
                if output.is_error {
                    stall.note_error(&call.name, &output.text);
                }
                match stall.call(call, output.is_error) {
                    crate::agent::safety::guard::Stall::Warn(advice) => {
                        if self.config.reliability.enabled {
                            engine.arm(call, &advice);
                            ui.diagnostic(&format!("Recovery checkpoint: `{}` keeps failing; correct its arguments or choose a different concrete action.", call.name));
                        } else {
                            ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Recovery, advice.as_str()));
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
                if call.name == "bash"
                    && !output.is_error
                    && call.arguments["command"]
                        .as_str()
                        .is_some_and(is_check_command)
                    && let Some(uri) = &artifact
                    && let Some(compact) = crate::agent::verify::verification::compact_success(&output.text, uri)
                {
                    body = compact;
                }
                if let Some(uri) = &artifact {
                    crate::agent::workflow::efficiency::evidence_link(&mut body, uri, dropped);
                }
                if let Some(notice) = &retain_notice {
                    body.push_str(notice);
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
                        (
                            self.step_checks.mutations,
                            self.step_checks.uncheckpointed_shell_commands,
                        ),
                        call.name == "browser" || call.name == "computer",
                    );
                }
                for reference in message["dial_images"].as_array().into_iter().flatten() {
                    ui.image(reference);
                }
                if crate::agent::workflow::exploration::TOOL_NAMES.contains(&call.name.as_str()) && !output.is_error {
                    message["dial_exploration_result"] = json!(true);
                }
                self.push(message)?;
                // A changed history snapshot may make a previously dead-ended compaction fit.
                compaction_failed = false;
                let (tokens, estimated) = self.context_usage();
                ui.context_usage(tokens, estimated);
            }
            if let Some(reason) = engine.pending_refusal() {
                self.accept_steering(round_steering, ui)?;
                return self.stop_turn(ui, stop.unwrap_or_else(|| reason.to_owned()), step + 1).map(crate::agent::run::engine::Tick::Finished);
            }
            if portable_batch_intercepted && rule_state.callback_retry() {
                // One inference retry per intercepted cell/batch, charged to the shared
                // portable-rule budget. Already completed/prefetched reads retain their results.
                budget.give_back();
            }
            if self.accept_steering(round_steering, ui)? || self.take_steering(ui)? {
                steering_pending = true;
                ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Steering, "Kept completed tool results and skipped unstarted old actions; replanning with your new instruction."));
                return Ok(crate::agent::run::engine::Tick::Again);
            }
            if portable_batch_intercepted {
                if !rule_state.callback_retry() {
                    let reason = if rule_state.callback_pending() {
                        "Portable guidance regeneration limit reached; the intercepted callback and remaining unstarted batch actions were refused. Completed work is preserved."
                    } else {
                        "Portable callback guidance could not be journaled; remaining unstarted actions were refused. Restore writable session storage before continuing."
                    };
                    return self.stop_turn(ui, reason.into(), step + 1).map(crate::agent::run::engine::Tick::Finished);
                }
                names = self.offered();
                *schemas = self.schemas(&names)?;
                return Ok(crate::agent::run::engine::Tick::Again);
            }
            match self.settle_exploration(backend, ui) {
                Ok(true) => {
                    names = self.offered();
                    *schemas = self.schemas(&names)?;
                    compaction_failed = true;
                    return Ok(crate::agent::run::engine::Tick::Again);
                }
                Ok(false) => {}
                Err(error) => return self.stop_turn(ui, format!(
                    "Investigation transition did not receive a successful durable acknowledgement: {error:#}. Automatic continuation stopped; no rollback or retry is implied."),
                    step + 1).map(crate::agent::run::engine::Tick::Finished),
            }
            if let Some(requested) = self.pending_context_rollover.take() {
                let request = Chat {
                    messages: context_before,
                    tools: std::mem::take(schemas),
                    temperature: request.temperature,
                    max_tokens: request.max_tokens,
                    seed: request.seed,
                };
                match self.rollover_context_using(backend, ui, requested, Some(&request)) {
                    Ok(_) => {
                        compaction_failed = true;
                        names = self.offered();
                        *schemas = self.schemas(&names)?;
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
                ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Recovery, advice.as_str()));
                pending_nudges.push(json!({"role":"user","content":advice,"dial_transient":true}));
            }
            // A short-circuited round neither failed nor succeeded, so it must not advance failure streaks.
            let stall_round = if duplicates.is_some() { crate::agent::safety::guard::Stall::None } else { stall.round(!round_succeeded) };
            let warning = match stall_round {
                crate::agent::safety::guard::Stall::Warn(advice) => Some(advice),
                crate::agent::safety::guard::Stall::None => None,
            };
            if let Some(advice) = warning {
                pending_nudges.push(json!({"role":"user","content":advice,"dial_transient":true}));
            }
            if duplicates.is_none()
                && let Some(advice) = pace.round(&reply.tool_calls)
            {
                pending_nudges.push(json!({"role":"user","content":advice,"dial_transient":true}));
            }
            if let Some(plan) = self.pending_plan.take() {
                self.push(json!({"role":"user","content":format!("Approved plan:\n{plan}\nProceed with implementation.")}))?;
            }
            reliability.round(!form_kinds.is_empty(), round_succeeded);
            // Escalation is between requests, so the next request is built from the new offer.
            if self.observe_local_round(!form_kinds.is_empty() || edit_failed, ui) {
                names = self.offered();
                *schemas = self.schemas(&names)?;
            }
            let tool_round_failed = !form_kinds.is_empty()
                || execution_failed
                || observed_check_failure
                || self.request_phase.phase == crate::config::adaptive::Phase::Recovery;
            if !self.hermetic {
                let mut control = self.session.control_state();
                settle_reliability(
                    &mut control["reliability"],
                    &reliability,
                    &measured,
                    single_call,
                    &self.step_checks,
                );
                self.session.set_control_state(control.clone())?;
                ui.request_policy(&control);
                // Repeated contract failures escalate the guidance instead of ending the turn; the step
                // budget still bounds the retries.
                let failures = reliability.consecutive_contract_failures;
                let limit = self.config.reliability.max_contract_failures;
                if self.config.reliability.enabled && limit > 0 && failures >= limit && failures.is_multiple_of(limit) {
                    pending_nudges.push(json!({"role":"user","dial_transient":true,"content":format!(
                        "[Escalation from dial] {failures} tool calls in a row broke the tool contract, and none of the rejected calls ran. Call only the offered tools, with arguments that match their schemas exactly. Do not restate the rules or explain the failure at length: make the next call now."
                    )}));
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
            if automatic_coach_pending {
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
                automatic_coach_pending = false;
            }
            let role_configured = self.config.model_roles.contains_key("coach");
            // A turn already ending is not charged for a coach-model call.
            if stop.is_none()
                && self.step_checks.auto_escalation.round(
                    tool_round_failed,
                    &self.config.escalation,
                    role_configured,
                )
            {
                let mut question = self.step_checks.auto_escalation.question(user_goal);
                let observations = coach_turn.evidence();
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
                        automatic_coach_pending = true;
                        pending_nudges.push(crate::agent::verify::auto_escalation::advice_message(
                            answer["advice"].as_str().unwrap_or_default(),
                        ));
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
                        ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Coach, format!("Automatic escalation skipped: {reason}")));
                        self.record(
                            step,
                            "auto_escalation",
                            json!({"status":"skipped","reason":reason}),
                        )?;
                    }
                }
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
                    return self.stop_turn(ui, message, step + 1).map(crate::agent::run::engine::Tick::Finished);
                }
                // First resume the model on its own:
                // most stalls end with one fresh instruction and nobody has to type "continue".
                if auto_continues < self.config.reliability.auto_continue_rounds
                    && !self.interrupt.load(Ordering::SeqCst)
                {
                    auto_continues += 1;
                    let headline = message.lines().next().unwrap_or_default().to_owned();
                    ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Continue, format!(
                        "Auto-continue {auto_continues}/{}: {headline}",
                        self.config.reliability.auto_continue_rounds
                    )));
                    self.record(
                        step,
                        "auto_continue",
                        json!({"attempt":auto_continues,"reason":headline}),
                    )?;
                    self.push(json!({"role":"user","content":format!(
                        "<system-notice>\n{message}\n\nContinue. MUST resume the most recent intent with a different concrete action and complete the unfinished work. NEVER repeat the unchanged call, pause to summarize progress, or ask whether to proceed.\n</system-notice>"
                    )}))?;

                    stall = Default::default();

                    todo_recovery = Default::default();
                    engine.reset();

                    reliability.consecutive_contract_failures = 0;
                    names = self.offered();
                    *schemas = self.schemas(&names)?;
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
                    && let Some(answer) =
                        ui.ask_user("Recovery needs direction", &message, &options)
                    && !answer.trim().is_empty()
                    && answer != options[1]
                {
                    self.push(json!({"role":"user","content":format!(
                        "Recovery direction from the user: {answer}\nResume the existing task using the evidence already returned. Correct the failed call or choose a genuinely different approach; do not repeat unchanged calls or fabricate success."
                    )}))?;

                    stall = Default::default();

                    todo_recovery = Default::default();
                    engine.reset();

                    reliability.consecutive_contract_failures = 0;
                    names = self.offered();
                    *schemas = self.schemas(&names)?;
                    return Ok(crate::agent::run::engine::Tick::Again);
                }
                message.push_str("\n\nThe chat and completed work are kept. Send a corrected instruction or say \"continue\" to resume.");
                return self.stop_turn(ui, message, step + 1).map(crate::agent::run::engine::Tick::Finished);
            }
            if !form_kinds.is_empty() || execution_failed || observed_check_failure {
                self.request_phase
                    .recover("tool round failed; correct its observed diagnostics");
            }
            names = self.offered();
            *schemas = self.schemas(&names)?;
            if let Some(mut finish) = finished {
                let accepted_live = self.flush_accepted_live_steering(ui)?;
                if self.take_steering(ui)? || accepted_live {
                    steering_pending = true;
                    return Ok(crate::agent::run::engine::Tick::Again);
                }
                if self.interrupt.load(Ordering::SeqCst) {
                    return Ok(crate::agent::run::engine::Tick::Finished(TurnReport {
                        text: String::new(),
                        outcome: TurnOutcome::Interrupted,
                        steps: step + 1,
                        interrupted: true,
                        trajectory: None,
                    }));
                }
                if finish.outcome == TurnOutcome::Completed {
                    if let Some(message) = self.coach_completion(
                        &mut coach_turn,
                        backend,
                        user_goal,
                        &finish.text,
                        step,
                        ui,
                    )? {
                        pending_nudges.push(message);
                        return Ok(crate::agent::run::engine::Tick::Again);
                    }
                    finish.text.push_str(&coach_turn.completion_note());
                    finish.text = self.step_checks.visual.summary(
                        finish.text,
                        (
                            self.step_checks.mutations,
                            self.step_checks.uncheckpointed_shell_commands,
                        ),
                    );
                    if self.logging {
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
                return Ok(crate::agent::run::engine::Tick::Finished(TurnReport {
                    text: String::new(),
                    outcome: TurnOutcome::Interrupted,
                    steps: step + 1,
                    interrupted: true,
                    trajectory: None,
                }));
            }
            if self.take_steering(ui)? {
                steering_pending = true;
            }
            if form_kinds.is_empty() && !execution_failed && self.logging {
                self.training_capture.record(
                    &context_before,
                    &request_tools,
                    llm::assistant_message(&reply),
                );
            }
            if !form_kinds.is_empty() {
                let failed = pending.get_or_insert_with(|| Pending::new(context_before));
                failed.kinds.extend(form_kinds);
                for rejected in rejected_calls {
                    failed.reject(rejected);
                }
            } else if !execution_failed
                && let (Some(failed), Some(store), Some((id, _, _))) =
                    (pending.take(), self.store, &self.trajectory)
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
        });
        if let Some(report) = driven? {
            return Ok(report);
        }

        if self.hermetic {
            bail!("agent exhausted the admitted step budget without a final answer")
        }
        self.stop_turn(
            ui,
            format!("Stopped: this turn used its {max_steps}-step limit. The chat and completed work are kept. Say \"continue\" to keep going, or raise harness.max_steps in /settings."),
            iteration,
        )
    }
}

/// Refresh round-owned fields without changing the meaning of verification evidence.
pub(in crate::agent) fn settle_reliability(
    state: &mut Value,
    stats: &crate::reliability::Stats,
    measured: &Option<Value>,
    single_call: bool,
    checks: &StepChecks,
) {
    // Results and bounded receipt history are owned by run_checks, not round settlement.
    // Update only round-owned fields so repair reentries cannot erase execution evidence.
    if !state.is_object() {
        *state = json!({});
    }
    state["stats"] = json!(stats);
    state["measured"] = json!(measured);
    state["interface"] = json!(if single_call {
        "compact single call"
    } else {
        "standard"
    });
    state["verification"] = json!(if checks.failed {
        "failed"
    } else if checks.clean() {
        "focused checks passed; review task behavior"
    } else if checks.mutations > 0 {
        "edits need verification"
    } else {
        "no edits this turn"
    });
    state["edit_loop"] = checks.status();
}

/// A fallback or cooldown return is a new provider backend. It must join the turn's usage ledger
/// before its first request, or its requests, tokens and prices bypass accounting.
pub(in crate::agent) fn join_turn_usage(usage: &crate::llm::usage::Scope, backend: &mut Backend) {
    backend.set_usage_scope(Some(usage.clone()));
}

/// A received reply rejected by this owner remains physical usage. The discard is recorded under
/// the purpose that requested it, because a repair tag is restored before the rejection is known.
fn record_rejected_reply(
    usage: &crate::llm::usage::Scope,
    backend: &mut Backend,
    repairing: bool,
) -> Result<()> {
    let purpose = if repairing {
        crate::llm::usage::Purpose::Repair
    } else {
        crate::llm::usage::Purpose::Agent
    };
    let previous = backend.set_usage_scope(Some(usage.for_purpose(purpose)));
    let result = backend.record_discarded_response();
    backend.set_usage_scope(previous);
    result
}

#[cfg(test)]
mod usage_join_tests;

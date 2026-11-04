//! Send one request, restore transport state and settle generation-time steering.
use super::*;

impl Agent<'_> {
    pub(super) fn generate_step(
        &mut self,
        context: &mut StepContext<'_, '_>,
        prepared: PreparedRequest,
    ) -> Result<Phase<Generation>> {
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
        let PreparedRequest {
            step,
            prep_started,
            mut request,
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
        } = prepared;
        // One-shot notes stay queued until a reply proves delivery (`TurnState::reply_received`):
        // every path below that discards this request before a reply resends them.
        self.efficiency_metrics
            .request(&request_overhead, lightweight, self.request_phase.phase);
        let requested_tokens = request.max_tokens;
        self.record(
            step,
            "tool_call_policy",
            request_control["reliability"]["call_policy"].clone(),
        )?;
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
            // Session snapshot of this turn: reuses the turn's record and actual interface, no probe.
            control["reliability"]["route"] = crate::reliability::route_snapshot(
                self.config,
                self.context_window(),
                turn.measured.as_ref(),
                single_call,
            );
            control["reliability"]["current_tools"] = json!(turn.names);
            self.session.set_control_state(control)?;
            let mut selected = serde_json::to_value(&selection)?;
            // Keep the exact call contract with the existing per-request policy/history.
            selected["call_policy"] =
                self.session.control_state()["reliability"]["call_policy"].clone();
            if let Some(effective) = effective {
                selected["temperature"] = effective["temperature"].clone();
                selected["top_p"] = effective["top_p"].clone();
                selected["max_tokens"] = effective["max_tokens"].clone();
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
        self.efficiency_metrics.prefix(&prefix);
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
            && !research_step
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
                    render_text: !research_step
                        && self.external_thinking_instruction().is_none()
                        && !repairing,
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
                        // The user already chose a separate worker: start it now instead of
                        // making it wait for this reply to finish. The generation is untouched.
                        if let Err(error) = self.start_independent_messages(ui) {
                            steering_error.get_or_insert(error);
                            generation_cancel.store(true, Ordering::SeqCst);
                        }
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
        // Matches whose interruptMode let this reply proceed; journaled once the reply is accepted.
        let rule_deferred = std::mem::take(&mut rule_monitor.deferred);
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
        // Retain the effective request, including hook-rewritten sampling parameters, only
        // when paid warming is enabled. Recovery may subsequently consume its messages.
        let warm_request = self
            .cache_warm
            .as_ref()
            .map(|_| rewritten.as_ref().unwrap_or(&request).clone());
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
                .map(crate::agent::run::engine::Tick::Finished).map(ControlFlow::Break);
        }
        if let Some(error) = steering_error {
            return Err(error)
                .map(crate::agent::run::engine::Tick::Finished)
                .map(ControlFlow::Break);
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
            return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
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
            return self
                .regenerate_for_guidance(
                    turn,
                    engine,
                    step,
                    (rule_hit, scoped_paths),
                    discarded,
                    ui,
                )
                .map(ControlFlow::Break);
        }
        Ok(ControlFlow::Continue(Generation {
            result,
            rule_deferred,
            sent: SentRequest {
                step,
                started,
                single_call,
                planning_required,
                research_step,
                repairing,
                requested_tokens,
                request_estimate,
                request_context,
                sent_messages,
                context_before,
                request_tools,
                offer,
                profile: selection.profile,
                temperature: request.temperature,
                max_tokens: request.max_tokens,
                seed: request.seed,
                warm_request,
                repeat_streak,
                streamed,
            },
        }))
    }
}

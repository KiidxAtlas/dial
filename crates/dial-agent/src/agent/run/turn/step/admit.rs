//! Normalize executable calls and enforce guidance before admitting a tool round.
use super::*;

impl Agent<'_> {
    pub(super) fn admit_tool_round(
        &mut self,
        context: &mut StepContext<'_, '_>,
        received: ReceivedReply,
    ) -> Result<Phase<Round>> {
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
        let ReceivedReply {
            mut reply,
            mut sent,
        } = received;
        let mut compatibility_loaded = HashSet::new();
        // Offered calls whose arguments belong to another catalog tool (`bash` with `pty`): the
        // call is still refused by schema validation; its owners load for the next round and the
        // refusal names them. Keyed by call id: (rejected keys, owners).
        let mut argument_routes = std::collections::HashMap::new();
        if !self.hermetic {
            let catalog = self.catalog();
            for call in &mut reply.tool_calls {
                if call.malformed_arguments {
                    continue;
                }
                let canonical =
                    crate::agent::run::tool_compat::canonical_name(&call.name, &catalog)
                        .into_owned();
                if canonical != call.name {
                    ui.diagnostic(&format!("Tool compatibility: {} -> {canonical}", call.name));
                    call.name = canonical;
                }
                if let Some(parameters) = sent
                    .offer
                    .iter()
                    .find(|s| s["function"]["name"] == call.name)
                    .and_then(|s| s.pointer("/function/parameters"))
                {
                    crate::agent::run::tool_compat::normalize(
                        &call.name,
                        parameters,
                        &mut call.arguments,
                    );
                    if !sent.planning_required
                        && !self.plan_mode
                        && let Some(route) = self.argument_owners(call, &sent.offer)
                    {
                        argument_routes.insert(call.id.clone(), route);
                    }
                } else if !sent.planning_required
                    && !self.plan_mode
                    && engine.rejection_at(&self.workspace, call).is_none()
                    && catalog.contains(&call.name)
                {
                    // The model never saw this schema: enable the real tool, then validate the
                    // call against the newly loaded schema below; only a valid call runs, through
                    // normal approval, and an invalid one is refused with that schema.
                    compatibility_loaded.insert(call.name.clone());
                }
            }
            let mut owners: Vec<String> = argument_routes
                .values()
                .flat_map(|(_, owners): &(Vec<String>, Vec<String>)| owners.iter().cloned())
                .collect();
            owners.sort();
            owners.dedup();
            if !owners.is_empty() {
                self.activate_tools(&owners)?;
                ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                    crate::agent::ui::ui_event::EventKind::Tools,
                    format!(
                        "Loaded {} for the next round: it accepts arguments the called tool rejected",
                        owners.join(", ")
                    ),
                ));
            }
            if !compatibility_loaded.is_empty() {
                let mut loaded: Vec<_> = compatibility_loaded.iter().cloned().collect();
                loaded.sort();
                self.activate_tools(&loaded)?;
                // Add only the loaded tools: this request's narrowed offer (single-call
                // simplification) must not widen to the full offer.
                let offered = self.offered();
                let fresh: Vec<String> = loaded
                    .iter()
                    .filter(|name| offered.contains(name))
                    .cloned()
                    .collect();
                for (name, schema) in fresh.iter().zip(self.schemas(&fresh)?) {
                    if !turn.names.contains(name) {
                        turn.names.push(name.clone());
                    }
                    if !schemas
                        .iter()
                        .any(|s| s["function"]["name"] == name.as_str())
                    {
                        schemas.push(schema);
                    }
                }
                for call in &mut reply.tool_calls {
                    if !compatibility_loaded.contains(&call.name) || call.malformed_arguments {
                        continue;
                    }
                    if let Some(parameters) = schemas
                        .iter()
                        .find(|s| s["function"]["name"] == call.name)
                        .and_then(|s| s.pointer("/function/parameters"))
                    {
                        crate::agent::run::tool_compat::normalize(
                            &call.name,
                            parameters,
                            &mut call.arguments,
                        );
                    }
                }
                ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                    crate::agent::ui::ui_event::EventKind::Tools,
                    format!(
                        "Loaded {} for this round; its calls are checked against the loaded schema",
                        loaded.join(", ")
                    ),
                ));
            }
        }
        if !self.hermetic {
            for call in &mut reply.tool_calls {
                if let Err(error) =
                    crate::agent::window::file_handles::expand(&self.workspace, call)
                {
                    call.malformed_arguments = true;
                    ui.notice(&format!("File reference refused: {error}"));
                }
            }
        }
        // Hermetic turns disable repair; its live endpoint probe is not evaluated either.
        // Hermetic turns disable repair; its live endpoint probe is not evaluated either. A call
        // whose arguments were routed to another tool is corrected by naming that tool; forcing
        // it into some offered schema would pick an unrelated tool that tolerates the stray keys.
        if let Some(kind) = self.constrained_repair_kind(
            turn,
            argument_routes.is_empty(),
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
        // Compatibility can decode JSON containers and rename arguments. Recheck that exact
        // executable form before journaling any assistant/tool pair or prefetching a read.
        let mut normalized_rules =
            turn.rule_catalog
                .monitor(&turn.rule_root, &turn.rule_state, "main");
        let normalized_hit = normalized_rules.completed(&reply);
        if let Some(failure) = normalized_rules.failure {
            bail!("{failure}");
        }
        let normalized_deferred = normalized_rules.deferred;
        let scoped_paths = turn.rule_catalog.pending_paths(
            &turn.rule_root,
            &turn.rule_state,
            "main",
            &reply.tool_calls,
        );
        if normalized_hit.is_some() || scoped_paths.is_some() {
            record_rejected_reply(&self.usage_scope, backend, sent.repairing)?;
            return self.regenerate_for_guidance(
                turn,
                engine,
                sent.step,
                (normalized_hit, scoped_paths),
                Some("Applicable portable guidance must be delivered before the normalized tool attempt; no tools executed."),
                ui,
            ).map(ControlFlow::Break);
        }
        if !normalized_deferred.is_empty() {
            self.admit_rule_triggers_judged(turn, normalized_deferred, Vec::new(), ui)?;
        }
        if let (Some(warm), Some(sent)) = (&self.cache_warm, &sent.warm_request) {
            warm.settled(sent, &reply, &self.usage_scope);
        }
        if turn.final_step {
            // The wrap-up step answers; tools it still asks for would outlive the step limit.
            self.record(
                sent.step,
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
                super::super::step_limit_reason(turn.budget.limit())
            ));
            return self
                .stop_turn(ui, text, sent.step + 1)
                .map(crate::agent::run::engine::Tick::Finished)
                .map(ControlFlow::Break);
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
        // Recovered calls have no native streaming authority. Discard any prepared results;
        // the serial owner must obtain one-shot human approval for each call.
        if reply.text_calls_lifted {
            if let Some(streamed) = sent.streamed.as_mut() {
                streamed.invalidate();
            }
            sent.streamed = None;
        }
        Ok(ControlFlow::Continue(Round {
            step: sent.step,
            single_call: sent.single_call,
            planning_required: sent.planning_required,
            reply,
            context_before: sent.context_before,
            request_tools: sent.request_tools,
            temperature: sent.temperature,
            max_tokens: sent.max_tokens,
            seed: sent.seed,
            compatibility_loaded,
            argument_routes,
            repeat_streak: sent.repeat_streak,
            streamed: sent.streamed,
            outcome: ToolOutcome::default(),
        }))
    }
}

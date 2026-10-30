//! Step admission: budget and cost stops, portable-rule reload, steering resets, worker delivery
//! and model restoration before a request is built.
use super::*;
use std::ops::ControlFlow;

impl Agent<'_> {
    /// Admits the next step and returns its index, or the tick that ends the turn here.
    pub(super) fn begin_step(
        &mut self,
        turn: &mut TurnState<'_>,
        engine: &mut crate::agent::run::engine::Engine,
        backend: &mut Backend,
        schemas: &mut Vec<Value>,
        ui: &mut dyn Ui,
    ) -> Result<ControlFlow<crate::agent::run::engine::Tick<TurnReport>, u32>> {
        if !turn.budget.begin() {
            return Ok(ControlFlow::Break(
                crate::agent::run::engine::Tick::Exhausted,
            ));
        }

        turn.rule_state =
            crate::agent::safety::rules::State::restore(&self.session.control_state());
        turn.rule_regenerations = turn.rule_state.regenerations();
        if turn.rule_state.callback_pending() {
            turn.rule_catalog = Arc::new(if !self.hermetic && self.config.portable_rules.enabled {
                self.layout
                    .as_ref()
                    .map(|layout| {
                        crate::agent::safety::rules::Catalog::discover(
                            layout,
                            &turn.rule_root,
                            &self.config.portable_rules,
                        )
                    })
                    .unwrap_or_default()
            } else {
                crate::agent::safety::rules::Catalog::default()
            });
            self.context_rule_catalog = Some(turn.rule_catalog.clone());
            for warning in &turn.rule_catalog.warnings {
                ui.notice(warning);
            }
            turn.rule_state.clear_callback();
        }
        let step = turn.iteration;
        turn.iteration = turn.iteration.saturating_add(1);
        if turn.budget.stalled_without_reply() {
            self.record(
                step,
                "recovery_watchdog",
                json!({"steps_without_reply":crate::agent::safety::guard::StepBudget::MAX_STEPS_WITHOUT_REPLY}),
            )?;
            return self
                .stop_turn(
                    ui,
                    format!(
                        "Stopped: recovery started {} steps in a row without receiving a model reply (compaction, fallback or context retries are not making progress). Completed work is kept; check the provider and context settings, then say \"continue\" to resume.",
                        crate::agent::safety::guard::StepBudget::MAX_STEPS_WITHOUT_REPLY
                    ),
                    step,
                )
                .map(crate::agent::run::engine::Tick::Finished)
                .map(ControlFlow::Break);
        }
        if let Some(why) = self.stop_boundary() {
            return self
                .stop_turn(ui, why, step)
                .map(crate::agent::run::engine::Tick::Finished)
                .map(ControlFlow::Break);
        }
        if step > 0 && !turn.unpriced_cap_noted && self.cost_caps_unenforceable() {
            turn.unpriced_cap_noted = true;
            ui.notice("A [budget] cost cap is configured, but this model reports no price, so the cap cannot stop this turn. Use the step limit or --max-time to bound it.");
        }
        let accepted_live = self.flush_accepted_live_steering(ui)?;
        let routed_steering = self.route_pending_messages(backend, ui)?;
        let taken = self.take_steering(ui)?;
        // Steering deferred for the turn optimizer is drafted and journaled before this request.
        let drafted = self.draft_deferred_steering(backend, ui)?;
        if taken || accepted_live || routed_steering || drafted {
            turn.steering_pending = true;
        }
        if std::mem::take(&mut turn.steering_pending) {
            self.transition_turn(
                turn,
                engine,
                super::transition::Transition::Steering,
                backend,
                schemas,
            )?;
        }
        // After steering, whose reset would drop these one-shot notes.
        turn.final_step = !self.hermetic && turn.budget.on_last_step();
        if turn.final_step {
            turn.pending_nudges.push(json!({"role":"user","dial_transient":true,"content":"[Dial] Step limit reached: this is the final request of this turn. Call no tools; they would not run. Reply now with what was done and its evidence, what remains unverified or unfinished, and the exact next step."}));
        } else if !turn.wrap_up_noted
            && let Some(budget) = self.boundary_near(turn.turn_started)
        {
            turn.wrap_up_noted = true;
            turn.pending_nudges.push(json!({"role":"user","dial_transient":true,"content":format!("[Dial] About 80% of this turn's {budget} is used. Prioritize finishing: complete the change in progress, run the most relevant check, and report; leave optional work for later.")}));
        }
        self.pump_subagents(ui);
        self.cur_step = step;
        // The admitted step sent no request, so it is not counted.
        if self.interrupt.load(Ordering::SeqCst) {
            return Ok(ControlFlow::Break(
                crate::agent::run::engine::Tick::Finished(TurnReport::interrupted(step)),
            ));
        }
        // Worker results are persisted before the request is built, so they stay in context and survive
        // an overflow retry; each one is delivered exactly once.
        self.deliver_worker_results()?;
        // A plan approved inside this turn ends planner-model use at this boundary, so implementation
        // runs on the main model. The loop owns the backend, so it is swapped in place, and the
        // output budget is sized again for the model that now answers.
        if !self.plan_mode && self.plan_model_prior.is_some() {
            self.plan_model_resume()?;
            if let Some(mut main) = self.backend.take() {
                main.set_usage_scope(Some(self.usage_scope.clone()));
                let mut planner = std::mem::replace(backend, main);
                planner.release();
                turn.max_tokens = backend.output_budget(turn.harness.max_tokens);
            }
        }
        self.observe_cost_warnings(&mut turn.turn_cost_warn, ui);
        // A fallback whose cooldown elapsed returns to the primary before anything is sized for it.
        if self.restore_primary_if_due(backend, ui) {
            turn.max_tokens = backend.output_budget(turn.harness.max_tokens);
        }
        self.sync_mcp_tools(schemas, ui)?;
        Ok(ControlFlow::Continue(step))
    }
}

impl Agent<'_> {
    /// The stop reason once a turn-wide boundary is reached: the `--max-time` deadline or an
    /// observed session/turn cost cap. Checked when a step is admitted, between the calls of one
    /// tool round and before settlement makes further model calls, so a long round cannot run
    /// far past a boundary. The interrupt flag is separate: it ends a turn as interrupted.
    pub(in crate::agent) fn stop_boundary(&self) -> Option<String> {
        if self.modes.launch.expired() {
            return Some("Stopped: --max-time deadline reached.".into());
        }
        if self.hermetic {
            return None;
        }
        let spent = self.session_cost().unwrap_or(0.0);
        // The external-turn ledger includes earlier repair/hook rounds and auxiliary work.
        let turn_spent = self.usage_scope.observed_turn_cost().unwrap_or(0.0);
        let caps = &self.config.budget;
        let why = match (caps.max_session_cost_usd, caps.max_turn_cost_usd) {
            (Some(cap), _) if cap > 0.0 && spent >= cap => {
                format!("observed session API cost ${spent:.4} reached the ${cap:.4} stop boundary")
            }
            (_, Some(cap)) if cap > 0.0 && turn_spent >= cap => format!(
                "observed API cost this turn is ${turn_spent:.4}, reaching its ${cap:.4} stop boundary"
            ),
            _ => return None,
        };
        Some(format!(
            "Stopped: {why}. This is not an invoice ceiling: missing prices/usage, discarded attempts and in-flight workers may add charges. Raise [budget] in the configuration to continue."
        ))
    }

    /// The turn-wide boundary of which at least 80% is used: elapsed time towards the
    /// `--max-time` deadline since the turn started, or observed cost towards a cost cap.
    fn boundary_near(&self, turn_started: Instant) -> Option<&'static str> {
        const NEAR: f64 = 0.8;
        if let Some(deadline) = self.modes.launch.deadline {
            let total = deadline
                .saturating_duration_since(turn_started)
                .as_secs_f64();
            if total > 0.0 && turn_started.elapsed().as_secs_f64() >= NEAR * total {
                return Some("time limit (--max-time)");
            }
        }
        if self.hermetic {
            return None;
        }
        let near = |spent: Option<f64>, cap: Option<f64>| matches!((spent, cap), (Some(spent), Some(cap)) if cap > 0.0 && spent >= NEAR * cap);
        let caps = &self.config.budget;
        (near(self.session_cost(), caps.max_session_cost_usd)
            || near(
                self.usage_scope.observed_turn_cost(),
                caps.max_turn_cost_usd,
            ))
        .then_some("cost cap")
    }

    /// A positive cost cap is configured but no cost has been observed after a request: the
    /// model is unpriced and the cap can never trip.
    fn cost_caps_unenforceable(&self) -> bool {
        let caps = &self.config.budget;
        let capped = [caps.max_session_cost_usd, caps.max_turn_cost_usd]
            .into_iter()
            .flatten()
            .any(|cap| cap > 0.0);
        !self.hermetic
            && capped
            && self.session_cost().is_none()
            && self.usage_scope.observed_turn_cost().is_none()
    }

    /// The model request timeout, clamped to the time left before `--max-time` so one request
    /// cannot run far past the deadline. `0` keeps its meaning of no transport timeout.
    pub(super) fn request_timeout(&self) -> u64 {
        clamp_to_deadline(
            self.config.worker_timeout_secs,
            self.modes
                .launch
                .deadline
                .map(|deadline| deadline.saturating_duration_since(Instant::now())),
        )
    }

    /// Applies dynamic tool-list changes at a request boundary: the offered schemas and system
    /// prompt follow MCP lists and selected-model image availability; queued MCP notices reach the
    /// UI.
    fn sync_mcp_tools(&mut self, schemas: &mut Vec<Value>, ui: &mut dyn Ui) -> Result<()> {
        let images_changed = match self.ext.sync_image_generation(&self.auth_config) {
            Ok(changed) => changed,
            Err(error) => {
                ui.notice(&format!(
                    "image_gen availability: {}",
                    crate::storage::redact::redact_text(&format!("{error:#}"))
                ));
                false
            }
        };
        if self.ext.sync_mcp() || images_changed {
            let names = self.offered();
            *schemas = self.schemas(&names)?;
            self.rebuild_system();
        }
        for notice in self.ext.take_notices() {
            ui.notice(&notice);
        }
        Ok(())
    }
}

/// `timeout` seconds (0 = unlimited) bounded by `remaining`, rounded up and never below one second.
pub(super) fn clamp_to_deadline(timeout: u64, remaining: Option<std::time::Duration>) -> u64 {
    let Some(remaining) = remaining else {
        return timeout;
    };
    let left = remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0);
    let left = left.max(1);
    if timeout == 0 {
        left
    } else {
        timeout.min(left)
    }
}

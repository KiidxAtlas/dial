//! One engine tick of the parent turn. Each phase owns its request/reply facts, while the
//! canonical turn state and runtime owners remain borrowed for the duration of the tick.
use super::*;
use std::ops::ControlFlow;

mod admit;
mod generate;
mod prepare;
mod recover;
mod reply;

#[cfg(test)]
mod tests;

/// The runtime owners shared by phases; no turn state is copied into this context.
struct StepContext<'step, 'goal> {
    turn: &'step mut TurnState<'goal>,
    engine: &'step mut crate::agent::run::engine::Engine,
    backend: &'step mut Backend,
    schemas: &'step mut Vec<Value>,
    ui: &'step mut dyn Ui,
}

type Phase<T> = ControlFlow<crate::agent::run::engine::Tick<TurnReport>, T>;

/// A headroom-checked request. Guidance is still queued until a received reply confirms delivery.
struct PreparedRequest {
    step: u32,
    prep_started: Instant,
    request: Chat,
    selection: crate::adaptive::Selection,
    request_control: Value,
    local_control: Option<Value>,
    request_overhead: crate::agent::window::context_meter::RequestOverhead,
    request_estimate: u64,
    request_context: u64,
    sent_messages: usize,
    lightweight: bool,
    single_call: bool,
    planning_required: bool,
    research_step: bool,
    repeat_streak: Option<crate::agent::safety::guard::RepeatedToolCall>,
}

/// Immutable request facts needed for recovery, completion and eventual tool settlement.
struct SentRequest {
    step: u32,
    started: Instant,
    single_call: bool,
    planning_required: bool,
    research_step: bool,
    repairing: bool,
    requested_tokens: u32,
    request_estimate: u64,
    request_context: u64,
    sent_messages: usize,
    context_before: Vec<Value>,
    request_tools: Vec<Value>,
    offer: Vec<Value>,
    profile: Option<crate::llm::profiles::ModelProfile>,
    temperature: f64,
    max_tokens: u32,
    seed: Option<u64>,
    warm_request: Option<Chat>,
    repeat_streak: Option<crate::agent::safety::guard::RepeatedToolCall>,
    streamed: Option<crate::agent::run::streaming_dispatch::StreamingDispatch>,
}

/// Transport output after cancellation, steering and temporary transport state have settled.
struct Generation {
    result: Result<llm::Reply>,
    rule_deferred: Vec<crate::agent::safety::rules::Hit>,
    sent: SentRequest,
}

/// A received reply, still subject to completion/recovery and executable-call admission.
struct ReceivedReply {
    reply: llm::Reply,
    sent: SentRequest,
}

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
        let mut context = StepContext {
            turn,
            engine,
            backend,
            schemas,
            ui,
        };
        let result = self.step_phases(&mut context);
        // Preparation and post-stream persistence can fail before the normal generation cleanup.
        // Never let one tick's profile or failover hint leak into a subsequent request.
        context.backend.next_request_profile(None);
        context.backend.next_request_failover(None);
        result
    }

    fn step_phases(
        &mut self,
        context: &mut StepContext<'_, '_>,
    ) -> Result<crate::agent::run::engine::Tick<TurnReport>> {
        let step = match self.begin_step(
            context.turn,
            context.engine,
            context.backend,
            context.schemas,
            context.ui,
        )? {
            ControlFlow::Continue(step) => step,
            ControlFlow::Break(tick) => return Ok(tick),
        };
        let prepared = match self.prepare_step_request(context, step)? {
            ControlFlow::Continue(prepared) => prepared,
            ControlFlow::Break(tick) => return Ok(tick),
        };
        let generation = match self.generate_step(context, prepared)? {
            ControlFlow::Continue(generation) => generation,
            ControlFlow::Break(tick) => return Ok(tick),
        };
        let received = match self.recover_generation(context, generation)? {
            ControlFlow::Continue(received) => received,
            ControlFlow::Break(tick) => return Ok(tick),
        };
        let received = match self.process_reply_or_finish(context, received)? {
            ControlFlow::Continue(received) => received,
            ControlFlow::Break(tick) => return Ok(tick),
        };
        let round = match self.admit_tool_round(context, received)? {
            ControlFlow::Continue(round) => round,
            ControlFlow::Break(tick) => return Ok(tick),
        };
        self.execute_round(
            context.turn,
            context.engine,
            context.backend,
            context.schemas,
            context.ui,
            round,
        )
    }

    /// Discard a generation that portable guidance must precede and request it again with the
    /// guidance delivered: a fired rule is remembered, file-scoped rules are activated for
    /// `paths`. Bounded by `portable_rules.max_regenerations`; at the limit the turn stops
    /// gracefully, and nothing from the discarded generation ran.
    pub(super) fn regenerate_for_guidance(
        &mut self,
        turn: &mut TurnState<'_>,
        engine: &mut crate::agent::run::engine::Engine,
        step: u32,
        guidance: (
            Option<crate::agent::safety::rules::Hit>,
            Option<std::collections::BTreeSet<String>>,
        ),
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
            engine,
            super::progress::Setback::GuidanceRegeneration,
            step,
            ui,
        )? {
            return Ok(crate::agent::run::engine::Tick::Finished(report));
        }
        turn.rule_regenerations += 1;
        turn.rule_state.set_regenerations(turn.rule_regenerations);
        let (hit, paths) = guidance;
        if let Some(hit) = hit {
            turn.rule_state
                .remember(&hit, "main")
                .map_err(anyhow::Error::msg)?;
            self.announce_rule(&hit, crate::agent::safety::rules::Trigger::Interrupt, ui);
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

    /// Journals the stream-rule matches that let an accepted main-agent reply proceed, then asks
    /// judged `question` rules about it. Every rule that fires is announced and its reminder
    /// reaches the next request.
    fn admit_rule_triggers(
        &mut self,
        turn: &mut TurnState<'_>,
        reply: &llm::Reply,
        deferred: Vec<crate::agent::safety::rules::Hit>,
        ui: &mut dyn Ui,
    ) -> Result<()> {
        let mut judged = Vec::new();
        if self.judges_rules() && !self.interrupt.load(Ordering::SeqCst) {
            for candidate in turn.rule_catalog.judged_candidates(
                &turn.rule_root,
                &turn.rule_state,
                "main",
                reply,
            ) {
                match crate::judgment::judge_rule(
                    &self.auth_config,
                    &self.config.user_dir,
                    &candidate.question,
                    &candidate.content,
                    Some(&self.usage_scope),
                    self.interrupt.clone(),
                ) {
                    Ok(judgment) if judgment.choice == crate::judgment::RULE_CHOICES[0] => {
                        judged.push(candidate.hit);
                    }
                    Ok(_) => {}
                    Err(error) => ui.diagnostic(&format!(
                        "Stream rule {} was not judged: {error:#}",
                        candidate.hit.source()
                    )),
                }
            }
        }
        self.admit_rule_triggers_judged(turn, deferred, judged, ui)
    }

    fn admit_rule_triggers_judged(
        &mut self,
        turn: &mut TurnState<'_>,
        deferred: Vec<crate::agent::safety::rules::Hit>,
        judged: Vec<crate::agent::safety::rules::Hit>,
        ui: &mut dyn Ui,
    ) -> Result<()> {
        use crate::agent::safety::rules::Trigger;
        if deferred.is_empty() && judged.is_empty() {
            return Ok(());
        }
        let fired = deferred
            .iter()
            .map(|hit| (hit, Trigger::Deferred))
            .chain(judged.iter().map(|hit| (hit, Trigger::Judged)));
        for (hit, trigger) in fired {
            turn.rule_state
                .remember(hit, "main")
                .map_err(anyhow::Error::msg)?;
            self.announce_rule(hit, trigger, ui);
        }
        let mut control = self.session.control_state();
        turn.rule_state.save(&mut control);
        self.session.set_control_state(control)?;
        Ok(())
    }

    /// Whether judged `question` rules are asked: `portable_rules.judge`, where `auto` requires
    /// an explicitly assigned `decision` role.
    fn judges_rules(&self) -> bool {
        use crate::config::types::RuleJudge;
        !self.hermetic
            && match self.config.portable_rules.judge {
                RuleJudge::Off => false,
                RuleJudge::On => true,
                RuleJudge::Auto => self.auth_config.role_selection("decision").is_some(),
            }
    }

    /// Shows that a stream rule fired.
    fn announce_rule(
        &self,
        hit: &crate::agent::safety::rules::Hit,
        trigger: crate::agent::safety::rules::Trigger,
        ui: &mut dyn Ui,
    ) {
        ui.notice(&hit.notice(trigger));
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
        engine: &mut crate::agent::run::engine::Engine,
        step: u32,
        kind: &'static str,
        pending: Pending,
        ui: &mut dyn Ui,
    ) -> Result<crate::agent::run::engine::Tick<TurnReport>> {
        if let Some(report) = self.setback(
            turn,
            engine,
            super::progress::Setback::UnusableReply,
            step,
            ui,
        )? {
            return Ok(crate::agent::run::engine::Tick::Finished(report));
        }
        let model = self.auth_config.selection();
        self.record(step, "constrained_repair", json!({"phase":"requested","trigger":kind,"provider":model.provider,"model":model.model}))?;
        self.request_phase
            .recover("the previous tool call was malformed");
        let failed = turn.pending.get_or_insert(pending);
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

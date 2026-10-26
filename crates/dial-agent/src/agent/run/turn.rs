//! Parent-turn orchestration: live steering, durable session state and learning policy.
//! Model transport, result recovery and native execution are shared with worker controllers.
//!
//! [`Agent::run_round`] drives one [`Agent::turn_step`] per engine tick over a [`TurnState`].
use crate::agent::modes::lifecycle::Lifecycle;
use crate::agent::*;

mod begin;
mod progress;
mod round;
mod settle;
mod step;
mod transition;

/// A reply admitted for tool execution, with the request facts its round still needs.
struct Round {
    step: u32,
    single_call: bool,
    planning_required: bool,
    reply: llm::Reply,
    /// Exact history and tools sent, kept for tool-recovery and training capture.
    context_before: Vec<Value>,
    request_tools: Vec<Value>,
    /// Sampling of the sent request, reused if the round rolls over to a new context.
    temperature: f64,
    max_tokens: u32,
    seed: Option<u64>,
    /// Tools loaded for compatibility this round; calls to them are retried next request.
    compatibility_loaded: HashSet<String>,
    repeat_streak: Option<crate::agent::safety::guard::RepeatedToolCall>,
    streamed: Option<crate::agent::run::streaming_dispatch::StreamingDispatch>,
    outcome: ToolOutcome,
}

/// What a round's tool calls produced; written by dispatch, consumed by settlement.
#[derive(Default)]
struct ToolOutcome {
    narration_warning: Option<String>,
    /// An identical repeated call set was answered from earlier results instead of running.
    short_circuited: bool,
    form_kinds: Vec<String>,
    rejected_calls: Vec<Value>,
    execution_failed: bool,
    observed_check_failure: bool,
    /// `experimental.outcome_gradient_rounds`: checks observed this round and their failures.
    checked: bool,
    failing_tests: std::collections::BTreeSet<String>,
    unnamed_failures: usize,
    stop: Option<String>,
    succeeded: bool,
    last_tool_failure: Option<String>,
    edit_failed: bool,
    finished: Option<crate::agent::verify::completion::Finish>,
    steering: Vec<String>,
    portable_batch_intercepted: bool,
}

/// Everything one parent turn carries from one engine tick to the next.
pub(in crate::agent) struct TurnState<'a> {
    user_goal: &'a str,
    harness: crate::config::Harness,
    adapter: Option<PathBuf>,
    cancel: crate::runtime::cancellation::TurnCancellation,
    turn_started: Instant,
    budget: crate::agent::safety::guard::StepBudget,
    iteration: u32,
    names: Vec<String>,
    // Portable rules.
    rule_root: PathBuf,
    rule_catalog: Arc<crate::agent::safety::rules::Catalog>,
    rule_state: crate::agent::safety::rules::State,
    rule_regenerations: u32,
    // Pre-edit assumption checks and post-edit reviews.
    reviewed_paths: std::collections::BTreeSet<String>,
    prechecked_paths: std::collections::BTreeSet<String>,
    revalidated_paths: std::collections::BTreeSet<String>,
    // Request sizing, compaction and output recovery.
    pending: Option<Pending>,
    /// Context overflows since the last reply; bounds overflow recovery per request, not per turn.
    overflow_retries: u32,
    compaction_failed: bool,
    /// A compaction ran since the last reply. If the next request is still over the compaction
    /// threshold, compacting again cannot help, so it is treated as a failed compaction.
    compacted_without_reply: bool,
    /// After a failed blocking compaction, retry only once the request has grown by a real
    /// amount (or would overflow): retrying every tool step re-ran the whole method chain.
    compaction_retry_floor: Option<u64>,
    /// The user was told once that a configured cost cap cannot trip on an unpriced model.
    unpriced_cap_noted: bool,
    /// The model was told once that most of the time or cost budget is used.
    wrap_up_noted: bool,
    /// The current step is the round's last: it must answer, not start tools.
    final_step: bool,
    max_tokens: u32,
    recovering_output: bool,
    repair: crate::agent::model::constrained_repair::Repair,
    stall_continues: u32,
    rejected_edit_previews: u32,
    reasoning_budget: Option<u32>,
    turn_effort: Option<crate::llm::profiles::ReasoningEffort>,
    // Guidance, coaching and planning.
    todo_recovery: crate::agent::workflow::todo_recovery::TodoRecovery,
    solo_todo_noted: bool,
    pending_nudges: Vec<Value>,
    emissions: crate::agent::run::guidance::Emissions,
    coach_turn: modes::advisor::CoachTurn,
    automatic_coach_pending: bool,
    auto_continues: u32,
    planning_notice: bool,
    plan_after_gate: bool,
    // Tool-contract reliability and stall/pace guards.
    measured: Option<Value>,
    reliability: crate::reliability::Stats,
    policy_signature: String,
    stall: crate::agent::safety::guard::StallGuard,
    pace: crate::agent::safety::guard::PaceGuard,
    // Completion acceptance.
    completion: crate::agent::verify::completion::CompletionGuard,
    completion_recovery: crate::agent::verify::completion::CompletionRecovery,
    completion_candidate: Option<String>,
    /// Recovery attempts since the last progress, across every recovery mechanism.
    progress: progress::ProgressLedger,
    // Background enrichment and steering.
    enrichment: crate::agent::run::enrichment::Enrichment,
    enrichment_epoch: u64,
    steering_pending: bool,
}

impl TurnState<'_> {
    /// A reply answered the last request: its one-shot notes reached the model, and per-request
    /// recovery (overflow retries, the no-reply watchdog, compaction effect) starts over. Until
    /// then a discarded request (fallback, overflow, steering, guidance regeneration) resends them.
    fn reply_received(&mut self) {
        self.pending_nudges.clear();
        self.emissions.confirm();
        self.budget.replied();
        self.overflow_retries = 0;
        self.compacted_without_reply = false;
    }

    /// Refund the step of an unusable reply. Once the free retries are spent the retry is
    /// charged as a real step, which the user is told so a shrinking budget is not a surprise.
    fn refund_unusable(&mut self, ui: &mut dyn Ui) {
        if !self.budget.refund() {
            ui.diagnostic(
                "Free retries for unusable replies are spent; this retry counts as a step.",
            );
        }
    }

    /// A compaction made room: retry the request and allow compaction again later this turn.
    fn compaction_succeeded(&mut self) {
        self.compaction_failed = false;
        self.compaction_retry_floor = None;
        self.compacted_without_reply = true;
    }

    /// Blocking compaction could not make room for a request of `request_context` tokens; it is
    /// retried only once the request has grown by a real amount.
    fn compaction_failed_at(&mut self, request_context: u64) {
        self.compaction_failed = true;
        self.compaction_retry_floor = Some(
            request_context.saturating_add(crate::config::compaction::SPECULATION_LEAD_MIN_TOKENS),
        );
    }
}

impl Agent<'_> {
    /// One user turn's first round with fresh turn state; see [`Agent::run_round`]. Production
    /// turns hold their state across rounds in `turn_inner`; tests drive a single round here.
    #[cfg(test)]
    pub(in crate::agent) fn run_loop(
        &mut self,
        backend: &mut Backend,
        names: &[String],
        schemas: &mut Vec<Value>,
        max_steps: u32,
        user_goal: &str,
        ui: &mut dyn Ui,
    ) -> Result<(TurnReport, u32)> {
        let mut turn = self.begin_turn(backend, names, max_steps, user_goal, ui)?;
        let mut engine = crate::agent::run::engine::Engine::default();
        self.run_round(&mut turn, &mut engine, backend, schemas, max_steps, ui)
    }

    /// Resume a turn whose final answer verification or a stop hook rejected, with a step
    /// allowance of its own. The turn keeps its judged effort, portable-rule scope, delivered
    /// advice, reviews and failure history instead of starting over; the caller has already
    /// pushed the feedback that resumes it.
    pub(in crate::agent) fn resume_round(
        &mut self,
        turn: &mut TurnState<'_>,
        engine: &mut crate::agent::run::engine::Engine,
        backend: &mut Backend,
        schemas: &mut Vec<Value>,
        max_steps: u32,
        ui: &mut dyn Ui,
    ) -> Result<(TurnReport, u32)> {
        turn.budget = self.step_budget(max_steps);
        self.transition_turn(
            turn,
            engine,
            transition::Transition::Resumed,
            backend,
            schemas,
        )?;
        self.run_round(turn, engine, backend, schemas, max_steps, ui)
    }

    /// Drive steps until the turn finishes or `max_steps` (the budget already in `turn`) runs
    /// out. Returns the report, whose `steps` count this round's attempts, and the work steps
    /// the round consumed.
    pub(in crate::agent) fn run_round(
        &mut self,
        turn: &mut TurnState<'_>,
        engine: &mut crate::agent::run::engine::Engine,
        backend: &mut Backend,
        schemas: &mut Vec<Value>,
        max_steps: u32,
        ui: &mut dyn Ui,
    ) -> Result<(TurnReport, u32)> {
        // Step indices keep counting across rounds so journal records stay distinct; reports
        // count this round's attempts, which the caller adds to the turn total.
        let first = turn.iteration;
        let report = self.drive_round(turn, engine, backend, schemas, max_steps, ui)?;
        Ok((
            TurnReport {
                steps: report.steps.saturating_sub(first),
                ..report
            },
            turn.budget.spent(),
        ))
    }

    fn drive_round(
        &mut self,
        turn: &mut TurnState<'_>,
        engine: &mut crate::agent::run::engine::Engine,
        backend: &mut Backend,
        schemas: &mut Vec<Value>,
        max_steps: u32,
        ui: &mut dyn Ui,
    ) -> Result<TurnReport> {
        let driven = engine.drive(|engine| self.turn_step(turn, engine, backend, schemas, ui));
        let driven = match driven {
            Err(error)
                if error
                    .downcast_ref::<crate::agent::run::engine::UncertainRefusal>()
                    .is_some() =>
            {
                let reason = engine
                    .pending_refusal()
                    .unwrap_or("An earlier tool ended with uncertain effects.");
                return self.stop_turn(
                    ui,
                    format!("Stopped: {reason} Automatic continuation was refused; completed effects were not rolled back or replayed. Say \"continue\" once the affected state is inspected."),
                    turn.iteration,
                );
            }
            driven => driven?,
        };
        if let Some(report) = driven {
            return Ok(report);
        }

        if self.hermetic {
            bail!("agent exhausted the admitted step budget without a final answer")
        }
        let reason = step_limit_reason(max_steps);
        let reason = match turn.completion_candidate.take() {
            Some(candidate) => format!("{candidate}\n\n{reason}"),
            None => reason,
        };
        self.stop_turn(ui, reason, turn.iteration)
    }
}

/// Why a round ended at its finite step limit, and how to go on.
fn step_limit_reason(max_steps: u32) -> String {
    format!(
        "Stopped: this turn used its {max_steps}-step limit. The chat and completed work are kept. Say \"continue\" to keep going, or raise harness.max_steps in /settings."
    )
}

impl Agent<'_> {
    /// A round's step allowance. Evaluation runs get no free retries: their budget is what
    /// they measure.
    fn step_budget(&self, max_steps: u32) -> crate::agent::safety::guard::StepBudget {
        crate::agent::safety::guard::StepBudget::new(
            max_steps,
            if self.hermetic {
                0
            } else {
                crate::agent::safety::guard::StepBudget::FREE_RETRIES
            },
        )
    }

    /// Turn-scoped setup, once per user turn: portable rules, thinking effort and reliability
    /// policy. Repair and stop-hook rounds reuse it through [`Agent::resume_round`].
    pub(in crate::agent) fn begin_turn<'g>(
        &mut self,
        backend: &mut Backend,
        names: &[String],
        max_steps: u32,
        user_goal: &'g str,
        ui: &mut dyn Ui,
    ) -> Result<TurnState<'g>> {
        let names = names.to_vec();
        let rule_root = self.workspace.root().to_path_buf();
        let rule_catalog = Arc::new(if !self.hermetic && self.config.portable_rules.enabled {
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
        let rule_regenerations: u32 = rule_state.regenerations();
        rule_state.set_regenerations(0);
        rule_state.clear_callback();
        let mut control = self.session.control_state();
        rule_state.save(&mut control);
        self.session.set_control_state(control)?;
        let reviewed_paths = std::collections::BTreeSet::new();
        let prechecked_paths = std::collections::BTreeSet::new();
        let revalidated_paths = std::collections::BTreeSet::new();
        let harness = self.config.effective_harness(&self.version.harness)?;
        let adapter = self.version.adapter.clone();
        let max_tokens = backend.output_budget(harness.max_tokens);
        self.report_retained_fallback(ui);
        let cancel = self.cancellation.turn(self.interrupt.clone());
        // `thinking = "auto"`: judge this user turn's effort once from its prompt, not per step.
        // External thinking promises provider reasoning off, so no judge runs for it.
        let mut turn_effort: Option<crate::llm::profiles::ReasoningEffort> = None;
        if self.config.thinking_auto
            && !self.hermetic
            && self.external_thinking_instruction().is_none()
        {
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
        let reliability: crate::reliability::Stats =
            serde_json::from_value(self.session.control_state()["reliability"]["stats"].clone())
                .unwrap_or_default();
        let turn_started = self.workflow_started;
        if !self.hermetic {
            let mut control = self.session.control_state();
            control["reliability"]["measured"] = json!(measured);
            self.session.set_control_state(control.clone())?;
            ui.request_policy(&control);
        }
        Ok(TurnState {
            user_goal,
            harness,
            adapter,
            cancel,
            turn_started,
            budget: self.step_budget(max_steps),
            iteration: 0,
            names,
            rule_root,
            rule_catalog,
            rule_state,
            rule_regenerations,
            reviewed_paths,
            prechecked_paths,
            revalidated_paths,
            pending: None,
            overflow_retries: 0,
            compacted_without_reply: false,
            compaction_failed: false,
            compaction_retry_floor: None,
            unpriced_cap_noted: false,
            wrap_up_noted: false,
            final_step: false,
            max_tokens,
            recovering_output: false,
            repair: Default::default(),
            stall_continues: 0,
            rejected_edit_previews: 0,
            reasoning_budget: None,
            turn_effort,
            todo_recovery: Default::default(),
            solo_todo_noted: false,
            pending_nudges: Vec::new(),
            emissions: Default::default(),
            coach_turn: Default::default(),
            automatic_coach_pending: false,
            auto_continues: 0,
            planning_notice: false,
            plan_after_gate: false,
            measured,
            reliability,
            policy_signature: String::new(),
            stall: Default::default(),
            pace: Default::default(),
            completion: Default::default(),
            completion_recovery: Default::default(),
            completion_candidate: None,
            progress: Default::default(),
            enrichment: Default::default(),
            enrichment_epoch: 0,
            steering_pending: false,
        })
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

#[cfg(test)]
mod completion_tests;

#[cfg(test)]
mod reliability_tests;

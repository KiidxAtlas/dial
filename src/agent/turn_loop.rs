//! The turn entry points and main step loop.

use super::*;

impl<'a> Agent<'a> {
    /// One user message through to a final answer (or interruption / step limit).
    pub fn turn(&mut self, user_text: &str, ui: &mut dyn Ui) -> Result<TurnReport> {
        self.turn_with_goal(user_text, ui)
    }
    /// RPC admission already cleared its interrupt before enqueueing; preserve a subsequent abort.
    pub fn turn_preserving_interrupt(
        &mut self,
        user_text: &str,
        ui: &mut dyn Ui,
    ) -> Result<TurnReport> {
        self.turn_with_state(user_text, ui, false, None)
    }
    /// ACP has already resolved explicit content blocks. Do not interpret resource bodies as
    /// terminal @file syntax: a referenced document must not attach other local files implicitly.
    pub(crate) fn turn_prepared_preserving_interrupt(
        &mut self,
        user_text: &str,
        message: Value,
        ui: &mut dyn Ui,
    ) -> Result<TurnReport> {
        ensure!(
            message["role"] == "user" && message["content"].as_str() == Some(user_text),
            "invalid prepared user message"
        );
        self.turn_with_state(user_text, ui, false, Some(message))
    }
    pub(super) fn turn_with_state(
        &mut self,
        user_text: &str,
        ui: &mut dyn Ui,
        reset_interrupt: bool,
        prepared: Option<Value>,
    ) -> Result<TurnReport> {
        if let Some(error) = &self.parts.system_template_error {
            anyhow::bail!("{error}");
        }
        self.note_compaction_input();
        self.usage_scope = self.usage_scope.next_turn(self.config.harness.max_steps)?;
        if let Some(backend) = &mut self.backend {
            backend.set_usage_scope(Some(self.usage_scope.clone()));
        }
        for backend in self.role_backends.values_mut() {
            backend.set_usage_scope(Some(self.usage_scope.clone()));
        }
        let mut observed =
            state_ui::StateUi::with_registry(ui, self.session.runtime(), None, self.ext.clone());
        self.live_tasks.bind(self.session.runtime());
        let turn_clock = Instant::now();
        let result = self.turn_inner(user_text, &mut observed, reset_interrupt, prepared);
        if let Err(error) = self.flush_local_evidence() {
            observed.diagnostic(&format!("Local model evidence not saved: {error:#}"));
        }
        if result
            .as_ref()
            .is_ok_and(|report| report.outcome == TurnOutcome::Completed)
        {
            // Goal mode keeps todos as the deliverable audit, so an accepted report never closes them there.
            if !self.modes.goal.tool_offered() {
                self.todo.complete_reported_work();
            }
            let mut control = self.session.control_state();
            control["todo"] = self.todo.snapshot();
            self.session.set_control_state(control)?;
            observed.todo(&self.todo);
        }
        // Observe after the final control append: it changes the branch revision that the
        // observation is bound to, so an earlier capture would be discarded as stale.
        if let Some(memory) = &self.memory_runtime {
            let authority = memory_authority(&self.session, 0);
            memory.resume(authority.clone());
            let messages = self.session.archived_user_assistant_messages();
            if let Err(error) = memory.observe_owned(&authority, messages) {
                observed.notice(&format!(
                    "Selected memory backend did not record this conversation revision: {error:#}"
                ));
            }
        }
        self.clear_context_rollover();
        self.note_compaction_input();
        let state = self.session.control_state();
        let changed_files = self
            .step_checks
            .turn
            .and_then(|turn| {
                self.checkpoints
                    .as_ref()
                    .map(|checkpoints| checkpoints.changed_files(turn))
            })
            .unwrap_or_default();
        if let (Some(history), Some(checkpoints), Some(turn)) =
            (&self.history, &self.checkpoints, self.step_checks.turn)
            && !changed_files.is_empty()
        {
            let recorded = checkpoints.turn_versions(turn).and_then(|versions| {
                let branch = crate::storage::history::current_branch(self.workspace.root());
                history.record(
                    self.session.id(),
                    turn,
                    &prompt::truncate_chars(user_text, 200),
                    branch.as_deref(),
                    &versions,
                )
            });
            if let Err(error) = recorded {
                observed.diagnostic(&format!("Code history not recorded: {error:#}"));
            }
        }
        let (outcome, steps) = match &result {
            Ok(report) => (
                serde_json::to_value(report.outcome)
                    .unwrap_or(Value::Null)
                    .as_str()
                    .unwrap_or("failed")
                    .to_owned(),
                report.steps,
            ),
            Err(_) => ("failed".to_string(), 0),
        };
        crate::telemetry::turn(
            self.config,
            self.hermetic,
            self.session.id(),
            &outcome,
            u64::from(steps),
            turn_clock.elapsed(),
        );
        let summary = json!({"outcome":outcome,"steps":steps,"checkpoint_turn":self.step_checks.turn,"changed_files":changed_files,"open_tasks":self.todo.tasks().iter().filter(|task| task.status.is_open()).count(),"recorded_checks":state["reliability"]["verification_results"],"checks_current":self.step_checks.checks_current(),"review":"/review-file","undo":"/undo"});
        observed.completion(&summary);
        observed.finish(!result.as_ref().is_ok_and(|turn| !turn.interrupted));
        let result = match (result, self.session.flush_runtime()) {
            (Err(error), Err(journal)) => {
                Err(error.context(format!("execution journal also failed: {journal:#}")))
            }
            (result, Ok(())) => result,
            (Ok(_), Err(journal)) => Err(journal),
        };
        if result
            .as_ref()
            .is_ok_and(|report| report.outcome == TurnOutcome::Completed && !report.interrupted)
        {
            self.metrics.turns_completed += 1;
        }
        self.seal_pending_memory_capture(
            result.as_ref().is_ok_and(|report| {
                report.outcome == TurnOutcome::Completed && !report.interrupted
            }),
        );
        result
    }
    pub(super) fn turn_inner(
        &mut self,
        user_text: &str,
        ui: &mut dyn Ui,
        reset_interrupt: bool,
        prepared: Option<Value>,
    ) -> Result<TurnReport> {
        let capture_start = (self.config.memory.autolearn
            && self.config.memory.autolearn_auto_continue)
            .then(|| self.session.branch_leaf().map(str::to_owned));
        self.clear_context_rollover();
        ensure!(
            !user_text.trim().is_empty() && user_text.len() <= 262_144,
            "message must be 1..262144 bytes"
        );
        if reset_interrupt {
            self.interrupt.store(false, Ordering::SeqCst);
        }
        let plugins = self.hook_plugins();
        if hooks::wanted(self.tool_hooks(), plugins.as_deref(), "user_prompt_submit") {
            let configured_hooks = self.tool_hooks();
            let root = self.workspace.root();
            let interrupt = self.interrupt.as_ref();
            let payload = json!({"prompt":user_text,"session_id":self.session.id()});
            let (verdict, notices) = hooks::with_heartbeat(ui, || {
                let outcome = hooks::fire_with(
                    configured_hooks,
                    plugins.as_deref(),
                    root,
                    "user_prompt_submit",
                    None,
                    payload,
                    interrupt,
                );
                (outcome.verdict, outcome.notices)
            });
            notices.iter().for_each(|n| {
                ui.event(&crate::agent::ui_event::UiEvent::new(
                    crate::agent::ui_event::EventKind::Hook,
                    n,
                ))
            });
            match verdict {
                hooks::Verdict::Block { reason } => {
                    bail!("message blocked by a user_prompt_submit hook: {reason}")
                }
                hooks::Verdict::Continue { output } => {
                    if !output.is_empty() {
                        ui.event(&crate::agent::ui_event::UiEvent::new(
                            crate::agent::ui_event::EventKind::Hook,
                            format!("hook: {output}"),
                        ));
                    }
                }
            }
        }
        if std::mem::take(&mut self.legacy_memory_migrated) {
            ui.diagnostic("Migrated legacy memory tool selection to provenance memory tools for this runtime; admitted history is unchanged.");
        }
        self.task_policy = crate::learn::task_policy::Policy::for_prompt(
            user_text,
            self.config.reasoning_guard.max_pre_action_secs,
        );
        self.request_phase = crate::adaptive::State::begin(self.task_policy.kind, user_text);
        self.workflow_started = Instant::now();
        self.efficiency_metrics = Default::default();
        *self.workflow_metrics.borrow_mut() = Default::default();
        self.refresh_version(ui, user_text)?;
        // User-managed skill imports/toggles take effect between turns, including RPC and CLI changes.
        if let Some(layout) = &self.layout {
            let catalog = if self.modes.skillful == Some(false) {
                String::new()
            } else {
                let visible: Vec<_> = skills::discover(layout)
                    .into_iter()
                    .filter(|skill| self.modes.launch.skill_visible(&skill.name))
                    .collect();
                skills::catalog_block(&visible)
            };
            if catalog != self.parts.skills {
                self.parts.skills = catalog;
                self.rebuild_system();
            }
        }
        // A step this local model reached in an earlier session applies from the first request.
        self.seed_local_escalation();
        self.begin_local_turn(ui);
        // Offered tools can change with version/harness refresh or skill toggles; rebuild only if content changed.
        self.rebuild_system();
        for notice in self.ext.take_notices() {
            ui.notice(&notice);
        }
        self.probe_context();
        if let Some(served) = self.context_cap {
            self.local_context_advice(served, ui);
        }
        self.step_probe = self
            .store
            .map(crate::learn::probe::load)
            .transpose()?
            .flatten()
            .filter(crate::learn::probe::informative);
        self.session.repair_incomplete_tools()?;
        let message = if let Some(message) = prepared {
            message
        } else if let Some(layout) = &self.layout {
            let (message, warnings) =
                context::expand_message(&layout.project_root, user_text, &self.interrupt);
            for warning in warnings {
                ui.notice(&warning);
            }
            message
        } else {
            json!({"role":"user","content":user_text})
        };
        let checkpoint_turn = self
            .checkpoints
            .as_ref()
            .map(|checkpoints| checkpoints.begin_turn(&prompt::truncate_chars(user_text, 60)))
            .transpose()?;
        if !self.hermetic {
            let mut control = self.session.control_state();
            let previous_receipts = control["reliability"]["verification_receipts"]
                .as_array()
                .cloned();
            control["reliability"] = json!({
                "stats":crate::reliability::Stats::default(),
                "verification":"no checks recorded this turn",
                "interface":"standard",
                "automatic_passing_checks":0,
                "automatic_failing_checks":0,
                "edit_loop":{"phase":"no_edits","mutations":0,"checked_mutations":0,"diff_surfaced_mutations":0,"uncheckpointed_shell_commands":0,"disclosed_shell_commands":0,"uncheckpointed_extension_calls":0,"disclosed_extension_calls":0}
            });
            if let Some(receipts) =
                previous_receipts.filter(|_| self.config.experimental.verification_receipts)
            {
                control["reliability"]["verification_receipts"] = json!(receipts);
            }
            self.session.set_control_state(control.clone())?;
            ui.request_policy(&control);
        }
        self.step_checks = StepChecks {
            turn: checkpoint_turn,
            visual: if self.hermetic {
                visual::Review::default()
            } else {
                visual::Review::new(user_text)
            },
            ..StepChecks::default()
        };
        if !self.hermetic
            && message.get("dial_images").is_some()
            && self
                .backend
                .as_ref()
                .is_some_and(Backend::accepts_image_input)
        {
            self.step_checks.visual.captured((0, 0), false);
        }
        for reference in message["dial_images"].as_array().into_iter().flatten() {
            ui.image(reference);
        }
        self.repair_tracker.clear();
        self.repair_sequence = 0;
        self.ext.fire_lifecycle(&LifecycleEvent {
            event: "turn_started".into(),
            session_id: Some(self.session.id().to_owned()),
            turn_id: checkpoint_turn.map(|turn| turn.to_string()),
            tool_name: None,
            is_error: None,
        });
        self.trajectory = match self.store {
            Some(store) if self.version.id != 0 => {
                let task = self.domain_override.as_ref().map_or_else(
                    || format!("session:{}", self.session.id()),
                    |_| "selfplay".to_owned(),
                );
                let domain = self
                    .domain_override
                    .clone()
                    .unwrap_or_else(|| self.config.use_case.clone());
                let id = store.begin_trajectory(self.version.id, &domain, &task, user_text)?;
                Some((id, domain, task))
            }
            _ => None,
        };
        self.assistance.begin(user_text);
        self.training_capture.clear();
        self.verifications.clear();
        self.behavioral_checks.clear();
        self.reasoning_recovery = reasoning::Recovery::default();
        self.logging = self.store.is_some();
        let mut message = message;
        if let (Some(hint), Some(content)) = (
            self.skill_hint(user_text),
            message["content"].as_str().map(str::to_owned),
        ) {
            message["content"] = json!(format!("{content}\n\n{hint}"));
        }
        // Recalled notes and verified repair patterns are decided once, for this message, and stored with it.
        // Recomputing them on every request (or putting them in the system prompt, as hints once were) would
        // change the text in front of the whole conversation and discard the provider's prompt cache.
        let mut context = self.experience_context(user_text, ui);
        self.invalidate_exploration(
            "new external user turn superseded the retained checkpoint",
            ui,
        )?;
        let recall_authority = memory_authority(&self.session, 0);
        let prepared_recall = match self.prepare_selected_memory(user_text, ui) {
            Ok(prepared) => prepared,
            Err(error) => {
                ui.diagnostic(&format!("Selected memory recall unavailable: {error:#}"));
                None
            }
        };
        // Recall attaches only facts not already shown on this branch; earlier ones stay in the
        // history of the message they were attached to, so nothing is re-appended.
        if let Some(saved) = prepared_recall
            .as_ref()
            .and_then(|prepared| prepared.context.as_deref())
        {
            context.push_str("\n\n");
            context.push_str(saved);
        }
        if let Some(memory) = &self.memory_runtime {
            let authority = memory_authority(&self.session, 0);
            memory.bind(authority.clone());
            match memory.instructions(&authority) {
                Ok(Some(saved)) if !saved.trim().is_empty() => context.push_str(&format!(
                    "\n\n<selected-memory trust=\"untrusted\">\n{saved}\n</selected-memory>"
                )),
                Ok(_) => {}
                Err(error) => ui.diagnostic(&format!(
                    "Selected memory context is unavailable: {error:#}"
                )),
            }
        }
        if !self.hermetic && visual::useful(user_text) {
            context.push_str("\n\n");
            context.push_str(visual::INITIAL_GUIDANCE);
        }
        if let Some(memory) = &self.memory {
            // The lookup is a search query, not the prompt: a pasted file or log must not fail the turn.
            let query = prompt::truncate_chars(user_text, RECALL_QUERY_CHARS);
            match memory.context_lookup(
                &query,
                self.config.memory.context_limit,
                efficiency::byte_limit(self.config.memory.context_max_bytes).min(
                    if self.config.efficiency.enabled {
                        efficiency::byte_limit(self.config.efficiency.ambient_context_bytes)
                    } else {
                        usize::MAX
                    },
                ),
            ) {
                Ok(recalled) => context.push_str(&recalled),
                Err(error) => {
                    ui.diagnostic(&format!("Provenance memory lookup unavailable: {error:#}"))
                }
            }
        }
        if self.config.codebase.ambient_context
            && let Some(saved) =
                self.codebase_context(&prompt::truncate_chars(user_text, RECALL_QUERY_CHARS), ui)
        {
            if !context.is_empty() {
                context.push_str("\n\n");
            }
            context.push_str(&saved);
        }
        if let Some(hints) = self.repair_context(user_text, ui) {
            if !context.is_empty() {
                context.push_str("\n\n");
            }
            context.push_str(&hints);
        }
        if !self.hermetic
            && self.config.assistance.repair_hints
            && let Some(store) = self.store
        {
            let repairs = match store
                .recent_executable_repairs(&self.workspace.root().to_string_lossy(), 16)
            {
                Ok(repairs) => repairs,
                Err(error) => {
                    ui.diagnostic(&format!("executable repair lookup unavailable: {error:#}"));
                    Vec::new()
                }
            };
            let terms: Vec<String> = user_text
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .filter(|term| term.len() >= 4)
                .take(32)
                .map(str::to_lowercase)
                .collect();
            let related: Vec<_> = repairs
                .into_iter()
                .filter(|(_, record)| {
                    let searchable = format!(
                        "{} {} {}",
                        record["before"]["objective"],
                        record["before"]["command"],
                        record["changes"]
                    )
                    .to_lowercase();
                    terms.iter().any(|term| searchable.contains(term))
                })
                .take(3)
                .collect();
            let mut hash_budget = 2 * 1024 * 1024;
            let matching: Vec<i64> = related
                .iter()
                .filter_map(|(trajectory, payload)| {
                    let record: repair_evidence::RepairRecord =
                        serde_json::from_value(payload.clone()).ok()?;
                    record
                        .matches_current(&self.workspace, &mut hash_budget)
                        .then_some(*trajectory)
                })
                .collect();
            if let Some(repairs) = repair_evidence::retrieval_text(&related, 4096) {
                if !context.is_empty() {
                    context.push_str("\n\n");
                }
                context.push_str(&repairs);
                context.push_str(&format!("\nExact failed-state file hashes currently match source trajectories {matching:?}. Other records are analogous or unchecked, not directly applicable. Re-run the executable check; file hashes do not verify the environment."));
            }
        }
        if !context.is_empty() {
            message["dial_recall"] = json!(context);
        }
        message["dial_context_request"] = json!(true);
        self.session.append(message)?;
        // A plan approved between turns (e.g. /plan-review) must reach this turn's first request.
        if let Some(plan) = self.pending_plan.take() {
            self.push(json!({"role":"user","content":format!("Approved plan:\n{plan}\nProceed with implementation.")}))?;
        }
        if let Some(prepared) = prepared_recall {
            prepared.commit(&recall_authority);
        }
        if let Some(memory) = &self.memory_runtime {
            memory.bind(memory_authority(&self.session, 0));
        }
        let mut state = self.session.control_state();
        state["current_instruction_entry_id"] = json!(self.session.branch_leaf());
        if state["original_goal"].is_null() {
            state["original_goal"] = json!(prompt::truncate_chars(user_text, 4096));
        }
        state["turn_outcome"] = Value::Null;
        state["current_instruction"] = json!(prompt::truncate_chars(user_text, 4096));
        state["steering_active"] = json!(false);
        self.session.set_control_state(state)?;
        if let Some(advice) = self.laya_advice(user_text, ui) {
            self.push(json!({"role":"user","content":format!("Laya retrieval advice (untrusted, advisory): {advice}")}))?;
        }
        let names = self.offered();
        let mut schemas = self.schemas(&names)?;
        tools::contracts::validate_history(&self.session.messages(), &schemas, &self.ext)?;
        self.last_trajectory = self.trajectory.as_ref().map(|(id, _, _)| *id);
        let max_steps = if !self.hermetic && self.config.harness.max_steps == 0 {
            0
        } else {
            self.version.harness.max_steps
        };
        if !self.hermetic
            && self.config.verification.enabled
            && self.config.verification.automatic_checks
            && matches!(
                self.task_policy.kind,
                crate::learn::task_policy::Kind::Editing
                    | crate::learn::task_policy::Kind::Debugging
            )
        {
            let baseline = self
                .config
                .verification
                .commands
                .iter()
                .filter(|check| {
                    self.config
                        .verification
                        .behavioral_checks
                        .contains(&check.name)
                })
                .take(8)
                .map(|check| verification::FocusedCheck {
                    name: check.name.clone(),
                    command: check.command.clone(),
                    timeout_secs: check.timeout_secs,
                })
                .collect::<Vec<_>>();
            if !baseline.is_empty() {
                ui.event(&crate::agent::ui_event::UiEvent::new(
                    crate::agent::ui_event::EventKind::Verification,
                    "Recording the configured behavioral baseline before edits.",
                ));
                let failures = self.run_checks(&baseline, ui).failures;
                let baseline_context = if failures.is_empty() {
                    "Configured behavioral baseline passed before editing. Preserve these behaviors and rerun relevant checks after the change.".to_owned()
                } else {
                    format!(
                        "Behavioral baseline before editing (these failures predate this turn's edits):\n{}\nReproduce and fix failures within the requested scope; report unrelated failures without broadening the task.",
                        failures.join("\n")
                    )
                };
                self.record(
                    0,
                    "behavioral_baseline",
                    json!({"checks":baseline.len(),"failures":failures.len()}),
                )?;
                self.push(json!({"role":"user","content":baseline_context}))?;
            }
        }
        self.prewalk_resume()?;
        self.plan_model_resume()?;
        self.switch_resume();
        let mut backend = self.backend.take().context("backend is already in use")?;
        // One finite budget: the initial run, verification repairs and stop-hook continuations all draw from it.
        let mut turn_steps = TurnSteps::new(max_steps);
        let mut result =
            self.run_loop(&mut backend, &names, &mut schemas, max_steps, user_text, ui);
        if let Ok(report) = &mut result {
            report.steps = turn_steps.charge(report.steps);
        }
        // A turn that ended before the next request still must not lose accepted steering.
        if let Err(error) = self.flush_accepted_live_steering(ui) {
            result = Err(error);
        }
        // Finalization is reentered after every stop-hook continuation. Repairs and hooks retain
        // their turn-wide bounds, and only completed candidates may run checks or resume work.
        const DEFAULT_REPAIR_ROUNDS: u64 = 3;
        const STOP_HOOK_ROUNDS: u32 = 3;
        let repair_limit = match self.config.verification.repair_attempts {
            0 => DEFAULT_REPAIR_ROUNDS,
            configured => u64::from(configured),
        };
        let mut repair_attempts = 0u64;
        let mut stop_rounds = 0u32;
        let plugins = self.hook_plugins();
        let mut candidate_summary = String::new();
        loop {
            if let Ok(report) = &result
                && report.outcome == TurnOutcome::Completed
                && !report.text.trim().is_empty()
            {
                candidate_summary.clone_from(&report.text);
            }
            if let Ok(report) = &mut result
                && report.outcome == TurnOutcome::Completed
                && (report.interrupted || self.interrupt.load(Ordering::SeqCst))
            {
                report.outcome = TurnOutcome::Interrupted;
                report.interrupted = true;
                report.text = unsettled_report(
                    &candidate_summary,
                    "Interrupted before finalization; completion was not accepted.",
                );
            }
            let already_verified = self.step_checks.clean();
            // A denied or failed edit leaves nothing to verify; checks then fail on unrelated code.
            if self.config.verification.enabled
                && self.config.verification.automatic_checks
                && !already_verified
                && let (Ok(report), Some(turn)) = (&result, checkpoint_turn)
                && report.outcome == TurnOutcome::Completed
                && !report.interrupted
                && !self.interrupt.load(Ordering::SeqCst)
                && (self.step_checks.mutations > 0
                    || self.step_checks.has_uncheckpointed_operations())
            {
                let mut checks = self.focused_checks(turn);
                self.step_checks
                    .include_failed_checks(self.workspace.root(), &mut checks);
                self.step_checks.record_checked();
                let mut run = self.run_checks(&checks, ui);
                self.step_checks.failed = !run.failures.is_empty();
                // Zero selects a bounded default; hooks cannot reset the repair allowance.
                let repair_outcome = (|| -> Result<()> {
                    while repair_attempts < repair_limit {
                        // Only confirmed code failures are repaired; infrastructure, permission, cancellation,
                        // timeout and unverified problems stay visible in the final diagnostic.
                        if run.actionable.is_empty() || self.interrupt.load(Ordering::SeqCst) {
                            break;
                        }
                        let Some(repair_steps) = turn_steps.remaining() else {
                            break;
                        };
                        self.metrics.repair_attempts += 1;
                        self.record(0, "repair_round", json!({"attempt":repair_attempts+1}))?;
                        self.push(json!({"role":"user","content":format!(
                                "Automatic focused verification failed (repair attempt {}). Fix the diagnostics, then rerun the checks:\n{}",
                                repair_attempts + 1, run.actionable.join("\n")
                            )}))?;
                        repair_attempts = repair_attempts.saturating_add(1);
                        let repaired = self.run_loop(
                            &mut backend,
                            &names,
                            &mut schemas,
                            repair_steps,
                            user_text,
                            ui,
                        );
                        self.flush_accepted_live_steering(ui)?;
                        let mut repaired = repaired?;
                        repaired.steps = turn_steps.charge(repaired.steps);
                        if repaired.outcome != TurnOutcome::Completed {
                            repaired.text = unsettled_report(&candidate_summary, &repaired.text);
                            result = Ok(repaired);
                            break;
                        }
                        if !repaired.text.trim().is_empty() {
                            candidate_summary.clone_from(&repaired.text);
                        }
                        result = Ok(repaired);
                        if self.step_checks.clean() {
                            run.actionable.clear();
                            run.failures.clear();
                        } else {
                            checks = self.focused_checks(turn);
                            self.step_checks
                                .include_failed_checks(self.workspace.root(), &mut checks);
                            self.step_checks.record_checked();
                            run = self.run_checks(&checks, ui);
                            self.step_checks.failed = !run.failures.is_empty();
                        }
                    }
                    Ok(())
                })();
                // Errors from the repair rounds must still restore the backend and finish the turn.
                if let Err(error) = repair_outcome {
                    result = Err(error);
                }
                if self.interrupt.load(Ordering::SeqCst)
                    && result
                        .as_ref()
                        .is_ok_and(|report| report.outcome == TurnOutcome::Completed)
                {
                    let steps = result.as_ref().map_or(0, |report| report.steps);
                    result = Ok(TurnReport {
                        text: unsettled_report(
                            &candidate_summary,
                            "Interrupted during focused verification; completion was not accepted.",
                        ),
                        outcome: TurnOutcome::Interrupted,
                        steps,
                        interrupted: true,
                        trajectory: None,
                    });
                } else if !run.failures.is_empty()
                    && result.as_ref().is_ok_and(|report| {
                        matches!(
                            report.outcome,
                            TurnOutcome::Completed | TurnOutcome::Incomplete
                        )
                    })
                {
                    let problems = run.failures.join("; ");
                    let diagnostic = if repair_attempts > 0 {
                        format!("Focused verification still fails after bounded repair: {problems}")
                    } else if !run.actionable.is_empty() {
                        format!(
                            "Focused verification failed and the turn's step budget ran out before an automatic repair: {problems}"
                        )
                    } else {
                        format!(
                            "Focused verification failed without a repairable code diagnosis, so no automatic repair ran. Inspect the environment or permissions and rerun the check: {problems}"
                        )
                    };
                    ui.notice(&diagnostic);
                    let steps = result.as_ref().map_or(0, |report| report.steps);
                    result = Ok(TurnReport {
                        text: unsettled_report(&candidate_summary, &diagnostic),
                        outcome: TurnOutcome::Failed,
                        steps,
                        interrupted: false,
                        trajectory: None,
                    });
                }
            }
            // A blocking stop hook feeds its reason back and resumes through verification above.
            let payload = match &result {
                Ok(report) => {
                    json!({"session_id":self.session.id(),"outcome":format!("{:?}", report.outcome),"answer":prompt::truncate_chars(&report.text, 4000),"stop_hook_active":stop_rounds > 0})
                }
                Err(_) => break,
            };
            if !hooks::wanted(self.tool_hooks(), plugins.as_deref(), "stop") {
                break;
            }
            let configured_hooks = self.tool_hooks();
            let root = self.workspace.root();
            let interrupt = self.interrupt.as_ref();
            let (verdict, notices) = hooks::with_heartbeat(ui, || {
                let outcome = hooks::fire_with(
                    configured_hooks,
                    plugins.as_deref(),
                    root,
                    "stop",
                    None,
                    payload,
                    interrupt,
                );
                (outcome.verdict, outcome.notices)
            });
            notices.iter().for_each(|n| {
                ui.event(&crate::agent::ui_event::UiEvent::new(
                    crate::agent::ui_event::EventKind::Hook,
                    n,
                ))
            });
            if self.interrupt.load(Ordering::SeqCst)
                && let Ok(report) = &mut result
                && report.outcome == TurnOutcome::Completed
            {
                report.outcome = TurnOutcome::Interrupted;
                report.interrupted = true;
                report.text = unsettled_report(
                    &candidate_summary,
                    "Interrupted during the stop hook; completion was not accepted.",
                );
                break;
            }
            let reason = match verdict {
                hooks::Verdict::Block { reason } => reason,
                hooks::Verdict::Continue { .. } => break,
            };
            let may_continue = matches!(&result, Ok(report) if report.outcome == TurnOutcome::Completed && !report.interrupted)
                && !self.interrupt.load(Ordering::SeqCst);
            if !may_continue {
                break;
            }
            if stop_rounds >= STOP_HOOK_ROUNDS {
                let diagnostic = format!(
                    "Blocked: stop hook still blocks completion after {STOP_HOOK_ROUNDS} rounds. {reason}"
                );
                ui.notice(&diagnostic);
                if let Ok(report) = &mut result {
                    report.outcome = TurnOutcome::Blocked;
                    report.text = unsettled_report(&candidate_summary, &diagnostic);
                }
                break;
            }
            let Some(resume_steps) = turn_steps.remaining() else {
                let diagnostic = format!(
                    "Stopped: this turn used its {max_steps}-step limit before the stop hook's requested change could run. {reason}"
                );
                ui.notice(&diagnostic);
                if let Ok(report) = &mut result {
                    report.outcome = TurnOutcome::Incomplete;
                    report.text = unsettled_report(&candidate_summary, &diagnostic);
                }
                break;
            };
            stop_rounds += 1;
            let mut resumed = (|| -> Result<TurnReport> {
                self.record(
                    0,
                    "stop_hook_block",
                    json!({"reason":reason,"round":stop_rounds}),
                )?;
                self.push(json!({"role":"user","content":format!("A stop hook blocked completion. Address this before finishing:\n{reason}")}))?;
                let report = self.run_loop(
                    &mut backend,
                    &names,
                    &mut schemas,
                    resume_steps,
                    user_text,
                    ui,
                );
                self.flush_accepted_live_steering(ui)?;
                report
            })();
            if let Ok(report) = &mut resumed {
                report.steps = turn_steps.charge(report.steps);
                if report.outcome != TurnOutcome::Completed {
                    report.text = unsettled_report(&candidate_summary, &report.text);
                }
            }
            result = resumed;
        }
        // Publish only after all hook continuations settle, not for an intermediate candidate.
        if !self.hermetic
            && !(self.config.verification.enabled && self.config.verification.automatic_checks)
            && result
                .as_ref()
                .is_ok_and(|report| report.outcome == TurnOutcome::Completed)
        {
            self.publish_no_automatic_check(ui);
        }
        self.ext.fire_lifecycle(&LifecycleEvent {
            event: "turn_finished".into(),
            session_id: Some(self.session.id().to_owned()),
            turn_id: checkpoint_turn.map(|turn| turn.to_string()),
            tool_name: None,
            is_error: Some(!result.as_ref().is_ok_and(|report| {
                report.outcome == TurnOutcome::Completed && !report.interrupted
            })),
        });
        self.backend = Some(backend);
        self.session.repair_incomplete_tools()?;
        if let Ok(report) = &result {
            let mut state = self.session.control_state();
            state["turn_outcome"] = json!(report.outcome);
            self.session.set_control_state(state)?;
        }
        if let Some(plan) = self.pending_plan.take() {
            self.push(json!({"role":"user","content":format!("Approved plan:\n{plan}\nProceed with implementation.")}))?;
        }
        if result
            .as_ref()
            .is_ok_and(|report| report.outcome == TurnOutcome::Completed)
        {
            self.finish_experience(ui);
            self.save_investigation_findings(ui);
            // The default architecture model is local MLX. With a cloud main model no compute is held for
            // the turn, so synthesis is skipped rather than making every turn wait for training.
            let arch_local = self.config.architecture_config().runs_dial_mlx();
            if !self.hermetic
                && self.config.codebase.synthesize_architecture
                && (!arch_local || self.config.uses_local_compute())
                && self.config.architecture_config().context_window >= 4096
                && !self.interrupt.load(Ordering::SeqCst)
                && ui.maintenance_ready()
                && let Err(error) = self.synthesize_architecture(false, ui)
            {
                ui.diagnostic(&format!("Architecture update deferred: {error:#}"));
            }
        }
        if let (Ok(report), Some(predecessor)) = (&result, capture_start) {
            self.schedule_memory_capture(report, predecessor.as_deref(), ui);
        }
        let (_, usage) = self.usage_scope.snapshot();
        let efficiency = json!({
            "version":1,
            "task_kind":self.task_policy.kind,
            "enabled":self.config.efficiency.enabled,
            "completed":result.as_ref().is_ok_and(|r| r.outcome == TurnOutcome::Completed),
            "outcome":result.as_ref().map(|r| r.outcome).ok(),
            "duration_ms":self.workflow_started.elapsed().as_millis(),
            "usage":usage,
            "projection_bytes":self.efficiency_metrics,
            "note":"Usage is provider-reported and may be partial; cache semantics vary by protocol. Projection bytes are not billed tokens. Completion is not independent correctness proof."
        });
        let mut control = self.session.control_state();
        control["efficiency"] = efficiency.clone();
        self.session.set_control_state(control)?;
        ui.efficiency(&efficiency);
        if let Some((id, _, _)) = self.trajectory.take() {
            if let Some(store) = self.store {
                match &result {
                    Ok(report) => {
                        store.finish_trajectory(id, Some(&report.text), None)?;
                        if report.outcome == TurnOutcome::Completed
                            && self.domain_override.is_none()
                            && !self.hermetic
                            && !self.behavioral_checks.is_empty()
                            && self.behavioral_checks.iter().all(|passed| *passed)
                            && self.step_checks.clean()
                            && let Err(error) = store.outcome(&crate::config::types::Outcome {
                                trajectory: id,
                                score: 100.0,
                                source: "checks:behavioral".into(),
                                reference: Some(format!(
                                    "{} behavioral check(s) passed after the last edit",
                                    self.behavioral_checks.len()
                                )),
                            })
                        {
                            ui.diagnostic(&format!("turn outcome was not recorded: {error:#}"));
                        }
                        if report.outcome == TurnOutcome::Completed
                            && let Err(error) = self.persist_turn(store, id)
                        {
                            ui.diagnostic(&format!(
                                "training observation was not saved: {error:#}"
                            ));
                        }
                    }
                    Err(error) => store.finish_trajectory(id, None, Some(&format!("{error:#}")))?,
                }
            }
            if let Ok(report) = &result {
                return Ok(TurnReport {
                    trajectory: Some(id),
                    ..result_clone(report)
                });
            }
        }
        result
    }

    pub(super) fn save_investigation_findings(&self, ui: &mut dyn Ui) {
        if !self.config.improve.enabled {
            return;
        }
        let Some(cache) = self.codebase.clone() else {
            return;
        };
        let findings: Vec<_> = self
            .todo
            .tasks()
            .iter()
            .filter(|task| task.investigation && task.status == todo::Status::Completed)
            .filter_map(|task| {
                Some((
                    task.content.clone(),
                    task.finding.clone()?,
                    task.evidence.clone(),
                ))
            })
            .rev()
            .take(3)
            .collect();
        if findings.is_empty() {
            return;
        }
        let session = self.session.id().to_owned();
        thread::scope(|scope| {
            let (tx, rx) = mpsc::sync_channel(1);
            scope.spawn(move || {
                let saved = findings
                    .iter()
                    .filter(|(question, finding, refs)| {
                        cache
                            .remember_investigation(question, finding, refs, &session)
                            .unwrap_or(false)
                    })
                    .count();
                let _ = tx.send(saved);
            });
            loop {
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(count) => {
                        if count > 0 {
                            ui.diagnostic(&format!("Saved {count} source-linked investigation findings for future sessions."));
                        }
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => ui.heartbeat(),
                    Err(_) => break,
                }
            }
        });
    }

    /// Retain observations, not self-assigned rewards. A model-selected shell command can spoof a
    /// successful test invocation; only a separate verifier or user verdict supplies training labels.
    pub(super) fn persist_turn(&self, store: &Store, trajectory: i64) -> Result<()> {
        store.save_messages(trajectory, &self.training_capture.payload())
    }
}

/// Keep the work visible without presenting a rejected candidate as an accepted completion.
fn unsettled_report(summary: &str, diagnostic: &str) -> String {
    if summary.trim().is_empty() || summary == diagnostic {
        diagnostic.to_owned()
    } else {
        format!("{diagnostic}\n\nCandidate work summary (completion not accepted):\n{summary}")
    }
}

/// One finite `max_steps` per user turn. The initial run, verification repairs and stop-hook
/// continuations all charge it; zero is the operator's unlimited choice and never runs out.
#[derive(Clone, Copy, Debug)]
struct TurnSteps {
    max: u32,
    spent: u32,
}

impl TurnSteps {
    fn new(max: u32) -> Self {
        Self { max, spent: 0 }
    }

    /// Steps the next round may spend (0 = unlimited); `None` once the shared budget is spent.
    fn remaining(self) -> Option<u32> {
        match self.max {
            0 => Some(0),
            max => max.checked_sub(self.spent).filter(|left| *left > 0),
        }
    }

    /// Charge one run's own step count and return the turn's cumulative count for its report.
    fn charge(&mut self, run_steps: u32) -> u32 {
        self.spent = self.spent.saturating_add(run_steps);
        self.spent
    }
}

#[cfg(test)]
mod turn_steps_tests;

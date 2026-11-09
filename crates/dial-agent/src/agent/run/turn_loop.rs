//! The turn entry points and main step loop.

use crate::agent::run::turn_draft::TurnOrigin;
use crate::agent::*;

mod admission;
mod context;
mod continuation;
mod settlement;

/// The hook-admitted text and its optional model draft have distinct provenance.
struct TurnPrompt<'input> {
    typed: std::borrow::Cow<'input, str>,
    drafted: Option<String>,
}

impl TurnPrompt<'_> {
    /// The typed (or hook-rewritten) text: the turn's authoritative instruction. A draft, when
    /// present, is only attached to the admitted message as an interpretation.
    fn typed(&self) -> &str {
        &self.typed
    }
}

/// Facts prepared before borrowing the backend for the whole execution phase.
struct AdmittedTurn<'input> {
    prompt: TurnPrompt<'input>,
    names: Vec<String>,
    schemas: Vec<Value>,
    max_steps: u32,
    checkpoint_turn: Option<u64>,
}

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
        self.turn_with_state(user_text, ui, false, None, TurnOrigin::User)
    }
    /// ACP has already resolved explicit content blocks. Do not interpret resource bodies as
    /// terminal @file syntax: a referenced document must not attach other local files implicitly.
    pub fn turn_prepared_preserving_interrupt(
        &mut self,
        user_text: &str,
        message: Value,
        ui: &mut dyn Ui,
    ) -> Result<TurnReport> {
        ensure!(
            message["role"] == "user" && message["content"].as_str() == Some(user_text),
            "invalid prepared user message"
        );
        self.turn_with_state(user_text, ui, false, Some(message), TurnOrigin::User)
    }
    pub(in crate::agent) fn turn_with_state(
        &mut self,
        user_text: &str,
        ui: &mut dyn Ui,
        reset_interrupt: bool,
        prepared: Option<Value>,
        origin: TurnOrigin,
    ) -> Result<TurnReport> {
        // A previous turn may have stopped on a full disk. Retry only its journal writer,
        // before admitting new work; never replay the failed model request or tool effects.
        if let Some(journal) = self.session.runtime().stream_writer() {
            journal.retry_failed_write()?;
        }
        self.training_capture.clear();
        self.coordination_turn_started(origin);
        self.monitor_turn_started(origin);
        // The turn owns the assertion, not the session: idle prompts must allow sleep.
        // Hermetic evaluation must not change host power state.
        let _idle_sleep = if self.hermetic {
            None
        } else {
            match crate::platform::power::IdleSleepGuard::acquire() {
                Ok(guard) => Some(guard),
                Err(error) => {
                    ui.notice(&format!(
                        "Could not prevent automatic sleep during this turn: {error:#}"
                    ));
                    None
                }
            }
        };
        // Hermetic evaluation must not spend money on background replays.
        let stops = if self.hermetic {
            Vec::new()
        } else {
            model::cache_warm::CacheWarm::sync(&mut self.cache_warm, &self.auth_config)?
        };
        for stop in stops {
            ui.notice(&format!(
                "Cache warming stopped: {}",
                model::cache_warm::describe(stop)
            ));
        }
        // Dropped on every exit path, so the warmer learns when the run ends.
        let _cache_warm_run = self.cache_warm.as_ref().map(|warm| warm.run_guard());
        // Reset current-turn evidence before any admission step can fail. Historical receipts
        // remain separately labelled and must still pass freshness checks before reuse.
        self.drain_background_check(ui, true);
        self.step_checks = StepChecks::default();
        self.verifications.clear();
        self.behavioral_checks.clear();
        let mut control = self.session.control_state();
        let previous_receipts = control["reliability"]["verification_receipts"]
            .as_array()
            .cloned();
        control["completion_evidence"] = Value::Null;
        control["turn_summary"] = Value::Null;
        control["reliability"] = json!({
            "stats":crate::reliability::Stats::default(),
            "verification":"no checks recorded this turn",
            "interface":"standard",
            "automatic_passing_checks":0,
            "automatic_failing_checks":0,
            "edit_loop":self.step_checks.status()
        });
        if let Some(receipts) =
            previous_receipts.filter(|_| self.config.experimental.verification_receipts)
        {
            control["reliability"]["verification_receipts"] = json!(receipts);
        }
        self.session.set_control_state(control.clone())?;
        if !self.hermetic {
            ui.request_policy(&control);
        }
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
        let mut observed = crate::agent::ui::state_ui::StateUi::with_registry(
            ui,
            self.session.runtime(),
            None,
            self.ext.clone(),
        );
        self.live_tasks.bind(self.session.runtime());
        let turn_clock = Instant::now();
        self.coordination_phase("working", Some(user_text));
        let result = self.turn_inner(user_text, &mut observed, reset_interrupt, prepared, origin);
        self.coordination_phase("idle", None);
        if let Err(error) = self.flush_local_evidence() {
            observed.diagnostic(&format!("Local model evidence not saved: {error:#}"));
        }
        if let Ok(report) = &result {
            let control = self.session.control_state();
            if let Some(notice) = crate::agent::verify::completion::outstanding_notice(
                &self.todo,
                &control["working_state"],
                report.outcome,
            ) {
                observed.notice(&notice);
            }
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
        // Background shell work may have written after its command returned.
        self.record_shell_changes(&mut observed);
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
                let branch =
                    crate::tools::knowledge::history::current_branch(self.workspace.root());
                history.record(
                    self.session.id(),
                    turn,
                    &crate::agent::window::prompt::truncate_chars(user_text, 200),
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
        // A failed turn keeps its cause in the journal; the transcript alone loses it on resume.
        let (error, error_class) = match &result {
            Err(error) => (
                Value::from(failure_cause(error)),
                Value::from(crate::llm::errors::classify(error).label()),
            ),
            Ok(_) => (Value::Null, Value::Null),
        };
        crate::telemetry::turn(
            self.config,
            self.hermetic,
            self.session.id(),
            &outcome,
            u64::from(steps),
            turn_clock.elapsed(),
        );
        let edit = self.step_checks.status();
        let summary = json!({"outcome":outcome,"steps":steps,"error":error,"error_class":error_class,"checkpoint_turn":self.step_checks.turn,"changed_files":changed_files,"open_tasks":self.todo.tasks().iter().filter(|task| task.status.is_open()).count(),"recorded_checks":state["reliability"]["verification_results"],"checks_current":self.step_checks.checks_current(),"evidence":state["completion_evidence"],"uncheckpointed_shell_commands":edit["uncheckpointed_shell_commands"],"uncheckpointed_extension_calls":edit["uncheckpointed_extension_calls"],"review":"/review-file","undo":"/undo"});
        observed.completion(&summary);
        let mut control = self.session.control_state();
        control["turn_summary"] = summary;
        if let Err(error) = self.session.set_control_state(control) {
            observed.diagnostic(&format!("Turn summary not saved: {error:#}"));
        }
        observed.finish(!result.as_ref().is_ok_and(|turn| !turn.interrupted));
        let result = match (result, self.session.flush_runtime()) {
            (Err(error), Err(journal)) => {
                Err(error.context(format!("execution journal also failed: {journal:#}")))
            }
            (result, Ok(())) => result,
            (Ok(_), Err(journal)) => Err(journal),
        };
        if let Ok(report) = &result {
            self.retain_training_turn(report, user_text, &mut observed);
        }
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
        self.monitor_turn_settled(origin, &result);
        result
    }
    pub(in crate::agent) fn turn_inner(
        &mut self,
        user_text: &str,
        ui: &mut dyn Ui,
        reset_interrupt: bool,
        prepared: Option<Value>,
        origin: TurnOrigin,
    ) -> Result<TurnReport> {
        let storage_before = self.session.io_metrics();
        let capture_start = (self.config.memory.autolearn
            && self.config.memory.autolearn_auto_continue)
            .then(|| self.session.branch_leaf().map(str::to_owned));
        let admitted = self.admit_turn(user_text, ui, reset_interrupt, prepared, origin)?;
        let checkpoint_turn = admitted.checkpoint_turn;
        let mut backend = self.backend.take().context("backend is already in use")?;
        // The execution phase borrows the backend: all its failures become a result, so
        // ownership is restored before any fallible final persistence or learning work.
        let result = self.run_admitted_turn(admitted, &mut backend, ui);
        self.backend = Some(backend);
        self.settle_turn(result, checkpoint_turn, capture_start, storage_before, ui)
    }

    /// Persists at most three findings. The write runs on a scoped thread only to keep the UI
    /// heartbeat alive while the cache is busy; the call itself waits for it.
    pub(in crate::agent) fn save_investigation_findings(&self, ui: &mut dyn Ui) {
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
            .filter(|task| {
                task.investigation && task.status == crate::agent::workflow::todo::Status::Completed
            })
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
    pub(in crate::agent) fn persist_turn(&self, store: &Store, trajectory: i64) -> Result<()> {
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
    attempts: u32,
}

impl TurnSteps {
    fn new(max: u32) -> Self {
        Self {
            max,
            spent: 0,
            attempts: 0,
        }
    }

    /// Steps the next round may spend (0 = unlimited); `None` once the shared budget is spent.
    fn remaining(self) -> Option<u32> {
        match self.max {
            0 => Some(0),
            max => max.checked_sub(self.spent).filter(|left| *left > 0),
        }
    }

    /// Budget consumed work separately while preserving raw attempts in public reports.
    fn charge(&mut self, run_attempts: u32, consumed: u32) -> u32 {
        self.spent = self.spent.saturating_add(consumed);
        self.attempts = self.attempts.saturating_add(run_attempts);
        self.attempts
    }
}

/// Characters of a failed turn's cause kept in its saved summary.
const FAILURE_CAUSE_CHARS: usize = 600;

/// The diagnostic chain of a failed turn, without the generic recovery advice every provider
/// failure carries, bounded so the saved summary stays small.
fn failure_cause(error: &anyhow::Error) -> String {
    let chain: Vec<String> = error
        .chain()
        .map(ToString::to_string)
        .filter(|cause| cause != crate::agent::run::failure_guidance::PROVIDER_RECOVERY)
        .collect();
    crate::agent::window::prompt::truncate_chars(&chain.join(": "), FAILURE_CAUSE_CHARS)
}

#[cfg(test)]
mod repair_progress_tests;
#[cfg(test)]
mod turn_steps_tests;

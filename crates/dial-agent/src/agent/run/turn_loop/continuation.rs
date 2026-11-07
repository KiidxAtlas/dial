//! Execution, verification repairs and stop hooks share one turn state and budget.

use super::{AdmittedTurn, TurnSteps, unsettled_report};
use crate::agent::run::{engine::Engine, turn::TurnState};
use crate::agent::*;
mod repair_progress;
use repair_progress::Fingerprint;

const DEFAULT_REPAIR_ROUNDS: u64 = 3;
const STOP_HOOK_ROUNDS: u32 = 3;

struct TurnContinuation<'turn> {
    schemas: Vec<Value>,
    steps: TurnSteps,
    engine: Engine,
    state: Option<TurnState<'turn>>,
    result: Result<TurnReport>,
    repair_limit: u64,
    repair_attempts: u64,
    stop_rounds: u32,
    candidate_summary: String,
}

enum HookProgress {
    Resumed,
    Settled,
}

impl Agent<'_> {
    pub(super) fn run_admitted_turn(
        &mut self,
        admitted: AdmittedTurn<'_>,
        backend: &mut Backend,
        ui: &mut dyn Ui,
    ) -> Result<TurnReport> {
        let AdmittedTurn {
            prompt,
            names,
            mut schemas,
            max_steps,
            checkpoint_turn,
        } = admitted;
        let user_text = prompt.typed();
        // One finite budget: the initial run, verification repairs and stop-hook continuations all draw from it.
        let mut turn_steps = TurnSteps::new(max_steps);
        // One turn state and engine for the whole user turn: repair and stop-hook rounds resume
        // it instead of judging effort, loading portable rules and learning failures again.
        let mut engine = crate::agent::run::engine::Engine::default();
        if self.modes.launch.unattended {
            engine.set_evidence_stall_limit(self.config.efficiency.evidence_stall_rounds);
        }
        let mut turn_state = None;
        let mut result = match self.begin_turn(backend, &names, max_steps, user_text, ui) {
            Ok(state) => {
                let state = turn_state.insert(state);
                self.run_round(state, &mut engine, backend, &mut schemas, max_steps, ui)
            }
            Err(error) => Err(error),
        }
        .map(|(mut report, consumed)| {
            report.steps = turn_steps.charge(report.steps, consumed);
            report
        });
        // A turn that ended before the next request still must not lose accepted steering.
        if let Err(error) = self.flush_accepted_live_steering(ui) {
            result = Err(error);
        }
        // Steering deferred for the drafter after the last request is journaled now. A failed
        // or interrupted turn keeps it queued; the next turn drafts it before its own message.
        if result.is_ok()
            && !self.interrupt.load(Ordering::SeqCst)
            && let Err(error) = self.draft_deferred_steering(backend, ui)
        {
            result = Err(error);
        }
        // Finalization is reentered after every stop-hook continuation. Repairs and hooks retain
        // their turn-wide bounds, and only completed candidates may run checks or resume work.
        let repair_limit = match self.config.verification.repair_attempts {
            0 => DEFAULT_REPAIR_ROUNDS,
            configured => u64::from(configured),
        };
        let mut run = TurnContinuation {
            schemas,
            steps: turn_steps,
            engine,
            state: turn_state,
            result,
            repair_limit,
            repair_attempts: 0,
            stop_rounds: 0,
            candidate_summary: String::new(),
        };
        loop {
            if let Ok(report) = &run.result
                && report.outcome == TurnOutcome::Completed
                && !report.text.trim().is_empty()
            {
                run.candidate_summary.clone_from(&report.text);
            }
            if let Ok(report) = &mut run.result
                && report.outcome == TurnOutcome::Completed
                && (report.interrupted || self.interrupt.load(Ordering::SeqCst))
            {
                report.outcome = TurnOutcome::Interrupted;
                report.interrupted = true;
                report.text = unsettled_report(
                    &run.candidate_summary,
                    "Interrupted before finalization; completion was not accepted.",
                );
            }
            self.verify_turn_candidate(&mut run, backend, checkpoint_turn, ui);
            if let HookProgress::Settled = self.resume_after_stop_hook(&mut run, backend, ui) {
                break;
            }
        }
        run.result
    }

    fn verify_turn_candidate(
        &mut self,
        run: &mut TurnContinuation<'_>,
        backend: &mut Backend,
        checkpoint_turn: Option<u64>,
        ui: &mut dyn Ui,
    ) {
        self.refresh_check_inputs();
        let already_verified = self.step_checks.clean();
        // A denied or failed edit leaves nothing to verify; checks then fail on unrelated code.
        if self.config.verification.enabled
            && self.config.verification.automatic_checks
            && !already_verified
            && let (Ok(report), Some(turn)) = (&run.result, checkpoint_turn)
            && report.outcome == TurnOutcome::Completed
            && !report.interrupted
            && !self.interrupt.load(Ordering::SeqCst)
            && (self.step_checks.mutations > 0 || self.step_checks.has_uncheckpointed_operations())
        {
            let mut checks = self.focused_checks(turn);
            self.step_checks
                .include_failed_checks(self.workspace.root(), &mut checks);
            self.step_checks.record_checked();
            let mut check_run = self.run_checks(&checks, ui);
            // Zero selects a bounded default; hooks cannot reset the repair allowance.
            let mut stalled = false;
            let repair_outcome = (|| -> Result<()> {
                while run.repair_attempts < run.repair_limit {
                    // Only confirmed code failures are repaired; infrastructure, permission, cancellation,
                    // timeout and unverified problems stay visible in the final diagnostic.
                    if check_run.actionable.is_empty() || self.interrupt.load(Ordering::SeqCst) {
                        break;
                    }
                    let Some(repair_steps) = run.steps.remaining() else {
                        break;
                    };
                    let repair_checks = checks.clone();
                    let round_start = Fingerprint::new(
                        &check_run.diagnoses,
                        self.step_checks.revision(),
                        self.check_input_snapshots(&checks),
                    );
                    self.metrics.repair_attempts += 1;
                    self.record(0, "repair_round", json!({"attempt":run.repair_attempts+1}))?;
                    self.push(json!({"role":"user","content":format!(
                            "Automatic focused verification failed (repair attempt {}). Fix the diagnostics, then rerun the checks:\n{}",
                            run.repair_attempts + 1, check_run.actionable.join("\n")
                        )}))?;
                    run.repair_attempts = run.repair_attempts.saturating_add(1);
                    let state = run
                        .state
                        .as_mut()
                        .context("a repair round needs the turn it resumes")?;
                    let repaired = self.resume_round(
                        state,
                        &mut run.engine,
                        backend,
                        &mut run.schemas,
                        repair_steps,
                        ui,
                    );
                    self.flush_accepted_live_steering(ui)?;
                    let (mut repaired, consumed) = repaired?;
                    repaired.steps = run.steps.charge(repaired.steps, consumed);
                    // Failed check obligations can make an otherwise finished repair incomplete.
                    // Still compare progress and verify changed inputs; this never promotes an
                    // incomplete candidate or retries blocked/interrupted work.
                    if !matches!(
                        repaired.outcome,
                        TurnOutcome::Completed | TurnOutcome::Incomplete
                    ) {
                        repaired.text = unsettled_report(&run.candidate_summary, &repaired.text);
                        run.result = Ok(repaired);
                        break;
                    }
                    if !repaired.text.trim().is_empty() {
                        run.candidate_summary.clone_from(&repaired.text);
                    }
                    run.result = Ok(repaired);
                    // Same confirmed diagnosis and no progress: another rerun would repeat it. The
                    // effect epoch and declared inputs are read before the rerun, so its probes do not count.
                    if self.step_checks.clean() {
                        check_run.actionable.clear();
                        check_run.failures.clear();
                    } else {
                        checks = self.focused_checks(turn);
                        self.step_checks
                            .include_failed_checks(self.workspace.root(), &mut checks);
                        self.step_checks.record_checked();
                        let revision = self.step_checks.revision();
                        let inputs = self.check_input_snapshots(&checks);
                        // Do not rerun a known failure merely because the model changed a todo or
                        // claimed completion. Complete input witnesses also catch reverted edits.
                        let unchanged = checks == repair_checks
                            && round_start.repeats(&Fingerprint::new(
                                &check_run.diagnoses,
                                revision,
                                inputs.clone(),
                            ));
                        if !unchanged {
                            check_run = self.run_checks(&checks, ui);
                        }
                        if unchanged
                            || (!check_run.actionable.is_empty()
                                && round_start.repeats(&Fingerprint::new(
                                    &check_run.diagnoses,
                                    revision,
                                    inputs,
                                )))
                        {
                            self.record(
                                0,
                                "repair_no_progress",
                                json!({"diagnosis":round_start.key(),"attempt":run.repair_attempts}),
                            )?;
                            stalled = true;
                            break;
                        }
                    }
                }
                Ok(())
            })();
            // Errors from the repair rounds must still restore the backend and finish the turn.
            if let Err(error) = repair_outcome {
                run.result = Err(error);
            }
            if self.interrupt.load(Ordering::SeqCst)
                && run
                    .result
                    .as_ref()
                    .is_ok_and(|report| report.outcome == TurnOutcome::Completed)
            {
                let steps = run.result.as_ref().map_or(0, |report| report.steps);
                run.result = Ok(TurnReport {
                    text: unsettled_report(
                        &run.candidate_summary,
                        "Interrupted during focused verification; completion was not accepted.",
                    ),
                    outcome: TurnOutcome::Interrupted,
                    steps,
                    interrupted: true,
                    trajectory: None,
                });
            } else if !check_run.failures.is_empty()
                && run.result.as_ref().is_ok_and(|report| {
                    matches!(
                        report.outcome,
                        TurnOutcome::Completed | TurnOutcome::Incomplete
                    )
                })
            {
                let problems = check_run.failures.join("; ");
                let diagnostic = if stalled {
                    format!(
                        "Focused verification still has the same confirmed diagnosis and its inputs did not change, so automatic repair stopped: {problems}"
                    )
                } else if run.repair_attempts > 0 {
                    format!("Focused verification still fails after bounded repair: {problems}")
                } else if !check_run.actionable.is_empty() {
                    format!(
                        "Focused verification failed and the turn's step budget ran out before an automatic repair: {problems}"
                    )
                } else {
                    format!(
                        "Focused verification has unchanged baseline failures or lacks a repairable new code diagnosis, so no automatic repair ran. Inspect the disclosed causes and rerun relevant checks: {problems}"
                    )
                };
                ui.notice(&diagnostic);
                let steps = run.result.as_ref().map_or(0, |report| report.steps);
                run.result = Ok(TurnReport {
                    text: unsettled_report(&run.candidate_summary, &diagnostic),
                    outcome: TurnOutcome::Failed,
                    steps,
                    interrupted: false,
                    trajectory: None,
                });
            }
        }
    }

    fn resume_after_stop_hook(
        &mut self,
        run: &mut TurnContinuation<'_>,
        backend: &mut Backend,
        ui: &mut dyn Ui,
    ) -> HookProgress {
        let max_steps = run.steps.max;
        // A blocking stop hook feeds its reason back and resumes through verification above.
        let payload = match &run.result {
            Ok(report) => {
                json!({"session_id":self.session.id(),"outcome":format!("{:?}", report.outcome),"answer":crate::agent::window::prompt::truncate_chars(&report.text, 4000),"stop_hook_active":run.stop_rounds > 0})
            }
            Err(_) => return HookProgress::Settled,
        };
        if !crate::agent::safety::hooks::wanted(self.tool_hooks(), "stop") {
            return HookProgress::Settled;
        }
        let configured_hooks = self.tool_hooks();
        let root = self.workspace.root();
        let interrupt = self.interrupt.as_ref();
        let (verdict, notices) = crate::agent::safety::hooks::with_heartbeat(ui, || {
            let outcome = crate::agent::safety::hooks::fire_with(
                configured_hooks,
                root,
                "stop",
                None,
                payload,
                interrupt,
            );
            (outcome.verdict, outcome.notices)
        });
        notices.iter().for_each(|n| {
            ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                crate::agent::ui::ui_event::EventKind::Hook,
                n,
            ))
        });
        if self.interrupt.load(Ordering::SeqCst)
            && let Ok(report) = &mut run.result
            && report.outcome == TurnOutcome::Completed
        {
            report.outcome = TurnOutcome::Interrupted;
            report.interrupted = true;
            report.text = unsettled_report(
                &run.candidate_summary,
                "Interrupted during the stop hook; completion was not accepted.",
            );
            return HookProgress::Settled;
        }
        let reason = match verdict {
            crate::agent::safety::hooks::Verdict::Block { reason } => reason,
            crate::agent::safety::hooks::Verdict::Continue { .. } => return HookProgress::Settled,
        };
        let may_continue = matches!(&run.result, Ok(report) if report.outcome == TurnOutcome::Completed && !report.interrupted)
            && !self.interrupt.load(Ordering::SeqCst);
        if !may_continue {
            return HookProgress::Settled;
        }
        if run.stop_rounds >= STOP_HOOK_ROUNDS {
            let diagnostic = format!(
                "Blocked: stop hook still blocks completion after {STOP_HOOK_ROUNDS} rounds. {reason}"
            );
            ui.notice(&diagnostic);
            if let Ok(report) = &mut run.result {
                report.outcome = TurnOutcome::Blocked;
                report.text = unsettled_report(&run.candidate_summary, &diagnostic);
            }
            return HookProgress::Settled;
        }
        let Some(resume_steps) = run.steps.remaining() else {
            let diagnostic = format!(
                "Stopped: this turn used its {max_steps}-step limit before the stop hook's requested change could run. {reason}"
            );
            ui.notice(&diagnostic);
            if let Ok(report) = &mut run.result {
                report.outcome = TurnOutcome::Incomplete;
                report.text = unsettled_report(&run.candidate_summary, &diagnostic);
            }
            return HookProgress::Settled;
        };
        run.stop_rounds += 1;
        let mut resumed = (|| -> Result<TurnReport> {
            self.record(
                0,
                "stop_hook_block",
                json!({"reason":reason,"round":run.stop_rounds}),
            )?;
            self.push(json!({"role":"user","content":format!("A stop hook blocked completion. Address this before finishing:\n{reason}")}))?;
            let state = run
                .state
                .as_mut()
                .context("a stop-hook round needs the turn it resumes")?;
            let report = self.resume_round(
                state,
                &mut run.engine,
                backend,
                &mut run.schemas,
                resume_steps,
                ui,
            );
            self.flush_accepted_live_steering(ui)?;
            report.map(|(mut report, consumed)| {
                report.steps = run.steps.charge(report.steps, consumed);
                report
            })
        })();
        if let Ok(report) = &mut resumed
            && report.outcome != TurnOutcome::Completed
        {
            report.text = unsettled_report(&run.candidate_summary, &report.text);
        }
        run.result = resumed;
        HookProgress::Resumed
    }
}

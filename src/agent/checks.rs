//! Focused verification after edits: check selection, runs, publication and completion validation.

use super::*;

/// One focused-check pass. `failures` keeps every final problem visible; `actionable` holds only the
/// diagnostics of confirmed code failures, the sole input to automatic repair.
pub(super) struct CheckRun {
    pub(super) failures: Vec<String>,
    pub(super) actionable: Vec<String>,
}

impl<'a> Agent<'a> {
    pub(super) fn note_verification(
        &mut self,
        call: &ToolCall,
        output: &ToolOutput,
        ui: &mut dyn Ui,
    ) {
        let Some(command) = call.arguments["command"].as_str() else {
            return;
        };
        let configured = self
            .config
            .verification
            .commands
            .iter()
            .find(|check| check.command == command);
        if configured.is_none()
            && !is_check_command(command)
            && !call.id.starts_with("focused-check:")
        {
            return;
        }
        let kind = if configured.is_some_and(|check| {
            self.config
                .verification
                .behavioral_checks
                .contains(&check.name)
        }) {
            repair_evidence::CheckKind::Behavioral
        } else {
            repair_evidence::CheckKind::SyntaxOnly
        };
        let check = configured
            .map(|check| verification::FocusedCheck {
                name: check.name.clone(),
                command: check.command.clone(),
                timeout_secs: check.timeout_secs,
            })
            .or_else(|| {
                call.id
                    .strip_prefix("focused-check:")
                    .map(|name| verification::FocusedCheck {
                        name: name.to_owned(),
                        command: command.to_owned(),
                        timeout_secs: 0,
                    })
            });
        // Share the verdict and runner context used by focused-check diagnostics.
        let verdict = verification::classify_output(
            check.as_ref(),
            output,
            self.interrupt.load(Ordering::SeqCst),
        );
        let exit = verdict.repair_exit_code(output);
        let passed = verdict == verification::CheckStatus::Passed;
        if !self.hermetic
            && matches!(kind, repair_evidence::CheckKind::Behavioral)
            && let Some(cache) = self.codebase.clone()
        {
            let command = command.to_owned();
            let _ = thread::scope(|scope| -> Result<_> {
                let (tx, rx) = mpsc::sync_channel(1);
                scope.spawn(move || {
                    let _ = tx.send(cache.record_behavior_check(&command, passed));
                });
                receive_with_heartbeat(&rx, ui)
            });
        }
        self.assistance.check(
            command,
            passed && matches!(kind, repair_evidence::CheckKind::Behavioral),
        );
        self.verifications.push(passed);
        if matches!(kind, repair_evidence::CheckKind::Behavioral) {
            self.behavioral_checks.push(passed);
        }
        let _ = self.record(
            self.cur_step,
            "verification",
            json!({
                "command":truncate(command, 200),"exit":exit,"passed":passed,"category":kind
            }),
        );
        if self.hermetic {
            return;
        }
        let (Some(store), Some(checkpoints), Some(turn), Some(artifacts), Some(trajectory)) = (
            self.store,
            self.checkpoints.as_ref(),
            self.step_checks.turn,
            self.workspace.artifacts(),
            self.trajectory.as_ref().map(|(id, _, _)| *id),
        ) else {
            return;
        };
        self.repair_sequence += 1;
        let captured = (|| -> Result<()> {
            let mut changes = checkpoints.repair_changes(turn)?;
            // Private checkpoint files are outside the read tool's workspace. Publish immutable
            // source bytes through artifact_read rather than hand the model unusable filesystem paths.
            for change in &mut changes {
                for reference in [&mut change.before_ref, &mut change.after_ref]
                    .into_iter()
                    .flatten()
                {
                    let mut source = std::fs::File::open(reference.as_str())?;
                    let mut spool = artifacts.spool()?;
                    std::io::copy(&mut source, &mut spool)?;
                    *reference = spool.finish()?;
                }
            }
            let evidence_ref = artifacts.retain(&output.text)?;
            let run = repair_evidence::CheckRun {
                command: command.to_owned(),
                cwd: self.workspace.root().to_string_lossy().into_owned(),
                objective: configured
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| "Syntax, build, or undeclared check only".into()),
                kind,
                tool_call_id: format!("{}:{}:{}", self.session.id(), call.id, self.repair_sequence),
                evidence_ref,
                exit_code: exit,
                sequence: self.repair_sequence,
            };
            if let Some(record) = self.repair_tracker.observe(run, changes) {
                store.executable_repair(
                    trajectory,
                    self.version.id,
                    &serde_json::to_value(record)?,
                )?;
            }
            Ok(())
        })();
        if let Err(error) = captured {
            self.repair_tracker.clear();
            let _ = self.record(
                self.cur_step,
                "repair_evidence_unavailable",
                json!({"error":format!("{error:#}")}),
            );
        }
    }

    pub(super) fn run_focused_check(
        &mut self,
        check: &verification::FocusedCheck,
        ui: &mut dyn Ui,
    ) -> ToolOutput {
        let args = json!({"command":check.command,"timeout":check.timeout_secs});
        // Long generated commands (an interpreter path plus inline code) say nothing the check name does not.
        let title = if check.command.len() <= 80 {
            format!("Verify {}: {}", check.name, check.command)
        } else {
            format!("Verify {}", check.name)
        };
        let call_id = format!("focused-check:{}", check.name);
        let mut display = args.clone();
        display["_dial_check"] = json!(check.name);
        ui.tool_call_with_args(&call_id, "bash", &title, &display);
        let started = Instant::now();
        let output = self
            .run_background_tool("bash", &args, ui, None)
            .unwrap_or_else(|| tools::execute(&self.workspace, "bash", &args, check.timeout_secs));
        ui.tool_timing("bash", started.elapsed().as_secs_f64());
        ui.tool_result_with_id(&call_id, "bash", &output);
        self.note_verification(
            &ToolCall {
                id: format!("focused-check:{}", check.name),
                name: "bash".into(),
                arguments: args,
                malformed_arguments: false,
            },
            &output,
            ui,
        );
        output
    }

    /// Focused checks for the files this turn changed: built-in syntax/type checks plus the configured
    /// commands whose extensions match.
    pub(super) fn focused_checks(&self, turn: u64) -> Vec<verification::FocusedCheck> {
        let paths = self
            .checkpoints
            .as_ref()
            .map(|checkpoints| checkpoints.changed_files(turn))
            .unwrap_or_default()
            .into_iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        // Endpoint users may never have run `dial setup`; a missing interpreter must not read as broken code.
        let python = if self.config.python.is_file() {
            self.config.python.clone()
        } else {
            PathBuf::from("python3")
        };
        let mut checks = verification::plan_with_runner(
            self.workspace.root(),
            &python,
            &paths,
            &self.config.verification.python_command,
        );
        if self.config.assistance.enabled && self.config.assistance.selective_verification {
            checks.extend(verification::related_tests_with_runner(
                self.workspace.root(),
                &python,
                &paths,
                &self.config.verification.python_command,
            ));
        }
        for custom in &self.config.verification.commands {
            let matches = custom.file_extensions.is_empty()
                || paths.iter().any(|path| {
                    path.extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| {
                            custom.file_extensions.iter().any(|suffix| suffix == ext)
                        })
                });
            if matches {
                checks.push(verification::FocusedCheck {
                    name: custom.name.clone(),
                    command: custom.command.clone(),
                    timeout_secs: custom.timeout_secs,
                });
            }
        }
        checks
    }

    pub(super) fn publish_no_automatic_check(&mut self, ui: &mut dyn Ui) {
        if self.hermetic {
            return;
        }
        let mut control = self.session.control_state();
        if control["reliability"].is_object() {
            control["reliability"]["verification"] =
                json!("no automatic focused check available; behavior unverified");
            control["reliability"]["checked_commands"] = json!([]);
            control["reliability"]["edit_loop"] = self.step_checks.status();
            if let Err(error) = self.session.set_control_state(control.clone()) {
                ui.diagnostic(&format!("could not save verification status: {error:#}"));
            }
            ui.request_policy(&control);
        }
    }
    pub(super) fn publish_edit_loop_status(&mut self, ui: &mut dyn Ui) {
        if self.hermetic {
            return;
        }
        let mut control = self.session.control_state();
        control["reliability"]["edit_loop"] = self.step_checks.status();
        if let Err(error) = self.session.set_control_state(control.clone()) {
            ui.diagnostic(&format!("could not save edit-loop status: {error:#}"));
        }
        ui.request_policy(&control);
    }
    /// List this turn's checkpointed changes (bounded) for the user when the turn ends and mark
    /// them and any uncheckpointed operations disclosed. The model gets no nudge and no extra round.
    pub(super) fn disclose_turn_changes(&mut self, ui: &mut dyn Ui) {
        self.step_checks.disclosed_shell_commands = self.step_checks.uncheckpointed_shell_commands;
        self.step_checks.disclosed_extension_calls =
            self.step_checks.uncheckpointed_extension_calls;
        if let (Some(turn), Some(checkpoints)) = (self.step_checks.turn, self.checkpoints.as_ref())
            && self.step_checks.mutations > self.step_checks.diff_surfaced
        {
            let paths = checkpoints.changed_files(turn);
            ui.changed_files(&paths);
        }
        // Without harness-run checks the status would otherwise keep the request-time placeholder.
        if !(self.config.verification.enabled && self.config.verification.automatic_checks) {
            self.publish_no_automatic_check(ui);
        }
        self.step_checks.diff_surfaced = self.step_checks.mutations;
        self.publish_edit_loop_status(ui);
    }

    /// Run `checks` until interrupted; one entry per failed check with its (clipped) output.
    pub(super) fn run_checks(
        &mut self,
        checks: &[verification::FocusedCheck],
        ui: &mut dyn Ui,
    ) -> CheckRun {
        if !checks.is_empty() {
            self.step_checks.failed_checks.clear();
        }
        let mut failures = Vec::new();
        let mut actionable = Vec::new();
        let mut results = Vec::new();
        let mut outcomes = Vec::new();
        let receipts = self.config.experimental.verification_receipts && !self.hermetic;
        let receipt_paths: Vec<String> = if receipts {
            {
                self.step_checks
                    .turn
                    .zip(self.checkpoints.as_ref())
                    .map(|(turn, checkpoints)| checkpoints.changed_files(turn))
                    .unwrap_or_default()
            }
        } else {
            Default::default()
        };
        let source_before = receipts
            .then(|| verification_receipts::source_identity(self.workspace.root(), &receipt_paths));
        for (index, check) in checks.iter().enumerate() {
            if self.interrupt.load(Ordering::SeqCst) {
                for skipped in &checks[index..] {
                    self.step_checks.failed_checks.push(skipped.clone());
                    results.push(json!({"name":skipped.name,"command":skipped.command,"status":verification::CheckStatus::Cancelled}));
                    outcomes.push(verification_receipts::Outcome {
                        name: skipped.name.clone(),
                        command: skipped.command.clone(),
                        status: verification::CheckStatus::Cancelled,
                        ran: false,
                    });
                }
                failures.push(format!("{} not run: verification interrupted", check.name));
                break;
            }
            let output = self.run_focused_check(check, ui);
            let status =
                verification::classify(check, &output, self.interrupt.load(Ordering::SeqCst));
            results.push(json!({"name":check.name,"command":check.command,"status":status}));
            outcomes.push(verification_receipts::Outcome {
                name: check.name.clone(),
                command: check.command.clone(),
                status,
                ran: true,
            });
            // A passing check is already its own `✓ Verify …` tool row; only a problem earns a notice.
            if status != verification::CheckStatus::Passed {
                ui.event(&crate::agent::ui_event::UiEvent::new(
                    crate::agent::ui_event::EventKind::Verification,
                    format!("Verification {}: {status:?}", check.name),
                ));
                self.step_checks.failed_checks.push(check.clone());
                let description = verification::environment_problem(check, &output.text).unwrap_or(
                    "Check failed; inspect the diagnostics and repair the observed failure.",
                );
                let message = format!(
                    "{} failed:\n{description}\nCommand: {}\n{}",
                    check.name,
                    check.command,
                    guard::clip_evidence(&output.text, 4096)
                );
                // Infrastructure, permission, cancellation, timeout and unverified problems are reported
                // without sending the model into code repair.
                if status.repairable() {
                    actionable.push(message.clone());
                }
                failures.push(message);
            }
        }
        let passed_count = results
            .iter()
            .filter(|result| result["status"] == "passed")
            .count() as u64;
        let failed_check_count = results.len() as u64 - passed_count;
        if !failures.is_empty()
            && let Some(paths) = self
                .step_checks
                .turn
                .as_ref()
                .zip(self.checkpoints.as_ref())
                .map(|(turn, checkpoints)| checkpoints.changed_files(*turn))
                .filter(|paths| !paths.is_empty())
        {
            let header = format!("Files changed in this turn: {}", paths.join(", "));
            if !actionable.is_empty() {
                actionable.insert(0, header.clone());
            }
            failures.insert(0, header);
        }
        self.step_checks.failed = !failures.is_empty();

        if !self.hermetic {
            let mut control = self.session.control_state();
            if control["reliability"].is_object() {
                let state = &mut control["reliability"];
                let changed = self
                    .step_checks
                    .turn
                    .zip(self.checkpoints.as_ref())
                    .map(|(turn, checkpoints)| checkpoints.changed_files(turn))
                    .unwrap_or_default()
                    .into_iter()
                    .map(PathBuf::from)
                    .collect::<Vec<_>>();
                let limits = verification::coverage_limits(&changed);
                state["verification"] = json!(if checks.is_empty() {
                    "no automatic focused check available; behavior unverified"
                } else if failures.is_empty() && !limits.is_empty() {
                    "focused checks passed with partial coverage; see coverage_limits"
                } else if failures.is_empty() {
                    "focused checks passed; assess coverage against requirements"
                } else {
                    "focused checks failed"
                });
                state["coverage_limits"] = json!(limits);
                state["checked_commands"] =
                    json!(checks.iter().map(|c| &c.name).collect::<Vec<_>>());
                state["verification_results"] = json!(results);
                state["automatic_passing_checks"] =
                    json!(state["automatic_passing_checks"].as_u64().unwrap_or(0) + passed_count);
                state["automatic_failing_checks"] = json!(
                    state["automatic_failing_checks"].as_u64().unwrap_or(0) + failed_check_count
                );
                state["edit_loop"] = self.step_checks.status();
                if let Some(before) = source_before.as_ref().filter(|_| !outcomes.is_empty()) {
                    let after = verification_receipts::source_identity(
                        self.workspace.root(),
                        &receipt_paths,
                    );
                    verification_receipts::append(
                        state,
                        verification_receipts::receipt(before, &after, &outcomes),
                    );
                }
                ui.request_policy(&control);
                if let Err(error) = self.session.set_control_state(control) {
                    ui.diagnostic(&format!("could not save verification status: {error:#}"));
                }
            }
        }
        CheckRun {
            failures,
            actionable,
        }
    }

    /// Side-effect-free completion blockers shared by `finish completed` and `goal complete`: explicit
    /// blockers, running shell or worker jobs, unreadable worker state, and failed checks with no edit
    /// awaiting a fresh check. Callers decide whether to pump workers or disclose changes.
    pub(super) fn completion_blockers(&self) -> Result<(), String> {
        let blocked = self
            .todo
            .tasks()
            .iter()
            .any(|task| task.status == todo::Status::Blocked);
        if blocked {
            return Err("Explicitly blocked tasks remain. Resolve their prerequisites or report outcome=blocked.".into());
        }
        let awaited = self.awaited_background_jobs();
        if !awaited.is_empty() {
            return Err(format!(
                "cannot complete while background jobs are running:\n{}\nWait for them with `bg wait` or stop them with `bg kill`, then try again.",
                awaited.join("\n")
            ));
        }
        let jobs = match self.live_tasks.status(None) {
            Ok(jobs) => jobs,
            Err(error) => {
                return Err(format!(
                    "cannot complete: live task state is unreadable ({error}); inspect subagent status before finishing"
                ));
            }
        };
        if jobs
            .as_array()
            .is_some_and(|jobs| jobs.iter().any(|job| job["status"] == "running"))
        {
            return Err("cannot complete while live tasks are running; wait with `task wait` or continue independent work, then try again".into());
        }
        // Every effect generation counts: a shell or extension repair after a failure is new work that the
        // outer verification pass rechecks, so only a failure with no newer effect is refused here.
        let unchecked_effects = self.step_checks.mutations > self.step_checks.checked
            || self.step_checks.uncheckpointed_shell_commands
                > self.step_checks.checked_shell_commands
            || self.step_checks.uncheckpointed_extension_calls
                > self.step_checks.checked_extension_calls;
        if !self.hermetic
            && self.step_checks.failed
            && (!self.config.verification.enabled || !unchecked_effects)
        {
            return Err("Cannot complete: focused checks failed and no new edits are awaiting a fresh check. Repair, rerun the failed check, or report a blocker.".into());
        }
        Ok(())
    }

    /// One completion boundary for plain reports and explicit finish calls. Pumps worker results
    /// first, so a worker that already finished is not reported as running.
    pub(super) fn validate_completion(&mut self, ui: &mut dyn Ui) -> Result<(), String> {
        self.pump_subagents(ui);
        self.completion_blockers()?;
        if !self.hermetic {
            self.disclose_turn_changes(ui);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;

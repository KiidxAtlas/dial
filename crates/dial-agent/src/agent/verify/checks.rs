//! Focused verification after edits: check selection, runs, publication and completion validation.

use crate::agent::*;

/// One focused-check pass. `failures` keeps every final problem visible; `actionable` holds only the
/// diagnostics of confirmed code failures, the sole input to automatic repair.
pub(in crate::agent) struct CheckRun {
    pub(in crate::agent) failures: Vec<String>,
    pub(in crate::agent) actionable: Vec<String>,
}

impl<'a> Agent<'a> {
    /// Capture a complete contract separately from whether a usable witness is available.
    /// `Err` means a contract exists but could not be captured; `Ok(None)` is undeclared.
    fn capture_check_inputs(
        &self,
        check: &verify::verification::FocusedCheck,
    ) -> Result<Option<verify::inputs::Snapshot>, ()> {
        if self.step_checks.evidence.is_repro(&check.command) {
            return Ok(None);
        }
        let mut configured = self
            .config
            .verification
            .commands
            .iter()
            .filter(|custom| custom.command == check.command || custom.name == check.name);
        if let Some(custom) = configured.next() {
            let Some(contract) = self.config.verification.inputs.get(&check.name) else {
                return Ok(None);
            };
            if configured.next().is_some()
                || custom.name != check.name
                || custom.command != check.command
                || custom.timeout_secs != check.timeout_secs
            {
                return Err(());
            }
            return self.capture_check_inputs_when_idle(|| {
                verify::inputs::capture(&self.workspace, contract)
            });
        }
        let python = if self.config.python.is_file() {
            self.config.python.clone()
        } else {
            PathBuf::from("python3")
        };
        let Some(source) = verify::verification::automatic_python_source(
            self.workspace.root(),
            &python,
            &self.config.verification.python_command,
            check,
        ) else {
            return Ok(None);
        };
        self.capture_check_inputs_when_idle(|| {
            verify::inputs::python_snapshot(&self.workspace, &source, &python)
        })
    }

    fn capture_check_inputs_when_idle(
        &self,
        capture: impl FnOnce() -> Option<verify::inputs::Snapshot>,
    ) -> Result<Option<verify::inputs::Snapshot>, ()> {
        if self.plan_mode || !self.awaited_background_jobs().is_empty() {
            return Err(());
        }
        let jobs = self.live_tasks.status(None).map_err(|_| ())?;
        if jobs
            .as_array()
            .ok_or(())?
            .iter()
            .any(|job| job["status"] == "running")
        {
            return Err(());
        }
        capture().map(Some).ok_or(())
    }

    fn check_inputs(
        &self,
        check: &verify::verification::FocusedCheck,
    ) -> Option<verify::inputs::Snapshot> {
        self.capture_check_inputs(check).ok().flatten()
    }

    pub(in crate::agent) fn reuse_check(
        &mut self,
        check: &verify::verification::FocusedCheck,
    ) -> bool {
        if self.interrupt.load(Ordering::SeqCst)
            || !self.step_checks.evidence.has_inputs(&check.command)
        {
            return false;
        }
        let Some(inputs) = self.check_inputs(check) else {
            self.step_checks.evidence.validate_inputs(
                &check.command,
                None,
                self.step_checks.revision(),
            );
            return false;
        };
        self.step_checks
            .evidence
            .reuse_inputs(&check.command, &inputs, self.step_checks.revision())
    }

    /// Revalidate even at an unchanged effect epoch: user edits do not advance controller counters.
    pub(in crate::agent) fn refresh_check_inputs(&mut self) {
        for check in self.step_checks.evidence.input_checks() {
            let inputs = self.check_inputs(&check);
            if self.step_checks.evidence.validate_inputs(
                &check.command,
                inputs.as_ref(),
                self.step_checks.revision(),
            ) {
                self.step_checks.checked = 0;
                self.step_checks.checked_shell_commands = 0;
                self.step_checks.checked_extension_calls = 0;
            }
        }
    }

    pub(in crate::agent) fn note_verification(
        &mut self,
        call: &ToolCall,
        output: &ToolOutput,
        ui: &mut dyn Ui,
    ) {
        self.note_verification_verdict(call, output, ui, None);
    }

    fn note_verification_verdict(
        &mut self,
        call: &ToolCall,
        output: &ToolOutput,
        ui: &mut dyn Ui,
        override_verdict: Option<verify::verification::CheckStatus>,
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
            crate::storage::repair_evidence::CheckKind::Behavioral
        } else {
            crate::storage::repair_evidence::CheckKind::SyntaxOnly
        };
        let check = configured
            .map(|check| crate::agent::verify::verification::FocusedCheck {
                name: check.name.clone(),
                command: check.command.clone(),
                timeout_secs: check.timeout_secs,
            })
            .or_else(|| {
                call.id.strip_prefix("focused-check:").map(|name| {
                    crate::agent::verify::verification::FocusedCheck {
                        name: name.to_owned(),
                        command: command.to_owned(),
                        timeout_secs: 0,
                    }
                })
            });
        // Share the verdict and runner context used by focused-check diagnostics.
        let verdict = override_verdict.unwrap_or_else(|| {
            crate::agent::verify::verification::classify_with_artifacts(
                check.as_ref(),
                configured
                    .and_then(|check| self.config.verification.runners.get(&check.name))
                    .map(|runner| (runner.program.as_str(), runner.module.as_deref())),
                output,
                self.interrupt.load(Ordering::SeqCst),
                self.workspace.artifacts(),
            )
        });
        let exit = verdict.repair_exit_code(output);
        let passed = verdict == crate::agent::verify::verification::CheckStatus::Passed;
        self.step_checks.evidence.observe_with_fix_revision(
            command,
            &call.id,
            verdict,
            self.step_checks.revision(),
            matches!(kind, crate::storage::repair_evidence::CheckKind::Behavioral),
            self.step_checks.fix_revision(),
        );
        self.step_checks.evidence.diagnose(command, &output.text);
        if !self.hermetic
            && matches!(kind, crate::storage::repair_evidence::CheckKind::Behavioral)
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
            passed && matches!(kind, crate::storage::repair_evidence::CheckKind::Behavioral),
        );
        self.verifications.push(passed);
        if matches!(kind, crate::storage::repair_evidence::CheckKind::Behavioral) {
            self.behavioral_checks.push(passed);
        }
        if let Err(error) = self.record(
            self.cur_step,
            "verification",
            json!({
                "command":truncate(command, 200),"exit":exit,"passed":passed,"category":kind
            }),
        ) {
            ui.notice(&format!("check result was not recorded: {error:#}"));
        }
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
            let run = crate::storage::repair_evidence::CheckRun {
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
            if let Err(record_error) = self.record(
                self.cur_step,
                "repair_evidence_unavailable",
                json!({"error":format!("{error:#}")}),
            ) {
                ui.notice(&format!(
                    "repair evidence was not recorded: {error:#}; {record_error:#}"
                ));
            }
        }
    }

    pub(in crate::agent) fn run_focused_check(
        &mut self,
        check: &crate::agent::verify::verification::FocusedCheck,
        ui: &mut dyn Ui,
    ) -> ToolOutput {
        let args = json!({"command":check.command,"timeout":check.timeout_secs});
        let inputs_before = self.capture_check_inputs(check);
        let revision_before = self.step_checks.revision();
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
        // Hermetic evaluation runs keep their trusted, non-prompting policy; explicit deny rules still hold.
        let approval = if self.plan_mode {
            crate::config::types::Approval::Readonly
        } else if self.hermetic {
            crate::config::types::Approval::Auto
        } else {
            self.approval
        };
        // Model-registered reproductions are untrusted shell text: they take the approval path a
        // configured command (and a model `bash` call) takes, never the generated-check boundary.
        let repro = self.step_checks.evidence.is_repro(&check.command);
        let configured = repro
            || self
                .config
                .verification
                .commands
                .iter()
                .any(|custom| custom.command == check.command);
        let gate = crate::agent::safety::approval::authorize_automatic_check(
            approval,
            &mut self.always,
            self.workspace.root(),
            &crate::agent::safety::approval::AutomaticCheck {
                name: &check.name,
                command: &check.command,
                configured,
            },
            ui,
            Some(self.interrupt.as_ref()),
        );
        let started = Instant::now();
        let mut output = match gate {
            Ok(persist) => {
                if persist && let Err(error) = self.persist_permissions() {
                    ui.notice(&format!(
                        "The rule applies to this session but could not be saved: {error:#}"
                    ));
                }
                if repro && let Some(reason) = self.repro_hook_refusal(&args, ui) {
                    ui.notice(&reason);
                    crate::runtime::tool_output::ToolOutput::err(format!(
                        "automatic check {} not run: {reason}",
                        check.name
                    ))
                    .with_status(crate::tools::ToolStatus::Blocked)
                } else {
                    if repro
                        && !crate::agent::safety::approval::safe_bash(&check.command)
                        && !crate::agent::safety::approval::safe_review_batch(&check.command)
                    {
                        self.step_checks.note_repro_run();
                    }
                    self.run_background_tool("bash", &args, ui, None)
                        .unwrap_or_else(|| {
                            tools::execute(&self.workspace, "bash", &args, check.timeout_secs)
                        })
                }
            }
            Err(denial) => {
                ui.notice(denial.message());
                crate::agent::safety::approval::denied_check_output(&check.name, &denial)
            }
        };
        let inputs_after = self.capture_check_inputs(check);
        let inputs_stale = inputs_before.is_err()
            || inputs_after.is_err()
            || inputs_before != inputs_after
            || (inputs_before.as_ref().is_ok_and(|inputs| inputs.is_some())
                && self.step_checks.revision() != revision_before);
        let unverified = inputs_stale && output.process_outcome().is_some_and(|p| p.started);
        if unverified {
            // Preserve process and effect facts: freshness is a verification verdict, not a
            // claim that the subprocess failed, was cancelled, or had unknown effects.
            output = output.with_status(crate::tools::ToolStatus::Error);
            output.text.push_str("\n[Verification unverified: declared inputs changed during the check or could not be captured completely. Rerun against stable inputs; this result earns neither pass nor code-failure repair credit.]");
        }
        if !matches!(&inputs_before, Ok(Some(_)))
            && output.process_outcome().is_some_and(|p| p.started)
        {
            // Even harness-launched commands without a complete contract may change hidden inputs.
            self.step_checks.evidence.invalidate_inputs();
        }
        ui.tool_timing("bash", started.elapsed().as_secs_f64());
        ui.tool_result_with_id(&call_id, "bash", &output);
        self.note_verification_verdict(
            &ToolCall {
                id: format!("focused-check:{}", check.name),
                name: "bash".into(),
                arguments: args,
                malformed_arguments: false,
            },
            &output,
            ui,
            unverified.then_some(verify::verification::CheckStatus::Unverified),
        );
        if self.step_checks.revision() == revision_before
            && !unverified
            && let Ok(Some(before)) = inputs_before
            && inputs_after == Ok(Some(before.clone()))
        {
            self.step_checks.evidence.bind_inputs(check, before);
        }
        let status = self.step_checks.evidence.latest_status(&check.command);
        if !unverified
            && let Some(status) = status
            && let Some(line) = self.counterfactual_after_check(check, status, ui)
        {
            output.text.push('\n');
            output.text.push_str(&line);
        }
        output
    }

    /// Focused checks for the files this turn changed: built-in syntax/type checks plus the configured
    /// commands whose extensions match.
    pub(in crate::agent) fn focused_checks(
        &self,
        turn: u64,
    ) -> Vec<crate::agent::verify::verification::FocusedCheck> {
        let paths = self
            .checkpoints
            .as_ref()
            .map(|checkpoints| checkpoints.changed_files(turn))
            .unwrap_or_default()
            .into_iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        self.focused_checks_for(&paths)
    }

    /// The gate's selection for the active checkpoint turn; only path-independent checks before one exists.
    pub(in crate::agent) fn current_focused_checks(
        &self,
    ) -> Vec<crate::agent::verify::verification::FocusedCheck> {
        match self.step_checks.turn {
            Some(turn) => self.focused_checks(turn),
            None => self.focused_checks_for(&[]),
        }
    }

    fn focused_checks_for(
        &self,
        paths: &[PathBuf],
    ) -> Vec<crate::agent::verify::verification::FocusedCheck> {
        // Endpoint users may never have run `dial setup`; a missing interpreter must not read as broken code.
        let python = if self.config.python.is_file() {
            self.config.python.clone()
        } else {
            PathBuf::from("python3")
        };
        let mut checks = crate::agent::verify::verification::plan_with_runner(
            self.workspace.root(),
            &python,
            paths,
            &self.config.verification.python_command,
        );
        if self.config.assistance.enabled && self.config.assistance.selective_verification {
            checks.extend(self.related_test_plan(paths).checks);
        }
        for custom in &self.config.verification.commands {
            let required = self
                .config
                .verification
                .requirements
                .values()
                .any(|requirement| requirement.checks.contains(&custom.name));
            let matches = required
                || custom.file_extensions.is_empty()
                || paths.iter().any(|path| {
                    path.extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| {
                            custom.file_extensions.iter().any(|suffix| suffix == ext)
                        })
                });
            if matches {
                checks.push(crate::agent::verify::verification::FocusedCheck {
                    name: custom.name.clone(),
                    command: custom.command.clone(),
                    timeout_secs: custom.timeout_secs,
                });
            }
        }
        // This turn's registered reproductions are selected like any other check.
        for repro in self.step_checks.evidence.repros() {
            if !checks.iter().any(|check| check.command == repro.command) {
                checks.push(repro.clone());
            }
        }
        checks
    }

    pub(in crate::agent) fn related_test_plan(
        &self,
        paths: &[PathBuf],
    ) -> crate::agent::verify::verification::RelatedTestPlan {
        let python = if self.config.python.is_file() {
            self.config.python.clone()
        } else {
            PathBuf::from("python3")
        };
        let mut plan = crate::agent::verify::verification::related_test_plan_with_runner(
            self.workspace.root(),
            &python,
            paths,
            &self.config.verification.python_command,
        );
        if !self.config.assistance.enabled || !self.config.assistance.selective_verification {
            plan.limits.push("selective behavioral test mapping is disabled; build/configured checks do not establish full behavioral coverage".into());
        }
        plan
    }

    pub(in crate::agent) fn publish_no_automatic_check(&mut self, ui: &mut dyn Ui) {
        if self.hermetic {
            return;
        }
        let mut control = self.session.control_state();
        if control["reliability"].is_object() {
            control["reliability"]["verification"] =
                json!("no automatic focused check available; behavior unverified");
            control["reliability"]["checked_commands"] = json!([]);
            let changed: Vec<_> = self
                .step_checks
                .turn
                .zip(self.checkpoints.as_ref())
                .map(|(turn, checkpoints)| checkpoints.changed_files(turn))
                .unwrap_or_default()
                .into_iter()
                .map(PathBuf::from)
                .collect();
            control["reliability"]["coverage_limits"] =
                json!(self.related_test_plan(&changed).limits);
            control["reliability"]["edit_loop"] = self.step_checks.status();
            if let Err(error) = self.session.set_control_state(control.clone()) {
                ui.diagnostic(&format!("could not save verification status: {error:#}"));
            }
            ui.request_policy(&control);
        }
    }
    pub(in crate::agent) fn publish_edit_loop_status(&mut self, ui: &mut dyn Ui) {
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
    pub(in crate::agent) fn disclose_turn_changes(&mut self, ui: &mut dyn Ui) {
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
    pub(in crate::agent) fn run_checks(
        &mut self,
        checks: &[crate::agent::verify::verification::FocusedCheck],
        ui: &mut dyn Ui,
    ) -> CheckRun {
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
        let source_before = receipts.then(|| {
            crate::agent::verify::verification_receipts::source_identity(
                self.workspace.root(),
                &receipt_paths,
            )
        });
        for (index, check) in checks.iter().enumerate() {
            if self.interrupt.load(Ordering::SeqCst) {
                for skipped in &checks[index..] {
                    self.step_checks
                        .failed_checks
                        .retain(|failed| failed.command != skipped.command);
                    self.step_checks.failed_checks.push(skipped.clone());
                    results.push(json!({"name":skipped.name,"command":skipped.command,"status":crate::agent::verify::verification::CheckStatus::Cancelled}));
                    self.step_checks.evidence.observe(
                        &skipped.command,
                        &format!("focused-check:{}", skipped.name),
                        crate::agent::verify::verification::CheckStatus::Cancelled,
                        self.step_checks.revision(),
                        self.config
                            .verification
                            .behavioral_checks
                            .contains(&skipped.name),
                    );
                    outcomes.push(crate::agent::verify::verification_receipts::Outcome {
                        name: skipped.name.clone(),
                        command: skipped.command.clone(),
                        status: crate::agent::verify::verification::CheckStatus::Cancelled,
                        ran: false,
                    });
                }
                failures.push(format!("{} not run: verification interrupted", check.name));
                break;
            }
            if self.reuse_check(check) {
                self.step_checks
                    .failed_checks
                    .retain(|failed| failed.command != check.command);
                results.push(json!({"name":check.name,"command":check.command,"status":"passed","reused":true,"ran":false}));
                outcomes.push(crate::agent::verify::verification_receipts::Outcome {
                    name: check.name.clone(),
                    command: check.command.clone(),
                    status: crate::agent::verify::verification::CheckStatus::Passed,
                    ran: false,
                });
                ui.notice(&format!(
                    "Reused verification {}: declared inputs unchanged (not rerun).",
                    check.name
                ));
                continue;
            }
            let output = self.run_focused_check(check, ui);
            let runner =
                self.config
                    .verification
                    .runners
                    .get(&check.name)
                    .filter(|_| {
                        self.config.verification.commands.iter().any(|custom| {
                            custom.name == check.name && custom.command == check.command
                        })
                    })
                    .map(|runner| (runner.program.as_str(), runner.module.as_deref()));
            // The focused runner applies freshness in addition to raw process classification.
            let status = self
                .step_checks
                .evidence
                .latest_status(&check.command)
                .unwrap_or(crate::agent::verify::verification::CheckStatus::Unverified);
            results.push(json!({"name":check.name,"command":check.command,"status":status,"reused":false,"ran":true}));
            outcomes.push(crate::agent::verify::verification_receipts::Outcome {
                name: check.name.clone(),
                command: check.command.clone(),
                status,
                ran: true,
            });
            self.step_checks
                .failed_checks
                .retain(|failed| failed.command != check.command);
            // A passing check is already its own `✓ Verify …` tool row; only a problem earns a notice.
            if status != crate::agent::verify::verification::CheckStatus::Passed {
                ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                    crate::agent::ui::ui_event::EventKind::Verification,
                    format!("Verification {}: {status:?}", check.name),
                ));
                self.step_checks.failed_checks.push(check.clone());
                // Requirement membership declares task scope without inventing behavioral credit.
                let required = self
                    .config
                    .verification
                    .requirements
                    .values()
                    .any(|requirement| requirement.checks.contains(&check.name));
                let unchanged = !required
                    && self
                        .step_checks
                        .evidence
                        .unchanged_unrelated_failure(&check.command);
                let description = if unchanged {
                    Some("Unchanged baseline failure outside declared behavioral scope; disclosed without automatic repair.")
                } else { crate::agent::verify::verification::runner_problem(
                    check,
                    runner,
                    &output.text,
                ) }
                .unwrap_or(
                    "Check failed; inspect the diagnostics and repair the observed failure.",
                );
                let message = format!(
                    "{} failed:\n{description}\nCommand: {}\n{}",
                    check.name,
                    check.command,
                    crate::agent::safety::guard::clip_evidence(&output.text, 4096)
                );
                // Infrastructure, permission, cancellation, timeout and unverified problems are reported
                // without sending the model into code repair.
                if status.repairable() && !unchanged {
                    actionable.push(message.clone());
                }
                failures.push(message);
            }
        }
        let passed_count = results
            .iter()
            .filter(|result| result["status"] == "passed" && result["reused"] != true)
            .count() as u64;
        let reused_count = results.iter().filter(|r| r["reused"] == true).count() as u64;
        let failed_check_count = results.len() as u64 - passed_count - reused_count;
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
        self.step_checks.failed = !self.step_checks.failed_checks.is_empty();

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
                let limits = self.related_test_plan(&changed).limits;
                state["verification"] = json!(if self.step_checks.failed {
                    "focused checks failed; retained obligations remain"
                } else if checks.is_empty() {
                    "no automatic focused check available; behavior unverified"
                } else if !limits.is_empty() {
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
                state["reused_passing_checks"] =
                    json!(state["reused_passing_checks"].as_u64().unwrap_or(0) + reused_count);
                state["edit_loop"] = self.step_checks.status();
                if let Some(before) = source_before.as_ref().filter(|_| !outcomes.is_empty()) {
                    let after = crate::agent::verify::verification_receipts::source_identity(
                        self.workspace.root(),
                        &receipt_paths,
                    );
                    crate::agent::verify::verification_receipts::append(
                        state,
                        crate::agent::verify::verification_receipts::receipt(
                            before, &after, &outcomes,
                        ),
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
    pub(in crate::agent) fn completion_blockers(&self) -> Result<(), String> {
        let blocked = self
            .todo
            .tasks()
            .iter()
            .any(|task| task.status == crate::agent::workflow::todo::Status::Blocked);
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
        if let Some(reason) = crate::agent::verify::requirements::strict_blocker(
            &self.config.verification,
            &self.step_checks.evidence,
            self.step_checks.revision(),
            &self.step_checks.visual,
        ) {
            return Err(reason);
        }
        if !self.hermetic
            && let Some(reason) = self.caller_obligation_blocker()
        {
            return Err(reason);
        }
        Ok(())
    }

    /// One completion boundary for plain reports and explicit finish calls. Pumps worker results
    /// first, so a worker that already finished is not reported as running.
    pub(in crate::agent) fn validate_completion(&mut self, ui: &mut dyn Ui) -> Result<(), String> {
        self.pump_subagents(ui);
        self.refresh_check_inputs();
        if !self.hermetic
            && !self.plan_mode
            && self.config.verification.strict_requirements
            && self.config.verification.automatic_checks
        {
            let revision = self.step_checks.revision();
            let required: Vec<_> = self
                .config
                .verification
                .commands
                .iter()
                .filter(|check| {
                    self.config
                        .verification
                        .requirements
                        .values()
                        .any(|requirement| requirement.checks.contains(&check.name))
                })
                .filter(|check| {
                    matches!(
                        self.step_checks
                            .evidence
                            .current_status(&check.command, revision),
                        "missing" | "stale" | "not_observed"
                    )
                })
                .map(|check| crate::agent::verify::verification::FocusedCheck {
                    name: check.name.clone(),
                    command: check.command.clone(),
                    timeout_secs: check.timeout_secs,
                })
                .collect();
            if !required.is_empty() {
                self.run_checks(&required, ui);
            }
        }
        self.completion_blockers()?;
        if !self.hermetic {
            self.disclose_turn_changes(ui);
        }
        Ok(())
    }

    /// An execution outcome is not a verdict on the deliverable. Keep the two independently
    /// observable without changing TurnReport or requiring a checklist for ordinary answers.
    pub(in crate::agent) fn completion_evidence(&self, outcome: TurnOutcome) -> Value {
        let accepted = outcome == TurnOutcome::Completed;
        let remaining = self
            .todo
            .tasks()
            .iter()
            .filter(|task| {
                task.status.is_open()
                    && !(accepted
                        && !self.modes.goal.tool_offered()
                        && task.status == crate::agent::workflow::todo::Status::InProgress)
            })
            .collect::<Vec<_>>();
        let revision = self.step_checks.revision();
        let checks = self.step_checks.evidence.snapshot(revision);
        let visual = self.step_checks.visual.status(revision);
        let requirements = crate::agent::verify::requirements::snapshot(
            &self.config.verification,
            &self.step_checks.evidence,
            revision,
            &self.step_checks.visual,
        );
        let verification = if self.step_checks.failed {
            "failed"
        } else if checks["stale_checks"].as_u64().unwrap_or(0) > 0 {
            "partial_or_stale"
        } else if checks["current_checks"].as_u64().unwrap_or(0) == 0 {
            "not_observed"
        } else if checks["passing_checks"] != checks["current_checks"] {
            "failed_or_unavailable"
        } else {
            "observed_checks_passed"
        };
        json!({
            "version": 1,
            "execution_outcome": outcome,
            "deliverable_coverage": if !remaining.is_empty() { "open_obligations" } else { requirements["coverage"].as_str().unwrap_or("not_independently_assessed") },
            "outstanding_tasks": remaining,
            "verification_coverage": verification,
            "visual_evidence": visual,
            "checks": checks,
            "requirements": requirements,
            "uncheckpointed_effects": self.step_checks.has_uncheckpointed_operations(),
            "caller_obligations": self.caller_obligations(),
            "counterfactual_checks": self.step_checks.counterfactual.evidence(),
            "note": "Observed checks and delivered pixels are evidence, not independent proof of every requested behavior. Undeclared requirements remain unassessed, not complete."
        })
    }
}

#[cfg(test)]
mod tests;

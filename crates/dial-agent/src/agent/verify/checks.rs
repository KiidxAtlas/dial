//! Focused verification after edits: check selection, runs, publication and completion validation.

use crate::agent::*;

/// One focused-check pass. `failures` keeps every final problem visible; `actionable` holds only the
/// diagnostics of confirmed code failures, the sole input to automatic repair.
pub(in crate::agent) struct CheckRun {
    pub(in crate::agent) failures: Vec<String>,
    pub(in crate::agent) actionable: Vec<String>,
    /// Confirmed diagnoses only: excludes changing prompt context such as the file-change header.
    pub(in crate::agent) diagnoses: Vec<String>,
}

/// Why a declared input witness could not be observed. Never a code-failure diagnosis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckInputError {
    AmbiguousContract,
    DeclaredInputsUnavailable,
    PlanMode,
    BackgroundJobs,
    WorkersUnavailable,
    WorkersRunning,
}

impl CheckInputError {
    fn reason(self) -> &'static str {
        match self {
            Self::AmbiguousContract => {
                "configured check does not uniquely match its input contract"
            }
            Self::DeclaredInputsUnavailable => "declared input contract could not be captured",
            Self::PlanMode => "plan mode prevents input capture",
            Self::BackgroundJobs => "background jobs may still change inputs",
            Self::WorkersUnavailable => "worker state could not be observed",
            Self::WorkersRunning => "running workers may still change inputs",
        }
    }
}

/// One bounded verification process, owned until its actual result is settled.
pub(in crate::agent) struct PendingCheck {
    check: verify::verification::FocusedCheck,
    inputs: Result<Option<verify::inputs::Snapshot>, CheckInputError>,
    revision: verify::evidence::Revision,
    started: Instant,
    cancel: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<ToolOutput>>,
}

impl Drop for PendingCheck {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl<'a> Agent<'a> {
    /// Capture a complete contract separately from whether a usable witness is available.
    /// An error explains why a witness is unavailable; `Ok(None)` is undeclared.
    fn capture_check_inputs(
        &self,
        check: &verify::verification::FocusedCheck,
    ) -> Result<Option<verify::inputs::Snapshot>, CheckInputError> {
        self.capture_check_inputs_sharing(check, &mut None)
    }

    /// `runtime` holds the interpreter identity for one revalidation sweep, captured on first need.
    /// Captures around a check's own run never share one: each must observe the runtime afresh.
    fn capture_check_inputs_sharing(
        &self,
        check: &verify::verification::FocusedCheck,
        runtime: &mut Option<Option<verify::inputs::PythonRuntime>>,
    ) -> Result<Option<verify::inputs::Snapshot>, CheckInputError> {
        let started = Instant::now();
        let result = self.capture_check_inputs_inner(check, runtime);
        self.efficiency_metrics
            .backend_operation("check_input_capture", started.elapsed());
        result
    }

    fn capture_check_inputs_inner(
        &self,
        check: &verify::verification::FocusedCheck,
        runtime: &mut Option<Option<verify::inputs::PythonRuntime>>,
    ) -> Result<Option<verify::inputs::Snapshot>, CheckInputError> {
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
                return Err(CheckInputError::AmbiguousContract);
            }
            return self.capture_check_inputs_when_idle(|| {
                verify::inputs::capture(&self.workspace, contract)
                    .map(Some)
                    .ok_or(CheckInputError::DeclaredInputsUnavailable)
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
            // Automatic inputs are an optional reuse optimization, not an operator contract.
            // Unknown runtimes still execute normally; a snapshot appearing/disappearing across
            // the run remains stale because the caller compares both captures.
            let runtime = runtime.get_or_insert_with(|| verify::inputs::python_runtime(&python));
            Ok(runtime.as_ref().and_then(|runtime| {
                verify::inputs::python_snapshot(&self.workspace, &source, runtime)
            }))
        })
    }

    fn capture_check_inputs_when_idle(
        &self,
        capture: impl FnOnce() -> Result<Option<verify::inputs::Snapshot>, CheckInputError>,
    ) -> Result<Option<verify::inputs::Snapshot>, CheckInputError> {
        if self.plan_mode {
            return Err(CheckInputError::PlanMode);
        }
        if !self.awaited_background_jobs().is_empty() {
            return Err(CheckInputError::BackgroundJobs);
        }
        let jobs = self
            .live_tasks
            .status(None)
            .map_err(|_| CheckInputError::WorkersUnavailable)?;
        if jobs
            .as_array()
            .ok_or(CheckInputError::WorkersUnavailable)?
            .iter()
            .any(|job| job["status"] == "running")
        {
            return Err(CheckInputError::WorkersRunning);
        }
        capture()
    }

    fn check_inputs(
        &self,
        check: &verify::verification::FocusedCheck,
    ) -> Option<verify::inputs::Snapshot> {
        self.capture_check_inputs(check).ok().flatten()
    }

    /// Declared input identities for `checks`, one per check in order; `None` when a check has no
    /// complete contract. Repair progress compares these rather than a bare effect epoch.
    pub(in crate::agent) fn check_input_snapshots(
        &self,
        checks: &[verify::verification::FocusedCheck],
    ) -> Vec<Option<verify::inputs::Snapshot>> {
        checks
            .iter()
            .map(|check| self.check_inputs(check))
            .collect()
    }

    pub(in crate::agent) fn reuse_check(
        &mut self,
        check: &verify::verification::FocusedCheck,
    ) -> bool {
        let started = Instant::now();
        let reused = self.reuse_check_inner(check);
        self.efficiency_metrics
            .backend_operation("check_reuse", started.elapsed());
        reused
    }

    fn reuse_check_inner(&mut self, check: &verify::verification::FocusedCheck) -> bool {
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
    /// One sweep is one observation of the installed interpreter, shared by its checks.
    pub(in crate::agent) fn refresh_check_inputs(&mut self) {
        let started = Instant::now();
        self.live_tasks
            .revalidate_acceptance(&self.config.verification);
        if let Err(error) = self.sync_contracts() {
            self.workflow_contracts
                .fail(format!("workflow evidence could not settle: {error:#}"));
        }
        let mut runtime = None;
        for check in self.step_checks.evidence.input_checks() {
            let inputs = self
                .capture_check_inputs_sharing(&check, &mut runtime)
                .ok()
                .flatten();
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
        self.efficiency_metrics
            .backend_operation("check_input_refresh", started.elapsed());
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
        }) || (configured.is_none()
            && call
                .id
                .strip_prefix("focused-check:")
                .is_some_and(crate::agent::verify::verification::test_targets::behavioral_target))
        {
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
        // A previous run made before any edit, shell effect or extension effect of this
        // instruction describes the code the user handed over: keep it as the command's
        // baseline. A debugging request is about such a failure, so it never becomes one.
        if self.task_policy.kind != crate::learn::task_policy::Kind::Debugging {
            self.step_checks.evidence.capture_baseline_of(command);
        }
        self.step_checks.evidence.observe_with_fix_revision(
            command,
            &call.id,
            verdict,
            self.step_checks.revision(),
            matches!(kind, crate::storage::repair_evidence::CheckKind::Behavioral),
            self.step_checks.fix_revision(),
        );
        let evidence_ref = self.retain_check_log(output);
        self.step_checks
            .evidence
            .bind_evidence_ref(command, evidence_ref);
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
            // source bytes through read artifact:// URIs rather than hand the model unusable filesystem paths.
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
        self.drain_background_check(ui, true);
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
        let model_registered = self
            .step_checks
            .registered_acceptance
            .iter()
            .any(|registered| registered.command == check.command);
        let configured = repro
            || model_registered
            || crate::agent::verify::verification::test_targets::exit_status_target(&check.name)
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
        let output = match gate {
            Ok(persist) => {
                if persist && let Err(error) = self.persist_permissions() {
                    ui.notice(&format!(
                        "The rule applies to this session but could not be saved: {error:#}"
                    ));
                }
                if (repro || model_registered)
                    && let Some(reason) = self.repro_hook_refusal(&args, ui)
                {
                    ui.notice(&reason);
                    crate::runtime::tool_output::ToolOutput::err(format!(
                        "automatic check {} not run: {reason}",
                        check.name
                    ))
                    .with_status(crate::tools::ToolStatus::Blocked)
                } else {
                    if (repro || model_registered)
                        && !crate::agent::safety::approval::safe_bash(&check.command)
                        && !crate::agent::safety::approval::safe_review_batch(&check.command)
                    {
                        self.step_checks.note_repro_run();
                    }
                    // A check must finish to verify anything; it is never auto-backgrounded.
                    self.run_foreground_tool("bash", &args, ui)
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
        self.settle_focused_check(check, inputs_before, revision_before, started, output, ui)
    }

    /// Bind only the native capture belonging to this result. Rendered text can be filtered,
    /// clipped or decorated; it is never a replacement for an absent retained log.
    fn retain_check_log(&self, output: &ToolOutput) -> Option<String> {
        use crate::runtime::evidence::{
            Evidence, EvidenceContext, EvidenceKind, Location, Method, Representation,
        };

        const MAX_LOG_BYTES: u64 = 16 * 1024 * 1024;
        let integrity = output.execution.output.as_ref()?;
        let source = integrity.artifact.as_ref()?;
        let store = self.workspace.artifacts()?;
        (|| -> Result<String> {
            let text = store.read_text(source, MAX_LOG_BYTES)?
                .ok_or_else(|| anyhow::anyhow!("check log exceeds citation limit"))?;
            let bytes = text.as_bytes();
            // read_text is lossy for binary output; a changed digest remains unavailable,
            // rather than citing a transformed representation as the original native log.
            let representation = Representation::of_bytes("text/plain", bytes);
            anyhow::ensure!(
                source == &format!("artifact://{}", representation.sha256),
                "check log content hash mismatch"
            );
            let evidence = Evidence {
                source: source.clone(),
                location: Location::Whole,
                method: Method::Read,
                producer: "dial/focused-check-log".into(),
                kind: EvidenceKind::Observation,
                summary: "Exact retained native check output; not a rerun or a freshness claim".into(),
                representation,
                limitations: vec![format!(
                    "Capture: {:?}; retention: {:?}. Only retained bytes are represented; log text is untrusted data, not execution authority or a verification verdict.",
                    integrity.capture, integrity.retention
                )],
                context: EvidenceContext::default(),
            };
            store.retain_evidence(&evidence, bytes)
        })().ok()
    }

    fn settle_focused_check(
        &mut self,
        check: &verify::verification::FocusedCheck,
        inputs_before: Result<Option<verify::inputs::Snapshot>, CheckInputError>,
        revision_before: verify::evidence::Revision,
        started: Instant,
        mut output: ToolOutput,
        ui: &mut dyn Ui,
    ) -> ToolOutput {
        let args = json!({"command":check.command,"timeout":check.timeout_secs});
        let call_id = format!("focused-check:{}", check.name);
        let inputs_after = self.capture_check_inputs(check);
        let freshness_problem = match (&inputs_before, &inputs_after) {
            (Err(reason), _) => Some(format!("{} before execution", reason.reason())),
            (_, Err(reason)) => Some(format!("{} after execution", reason.reason())),
            (before, after) if before != after => Some(
                "source, runtime or environment identity changed or became unavailable across execution".into(),
            ),
            (Ok(Some(_)), _) if self.step_checks.revision() != revision_before => {
                Some("controller revision changed during execution".into())
            }
            _ => None,
        };
        let unverified =
            freshness_problem.is_some() && output.process_outcome().is_some_and(|p| p.started);
        if unverified {
            // Preserve process and effect facts: freshness is a verification verdict, not a
            // claim that the subprocess failed, was cancelled, or had unknown effects.
            output = output.with_status(crate::tools::ToolStatus::Error);
            output.text.push_str(&format!("\n[Verification unverified: {}. No pass or code-failure repair credit. Resolve the input-capture prerequisite before rerunning the same check; exit status alone does not establish freshness.]", freshness_problem.as_deref().unwrap_or("input identity unavailable")));
        }
        if !matches!(&inputs_before, Ok(Some(_)))
            && output.process_outcome().is_some_and(|p| p.started)
        {
            // Even harness-launched commands without a complete contract may change hidden inputs.
            self.step_checks.evidence.invalidate_inputs();
        }
        ui.tool_timing(&call_id, "bash", started.elapsed().as_secs_f64());
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

    /// Settle on the controller; never convert an unfinished process into evidence.
    pub(in crate::agent) fn drain_background_check(&mut self, ui: &mut dyn Ui, wait: bool) {
        let Some(pending) = self.pending_check.as_ref() else {
            return;
        };
        if !wait
            && !pending
                .worker
                .as_ref()
                .is_some_and(|worker| worker.is_finished())
        {
            return;
        }
        let mut pending = self
            .pending_check
            .take()
            .expect("pending check inspected above");
        while pending
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            if self.interrupt.load(Ordering::SeqCst) {
                pending.cancel.store(true, Ordering::SeqCst);
            }
            self.pump_subagents(ui);
            std::thread::sleep(Duration::from_millis(20));
        }
        let output = pending
            .worker
            .take()
            .expect("pending check owns worker")
            .join()
            .unwrap_or_else(|_| {
                ToolOutput::err("focused verification worker panicked; no completion observed")
            });
        self.settle_focused_check(
            &pending.check,
            pending.inputs.clone(),
            pending.revision,
            pending.started,
            output,
            ui,
        );
    }

    /// Start only operator-configured checks with complete input contracts. They remain
    /// ordinary shell authority, not permission grants, and one process is owned at a time.
    pub(in crate::agent) fn start_background_check(&mut self, ui: &mut dyn Ui) {
        if !self.config.experimental.background_checks
            || !self.config.verification.automatic_checks
            || self.hermetic
            || self.plan_mode
            || self.pending_check.is_some()
            || self.interrupt.load(Ordering::SeqCst)
        {
            return;
        }
        let checks = self.current_focused_checks();
        let Some((check, inputs)) = checks.into_iter().find_map(|check| {
            if !self
                .config
                .verification
                .commands
                .iter()
                .any(|custom| custom.name == check.name && custom.command == check.command)
            {
                return None;
            }
            let inputs = self.capture_check_inputs(&check).ok().flatten()?;
            Some((check, inputs))
        }) else {
            return;
        };
        if self.reuse_check(&check) {
            return;
        }
        let call_id = format!("focused-check:{}", check.name);
        let args = json!({"command":check.command,"timeout":check.timeout_secs});
        ui.tool_call_with_args(
            &call_id,
            "bash",
            &format!("Verify {} (background)", check.name),
            &args,
        );
        let gate = crate::agent::safety::approval::authorize_automatic_check(
            self.approval,
            &mut self.always,
            self.workspace.root(),
            &crate::agent::safety::approval::AutomaticCheck {
                name: &check.name,
                command: &check.command,
                configured: true,
            },
            ui,
            Some(self.interrupt.as_ref()),
        );
        let started = Instant::now();
        let revision = self.step_checks.revision();
        match gate {
            Err(denial) => {
                let output =
                    crate::agent::safety::approval::denied_check_output(&check.name, &denial);
                self.settle_focused_check(&check, Ok(Some(inputs)), revision, started, output, ui);
                return;
            }
            Ok(persist) => {
                if persist && let Err(error) = self.persist_permissions() {
                    ui.notice(&format!(
                        "The rule applies to this session but could not be saved: {error:#}"
                    ));
                }
            }
        }
        let workspace = self.workspace.clone();
        let mut config = self.effective_config().clone();
        // Verification owns the actual process to completion, not a detached shell ticket.
        // This is an execution-local clone; operator settings and ordinary bash are unchanged.
        config.shell.auto_background_secs = 0;
        let registry = self.ext.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let worker = std::thread::Builder::new()
            .name("dial-focused-check".into())
            .spawn(move || {
                crate::tools::exec::execution::run(&worker_cancel, || {
                    let _serial = workspace.serial_operation();
                    if worker_cancel.load(Ordering::SeqCst) {
                        return ToolOutput::err("focused verification cancelled before execution");
                    }
                    crate::tools::exec::execution::native(
                        &crate::tools::integrations::ext::ToolContext {
                            workspace: &workspace,
                            config: &config,
                            mutation_authorizer: None,
                            confirm: None,
                        },
                        &registry,
                        "bash",
                        &args,
                        &|_| {},
                        &worker_cancel,
                    )
                    .expect("bash is a native tool")
                })
            });
        match worker {
            Ok(worker) => {
                self.pending_check = Some(PendingCheck {
                    check,
                    inputs: Ok(Some(inputs)),
                    revision,
                    started,
                    cancel,
                    worker: Some(worker),
                })
            }
            Err(error) => {
                self.settle_focused_check(
                    &check,
                    Ok(Some(inputs)),
                    revision,
                    started,
                    ToolOutput::err(format!("starting focused verification: {error}")),
                    ui,
                );
            }
        }
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
        let mut checks = match self.step_checks.turn {
            Some(turn) => self.focused_checks(turn),
            None => self.focused_checks_for(&[]),
        };
        for check in &self.step_checks.registered_acceptance {
            if !checks.iter().any(|prior| prior.command == check.command) {
                checks.push(check.clone());
            }
        }
        checks
    }

    /// Interpreter for generated Python checks. Endpoint users may never have run `dial setup`;
    /// a missing interpreter must not read as broken code.
    pub(in crate::agent) fn check_python(&self) -> PathBuf {
        if self.config.python.is_file() {
            self.config.python.clone()
        } else {
            PathBuf::from("python3")
        }
    }

    /// Whether a check's result is behavioral evidence: an operator-declared behavioral check or
    /// a test runner selected for a `verify` target.
    pub(in crate::agent) fn behavioral_check(&self, name: &str) -> bool {
        self.config
            .verification
            .behavioral_checks
            .iter()
            .any(|behavioral| behavioral == name)
            || crate::agent::verify::verification::test_targets::behavioral_target(name)
    }

    fn focused_checks_for(
        &self,
        paths: &[PathBuf],
    ) -> Vec<crate::agent::verify::verification::FocusedCheck> {
        let python = self.check_python();
        let mut checks = crate::agent::verify::verification::plan_with_runner(
            self.workspace.root(),
            &python,
            paths,
            &self.config.verification.python_command,
        );
        checks.retain(|check| {
            !crate::agent::verify::verification::python_ast_unrunnable(
                self.workspace.root(),
                &python,
                &self.config.verification.python_command,
                check,
            )
        });
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

    fn verification_source_identity(
        &self,
        paths: &[String],
    ) -> crate::agent::verify::verification_receipts::SourceIdentity {
        let started = Instant::now();
        let identity = crate::agent::verify::verification_receipts::source_identity(
            self.workspace.root(),
            paths,
        );
        self.efficiency_metrics
            .backend_operation("verification_source_identity", started.elapsed());
        identity
    }

    /// Run `checks` until interrupted; one entry per failed check with its (clipped) output.
    pub(in crate::agent) fn run_checks(
        &mut self,
        checks: &[crate::agent::verify::verification::FocusedCheck],
        ui: &mut dyn Ui,
    ) -> CheckRun {
        self.drain_background_check(ui, true);
        let mut failures = Vec::new();
        let mut actionable = Vec::new();
        let mut diagnoses = Vec::new();
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
        let source_before = receipts.then(|| self.verification_source_identity(&receipt_paths));
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
                        self.behavioral_check(&skipped.name),
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
                results.push(json!({"name":check.name,"command":check.command,"status":"passed","reused":true,"ran":false,"evidence_ref":self.step_checks.evidence.evidence_ref(&check.command)}));
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
            if output
                .execution
                .process
                .as_ref()
                .is_some_and(|process| process.started)
            {
                self.step_checks.observations += 1;
                self.efficiency_metrics.full_builds += u64::from(
                    crate::agent::workflow::efficiency::broad_check(&check.command),
                );
            }
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
            results.push(json!({"name":check.name,"command":check.command,"status":status,"reused":false,"ran":true,"evidence_ref":self.step_checks.evidence.evidence_ref(&check.command)}));
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
                use crate::agent::verify::verification::CheckStatus;
                let description = if unchanged {
                    "Unchanged baseline failure outside declared behavioral scope; disclosed without automatic repair."
                } else {
                    match status {
                        CheckStatus::CodeFailed => "Check failed with a code/test diagnostic; inspect it and repair the observed failure.",
                        CheckStatus::FailedUnknownCause => "Check exited nonzero, but its cause is unconfirmed. Inspect the retained diagnostics and runner/environment before deciding whether source repair is appropriate.",
                        CheckStatus::EnvironmentUnavailable => crate::agent::verify::verification::runner_problem(check, runner, &output.text).unwrap_or("Verification environment unavailable; restore the declared runner/environment and rerun without changing application code."),
                        CheckStatus::PermissionDenied => "Verification permission denied; obtain the required approval before rerunning. This is not a source-code diagnosis.",
                        CheckStatus::Cancelled => "Verification cancelled; no source-code diagnosis was established.",
                        CheckStatus::TimedOut => "Verification timed out; inspect the run and its limits before deciding on a cause.",
                        CheckStatus::Unverified | CheckStatus::Passed => "Verification evidence is incomplete; inspect the run and rerun the same acceptance check when its prerequisites are met.",
                    }
                };
                let label = if status == CheckStatus::Unverified {
                    "unverified"
                } else {
                    "failed"
                };
                let message = format!(
                    "{} {label}:\n{description}\nCommand: {}\n{}",
                    check.name,
                    check.command,
                    crate::agent::safety::guard::clip_evidence(&output.text, 4096)
                );
                // An unexplained failure, infrastructure, permission, cancellation, timeout or
                // unverified problem is reported without sending the model into source repair.
                if status.repairable() && !unchanged {
                    diagnoses.push(message.clone());
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
                    let after = self.verification_source_identity(&receipt_paths);
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
            diagnoses,
        }
    }

    /// A recorded external prerequisite halts automation; missing evidence alone is incomplete.
    /// Classify from controller state rather than a model's explanation or diagnostic prose.
    pub(in crate::agent) fn completion_rejection_outcome(&self) -> TurnOutcome {
        let blocked_task = self.todo.blocked_steps().next().is_some();
        let control = self.session.control_state();
        let blocked_deliverable = control["working_state"]["deliverables"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["status"] == "blocked"));
        if blocked_task || blocked_deliverable {
            TurnOutcome::Blocked
        } else {
            TurnOutcome::Incomplete
        }
    }

    /// Side-effect-free completion blockers shared by `finish completed` and `goal complete`: explicit
    /// blockers, running shell or worker jobs, unreadable worker state, and failed checks with no edit
    /// awaiting a fresh check. Callers decide whether to pump workers or disclose changes.
    pub(in crate::agent) fn completion_blockers(&self) -> Result<(), String> {
        self.completion_blockers_scoped(true)
    }

    /// [`Self::completion_blockers`]; `open_steps` false leaves unfinished (not blocked) execution
    /// steps to the caller, which on non-lean routes lists them with the accepted answer instead.
    pub(in crate::agent) fn completion_blockers_scoped(
        &self,
        open_steps: bool,
    ) -> Result<(), String> {
        let blocked: Vec<_> = self
            .todo
            .blocked_steps()
            .map(|task| {
                format!(
                    "{}: {}",
                    task.content,
                    task.note
                        .as_deref()
                        .unwrap_or("No blocking reason recorded")
                )
            })
            .collect();
        if !blocked.is_empty() {
            return Err(format!(
                "Explicitly blocked tasks remain. Resolve their prerequisites or report outcome=blocked.\n{}",
                blocked.join("\n")
            ));
        }
        let open = self
            .todo
            .tasks()
            .iter()
            .filter(|task| {
                task.status.is_open() && task.kind == crate::agent::workflow::todo::Kind::Step
            })
            .map(|task| task.content.as_str())
            .collect::<Vec<_>>();
        if open_steps && !open.is_empty() {
            return Err(format!(
                "Execution steps remain open: {}. Complete or explicitly block them; a final reply does not close them.",
                open.join("; ")
            ));
        }
        let control = self.session.control_state();
        let working = &control["working_state"];
        if let Some(blocker) = crate::agent::workflow::working_state::completion_blocker(working) {
            return Err(blocker);
        }
        let coverage = verify::requirements::snapshot(
            &self.config.verification,
            &self.step_checks.evidence,
            self.step_checks.revision(),
            &self.step_checks.visual,
        );
        if let Some(blocker) = verify::requirements::deliverable_blocker(&coverage, working) {
            return Err(blocker);
        }
        let unpassed = self.unpassed_check_links(working);
        if !unpassed.is_empty() {
            return Err(format!(
                "Satisfied deliverables cite checks that are not currently passing: {}. Run or fix those checks, or cancel or block the deliverables.",
                unpassed.join(", ")
            ));
        }
        if !self.side_questions.running_ids().is_empty() {
            return Err("cannot complete while side questions are running; use side_request wait or cancel and collect their results".into());
        }
        if self.side_questions.has_undelivered() {
            return Err("cannot complete before completed side-question results have been delivered to the conversation".into());
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
        // Failures that say nothing about this turn's code (see `disclosed_failure`) are disclosed
        // with the accepted answer; any other current failure still refuses completion.
        let diagnosis = self.diagnosis_requested();
        if !self.hermetic
            && self.step_checks.failed
            && (!self.config.verification.enabled || !unchecked_effects)
            && self
                .step_checks
                .failed_checks
                .iter()
                .any(|check| self.disclosed_failure(check, diagnosis).is_none())
        {
            return Err("Cannot complete: focused checks failed and no new edits are awaiting a fresh check. Repair, rerun the failed check, or report a blocker.".into());
        }
        if let Some(reason) = self.workflow_contracts.blocker() {
            return Err(reason);
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
        // Obligations exist only for non-hermetic user turns with target evidence enabled; the
        // tool offer is built only when one is open.
        if !self.plan_mode
            && self.step_checks.targets.has_obligations()
            && let Some(reason) = self.step_checks.targets.blocker(
                &self.workspace,
                self.step_checks.effect_epoch(),
                self.terminal_launch_available(),
                diagnosis && self.step_checks.mutations == 0,
            )
        {
            return Err(reason);
        }
        Ok(())
    }

    /// The latest instruction asks why something fails without asking for a repair.
    fn diagnosis_requested(&self) -> bool {
        self.session.control_state()["current_instruction"]
            .as_str()
            .is_some_and(crate::learn::task_policy::diagnosis_only)
    }

    /// Why a current failure of `check` is disclosed with an accepted answer rather than refusing
    /// it, or `None` while it stays a completion blocker:
    /// - an unavailable environment, a denied permission or a time limit says nothing about the
    ///   code, unless the same check passed before this instruction's first effect;
    /// - a request to diagnose a failure is answered by observing it, while nothing was edited;
    /// - an unchanged pre-existing code failure outside declared requirements predates the turn
    ///   (never on a debugging request, which is about such a failure).
    pub(in crate::agent) fn disclosed_failure(
        &self,
        check: &crate::agent::verify::verification::FocusedCheck,
        diagnosis: bool,
    ) -> Option<&'static str> {
        use crate::agent::verify::verification::CheckStatus;
        let evidence = &self.step_checks.evidence;
        if matches!(
            evidence.current_status(&check.command, self.step_checks.revision()),
            "not_observed" | "stale" | "passed"
        ) {
            return None;
        }
        let passed_before = evidence.baseline_status(&check.command) == Some(CheckStatus::Passed);
        let required = self
            .config
            .verification
            .requirements
            .values()
            .any(|requirement| requirement.checks.contains(&check.name));
        match evidence.latest_status(&check.command)? {
            CheckStatus::EnvironmentUnavailable if !passed_before => {
                Some("verification environment unavailable")
            }
            CheckStatus::PermissionDenied if !passed_before => {
                Some("verification permission denied")
            }
            CheckStatus::TimedOut if !passed_before => Some("verification timed out"),
            CheckStatus::Cancelled => None,
            _ if diagnosis && self.step_checks.mutations == 0 => {
                Some("the failure this request asked to diagnose; nothing was edited")
            }
            CheckStatus::CodeFailed
                if !required
                    && self.task_policy.kind != crate::learn::task_policy::Kind::Debugging
                    && evidence.unchanged_unrelated_failure(&check.command) =>
            {
                Some("unchanged pre-existing failure outside the requested scope")
            }
            _ => None,
        }
    }

    /// Current failures an accepted answer discloses instead of being refused for, one line each:
    /// failed checks (`disclosed_failure`) and, for a diagnosis request, requested runs that failed.
    pub(in crate::agent) fn disclosed_gaps(&self) -> Vec<String> {
        let diagnosis = self.diagnosis_requested();
        let mut seen = std::collections::BTreeSet::new();
        let mut checks = self.step_checks.failed_checks.clone();
        if self.unverified_edits_apply() {
            checks.extend(self.current_focused_checks());
        }
        let mut gaps: Vec<String> = checks
            .into_iter()
            .filter(|check| seen.insert(check.command.clone()))
            .filter_map(|check| {
                self.disclosed_failure(&check, diagnosis)
                    .map(|why| format!("- {}: {why}", check.name))
            })
            .collect();
        if diagnosis
            && self.step_checks.mutations == 0
            && !self.plan_mode
            && self.step_checks.targets.has_obligations()
        {
            gaps.extend(self.step_checks.targets.conclusive_failures(
                &self.workspace,
                self.step_checks.effect_epoch(),
                self.terminal_launch_available(),
            ));
        }
        gaps
    }

    /// Eligible offered or loadable terminal launches, not an execution grant.
    fn terminal_launch_available(&self) -> bool {
        self.launch_recovery_capabilities(&self.offered())
            .terminal_available()
    }

    /// Whether this turn owes evidence for its edits at all: something was edited outside plan mode,
    /// verification is on, and automatic checks will not verify after the turn anyway.
    fn unverified_edits_apply(&self) -> bool {
        !(self.hermetic
            || self.plan_mode
            || !self.config.verification.enabled
            || self.config.verification.automatic_checks
            || self.step_checks.mutations == 0)
    }

    /// The applicable checks when this turn's file edits have no passing check on the current
    /// revision. `None` when no evidence is owed: nothing was edited, no check applies, automatic
    /// checks verify after the turn anyway, a relevant `check` or shell test command passed, or
    /// every applicable check either passed or failed for a disclosed reason (`disclosed_failure`).
    pub(in crate::agent) fn unverified_edits(&mut self) -> Option<Vec<String>> {
        if !self.unverified_edits_apply() {
            return None;
        }
        self.refresh_check_inputs();
        let revision = self.step_checks.revision();
        if self.step_checks.shell_check_passed == Some(revision) {
            return None;
        }
        let checks = self.current_focused_checks();
        let passed = |check: &crate::agent::verify::verification::FocusedCheck| {
            self.step_checks
                .evidence
                .current_status(&check.command, revision)
                == "passed"
        };
        let diagnosis = self.diagnosis_requested();
        let observed = checks.iter().any(passed)
            || checks
                .iter()
                .all(|check| passed(check) || self.disclosed_failure(check, diagnosis).is_some());
        (!checks.is_empty() && !observed)
            .then(|| checks.into_iter().map(|check| check.name).collect())
    }

    /// The completion gate's evidence question, answered by running the applicable checks once
    /// (same approval and freshness path as the `check` tool) before a model round is spent on a
    /// reminder. Returns the still-unverified check names and the observed failures, which keep
    /// the reminder even when another applicable check passed.
    pub(in crate::agent) fn gate_unverified_edits(
        &mut self,
        run: bool,
        ui: &mut dyn Ui,
    ) -> (Option<Vec<String>>, Vec<String>) {
        let unverified = self.unverified_edits();
        if !run || unverified.is_none() || self.interrupt.load(Ordering::SeqCst) {
            return (unverified, Vec::new());
        }
        let checks = self.current_focused_checks();
        let failures = self.run_checks(&checks, ui).failures;
        let diagnosis = self.diagnosis_requested();
        let blocking = checks.iter().any(|check| {
            self.step_checks
                .failed_checks
                .iter()
                .any(|failed| failed.command == check.command)
                && self.disclosed_failure(check, diagnosis).is_none()
        });
        if failures.is_empty() || !blocking {
            // Only disclosed failures remain; `disclosed_gaps` reports them with the answer.
            (self.unverified_edits(), Vec::new())
        } else {
            (unverified, failures)
        }
    }

    /// One completion boundary for plain reports and explicit finish calls. Pumps worker results
    /// first, so a worker that already finished is not reported as running.
    pub(in crate::agent) fn validate_completion(&mut self, ui: &mut dyn Ui) -> Result<(), String> {
        self.validate_completion_scoped(ui, true)
    }

    /// [`Self::validate_completion`]; `open_steps` as in [`Self::completion_blockers_scoped`].
    pub(in crate::agent) fn validate_completion_scoped(
        &mut self,
        ui: &mut dyn Ui,
        open_steps: bool,
    ) -> Result<(), String> {
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
        // `finish` is refused once while an unattended run would silently kill a session service.
        if let Some(notice) = self.exit_bound_jobs_notice() {
            return Err(notice);
        }
        self.completion_blockers_scoped(open_steps)?;
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
            .filter(|task| task.status.is_open())
            .collect::<Vec<_>>();
        let working = self.session.control_state();
        let declared_open =
            crate::agent::workflow::working_state::open_deliverables(&working["working_state"]);
        let unpassed_links = self.unpassed_check_links(&working["working_state"]);
        let declared_blocker =
            crate::agent::workflow::working_state::completion_blocker(&working["working_state"]);
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
            "work_completion_accepted": accepted,
            "deliverable_coverage": if remaining.iter().any(|task| task.kind == crate::agent::workflow::todo::Kind::Step) || declared_blocker.is_some() || !unpassed_links.is_empty() { "open_obligations" } else { requirements["coverage"].as_str().unwrap_or("not_independently_assessed") },
            "outstanding_tasks": remaining,
            "optional_follow_ups": remaining.iter().filter(|task| task.kind == crate::agent::workflow::todo::Kind::FollowUp).collect::<Vec<_>>(),
            "declared_deliverables": working["working_state"]["deliverables"],
            "outstanding_deliverables": declared_open,
            "deliverable_claim_policy":"model_reported_not_independent_verification",
            "unpassed_check_links": unpassed_links,
            "reconciliation": self.deliverable_reconciliation(&working["working_state"]),
            "verification_coverage": verification,
            "visual_evidence": visual,
            "checks": checks,
            "requirements": requirements,
            "workflow_jobs": self.workflow_contracts.snapshot(),
            "uncheckpointed_effects": self.step_checks.has_uncheckpointed_operations(),
            "caller_obligations": self.caller_obligations(),
            "requested_targets": self.step_checks.targets.evidence(&self.workspace, self.step_checks.effect_epoch()),
            "counterfactual_checks": self.step_checks.counterfactual.evidence(),
            "disclosed_gaps": self.disclosed_gaps(),
            "note": "Observed checks and delivered pixels are evidence, not independent proof of every requested behavior. Undeclared requirements remain unassessed, not complete."
        })
    }

    /// Resolve evidence per obligation without promoting model claims or arbitrary references.
    /// A passing linked check establishes that check's result, not complete semantic coverage.
    pub(in crate::agent) fn deliverable_reconciliation(&self, working: &Value) -> Value {
        let links = self.deliverable_check_links(working);
        let coverage = verify::requirements::snapshot(
            &self.config.verification,
            &self.step_checks.evidence,
            self.step_checks.revision(),
            &self.step_checks.visual,
        );
        let items: Vec<Value> = working["deliverables"]
            .as_array()
            .into_iter()
            .flatten()
            .take(32)
            .map(|item| {
                let checks: Vec<Value> = links.iter()
                    .filter(|(id, _, _)| item["id"].as_str() == Some(id.as_str()))
                    .map(|(_, name, status)| json!({"name":name,"status":status}))
                    .collect();
                let evidence_status = if item["status"] != "satisfied" {
                    "not_claimed_satisfied"
                } else if checks.is_empty() {
                    "claim_only"
                } else if checks.iter().all(|check| check["status"] == "passed") {
                    "linked_checks_passed"
                } else {
                    "linked_checks_unresolved"
                };
                json!({
                    "id":item["id"], "description":item["description"],
                    "claimed_status":item["status"], "evidence_status":evidence_status,
                    "acceptance": verify::requirements::deliverable_acceptance(&coverage, item["id"].as_str().unwrap_or_default()),
                    "recorded_origin":item["origin"], "scope_receipt":item["receipt"],
                    "checks":checks,
                    "unverified_refs":item["evidence_refs"].as_array().into_iter().flatten()
                        .filter(|reference| !reference.as_str().is_some_and(|value| value.starts_with(crate::agent::workflow::working_state::CHECK_REF_PREFIX)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        json!({
            "deliverables":items,
            "omitted_deliverables":working["deliverables"].as_array().map_or(0, |items| items.len().saturating_sub(32)),
            "last_scope_change":working["last_scope_change"],
            "undeclared_requirements":"unassessed",
            "semantic_coverage":"not_independently_verified"
        })
    }

    /// `id -> check:name` links without exactly one current passing command. Never executes refs.
    fn unpassed_check_links(&self, working: &Value) -> Vec<String> {
        self.deliverable_check_links(working)
            .into_iter()
            .filter(|(_, _, status)| status != "passed")
            .map(|(id, name, status)| format!("{id} -> check:{name} ({status})"))
            .collect()
    }

    fn deliverable_check_links(&self, working: &Value) -> Vec<(String, String, String)> {
        let links = crate::agent::workflow::working_state::check_references(working);
        if links.is_empty() {
            return Vec::new();
        }
        let revision = self.step_checks.revision();
        let selection = self.current_focused_checks();
        links
            .into_iter()
            .map(|(id, name)| {
                let mut commands: Vec<&str> = self
                    .config
                    .verification
                    .commands
                    .iter()
                    .filter(|check| check.name == name)
                    .map(|check| check.command.as_str())
                    .collect();
                commands.extend(
                    selection
                        .iter()
                        .chain(self.step_checks.evidence.repros().iter())
                        .filter(|check| check.name == name)
                        .map(|check| check.command.as_str()),
                );
                commands.sort_unstable();
                commands.dedup();
                let status = match commands.as_slice() {
                    [] => "unknown_check",
                    [command] => self.step_checks.evidence.current_status(command, revision),
                    _ => "ambiguous_check",
                };
                (id, name, status.to_owned())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;

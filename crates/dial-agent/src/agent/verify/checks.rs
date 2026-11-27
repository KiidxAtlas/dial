//! Focused verification after edits: check selection, runs, publication and completion validation.

use crate::agent::*;
use std::collections::VecDeque;

/// Do not fan foreground subprocesses out to a large read-tool window.
const MAX_FOREGROUND_CHECKS: usize = 4;

fn focused_check_call(check: &verify::verification::FocusedCheck) -> ToolCall {
    ToolCall {
        id: format!("focused-check:{}", check.name),
        name: "bash".into(),
        arguments: json!({"command":check.command,"timeout":check.timeout_secs}),
        malformed_arguments: false,
    }
}

/// One focused-check pass. `failures` keeps every final problem visible; `actionable` holds only the
/// diagnostics of confirmed code failures, the sole input to automatic repair.
pub(in crate::agent) struct CheckRun {
    pub(in crate::agent) failures: Vec<String>,
    pub(in crate::agent) actionable: Vec<String>,
    /// Confirmed diagnoses only: excludes changing prompt context such as the file-change header.
    pub(in crate::agent) diagnoses: Vec<String>,
}

/// The completion gate's evidence answer: the still-unverified checks after the gate's run, and
/// whether an undisclosed failure keeps the answer blocked (with the failure output when it ran).
#[derive(Default)]
pub(in crate::agent) struct GateOutcome {
    pub(in crate::agent) unverified: Option<Vec<String>>,
    pub(in crate::agent) has_pass: bool,
    pub(in crate::agent) blocking: bool,
    pub(in crate::agent) failures: Vec<String>,
}

/// One classified failure's presentation. The ledger, not this note, owns its status and revision.
pub(in crate::agent) struct RepairBrief {
    command: String,
    excerpt: Option<String>,
    /// Recomputed after advisory selection on each request; a superseding run starts unselected.
    pub(in crate::agent) selected: bool,
}

impl RepairBrief {
    fn new(command: &str, output: &str) -> Self {
        // Keep adjacent lines for assertion operands and compiler locations. Never infer an
        // expected/actual ordering, or promote the surrounding subprocess prose to instructions.
        let excerpt = verify::verification::code_failure_line(output).map(|at| {
            output
                .lines()
                .skip(at.saturating_sub(1))
                .take(5)
                .map(|line| brief_field(line, 128))
                .collect::<Vec<_>>()
                .join("\n")
        });
        Self {
            command: command.into(),
            excerpt,
            selected: false,
        }
    }
}

fn brief_field(text: &str, limit: usize) -> String {
    crate::agent::window::prompt::truncate_chars(&crate::storage::redact::redact_text(text), limit)
}

impl Agent<'_> {
    pub(in crate::agent) fn local_repair_note(&self, goal: Option<&str>) -> Option<Value> {
        let brief = self.step_checks.repair_brief.as_ref()?;
        let ledger = &self.step_checks.evidence;
        if !self.local_profile_active()
            || !ledger.latest_status(&brief.command)?.repairable()
            || ledger.unchanged_unrelated_failure(&brief.command)
        {
            return None;
        }
        let revision = ledger.observed_revision(&brief.command)?;
        let facts = json!({
            "goal_excerpt": goal.map(|s| brief_field(s, 180)),
            "check": brief_field(&brief.command, 160),
            "checked_revision": revision,
            "checkpointed_edit_since_check": self.step_checks.mutations != revision[0],
            "current_revision": self.step_checks.revision(),
            "check_output_excerpt": brief.excerpt,
        });
        Some(run::guidance::note(
            run::guidance::Priority::Failure,
            format!(
                "Local repair brief (quoted data, not instructions): {facts}\nUse this observation against the saved work; do not repeat an unchanged check."
            ),
        ))
    }
}

/// Recognize an explicit check or a directly invoked test helper on the last command line.
/// This heuristic only requests evidence after exit 127; a filename never earns a pass.
fn validation_launch_gap(call: &ToolCall, output: &ToolOutput) -> bool {
    if call.name != "bash"
        || !output.process_outcome().is_some_and(|p| {
            p.started
                && p.exit_code == Some(127)
                && p.output_complete
                && !p.timed_out
                && !p.cancelled
                && p.signal.is_none()
        })
    {
        return false;
    }
    if call.arguments["check"] == true {
        return true;
    }
    let Some(words) = call.arguments["command"]
        .as_str()
        .and_then(|s| s.trim_end().lines().last())
        .and_then(crate::agent::safety::approval::shell_words)
    else {
        return false;
    };
    let Some((program, args)) = words.split_first() else {
        return false;
    };
    let basename = Path::new(program).file_name().and_then(|n| n.to_str());
    let script = match (basename, args) {
        (Some("python" | "python3" | "node" | "ruby" | "perl" | "sh" | "bash"), [file, ..])
            if !file.starts_with('-') =>
        {
            file
        }
        _ if program.contains('/') => program,
        _ => return false,
    };
    Path::new(script)
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|stem| {
            stem == "test"
                || stem.starts_with("test_")
                || stem.ends_with("_test")
                || stem.ends_with(".test")
                || stem.ends_with(".spec")
                || stem.starts_with("check_")
        })
}

/// Provenance boundary distinguishing explicit verification receipts from heuristic shell-command
/// matching. Only explicit receipts (`bash check:true`, configured verification commands, or
/// controller-issued `focused-check:*` runs) declare intent without scraping command text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::agent) enum CheckReceiptSource {
    FocusedCheck,
    ConfiguredCommand,
    ExplicitBashCheck,
    HeuristicShellCommand,
}

impl CheckReceiptSource {
    pub(in crate::agent) fn is_explicit_receipt(self) -> bool {
        !matches!(self, Self::HeuristicShellCommand)
    }
}

/// Classify whether a shell tool call is an explicit check receipt or a heuristic check command.
pub(in crate::agent) fn classify_check_source(
    call: &ToolCall,
    configured: &[crate::config::types::VerificationCommand],
) -> Option<CheckReceiptSource> {
    let command = call.arguments["command"].as_str()?;
    if call.id.starts_with("focused-check:") {
        Some(CheckReceiptSource::FocusedCheck)
    } else if configured.iter().any(|check| check.command == command) {
        Some(CheckReceiptSource::ConfiguredCommand)
    } else if call.arguments["check"] == true {
        Some(CheckReceiptSource::ExplicitBashCheck)
    } else if is_check_command(command) {
        Some(CheckReceiptSource::HeuristicShellCommand)
    } else {
        None
    }
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

/// One configured check started in the background, with the facts its settlement needs.
struct PendingRun {
    check: verify::verification::FocusedCheck,
    inputs: Result<Option<verify::inputs::Snapshot>, CheckInputError>,
    revision: verify::evidence::Revision,
    started: Instant,
}

/// Bounded verification processes started together (`experimental.background_check_slots`),
/// owned until every actual result is settled. The worker returns one output per run, in order.
pub(in crate::agent) struct PendingCheck {
    runs: Vec<PendingRun>,
    cancel: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<Vec<ToolOutput>>>,
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
        let mut runtime = None;
        checks
            .iter()
            .map(|check| {
                self.capture_check_inputs_sharing(check, &mut runtime)
                    .ok()
                    .flatten()
            })
            .collect()
    }

    pub(in crate::agent) fn reuse_check(
        &mut self,
        check: &verify::verification::FocusedCheck,
    ) -> bool {
        self.reuse_check_with_capture(check, None)
    }

    fn reuse_check_with_capture(
        &mut self,
        check: &verify::verification::FocusedCheck,
        capture: Option<verify::inputs::Snapshot>,
    ) -> bool {
        let started = Instant::now();
        let reused = self.reuse_check_inner(check, capture);
        self.efficiency_metrics
            .backend_operation("check_reuse", started.elapsed());
        reused
    }

    fn reuse_check_inner(
        &mut self,
        check: &verify::verification::FocusedCheck,
        capture: Option<verify::inputs::Snapshot>,
    ) -> bool {
        if self.interrupt.load(Ordering::SeqCst)
            || !self.step_checks.evidence.has_inputs(&check.command)
        {
            return false;
        }
        let Some(inputs) = capture.or_else(|| self.check_inputs(check)) else {
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
        if validation_launch_gap(call, output) {
            self.step_checks.validation_launch_gap = true;
            self.step_checks.eval_checks.forget_recovery();
            self.step_checks.shell_check_passed = None;
        }
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
        let Some(source) = classify_check_source(call, &self.config.verification.commands) else {
            return;
        };
        // Non-configured `bash check:true` commands are settled by `round/settle.rs` after
        // checking `exit_status_masked`; do not record pipe-masked ad-hoc commands as passes here.
        if source == CheckReceiptSource::ExplicitBashCheck && !is_check_command(command) {
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
        // Configured/focused receipts already considered the pipeline in shared classification.
        // For undeclared shell commands only this caller has the original command text.
        let verdict = if check.is_none() && verdict == verify::verification::CheckStatus::Passed {
            verify::verification::CheckStatus::from_shell_success(command)
        } else {
            verdict
        };
        // A display-only command, or one that excludes a test seen failing this turn, is not
        // evidence that the work is right.
        let verdict = if verdict == verify::verification::CheckStatus::Passed
            && source != CheckReceiptSource::FocusedCheck
            && uncounted_pass_note(
                command,
                &self.step_checks.failed_tests,
                self.session.control_state()["current_instruction"]
                    .as_str()
                    .unwrap_or_default(),
            )
            .is_some()
        {
            verify::verification::CheckStatus::Unverified
        } else {
            verdict
        };
        let exit = verdict.repair_exit_code(output);
        let passed = verdict == crate::agent::verify::verification::CheckStatus::Passed;
        if passed {
            ui.check_passed_with_delivery(&|| {
                crate::agent::workflow::working_state::open_deliverables(
                    &self.session.control_state()["working_state"],
                )
                .len()
            });
        }
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
        self.record_repair_link(command, &call.id, verdict, ui);
        let evidence_ref = self.retain_check_log(output);
        self.step_checks
            .evidence
            .bind_evidence_ref(command, evidence_ref);
        let text = verify::verification::classification_text(output, self.workspace.artifacts());
        self.step_checks
            .evidence
            .diagnose(command, text.as_deref().unwrap_or_default());
        if verdict.repairable()
            && self.local_profile_active()
            && !self
                .step_checks
                .evidence
                .unchanged_unrelated_failure(command)
        {
            self.step_checks.repair_brief = Some(RepairBrief::new(
                command,
                text.as_deref().unwrap_or_default(),
            ));
        }
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

    /// Off-prompt structured evidence linking an observed code failure to the same check's later
    /// verdict. The failure event is written when observed, so an unresolved failure stays unknown.
    /// Reused checks never reach this path, so reuse cannot resolve a failure. Commands are redacted
    /// and bounded before they leave the controller.
    fn record_repair_link(
        &mut self,
        command: &str,
        call_id: &str,
        verdict: verify::verification::CheckStatus,
        ui: &mut dyn Ui,
    ) {
        let revision = self.step_checks.revision();
        let step = self.cur_step;
        let shown = truncate(&crate::storage::redact::redact_text(command), 200);
        if let Some(mut linked) = self
            .step_checks
            .evidence
            .link_verdict(command, call_id, verdict, revision)
        {
            linked.command = shown.clone();
            if let Err(error) = self.record(step, "check_repair_verdict", json!(linked)) {
                ui.notice(&format!("check repair verdict was not recorded: {error:#}"));
            }
        }
        if verdict == verify::verification::CheckStatus::CodeFailed
            && let Err(error) = self.record(
                step,
                "check_failure_observed",
                json!({"command":shown,"call_id":call_id,"revision":revision}),
            )
        {
            ui.notice(&format!("check failure was not recorded: {error:#}"));
        }
    }

    /// Display and authorize each check on the controller. Group admission is never authority.
    fn focused_check_refusal(
        &mut self,
        check: &verify::verification::FocusedCheck,
        call: &ToolCall,
        ui: &mut dyn Ui,
    ) -> Option<ToolOutput> {
        // Long generated commands (an interpreter path plus inline code) say nothing the check name does not.
        let title = if check.command.len() <= 80 {
            format!("Verify {}: {}", check.name, check.command)
        } else {
            format!("Verify {}", check.name)
        };
        let mut display = call.arguments.clone();
        display["_dial_check"] = json!(check.name);
        ui.tool_call_with_args(&call.id, "bash", &title, &display);
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
        match gate {
            Ok(persist) => {
                if persist && let Err(error) = self.persist_latest_permission_rule() {
                    ui.notice(&format!(
                        "The rule applies to this session but could not be saved: {error:#}"
                    ));
                }
                if (repro || model_registered)
                    && let Some(reason) = self.repro_hook_refusal(&call.arguments, ui)
                {
                    ui.notice(&reason);
                    Some(
                        ToolOutput::err(format!(
                            "automatic check {} not run: {reason}",
                            check.name
                        ))
                        .with_status(crate::tools::ToolStatus::Blocked),
                    )
                } else {
                    if (repro || model_registered)
                        && !crate::agent::safety::approval::safe_bash(&check.command)
                        && !crate::agent::safety::approval::safe_review_batch(&check.command)
                    {
                        self.step_checks.note_repro_run();
                    }
                    None
                }
            }
            Err(denial) => {
                ui.notice(denial.message());
                Some(crate::agent::safety::approval::denied_check_output(
                    &check.name,
                    &denial,
                ))
            }
        }
    }

    pub(in crate::agent) fn run_focused_check(
        &mut self,
        check: &verify::verification::FocusedCheck,
        ui: &mut dyn Ui,
    ) -> ToolOutput {
        self.drain_background_check(ui, true);
        let call = focused_check_call(check);
        let inputs_before = self.capture_check_inputs(check);
        let revision_before = self.step_checks.revision();
        let coverage = (self.config.experimental.changed_line_coverage
            && self.step_checks.mutations > 0
            && self.behavioral_check(&check.name)
            && !self.step_checks.evidence.is_repro(&check.command))
        .then(|| self.turn_versions_now())
        .flatten()
        .and_then(|versions| verify::coverage::prepare(self.workspace.root(), &versions))
        .and_then(|probe| {
            let wrapped = probe.wrap(&self.workspace, &check.command)?;
            Some((
                probe,
                json!({"command":wrapped,"timeout":check.timeout_secs}),
            ))
        });
        let refusal = self.focused_check_refusal(check, &call, ui);
        let started = Instant::now();
        // Collectors are injected through the environment; evidence keeps the declared command.
        let args = coverage.as_ref().map_or(&call.arguments, |(_, args)| args);
        let output = refusal.unwrap_or_else(|| {
            self.run_foreground_tool("bash", args, ui)
                .unwrap_or_else(|| {
                    tools::execute(&self.workspace, "bash", args, check.timeout_secs)
                })
        });
        let coverage_line = coverage
            .as_ref()
            .and_then(|(probe, _)| probe.report(&check.name));
        drop(coverage);
        let mut output =
            self.settle_focused_check(check, inputs_before, revision_before, started, output, ui);
        if let Some(line) = coverage_line
            && self.step_checks.evidence.latest_status(&check.command)
                == Some(verify::verification::CheckStatus::Passed)
        {
            output.text.push('\n');
            output.text.push_str(&line);
        }
        output
    }

    /// Run a bounded consecutive prefix of independently admitted inspections. Input contracts
    /// describe reads, not side effects: even two disjoint contracts do not make arbitrary test
    /// commands compatible. Unknown shell effects, probes and registered commands stay serial.
    pub(super) fn run_check_prefix(
        &mut self,
        checks: &[verify::verification::FocusedCheck],
        allow_reuse: bool,
        ui: &mut dyn Ui,
    ) -> VecDeque<ToolOutput> {
        let mut results = VecDeque::new();
        if self.hermetic
            || self.plan_mode
            || self.config.experimental.changed_line_coverage
            || checks.len() < 2
        {
            return results;
        }
        self.drain_background_check(ui, true);
        let mut candidates: Vec<(&verify::verification::FocusedCheck, ToolCall)> = Vec::new();
        for check in checks.iter().take(
            self.effective_config()
                .task
                .parallel_read_limit()
                .min(MAX_FOREGROUND_CHECKS),
        ) {
            if self.interrupt.load(Ordering::SeqCst)
                || self.step_checks.evidence.is_repro(&check.command)
                || self
                    .step_checks
                    .registered_acceptance
                    .iter()
                    .any(|prior| prior.command == check.command)
                || (allow_reuse && self.step_checks.evidence.has_inputs(&check.command))
                || !self.config.verification.inputs.contains_key(&check.name)
                || candidates
                    .iter()
                    .any(|(prior, _)| prior.name == check.name || prior.command == check.command)
            {
                break;
            }
            let mut call = focused_check_call(check);
            // Explicit zero-only exit semantics preserve the normal verdict and tell the bash
            // runner not to hand a still-running inspection to bg on timeout or steering.
            call.arguments["expected_exit_codes"] = json!([0]);
            if crate::agent::run::dependency_dispatch::footprint_for(
                &self.workspace,
                self.edit_mode(),
                &self.ext,
                &call,
            )
            .is_none_or(|footprint| footprint.writes())
            {
                break;
            }
            candidates.push((check, call));
        }
        if candidates.len() < 2 {
            return results;
        }
        let candidates: Vec<_> = candidates
            .into_iter()
            .map_while(|(check, call)| {
                self.capture_check_inputs(check)
                    .ok()
                    .flatten()
                    .map(|inputs| (check, call, inputs))
            })
            .collect();
        if candidates.len() < 2 {
            return results;
        }
        let revision = self.step_checks.revision();
        let mut calls = Vec::new();
        let mut entries = Vec::new();
        for (check, call, inputs) in candidates {
            let refused = if self.interrupt.load(Ordering::SeqCst) {
                Some(crate::agent::run::dispatch::interrupted_tool())
            } else {
                self.focused_check_refusal(check, &call, ui)
            };
            if refused.is_none() {
                calls.push(call);
            }
            entries.push((check, inputs, Instant::now(), refused));
        }
        let mut ran = if calls.is_empty() {
            Vec::new()
        } else {
            self.run_background_tools(&calls, ui, None, None)
                .unwrap_or_default()
        }
        .into_iter();
        for (check, inputs, started, refused) in entries {
            let output = refused
                .or_else(|| ran.next().map(|(output, _)| output))
                .unwrap_or_else(|| {
                    ToolOutput::err("focused verification worker returned no completion")
                });
            results.push_back(self.settle_focused_check(
                check,
                Ok(Some(inputs)),
                revision,
                started,
                output,
                ui,
            ));
        }
        results
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
        let mut outputs = pending
            .worker
            .take()
            .expect("pending check owns worker")
            .join()
            .unwrap_or_default()
            .into_iter();
        for run in std::mem::take(&mut pending.runs) {
            // A run without an output never completed; it earns no evidence.
            let output = outputs.next().unwrap_or_else(|| {
                ToolOutput::err("focused verification worker panicked; no completion observed")
            });
            self.settle_focused_check(
                &run.check,
                run.inputs,
                run.revision,
                run.started,
                output,
                ui,
            );
        }
    }

    /// Start only operator-configured checks with complete input contracts: up to
    /// `experimental.background_check_slots` of them, run side by side inside one workspace
    /// serial-operation boundary. They remain ordinary shell authority, not permission grants.
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
        let slots = usize::from(self.config.experimental.background_check_slots.max(1));
        let candidates: Vec<_> = self
            .current_focused_checks()
            .into_iter()
            .filter(|check| {
                self.config
                    .verification
                    .commands
                    .iter()
                    .any(|custom| custom.name == check.name && custom.command == check.command)
            })
            .filter_map(|check| {
                let inputs = self.capture_check_inputs(&check).ok().flatten()?;
                let reused = self.reuse_check_with_capture(&check, Some(inputs.clone()));
                (!reused).then_some((check, inputs))
            })
            .take(slots)
            .collect();
        let mut runs = Vec::new();
        for (check, inputs) in candidates {
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
                    self.settle_focused_check(
                        &check,
                        Ok(Some(inputs)),
                        revision,
                        started,
                        output,
                        ui,
                    );
                    continue;
                }
                Ok(persist) => {
                    if persist && let Err(error) = self.persist_latest_permission_rule() {
                        ui.notice(&format!(
                            "The rule applies to this session but could not be saved: {error:#}"
                        ));
                    }
                }
            }
            runs.push(PendingRun {
                check,
                inputs: Ok(Some(inputs)),
                revision,
                started,
            });
        }
        if runs.is_empty() {
            return;
        }
        let workspace = self.workspace.clone();
        let mut config = self.effective_config().clone();
        // Verification owns the actual process to completion, not a detached shell ticket.
        // This is an execution-local clone; operator settings and ordinary bash are unchanged.
        config.shell.auto_background_secs = 0;
        let registry = self.ext.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let commands: Vec<Value> = runs
            .iter()
            .map(|run| json!({"command":run.check.command,"timeout":run.check.timeout_secs}))
            .collect();
        let worker = std::thread::Builder::new()
            .name("dial-focused-check".into())
            .spawn(move || {
                let check = |args: &Value| {
                    crate::tools::exec::execution::run(&worker_cancel, || {
                        if worker_cancel.load(Ordering::SeqCst) {
                            return ToolOutput::err(
                                "focused verification cancelled before execution",
                            );
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
                            args,
                            &|_| {},
                            &worker_cancel,
                        )
                        .expect("bash is a native tool")
                    })
                };
                // The checks share one serial-operation boundary: no other workspace operation
                // runs beside them, only each other.
                let _serial = workspace.serial_operation();
                if let [args] = commands.as_slice() {
                    return vec![check(args)];
                }
                std::thread::scope(|scope| {
                    let handles: Vec<_> = commands
                        .iter()
                        .map(|args| scope.spawn(|| check(args)))
                        .collect();
                    handles
                        .into_iter()
                        .map(|handle| {
                            handle.join().unwrap_or_else(|_| {
                                ToolOutput::err(
                                    "focused verification worker panicked; no completion observed",
                                )
                            })
                        })
                        .collect()
                })
            });
        match worker {
            Ok(worker) => {
                self.pending_check = Some(PendingCheck {
                    runs,
                    cancel,
                    worker: Some(worker),
                })
            }
            Err(error) => {
                for run in runs {
                    self.settle_focused_check(
                        &run.check,
                        run.inputs,
                        run.revision,
                        run.started,
                        ToolOutput::err(format!("starting focused verification: {error}")),
                        ui,
                    );
                }
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
    /// a missing interpreter must not read as broken code. Dial's managed interpreter is used only
    /// when sandboxed commands can launch it: `--sandbox dev` hides protected Dial storage.
    pub(in crate::agent) fn check_python(&self) -> PathBuf {
        if self.config.python.is_file() && !self.workspace.sandbox_hides(&self.config.python) {
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
        self.publish_edit_loop_results(ui, None);
    }

    pub(super) fn publish_edit_loop_results(&mut self, ui: &mut dyn Ui, results: Option<&[Value]>) {
        if self.hermetic {
            return;
        }
        let mut control = self.session.control_state();
        control["reliability"]["edit_loop"] = self.step_checks.status();
        if let Some(results) = results {
            control["reliability"]["verification_results"] = json!(results);
        }
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
        let mut grouped = VecDeque::new();
        for (index, check) in checks.iter().enumerate() {
            if grouped.is_empty() && self.interrupt.load(Ordering::SeqCst) {
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
            if grouped.is_empty() && self.reuse_check(check) {
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
            if grouped.is_empty() {
                grouped = self.run_check_prefix(&checks[index..], true, ui);
            }
            let output = grouped
                .pop_front()
                .unwrap_or_else(|| self.run_focused_check(check, ui));
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
        let control = self.session.control_state();
        crate::agent::workflow::task_ledger::TaskLedger::rejection_outcome(&self.todo, &control)
    }

    /// Side-effect-free completion blockers shared by `finish completed`, plain completion and
    /// `goal complete`: explicit blockers (their recorded reason is kept), open execution steps,
    /// running shell or worker jobs, unreadable worker state, and failed checks with no edit
    /// awaiting a fresh check. Callers decide whether to pump workers or disclose changes.
    pub(in crate::agent) fn completion_blockers(&self) -> Result<(), String> {
        let control = self.session.control_state();
        let working = &control["working_state"];
        if let Some(blocker) = crate::agent::workflow::task_ledger::TaskLedger::completion_blocker(
            &self.todo, &control,
        ) {
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
            return Err("cannot complete while side questions are running; use `task` action=question_wait or question_cancel and collect their results".into());
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
        // An unattended run kills its session jobs on exit. Accepting completion while one runs
        // would let the answer describe a service that is gone before anyone can use it.
        let exit_bound = self.exit_bound_session_jobs();
        if !exit_bound.is_empty() {
            return Err(format!(
                "cannot complete while session jobs are running in an unattended run:\n{}\nDial kills these jobs when this run exits. If the task needs one afterwards (a server or service), stop it with `bg kill`, start it again with `bg` action=start persist=true name=<name> (without `ready`), and confirm it serves; otherwise stop it with `bg kill`. Then try again.",
                exit_bound.join("\n")
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
        // automatic post-turn verification pass reruns together with the retained failures. Only that pass
        // may defer a failure; without it nothing reruns the check, so the failure blocks until it passes.
        let unchecked_effects = self.step_checks.mutations > self.step_checks.checked
            || self.step_checks.uncheckpointed_shell_commands
                > self.step_checks.checked_shell_commands
            || self.step_checks.uncheckpointed_extension_calls
                > self.step_checks.checked_extension_calls;
        let rechecked_after_turn = self.config.verification.enabled
            && self.config.verification.automatic_checks
            && unchecked_effects;
        // Failures that say nothing about this turn's code (see `disclosed_failure`) are disclosed
        // with the accepted answer; any other current failure still refuses completion.
        let diagnosis = self.diagnosis_requested();
        if !self.hermetic && self.step_checks.failed && !rechecked_after_turn {
            let blocking = self
                .step_checks
                .failed_checks
                .iter()
                .filter(|check| self.disclosed_failure(check, diagnosis).is_none())
                .map(|check| check.name.as_str())
                .collect::<Vec<_>>();
            if !blocking.is_empty() {
                let targets = &blocking[..blocking
                    .len()
                    .min(crate::tools::edit::editing::MAX_VERIFY_TARGETS)];
                let recovery = json!({"targets": targets});
                return Err(format!(
                    "Cannot complete: focused checks failed and have not passed since. After repairing the cause, start with the retained checks below; additional failures may remain.\nRecovery call: verify {recovery}\nBlocking checks: {}. A check registered under another name does not discharge them. Do not replace a valid check with a weaker one; report a blocker if it cannot be rerun.",
                    blocking.join(", ")
                ));
            }
        }
        if !self.hermetic {
            let open = self
                .step_checks
                .failed_runs
                .unresolved(self.workspace.root());
            if !open.is_empty() {
                let lines = open
                    .iter()
                    .map(|(path, run)| {
                        format!("- {path}: {} failed with {}", run.consumer, run.summary)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                return Err(format!(
                    "Cannot complete: a file changed this turn failed to parse or compile for one of its consumers, and since then the file has not changed and that consumer has not run it successfully:\n{lines}\nA different consumer passing (for example compiling it another way) does not cover this failure. Fix the file and rerun the same consumer; if it cannot be fixed, finish as blocked and name the failure."
                ));
            }
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
    pub(in crate::agent) fn diagnosis_requested(&self) -> bool {
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

    /// When no normal test applies, require evidence for a failed validation launch or a newly
    /// generated standalone output. Ordinary source edits do not invent test obligations.
    pub(in crate::agent) fn artifact_check_reminder(&self) -> Option<&'static str> {
        self.unchecked_artifacts().map(|(_, reminder)| reminder)
    }

    /// Immediate feedback for a passing Eval cell whose assertions still leave the pending
    /// artifacts without a source binding, so recovery happens before the completion gate.
    pub(in crate::agent) fn unbound_eval_check_note(&self) -> Option<String> {
        let (paths, _) = self.unchecked_artifacts()?;
        let shown = paths.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
        let more = paths.len().saturating_sub(4);
        let more = if more == 0 {
            String::new()
        } else {
            format!(" and {more} more")
        };
        Some(format!(
            "[Unbound Eval checks] These passing assertions do not verify the edited file(s): {shown}{more}. In one cell, read each with src = dial.read(path), test that saved code, and pass src as dial.check's third argument."
        ))
    }

    fn unchecked_artifacts(&self) -> Option<(Vec<String>, &'static str)> {
        if !self.config.verification.enabled
            || self.hermetic
            || self.plan_mode
            || self.step_checks.mutations == 0
        {
            return None;
        }
        let changed = self.step_checks.turn.zip(self.checkpoints.as_ref());
        let (paths, reminder) = if self.step_checks.validation_launch_gap {
            (
                changed
                    .map(|(turn, cp)| cp.changed_files(turn))
                    .unwrap_or_default(),
                super::completion::UNCHECKED_AFTER_LAUNCH_GAP,
            )
        } else {
            let (turn, cp) = changed?;
            let paths: Vec<_> = cp
                .created_files(turn)
                .into_iter()
                .filter(|p| {
                    matches!(
                        Path::new(p).extension().and_then(|s| s.to_str()),
                        Some(
                            "regex"
                                | "regexp"
                                | "csv"
                                | "tsv"
                                | "json"
                                | "jsonl"
                                | "sh"
                                | "py"
                                | "js"
                        )
                    )
                })
                .collect();
            if paths.is_empty() {
                return None;
            }
            (
                paths,
                "New standalone output has no observed check. Run its intended consumer on the saved file (verify acceptance or bash check:true), or bind an Eval assertion to each saved output with a whole-file dial.read receipt. Report any unverified consumer.",
            )
        };
        let revision = self.step_checks.revision();
        (self.step_checks.shell_check_passed != Some(revision)
            && self.step_checks.evidence.passing_behavioral(revision) == 0
            && self.current_focused_checks().is_empty()
            && (self.step_checks.effects_unknown
                || !self
                    .step_checks
                    .eval_checks
                    .covers_artifacts(&self.workspace, &paths)))
        .then_some((paths, reminder))
    }

    /// An unattended delivery after a recorded effect needs current verification evidence.
    /// Read-only turns have no such obligation; request classification cannot waive one.
    pub(in crate::agent) fn unattended_verification_gap(&self) -> Option<&'static str> {
        if !self.modes.launch.unattended
            || !self.config.verification.enabled
            || self.hermetic
            || self.plan_mode
            || self.step_checks.effect_epoch() == 0
        {
            return None;
        }
        let revision = self.step_checks.revision();
        // An Eval receipt counts only when it is bound to a file this turn changed; a passing
        // assertion about an unrelated input file is not evidence for the delivered work.
        let eval_pass = match self.step_checks.turn.zip(self.checkpoints.as_ref()) {
            Some((turn, checkpoints)) => {
                let changed = checkpoints.changed_files(turn);
                if changed.is_empty() {
                    self.step_checks.eval_checks.current_pass(&self.workspace)
                } else {
                    self.step_checks
                        .eval_checks
                        .current_pass_for(&self.workspace, &changed)
                }
            }
            None => self.step_checks.eval_checks.current_pass(&self.workspace),
        };
        let verified = self.step_checks.shell_check_passed == Some(revision)
            || self
                .step_checks
                .evidence
                .current_focused_behavior_pass(revision)
            || eval_pass;
        (!verified).then_some(UNATTENDED_UNVERIFIED)
    }

    /// The values of a passing self-confirming shell check, when it is the only current evidence
    /// of an unattended run.
    pub(in crate::agent) fn self_confirming_gap(&self) -> Option<String> {
        if !self.modes.launch.unattended || self.hermetic || self.plan_mode {
            return None;
        }
        let revision = self.step_checks.revision();
        let (at, values) = self.step_checks.self_confirming_check.as_ref()?;
        (*at == revision
            && self.step_checks.shell_check_passed == Some(revision)
            && !self
                .step_checks
                .evidence
                .current_focused_behavior_pass(revision))
        .then(|| self_confirming_reason(values))
    }

    /// The functions the request names, once per turn in an unattended run with effects: the
    /// model's own harness calls them its own way, which can differ from the request's caller.
    pub(in crate::agent) fn caller_interface_gap(&self) -> Option<String> {
        if !self.modes.launch.unattended
            || self.hermetic
            || self.plan_mode
            || self.step_checks.effect_epoch() == 0
        {
            return None;
        }
        let control = self.session.control_state();
        let instruction = control["current_instruction"].as_str().unwrap_or_default();
        let names = requested_functions(instruction);
        (!names.is_empty()).then(|| caller_interface_reason(&names))
    }

    /// Once per turn in an unattended run with effects, when the request asks for speed: output
    /// equality, the usual self-check, says nothing about it.
    pub(in crate::agent) fn performance_gap(&self) -> Option<String> {
        if !self.modes.launch.unattended
            || self.hermetic
            || self.plan_mode
            || self.step_checks.effect_epoch() == 0
        {
            return None;
        }
        let control = self.session.control_state();
        let instruction = control["current_instruction"].as_str().unwrap_or_default();
        requested_speed(instruction).map(performance_reason)
    }

    /// Once per turn in an unattended run with effects, when the request names a tool as the
    /// ground truth and no passing check ran it: a value computed another way can differ.
    pub(in crate::agent) fn reference_tool_gap(&self) -> Option<String> {
        if !self.modes.launch.unattended
            || self.hermetic
            || self.plan_mode
            || self.step_checks.effect_epoch() == 0
        {
            return None;
        }
        let control = self.session.control_state();
        let instruction = control["current_instruction"].as_str().unwrap_or_default();
        let unrun: Vec<String> = reference_tools(instruction)
            .into_iter()
            .filter(|tool| {
                !self
                    .step_checks
                    .shell_check_runs
                    .iter()
                    .any(|(command, passed, _)| *passed && names_word(command, tool))
            })
            .collect();
        (!unrun.is_empty()).then(|| reference_tool_reason(&unrun))
    }

    /// Describe each applicable check from one selection and one refreshed evidence snapshot.
    fn current_gate_outcome(&mut self) -> GateOutcome {
        let mut gate = GateOutcome::default();
        if !self.unverified_edits_apply() {
            return gate;
        }
        self.refresh_check_inputs();
        let revision = self.step_checks.revision();
        let diagnosis = self.diagnosis_requested();
        for check in self.current_focused_checks() {
            if self
                .step_checks
                .evidence
                .current_status(&check.command, revision)
                == "passed"
            {
                gate.has_pass = true;
            } else if self.disclosed_failure(&check, diagnosis).is_none() {
                gate.blocking |= self
                    .step_checks
                    .failed_checks
                    .iter()
                    .any(|failed| failed.command == check.command);
                gate.unverified
                    .get_or_insert_with(Vec::new)
                    .push(check.name);
            }
        }
        gate
    }

    /// The completion gate's evidence question. `unverified` is the still-unverified set after any
    /// run this call made; `blocking` is an undisclosed failure on the current revision, which keeps
    /// the answer blocked even when another applicable check passed.
    pub(in crate::agent) fn gate_unverified_edits(
        &mut self,
        first_attempt: bool,
        ui: &mut dyn Ui,
    ) -> GateOutcome {
        let gate = self.current_gate_outcome();
        if gate.unverified.is_none() || self.interrupt.load(Ordering::SeqCst) {
            return gate;
        }
        let mut checks = self.current_focused_checks();
        if !first_attempt {
            // The model may have spent its reminder on an open task, before we could run any
            // checks. Run missing checks and refresh stale passes, but do not retry failures,
            // refusals or cancellations just because the model tried to finish again.
            let evidence = &self.step_checks.evidence;
            let revision = self.step_checks.revision();
            checks.retain(|check| match evidence.latest_status(&check.command) {
                None => true,
                Some(verify::verification::CheckStatus::Passed) => {
                    evidence.current_status(&check.command, revision) == "stale"
                }
                _ => false,
            });
            if checks.is_empty() {
                return gate;
            }
        }
        let failures = self.run_checks(&checks, ui).failures;
        let mut gate = self.current_gate_outcome();
        if gate.blocking {
            gate.failures = failures;
        }
        gate
    }

    /// One completion boundary for plain reports and explicit finish calls, on every route. Pumps
    /// worker results first, so a worker that already finished is not reported as running.
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
        // `finish` is refused once while an unattended run would silently kill a session service.
        if let Some(notice) = self.exit_bound_jobs_notice() {
            return Err(notice);
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
            "artifact_local_checks": self.step_checks.eval_checks.snapshot(&self.workspace),
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

/// Shared by the completed and answered delivery paths.
pub(in crate::agent) const UNATTENDED_UNVERIFIED: &str = "This unattended run recorded workspace or environment effects, but no declared check has passed on the current state. Verify the deliverable with a configured check, `verify`, `bash` with check:true, or a file-bound Eval assertion that fails when the content is wrong; showing or listing the file does not count. Report a concrete blocker if verification cannot proceed.";

/// Told once per unattended turn, on the first tool result after which verification is owed.
pub(in crate::agent) const UNATTENDED_CHECK_HINT: &str = "[Dial] This run is unattended: finishing is accepted only after a check passes on the final state. When you verify the result, run that command as a foreground `bash` call with check:true (no pipe, or start it with `set -o pipefail;`), written to exit non-zero when the deliverable is wrong; a separate display or listing does not count.";

/// Why a zero-exit check is not counted, or `None` when it is real evidence: it only displays or
/// lists files, or it excludes a test seen failing earlier in this turn (and not named by the
/// request). The returned note is appended to the check's result.
pub(in crate::agent) fn uncounted_pass_note(
    command: &str,
    failed_tests: &std::collections::BTreeSet<String>,
    instruction: &str,
) -> Option<String> {
    if display_only(command) {
        return Some(DISPLAY_ONLY_CHECK.to_owned());
    }
    let excluded: Vec<String> = excluded_tests(command)
        .into_iter()
        .filter(|name| {
            let last = name.rsplit("::").next().unwrap_or(name);
            !instruction.contains(last) && seen_failing(name, failed_tests)
        })
        .collect();
    (!excluded.is_empty()).then(|| {
        format!(
            "\n[Dial check] Not counted as a passing check: it excludes {}, which failed earlier in this turn. Excluding a failing test hides the failure rather than fixing it. Fix the cause and run the check with that test included, or report it as a remaining failure.",
            excluded.join(", ")
        )
    })
}

const DISPLAY_ONLY_CHECK: &str = "\n[Dial check] Not counted as a passing check: this command only displays or lists files, so it shows the deliverable exists, not that it is right. Check the content with a command that fails when it is wrong, such as a test, an assertion or a computation that derives the expected result independently.";

/// Commands that only show files or text and cannot fail on wrong content.
const DISPLAY_PROGRAMS: &[&str] = &[
    "cat", "head", "tail", "ls", "wc", "stat", "file", "echo", "printf", "pwd", "du", "xxd", "od",
    "hexdump", "tree", "realpath", "readlink", "true", "less", "more", "nl",
];

/// Whether every stage of `command` is a display command (after a leading `cd DIR`), with no
/// substitution or comparison that could make it fail on content.
fn display_only(command: &str) -> bool {
    if command.contains("$(") || command.contains('`') || command.trim().is_empty() {
        return false;
    }
    let mut stages = command
        .split(['\n', ';', '|', '&'])
        .map(str::trim)
        .filter(|stage| !stage.is_empty() && !stage.starts_with('#'))
        .peekable();
    if stages.peek().is_some_and(|first| first.starts_with("cd ")) {
        stages.next();
    }
    let mut any = false;
    for stage in stages {
        let program = stage
            .split_whitespace()
            .find(|word| !word.contains('='))
            .unwrap_or_default();
        let program = program.rsplit('/').next().unwrap_or(program);
        // `2>&1` splits into a `1` stage.
        if program.chars().all(|c| c.is_ascii_digit() || c == '>') {
            continue;
        }
        if !DISPLAY_PROGRAMS.contains(&program) {
            return false;
        }
        any = true;
    }
    any
}

/// Shell words of `command`, honoring single and double quotes.
fn words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut started = false;
    for c in command.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                started = true;
            }
            None if c.is_whitespace() => {
                if started || !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
                started = false;
            }
            None => current.push(c),
        }
    }
    if started || !current.is_empty() {
        words.push(current);
    }
    words
}

/// Tests a check command leaves out: pytest `--deselect ID` and `-k "not NAME"`, libtest
/// `--skip NAME`.
fn excluded_tests(command: &str) -> Vec<String> {
    let words = words(command);
    let mut excluded = Vec::new();
    let mut index = 0;
    while index < words.len() {
        let word = words[index].as_str();
        let (flag, inline) = match word.split_once('=') {
            Some((flag, value)) if flag.starts_with('-') => (flag, Some(value.to_owned())),
            _ => (word, None),
        };
        if matches!(flag, "--deselect" | "--skip" | "-k") {
            let value = inline.or_else(|| {
                index += 1;
                words.get(index).cloned()
            });
            if let Some(value) = value {
                if flag == "-k" {
                    let tokens: Vec<&str> = value
                        .split(|c: char| c.is_whitespace() || matches!(c, '(' | ')'))
                        .filter(|token| !token.is_empty())
                        .collect();
                    excluded.extend(
                        tokens
                            .windows(2)
                            .filter(|pair| pair[0] == "not")
                            .map(|pair| pair[1].to_owned()),
                    );
                } else {
                    excluded.push(value);
                }
            }
        }
        index += 1;
    }
    excluded
}

/// Whether `name` (a test id, a name or a `-k` substring) matches a test that failed this turn.
fn seen_failing(name: &str, failed: &std::collections::BTreeSet<String>) -> bool {
    let last = name.rsplit("::").next().unwrap_or(name);
    failed.iter().any(|test| {
        test == name
            || test.rsplit("::").next() == Some(last)
            || (last.len() >= 4 && test.contains(last))
    })
}

/// Distinctive numbers in `text`: digit runs of two or more digits, or decimals, standing alone
/// (not part of a name such as `t2.py` or `v1_2`).
pub(in crate::agent) fn distinctive_numbers(text: &str) -> Vec<(usize, String)> {
    let bytes = text.as_bytes();
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut found = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if !bytes[at].is_ascii_digit() || (at > 0 && (word(bytes[at - 1]) || bytes[at - 1] == b'.'))
        {
            at += 1;
            continue;
        }
        let start = at;
        while at < bytes.len() && (bytes[at].is_ascii_digit() || bytes[at] == b'.') {
            at += 1;
        }
        let token = text[start..at].trim_end_matches('.');
        let standalone = bytes.get(at).is_none_or(|&b| !word(b));
        let decimal = token.contains('.') && token.split('.').count() == 2;
        if standalone && (decimal || (!token.contains('.') && token.len() >= 2)) {
            found.push((start, token.to_owned()));
        }
    }
    found
}

/// Numbers a check compares against: distinctive literals right after `=`, `==`, `<`, `>` or a
/// `test` comparison (`-eq` …), outside `--option=value` arguments.
fn expected_literals(command: &str) -> Vec<String> {
    let mut values: Vec<String> = distinctive_numbers(command)
        .into_iter()
        .filter(|(start, _)| {
            let before = command[..*start].trim_end_matches([' ', '\t', '\'', '"', '(', '[']);
            let last = before
                .rsplit(char::is_whitespace)
                .next()
                .unwrap_or_default();
            let option = last.starts_with('-') && last.contains('=');
            !option
                && (before.ends_with(['=', '<', '>'])
                    || ["-eq", "-ne", "-gt", "-ge", "-lt", "-le"].contains(&last))
        })
        .map(|(_, value)| value)
        .collect();
    values.dedup();
    values
}

/// Short, standalone output values used by literal file snapshot checks.
pub(in crate::agent) fn snapshot_words(text: &str) -> impl Iterator<Item = &str> {
    text.lines().map(str::trim).filter(|line| {
        (2..=64).contains(&line.len())
            && line.as_bytes()[0].is_ascii_alphabetic()
            && line
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}

/// Recognize a literal string comparison against a file read, not a general shell assertion.
fn snapshot_word(command: &str) -> Option<&str> {
    static SNAPSHOT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"^test\s+"\$\(cat\s+[/A-Za-z0-9_.-]+\)"\s+==?\s+(?:"([A-Za-z][A-Za-z0-9_-]{1,63})"|'([A-Za-z][A-Za-z0-9_-]{1,63})'|([A-Za-z][A-Za-z0-9_-]{1,63}))$"#).expect("valid snapshot pattern")
    });
    let captures = SNAPSHOT.captures(command.trim())?;
    (1..=3).find_map(|index| captures.get(index).map(|value| value.as_str()))
}

/// A literal whole-line match establishes membership, not completeness.
fn single_line_membership(command: &str, instruction: &str) -> Option<String> {
    static MEMBERSHIP: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r#"^grep[ \t]+-(?:qx|xq|Fqx|qFx|qxF)[ \t]+(?:"([A-Za-z0-9_][A-Za-z0-9_-]{0,63})"|'([A-Za-z0-9_][A-Za-z0-9_-]{0,63})'|([A-Za-z0-9_][A-Za-z0-9_-]{0,63}))[ \t]+[/A-Za-z0-9_.-]+$"#,
        )
        .expect("valid membership pattern")
    });
    static EXHAUSTIVE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)\b(?:all\s+(?:qualifying|matching|valid|winning)|every\s+(?:qualifying|matching|valid|winning)|(?:print|list|write|return|include)\s+them\s+all)\b")
            .expect("valid exhaustive request pattern")
    });
    if !EXHAUSTIVE.is_match(instruction) {
        return None;
    }
    let captures = MEMBERSHIP.captures(command.trim())?;
    (1..=3).find_map(|index| captures.get(index).map(|value| value.as_str().to_owned()))
}

/// Expected values from a membership-only check on an exhaustive request, or a snapshot of
/// values the run printed earlier and the user did not supply.
pub(in crate::agent) fn self_confirming_values(
    command: &str,
    printed: &std::collections::BTreeSet<String>,
    instruction: &str,
) -> Option<Vec<String>> {
    if let Some(word) = single_line_membership(command, instruction) {
        return Some(vec![word]);
    }
    if let Some(word) = snapshot_word(command)
        && printed.contains(word)
        && !instruction
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
            .any(|token| token == word)
    {
        return Some(vec![word.to_owned()]);
    }
    let expected = expected_literals(command);
    let requested: Vec<String> = distinctive_numbers(instruction)
        .into_iter()
        .map(|(_, value)| value)
        .collect();
    (!expected.is_empty()
        && expected
            .iter()
            .all(|value| printed.contains(value) && !requested.contains(value)))
    .then_some(expected)
}

/// Told once, in an unattended run, when the only passing check is self-confirming.
pub(in crate::agent) fn self_confirming_reason(values: &[String]) -> String {
    format!(
        "The passing check only tests for specific values ({}); it does not independently establish correctness. Before finishing, compare the deliverable with independent evidence: the input itself, the task's own examples or tests, a worked case, or a second method. If the request asks for all qualifying results, independently enumerate candidates and compare the full set to the saved output. If correctness is already established, finish again.",
        values.join(", ")
    )
}

/// Told once, in an unattended run, when the request names functions: a harness the model wrote
/// itself calls them its own way, which can differ from how the request's caller will.
pub(in crate::agent) fn caller_interface_reason(names: &[String]) -> String {
    let list = names
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "The request names {list}. Its caller will use only what the request says, and will usually pass the leading arguments by position. Before finishing, write down the call a reader of the request would most likely make: the input the request calls primary as the first positional argument, then the other inputs in the order the request introduces them, each in the request's terms (a range or domain it describes as one thing is one argument, such as a two-element vector, not two). Run exactly that call once in a fresh process, outside your own test helpers, and make it fail if the result is wrong. If it is refused or misread, change the signature so it works (the primary input first; accept a range both as one value and as separate bounds), then finish again. If this is already established, finish again."
    )
}

/// Tools the request names as the ground truth or reference (`primer3's oligotm tool should be
/// considered the ground truth`, `` `jq` is the reference ``): in a sentence that says so, the word
/// before `tool`, `program`, `command`, `binary` or `utility`, and single backticked command
/// names. Bounded to a few names.
pub(in crate::agent) fn reference_tools(instruction: &str) -> Vec<String> {
    let mut tools: Vec<String> = Vec::new();
    let mut add = |name: &str| {
        let name = name
            .trim_matches(|c: char| !c.is_ascii_alphanumeric())
            .trim_end_matches("'s");
        let plain = !name.is_empty()
            && name.len() <= 40
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && !matches!(name, "the" | "a" | "an" | "this" | "that" | "your" | "its");
        if plain && tools.len() < 3 && !tools.iter().any(|tool| tool == name) {
            tools.push(name.to_owned());
        }
    };
    for sentence in instruction
        .split(['\n', ';'])
        .flat_map(|line| line.split(". "))
    {
        let lower = sentence.to_ascii_lowercase();
        if ![
            "ground truth",
            "source of truth",
            "reference implementation",
        ]
        .iter()
        .any(|phrase| lower.contains(phrase))
        {
            continue;
        }
        let words: Vec<&str> = sentence.split_whitespace().collect();
        for pair in words.windows(2) {
            let kind = pair[1].trim_matches(|c: char| !c.is_ascii_alphabetic());
            if matches!(
                kind.to_ascii_lowercase().as_str(),
                "tool" | "program" | "command" | "binary" | "utility"
            ) {
                add(pair[0]);
            }
        }
        for (index, quoted) in sentence.split('`').enumerate() {
            if index % 2 == 1 && !quoted.contains(char::is_whitespace) && !quoted.starts_with('-') {
                add(quoted);
            }
        }
    }
    tools
}

/// Whether `command` contains `word` as a whole shell word or path component.
pub(in crate::agent) fn names_word(command: &str, word: &str) -> bool {
    command
        .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .any(|part| part == word)
}

/// Told once, in an unattended run, when the request names a ground-truth tool no check ran.
pub(in crate::agent) fn reference_tool_reason(tools: &[String]) -> String {
    let list = tools
        .iter()
        .map(|tool| format!("`{tool}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "The request names {list} as the ground truth, and no passing check ran it, so values you computed another way (a reimplementation or a formula) may differ from it. Before finishing, run that tool itself on your final result, with the options the request gives (install it if it is missing, for example from the package or source that provides it), in a check:true that fails when its output breaks the request's limits. Leave margin rather than landing at a limit. If the tool cannot be run here, say so in your answer."
    )
}

/// The word or phrase by which the request asks for speed (`as efficient as possible`,
/// `optimize`, `faster`, `speed up`), or `None`.
pub(in crate::agent) fn requested_speed(instruction: &str) -> Option<String> {
    let lower = instruction.to_ascii_lowercase();
    for phrase in ["speed up", "speedup", "as fast as"] {
        if lower.contains(phrase) {
            return Some(phrase.to_owned());
        }
    }
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .find(|word| {
            word.starts_with("optimiz")
                || word.starts_with("optimis")
                || word.starts_with("efficien")
                || matches!(*word, "faster" | "fastest" | "performant")
        })
        .map(str::to_owned)
}

/// Told once, in an unattended run, when the request asks for speed.
pub(in crate::agent) fn performance_reason(word: String) -> String {
    format!(
        "The request asks for speed (\"{word}\"), and matching output does not show that. Before finishing, time the delivered version on the request's own input in a fresh process, print the time, and make a check:true fail unless it is clearly faster than the original (if there is one), or within any limit the request states. Run a slow original only once: save its output and time to files, then compare each later version against those files instead of running the original again. Make sure the timed command really runs the work (for example the query against its database, not an empty input). If yours is slow, find the repeated or quadratic work (correlated subqueries, nested scans, missing indexes it may use) and rework it, then check again. When the request asks for the fastest result (\"as efficient as possible\", \"optimize\"), beating the original is not the bar, since a reference solution may be timed against yours: try at least one alternative formulation, time each a few times on the real input, and keep the fastest. If speed is not what the request asks, finish again."
    )
}

/// Function names the request asks for: `function named "ars"`, `function called ars`,
/// ``a `parse()` function``. Bounded to a few names; quotes, backticks and `()` are stripped.
pub(in crate::agent) fn requested_functions(instruction: &str) -> Vec<String> {
    let words: Vec<&str> = instruction.split_whitespace().collect();
    let ident = |word: &str| {
        let name = word
            .trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | ',' | '.' | ':' | ';'))
            .trim_end_matches("()");
        let name = name.trim_matches(|c: char| matches!(c, '"' | '\'' | '`'));
        let quoted = word.starts_with(['"', '\'', '`']);
        (quoted
            && !name.is_empty()
            && name.len() <= 64
            && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.')))
        .then(|| name.to_owned())
    };
    let mut names = Vec::new();
    for (index, word) in words.iter().enumerate() {
        let lower = word.to_ascii_lowercase();
        if !matches!(lower.as_str(), "function" | "method") {
            continue;
        }
        // `function named "x"`, `function `x``, or `"x()" function`; a bare quoted word before
        // `function` (`the "d" function family`) is description, not a name.
        let after = match words.get(index + 1).map(|w| w.to_ascii_lowercase()) {
            Some(w) if w == "named" || w == "called" => words.get(index + 2).copied(),
            _ => words.get(index + 1).copied().filter(|w| w.starts_with('`')),
        };
        let before = index
            .checked_sub(1)
            .and_then(|i| words.get(i).copied())
            .filter(|w| w.contains("()"));
        for name in [after, before].into_iter().flatten().filter_map(ident) {
            if !names.contains(&name) && names.len() < 4 {
                names.push(name);
            }
        }
    }
    names
}

/// Appended when an aggregate zero exit cannot establish that the checked pipeline passed. The
/// note names the accepted shape, because a model that already wrote `pipefail` somewhere else
/// (after `cd dir &&`, or before several commands) cannot tell from a generic note what is wrong.
pub(in crate::agent) fn masked_check_note(command: &str) -> String {
    let fix = if trailing_display_masks(command) {
        "The command ends with a display command (`echo`, `tail`, `cat` ...) after the check, so its exit status is that command's, not the check's. End with the check itself (put any display before it, or use `cmd > log 2>&1 && tail log`), or run the check alone."
    } else if command.contains("pipefail") {
        "`set -o pipefail` counts only as the very first statement, unconditional (no `cd dir &&` or other command before it), with every pipeline inside the final `&&`-chain (no `||`, `!`, `&`, or later `set`). Set the directory with the bash `cwd` argument instead of a leading `cd`, or redirect output to a file and check that file without a pipe."
    } else {
        "Rerun without the pipe (redirect output to a file and check that file), or start the command with `set -o pipefail;` followed by the checked pipeline (or `&&`-chain ending in it). Set the directory with the bash `cwd` argument instead of a leading `cd`."
    };
    format!(
        "\n[Dial check] Not counted as a passing check: the shell's zero exit does not establish that the checked command passed. {fix}"
    )
}

/// Only a literal, unconditional setup followed by the checked foreground pipeline makes an
/// aggregate zero reliable here. Do not try to simulate shell option state across other commands.
pub(in crate::agent) fn exit_status_masked(command: &str) -> bool {
    use ast_grep_language::{LanguageExt, SupportLang};

    if trailing_display_masks(command) {
        return true;
    }
    if !command.contains('|') {
        return false;
    }
    let ast = SupportLang::Bash.ast_grep(command);
    let root = ast.root();
    if root.get_inner_node().has_error() {
        return true;
    }
    // A predicate comparing a substitution's output checks its value, not its exit.
    let pipelines = root
        .dfs()
        .filter(|n| n.kind() == "pipeline")
        .filter(|node| {
            let predicate = node
                .ancestors()
                .find(|n| n.kind() == "command_substitution")
                .and_then(|substitution| {
                    substitution
                        .ancestors()
                        .find(|n| matches!(n.kind().as_ref(), "command" | "test_command"))
                });
            !predicate.is_some_and(|n| {
                n.kind() == "test_command"
                    || n.field("name").is_some_and(|name| name.text() == "test")
            })
        });
    let checked_pipelines: Vec<usize> = pipelines.map(|node| node.node_id()).collect();
    if checked_pipelines.is_empty() {
        return false;
    }

    let is_enable_pipefail = |text: &str, kind: &str| -> bool {
        let mut words = text.split([' ', '\t']).filter(|word| !word.is_empty());
        kind == "command"
            && words.next() == Some("set")
            && words
                .next()
                .and_then(|w| w.strip_prefix('-')?.strip_suffix('o'))
                .is_some_and(|flags| flags.bytes().all(|b| matches!(b, b'e' | b'u' | b'x')))
            && words.next() == Some("pipefail")
            && words.next().is_none()
    };

    // Keep operators (including background `&`) when inspecting top-level statements.
    let statements: Vec<_> = root
        .children()
        .filter(|n| !matches!(n.kind().as_ref(), "comment" | ";"))
        .collect();
    if statements.is_empty() || statements.iter().any(|node| node.kind() == "&") {
        return true;
    }
    let (setup_id, chain_root, drop_first) = if statements.len() == 1 {
        (None, statements[0].clone(), true)
    } else {
        let setup = &statements[0];
        if !is_enable_pipefail(&setup.text(), setup.kind().as_ref()) {
            return true;
        }
        (
            Some(setup.node_id()),
            statements.last().unwrap().clone(),
            false,
        )
    };

    // Flatten the final statement's `&&`-chain in left-to-right order; any `||` fails closed.
    let mut chain = Vec::new();
    let mut stack = vec![chain_root];
    while let Some(node) = stack.pop() {
        let node = node
            .field("body")
            .filter(|_| node.kind() == "redirected_statement")
            .unwrap_or(node);
        if node.kind() == "list" {
            let items: Vec<_> = node.children().filter(|n| n.kind() != "comment").collect();
            let [left, operator, right] = items.as_slice() else {
                return true;
            };
            if operator.kind() != "&&" {
                return true;
            }
            stack.push(right.clone());
            stack.push(left.clone());
        } else {
            chain.push(node);
        }
    }
    let (setup_id, checked_items) = if drop_first {
        let Some((first, rest)) = chain.split_first() else {
            return true;
        };
        if rest.is_empty() || !is_enable_pipefail(&first.text(), first.kind().as_ref()) {
            return true;
        }
        (first.node_id(), rest)
    } else {
        (setup_id.unwrap(), chain.as_slice())
    };

    // Reject any second `set` command anywhere in the script (e.g. `set +o pipefail`).
    if root.dfs().any(|node| {
        node.node_id() != setup_id
            && node.kind() == "command"
            && node.field("name").is_some_and(|name| name.text() == "set")
    }) {
        return true;
    }

    let mut covered_pipelines = Vec::new();
    let mut stack = checked_items.to_vec();
    while let Some(node) = stack.pop() {
        let node = node
            .field("body")
            .filter(|_| node.kind() == "redirected_statement")
            .unwrap_or(node);
        match node.kind().as_ref() {
            "command" | "test_command" | "comment" => {}
            "pipeline" => {
                // Bash gives pipes precedence over &&, but tree-sitter-bash can put a list
                // containing another pipeline inside a stage (`a | b | c && d`). Both
                // operators preserve failures here, so validate nested nodes the same way.
                covered_pipelines.push(node.node_id());
                stack.extend(node.named_children());
            }
            "list" => {
                let items: Vec<_> = node.children().filter(|n| n.kind() != "comment").collect();
                let [left, operator, right] = items.as_slice() else {
                    return true;
                };
                if operator.kind() != "&&" {
                    return true;
                }
                stack.push(right.clone());
                stack.push(left.clone());
            }
            _ => return true,
        }
    }
    !checked_pipelines
        .iter()
        .all(|id| covered_pipelines.contains(id))
}

/// `cmd > log 2>&1; echo exit=$?; tail log`: the shell exits with the trailing display command's
/// status, so a failing `cmd` still yields zero. Leading `cd`/`set`/`export` setup and an earlier
/// display-only statement do not count as the checked work.
fn trailing_display_masks(command: &str) -> bool {
    use ast_grep_language::{LanguageExt, SupportLang};

    let ast = SupportLang::Bash.ast_grep(command);
    let root = ast.root();
    if root.get_inner_node().has_error() {
        return false;
    }
    let statements: Vec<String> = root
        .children()
        .filter(|n| !matches!(n.kind().as_ref(), "comment" | ";" | "&"))
        .map(|n| n.text().to_string())
        .collect();
    let Some((last, earlier)) = statements.split_last() else {
        return false;
    };
    let setup = |text: &str| {
        !text.contains("&&")
            && !text.contains("||")
            && matches!(
                text.split_whitespace().next(),
                Some("cd" | "set" | "export" | "source" | ".")
            )
    };
    display_only(last)
        && earlier
            .iter()
            .any(|statement| !setup(statement) && !display_only(statement))
}

#[cfg(test)]
mod tests;

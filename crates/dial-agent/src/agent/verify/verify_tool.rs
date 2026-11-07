//! `verify`: run named tests, or the focused checks the completion gate would select right now.
//!
//! Targets are test files or `file::test` entries (the runner is chosen by
//! `verification::test_targets`), names of gate checks, or `failed` for the latest code failures.
//! Without targets the selection is the gate's (`focused_checks` plus retained failures). Every
//! run goes through `run_focused_check`, so approvals, the sandbox, classification and the
//! evidence ledger are the ones final verification reads. Passing controller observations can be
//! reused only with a complete operator-declared input contract; explicit targets always execute.
//! `edit`/`write` `verify` entries run through the same path once per assistant batch.
//!
//! `repro` registers a model-written reproduction for this turn only. It joins the gate's
//! selection, is approved like a `bash` call, and earns behavioral credit only after a code
//! failure at an earlier effect revision and a pass at the final one (`evidence::ReproState`).
use crate::agent::verify::evidence::{MAX_REPROS, ReproState};
use crate::agent::verify::verification::test_targets::{self, TestCounts};
use crate::agent::verify::verification::{CheckStatus, FocusedCheck};
use crate::agent::*;
use crate::tools::edit::editing::MAX_VERIFY_TARGETS;

pub(in crate::agent) const NAME: &str = "verify";
/// The target that reruns every check whose latest result is a code failure.
const FAILED: &str = "failed";

/// Kept diagnostic bytes per non-passing check.
const DIAGNOSTIC_BYTES: usize = 1500;
/// Registered reproductions get the deadline a model `bash` call gets by default.
const REPRO_TIMEOUT_SECS: u64 = 300;
const REPRO_PREFIX: &str = "repro: ";
const REPRO_NAME_BYTES: usize = 64;
const REPRO_COMMAND_BYTES: usize = 4000;

/// `predictions` adds the optional `expect` argument (`experimental.check_predictions`).
pub(in crate::agent) fn schema(predictions: bool) -> Value {
    let mut schema = json!({"type":"function","function":{"name":NAME,"description":"verify: run tests and this turn's focused checks; reports Passed/Failed with failure traces, and passes count toward final verification. targets: test files or `file::test` (Dial picks pytest, unittest, cargo, go, jest or vitest), check names from an earlier result, or `failed` to rerun code failures. Omit targets to run every check the completion gate selects (passes whose declared inputs are unchanged may be reused). repro registers a bug reproduction (fails before the fix, passes after; max 4 per turn, same name replaces), approved like bash and rerun by the final gate; behavioral credit needs an observed failure, then a pass.","parameters":{"type":"object","properties":{"targets":{"type":"array","maxItems":MAX_VERIFY_TARGETS,"items":{"type":"string"},"description":"Test files, `file::test`, check names or `failed`; omit for the gate's selection."},"repro":{"type":"object","properties":{"name":{"type":"string"},"command":{"type":"string","description":"Exits non-zero while the bug is present, zero once fixed."}},"required":["name","command"],"additionalProperties":false}},"additionalProperties":false}}});
    if predictions {
        schema["function"]["parameters"]["properties"]["expect"] =
            crate::tools::surface::contracts::check_prediction_property();
    }
    schema
}

/// The `targets` argument: `None` when omitted, an error for a non-list or non-string entry.
fn requested_targets(args: &Value) -> Result<Option<Vec<String>>, String> {
    let entries = match args.get("targets") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Array(entries)) => entries,
        Some(_) => {
            return Err("targets must be an array of test files, check names or `failed`".into());
        }
    };
    if entries.len() > MAX_VERIFY_TARGETS {
        return Err(format!(
            "targets accepts at most {MAX_VERIFY_TARGETS} entries"
        ));
    }
    entries
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .map(|entry| entry.trim().to_owned())
                .filter(|entry| !entry.is_empty())
                .ok_or_else(|| "targets entries must be non-empty strings".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// Runner totals for one result: the summary's test counts, else the check itself as one.
fn result_counts(status: CheckStatus, output: Option<&ToolOutput>) -> TestCounts {
    let passed = status == CheckStatus::Passed;
    let mut counts = output
        .and_then(|output| test_targets::test_counts(&output.text))
        .unwrap_or(TestCounts {
            passed: u64::from(passed),
            failed: u64::from(!passed),
        });
    // A failed run whose summary shows no failure (a build error, a timeout) is still a failure.
    if !passed && counts.failed == 0 {
        counts.failed = 1;
    }
    counts
}

/// Compare a declared `expect` with the observed outcome. `None` without a prediction (or when
/// predictions are off) or when nothing ran to completion (cancelled).
fn prediction(enabled: bool, args: &Value, all_passed: bool, cancelled: bool) -> Option<Value> {
    let expected = args["expect"].as_str().filter(|_| enabled && !cancelled)?;
    let expected_pass = match expected {
        "pass" => true,
        "fail" => false,
        _ => return None,
    };
    let observed = if all_passed { "pass" } else { "fail" };
    Some(json!({"expected":expected,"observed":observed,"matched":expected_pass == all_passed}))
}

/// The reproduction `args` asks to register, if any. Command text is untrusted model input; it
/// is only validated for shape here and authorized like a shell call when it runs.
fn requested_repro(args: &Value) -> Option<Result<FocusedCheck, String>> {
    let repro = args.get("repro").filter(|value| !value.is_null())?;
    if !repro.is_object() {
        // Schema-directed normalization already decodes a JSON-encoded object; what remains is a
        // string that is not valid JSON or another type. Name that instead of a misleading
        // `repro.name` error.
        let detail = match repro.as_str().map(serde_json::from_str::<Value>) {
            Some(Err(error)) => format!("a string that is not valid JSON ({error})"),
            Some(Ok(_)) => "a JSON-encoded value that is not an object".to_owned(),
            None => "not an object".to_owned(),
        };
        return Some(Err(format!(
            "repro must be an object {{\"name\": \"<short-name>\", \"command\": \"<shell command>\"}}; got {detail}"
        )));
    }
    let name = repro["name"].as_str().unwrap_or_default().trim();
    let name = name
        .strip_prefix(REPRO_PREFIX.trim_end())
        .map_or(name, str::trim);
    let command = repro["command"].as_str().unwrap_or_default().trim();
    Some(
        if name.is_empty()
            || name.len() > REPRO_NAME_BYTES
            || name.chars().any(|c| c.is_control() || c == ';')
        {
            Err(format!(
                "repro.name must be 1-{REPRO_NAME_BYTES} bytes without control characters or ';'"
            ))
        } else if command.is_empty() || command.len() > REPRO_COMMAND_BYTES {
            Err(format!(
                "repro.command must be a non-empty shell command of at most {REPRO_COMMAND_BYTES} bytes"
            ))
        } else {
            Ok(FocusedCheck {
                name: format!("{REPRO_PREFIX}{name}"),
                command: command.to_owned(),
                timeout_secs: REPRO_TIMEOUT_SECS,
            })
        },
    )
}

/// `verify` entries of the `edit`/`write` calls in one assistant batch. They run once, after the
/// batch's last mutation, so a test written earlier in the same response is on disk first.
#[derive(Default)]
pub(in crate::agent) struct VerifyBatch {
    /// Ids of the batch's `edit`/`write` calls, in order. A mutation outside them (a direct or
    /// callback dispatch) is its own batch.
    mutations: Vec<String>,
    pending: Vec<String>,
    calls: std::collections::HashSet<String>,
    settled: std::collections::HashSet<String>,
}

/// What a finished mutation means for the batch's `verify` entries.
pub(in crate::agent) enum VerifyStep {
    /// Not the last mutation and nothing requested: nothing to report.
    Nothing,
    /// Requested entries wait for the batch's last mutation.
    Deferred,
    /// The batch's last mutation succeeded: run these entries now.
    Run(Vec<String>),
    /// The batch's last mutation failed: the requested entries are not run.
    Skipped(usize),
}

impl VerifyBatch {
    pub(in crate::agent) fn begin(&mut self, calls: &[ToolCall]) {
        self.mutations = calls
            .iter()
            .filter(|call| matches!(call.name.as_str(), "edit" | "write"))
            .map(|call| call.id.clone())
            .collect();
        self.pending.clear();
        self.calls = calls.iter().map(|call| call.id.clone()).collect();
        self.settled.clear();
    }

    pub(in crate::agent) fn settle_once(&mut self, id: &str) -> bool {
        !self.calls.contains(id) || self.settled.insert(id.to_owned())
    }

    pub(in crate::agent) fn end(&mut self) {
        *self = Self::default();
    }

    /// Record a finished `edit`/`write`. Entries of a failed call are not collected.
    pub(in crate::agent) fn mutation_finished(
        &mut self,
        call: &ToolCall,
        succeeded: bool,
    ) -> VerifyStep {
        let requested =
            crate::tools::edit::editing::verify_targets(&call.arguments).unwrap_or_default();
        let in_batch = self.mutations.contains(&call.id);
        let mut pending = if in_batch {
            std::mem::take(&mut self.pending)
        } else {
            Vec::new()
        };
        if succeeded {
            for entry in &requested {
                if !pending.contains(entry) {
                    pending.push(entry.clone());
                }
            }
        }
        if in_batch && self.mutations.last() != Some(&call.id) {
            self.pending = pending;
            return if succeeded && !requested.is_empty() {
                VerifyStep::Deferred
            } else {
                VerifyStep::Nothing
            };
        }
        match (succeeded, pending.is_empty()) {
            (true, false) => VerifyStep::Run(pending),
            (false, false) => VerifyStep::Skipped(pending.len()),
            (false, true) if !requested.is_empty() => VerifyStep::Skipped(requested.len()),
            _ => VerifyStep::Nothing,
        }
    }
}

fn status_name(status: CheckStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Compiler/test diagnostic lines when the output has any, otherwise the clipped output.
fn diagnostics(text: &str) -> String {
    let mut collector = crate::tools::code::diagnostics::Collector::new(DIAGNOSTIC_BYTES);
    collector.feed(text.as_bytes());
    collector.finish();
    let (kept, _, overflow) = collector.kept();
    if kept.is_empty() {
        return crate::agent::safety::guard::clip_evidence(text.trim(), DIAGNOSTIC_BYTES);
    }
    let mut kept = kept.trim_end().to_owned();
    if overflow > 0 {
        kept.push_str(&format!("\n({overflow} more diagnostic lines not shown)"));
    }
    kept
}

/// Preserve the native mutation's outcome and effects: verification failure is not refusal.
pub(in crate::agent) fn attach_post_mutation_verify(
    output: &mut ToolOutput,
    tool: &str,
    verify: ToolOutput,
) {
    crate::tools::surface::outcome::append_text(
        output,
        &format!(
            "\n\n[verify after this response's edits; the {tool} remains applied]\n{}",
            verify.text
        ),
    );
    if !output.data.as_ref().is_some_and(Value::is_object) {
        output.data = Some(json!({format!("{tool}_result"):output.data.take()}));
    }
    output.data.as_mut().expect("object mutation result")[format!("post_{tool}_verify")] =
        json!({"is_error":verify.is_error,"text":verify.text,"data":verify.data});
}

pub(in crate::agent) fn post_mutation_verify(output: &ToolOutput) -> Option<&Value> {
    let data = output.data.as_ref()?;
    data.get("post_edit_verify")
        .or_else(|| data.get("post_write_verify"))
}

impl Agent<'_> {
    pub(in crate::agent) fn verify_tool(&mut self, args: &Value, ui: &mut dyn Ui) -> ToolOutput {
        let predictions = self.config.experimental.check_predictions;
        if predictions
            && let Some(expect) = args.get("expect").filter(|value| !value.is_null())
            && !matches!(expect.as_str(), Some("pass" | "fail"))
        {
            return ToolOutput::err("expect must be \"pass\" or \"fail\"");
        }
        if args.get("repro").is_none_or(Value::is_null) && args.get("command").is_some() {
            return ToolOutput::err(
                "verify takes no top-level `command`. To register a bug reproduction, call verify with repro: {\"name\": \"<short-name>\", \"command\": \"<shell command>\"}; the command must exit non-zero while the bug is present and zero once fixed. To run tests, list them in targets.",
            );
        }
        let targets = match requested_targets(args) {
            Ok(targets) => targets,
            Err(error) => return ToolOutput::err(error),
        };
        self.refresh_check_inputs();
        let failed_only = targets.iter().flatten().any(|target| target == FAILED);
        let mut names: Option<Vec<String>> = targets
            .as_ref()
            .map(|targets| targets.iter().filter(|t| *t != FAILED).cloned().collect());
        // `failed` alone reruns every code failure, not an empty subset.
        if failed_only && names.as_ref().is_some_and(Vec::is_empty) {
            names = None;
        }
        let repro = match requested_repro(args) {
            Some(Err(error)) => return ToolOutput::err(error),
            Some(Ok(check)) => Some(check),
            None => None,
        };
        if repro.is_some() && failed_only {
            return ToolOutput::err(
                "repro cannot be combined with `failed`: register and run it first, then name it or `failed` in targets to rerun it",
            );
        }
        let mut candidates = self.current_focused_checks();
        self.step_checks
            .include_failed_checks(self.workspace.root(), &mut candidates);
        let listed = |checks: &[FocusedCheck]| {
            checks
                .iter()
                .map(|check| check.name.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        };
        // Selection errors must not replace a reproduction or clear its failed evidence.
        // Registration adds only its own name; replacing a name keeps that name selectable.
        if let Some(names) = &mut names {
            if names.is_empty() && repro.is_none() {
                return ToolOutput::err(
                    "targets must name at least one test or check; omit targets to run every applicable check",
                );
            }
            if let Err(error) = self.resolve_targets(names, &mut candidates, repro.as_ref()) {
                return ToolOutput::err(format!(
                    "{error}. Check names: {}",
                    if candidates.is_empty() {
                        "none apply yet".to_owned()
                    } else {
                        listed(&candidates)
                    }
                ));
            }
        }
        // A registered reproduction runs with the rest of the gate's selection once the turn has
        // changes, so one call reports every applicable check instead of prompting a second plain
        // `verify` for the ones still lacking a pass. Before any change it runs alone as a baseline.
        let explicit_names = names.is_some();
        let mut registered = None;
        if let Some(check) = repro {
            if let Err(error) = self.register_repro(&check, &mut candidates) {
                return ToolOutput::err(error);
            }
            names.get_or_insert_with(Vec::new).push(check.name.clone());
            registered = Some(check.name);
        }
        let changed =
            self.step_checks.mutations > 0 || self.step_checks.has_uncheckpointed_operations();
        if registered.is_some() && changed && !explicit_names {
            names = None;
        }
        let note = |text: String| self.check_receipt(ToolOutput::ok(text), &candidates);
        if candidates.is_empty() {
            return note(if changed {
                "No focused check applies to this turn's changes; the completion gate has nothing to run. Name the tests that cover the change in targets.".into()
            } else {
                "No changes yet and no focused check applies; nothing run.".into()
            });
        }
        let mut selected = candidates.clone();
        if let Some(names) = &names {
            selected.retain(|check| names.contains(&check.name));
        }
        // A reproduction is opaque shell work that advances the effect revision. Running it
        // first keeps the other checks' passes current at the revision this receipt reports.
        selected.sort_by_key(|check| !self.step_checks.evidence.is_repro(&check.command));
        if failed_only {
            selected.retain(|check| {
                self.step_checks.evidence.latest_status(&check.command)
                    == Some(CheckStatus::CodeFailed)
            });
            if selected.is_empty() {
                return note(format!(
                    "No check's latest result is code_failed; nothing run. Checks: {}",
                    listed(&candidates)
                ));
            }
        }
        // The gate runs nothing before the first change; an explicit subset is a baseline request.
        if !changed && names.is_none() && !failed_only {
            return note(format!(
                "No changes yet, so the completion gate runs nothing. Checks that would apply now: {}. Name them in targets to run a baseline.",
                listed(&candidates)
            ));
        }

        let mut results = Vec::new();
        let mut text = Vec::new();
        let mut counts = TestCounts::default();
        let mut all_passed = true;
        let mut cancelled = false;
        let mut refused = None;
        for check in &selected {
            // The reproduction just registered always executes; other checks may reuse passes.
            let reused = names.is_none()
                && !failed_only
                && registered.as_ref() != Some(&check.name)
                && self.reuse_check(check);
            let (status, output) = if self.interrupt.load(Ordering::SeqCst) {
                self.step_checks.evidence.observe(
                    &check.command,
                    &format!("focused-check:{}", check.name),
                    CheckStatus::Cancelled,
                    self.step_checks.revision(),
                    self.behavioral_check(&check.name),
                );
                cancelled = true;
                (CheckStatus::Cancelled, None)
            } else if reused {
                (CheckStatus::Passed, None)
            } else {
                let output = self.run_focused_check(check, ui);
                // Include the focused runner's declared-input freshness verdict, not only exit facts.
                let status = self
                    .step_checks
                    .evidence
                    .latest_status(&check.command)
                    .unwrap_or(CheckStatus::Unverified);
                (status, Some(output))
            };
            if status != CheckStatus::Cancelled {
                let ran = result_counts(status, output.as_ref());
                counts.passed += ran.passed;
                counts.failed += ran.failed;
            }
            // Same bookkeeping as a gate pass: a check's most recent non-pass is retained.
            self.step_checks
                .failed_checks
                .retain(|failed| failed.command != check.command);
            let name = status_name(status);
            text.push(format!("{name}: {}\n  $ {}", check.name, check.command));
            if reused {
                text.push("  reused: declared inputs unchanged; not rerun".into());
            }
            if status != CheckStatus::Passed {
                self.step_checks.reproduced |= status == CheckStatus::CodeFailed;
                all_passed = false;
                self.step_checks.failed_checks.push(check.clone());
                if let Some(output) = &output {
                    if let Some(problem) =
                        crate::agent::verify::verification::environment_problem(check, &output.text)
                    {
                        text.push(format!("  {problem}"));
                    }
                    text.push(diagnostics(&output.text));
                }
            }
            let mut result = json!({"name":check.name,"command":check.command,"status":name,"reused":reused,"ran":output.is_some(),"evidence_ref":self.step_checks.evidence.evidence_ref(&check.command)});
            if test_targets::exit_status_target(&check.name) {
                result["evidence"] = json!("exit_status_only");
            }
            if reused {
                result["observed_revision"] =
                    json!(self.step_checks.evidence.observed_revision(&check.command));
            }
            if self.step_checks.evidence.is_repro(&check.command) {
                // A reproduction that passes before any change does not show the bug (often
                // inverted exit logic). Registering it would make the final gate rerun a command
                // that fails once the code is fixed, so it is withdrawn and the call says why.
                if registered.as_ref() == Some(&check.name)
                    && !changed
                    && status == CheckStatus::Passed
                {
                    self.step_checks.evidence.withdraw_repro(&check.command);
                    candidates.retain(|candidate| candidate.command != check.command);
                    result["registration"] = json!("refused");
                    refused = Some(format!(
                        "Not registered: {} passed before any change, so it does not reproduce the bug. A reproduction must exit non-zero while the bug is present and zero once fixed; check its exit logic (for example an inverted assertion or a misplaced `||`/`&&`), then register a corrected command.",
                        check.name
                    ));
                } else {
                    let state = self
                        .step_checks
                        .evidence
                        .repro_state(&check.command, self.step_checks.revision());
                    text.push(format!("  reproduction: {}", state.describe()));
                    result["reproduction"] = json!(state.as_str());
                    if status == CheckStatus::Passed && state == ReproState::NotShownToReproduce {
                        text.push("  Post-fix check passed; pre-fix failure was not observed. Rereads or unchanged reruns cannot recover that history. Do not undo the fix to manufacture credit. Report the gap if fail-then-pass proof is required; otherwise finish once remaining acceptance checks pass.".into());
                    }
                }
            }
            results.push(result);
        }
        self.step_checks.failed = !self.step_checks.failed_checks.is_empty();
        if !self.hermetic {
            self.publish_edit_loop_status(ui);
        }
        let changed_paths: Vec<PathBuf> = self
            .step_checks
            .turn
            .zip(self.checkpoints.as_ref())
            .map(|(turn, checkpoints)| checkpoints.changed_files(turn))
            .unwrap_or_default()
            .into_iter()
            .map(PathBuf::from)
            .collect();
        let limits = self.related_test_plan(&changed_paths).limits;
        text.insert(
            0,
            format!(
                "Verify: Passed {}, Failed {}{}",
                counts.passed,
                counts.failed,
                if cancelled {
                    " (cancelled before every check ran)"
                } else {
                    ""
                }
            ),
        );
        if selected
            .iter()
            .any(|check| test_targets::exit_status_target(&check.name))
        {
            text.push("A plain script is judged only by its exit status; it asserts only what it checks itself.".into());
        }
        text.push(if let Some(refused) = &refused {
            refused.clone()
        } else if all_passed {
            "Passes count toward final verification. Later changes stale evidence unless complete declared inputs are revalidated unchanged.".into()
        } else if registered.is_some() && selected.len() == 1 {
            "Reproduction registered. Fix the code, then rerun it by name or with targets [\"failed\"]; the final gate reruns it.".into()
        } else {
            "Repair confirmed code failures, then rerun with targets [\"failed\"]. Environment, permission and timeout results are not code defects.".into()
        });
        if !limits.is_empty() {
            text.push(format!("Coverage gaps: {}", limits.join("; ")));
        }
        let prediction = prediction(predictions, args, all_passed, cancelled);
        if let Some(prediction) = &prediction
            && prediction["matched"] == false
        {
            text.push(format!(
                "Prediction mismatch: expected {}, observed {}. The model of the code that produced this prediction is wrong somewhere; find where before the next edit.",
                prediction["expected"].as_str().unwrap_or_default(),
                prediction["observed"].as_str().unwrap_or_default()
            ));
        }
        let mut data = json!({"revision":self.step_checks.revision(),"counts":{"passed":counts.passed,"failed":counts.failed},"results":results,"coverage_limits":limits});
        if let Some(prediction) = prediction {
            data["prediction"] = prediction;
        }
        self.check_receipt(
            ToolOutput {
                execution: Default::default(),
                data: Some(data),
                text: text.join("\n"),
                is_error: !all_passed || refused.is_some(),
            },
            &candidates,
        )
    }

    /// Map each requested name to a selectable check: an exact or `repro:`-prefixed check name,
    /// else a test-file entry added to `candidates` (reusing a candidate with the same command).
    /// Every entry is resolved before any is reported, so one error lists all refusals.
    fn resolve_targets(
        &self,
        names: &mut [String],
        candidates: &mut Vec<FocusedCheck>,
        repro: Option<&FocusedCheck>,
    ) -> Result<(), String> {
        let python = self.check_python();
        let known = |name: &str, candidates: &[FocusedCheck]| {
            candidates.iter().any(|check| check.name == name)
                || repro.is_some_and(|check| check.name == name)
        };
        let mut refused = Vec::new();
        for name in names.iter_mut() {
            if known(name, candidates) {
                continue;
            }
            let prefixed = format!("{REPRO_PREFIX}{name}");
            if known(&prefixed, candidates) {
                *name = prefixed;
                continue;
            }
            match test_targets::target_check(
                self.workspace.root(),
                &python,
                &self.config.verification.python_command,
                name,
            ) {
                Ok(check) => match candidates.iter().find(|c| c.command == check.command) {
                    Some(existing) => *name = existing.name.clone(),
                    None => {
                        *name = check.name.clone();
                        candidates.push(check);
                    }
                },
                Err(reason) => refused.push(reason),
            }
        }
        if refused.is_empty() {
            Ok(())
        } else {
            Err(format!("not run: {}", refused.join("; ")))
        }
    }

    /// One revision-scoped view of execution and remaining applicable checks. Reporting scope
    /// never runs a check, promotes stale evidence or treats an empty selection as verification.
    fn check_receipt(&self, mut output: ToolOutput, candidates: &[FocusedCheck]) -> ToolOutput {
        const MAX_PENDING: usize = 16;
        const MAX_SUMMARY: usize = 4;
        let revision = self.step_checks.revision();
        let pending: Vec<_> = candidates
            .iter()
            .filter_map(|check| {
                let status = self
                    .step_checks
                    .evidence
                    .current_status(&check.command, revision);
                (status != "passed").then_some((check.name.as_str(), status))
            })
            .collect();
        let data = output
            .data
            .get_or_insert_with(|| json!({"revision":revision,"results":[]}));
        data["scope"] = json!({
            "applicable_checks":candidates.len(),
            "pending_checks":pending.iter().take(MAX_PENDING)
                .map(|(name,status)| json!({"name":name,"status":status})).collect::<Vec<_>>(),
            "pending_count":pending.len(),
            "omitted_pending_checks":pending.len().saturating_sub(MAX_PENDING),
        });
        if !pending.is_empty() {
            let summary = pending
                .iter()
                .take(MAX_SUMMARY)
                .map(|(name, status)| {
                    format!(
                        "{} ({status})",
                        crate::agent::safety::guard::clip_evidence(name, 80)
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            let omitted = pending.len().saturating_sub(MAX_SUMMARY);
            output.text.push_str(&format!(
                "\nFocused scope: {} applicable check(s) lack a current pass: {summary}{}.",
                pending.len(),
                if omitted > 0 {
                    format!("; {omitted} more")
                } else {
                    String::new()
                },
            ));
        }
        output
    }

    /// `pre_tool_use` policy a model `bash` call of the same reproduction would meet. A block or
    /// a rewrite refuses the run; reproductions never execute hook-rewritten text.
    pub(in crate::agent) fn repro_hook_refusal(
        &self,
        args: &Value,
        ui: &mut dyn Ui,
    ) -> Option<String> {
        let hooks = self.tool_hooks();
        if !crate::agent::safety::hooks::wanted(hooks, "pre_tool_use") {
            return None;
        }
        let root = self.workspace.root().to_path_buf();
        let interrupt = self.interrupt.as_ref();
        let mut notices = Vec::new();
        let result = crate::agent::safety::hooks::with_heartbeat(ui, || {
            crate::agent::safety::hooks::pre_tool_with(
                hooks,
                &root,
                "bash",
                args,
                interrupt,
                &mut |notice| notices.push(notice.to_owned()),
            )
        });
        for notice in &notices {
            ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                crate::agent::ui::ui_event::EventKind::Hook,
                notice,
            ));
        }
        match result {
            Err(reason) => Some(reason),
            Ok(Some(rewritten)) if &rewritten != args => Some(
                "a pre_tool_use hook rewrote this reproduction; register a command the hook accepts unchanged".into(),
            ),
            Ok(_) => None,
        }
    }

    /// Register (or replace by name) a turn-scoped reproduction and add it to `candidates`.
    /// Commands the gate already selects or that are configured cannot be re-registered: their
    /// existing evidence must not be relabeled as a reproduction.
    fn register_repro(
        &mut self,
        check: &FocusedCheck,
        candidates: &mut Vec<FocusedCheck>,
    ) -> Result<(), String> {
        let evidence = &self.step_checks.evidence;
        if let Some(selected) = candidates.iter().find(|candidate| {
            !evidence.is_repro(&candidate.command)
                && (candidate.name == check.name || candidate.command == check.command)
        }) {
            return Err(format!(
                "{} is already a selected check; name it in targets instead",
                selected.name
            ));
        }
        if self
            .config
            .verification
            .commands
            .iter()
            .any(|configured| configured.command == check.command)
        {
            return Err(
                "that command is a configured verification command; name it in targets instead"
                    .into(),
            );
        }
        if let Some(other) = evidence
            .repros()
            .iter()
            .find(|other| other.command == check.command && other.name != check.name)
        {
            return Err(format!("already registered as {}", other.name));
        }
        let replaced = self
            .step_checks
            .evidence
            .register_repro(check.clone())
            .map_err(|error| {
                format!("{error}. Registered: {}", {
                    let names: Vec<_> = self
                        .step_checks
                        .evidence
                        .repros()
                        .iter()
                        .map(|r| r.name.as_str())
                        .collect();
                    names.join("; ")
                })
            })?;
        if let Some(old) = replaced {
            // A withdrawn reproduction is no longer an obligation of the gate.
            self.step_checks
                .failed_checks
                .retain(|failed| failed.command != old.command);
            self.step_checks.failed = !self.step_checks.failed_checks.is_empty();
            candidates.retain(|candidate| candidate.command != old.command);
        }
        if !candidates
            .iter()
            .any(|candidate| candidate.command == check.command)
        {
            candidates.push(check.clone());
        }
        debug_assert!(self.step_checks.evidence.repros().len() <= MAX_REPROS);
        Ok(())
    }
}

#[cfg(test)]
mod tests;

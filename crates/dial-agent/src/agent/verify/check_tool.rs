//! `check`: run, on demand, the focused checks the completion gate would select right now.
//!
//! Selection is the gate's (`focused_checks` plus retained failures) and every run goes through
//! `run_focused_check`, so approvals, the sandbox, classification and the evidence ledger are the
//! ones final verification reads. There is no separate pass cache: a pass recorded here counts
//! exactly as a gate run at the same effect revision would, and any later edit makes it stale.
//!
//! `repro` registers a model-written reproduction for this turn only. It joins the gate's
//! selection, is approved like a `bash` call, and earns behavioral credit only after a code
//! failure at an earlier effect revision and a pass at the final one (`evidence::ReproState`).
use crate::agent::verify::evidence::MAX_REPROS;
use crate::agent::verify::verification::{CheckStatus, FocusedCheck};
use crate::agent::*;

pub(in crate::agent) const NAME: &str = "check";

/// Kept diagnostic bytes per non-passing check.
const DIAGNOSTIC_BYTES: usize = 1500;
/// Registered reproductions get the deadline a model `bash` call gets by default.
const REPRO_TIMEOUT_SECS: u64 = 300;
const REPRO_PREFIX: &str = "repro: ";
const REPRO_NAME_BYTES: usize = 64;
const REPRO_COMMAND_BYTES: usize = 4000;

pub(in crate::agent) fn schema() -> Value {
    json!({"type":"function","function":{"name":NAME,"description":"Run the checks Dial's completion gate selects for this turn's changes (build/type checks, related tests, configured commands) and report each status, command and key diagnostics. A pass at the current edits counts toward final verification; a later edit makes it stale. `names` runs a subset; `failed_only` reruns checks whose latest result was code_failed. `repro` registers a shell command that reproduces the reported bug and runs it now; it is approved like bash, rerun by the final gate, kept for this turn only (max 4, same name replaces), and earns behavioral credit only if it fails before your fix and passes after it.","parameters":{"type":"object","properties":{"names":{"type":"array","maxItems":16,"items":{"type":"string"},"description":"Exact check names to run; omit for all."},"failed_only":{"type":"boolean"},"repro":{"type":"object","properties":{"name":{"type":"string"},"command":{"type":"string","description":"Exits non-zero while the bug is present, zero once fixed."}},"required":["name","command"],"additionalProperties":false}},"additionalProperties":false}}})
}

/// The reproduction `args` asks to register, if any. Command text is untrusted model input; it
/// is only validated for shape here and authorized like a shell call when it runs.
fn requested_repro(args: &Value) -> Option<Result<FocusedCheck, String>> {
    let repro = args.get("repro").filter(|value| !value.is_null())?;
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

impl Agent<'_> {
    pub(in crate::agent) fn check_tool(&mut self, args: &Value, ui: &mut dyn Ui) -> ToolOutput {
        let mut names: Option<Vec<String>> = args["names"].as_array().map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        });
        let failed_only = args["failed_only"].as_bool().unwrap_or(false);
        let repro = match requested_repro(args) {
            Some(Err(error)) => return ToolOutput::err(error),
            Some(Ok(check)) => Some(check),
            None => None,
        };
        let mut candidates = self.current_focused_checks();
        self.step_checks
            .include_failed_checks(self.workspace.root(), &mut candidates);
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
        let listed = |checks: &[FocusedCheck]| {
            checks
                .iter()
                .map(|check| check.name.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        };
        let note = |text: String| ToolOutput {
            execution: Default::default(),
            data: None,
            text,
            is_error: false,
        };
        if candidates.is_empty() {
            return note(if changed {
                "No focused check applies to this turn's changes; the completion gate has nothing to run. Verify behavior another way (for example a targeted test through bash).".into()
            } else {
                "No changes yet and no focused check applies; nothing run.".into()
            });
        }
        let mut selected = candidates.clone();
        if let Some(names) = &names {
            let unknown: Vec<&str> = names
                .iter()
                .filter(|name| !candidates.iter().any(|check| &check.name == *name))
                .map(String::as_str)
                .collect();
            if !unknown.is_empty() {
                return ToolOutput::err(format!(
                    "unknown check name(s): {}. Available: {}",
                    unknown.join("; "),
                    listed(&candidates)
                ));
            }
            selected.retain(|check| names.contains(&check.name));
        }
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
                "No changes yet, so the completion gate runs nothing. Checks that would apply now: {}. Pass `names` to run a baseline.",
                listed(&candidates)
            ));
        }

        let mut results = Vec::new();
        let mut text = Vec::new();
        let mut all_passed = true;
        for check in &selected {
            let (status, output) = if self.interrupt.load(Ordering::SeqCst) {
                self.step_checks.evidence.observe(
                    &check.command,
                    &format!("focused-check:{}", check.name),
                    CheckStatus::Cancelled,
                    self.step_checks.revision(),
                    self.config
                        .verification
                        .behavioral_checks
                        .contains(&check.name),
                );
                (CheckStatus::Cancelled, None)
            } else {
                let output = self.run_focused_check(check, ui);
                let status = crate::agent::verify::verification::classify(
                    check,
                    &output,
                    self.interrupt.load(Ordering::SeqCst),
                );
                (status, Some(output))
            };
            // Same bookkeeping as a gate pass: a check's most recent non-pass is retained.
            self.step_checks
                .failed_checks
                .retain(|failed| failed.command != check.command);
            let name = status_name(status);
            text.push(format!("{name}: {}\n  $ {}", check.name, check.command));
            if status != CheckStatus::Passed {
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
            let mut result = json!({"name":check.name,"command":check.command,"status":name});
            if self.step_checks.evidence.is_repro(&check.command) {
                let state = self
                    .step_checks
                    .evidence
                    .repro_state(&check.command, self.step_checks.revision());
                text.push(format!("  reproduction: {}", state.describe()));
                result["reproduction"] = json!(state.as_str());
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
        text.push(if all_passed {
            "Passes at the current edits count toward final verification; any further edit makes them stale.".into()
        } else if registered.is_some() && selected.len() == 1 {
            "Reproduction registered. Fix the code, then rerun it with `names` or failed_only; the final gate reruns it.".into()
        } else {
            "Repair confirmed code failures, then rerun with failed_only. Environment, permission and timeout results are not code defects.".into()
        });
        if !limits.is_empty() {
            text.push(format!("Coverage gaps: {}", limits.join("; ")));
        }
        ToolOutput {
            execution: Default::default(),
            data: Some(
                json!({"revision":self.step_checks.revision(),"results":results,"coverage_limits":limits}),
            ),
            text: text.join("\n"),
            is_error: !all_passed,
        }
    }

    /// `pre_tool_use` policy a model `bash` call of the same reproduction would meet. A block or
    /// a rewrite refuses the run; reproductions never execute hook-rewritten text.
    pub(in crate::agent) fn repro_hook_refusal(
        &self,
        args: &Value,
        ui: &mut dyn Ui,
    ) -> Option<String> {
        let plugins = self.hook_plugins();
        let hooks = self.tool_hooks();
        if !crate::agent::safety::hooks::wanted(hooks, plugins.as_deref(), "pre_tool_use") {
            return None;
        }
        let root = self.workspace.root().to_path_buf();
        let interrupt = self.interrupt.as_ref();
        let mut notices = Vec::new();
        let result = crate::agent::safety::hooks::with_heartbeat(ui, || {
            crate::agent::safety::hooks::pre_tool_with(
                hooks,
                plugins.as_deref(),
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
                "{} is already a selected check; run it with names instead",
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
                "that command is a configured verification command; run it with names instead"
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

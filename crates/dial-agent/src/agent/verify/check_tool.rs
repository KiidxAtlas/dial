//! `check`: run, on demand, the focused checks the completion gate would select right now.
//!
//! Selection is the gate's (`focused_checks` plus retained failures) and every run goes through
//! `run_focused_check`, so approvals, the sandbox, classification and the evidence ledger are the
//! ones final verification reads. There is no separate pass cache: a pass recorded here counts
//! exactly as a gate run at the same effect revision would, and any later edit makes it stale.
use crate::agent::verify::verification::{CheckStatus, FocusedCheck};
use crate::agent::*;

pub(in crate::agent) const NAME: &str = "check";

/// Kept diagnostic bytes per non-passing check.
const DIAGNOSTIC_BYTES: usize = 1500;

pub(in crate::agent) fn schema() -> Value {
    json!({"type":"function","function":{"name":NAME,"description":"Run the checks Dial's completion gate selects for this turn's changes (build/type checks, related tests, configured commands) and report each status, command and key diagnostics. A pass at the current edits counts toward final verification; a later edit makes it stale. `names` runs a subset; `failed_only` reruns checks whose latest result was code_failed.","parameters":{"type":"object","properties":{"names":{"type":"array","maxItems":16,"items":{"type":"string"},"description":"Exact check names to run; omit for all."},"failed_only":{"type":"boolean"}},"additionalProperties":false}}})
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
        let names: Option<Vec<String>> = args["names"].as_array().map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        });
        let failed_only = args["failed_only"].as_bool().unwrap_or(false);
        let mut candidates = self.current_focused_checks();
        self.step_checks
            .include_failed_checks(self.workspace.root(), &mut candidates);
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

        let revision = self.step_checks.revision();
        let mut results = Vec::new();
        let mut text = Vec::new();
        let mut all_passed = true;
        for check in &selected {
            let (status, output) = if self.interrupt.load(Ordering::SeqCst) {
                self.step_checks.evidence.observe(
                    &check.command,
                    &format!("focused-check:{}", check.name),
                    CheckStatus::Cancelled,
                    revision,
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
            results.push(json!({"name":check.name,"command":check.command,"status":name}));
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
        } else {
            "Repair confirmed code failures, then rerun with failed_only. Environment, permission and timeout results are not code defects.".into()
        });
        if !limits.is_empty() {
            text.push(format!("Coverage gaps: {}", limits.join("; ")));
        }
        ToolOutput {
            execution: Default::default(),
            data: Some(json!({"revision":revision,"results":results,"coverage_limits":limits})),
            text: text.join("\n"),
            is_error: !all_passed,
        }
    }
}

#[cfg(test)]
mod tests;

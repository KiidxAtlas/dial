//! Counterfactual focused-check runs in isolated project copies.
//!
//! - `experimental.fault_witness`: after a behavioral focused check passes on a turn with edits,
//!   rerun it once per evidence revision in a copy with the turn's checkpointed changes reverted.
//!   Failing there shows the check exercises the change; passing shows it does not.
//! - `experimental.edit_bisection_runs`: when a check that passed earlier this turn fails after
//!   edits to two or more files, revert halves of those files (to their content at the last
//!   pass) in copies to find which file's change broke it, within the configured run budget.
//!
//! Both are evidence only: they never change check status, repair credit or the real workspace.
//! Copies exclude dependency and build directories, so compiled checks rebuild from cold. Shell
//! effects outside checkpoints are not reverted, so results are labeled with that limit.

use crate::agent::verify::verification::{CheckStatus, FocusedCheck};
use crate::agent::*;
use crate::storage::checkpoint::TurnVersion;
use std::collections::BTreeMap;

/// Bytes of changed-file content kept per passing snapshot for bisection.
const SNAPSHOT_BYTES: usize = 16 << 20;
/// Changed files a bisection considers.
const MAX_BISECT_FILES: usize = 64;

/// Project-relative path to content (`None`: absent) to install in a copy.
pub(in crate::agent) type Overrides = BTreeMap<String, Option<Vec<u8>>>;

#[derive(Default)]
pub(in crate::agent) struct State {
    /// Check name to fault-witness verdict (with the revision it was taken at).
    witnesses: BTreeMap<String, Value>,
    /// Check command to the turn-changed file contents at its latest pass.
    passing: BTreeMap<String, Overrides>,
    /// Check name to the latest bisection report.
    bisections: BTreeMap<String, Value>,
}

impl State {
    pub(in crate::agent) fn evidence(&self) -> Value {
        if self.witnesses.is_empty() && self.bisections.is_empty() {
            return Value::Null;
        }
        json!({
            "fault_witness": self.witnesses,
            "edit_bisection": self.bisections,
            "limits": "Isolated copies revert checkpointed file edits only; shell effects are not reverted and dependency/build directories start empty.",
        })
    }
}

/// Generated syntax/type checks pass without the turn's changes by design; only configured
/// commands and related tests are behavioral enough to witness a fault.
fn behavioral(config: &Config, check: &FocusedCheck) -> bool {
    check.name.starts_with("related ")
        || config
            .verification
            .commands
            .iter()
            .any(|configured| configured.command == check.command)
}

/// Files whose current content differs from `passing`, with the content to restore. A file
/// missing from the snapshot was unchanged at that pass, so its turn-start content applies.
pub(in crate::agent) fn changed_since(versions: &[TurnVersion], passing: &Overrides) -> Overrides {
    versions
        .iter()
        .filter(|version| version.in_project)
        .filter_map(|version| {
            let then = passing
                .get(&version.path)
                .cloned()
                .unwrap_or_else(|| version.before.clone());
            (then != version.after).then(|| (version.path.clone(), then))
        })
        .collect()
}

/// Turn-changed content now, or `None` when it exceeds the snapshot budget.
fn snapshot(versions: &[TurnVersion]) -> Option<Overrides> {
    let mut bytes = 0;
    let mut out = Overrides::new();
    for version in versions.iter().filter(|version| version.in_project) {
        bytes += version.after.as_ref().map_or(0, Vec::len);
        if bytes > SNAPSHOT_BYTES {
            return None;
        }
        out.insert(version.path.clone(), version.after.clone());
    }
    Some(out)
}

/// Next bisection step over `candidates`: the half to revert. `None` once one file remains.
pub(in crate::agent) fn next_half(candidates: &[String]) -> Option<&[String]> {
    (candidates.len() > 1).then(|| &candidates[..candidates.len() / 2])
}

/// Install `overrides` into the copy at `root`, refusing paths that escape it.
fn install(root: &Path, overrides: &Overrides) -> anyhow::Result<()> {
    for (path, content) in overrides {
        let destination =
            crate::agent::workflow::solve::confined_destination(root, Path::new(path))?;
        match content {
            Some(bytes) => {
                if let Some(parent) = destination.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&destination, bytes)?;
            }
            None => match std::fs::remove_file(&destination) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(error.into());
                }
                _ => {}
            },
        }
    }
    Ok(())
}

impl Agent<'_> {
    fn turn_versions_now(&self) -> Option<Vec<TurnVersion>> {
        let (checkpoints, turn) = (self.checkpoints.as_ref()?, self.step_checks.turn?);
        checkpoints.turn_versions(turn).ok()
    }

    /// Run `check` in a fresh project copy with `overrides` installed; its classified status.
    fn counterfactual_run(
        &self,
        check: &FocusedCheck,
        overrides: &Overrides,
        ui: &mut dyn Ui,
    ) -> anyhow::Result<CheckStatus> {
        let copy = crate::workspace_copy::clone_project(self.workspace.root())?;
        install(copy.path(), overrides)?;
        let workspace = crate::tools::Workspace::new(copy.path())?;
        let output = crate::agent::workflow::solve::run_isolated(
            &workspace,
            &check.command,
            check.timeout_secs,
            self.interrupt.as_ref(),
            ui,
        )?;
        Ok(crate::agent::verify::verification::classify_output(
            Some(check),
            &output,
            self.interrupt.load(Ordering::SeqCst),
        ))
    }

    /// After `run_focused_check` recorded `status`: witness a pass, remember it for bisection,
    /// or bisect a regression. Returns a line for the check output, if any.
    pub(in crate::agent) fn counterfactual_after_check(
        &mut self,
        check: &FocusedCheck,
        status: CheckStatus,
        ui: &mut dyn Ui,
    ) -> Option<String> {
        let experimental = &self.config.experimental;
        let (witness, bisect_runs) = (experimental.fault_witness, experimental.edit_bisection_runs);
        if (!witness && bisect_runs == 0)
            || self.step_checks.mutations == 0
            || self.step_checks.evidence.is_repro(&check.command)
            || self.interrupt.load(Ordering::SeqCst)
        {
            return None;
        }
        let versions = self.turn_versions_now()?;
        match status {
            CheckStatus::Passed => {
                if bisect_runs > 0
                    && let Some(snapshot) = snapshot(&versions)
                {
                    self.step_checks
                        .counterfactual
                        .passing
                        .insert(check.command.clone(), snapshot);
                }
                if witness && behavioral(self.config, check) {
                    return self.fault_witness(check, &versions, ui);
                }
                None
            }
            CheckStatus::CodeFailed if bisect_runs > 0 => {
                let passing = self
                    .step_checks
                    .counterfactual
                    .passing
                    .get(&check.command)?;
                let changed = changed_since(&versions, passing);
                if changed.len() < 2 || changed.len() > MAX_BISECT_FILES {
                    return None;
                }
                self.bisect(check, changed, bisect_runs, ui)
            }
            _ => None,
        }
    }

    fn fault_witness(
        &mut self,
        check: &FocusedCheck,
        versions: &[TurnVersion],
        ui: &mut dyn Ui,
    ) -> Option<String> {
        let revision = self.step_checks.revision();
        if self
            .step_checks
            .counterfactual
            .witnesses
            .get(&check.name)
            .is_some_and(|witness| witness["revision"] == json!(revision))
        {
            return None;
        }
        let reverted: Overrides = versions
            .iter()
            .filter(|version| version.in_project && version.before != version.after)
            .map(|version| (version.path.clone(), version.before.clone()))
            .collect();
        if reverted.is_empty() {
            return None;
        }
        ui.notice(&format!(
            "Fault witness: rerunning {} without this turn's edits",
            check.name
        ));
        let (verdict, line) = match self.counterfactual_run(check, &reverted, ui) {
            Ok(CheckStatus::CodeFailed) => (
                "fails_without_turn_changes",
                format!(
                    "[fault witness: {} fails with this turn's edits reverted, so it exercises the change]",
                    check.name
                ),
            ),
            Ok(CheckStatus::Passed) => (
                "passes_without_turn_changes",
                format!(
                    "[fault witness: {} also passes with this turn's edits reverted, so it does not show the change works; add or run a check that fails without it]",
                    check.name
                ),
            ),
            Ok(other) => (
                "inconclusive",
                format!(
                    "[fault witness: inconclusive ({}) for {}]",
                    json!(other).as_str().unwrap_or("unknown"),
                    check.name
                ),
            ),
            Err(error) => (
                "inconclusive",
                format!("[fault witness: not run for {}: {error:#}]", check.name),
            ),
        };
        self.step_checks.counterfactual.witnesses.insert(
            check.name.clone(),
            json!({"verdict":verdict,"revision":revision,"reverted_files":reverted.keys().collect::<Vec<_>>()}),
        );
        Some(line)
    }

    fn bisect(
        &mut self,
        check: &FocusedCheck,
        changed: Overrides,
        budget: u8,
        ui: &mut dyn Ui,
    ) -> Option<String> {
        ui.notice(&format!(
            "Edit bisection: locating the change that broke {}",
            check.name
        ));
        let run = |agent: &Self, files: &[String], ui: &mut dyn Ui| {
            let overrides: Overrides = files
                .iter()
                .filter_map(|path| {
                    changed
                        .get(path)
                        .map(|content| (path.clone(), content.clone()))
                })
                .collect();
            agent.counterfactual_run(check, &overrides, ui).ok()
        };
        let all: Vec<String> = changed.keys().cloned().collect();
        // Reverting everything must restore the pass, or the failure is not explained by these edits.
        let confirmed = run(self, &all, ui) == Some(CheckStatus::Passed);
        let mut runs = 1u8;
        let mut candidates = all.clone();
        let mut stopped = None;
        if confirmed {
            while let Some(half) = next_half(&candidates) {
                if runs >= budget {
                    stopped = Some("run budget exhausted");
                    break;
                }
                let half = half.to_vec();
                runs += 1;
                match run(self, &half, ui) {
                    Some(CheckStatus::Passed) => candidates = half,
                    Some(CheckStatus::CodeFailed) => candidates.retain(|path| !half.contains(path)),
                    _ => {
                        stopped = Some("inconclusive run");
                        break;
                    }
                }
            }
        }
        let culprit = (confirmed && candidates.len() == 1).then(|| candidates[0].clone());
        let report = json!({
            "changed_since_last_pass": all,
            "reverting_all_restores_pass": confirmed,
            "candidates": candidates,
            "culprit": culprit,
            "runs": runs,
            "stopped": stopped,
            "assumption": "a single file's change caused the regression",
        });
        self.step_checks
            .counterfactual
            .bisections
            .insert(check.name.clone(), report);
        Some(match (&culprit, confirmed) {
            (Some(path), _) => format!(
                "[edit bisection: reverting only {path} to its content at the last pass of {} makes it pass again ({runs} isolated runs)]",
                check.name
            ),
            (None, true) => format!(
                "[edit bisection: the regression is within {} ({runs} isolated runs; {})]",
                candidates.join(", "),
                stopped.unwrap_or("files interact")
            ),
            (None, false) => format!(
                "[edit bisection: reverting all {} files changed since the last pass did not restore {}; the failure is not explained by those edits alone]",
                all.len(),
                check.name
            ),
        })
    }
}

#[cfg(test)]
mod tests;

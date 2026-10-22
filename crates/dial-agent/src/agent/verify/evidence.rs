//! Controller-observed check evidence, scoped to the effects it actually covered.
//! History/baselines are diagnostics; only latest-revision observations earn outcome credit.
use super::verification::{CheckStatus, FocusedCheck};
use serde::Serialize;
use std::collections::BTreeMap;

pub(in crate::agent) type Revision = [u64; 3];

/// Model-registered reproductions per turn (`check` `repro`).
pub(in crate::agent) const MAX_REPROS: usize = 4;

/// Call-id prefix of controller-run checks (`run_focused_check`: `check` and the completion gate).
pub(in crate::agent) const FOCUSED_CHECK_CALL: &str = "focused-check:";

#[derive(Clone, Serialize)]
struct Observation {
    call_id: String,
    status: CheckStatus,
    revision: Revision,
    /// Original execution revision never changes on reuse.
    #[serde(skip_serializing_if = "Option::is_none")]
    validated_revision: Option<Revision>,
    invalidated: bool,
    #[serde(skip)]
    inputs: Option<super::inputs::Snapshot>,
    #[serde(skip)]
    input_check: Option<FocusedCheck>,
    behavioral: bool,
    diagnostic: Option<String>,
}

impl Observation {
    fn current(&self, revision: Revision) -> bool {
        !self.invalidated && self.validated_revision.unwrap_or(self.revision) == revision
    }
}

/// Controller-run history of one registered reproduction. Model `bash` runs of the same text are
/// ordinary observations, not reproduction evidence.
#[derive(Clone, Copy, Default)]
struct Reproduction {
    last_failed: Option<Revision>,
    /// Status, full evidence revision, and reproduction-excluded change identity.
    last: Option<(CheckStatus, Revision, Revision)>,
}

/// Where a registered reproduction stands at a revision. Only `Fixed` earns behavioral credit;
/// `NotPassing` and `Stale` withhold all behavioral credit, like a failing configured behavioral check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::agent) enum ReproState {
    NotShownToReproduce,
    NotPassing,
    Stale,
    NotShownFixed,
    Fixed,
}

impl ReproState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotShownToReproduce => "not_shown_to_reproduce",
            Self::NotPassing => "not_passing",
            Self::Stale => "stale",
            Self::NotShownFixed => "not_shown_fixed",
            Self::Fixed => "fixed",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Self::NotShownToReproduce => {
                "not shown to reproduce (no code failure observed this turn); no behavioral credit"
            }
            Self::NotPassing => "reproduces the bug; not passing yet",
            Self::Stale => "stale; rerun it after the latest change",
            Self::NotShownFixed => {
                "passed with no change since it last failed; no behavioral credit"
            }
            Self::Fixed => {
                "failed before a change and passes now; behavioral credit at the current edits"
            }
        }
    }
}

#[derive(Default)]
pub(in crate::agent) struct Ledger {
    latest: BTreeMap<String, Observation>,
    baseline: BTreeMap<String, Observation>,
    /// Turn-scoped: the ledger is rebuilt with `StepChecks` at every user turn.
    repros: Vec<FocusedCheck>,
    reproductions: BTreeMap<String, Reproduction>,
}

impl Ledger {
    pub fn observe(
        &mut self,
        command: &str,
        call_id: &str,
        status: CheckStatus,
        revision: Revision,
        behavioral: bool,
    ) {
        self.observe_with_fix_revision(command, call_id, status, revision, behavioral, revision);
    }

    /// Reproduction attempts invalidate older evidence, but are not fixes in their own right.
    pub fn observe_with_fix_revision(
        &mut self,
        command: &str,
        call_id: &str,
        status: CheckStatus,
        revision: Revision,
        behavioral: bool,
        fix_revision: Revision,
    ) {
        if call_id.starts_with(FOCUSED_CHECK_CALL) && self.is_repro(command) {
            let entry = self.reproductions.entry(command.to_owned()).or_default();
            entry.last = Some((status, revision, fix_revision));
            if status == CheckStatus::CodeFailed {
                entry.last_failed = Some(fix_revision);
            }
        }
        self.latest.insert(
            command.to_owned(),
            Observation {
                call_id: call_id.to_owned(),
                status,
                revision,
                validated_revision: None,
                invalidated: false,
                inputs: None,
                input_check: None,
                behavioral,
                diagnostic: None,
            },
        );
    }

    pub(super) fn bind_inputs(&mut self, check: &FocusedCheck, inputs: super::inputs::Snapshot) {
        if let Some(observation) = self.latest.get_mut(&check.command)
            && observation.status == CheckStatus::Passed
        {
            observation.inputs = Some(inputs);
            observation.input_check = Some(check.clone());
        }
    }

    pub(super) fn has_inputs(&self, command: &str) -> bool {
        self.latest
            .get(command)
            .is_some_and(|o| o.inputs.is_some() && !o.invalidated)
    }

    pub(super) fn invalidate_inputs(&mut self) {
        for observation in self.latest.values_mut().filter(|o| o.inputs.is_some()) {
            observation.invalidated = true;
        }
    }

    pub(super) fn observed_revision(&self, command: &str) -> Option<Revision> {
        self.latest.get(command).map(|o| o.revision)
    }

    pub(super) fn input_checks(&self) -> Vec<FocusedCheck> {
        self.latest
            .values()
            .filter_map(|observation| observation.input_check.clone())
            .collect()
    }

    /// A failed/unknown revalidation is sticky until a real execution supplies new evidence.
    /// Returns whether the observation is invalid, including a prior failed revalidation.
    pub(super) fn validate_inputs(
        &mut self,
        command: &str,
        inputs: Option<&super::inputs::Snapshot>,
        revision: Revision,
    ) -> bool {
        let Some(observation) = self.latest.get_mut(command) else {
            return false;
        };
        let valid = inputs.is_some()
            && observation.inputs.as_ref() == inputs
            && observation.revision[1..] == revision[1..];
        observation.invalidated |= !valid;
        observation.invalidated
    }

    pub(super) fn reuse_inputs(
        &mut self,
        command: &str,
        inputs: &super::inputs::Snapshot,
        revision: Revision,
    ) -> bool {
        if self.is_repro(command) {
            return false;
        }
        self.validate_inputs(command, Some(inputs), revision);
        let Some(observation) = self.latest.get_mut(command) else {
            return false;
        };
        if observation.invalidated
            || observation.status != CheckStatus::Passed
            || observation.inputs.as_ref() != Some(inputs)
        {
            return false;
        }
        observation.validated_revision = Some(revision);
        true
    }

    /// Keep a bounded, exact diagnostic identity rather than comparing exit status alone.
    /// Oversized/empty output is not enough evidence to suppress a potentially new failure.
    pub fn diagnose(&mut self, command: &str, output: &str) {
        use sha2::{Digest, Sha256};
        if let Some(observation) = self.latest.get_mut(command) {
            observation.diagnostic = (!output.trim().is_empty() && output.len() <= 65_536)
                .then(|| format!("{:x}", Sha256::digest(output.as_bytes())));
        }
    }

    /// Baselines stay visible, but unchanged failures outside declared behavioral/repro scope
    /// must not manufacture unrelated repair assignments. Unknown or changed diagnostics remain
    /// conservative: they can never be suppressed merely because the baseline also failed.
    pub fn unchanged_unrelated_failure(&self, command: &str) -> bool {
        let (Some(before), Some(now)) = (self.baseline.get(command), self.latest.get(command))
        else {
            return false;
        };
        !now.behavioral
            && !self.is_repro(command)
            && before.status == CheckStatus::CodeFailed
            && now.status == CheckStatus::CodeFailed
            && before.diagnostic.is_some()
            && before.diagnostic == now.diagnostic
    }

    /// Check subprocesses may themselves advance an uncheckpointed-effect epoch. Bind their
    /// observation after the owner settles that same call, never after some unrelated mutation.
    pub fn settle_call(&mut self, call_id: &str, revision: Revision) {
        for observation in self.latest.values_mut().filter(|o| o.call_id == call_id) {
            observation.revision = revision;
        }
    }

    pub fn capture_baseline(&mut self) {
        self.baseline = self.latest.clone();
    }

    /// Register or replace (same name) a reproduction. A replacement starts with no history, so
    /// a corrected command must fail again before it can earn credit. Returns the replaced check.
    pub fn register_repro(&mut self, check: FocusedCheck) -> Result<Option<FocusedCheck>, String> {
        if let Some(existing) = self.repros.iter_mut().find(|r| r.name == check.name) {
            let old = std::mem::replace(existing, check);
            self.reproductions.remove(&old.command);
            // A withdrawn command is no longer evidence of anything; do not leave it as stale.
            self.latest.remove(&old.command);
            return Ok(Some(old));
        }
        if self.repros.len() >= MAX_REPROS {
            return Err(format!(
                "at most {MAX_REPROS} reproductions per turn; reuse a name to replace one"
            ));
        }
        self.repros.push(check);
        Ok(None)
    }

    pub fn repros(&self) -> &[FocusedCheck] {
        &self.repros
    }

    pub fn is_repro(&self, command: &str) -> bool {
        self.repros.iter().any(|check| check.command == command)
    }

    /// Credit needs a controller-observed code failure before an outside change and a
    /// controller-observed pass at `revision`. Reproduction effects alone are not a fix.
    pub fn repro_state(&self, command: &str, revision: Revision) -> ReproState {
        let history = self.reproductions.get(command).copied().unwrap_or_default();
        let Some(failed) = history.last_failed else {
            return ReproState::NotShownToReproduce;
        };
        match history.last {
            None => ReproState::Stale,
            Some((_, observed, _)) if observed != revision => ReproState::Stale,
            Some((status, _, _)) if status != CheckStatus::Passed => ReproState::NotPassing,
            Some((_, _, fixed)) if failed == fixed => ReproState::NotShownFixed,
            Some(_) => ReproState::Fixed,
        }
    }

    /// Every configured behavioral check and every reproduction shown to fail must pass at
    /// `revision`; each reproduction fixed across a change adds one.
    pub fn passing_behavioral(&self, revision: Revision) -> usize {
        let behavioral: Vec<_> = self
            .latest
            .iter()
            .filter(|(command, o)| o.behavioral && !self.is_repro(command))
            .map(|(_, o)| o)
            .collect();
        if behavioral
            .iter()
            .any(|o| !o.current(revision) || o.status != CheckStatus::Passed)
        {
            return 0;
        }
        let mut fixed = 0;
        for check in &self.repros {
            match self.repro_state(&check.command, revision) {
                ReproState::Fixed => fixed += 1,
                ReproState::NotShownToReproduce | ReproState::NotShownFixed => {}
                ReproState::NotPassing | ReproState::Stale => return 0,
            }
        }
        behavioral.len() + fixed
    }

    /// The most recent status recorded for `command` at any revision (`check failed_only`).
    pub fn latest_status(&self, command: &str) -> Option<CheckStatus> {
        self.latest
            .get(command)
            .map(|observation| observation.status)
    }

    /// Missing and old observations are not interchangeable with a current passing result.
    pub fn current_status(&self, command: &str, revision: Revision) -> &'static str {
        match self.latest.get(command) {
            None => "not_observed",
            Some(observation) if !observation.current(revision) => "stale",
            Some(observation) => match observation.status {
                CheckStatus::Passed => "passed",
                CheckStatus::CodeFailed => "code_failed",
                CheckStatus::EnvironmentUnavailable => "environment_unavailable",
                CheckStatus::PermissionDenied => "permission_denied",
                CheckStatus::Cancelled => "cancelled",
                CheckStatus::TimedOut => "timed_out",
                CheckStatus::Unverified => "unverified",
            },
        }
    }

    pub fn snapshot(&self, revision: Revision) -> serde_json::Value {
        let current: Vec<_> = self
            .latest
            .values()
            .filter(|o| o.current(revision))
            .collect();
        let reproductions: Vec<_> = self
            .repros
            .iter()
            .map(|check| {
                serde_json::json!({
                    "name": check.name,
                    "command": check.command,
                    "state": self.repro_state(&check.command, revision).as_str(),
                })
            })
            .collect();
        serde_json::json!({
            "revision": revision,
            "current_checks": current.len(),
            "passing_checks": current.iter().filter(|o| o.status == CheckStatus::Passed).count(),
            "stale_checks": self.latest.len() - current.len(),
            "passing_behavioral_checks": self.passing_behavioral(revision),
            "reproductions": reproductions,
            "latest": self.latest,
            "baseline": self.baseline,
        })
    }
}

#[cfg(test)]
mod tests;

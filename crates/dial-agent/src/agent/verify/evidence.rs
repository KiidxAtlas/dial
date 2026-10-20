//! Controller-observed check evidence, scoped to the effects it actually covered.
//! History/baselines are diagnostics; only latest-revision observations earn outcome credit.
use super::verification::CheckStatus;
use serde::Serialize;
use std::collections::BTreeMap;

pub(in crate::agent) type Revision = [u64; 3];

#[derive(Clone, Serialize)]
struct Observation {
    call_id: String,
    status: CheckStatus,
    revision: Revision,
    behavioral: bool,
}

#[derive(Default)]
pub(in crate::agent) struct Ledger {
    latest: BTreeMap<String, Observation>,
    baseline: BTreeMap<String, Observation>,
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
        self.latest.insert(
            command.to_owned(),
            Observation {
                call_id: call_id.to_owned(),
                status,
                revision,
                behavioral,
            },
        );
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

    pub fn passing_behavioral(&self, revision: Revision) -> usize {
        let behavioral: Vec<_> = self.latest.values().filter(|o| o.behavioral).collect();
        if behavioral
            .iter()
            .any(|o| o.revision != revision || o.status != CheckStatus::Passed)
        {
            0
        } else {
            behavioral.len()
        }
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
            Some(observation) if observation.revision != revision => "stale",
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
            .filter(|o| o.revision == revision)
            .collect();
        serde_json::json!({
            "revision": revision,
            "current_checks": current.len(),
            "passing_checks": current.iter().filter(|o| o.status == CheckStatus::Passed).count(),
            "stale_checks": self.latest.len() - current.len(),
            "passing_behavioral_checks": self.passing_behavioral(revision),
            "latest": self.latest,
            "baseline": self.baseline,
        })
    }
}

#[cfg(test)]
mod tests;

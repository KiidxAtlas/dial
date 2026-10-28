//! Worker-only model and thinking settings. They are validated when queued, then applied to one
//! worker's backend at its next request boundary. The launched config is never changed.

use crate::agent::modes::switch;
use crate::config::thinking::parse_thinking;
use crate::config::types::Config;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// Settings for one worker. An absent field keeps what the worker already runs with.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkerSettings {
    pub model: Option<String>,
    pub thinking: Option<String>,
}

impl WorkerSettings {
    pub fn is_empty(&self) -> bool {
        self.model.is_none() && self.thinking.is_none()
    }

    /// Overlays `update` field by field, so a later update never clears an earlier choice.
    pub fn merge(&mut self, update: WorkerSettings) {
        if update.model.is_some() {
            self.model = update.model;
        }
        if update.thinking.is_some() {
            self.thinking = update.thinking;
        }
    }
}

/// Settings accepted for one running worker. `queued` is set by each accepted update and cleared when the
/// worker takes them at a request boundary; `inflight` is the revision that take captured, the only revision
/// that application can acknowledge or fail. Newer updates stay queued and are never acknowledged early.
#[derive(Debug, Default)]
pub(super) struct SettingsSlot {
    pub(super) accepted: WorkerSettings,
    pub(super) queued: bool,
    pub(super) applied: bool,
    /// Bumped by each accepted update.
    pub(super) revision: u64,
    /// The highest revision whose application succeeded.
    pub(super) applied_revision: u64,
    /// Revision taken at the current request boundary and not yet acknowledged or failed.
    pub(super) inflight: Option<u64>,
    /// The revision whose application failed, and the error. The worker keeps its previous backend.
    pub(super) failure: Option<(u64, String)>,
}

impl SettingsSlot {
    /// Typed settings state. A queued update always reads as queued, so an earlier failure never masks it.
    pub(super) fn state(&self) -> &'static str {
        if self.queued {
            "queued"
        } else if self.failure.is_some() {
            "not_applied"
        } else if self.revision == 0 {
            "launch"
        } else if self.applied_revision == self.revision {
            "applied"
        } else {
            "applying"
        }
    }

    /// One-line worker status for progress prose. Consumers read `metadata` instead of parsing it.
    pub(super) fn status(&self) -> String {
        let state = match (self.state(), &self.failure) {
            ("not_applied", Some((_, error))) => format!("not applied: {error}"),
            ("launch", _) => "launch config".to_owned(),
            (label, _) => label.to_owned(),
        };
        format!(
            "config rev {} {state}; model {}, thinking {}",
            self.revision,
            self.accepted.model.as_deref().unwrap_or("inherited"),
            self.accepted.thinking.as_deref().unwrap_or("inherited"),
        )
    }

    /// Typed settings metadata published on the worker body: requested and applied revisions and state.
    pub(super) fn metadata(&self) -> serde_json::Value {
        serde_json::json!({
            "revision": self.revision,
            "applied_revision": self.applied_revision,
            "state": self.state(),
            "requested": {"model": self.accepted.model, "thinking": self.accepted.thinking},
            "failure": self.failure.as_ref().map(|(_, error)| error),
        })
    }

    /// Takes queued settings at a request boundary, capturing their revision as the one in flight.
    pub(super) fn take(&mut self) -> Option<WorkerSettings> {
        if !self.queued {
            return None;
        }
        self.queued = false;
        self.inflight = Some(self.revision);
        // An earlier revision's failure is superseded by the one now being applied.
        self.failure = None;
        Some(self.accepted.clone())
    }

    /// Acknowledges the in-flight revision as applied. Newer queued updates are left untouched.
    pub(super) fn acknowledge(&mut self) {
        let revision = self.inflight.take().unwrap_or(self.applied_revision);
        self.applied = true;
        self.applied_revision = self.applied_revision.max(revision);
        if self
            .failure
            .as_ref()
            .is_some_and(|(failed, _)| *failed <= revision)
        {
            self.failure = None;
        }
    }

    /// Records that the in-flight revision could not be applied. The previous backend stays active.
    pub(super) fn fail(&mut self, error: &str) {
        let revision = self.inflight.take().unwrap_or(self.applied_revision);
        self.failure = Some((revision, error.to_owned()));
    }
}

/// The config a request runs with once `settings` overlays the worker's launched `base`.
pub fn apply_settings(base: &Config, settings: &WorkerSettings) -> Result<Config> {
    let selection = match &settings.model {
        Some(selector) => switch::resolve(base, selector)?,
        None => base.selection().clone(),
    };
    let effort = match settings.thinking.as_deref() {
        None => None,
        Some(level) => {
            let level = parse_thinking(level).map_err(anyhow::Error::msg)?;
            match level.reasoning_effort() {
                Some(effort) => Some(effort),
                None => {
                    bail!("`auto` thinking is judged per user turn; a worker needs a fixed level")
                }
            }
        }
    };
    switch::switched_config(base, &selection, effort)
}

#[cfg(test)]
mod tests;

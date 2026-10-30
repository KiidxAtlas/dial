//! `[solve]` and `[escalation]` settings.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SolveConfig {
    pub enabled: bool,
    pub max_candidates: usize,
    pub max_steps: u32,
    pub max_snapshot_bytes: usize,
    /// Independent candidate slots; local inference still obeys its serial owner.
    pub concurrency: usize,
}

impl Default for SolveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_candidates: 0,
            max_steps: 0,
            max_snapshot_bytes: 0,
            concurrency: 1,
        }
    }
}

impl SolveConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=64).contains(&self.concurrency),
            "solve.concurrency must be 1..=64"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EscalationConfig {
    pub enabled: bool,
    pub allow_remote: bool,
    pub max_evidence_bytes: usize,
    /// Consecutive failed tool rounds on one turn before the coach role is consulted automatically. 0 disables.
    pub auto_after_failed_rounds: u32,
    /// Automatic consultations allowed per turn.
    pub max_auto_per_turn: u32,
}

impl Default for EscalationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allow_remote: false,
            max_evidence_bytes: 0,
            auto_after_failed_rounds: 3,
            max_auto_per_turn: 1,
        }
    }
}

impl EscalationConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.auto_after_failed_rounds <= 8,
            "escalation.auto_after_failed_rounds must be 0..8"
        );
        ensure!(
            self.max_auto_per_turn <= 8,
            "escalation.max_auto_per_turn must be 0..8"
        );
        Ok(())
    }
}

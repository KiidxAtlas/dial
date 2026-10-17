//! `[reliability]` settings.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub enabled: bool,
    pub measured_interfaces: bool,
    pub examples: bool,
    pub max_contract_failures: u32,
    /// When a progress guard would end the turn (repeated observations, a tool failing in a row,
    /// stalled todo updates), resume the model with a continue notice this many times per turn
    /// before asking the user for direction. 0 asks immediately.
    pub auto_continue_rounds: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            measured_interfaces: false,
            examples: true,
            max_contract_failures: 3,
            auto_continue_rounds: 2,
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=8).contains(&self.max_contract_failures),
            "reliability.max_contract_failures must be 1..8"
        );
        Ok(())
    }
}

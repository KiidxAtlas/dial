//! `[stats]` settings for the local usage database.

use serde::{Deserialize, Serialize};

/// `[stats]`: local usage ledger and dashboard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StatsConfig {
    /// Record per-request and per-tool usage into the local stats database.
    pub enabled: bool,
    /// Dashboard port for `dial stats --serve` (loopback only).
    pub port: u16,
}

impl Default for StatsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: DEFAULT_PORT,
        }
    }
}

pub const DEFAULT_PORT: u16 = 3847;

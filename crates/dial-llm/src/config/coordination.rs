//! Lightweight coordination between independent sessions in the same checkout.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CoordinationConfig {
    pub enabled: bool,
    /// Let open interactive sessions resume unfinished tasks when fresh peer mail arrives.
    pub auto_wake: bool,
}

impl Default for CoordinationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_wake: true,
        }
    }
}

#[cfg(test)]
mod tests;

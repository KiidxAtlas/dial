//! Lightweight coordination between independent sessions in the same checkout.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CoordinationConfig {
    pub enabled: bool,
}

impl Default for CoordinationConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[cfg(test)]
mod tests;

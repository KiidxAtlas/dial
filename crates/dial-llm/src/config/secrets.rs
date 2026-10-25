//! `[secrets]`: mask secret values before conversation content reaches a hosted provider.

use serde::{Deserialize, Serialize};

/// Shortest secret that is masked. Shorter values collide with ordinary words and numbers.
pub const MIN_SECRET_BYTES: usize = 8;

/// `[secrets]`: provider-bound masking (`llm::secrets`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Replace secrets in provider-bound messages with reversible placeholders.
    pub enabled: bool,
    /// Also mask the values of secret-named environment variables (`*_API_KEY`, `*_TOKEN`, ...).
    pub env: bool,
    /// Extra literal secrets to mask.
    pub values: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            env: true,
            values: Vec::new(),
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        for value in &self.values {
            if value.len() < MIN_SECRET_BYTES {
                return Err(format!(
                    "secrets.values entries must be at least {MIN_SECRET_BYTES} bytes; shorter values match ordinary text"
                ));
            }
        }
        Ok(())
    }
}

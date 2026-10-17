//! `[steering]` settings.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub enabled: bool,
    /// Build and admit vectors during automatic improvement.
    pub automatic: bool,
    pub vector: String,
    pub strength: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            automatic: true,
            vector: "coding".into(),
            strength: 0.5,
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.vector.is_empty()
                && self.vector.len() <= 64
                && self
                    .vector
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
            "steering vector name must use 1–64 letters, digits, - or _"
        );
        ensure!(
            self.strength.is_finite() && (0.0..=2.0).contains(&self.strength),
            "steering strength must be 0..2"
        );
        Ok(())
    }
}

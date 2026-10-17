//! `[adaptive]` settings.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub enabled: bool,
    pub sampling: bool,
    pub reasoning: bool,
    pub output_limits: bool,
    pub pin_sampling: bool,
    pub phase: Phase,
    pub investigation_temperature: f64,
    pub editing_temperature: f64,
    pub verification_temperature: f64,
    pub recovery_temperature: f64,
    pub exploration_temperature: f64,
    pub investigation_tokens: u32,
    pub verification_tokens: u32,
    pub recovery_tokens: u32,
    pub exploration_tokens: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            sampling: false,
            reasoning: true,
            output_limits: false,
            pin_sampling: false,
            phase: Phase::Auto,
            investigation_temperature: 0.2,
            editing_temperature: 0.15,
            verification_temperature: 0.05,
            recovery_temperature: 0.1,
            exploration_temperature: 0.5,
            investigation_tokens: 2048,
            verification_tokens: 1536,
            recovery_tokens: 2048,
            exploration_tokens: 4096,
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        for t in [
            self.investigation_temperature,
            self.editing_temperature,
            self.verification_temperature,
            self.recovery_temperature,
            self.exploration_temperature,
        ] {
            ensure!(
                t.is_finite() && (0.0..=2.0).contains(&t),
                "adaptive temperatures must be finite in 0..2"
            );
        }
        for n in [
            self.investigation_tokens,
            self.verification_tokens,
            self.recovery_tokens,
            self.exploration_tokens,
        ] {
            ensure!(
                (256..=32768).contains(&n),
                "adaptive output budgets must be 256..32768"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Auto,
    Investigation,
    Editing,
    Verification,
    Recovery,
    Exploration,
    General,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Investigation => "investigation",
            Self::Editing => "editing",
            Self::Verification => "verification",
            Self::Recovery => "recovery",
            Self::Exploration => "exploration",
            Self::General => "general",
        }
    }
}

//! Dial Laya decision-service settings and checkpoint resolution (data only; the service client stays in `agent::laya`).

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionEngineType {
    #[default]
    Laya,
    Kev,
}

/// Provider name that marks `model_roles.decision` as the Laya checkpoint.
pub const PROVIDER: &str = "laya";

/// Model id of a `judge` role served by Laya. The judge shares the service, and so the checkpoint, of the
/// `decision` role (else the installed default), so judging never restarts the service with another one.
pub const JUDGE_MODEL: &str = "decision";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub engine: DecisionEngineType,
    pub enabled: bool,
    pub auto_start: bool,
    pub endpoint: String,
    pub timeout_secs: u64,
    /// Checkpoint the service loads: the assigned `decision` role. Set from the model roles, never read from TOML.
    #[serde(skip)]
    pub checkpoint: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            engine: DecisionEngineType::Laya,
            enabled: false,
            auto_start: true,
            endpoint: "http://127.0.0.1:8002".into(),
            timeout_secs: 0,
            checkpoint: None,
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.engine == DecisionEngineType::Laya,
            "decision engine `kev` has no implementation in this build; set [laya] engine = \"laya\""
        );
        let authority = self
            .endpoint
            .strip_prefix("http://")
            .context("Decision engine requires a local http endpoint")?;
        let (host, port) = authority
            .split_once(':')
            .context("Decision engine endpoint requires an explicit port")?;
        ensure!(
            matches!(host, "127.0.0.1" | "localhost") && port.parse::<u16>().is_ok_and(|p| p > 0),
            "Decision engine endpoint must be http://127.0.0.1:PORT or http://localhost:PORT, without a path"
        );
        Ok(())
    }
}

/// The checkpoint the service must load: the assigned `decision` role, else the installed default.
pub fn expected_checkpoint(config: &Config) -> Option<String> {
    config.checkpoint.clone().or_else(|| {
        let model = runtime_dir().ok()?.join("model");
        model
            .join("rl_agent_config.json")
            .is_file()
            .then(|| model.to_string_lossy().into_owned())
    })
}

pub fn runtime_dir() -> Result<std::path::PathBuf> {
    let root = super::paths::user_dir()?;
    let root = if root.is_absolute() {
        root
    } else {
        std::env::current_dir()?.join(root)
    };
    Ok(root.join("laya"))
}

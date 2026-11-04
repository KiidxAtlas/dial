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

/// Provider for Laya typed choices through the unified `decision` role.
pub const PROVIDER: &str = "laya";

/// Model id selecting the Laya engine; its checkpoint is a separate service setting.
pub const JUDGE_MODEL: &str = "decision";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub engine: DecisionEngineType,
    pub enabled: bool,
    pub auto_start: bool,
    pub endpoint: String,
    pub timeout_secs: u64,
    /// Local path or Hugging Face repository loaded by the managed service.
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
            self.timeout_secs <= 86400,
            "laya.timeout_secs must be 0 (unlimited) or 1..86400"
        );
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
        ensure!(
            self.checkpoint
                .as_ref()
                .is_none_or(|value| !value.trim().is_empty()),
            "laya.checkpoint must be nonempty when set"
        );
        Ok(())
    }
}

/// Explicit service checkpoint, else the installed default.
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
    Ok(super::paths::laya_dir(&root))
}

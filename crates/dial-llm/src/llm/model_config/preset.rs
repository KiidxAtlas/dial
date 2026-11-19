//! Named model presets: role assignments, the main thinking level, and portable boolean controls.
//! Config files store them under `[model_presets]`; `config::types` re-exports the type unchanged.
use super::ModelSelection;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A named model preset: role assignments, optional vision/checkpoint selections and thinking.
/// `main` (with its `thinking` level) restores the Conversation model; `roles` holds the helper
/// roles, each with its own optional `thinking`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelPreset {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main: Option<ModelSelection>,
    pub roles: BTreeMap<String, ModelSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<crate::llm::profiles::ReasoningEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vision: Option<crate::config::vision::Config>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub laya_checkpoint: Option<String>,
    /// Portable scalar controls only; never model routes, credentials or machine paths.
    #[serde(
        default,
        deserialize_with = "deserialize_setup",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub setup: BTreeMap<String, toml::Value>,
}

impl ModelPreset {
    /// The explicit portable setup allowlist. All entries are boolean controls.
    pub const SETUP_KEYS: &[&str] = &[
        "fast_mode",
        "slow_mode",
        "extended_context",
        "ui.tips",
        "memory.enabled",
        "laya.enabled",
        "assistance.teacher_enabled",
        "improve.distill",
        "escalation.enabled",
        "escalation.allow_remote",
    ];

    /// Validate on application too, since callers can construct presets in memory.
    pub fn validate_setup(&self) -> anyhow::Result<()> {
        for (key, value) in &self.setup {
            anyhow::ensure!(
                Self::SETUP_KEYS.contains(&key.as_str()) && value.is_bool(),
                "unsafe or unknown setup setting: {key}; only allowlisted boolean controls are supported"
            );
        }
        Ok(())
    }
}

fn deserialize_setup<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, toml::Value>, D::Error> {
    let setup = BTreeMap::<String, toml::Value>::deserialize(deserializer)?;
    let preset = ModelPreset {
        setup,
        ..Default::default()
    };
    preset.validate_setup().map_err(serde::de::Error::custom)?;
    Ok(preset.setup)
}

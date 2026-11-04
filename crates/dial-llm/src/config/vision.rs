//! Explicit model selections for optional vision tools, independent of chat model roles.
use super::types::{Config as RootConfig, MAX_CONTEXT_WINDOW};
use crate::llm::model_config::{ApiProtocol, ModelSelection};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub grounding: Option<ModelSelection>,
    pub image_embedding: Option<ModelSelection>,
}

impl RootConfig {
    /// Only explicit feature selections enable vision tools; the main model is not a fallback.
    pub fn vision_selection(&self, feature: &str) -> Option<&ModelSelection> {
        match feature {
            "grounding" => self.vision.grounding.as_ref(),
            "image_embedding" => self.vision.image_embedding.as_ref(),
            _ => None,
        }
    }

    /// Resolve an explicit feature model using the shared provider/endpoint selection rules.
    /// Callers must check `vision_selection` before offering or invoking an optional tool.
    pub fn for_vision(&self, feature: &str) -> Self {
        self.vision_selection(feature)
            .map_or_else(|| self.clone(), |selection| self.with_selection(selection))
    }

    pub(crate) fn validate_vision(&self) -> Result<()> {
        for feature in ["grounding", "image_embedding"] {
            let Some(selection) = self.vision_selection(feature) else {
                continue;
            };
            ensure!(
                !selection.model.trim().is_empty(),
                "vision.{feature} has no model"
            );
            ensure!(
                selection
                    .context_window
                    .is_none_or(|w| (2048..=MAX_CONTEXT_WINDOW).contains(&w)),
                "invalid vision.{feature} context window"
            );
            if let Some(url) = &selection.base_url {
                crate::llm::provider::endpoint_url(url, selection.allow_insecure)?;
            }
            let resolved = self.for_vision(feature);
            match &resolved.endpoint {
                None => ensure!(
                    selection.provider == "mlx",
                    "vision.{feature} needs a local MLX model or an OpenAI-compatible endpoint"
                ),
                Some(endpoint) => {
                    ensure!(
                        endpoint
                            .protocol
                            .unwrap_or_else(|| ApiProtocol::builtin(&selection.provider))
                            == ApiProtocol::ChatCompletions,
                        "vision.{feature} needs an OpenAI-compatible endpoint (chat-completions protocol)"
                    );
                }
            }
        }
        Ok(())
    }
}

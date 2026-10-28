//! `[image]` settings: the OpenAI-compatible endpoint used by the `image_gen` tool.

use serde::{Deserialize, Serialize};

/// `[image]` configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ImageConfig {
    /// Base URL of an OpenAI-compatible API (`/images/generations`).
    pub base_url: String,
    /// Image model identifier sent with each generation request.
    pub model: String,
    /// Environment variable holding the API key. Unset uses the provider's standard variable
    /// (`OPENAI_API_KEY` for api.openai.com) or a key saved through `/providers`.
    pub api_key_env: Option<String>,
    /// Per request timeout in seconds. Zero disables the timeout.
    pub timeout_secs: u64,
    /// Offer `image_gen` without an `integrations` entry whenever a shared image route resolves
    /// with its required credentials, independently of the main chat model's vision capability.
    pub auto: bool,
}

impl Default for ImageConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-image-1".into(),
            api_key_env: None,
            timeout_secs: 180,
            auto: true,
        }
    }
}

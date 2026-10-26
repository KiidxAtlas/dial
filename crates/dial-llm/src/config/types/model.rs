//! Model access: presets, OpenAI-compatible endpoints, the context budget and stream timeouts.
use super::*;

/// A named model preset (`/modelpreset`): role model assignments plus the thinking level.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelPreset {
    pub roles: BTreeMap<String, crate::llm::model_config::ModelSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<crate::llm::profiles::ReasoningEffort>,
}

/// Use an OpenAI-compatible server (LM Studio, Ollama, llama.cpp, a hosted API) instead of the
/// supervised local MLX model. Weight updates are unavailable for endpoints.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EndpointConfig {
    pub base_url: String,
    /// Name of the environment variable holding the bearer token (the token itself is never stored).
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub allow_insecure: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<crate::llm::model_config::ApiProtocol>,
    #[serde(
        default,
        deserialize_with = "crate::llm::model_config::deserialize_headers",
        skip_serializing_if = "Option::is_none"
    )]
    pub headers: Option<crate::llm::model_config::RequestHeaders>,
    /// ChatGPT workspace pin. Other providers reject this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

pub use crate::llm::model_config::MAX_CONTEXT_WINDOW;

/// Operator context budget. Auto follows model metadata; a number caps retained history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContextWindow {
    #[default]
    Auto,
    Tokens(u32),
}

impl ContextWindow {
    pub fn tokens(self) -> Option<u32> {
        match self {
            Self::Auto => None,
            Self::Tokens(n) => Some(n),
        }
    }
}

impl Serialize for ContextWindow {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Auto => serializer.serialize_str("auto"),
            Self::Tokens(n) => serializer.serialize_u32(*n),
        }
    }
}

impl<'de> Deserialize<'de> for ContextWindow {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Input {
            Number(u32),
            Text(String),
        }
        match Input::deserialize(deserializer)? {
            Input::Text(s) if s.eq_ignore_ascii_case("auto") => Ok(Self::Auto),
            Input::Number(n) if (2048..=MAX_CONTEXT_WINDOW).contains(&n) => Ok(Self::Tokens(n)),
            _ => Err(serde::de::Error::custom(
                "context window must be 'auto' or 2048..2097152 tokens",
            )),
        }
    }
}

/// Default silence limit for remote model streams (Anthropic, ChatGPT, OpenAI-compatible hosts).
/// A stalled connection fails and retries instead of waiting forever.
pub const DEFAULT_REMOTE_IDLE_TIMEOUT_SECS: u64 = 300;

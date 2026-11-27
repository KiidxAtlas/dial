//! Provider specification, endpoint URL validation, protocol-to-wire routing, and metadata hints.
//!
//! Extracted from `provider.rs` so provider identity/validation rules live independently of
//! transport execution, retry loops, and response streaming.

use super::super as llm;
use super::super::model_config::ApiProtocol;
use anyhow::{Context, Result, bail, ensure};
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

/// `config.json` files larger than this are not read for the context hint.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
/// Provider and model labels of a [`ProviderKind::Unconfigured`] route, shown in status lines.
pub const UNCONFIGURED_PROVIDER: &str = "none";
pub const UNCONFIGURED_MODEL: &str = "no model";

/// Why a request on an unconfigured route fails, and how to fix it.
pub fn unconfigured_message(reason: &str) -> String {
    let reason = reason.trim();
    if reason.is_empty() {
        "No model is selected. Pick one with /model, or set up a provider in /providers.".to_owned()
    } else {
        format!(
            "No usable model ({reason}). Pick one with /model, or set up a provider in /providers."
        )
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum ProviderKind {
    /// Dial's own ChatGPT OAuth registration, refreshed per request.
    ChatGpt { model: String, user_dir: PathBuf },
    /// A local model directory served by a supervised `mlx_lm.server`.
    Local { model_dir: String },
    /// A remote (or self-hosted) OpenAI-compatible chat endpoint.
    Endpoint {
        base_url: String,
        model: String,
        api_key: Option<String>,
    },
    /// Anthropic's native Messages API endpoint.
    AnthropicEndpoint {
        base_url: String,
        model: String,
        api_key: Option<String>,
    },
    /// Claude subscription OAuth refreshed before each request.
    AnthropicSubscription { model: String, user_dir: PathBuf },
    /// Apple's on-device system model through Dial's FoundationModels helper (`llm::apple`).
    Apple,
    /// No usable model yet: the user skipped setup or the configured route could not be built.
    /// The shell runs; every model request fails with `reason` and a pointer to `/model`.
    Unconfigured { reason: String },
}

// Not derived: the key must never reach a log through `{:?}`.
impl fmt::Debug for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderKind::ChatGpt { model, .. } => {
                f.debug_struct("ChatGpt").field("model", model).finish()
            }
            ProviderKind::AnthropicSubscription { model, .. } => f
                .debug_struct("AnthropicSubscription")
                .field("model", model)
                .finish(),
            ProviderKind::Local { model_dir } => f
                .debug_struct("Local")
                .field("model_dir", model_dir)
                .finish(),
            ProviderKind::Endpoint {
                base_url,
                model,
                api_key,
            } => f
                .debug_struct("Endpoint")
                .field("base_url", &llm::display_url(base_url))
                .field("model", model)
                .field("api_key", &api_key.as_ref().map(|_| "<redacted>"))
                .finish(),
            ProviderKind::AnthropicEndpoint {
                base_url,
                model,
                api_key,
            } => f
                .debug_struct("AnthropicEndpoint")
                .field("base_url", &llm::display_url(base_url))
                .field("model", model)
                .field("api_key", &api_key.as_ref().map(|_| "<redacted>"))
                .finish(),
            ProviderKind::Apple => f.write_str("Apple"),
            ProviderKind::Unconfigured { .. } => f.write_str("Unconfigured"),
        }
    }
}

/// Validate an endpoint `base_url` and return the chat-completions URL.
pub fn endpoint_url(base_url: &str, allow_insecure: bool) -> Result<String> {
    // Unresolved built-in templates name the config setting that would fill them.
    if base_url.contains("<account>") {
        bail!(
            "endpoint base_url has an unset <account> placeholder: set snowflake_account (Snowflake Cortex) or cloudflare_account_id (Cloudflare AI Gateway)"
        );
    }
    if base_url.contains("<gateway>") {
        bail!(
            "endpoint base_url has an unset <gateway> placeholder: set cloudflare_gateway_id (Cloudflare AI Gateway)"
        );
    }
    // Messages below never quote `base_url`: it may carry userinfo.
    let uri: ureq::http::Uri = base_url
        .parse()
        .map_err(|_| anyhow::anyhow!("endpoint base_url is not a valid URL"))?;
    let scheme = uri.scheme_str().unwrap_or_default();
    ensure!(
        scheme == "http" || scheme == "https",
        "endpoint base_url must start with http:// or https://"
    );
    let Some(authority) = uri.authority() else {
        bail!("endpoint base_url has no host")
    };
    ensure!(
        !authority.as_str().contains('@'),
        "endpoint base_url must not contain credentials; use the api key setting"
    );
    ensure!(
        uri.query().is_none(),
        "endpoint base_url must not contain a query string"
    );
    let host = authority.host();
    ensure!(!host.is_empty(), "endpoint base_url has no host");
    if scheme == "http" && !allow_insecure && !llm::is_local_host(host) {
        bail!(
            "refusing plain http to non-local host {host}: use https, or opt in explicitly to insecure endpoints"
        );
    }
    Ok(llm::completions_url(base_url))
}

/// Validate an Anthropic base URL and return its Messages API URL.
pub fn anthropic_url(base_url: &str, allow_insecure: bool) -> Result<String> {
    let completions = endpoint_url(base_url, allow_insecure)?;
    let prefix = completions
        .strip_suffix("/chat/completions")
        .context("invalid Anthropic endpoint URL")?;
    Ok(format!("{prefix}/messages"))
}

/// Which exchange function carries a request; derived from the configured [`ApiProtocol`] by
/// [`Wire::of`] so a new protocol must be routed deliberately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Wire {
    ChatCompletions,
    Anthropic,
    /// ChatGPT subscription backend (Codex Responses over WebSocket or HTTP).
    Codex,
    /// Responses API over HTTP.
    Responses,
    Gemini,
    Ollama,
    /// Everything served through `llm::transports`.
    Transport,
}

impl Wire {
    pub(super) fn of(protocol: ApiProtocol) -> Self {
        match protocol {
            ApiProtocol::ChatCompletions => Self::ChatCompletions,
            ApiProtocol::Anthropic => Self::Anthropic,
            ApiProtocol::Responses => Self::Responses,
            ApiProtocol::Gemini => Self::Gemini,
            ApiProtocol::Ollama => Self::Ollama,
            ApiProtocol::Bedrock
            | ApiProtocol::Vertex
            | ApiProtocol::VertexAnthropic
            | ApiProtocol::AzureResponses
            | ApiProtocol::Copilot
            | ApiProtocol::CloudCodeAssist
            | ApiProtocol::Antigravity
            | ApiProtocol::GitlabDuo
            | ApiProtocol::FactoryDroid
            | ApiProtocol::DevinAgent
            | ApiProtocol::Cursor
            | ApiProtocol::GitlabDuoAgent => Self::Transport,
        }
    }
}

/// Validate a provider without contacting it (for config loading).
pub fn validate(kind: &ProviderKind, allow_insecure: bool) -> Result<()> {
    match kind {
        ProviderKind::ChatGpt { model, user_dir } => {
            ensure!(
                !model.trim().is_empty() && !user_dir.as_os_str().is_empty(),
                "ChatGPT requires a model and private user storage"
            );
            Ok(())
        }
        ProviderKind::AnthropicSubscription { model, user_dir } => {
            ensure!(
                !model.trim().is_empty() && !user_dir.as_os_str().is_empty(),
                "Anthropic subscription requires a model and private user storage"
            );
            Ok(())
        }
        ProviderKind::Local { model_dir } => {
            ensure!(
                !model_dir.is_empty(),
                "local model directory must not be empty"
            );
            Ok(())
        }
        ProviderKind::Endpoint {
            base_url,
            model,
            api_key,
        }
        | ProviderKind::AnthropicEndpoint {
            base_url,
            model,
            api_key,
        } => {
            ensure!(!model.trim().is_empty(), "endpoint model must not be empty");
            if let Some(key) = api_key {
                ensure!(
                    !key.is_empty() && key.bytes().all(|b| b.is_ascii_graphic()),
                    "api key is empty or contains characters that are not allowed in an HTTP header"
                );
            }
            endpoint_url(base_url, allow_insecure).map(drop)
        }
        ProviderKind::Apple | ProviderKind::Unconfigured { .. } => Ok(()),
    }
}

/// A grounded local or bundled remote context window.
pub fn context_window_hint(kind: &ProviderKind) -> Option<u32> {
    match kind {
        ProviderKind::ChatGpt { model, .. }
        | ProviderKind::AnthropicSubscription { model, .. }
        | ProviderKind::Endpoint { model, .. }
        | ProviderKind::AnthropicEndpoint { model, .. } => {
            llm::catalog::known_context_window(model)
        }
        ProviderKind::Local { model_dir } => local_context_window(Path::new(model_dir)),
        ProviderKind::Apple => Some(llm::apple::availability().context_size),
        ProviderKind::Unconfigured { .. } => None,
    }
}

fn local_context_window(model_dir: &Path) -> Option<u32> {
    let mut text = String::new();
    std::fs::File::open(model_dir.join("config.json"))
        .ok()?
        .take(MAX_CONFIG_BYTES)
        .read_to_string(&mut text)
        .ok()?;
    let config: serde_json::Value = serde_json::from_str(&text).ok()?;
    [
        config.pointer("/max_position_embeddings"),
        config.pointer("/text_config/max_position_embeddings"),
    ]
    .into_iter()
    .flatten()
    .find_map(|value| value.as_u64())
    .and_then(|window| u32::try_from(window).ok())
    .filter(|&window| window > 0)
}

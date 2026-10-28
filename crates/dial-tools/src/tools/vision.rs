//! Vision model roles used by tools: `grounding` (find a described element in a screenshot) and
//! `image_embedding` (multimodal vectors for the `image_search` tool).
//!
//! Both run on Dial's local MLX-VLM server (Apple Silicon) or on any OpenAI-compatible endpoint.
//! A role that is unassigned leaves its tool surface off.

pub mod embedding;
pub mod grounding;
pub mod search;

use crate::config::types::Config;
use crate::llm::model_config::ApiProtocol;
use anyhow::{Result, ensure};

/// Where a vision role's model runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Route {
    /// Dial launches the MLX checkpoint (local directory or Hugging Face repository id).
    Local,
    /// An OpenAI-compatible HTTP endpoint.
    Endpoint {
        base_url: String,
        api_key_env: Option<String>,
        allow_insecure: bool,
    },
}

/// The route of `role` from its resolved config. Other protocols (Anthropic, Responses, Gemini
/// native, subscriptions) are refused with the supported alternatives.
pub(crate) fn route(role: &str, resolved: &Config) -> Result<Route> {
    match &resolved.endpoint {
        None => {
            ensure!(
                resolved.provider.as_deref().is_none_or(|p| p == "mlx"),
                "the {role} role needs a local MLX model or an OpenAI-compatible endpoint; {} cannot serve it",
                resolved.provider.as_deref().unwrap_or("this provider")
            );
            Ok(Route::Local)
        }
        Some(endpoint) => {
            let protocol = endpoint
                .protocol
                .or_else(|| resolved.provider.as_deref().map(ApiProtocol::builtin));
            ensure!(
                protocol.is_none_or(|p| p == ApiProtocol::ChatCompletions),
                "the {role} role needs an OpenAI-compatible endpoint; {} uses the {} API",
                endpoint.base_url,
                protocol.map_or("unknown", |p| p.as_str())
            );
            Ok(Route::Endpoint {
                base_url: endpoint.base_url.clone(),
                api_key_env: endpoint.api_key_env.clone(),
                allow_insecure: endpoint.allow_insecure,
            })
        }
    }
}

/// Image media type from magic bytes, for data URLs.
pub(crate) fn image_mime(bytes: &[u8]) -> Result<&'static str> {
    Ok(match image::guess_format(bytes)? {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        other => anyhow::bail!("unsupported image format {other:?}"),
    })
}

#[cfg(test)]
mod tests;

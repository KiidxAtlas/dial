//! Dispatch for provider-specific transports (Bedrock, Vertex, Azure, Copilot, Cloud Code Assist,
//! GitLab Duo, Factory Droid) and a shared entry to Dial's standard wire protocols for transports
//! that proxy them with different URLs, headers or body shaping.
use super::model_config::ApiProtocol;
use super::profiles::ModelProfile;
use super::{Chat, Endpoint, Reply, StreamEvent};
use anyhow::{Result, bail};
use serde_json::Value;
use std::sync::{Arc, atomic::AtomicBool};

/// One resolved provider route for an extended transport.
pub(super) struct Route<'a> {
    pub protocol: ApiProtocol,
    /// Structurally validated base URL (selection base, or the OAuth grant's plan endpoint).
    pub base_url: &'a str,
    pub model: &'a str,
    /// API key / bearer credential from config, env, saved key or OAuth access token.
    pub auth: Option<&'a str>,
    /// Stable account identity for OAuth grants; never secret credential material.
    pub account_id: Option<&'a str>,
    /// Operator custom headers followed by OAuth-provided headers.
    pub headers: &'a [(&'a str, &'a str)],
    /// Cloud project from an OAuth grant (Cloud Code Assist).
    pub project_id: Option<&'a str>,
    /// Whether operator policy allows HTTP endpoints.
    pub allow_insecure: bool,
    /// Durable owner conversation id, when the caller has one.
    pub session_id: Option<&'a str>,
    /// Stable id for this logical request across transport retries.
    pub request_id: Option<&'a str>,
}

pub(super) type OnText<'a, 'b> = &'a mut Option<&'b mut dyn FnMut(&str) -> bool>;

/// Whether `protocol` is served by this module rather than the built-in dispatch.
#[cfg(test)]
pub(super) fn is_extended(protocol: ApiProtocol) -> bool {
    matches!(
        protocol,
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
    )
}

pub(super) fn exchange(
    route: &Route<'_>,
    req: &Chat,
    timeout_secs: u64,
    on_text: OnText<'_, '_>,
    interrupt: Arc<AtomicBool>,
    profile: Option<&ModelProfile>,
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
) -> Result<Reply> {
    match route.protocol {
        ApiProtocol::Bedrock => super::bedrock::exchange(
            route,
            req,
            timeout_secs,
            on_text,
            interrupt,
            profile,
            on_event,
        ),
        ApiProtocol::Vertex => super::vertex::exchange_gemini(
            route,
            req,
            timeout_secs,
            on_text,
            interrupt,
            profile,
            on_event,
        ),
        ApiProtocol::VertexAnthropic => super::vertex::exchange_anthropic(
            route,
            req,
            timeout_secs,
            on_text,
            interrupt,
            profile,
            on_event,
        ),
        ApiProtocol::AzureResponses => super::azure::exchange(
            route,
            req,
            timeout_secs,
            on_text,
            interrupt,
            profile,
            on_event,
        ),
        ApiProtocol::Copilot => super::copilot::exchange(
            route,
            req,
            timeout_secs,
            on_text,
            interrupt,
            profile,
            on_event,
        ),
        ApiProtocol::CloudCodeAssist | ApiProtocol::Antigravity => {
            super::cloud_code_assist::exchange(
                route,
                req,
                timeout_secs,
                on_text,
                interrupt,
                profile,
                on_event,
            )
        }
        ApiProtocol::GitlabDuo => super::gitlab_duo::exchange(
            route,
            req,
            timeout_secs,
            on_text,
            interrupt,
            profile,
            on_event,
        ),
        ApiProtocol::FactoryDroid => super::factory_droid::exchange(
            route,
            req,
            timeout_secs,
            on_text,
            interrupt,
            profile,
            on_event,
        ),
        ApiProtocol::DevinAgent => super::devin::exchange(
            route,
            req,
            timeout_secs,
            on_text,
            interrupt,
            profile,
            on_event,
        ),
        ApiProtocol::Cursor => super::cursor::exchange(
            route,
            req,
            timeout_secs,
            on_text,
            interrupt,
            profile,
            on_event,
        ),
        other => bail!("{} is served by the built-in transports", other.as_str()),
    }
}

/// Dial's standard wire protocols, for transports that proxy them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Wire {
    /// OpenAI chat completions; `url` is the full `…/chat/completions` URL.
    ChatCompletions,
    /// OpenAI Responses; `url` is the full `…/responses` URL.
    Responses,
    /// Anthropic Messages; `url` is the full `…/messages` (or equivalent) URL.
    Anthropic,
    /// Gemini generateContent SSE; `url` is the API base (the model path is appended).
    Gemini,
}

/// Body hook for Anthropic-shaped proxies (applied after Dial builds the Messages body).
pub(super) type AnthropicShape<'a> = &'a dyn Fn(&mut Value) -> Result<()>;

/// Run one request over a standard wire protocol. Credentials in `endpoint.auth` follow that
/// protocol's convention (Bearer for OpenAI shapes, `x-api-key` for Anthropic, `x-goog-api-key`
/// for Gemini); proxies that need a different scheme pass `auth: None` and an explicit header.
pub(super) fn exchange_wire(
    wire: Wire,
    endpoint: Endpoint<'_>,
    req: &Chat,
    timeout_secs: u64,
    on_text: OnText<'_, '_>,
    interrupt: Arc<AtomicBool>,
    profile: Option<&ModelProfile>,
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
    anthropic_shape: Option<AnthropicShape<'_>>,
) -> Result<Reply> {
    match wire {
        Wire::ChatCompletions => super::provider::exchange(
            endpoint,
            req,
            timeout_secs,
            on_text,
            super::Control {
                interrupt: Some(interrupt),
                profile,
                reasoning_guard: None,
                on_event,
            },
        ),
        Wire::Responses => super::responses::exchange_api(
            endpoint,
            req,
            timeout_secs,
            on_text,
            interrupt,
            profile,
            None,
            on_event,
        ),
        Wire::Anthropic => super::provider::anthropic_exchange_shaped(
            endpoint,
            req,
            timeout_secs,
            on_text,
            interrupt,
            profile,
            on_event,
            anthropic_shape,
        ),
        Wire::Gemini => super::gemini::exchange(
            endpoint,
            req,
            timeout_secs,
            on_text,
            interrupt,
            profile,
            on_event,
        ),
    }
}

/// Request URL for `protocol` at `base` (used when an OAuth grant overrides the base URL).
pub(super) fn route_url(protocol: ApiProtocol, base: &str, allow_insecure: bool) -> Result<String> {
    let completions = super::provider::endpoint_url(base, allow_insecure)?;
    Ok(match protocol {
        ApiProtocol::ChatCompletions => completions,
        ApiProtocol::Anthropic => super::provider::anthropic_url(base, allow_insecure)?,
        ApiProtocol::Responses => {
            completions.trim_end_matches("/chat/completions").to_owned() + "/responses"
        }
        _ => base.trim_end_matches('/').to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extended_protocols_are_exactly_the_new_transports() {
        for p in [
            ApiProtocol::ChatCompletions,
            ApiProtocol::Responses,
            ApiProtocol::Anthropic,
            ApiProtocol::Gemini,
            ApiProtocol::Ollama,
        ] {
            assert!(!is_extended(p), "{}", p.as_str());
        }
        for p in [
            ApiProtocol::Bedrock,
            ApiProtocol::Vertex,
            ApiProtocol::VertexAnthropic,
            ApiProtocol::AzureResponses,
            ApiProtocol::Copilot,
            ApiProtocol::CloudCodeAssist,
            ApiProtocol::Antigravity,
            ApiProtocol::GitlabDuo,
            ApiProtocol::FactoryDroid,
            ApiProtocol::DevinAgent,
            ApiProtocol::Cursor,
        ] {
            assert!(is_extended(p), "{}", p.as_str());
        }
    }

    #[test]
    fn route_urls_follow_protocol_conventions() {
        assert_eq!(
            route_url(
                ApiProtocol::ChatCompletions,
                "https://api.githubcopilot.com",
                false
            )
            .unwrap(),
            "https://api.githubcopilot.com/v1/chat/completions"
        );
        assert_eq!(
            route_url(ApiProtocol::Responses, "https://x.example/v1", false).unwrap(),
            "https://x.example/v1/responses"
        );
        assert_eq!(
            route_url(ApiProtocol::Anthropic, "https://x.example/v1", false).unwrap(),
            "https://x.example/v1/messages"
        );
        assert_eq!(
            route_url(
                ApiProtocol::Copilot,
                "https://api.githubcopilot.com/",
                false
            )
            .unwrap(),
            "https://api.githubcopilot.com"
        );
        assert!(route_url(ApiProtocol::Copilot, "http://remote.example", false).is_err());
    }
}

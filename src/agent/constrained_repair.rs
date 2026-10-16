//! One constrained retry after a malformed local tool call. The retry asks the endpoint for JSON
//! naming an offered tool (`llm::tool_repair`); the converted call still passes runtime validation
//! and approvals like any other call.

use super::{call_form_error, looks_like_text_call};
use crate::config::types::AssistanceConfig;
use crate::llm::Reply;
use crate::llm::profiles::{
    ModelCapabilities, ModelProfile, StructuredOutput, resolve_structured_output,
};
use crate::llm::tool_repair;
use serde_json::Value;

/// Whether a retry may be requested: the config is on, the session is not hermetic, and the
/// effective endpoint enforces structured output. `endpoint` is the mechanism detected on the live
/// endpoint (`Backend::endpoint_structured_output`); a declared profile still wins.
pub(super) fn enabled(
    config: &AssistanceConfig,
    hermetic: bool,
    provider: &str,
    model: &str,
    profile: Option<&ModelProfile>,
    endpoint: Option<StructuredOutput>,
) -> bool {
    let default = ModelCapabilities::default();
    let (provider, model, capabilities) = match profile {
        Some(profile) => (
            profile.provider.as_str(),
            profile.model.as_str(),
            &profile.capabilities,
        ),
        None => (provider, model, &default),
    };
    let endpoint = endpoint.or(profile.and_then(|profile| profile.endpoint_structured_output));
    let mechanism = resolve_structured_output(provider, model, capabilities, endpoint);
    config.constrained_repair && !hermetic && mechanism != StructuredOutput::Disabled
}

#[derive(Debug, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Idle,
    /// The next request is the constrained retry.
    Requested,
    /// The retry was sent. Further malformed replies take the ordinary nudge path until a clean reply.
    Spent,
}

/// Per-turn repair state; allows at most one retry per malformed sequence.
#[derive(Debug, Default)]
pub(super) struct Repair {
    phase: Phase,
}

impl Repair {
    /// The form kind that justifies a retry for this reply, when one is allowed.
    pub(super) fn trigger(
        &self,
        enabled: bool,
        reply: &Reply,
        schemas: &[Value],
        names: &[String],
    ) -> Option<&'static str> {
        if !enabled || self.phase != Phase::Idle {
            return None;
        }
        if reply.dropped_tool_call() {
            return Some("dropped_call");
        }
        if reply.tool_calls.is_empty() {
            return looks_like_text_call(&reply.text, names).then_some("text_call");
        }
        reply
            .tool_calls
            .iter()
            .find_map(|call| match call_form_error(call, schemas, names) {
                Some((kind @ ("malformed_arguments" | "schema"), _)) => Some(kind),
                _ => None,
            })
    }

    pub(super) fn request(&mut self) {
        self.phase = Phase::Requested;
    }

    /// True when the request about to be sent is the constrained retry. Consumes the request.
    pub(super) fn take_request(&mut self) -> bool {
        let requested = self.phase == Phase::Requested;
        if requested {
            self.phase = Phase::Spent;
        }
        requested
    }

    /// A clean reply ends the malformed sequence.
    pub(super) fn settle(&mut self) {
        if self.phase == Phase::Spent {
            self.phase = Phase::Idle;
        }
    }
}

/// Converts the constrained retry's reply into a tool call. Native calls the endpoint still
/// produced are kept. On error the reason is returned; the model text is not echoed.
pub(super) fn finish(
    reply: &mut Reply,
    offered: &[String],
    id: String,
) -> Result<&'static str, String> {
    if !reply.tool_calls.is_empty() {
        return Ok("native");
    }
    let call = tool_repair::call_from_reply(&reply.text, offered, id)?;
    reply.tool_calls = vec![call];
    reply.text.clear();
    reply.finish_reason = "tool_calls".into();
    Ok("converted")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::Config;

    fn dropped() -> Reply {
        Reply {
            finish_reason: "tool_calls".into(),
            ..Default::default()
        }
    }

    #[test]
    fn enabled_follows_provider_default_and_explicit_override() {
        let config = Config::default().assistance;
        let lm = ModelProfile::minimal("lm-studio", "m");
        let mlx = ModelProfile::minimal("mlx", "m");
        assert!(enabled(&config, false, "lm-studio", "m", Some(&lm), None));
        assert!(enabled(&config, false, "ollama", "m", None, None));
        assert!(!enabled(&config, false, "mlx", "m", Some(&mlx), None));
        assert!(!enabled(&config, true, "lm-studio", "m", Some(&lm), None));
        let mut off = ModelProfile::minimal("lm-studio", "m");
        off.capabilities.structured_output = Some(StructuredOutput::Disabled);
        assert!(!enabled(&config, false, "lm-studio", "m", Some(&off), None));
        let mut disabled = config.clone();
        disabled.constrained_repair = false;
        assert!(!enabled(
            &disabled,
            false,
            "lm-studio",
            "m",
            Some(&lm),
            None
        ));
    }

    #[test]
    fn detected_llama_server_enables_repair_with_or_without_a_profile() {
        let config = Config::default().assistance;
        let json = Some(StructuredOutput::JsonSchema);
        // A generic OpenAI-compatible endpoint declares no mechanism; the detected one decides.
        assert!(!enabled(
            &config,
            false,
            "openai-compatible",
            "m",
            None,
            None
        ));
        assert!(enabled(
            &config,
            false,
            "openai-compatible",
            "m",
            None,
            json
        ));
        let plain = ModelProfile::minimal("openai-compatible", "m");
        assert!(enabled(
            &config,
            false,
            "openai-compatible",
            "m",
            Some(&plain),
            json
        ));
        // A detection already recorded on the profile (set at the request boundary) counts the same.
        let mut recorded = plain.clone();
        recorded.endpoint_structured_output = json;
        assert!(enabled(
            &config,
            false,
            "openai-compatible",
            "m",
            Some(&recorded),
            None
        ));
        // An explicit declaration still wins over detection.
        let mut declared_off = recorded.clone();
        declared_off.capabilities.structured_output = Some(StructuredOutput::Disabled);
        assert!(!enabled(
            &config,
            false,
            "openai-compatible",
            "m",
            Some(&declared_off),
            json
        ));
    }

    #[test]
    fn at_most_one_retry_per_malformed_sequence() {
        let mut repair = Repair::default();
        assert_eq!(
            repair.trigger(true, &dropped(), &[], &[]),
            Some("dropped_call")
        );
        assert_eq!(repair.trigger(false, &dropped(), &[], &[]), None);
        repair.request();
        assert!(repair.take_request());
        assert!(!repair.take_request());
        assert_eq!(repair.trigger(true, &dropped(), &[], &[]), None);
        repair.settle();
        assert_eq!(
            repair.trigger(true, &dropped(), &[], &[]),
            Some("dropped_call")
        );
    }

    #[test]
    fn constrained_reply_converts_and_plain_text_is_rejected() {
        let offered = vec!["read".to_owned()];
        let mut ok = Reply {
            text: r#"{"name":"read","arguments":{"path":"a"}}"#.into(),
            finish_reason: "stop".into(),
            ..Default::default()
        };
        assert_eq!(finish(&mut ok, &offered, "c1".into()), Ok("converted"));
        assert_eq!(ok.tool_calls.len(), 1);
        assert_eq!(ok.tool_calls[0].name, "read");
        assert!(ok.text.is_empty());

        let mut prose = Reply {
            text: "I will read the file now.".into(),
            finish_reason: "stop".into(),
            ..Default::default()
        };
        assert!(finish(&mut prose, &offered, "c2".into()).is_err());
        assert!(prose.tool_calls.is_empty());
    }
}

use super::*;
use crate::llm::profiles::{ModelProfile, ReasoningEffort};
use serde_json::json;

fn http(status: u16, message: &str) -> anyhow::Error {
    let body = json!({"error": {"message": message}}).to_string();
    let body = crate::llm::Bounded {
        data: body.into_bytes(),
        truncated: false,
    };
    crate::llm::http_error(status, &body, None, None::<std::time::Duration>)
}

#[test]
fn only_a_400_naming_the_reasoning_parameter_is_a_rejection() {
    for message in [
        "Unsupported parameter: 'reasoning_effort' is not supported with this model.",
        "Unrecognized request argument supplied: reasoning.effort",
        "Unsupported parameter(s) for this model: reasoning",
    ] {
        assert!(rejects_reasoning(&http(400, message)), "{message}");
    }
    assert!(!rejects_reasoning(&http(400, "max_tokens is too large")));
    assert!(!rejects_reasoning(&http(
        400,
        "Unsupported parameter: 'seed'"
    )));
    assert!(!rejects_reasoning(&http(500, "reasoning_effort exploded")));
    assert!(!rejects_reasoning(&anyhow::anyhow!("reasoning_effort")));
}

#[test]
fn memory_is_per_endpoint_and_model() {
    let memory = Memory::default();
    assert!(!memory.remembered("https://a/v1", "m"));
    memory.remember("https://a/v1", "m");
    assert!(memory.remembered("https://a/v1", "m"));
    assert!(!memory.remembered("https://a/v1", "other"));
    assert!(!memory.remembered("https://b/v1", "m"));
}

#[test]
fn stripping_removes_effort_and_explicit_thinking_off() {
    let profile = ModelProfile {
        reasoning_effort: Some(ReasoningEffort::High),
        thinking: Some(false),
        ..Default::default()
    };
    let stripped = without_reasoning(profile);
    assert_eq!(stripped.reasoning_effort, None);
    assert_eq!(stripped.thinking, None);
    let on = without_reasoning(ModelProfile {
        thinking: Some(true),
        ..Default::default()
    });
    assert_eq!(on.thinking, Some(true));
}

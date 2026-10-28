use super::*;

#[test]
fn accumulator_streams_wrapped_cca_parts_and_retains_signature_and_execution_id() {
    let patience = super::super::Patience::for_stream(30, false);
    let mut accumulator = Accumulator::default();
    let mut visible = String::new();
    let mut on_text: Option<&mut dyn FnMut(&str) -> bool> = Some(&mut |delta| {
        visible.push_str(delta);
        true
    });
    let mut on_event = |_event: &StreamEvent| true;
    accumulator.feed(
        r#"{"responseId":"execution","response":{"candidates":[{"content":{"role":"model","parts":[{"text":"answer","thoughtSignature":"signed-native-value"}]},"finishReason":"STOP"}]}}"#,
        &patience,
        &mut on_text,
        &mut on_event,
    ).unwrap();
    assert_eq!(visible, "answer");
    assert_eq!(accumulator.execution_id(), Some("execution"));
    let endpoint = Endpoint {
        url: "https://example.test",
        auth: None,
        model: "gemini-3.1-pro",
        headers: &[],
    };
    let reply = accumulator.finish(&endpoint, "scope".into()).unwrap();
    assert_eq!(
        reply.provider_content[0]["parts"][0]["thoughtSignature"],
        "signed-native-value"
    );
}

#[test]
fn reasoning_effort_wire_reaches_gemini_thinking_level_and_none_keeps_zero_budget() {
    use crate::llm::profiles::{ModelCapabilities, ModelProfile, ReasoningApi, ReasoningEffort};
    use std::collections::BTreeMap;
    let chat = Chat {
        messages: vec![serde_json::json!({"role":"user","content":"hi"})],
        tools: vec![],
        temperature: 0.2,
        max_tokens: 64,
        seed: None,
    };
    let endpoint = Endpoint {
        url: "https://example.test",
        auth: None,
        model: "gemini-3.1-pro",
        headers: &[],
    };
    let mut profile = ModelProfile {
        provider: "gemini".into(),
        model: "gemini-3.1-pro".into(),
        reasoning_effort: Some(ReasoningEffort::High),
        capabilities: ModelCapabilities {
            reasoning: ReasoningApi::GeminiLevel,
            reasoning_efforts: vec![ReasoningEffort::None, ReasoningEffort::High],
            reasoning_effort_wire: BTreeMap::from([(ReasoningEffort::High, "medium".to_string())]),
            ..Default::default()
        },
        ..Default::default()
    };
    let level = |body: &Value| body["generationConfig"]["thinkingConfig"]["thinkingLevel"].clone();
    let body = request(&endpoint, &chat, Some(&profile), "main").unwrap();
    assert_eq!(level(&body), "MEDIUM");
    profile.capabilities.reasoning_effort_wire.clear();
    let body = request(&endpoint, &chat, Some(&profile), "main").unwrap();
    assert_eq!(level(&body), "HIGH");
    profile.reasoning_effort = Some(ReasoningEffort::None);
    let body = request(&endpoint, &chat, Some(&profile), "main").unwrap();
    let thinking = &body["generationConfig"]["thinkingConfig"];
    assert_eq!(thinking["thinkingBudget"], 0);
    assert!(thinking.get("thinkingLevel").is_none());
}

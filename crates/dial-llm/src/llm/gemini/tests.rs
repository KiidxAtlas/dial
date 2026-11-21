use super::*;

#[test]
fn hosted_defaults_leave_neon_output_to_the_provider() {
    let endpoint = Endpoint {
        url: "https://example.test",
        auth: None,
        model: "gemini-4-neon-high",
        headers: &[],
    };
    let chat = Chat {
        messages: vec![json!({"role":"user","content":"hello"})],
        tools: vec![],
        temperature: 0.2,
        max_tokens: 8192,
        seed: None,
    };
    let mut profile = ModelProfile::minimal("google-antigravity", "gemini-4-neon");
    profile.capabilities.temperature = false;
    profile.inherit_output_limit = true;
    let body = request_for_proxy(&endpoint, &chat, Some(&profile), "scope").unwrap();
    let controls = &body["generationConfig"];
    assert!(controls.get("maxOutputTokens").is_none(), "{controls}");
    assert!(controls.get("temperature").is_none(), "{controls}");
    profile.inherit_output_limit = false;
    let bounded = request_for_proxy(&endpoint, &chat, Some(&profile), "scope").unwrap();
    assert_eq!(bounded["generationConfig"]["maxOutputTokens"], 8192);
}

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
fn stream_regression_truncated_call_keeps_partial_text_without_replay() {
    let patience = Patience::for_stream(30, false);
    let parts = json!([{"text":"partial answer"}, {"functionCall":{}}]);
    for reason in ["MAX_TOKENS", "STOP"] {
        let mut stream = Accumulator::default();
        stream.feed(r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"read","args":{}}}]}}]}"#, &patience, &mut None, &mut |_| true).unwrap();
        let candidate = json!({"content":{"parts":parts}, "finishReason":reason});
        let result = stream.feed(
            &json!({"candidates":[candidate]}).to_string(),
            &patience,
            &mut None,
            &mut |_| true,
        );
        if reason == "STOP" {
            assert!(result.unwrap_err().to_string().contains("no name"));
            continue;
        }
        result.unwrap();
        let endpoint = Endpoint {
            url: "https://example.test",
            model: "gemini",
            auth: None,
            headers: &[],
        };
        let reply = stream.finish(&endpoint, "scope".into()).unwrap();
        assert_eq!(
            (reply.finish_reason.as_str(), reply.text.as_str()),
            ("length", "partial answer")
        );
        assert!(reply.tool_calls.is_empty());
        let carrier = &reply.provider_content[0];
        assert_eq!(carrier["calls"], json!([]));
        assert_eq!(carrier["parts"], json!([{"text":"partial answer"}]));
    }
}

#[test]
fn stream_regression_wrapped_provider_error_is_not_silently_ignored() {
    let patience = Patience::for_stream(30, false);
    let error =
        json!({"error":{"code":403,"status":"PERMISSION_DENIED","message":"model access denied"}});
    for event in [error.clone(), json!({"response":error})] {
        let failure = Accumulator::default()
            .feed(&event.to_string(), &patience, &mut None, &mut |_| true)
            .unwrap_err();
        assert!(
            failure.to_string().contains("PERMISSION_DENIED"),
            "{failure}"
        );
        assert!(
            failure.to_string().contains("model access denied"),
            "{failure}"
        );
    }
}

#[test]
fn antigravity_pro_catalog_effort_reaches_gemini_request() {
    let chat = Chat {
        messages: vec![json!({"role":"user","content":"hey"})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 1024,
        seed: None,
    };
    for (model, effort, thinking, expected) in [
        (
            "gemini-3.1-pro",
            Some(ReasoningEffort::Low),
            true,
            json!({"includeThoughts": true, "thinkingLevel": "LOW"}),
        ),
        (
            "gemini-3.1-pro",
            Some(ReasoningEffort::High),
            true,
            json!({"includeThoughts": true, "thinkingLevel": "HIGH"}),
        ),
        (
            "gemini-2.5-pro",
            None,
            false,
            json!({"includeThoughts": false, "thinkingBudget": 0}),
        ),
    ] {
        let config = crate::config::Config {
            provider: Some("google-antigravity".into()),
            model: model.into(),
            ..Default::default()
        };
        let mut profile = crate::config::thinking::reasoning_profile(&config)
            .unwrap()
            .unwrap();
        profile.reasoning_effort = effort;
        profile.thinking = Some(thinking);
        let endpoint = Endpoint {
            url: "https://example.test",
            model,
            auth: None,
            headers: &[],
        };
        let body = request(&endpoint, &chat, Some(&profile), "scope").unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"], expected,
            "{model}"
        );
    }
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

#[test]
fn foreign_tool_history_replays_as_text_instead_of_refusing_the_chat() {
    let chat = Chat {
        messages: vec![
            json!({"role":"user","content":"look at a.rs"}),
            // A call made by another provider: no Gemini native envelope or signature.
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function",
                "function":{"name":"read","arguments":"{\"path\":\"a.rs\"}"}}]}),
            json!({"role":"tool","tool_call_id":"call_1","content":"fn main() {}"}),
            json!({"role":"user","content":"now summarize"}),
        ],
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
    let body = request(&endpoint, &chat, None, "scope").unwrap();
    let text = body["contents"].to_string();
    // No function-call part is invented for a call this endpoint never signed.
    assert!(!text.contains("functionCall"), "{text}");
    assert!(!text.contains("functionResponse"), "{text}");
    assert!(text.contains("read({\\\"path\\\":\\\"a.rs\\\"})"), "{text}");
    assert!(text.contains("fn main() {}"), "{text}");
    assert_eq!(body["contents"][1]["role"], "model");
}

#[test]
fn antigravity_model_variant_sends_no_thinking_config_and_refuses_stale_effort() {
    use crate::llm::profiles::{ModelCapabilities, ModelProfile, ReasoningApi, ReasoningEffort};
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
        model: "gemini-3.5-flash-high",
        headers: &[],
    };
    let profile = |thinking, effort| ModelProfile {
        provider: "google-antigravity".into(),
        model: "gemini-3.5-flash".into(),
        thinking,
        reasoning_effort: Some(effort),
        capabilities: ModelCapabilities {
            reasoning: ReasoningApi::AntigravityModelVariant,
            reasoning_efforts: vec![ReasoningEffort::High],
            ..Default::default()
        },
        ..Default::default()
    };
    let body = request(
        &endpoint,
        &chat,
        Some(&profile(None, ReasoningEffort::High)),
        "main",
    )
    .unwrap();
    assert!(body["generationConfig"].get("thinkingConfig").is_none());
    // A stale or unlisted effort, or a thinking switch, is refused before any wire model is chosen.
    assert!(
        request(
            &endpoint,
            &chat,
            Some(&profile(None, ReasoningEffort::Low)),
            "main"
        )
        .is_err()
    );
    assert!(
        request(
            &endpoint,
            &chat,
            Some(&profile(Some(true), ReasoningEffort::High)),
            "main"
        )
        .is_err()
    );
}

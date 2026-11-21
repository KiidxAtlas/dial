use super::*;

#[test]
fn antigravity_envelope_replays_scoped_state_and_honors_loopback() {
    let route = Route {
        protocol: super::super::model_config::ApiProtocol::Antigravity,
        base_url: "http://127.0.0.1:9000",
        model: "gemini-3.1-pro-low",
        auth: None,
        account_id: Some("account"),
        headers: &[],
        project_id: Some("project"),
        allow_insecure: true,
        session_id: Some("session"),
        request_id: None,
    };
    let owned = request_headers(&route).unwrap();
    let headers = owned
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect::<Vec<_>>();
    let scope = super::super::model_config::native_replay_scope(
        "antigravity",
        route.base_url,
        Some(route.model),
        route.account_id,
        route.auth,
        &headers,
    );
    let chat = Chat {
        messages: vec![
            json!({"role":"user","content":"hello"}),
            json!({"role":"assistant","content":"prior", "dial_provider_content":[{
                "type":"dial_gemini_content","scope":scope,"execution_id":"exec","parts":[],"calls":[]}]}),
        ],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 100,
        seed: None,
    };
    let body = envelope(&route, route.model, json!({"contents": []}), &chat, &scope).unwrap();
    assert_eq!(body["userAgent"], "antigravity");
    assert_eq!(body["requestType"], "agent");
    assert!(
        body["request"]["sessionId"]
            .as_str()
            .unwrap()
            .starts_with('-')
    );
    assert!(body["requestId"].as_str().unwrap().starts_with("agent/"));
    assert_eq!(body["request"]["labels"]["last_execution_id"], "exec");
    assert_eq!(
        body["request"]["labels"]["trajectory_id"]
            .as_str()
            .unwrap()
            .len(),
        36
    );
    assert_eq!(body["request"]["labels"]["last_step_index"], "2");
    assert_eq!(
        body["request"]["labels"]["model_enum"],
        "MODEL_PLACEHOLDER_M36"
    );
    assert!(
        body["request"]["generationConfig"]
            .get("maxOutputTokens")
            .is_none()
    );
    for (requested, expected) in [(100, 100), (100_000, 65_535)] {
        let body = envelope(
            &route,
            route.model,
            json!({"generationConfig":{"maxOutputTokens":requested}}),
            &chat,
            &scope,
        )
        .unwrap();
        assert_eq!(
            body["request"]["generationConfig"]["maxOutputTokens"],
            expected
        );
    }
    assert_eq!(
        endpoint(&route).unwrap(),
        "http://127.0.0.1:9000/v1internal:streamGenerateContent?alt=sse"
    );
}

#[test]
fn antigravity_model_variant_selects_wire_model_only_with_a_declared_effort() {
    use crate::llm::profiles::{ModelCapabilities, ReasoningApi, ReasoningEffort};
    let profile = |reasoning, model: &str, effort| ModelProfile {
        provider: "google-antigravity".into(),
        model: model.into(),
        reasoning_effort: effort,
        capabilities: ModelCapabilities {
            reasoning,
            reasoning_efforts: vec![ReasoningEffort::High],
            ..Default::default()
        },
        ..Default::default()
    };
    let antigravity = super::super::model_config::ApiProtocol::Antigravity;
    let cloud = super::super::model_config::ApiProtocol::CloudCodeAssist;
    let variant = ReasoningApi::AntigravityModelVariant;
    let cases = [
        // A declared effort selects the listed variant at the transport boundary.
        (
            antigravity,
            "gemini-3.5-flash",
            profile(variant, "gemini-3.5-flash", Some(ReasoningEffort::High)),
            crate::llm::catalog::antigravity_effort_model(
                "gemini-3.5-flash",
                ReasoningEffort::High,
            )
            .unwrap(),
        ),
        // No active effort keeps the selected representative, including an explicit suffixed ID.
        (
            antigravity,
            "gemini-3.1-pro-low",
            profile(variant, "gemini-3.1-pro-low", None),
            "gemini-3.1-pro-low".into(),
        ),
        // Other protocols and legacy effort profiles never rewrite the model.
        (
            cloud,
            "gemini-3.5-flash",
            profile(variant, "gemini-3.5-flash", Some(ReasoningEffort::High)),
            "gemini-3.5-flash".into(),
        ),
        (
            antigravity,
            "gemini-3.5-flash",
            profile(
                ReasoningApi::GeminiLevel,
                "gemini-3.5-flash",
                Some(ReasoningEffort::High),
            ),
            "gemini-3.5-flash".into(),
        ),
    ];
    for (protocol, model, profile, expected) in cases {
        let route = Route {
            protocol,
            base_url: "http://127.0.0.1:9000",
            model,
            auth: None,
            account_id: Some("account"),
            headers: &[],
            project_id: Some("project"),
            allow_insecure: true,
            session_id: None,
            request_id: None,
        };
        let wire = wire_model(&route, Some(&profile)).unwrap();
        assert_eq!(wire, expected, "{model}");
        let chat = Chat {
            messages: vec![json!({"role":"user","content":"hello"})],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 100,
            seed: None,
        };
        let body = envelope(&route, &wire, json!({"contents": []}), &chat, "scope").unwrap();
        assert_eq!(body["model"], wire);
    }
    // An effort the profile does not list is refused, never replaced by a bare or representative ID.
    let route = Route {
        protocol: antigravity,
        base_url: "http://127.0.0.1:9000",
        model: "gemini-3.5-flash",
        auth: None,
        account_id: Some("account"),
        headers: &[],
        project_id: Some("project"),
        allow_insecure: true,
        session_id: None,
        request_id: None,
    };
    let mut unlisted = profile(variant, "gemini-3.5-flash", Some(ReasoningEffort::High));
    unlisted.capabilities.reasoning_efforts.clear();
    assert!(wire_model(&route, Some(&unlisted)).is_err());
}

#[test]
fn antigravity_model_variant_exchange_sends_and_records_the_wire_model() {
    use crate::llm::profiles::{ModelCapabilities, ReasoningApi, ReasoningEffort};
    use std::sync::{Arc, atomic::AtomicBool};
    // Fixture: one terminal Gemini stream event carrying text.
    const SSE: &str = "data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}]}}\n\n";
    let server = crate::llm::fake::serve(vec![crate::llm::fake::Script::Sse(SSE.into())]);
    let base = server.url.clone();
    let route = Route {
        protocol: super::super::model_config::ApiProtocol::Antigravity,
        base_url: &base,
        model: "gemini-3.5-flash",
        auth: None,
        account_id: Some("account"),
        headers: &[],
        project_id: Some("project"),
        allow_insecure: true,
        session_id: None,
        request_id: None,
    };
    let profile = ModelProfile {
        provider: "google-antigravity".into(),
        model: "gemini-3.5-flash".into(),
        reasoning_effort: Some(ReasoningEffort::High),
        capabilities: ModelCapabilities {
            reasoning: ReasoningApi::AntigravityModelVariant,
            reasoning_efforts: vec![ReasoningEffort::High],
            ..Default::default()
        },
        ..Default::default()
    };
    let chat = Chat {
        messages: vec![json!({"role":"user","content":"hi"})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    };
    let reply = exchange(
        &route,
        &chat,
        5,
        &mut None,
        Arc::new(AtomicBool::new(false)),
        Some(&profile),
        &mut |_| true,
    )
    .unwrap();
    let raw = server.requests().remove(0);
    let path = crate::llm::fake::request_line(&raw)
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let body = crate::llm::fake::body(&raw);
    let wire =
        crate::llm::catalog::antigravity_effort_model("gemini-3.5-flash", ReasoningEffort::High)
            .expect("listed effort has a wire ID");
    assert_eq!(path, "/v1internal:streamGenerateContent?alt=sse");
    assert_eq!(body["model"], wire);
    assert!(
        body["request"]["generationConfig"]
            .get("thinkingConfig")
            .is_none()
    );
    assert_eq!(reply.text, "ok");
    // The replay carrier names the wire model, so the next turn's scope and model check agree.
    assert!(
        reply
            .provider_content
            .iter()
            .any(|item| item["model"] == wire)
    );
}

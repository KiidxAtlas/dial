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
    let body = envelope(&route, json!({"contents": []}), &chat, &scope).unwrap();
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
    assert_eq!(
        body["request"]["generationConfig"]["maxOutputTokens"],
        65535
    );
    assert_eq!(
        endpoint(&route).unwrap(),
        "http://127.0.0.1:9000/v1internal:streamGenerateContent?alt=sse"
    );
}

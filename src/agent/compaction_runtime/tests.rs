use super::*;
use crate::{
    config::types::{Config, HookConfig},
    llm::{fake, provider::ProviderKind},
};
use serde_json::json;

#[test]
fn compaction_summary_request_gets_before_provider_request() {
    let event = json!({"choices":[{"delta":{"content":"summary"},"finish_reason":"stop"}]});
    let server = fake::serve(vec![fake::Script::Sse(format!(
        "data: {event}\n\ndata: [DONE]\n\n"
    ))]);
    let mut backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: format!("{}/v1", server.url),
            model: "test".into(),
            api_key: None,
        },
        std::path::PathBuf::new(),
        5,
    )
    .with_session_id("compaction-test")
    .with_profile(llm::profiles::ModelProfile {
        provider: "openai".into(),
        model: "test".into(),
        max_tokens: Some(1024),
        ..Default::default()
    });
    let rewrite = HookConfig {
        event: "before_provider_request".into(),
        matcher: None,
        command: vec![
            "sh".into(),
            "-c".into(),
            r#"printf '{"payload":{"messages":[{"role":"user","content":"rewritten"}],"tools":[],"temperature":0.0,"max_tokens":64,"seed":null}}'"#.into(),
        ],
        timeout_secs: 30,
        fail_closed: None,
    };
    let dir = tempfile::tempdir().unwrap();
    let mut complete = portable_summary_completion(
        Config::default(),
        None,
        dir.path().to_path_buf(),
        json!({}),
        vec![rewrite],
        None,
        String::new(),
        false,
    );
    let source = Chat {
        messages: vec![json!({"role":"user","content":"original"})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    };
    let summary = complete(
        &mut backend,
        &source,
        &mut super::super::NullUi,
        Arc::new(AtomicBool::new(false)),
        5,
    )
    .unwrap();
    assert_eq!(summary, "summary");
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(fake::body(&requests[0])["max_tokens"], 64);
    assert_eq!(backend.model_profile().unwrap().max_tokens, Some(1024));
    let sent = fake::body(&requests[0])["messages"].to_string();
    assert!(
        sent.contains("rewritten") && !sent.contains("original"),
        "{sent}"
    );
}

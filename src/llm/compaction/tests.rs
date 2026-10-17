use super::*;
use llm::profiles::ModelProfile;

#[test]
fn compaction_summary_chatgpt_completes_without_injected_overrides() {
    let dir = tempfile::tempdir().unwrap();
    llm::chatgpt::test_account(dir.path(), false);
    let output = json!({"type":"message","role":"assistant","status":"completed",
        "content":[{"type":"output_text","text":"summary","annotations":[]}]});
    let server = llm::fake::serve(vec![llm::fake::Script::Sse(format!(
        "data: {}\n\ndata: {}\n\n",
        json!({"type":"response.output_text.delta","delta":"summary"}),
        json!({"type":"response.completed","response":{"status":"completed","output":[output],"usage":{"input_tokens":10,"output_tokens":2}}})
    ))]);
    let mut backend = Backend::new(
        ProviderKind::ChatGpt {
            model: "account-model".into(),
            user_dir: dir.path().to_path_buf(),
        },
        Default::default(),
        5,
    )
    .with_session_id("compaction-summary-chatgpt")
    .with_profile(ModelProfile {
        provider: "chatgpt".into(),
        model: "account-model".into(),
        ..Default::default()
    });
    backend.chatgpt_url = Some(format!("{}/codex/responses", server.url));
    let mut complete = crate::agent::compaction_runtime::portable_summary_completion(
        crate::config::types::Config::default(),
        None,
        dir.path().to_path_buf(),
        json!({}),
        vec![],
        None,
        String::new(),
        false,
    );
    let request = Chat {
        messages: vec![json!({"role":"user","content":"Summarize this conversation."})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    };
    let summary = complete(
        &mut backend,
        &request,
        &mut crate::agent::NullUi,
        Arc::new(AtomicBool::new(false)),
        5,
    )
    .unwrap();
    assert_eq!(summary, "summary");
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let body = llm::fake::body(&requests[0]);
    for field in ["max_tokens", "max_output_tokens", "temperature", "top_p"] {
        assert!(body.get(field).is_none(), "unexpected {field}: {body}");
    }
    assert!(backend.model_profile().unwrap().max_tokens.is_none());
}

#[test]
fn compaction_summary_preserves_explicit_chatgpt_override_errors() {
    let mut backend = Backend::new(
        ProviderKind::ChatGpt {
            model: "account-model".into(),
            user_dir: Default::default(),
        },
        Default::default(),
        5,
    );
    assert!(backend.compaction_summary_profile(64).is_none());
    let request = Chat {
        messages: vec![json!({"role":"user","content":"Summarize."})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    };
    for settings in ["max_tokens=128", "temperature=0.5", "top_p=0.9"] {
        let profile: ModelProfile = toml::from_str(&format!(
            "provider='chatgpt'\nmodel='account-model'\n{settings}\n[capabilities]\ntemperature=true\ntop_p=true"
        ))
        .unwrap();
        backend.next_request_profile(Some(profile));
        let profile = backend.compaction_summary_profile(request.max_tokens);
        let error =
            llm::responses::request_codex("account-model", &request, profile.as_ref()).unwrap_err();
        assert!(
            error.to_string().contains("ChatGPT plan usage"),
            "{error:#}"
        );
    }
}

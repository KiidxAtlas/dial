use super::*;

#[test]
fn compaction_summary_role_chatgpt_does_not_inject_output_limits() {
    for assigned in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        llm::chatgpt::test_account(home.path(), false);
        let output = json!({"type":"message","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":"summary","annotations":[]}]});
        let server = llm::fake::serve(vec![llm::fake::Script::Sse(format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"type":"response.output_text.delta","delta":"summary"}),
            json!({"type":"response.completed","response":{"status":"completed","output":[output],"usage":{"input_tokens":10,"output_tokens":2}}})
        ))]);
        let profile = llm::profiles::ModelProfile {
            provider: "chatgpt".into(),
            model: "account-model".into(),
            ..Default::default()
        };
        let mut config = Config {
            provider: Some("chatgpt".into()),
            model: "account-model".into(),
            user_dir: home.path().to_path_buf(),
            model_profile: Some("plan".into()),
            ..Default::default()
        };
        config.model_profiles.insert("plan".into(), profile.clone());
        if assigned {
            config
                .model_roles
                .insert("small".into(), config.selection());
        }
        let backend = || {
            let mut backend = Backend::new(
                ProviderKind::ChatGpt {
                    model: "account-model".into(),
                    user_dir: home.path().to_path_buf(),
                },
                Default::default(),
                5,
            )
            .with_session_id("role-compaction")
            .with_profile(profile.clone());
            backend.chatgpt_url = Some(format!("{}/codex/responses", server.url));
            backend
        };
        let version = Version {
            id: 0,
            parent: None,
            kind: "config".into(),
            harness: config.harness.clone(),
            adapter: None,
            status: "active".into(),
        };
        let mut agent =
            Agent::hermetic(&config, backend(), &version, workspace.path(), None, None).unwrap();
        if assigned {
            agent.role_backends.insert("small".into(), backend());
        }
        let mut main = agent.backend.take().unwrap();
        let request = Chat {
            messages: vec![json!({"role":"user","content":"Summarize."})],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 64,
            seed: None,
        };
        assert_eq!(
            agent
                .role_chat(
                    &mut main,
                    "small",
                    llm::usage::Purpose::Compaction,
                    &request,
                    &mut NullUi,
                )
                .unwrap(),
            "summary"
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        let body = llm::fake::body(&requests[0]);
        for field in ["max_tokens", "max_output_tokens", "temperature", "top_p"] {
            assert!(
                body.get(field).is_none(),
                "assigned={assigned}, {field}: {body}"
            );
        }
        assert!(main.model_profile().unwrap().max_tokens.is_none());
        assert!(config.active_model_profile().unwrap().max_tokens.is_none());
    }
}

#[test]
fn auxiliary_output_budget_preserves_endpoint_caps_and_selected_profiles() {
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "test".into(),
            api_key: None,
        },
        Default::default(),
        5,
    )
    .with_profile(llm::profiles::ModelProfile {
        provider: "openai".into(),
        model: "test".into(),
        max_tokens: Some(1024),
        ..Default::default()
    });
    let selected = llm::profiles::ModelProfile {
        max_tokens: Some(32),
        ..backend.model_profile().unwrap().clone()
    };
    assert_eq!(
        backend
            .bounded_request_profile(None, 64)
            .unwrap()
            .max_tokens,
        Some(64)
    );
    assert_eq!(
        backend
            .bounded_request_profile(Some(&selected), 64)
            .unwrap()
            .max_tokens,
        Some(32)
    );
    assert_eq!(
        backend.bounded_request_profile(None, 0).unwrap().max_tokens,
        Some(1024)
    );
    assert_eq!(backend.model_profile().unwrap().max_tokens, Some(1024));
}

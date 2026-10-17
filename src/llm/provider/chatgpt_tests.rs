use super::*;
use serde_json::json;
#[test]
fn remote_silence_limit_preserves_local_worker_allowances() {
    let make = |kind| Backend::new(kind, PathBuf::new(), 600).with_remote_idle_timeout(90);
    for kind in [
        ProviderKind::ChatGpt {
            model: "test".into(),
            user_dir: PathBuf::new(),
        },
        ProviderKind::Endpoint {
            base_url: "https://example.com/v1".into(),
            model: "test".into(),
            api_key: None,
        },
        ProviderKind::AnthropicEndpoint {
            base_url: "https://example.com/v1".into(),
            model: "test".into(),
            api_key: None,
        },
    ] {
        let backend = make(kind);
        assert_eq!(backend.response_timeout(3600), 90);
        assert_eq!(backend.response_timeout(30), 30);
    }
    for kind in [
        ProviderKind::Local {
            model_dir: "local-model".into(),
        },
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:1234/v1".into(),
            model: "test".into(),
            api_key: None,
        },
    ] {
        assert_eq!(make(kind).response_timeout(3600), 3600);
    }
}

#[test]
fn remote_default_bounds_unlimited_workers_while_local_servers_stay_unlimited() {
    let limit = crate::config::types::DEFAULT_REMOTE_IDLE_TIMEOUT_SECS;
    let remote = Backend::new(
        ProviderKind::ChatGpt {
            model: "test".into(),
            user_dir: PathBuf::new(),
        },
        PathBuf::new(),
        600,
    );
    assert_eq!(remote.response_timeout(0), limit);
    let local = Backend::new(
        ProviderKind::Local {
            model_dir: "local-model".into(),
        },
        PathBuf::new(),
        600,
    );
    assert_eq!(local.response_timeout(0), 0);
}

#[test]
fn subscription_stall_is_bounded_retried_and_never_executes_partial_calls() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let layout =
        crate::config::paths::Layout::with_user_dir(&project, temp.path().join("home")).unwrap();
    llm::chatgpt::test_account(&layout.user_dir, false);
    // This exercises the SSE stall path; the WebSocket handshake would consume a script.
    let overlay: toml::Table = toml::from_str("model='account-model'\nprovider='chatgpt'\napproval='auto'\nintegrations=[]\nremote_model_idle_timeout_secs=1\ncodex_websocket=false\n[retry]\nmax_attempts=2\n[sandbox]\nmode='off'\n[improve]\nenabled=false").unwrap();
    let config = crate::config::types::Config::load(&layout, Some(overlay)).unwrap();
    let item = json!({"type":"function_call","namespace":"dial","name":"write","id":"item1","call_id":"call1","status":"completed","arguments":"{\"path\":\"marker.txt\",\"content\":\"must not run\"}"});
    let first = format!(
        "data: {}\n\ndata: {}\n\n",
        json!({"type":"response.output_item.added","output_index":0,"item":item}),
        json!({"type":"response.output_item.done","output_index":0,"item":item})
    );
    let stall = || llm::fake::Script::StallAfter {
        first: first.clone(),
        silence: Duration::from_millis(320),
    };
    let server = llm::fake::serve(vec![stall(), stall()]);
    let mut backend = crate::agent::make_backend(&config)
        .unwrap()
        .with_test_idle_limit(Duration::from_millis(200));
    backend.chatgpt_url = Some(format!("{}/codex/responses", server.url));
    backend.retry_backoff = Duration::from_millis(1);
    let session = crate::storage::session::Session::create(
        &layout,
        crate::storage::session::SessionMeta {
            model: config.model.clone(),
        },
    )
    .unwrap();
    let mut agent = crate::agent::Agent::interactive(
        &config,
        backend,
        layout,
        session,
        None,
        None,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    struct Ui(usize);
    impl crate::agent::Ui for Ui {
        fn heartbeat(&mut self) {
            self.0 += 1;
        }
    }
    let mut ui = Ui(0);
    let started = Instant::now();
    let error = agent
        .turn("Create marker.txt", &mut ui)
        .err()
        .expect("stalled request must fail");
    // A stalled Responses stream made no semantic progress: it is retried as an incomplete
    // stream, within the configured attempts, and never releases the partial tool call.
    assert!(
        format!("{error:#}").contains("no semantic progress"),
        "{error:#}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "each attempt must be bounded by the remote idle limit, not the worker allowance"
    );
    assert!(
        ui.0 > 0,
        "the UI must keep pumping while the provider stalls"
    );
    assert!(!project.join("marker.txt").exists());
    assert_eq!(
        server.requests().len(),
        2,
        "retries stop at retry.max_attempts"
    );
}

#[test]
fn codex_server_without_websocket_costs_one_handshake_per_session() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let layout =
        crate::config::paths::Layout::with_user_dir(&project, temp.path().join("home")).unwrap();
    llm::chatgpt::test_account(&layout.user_dir, false);
    let overlay: toml::Table = toml::from_str("model='account-model'\nprovider='chatgpt'\napproval='auto'\nintegrations=[]\n[sandbox]\nmode='off'\n[improve]\nenabled=false").unwrap();
    let config = crate::config::types::Config::load(&layout, Some(overlay)).unwrap();
    let text = |text: &str| {
        let out = json!({"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]});
        llm::fake::Script::Sse(format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"type":"response.output_text.delta","delta":text}),
            json!({"type":"response.completed","response":{"status":"completed","output":[out],"usage":{"input_tokens":10,"output_tokens":2}}})
        ))
    };
    // The first script answers the WebSocket upgrade with plain HTTP.
    let server = llm::fake::serve(vec![text("unused"), text("one"), text("two")]);
    let mut backend = Backend::new(
        ProviderKind::ChatGpt {
            model: config.model.clone(),
            user_dir: layout.user_dir.clone(),
        },
        config.python.clone(),
        5,
    )
    .with_codex_websocket(true);
    backend.chatgpt_url = Some(format!("{}/codex/responses", server.url));
    let session = crate::storage::session::Session::create(
        &layout,
        crate::storage::session::SessionMeta {
            model: config.model.clone(),
        },
    )
    .unwrap();
    let backend = backend.with_session_id(session.id().to_owned());
    let mut agent = crate::agent::Agent::interactive(
        &config,
        backend,
        layout,
        session,
        None,
        None,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    struct Ui;
    impl crate::agent::Ui for Ui {}
    assert_eq!(agent.turn("first", &mut Ui).unwrap().text, "one");
    assert_eq!(agent.turn("second", &mut Ui).unwrap().text, "two");
    let requests = server.requests();
    assert_eq!(
        requests.len(),
        3,
        "the second request must go straight to SSE"
    );
    assert!(String::from_utf8_lossy(&requests[0]).starts_with("GET "));
    assert!(String::from_utf8_lossy(&requests[2]).starts_with("POST "));
}

#[test]
fn subscription_provider_recovers_disconnection_runs_real_tools_and_replays_results() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let layout =
        crate::config::paths::Layout::with_user_dir(&project, temp.path().join("home")).unwrap();
    llm::chatgpt::test_account(&layout.user_dir, false);
    let overlay:toml::Table=toml::from_str("model='account-model'\nprovider='chatgpt'\napproval='auto'\nintegrations=[]\n[sandbox]\nmode='off'\n[improve]\nenabled=false").unwrap();
    let config = crate::config::types::Config::load(&layout, Some(overlay)).unwrap();
    assert_eq!(
        config.endpoint.as_ref().unwrap().base_url,
        llm::chatgpt::BASE_URL
    );
    let output = json!({"type":"function_call","namespace":"dial","name":"write","id":"item1","call_id":"call1","status":"completed","arguments":"{\"path\":\"marker.txt\",\"content\":\"real output\"}"});
    let final_output = json!({"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Marker written.","annotations":[]}]});
    let sse = |out: serde_json::Value, text: &str| {
        format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"type":"response.output_text.delta","delta":text}),
            json!({"type":"response.completed","response":{"status":"completed","output":[out],"usage":{"input_tokens":100,"output_tokens":12}}})
        )
    };
    let server = llm::fake::serve(vec![
        llm::fake::Script::SseDisconnect(format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\n",
            json!({"type":"response.output_text.delta","delta":"partial answer"}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","namespace":"dial","name":"write","id":"partial-item","call_id":"partial-call","status":"completed","arguments":"{\"path\":\"must-not-exist.txt\",\"content\":\"partial\"}"}}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","namespace":"dial","name":"write","id":"partial-item","call_id":"partial-call","status":"completed","arguments":"{\"path\":\"must-not-exist.txt\",\"content\":\"partial\"}"}}),
        )),
        llm::fake::Script::Sse(sse(output.clone(), "")),
        // The edit's completion review is attached to this request; no extra round.
        llm::fake::Script::Sse(sse(final_output, "Marker written.")),
    ]);
    let mut backend = Backend::new(
        ProviderKind::ChatGpt {
            model: config.model.clone(),
            user_dir: layout.user_dir.clone(),
        },
        config.python.clone(),
        5,
    );
    backend.chatgpt_url = Some(format!("{}/codex/responses", server.url));
    backend.retry_backoff = Duration::from_millis(1);
    let session = crate::storage::session::Session::create(
        &layout,
        crate::storage::session::SessionMeta {
            model: config.model.clone(),
        },
    )
    .unwrap();
    let mut agent = crate::agent::Agent::interactive(
        &config,
        backend,
        layout.clone(),
        session,
        None,
        None,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    struct Ui;
    impl crate::agent::Ui for Ui {}
    let report = agent.turn("Create marker.txt", &mut Ui).unwrap();
    assert_eq!(report.outcome, crate::agent::TurnOutcome::Completed);
    assert_eq!(report.text, "Marker written.");
    assert!(
        !project.join("must-not-exist.txt").exists(),
        "a completed tool item without response.completed must not execute"
    );
    assert_eq!(
        std::fs::read_to_string(project.join("marker.txt")).unwrap(),
        "real output"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(llm::fake::body(&requests[0]), llm::fake::body(&requests[1]));
    let expected_auth = format!("Bearer {}", llm::chatgpt::test_access("oaiapp_test"));
    assert_eq!(
        llm::fake::header(&requests[0], "authorization").as_deref(),
        Some(expected_auth.as_str())
    );
    assert_eq!(
        llm::fake::header(&requests[0], "chatgpt-account-id").as_deref(),
        Some("oaiapp_test")
    );
    assert_eq!(
        llm::fake::header(&requests[0], "originator").as_deref(),
        Some("dial")
    );
    assert_eq!(
        llm::fake::header(&requests[0], "version").as_deref(),
        Some("0.159.0")
    );
    assert_eq!(
        llm::fake::request_line(&requests[0]),
        "POST /codex/responses HTTP/1.1"
    );
    let second = llm::fake::body(&requests[2]);
    let input = second["input"].as_array().unwrap();
    assert!(input.iter().any(|item| item["type"] == "function_call"
        && item["name"] == "write"
        && item.get("namespace").is_none()));
    assert!(
        input
            .iter()
            .any(|item| item["type"] == "function_call_output" && item["call_id"] == "call1")
    );
    assert!(
        second["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "finish")
    );
}

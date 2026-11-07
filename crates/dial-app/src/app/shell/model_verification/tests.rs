use super::*;
use crate::app::shell::testkit::shell;
use crate::llm::{fake, model_config::ModelSelection};

#[test]
fn response_evidence_is_separate_from_configuration_and_cached_metadata() {
    let (_home, _project, app, mut shell) = shell();
    let config = app.config.clone();
    assert!(shell.model_response_status(&config).starts_with("Unknown"));
    shell.catalog.slot.lock().unwrap().1 = Some(Default::default());
    assert!(shell.model_response_status(&config).starts_with("Unknown"));
    shell.model_verification = Some((identity(&config).unwrap(), false));
    assert!(
        shell
            .model_response_status(&config)
            .starts_with("Not verified")
    );
    shell.model_verification = Some((identity(&config).unwrap(), true));
    assert!(shell.model_response_status(&config).starts_with("Verified"));
    for changed in [
        {
            let mut c = config.clone();
            c.model = "different-model".into();
            c
        },
        {
            let mut c = config.clone();
            c.provider = Some("different-provider".into());
            c
        },
        {
            let mut c = config.clone();
            c.retry.max_attempts += 1;
            c
        },
    ] {
        assert!(shell.model_response_status(&changed).starts_with("Unknown"));
    }
}

#[test]
fn cancelled_diagnostic_never_initializes_a_backend_or_creates_state() {
    let root = tempfile::tempdir().unwrap();
    let config = Config {
        user_dir: root.path().join("absent"),
        ..Default::default()
    };
    let error = response_test(
        &config,
        Arc::new(AtomicBool::new(true)),
        Duration::from_secs(1),
    )
    .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    assert!(!config.user_dir.exists());
}

#[test]
fn diagnostic_error_hints_never_copy_secrets_server_bodies_or_urls() {
    for raw in [
        "401 token=TOP_SECRET",
        "connection refused https://secret:password@example.invalid",
        "model not found api_key=PRIVATE",
        "unusual backend failure PRIVATE",
    ] {
        let hint = failure_hint(&anyhow::anyhow!("{raw}"));
        assert!(
            !hint.contains("TOP_SECRET") && !hint.contains("password") && !hint.contains("PRIVATE"),
            "{hint}"
        );
    }
}

#[test]
fn bounded_diagnostic_requires_a_nonempty_response_and_sends_no_tools_or_history() {
    for (status, text, succeeds) in [(200, "OK", true), (200, "", false), (401, "secret", false)] {
        let server = fake::serve(vec![fake::json(
            status,
            &serde_json::json!({
                "choices":[{"message":{"role":"assistant","content":text},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}
            }),
        )]);
        let (_home, _project, _layout, config) = crate::app::tests::config();
        let mut selection = ModelSelection::new("fixture", "test");
        selection.base_url = Some(format!("{}/v1", server.url));
        let mut config = config.with_selection(&selection);
        let mut profile = crate::llm::profiles::ModelProfile::minimal("fixture", "test");
        profile.max_tokens = Some(4096);
        config.model_profiles.insert("diagnostic".into(), profile);
        config.model_profile = Some("diagnostic".into());
        let result = response_test(
            &config,
            Arc::new(AtomicBool::new(false)),
            Duration::from_secs(2),
        );
        assert_eq!(
            result.is_ok(),
            succeeds,
            "status={status} text={text:?}: {result:?}"
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 1, "no automatic retry");
        let body = fake::body(&requests[0]);
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert!(
            body.get("tools")
                .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty))
        );
        assert!(body["max_tokens"].as_u64().is_some_and(|n| n <= 32));
    }
}

#[test]
fn deadline_and_mid_request_cancellation_stop_without_success() {
    let server = fake::serve(vec![fake::Script::Stall {
        silence: Duration::from_secs(2),
    }]);
    let (_home, _project, _layout, config) = crate::app::tests::config();
    let mut selection = ModelSelection::new("fixture", "test");
    selection.base_url = Some(format!("{}/v1", server.url));
    let config = config.with_selection(&selection);
    let started = Instant::now();
    assert!(
        response_test(
            &config,
            Arc::new(AtomicBool::new(false)),
            Duration::from_millis(300)
        )
        .is_err()
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "deadline did not stop promptly"
    );

    // Cancel only after observing request bytes, not a scheduler-dependent sleep.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut selection = ModelSelection::new("fixture", "test");
    selection.base_url = Some(format!("http://{}/v1", listener.local_addr().unwrap()));
    let config = config.with_selection(&selection);
    let cancel = Arc::new(AtomicBool::new(false));
    std::thread::scope(|scope| {
        let flag = cancel.clone();
        scope.spawn(move || {
            use std::io::Read;
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // Accepted sockets can inherit O_NONBLOCK on macOS; the fixture's
                        // bounded read must actually wait for request bytes before cancellation.
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        let mut bytes = [0; 4096];
                        assert!(stream.read(&mut bytes).unwrap() > 0);
                        flag.store(true, Ordering::SeqCst);
                        break;
                    }
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::yield_now()
                    }
                    Err(error) => panic!("request not observed before cancellation: {error}"),
                }
            }
        });
        let error = response_test(&config, cancel, Duration::from_secs(2)).unwrap_err();
        assert!(error.to_string().contains("cancelled"), "{error:#}");
    });
}

#[test]
fn busy_diagnostic_never_queues_a_paid_request_or_changes_evidence() {
    let (_home, _project, app, mut shell) = shell();
    shell.status.phase = crate::ui::status::Phase::Working;
    shell.verify_selected_model(&app, None).unwrap();
    assert!(shell.model_verification.is_none());
    assert!(shell.deferred.is_empty());
    assert!(shell.startup.is_none());
}

use super::*;

fn with_agent(test: impl FnOnce(&mut Agent<'_>, &Layout)) {
    with_configured_agent(|_| {}, test);
}

fn with_configured_agent(
    configure: impl FnOnce(&mut crate::config::types::Config),
    test: impl FnOnce(&mut Agent<'_>, &Layout),
) {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
    let mut config = crate::config::types::Config {
        state_dir: layout.state_dir.clone(),
        compute_locks: false,
        ..Default::default()
    };
    config.codebase.enabled = false;
    config.memory.backend = crate::config::memory_backend::MemoryBackendId::Off;
    configure(&mut config);
    let backend = crate::llm::provider::Backend::new(
        crate::llm::provider::ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "test".into(),
            api_key: None,
        },
        std::path::PathBuf::new(),
        5,
    );
    let session = crate::storage::session::Session::create(
        &layout,
        crate::storage::session::SessionMeta {
            model: "test".into(),
        },
    )
    .unwrap();
    let mut agent = Agent::interactive(
        &config,
        backend,
        layout.clone(),
        session,
        None,
        None,
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .unwrap();
    test(&mut agent, &layout);
}

#[test]
fn empty_session_switch_still_publishes_new_identity() {
    with_agent(|agent, layout| {
        let sides = SideControl::default();
        sides.bind(agent, None).unwrap();
        let first = sides.next_event().unwrap().unwrap();
        assert_eq!(first["session_id"], agent.session().id());
        assert!(sides.next_event().unwrap().is_none());
        let next = crate::storage::session::Session::create(
            layout,
            crate::storage::session::SessionMeta {
                model: "test".into(),
            },
        )
        .unwrap();
        let workspace = agent.prepare_session_workspace(&next).unwrap();
        agent.swap_session_prepared(next, workspace);
        sides.bind(agent, None).unwrap();
        let event = sides.next_event().unwrap().unwrap();
        assert_eq!(event["session_id"], agent.session().id());
        assert_ne!(event["session_id"], first["session_id"]);
        assert_eq!(event["event"], "side_state");
        assert_eq!(event.get("request_id"), Some(&Value::Null));
        assert_eq!(event["state"]["threads"], json!([]));
    });
}

#[test]
fn suspension_withdraws_context_without_losing_session_identity() {
    with_agent(|agent, _| {
        let sides = SideControl::default();
        sides.bind(agent, Some("pending main work")).unwrap();
        assert_eq!(
            sides
                .0
                .lock()
                .unwrap()
                .snapshot
                .as_ref()
                .unwrap()
                .pending_prompt(),
            Some("pending main work")
        );
        sides.suspend().unwrap();
        let error = sides
            .handle(&json!({"action":"ask","question":"why?"}))
            .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<super::super::RequestError>()
                .unwrap()
                .code,
            ErrorCode::Busy
        );
        assert_eq!(
            sides.next_event().unwrap().unwrap()["session_id"],
            agent.session().id()
        );
        sides.bind(agent, None).unwrap();
        assert!(
            sides
                .0
                .lock()
                .unwrap()
                .snapshot
                .as_ref()
                .unwrap()
                .pending_prompt()
                .is_none()
        );
    });
}

#[test]
fn move_admission_refuses_pending_side_writes_and_fences_new_requests_after_settlement() {
    with_agent(|agent, _| {
        let sides = SideControl::default();
        sides.bind(agent, None).unwrap();
        let (pending, sender) = crate::side::tests::pending_request(agent.session());
        sides.0.lock().unwrap().side = pending;
        for cancelled in [false, true] {
            if cancelled {
                assert_eq!(
                    sides.handle(&json!({"action":"cancel"})).unwrap()["cancelled"],
                    true
                );
            }
            let error = sides.prepare_move().unwrap_err();
            assert_eq!(
                error
                    .downcast_ref::<super::super::RequestError>()
                    .unwrap()
                    .code,
                ErrorCode::Busy
            );
        }
        drop(sender);
        sides.next_event().unwrap();
        sides.prepare_move().unwrap();
        let error = sides
            .handle(&json!({"action":"ask","question":"start after admission"}))
            .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<super::super::RequestError>()
                .unwrap()
                .code,
            ErrorCode::Busy
        );
    });
}

#[test]
fn failed_session_load_discards_old_context_and_disconnect_cannot_reopen_it() {
    with_agent(|agent, layout| {
        let sides = SideControl::default();
        sides.bind(agent, None).unwrap();
        let next = crate::storage::session::Session::create(
            layout,
            crate::storage::session::SessionMeta {
                model: "test".into(),
            },
        )
        .unwrap();
        let side_path = crate::storage::side_thread::path_for(next.path().unwrap());
        std::fs::write(&side_path, "not json").unwrap();
        let workspace = agent.prepare_session_workspace(&next).unwrap();
        agent.swap_session_prepared(next, workspace);
        assert!(sides.bind(agent, None).is_err());
        assert!(sides.0.lock().unwrap().snapshot.is_none());
        assert!(sides.0.lock().unwrap().session.is_none());
        assert!(
            sides
                .handle(&json!({"action":"ask","question":"why?"}))
                .is_err()
        );
        sides.cancel();
        std::fs::remove_file(side_path).unwrap();
        sides.bind(agent, None).unwrap();
        assert!(sides.0.lock().unwrap().snapshot.is_none());
        assert!(sides.handle(&json!({"action":"list"})).is_err());
    });
}

#[test]
fn refreshed_context_tracks_rewind_and_compaction_boundaries() {
    with_agent(|agent, _| {
        let sides = SideControl::default();
        for text in ["first", "second"] {
            agent
                .session_mut()
                .append(json!({"role":"user","content":text}))
                .unwrap();
            agent
                .session_mut()
                .append(json!({"role":"assistant","content":"answer"}))
                .unwrap();
        }
        sides.bind(agent, None).unwrap();
        let before = sides
            .0
            .lock()
            .unwrap()
            .snapshot
            .as_ref()
            .unwrap()
            .anchor()
            .map(str::to_owned);
        agent.session_mut().rewind(1).unwrap();
        sides.bind(agent, None).unwrap();
        let rewound = sides
            .0
            .lock()
            .unwrap()
            .snapshot
            .as_ref()
            .unwrap()
            .anchor()
            .map(str::to_owned);
        assert_ne!(before, rewound);
        let id = agent.session().id().to_owned();
        agent
            .session_mut()
            .reset_context(
                &id,
                rewound.as_deref(),
                vec![json!({"role":"user","content":"compacted handoff"})],
                true,
                "a".repeat(64),
            )
            .unwrap();
        sides.bind(agent, None).unwrap();
        let state = sides.0.lock().unwrap();
        let snapshot = state.snapshot.as_ref().unwrap();
        assert_ne!(snapshot.anchor(), rewound.as_deref());
        assert_eq!(snapshot.session_id(), id);
    });
}

#[test]
fn protected_local_side_inference_is_refused_before_creating_a_thread() {
    for (url, locks, requires_admission) in [
        (None, true, true),
        (None, false, false),
        (Some("http://127.0.0.1:9/v1"), true, false),
        (Some("https://inference.example.com/v1"), true, false),
    ] {
        with_configured_agent(
            |config| {
                config.provider = Some(if url.is_some() { "endpoint" } else { "mlx" }.into());
                config.model = "test".into();
                config.compute_locks = locks;
                config.endpoint = url.map(|url| crate::config::types::EndpointConfig {
                    base_url: url.into(),
                    headers: None,
                    api_key_env: None,
                    allow_insecure: true,
                    protocol: None,
                    account_id: None,
                });
            },
            |agent, _| {
                let sides = SideControl::default();
                sides.bind(agent, None).unwrap();
                assert_eq!(
                    sides
                        .0
                        .lock()
                        .unwrap()
                        .snapshot
                        .as_ref()
                        .unwrap()
                        .requires_compute_lock(),
                    requires_admission,
                    "{url:?}, locks={locks}"
                );
                if requires_admission {
                    let error = sides
                        .handle(&json!({"action":"ask","question":"why?"}))
                        .unwrap_err();
                    assert_eq!(
                        error
                            .downcast_ref::<super::super::RequestError>()
                            .unwrap()
                            .code,
                        ErrorCode::Refused
                    );
                    let state = sides.handle(&json!({"action":"list"})).unwrap();
                    assert_eq!(state["threads"], json!([]));
                    assert!(state["streaming"].is_null());
                }
            },
        );
    }
}

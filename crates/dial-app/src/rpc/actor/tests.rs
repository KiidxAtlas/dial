use super::*;
use std::cell::RefCell;

#[test]
fn panics_reply_to_pending_request_then_fatal_and_stop() {
    for id in [None, Some(json!(17)), Some(json!("request"))] {
        let events = RefCell::new(Vec::new());
        let outcome = run_boundary(
            |pending| {
                *pending = id.clone();
                panic!("private panic payload");
            },
            |id, result| {
                let error = result.unwrap_err();
                assert_eq!(
                    error.downcast_ref::<RequestError>().unwrap().code,
                    ErrorCode::InternalError
                );
                events
                    .borrow_mut()
                    .push(json!({"reply":id,"error":error.to_string()}));
            },
            |message| events.borrow_mut().push(json!({"fatal":message})),
        );
        assert!(outcome.is_err());
        let events = events.into_inner();
        assert_eq!(events.len(), if id.is_some() { 2 } else { 1 });
        if let Some(id) = id {
            assert_eq!(events[0]["reply"], id);
        }
        assert!(events.last().unwrap()["fatal"].is_string());
        assert!(
            !events
                .iter()
                .any(|event| event.to_string().contains("private panic payload"))
        );
    }
}

#[test]
fn normal_actor_failure_emits_fatal_without_duplicate_request_reply() {
    let events = RefCell::new(Vec::new());
    let outcome = run_boundary(
        |_| anyhow::bail!("initialization failed"),
        |_, _| panic!("normal errors already have request replies"),
        |message| events.borrow_mut().push(message.to_owned()),
    );
    assert!(outcome.is_err());
    assert_eq!(*events.borrow(), ["initialization failed"]);
    assert!(
        run_boundary(
            |_| Ok(()),
            |_, _| panic!("unexpected reply"),
            |_| panic!("unexpected fatal")
        )
        .is_ok()
    );
}

fn with_runtime(configure: impl FnOnce(&mut Config), f: impl FnOnce(&mut ActorRuntime<'_>)) {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
    let mut config = Config {
        state_dir: layout.state_dir.clone(),
        compute_locks: false,
        ..Config::default()
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
    let flag = Arc::new(AtomicBool::new(false));
    let agent = Agent::interactive(
        &config,
        backend,
        layout.clone(),
        Session::create(
            &layout,
            SessionMeta {
                model: "test".into(),
            },
        )
        .unwrap(),
        None,
        None,
        flag.clone(),
    )
    .unwrap();
    let out = Sender {
        output: Arc::new(Mutex::new(Output {
            acp: None,
            protocol: DEFAULT_PROTOCOL_VERSION,
            interactive: false,
            envelope: Envelope::default(),
        })),
        writer: crate::output::StdoutWriter::new().unwrap(),
        failed: Arc::new(AtomicBool::new(false)),
        failure: Arc::new(OnceLock::new()),
    };
    let (_, approvals) = mpsc::channel();
    let (_, questions) = mpsc::channel();
    let (_, rx) = mpsc::channel();
    let ui = RpcUi {
        out: out.clone(),
        approvals,
        pending_approval: Arc::new(Mutex::new(None)),
        question_answers: questions,
        pending_question: Arc::new(Mutex::new(None)),
        interrupt: flag.clone(),
        input_disconnected: Arc::new(AtomicBool::new(false)),
        steering: Arc::new(Mutex::new(Vec::new())),
        prompt_queued: Arc::new(AtomicBool::new(false)),
        next: 0,
        node_cursors: Default::default(),
        node_revisions: Default::default(),
        node_meta: Default::default(),
        projection_dirty: false,
        tree: None,
    };
    let mut runtime = ActorRuntime {
        parked: Default::default(),
        ui,
        agent,
        compute: None,
        layout: &layout,
        config: &config,
        overlay: &toml::Table::new(),
        acp: true,
        rx,
        worker_out: &out,
        flag,
        active: Arc::new(AtomicBool::new(false)),
        worker_closing: Arc::new(AtomicBool::new(false)),
        worker_prompt_queued: Arc::new(AtomicBool::new(false)),
        worker_host: Default::default(),
        worker_uris: Default::default(),
        worker_query_cache: Default::default(),
        worker_live: Default::default(),
        projected_messages: 0,
        projected_at: Instant::now(),
        current: true,
    };
    f(&mut runtime);
}

#[cfg(unix)]
#[test]
fn implicit_parked_activation_obeys_switch_veto_without_teardown() {
    with_runtime(
        |config| {
            config.hooks.push(crate::config::types::HookConfig {
                event: "session_before_switch".into(),
                matcher: None,
                command: vec![
                    "sh".into(),
                    "-c".into(),
                    "printf '{\"decision\":\"block\",\"reason\":\"stay here\"}'".into(),
                ],
                timeout_secs: 5,
                fail_closed: None,
            });
        },
        |runtime| {
            let current = runtime.agent.session().id().to_owned();
            let next = Session::create(
                runtime.layout,
                SessionMeta {
                    model: "test".into(),
                },
            )
            .unwrap();
            let wanted = next.id().to_owned();
            runtime
                .parked
                .insert(wanted.clone(), ParkedSession::fresh(&runtime.agent, next));
            runtime.agent.set_plan_mode(true);
            let background = runtime.agent.background_control().unwrap();
            runtime.worker_uris.declare(vec!["ticket".into()]);
            let error = runtime
                .handle_request(
                    &json!({"method":"plan","params":{"session_id":wanted,"enabled":false}}),
                )
                .unwrap_err();
            assert!(error.to_string().contains("stay here"), "{error:#}");
            assert_eq!(runtime.agent.session().id(), current);
            assert!(runtime.agent.plan_mode());
            assert!(runtime.parked.contains_key(&wanted));
            assert!(Arc::ptr_eq(
                &background,
                &runtime.agent.background_control().unwrap()
            ));
            assert!(
                crate::tools::integrations::host_uri::HostUriResolver::serves(
                    &host_uris::Resolver::new(
                        runtime.worker_uris.clone(),
                        runtime.worker_out.clone()
                    ),
                    "ticket"
                )
            );
        },
    );
}

#[test]
fn session_activation_replaces_roots_and_restores_saved_incoming_roots() {
    for acp in [false, true] {
        with_runtime(
            |_| {},
            |runtime| {
                runtime.acp = acp;
                let a_dir = tempfile::tempdir().unwrap();
                let b_dir = tempfile::tempdir().unwrap();
                let a_root = a_dir.path().canonicalize().unwrap();
                let b_root = b_dir.path().canonicalize().unwrap();
                runtime.agent.workspace_mut().add_root(&a_root).unwrap();
                runtime.agent.persist_workspace_roots().unwrap();
                let a_id = runtime.agent.session().id().to_owned();
                let mut b = Session::create(
                    runtime.layout,
                    SessionMeta {
                        model: "test".into(),
                    },
                )
                .unwrap();
                b.set_additional_dirs(std::slice::from_ref(&b_root))
                    .unwrap();
                let b_id = b.id().to_owned();
                drop(b);
                runtime.resume(&json!({"session_id":b_id})).unwrap();
                assert_eq!(
                    runtime.agent.workspace().additional_roots(),
                    std::slice::from_ref(&b_root),
                    "acp={acp}"
                );
                assert!(
                    runtime
                        .agent
                        .workspace()
                        .resolve(a_root.to_str().unwrap())
                        .is_err()
                );
                let b_file = b_root.join("checkpoint.txt");
                std::fs::write(&b_file, "before").unwrap();
                let checkpoints = runtime.agent.checkpoints().unwrap().clone();
                checkpoints.begin_turn("incoming edit").unwrap();
                runtime.agent.workspace().observe_write(&b_file).unwrap();
                std::fs::write(&b_file, "after").unwrap();
                assert_eq!(
                    checkpoints.list()[0].files,
                    1,
                    "incoming root must be checkpointed"
                );
                checkpoints.rollback(1).unwrap();
                assert_eq!(std::fs::read_to_string(&b_file).unwrap(), "before");
                runtime.new_session().unwrap();
                assert!(
                    runtime.agent.workspace().additional_roots().is_empty(),
                    "acp={acp}"
                );
                runtime.resume(&json!({"session_id":a_id})).unwrap();
                assert_eq!(
                    runtime.agent.workspace().additional_roots(),
                    [a_root],
                    "acp={acp}"
                );
                runtime.resume(&json!({"session_id":b_id})).unwrap();
                assert_eq!(
                    runtime.agent.workspace().additional_roots(),
                    [b_root],
                    "acp={acp}"
                );
            },
        );
    }
}

#[test]
fn invalid_parked_checkpoint_setup_preserves_active_session_and_writer() {
    with_runtime(
        |_| {},
        |runtime| {
            let current = runtime.agent.session().id().to_owned();
            let next = Session::create(
                runtime.layout,
                SessionMeta {
                    model: "test".into(),
                },
            )
            .unwrap();
            let wanted = next.id().to_owned();
            runtime
                .parked
                .insert(wanted.clone(), ParkedSession::fresh(&runtime.agent, next));
            std::fs::write(
                runtime.layout.checkpoints_dir().join(&wanted),
                "not a directory",
            )
            .unwrap();
            let background = runtime.agent.background_control().unwrap();
            assert!(runtime.activate_parked(&wanted).is_err());
            assert_eq!(runtime.agent.session().id(), current);
            assert!(runtime.parked.contains_key(&wanted));
            assert!(Arc::ptr_eq(
                &background,
                &runtime.agent.background_control().unwrap()
            ));
        },
    );
}

#[test]
fn returning_to_parked_session_does_not_restore_sensitive_grants() {
    with_runtime(
        |_| {},
        |runtime| {
            let a_id = runtime.agent.session().id().to_owned();
            let sensitive = runtime.layout.user_dir.join("sensitive-test");
            std::fs::create_dir_all(&sensitive).unwrap();
            let sensitive = sensitive.canonicalize().unwrap();
            let file = sensitive.join("note");
            std::fs::write(&file, "private").unwrap();
            runtime.agent.workspace_mut().add_root(&sensitive).unwrap();
            runtime
                .agent
                .workspace_mut()
                .grant_sensitive_directory(&sensitive)
                .unwrap();
            runtime.agent.persist_workspace_roots().unwrap();
            assert!(
                runtime
                    .agent
                    .workspace()
                    .resolve(file.to_str().unwrap())
                    .is_ok()
            );
            runtime.new_session().unwrap();
            runtime.activate_parked(&a_id).unwrap();
            assert_eq!(runtime.agent.workspace().additional_roots(), [sensitive]);
            assert!(
                runtime
                    .agent
                    .workspace()
                    .resolve(file.to_str().unwrap())
                    .is_err()
            );
        },
    );
}

fn code(error: &anyhow::Error) -> ErrorCode {
    error
        .downcast_ref::<RequestError>()
        .unwrap_or_else(|| panic!("uncoded error: {error:#}"))
        .code
}

#[test]
fn set_approval_changes_the_mode_and_refuses_unknown_modes() {
    with_runtime(
        |_| {},
        |runtime| {
            runtime.acp = false;
            let result = runtime
                .handle_request(&json!({"method":"set_approval","params":{"mode":"readonly"}}))
                .unwrap();
            assert_eq!(result["approval"], json!(runtime.agent.approval()));
            assert_eq!(
                runtime.agent.approval(),
                crate::config::types::Approval::Readonly
            );
            let error = runtime
                .handle_request(&json!({"method":"set_approval","params":{"mode":"yolo"}}))
                .unwrap_err();
            assert_eq!(code(&error), ErrorCode::InvalidParams);
            assert_eq!(
                runtime.agent.approval(),
                crate::config::types::Approval::Readonly
            );
        },
    );
}

#[test]
fn add_and_remove_dir_use_the_workspace_owner() {
    with_runtime(
        |_| {},
        |runtime| {
            runtime.acp = false;
            let extra = tempfile::tempdir().unwrap();
            let path = extra.path().canonicalize().unwrap();
            let added = runtime
                .handle_request(&json!({"method":"add_dir","params":{"path":path}}))
                .unwrap();
            assert_eq!(added["changed"], true);
            assert_eq!(added["dirs"], json!([path]));
            assert_eq!(
                runtime.agent.workspace().additional_roots(),
                std::slice::from_ref(&path)
            );
            let again = runtime
                .handle_request(&json!({"method":"add_dir","params":{"path":path}}))
                .unwrap();
            assert_eq!(again["changed"], false);
            let removed = runtime
                .handle_request(&json!({"method":"remove_dir","params":{"path":path}}))
                .unwrap();
            assert_eq!(removed["changed"], true);
            assert_eq!(removed["dirs"], json!([]));
            let error = runtime
                .handle_request(&json!({"method":"add_dir","params":{}}))
                .unwrap_err();
            assert_eq!(code(&error), ErrorCode::InvalidParams);
        },
    );
}

#[test]
fn retry_refuses_when_there_is_no_unanswered_prompt() {
    with_runtime(
        |_| {},
        |runtime| {
            runtime.acp = false;
            let error = runtime
                .handle_request(&json!({"method":"retry","params":{}}))
                .unwrap_err();
            assert_eq!(code(&error), ErrorCode::InvalidParams);
            assert!(error.to_string().contains("session is empty"), "{error:#}");
        },
    );
}

#[test]
fn restart_model_refuses_external_endpoints() {
    with_runtime(
        |_| {},
        |runtime| {
            runtime.acp = false;
            let error = runtime
                .handle_request(&json!({"method":"restart_model","params":{}}))
                .unwrap_err();
            assert_eq!(code(&error), ErrorCode::InvalidParams);
            assert!(
                error.to_string().contains("external endpoints"),
                "{error:#}"
            );
        },
    );
}

#[test]
fn send_validates_attachments_before_starting_a_turn() {
    with_runtime(
        |_| {},
        |runtime| {
            runtime.acp = false;
            for params in [
                json!({"text":"look","images":"one.png"}),
                json!({"text":"look","images":[1]}),
                json!({"text":"look","inline_images":["%%%"]}),
                // The test endpoint does not accept images, so a valid attachment is refused too.
                json!({"text":"look","inline_images":["aGVsbG8="]}),
            ] {
                let error = runtime
                    .handle_request(&json!({"method":"send","params":params}))
                    .unwrap_err();
                assert_eq!(code(&error), ErrorCode::InvalidParams, "{params}");
            }
            assert_eq!(runtime.agent.session().message_count(), 0);
        },
    );
}

#[test]
fn available_commands_carry_their_rpc_surface() {
    with_runtime(
        |_| {},
        |runtime| {
            runtime.acp = false;
            let result = runtime
                .handle_request(&json!({"method":"get_available_commands","params":{}}))
                .unwrap();
            let commands = result["commands"].as_array().unwrap();
            let find = |name: &str| commands.iter().find(|row| row["name"] == name).unwrap();
            assert_eq!(
                find("add-dir")["rpc"],
                json!({"surface":"method","method":"add_dir"})
            );
            assert_eq!(find("commit")["rpc"]["surface"], "terminal");
            assert!(
                commands.iter().all(|row| row["rpc"].is_object()),
                "{commands:?}"
            );
        },
    );
}

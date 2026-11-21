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

pub(super) fn with_runtime(
    configure: impl FnOnce(&mut Config),
    f: impl FnOnce(&mut ActorRuntime<'_>),
) {
    // Actor prompt routing consults the process-wide room. Keep fixture sessions isolated from
    // collaboration's guest/host fixtures, including the guest retry admission regression.
    let _room = collaboration::TEST_ROOM.lock().unwrap();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
    let mut config = Config {
        state_dir: layout.state_dir.clone(),
        model: "test".into(),
        use_case: "coding".into(),
        compute_locks: false,
        ..Config::default()
    };
    config.codebase.enabled = false;
    config.memory.backend = crate::config::memory_backend::MemoryBackendId::Off;
    config.improve.enabled = false;
    configure(&mut config);
    let (store, _) = serving_setup(&config).unwrap();
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
        store.as_ref(),
        flag.clone(),
    )
    .unwrap();
    let out = Sender {
        output: Arc::new(Mutex::new(Output {
            acp: None,
            protocol: DEFAULT_PROTOCOL_VERSION,
            interactive: false,
            tree_events: true,
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
        steering: Default::default(),
        prompt_queued: Arc::new(AtomicBool::new(false)),
        next: 0,
        node_cursors: Default::default(),
        node_revisions: Default::default(),
        node_meta: Default::default(),
        requests_done: Default::default(),
        projection_dirty: false,
        tree: None,
    };
    let mut runtime = ActorRuntime {
        steering_cleanup: SteeringCleanup::new(&ui),
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
        recovery_reply: None,
        current: true,
        sides: Default::default(),
    };
    // Exercise runtime unwinding without poisoning the process-wide fixture isolation lock.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        f(&mut runtime);
    }));
    drop(_room);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn acp_contended_resume_refuses_without_creating_a_sibling() {
    with_runtime(
        |_| {},
        |runtime| {
            let held = Session::create(
                runtime.layout,
                SessionMeta {
                    model: "test".into(),
                },
            )
            .unwrap();
            let before = Session::find_prefix(runtime.layout, "").unwrap().len();
            let active = runtime.agent.session().id().to_owned();
            let result = runtime.resume(&json!({"session_id":held.id()}));
            assert!(
                result.is_err(),
                "ACP must preserve the requested session identity"
            );
            assert_eq!(runtime.agent.session().id(), active);
            assert_eq!(
                Session::find_prefix(runtime.layout, "").unwrap().len(),
                before
            );
        },
    );
}

#[test]
fn native_session_switches_reseed_live_interactions() {
    for method in ["new", "resume", "fork"] {
        with_runtime(
            |_| {},
            |runtime| {
                runtime.acp = false;
                runtime
                    .worker_out
                    .emit(json!({"event":"todo","tasks":[{"id":"old"}]}));
                match method {
                    "new" => {
                        runtime.new_session().unwrap();
                    }
                    "resume" => {
                        let session = Session::create(
                            runtime.layout,
                            SessionMeta {
                                model: "test".into(),
                            },
                        )
                        .unwrap();
                        let id = session.id().to_owned();
                        drop(session);
                        runtime.resume(&json!({"session_id":id})).unwrap();
                    }
                    "fork" => {
                        runtime.fork_native(&json!({})).unwrap();
                    }
                    _ => unreachable!(),
                }
                let output = runtime.worker_out.output.lock().unwrap();
                let live = output
                    .envelope
                    .live
                    .snapshot(output.envelope.session_id.as_deref(), 0);
                assert_eq!(live["session_id"], runtime.agent.session().id(), "{method}");
                assert_eq!(
                    live["todo"],
                    json!(runtime.agent.todo().snapshot()),
                    "{method}"
                );
            },
        );
    }
}

#[test]
fn steering_admission_is_scoped_and_settlement_discards_unconsumed_input() {
    with_runtime(
        |_| {},
        |runtime| {
            // An administrative request may hold generic busy without owning a model turn.
            runtime.active.store(true, Ordering::SeqCst);
            assert!(
                runtime
                    .ui
                    .steering
                    .lock()
                    .unwrap()
                    .enqueue("not a turn", None)
                    .is_err()
            );
            let session = runtime.agent.session().id().to_owned();
            {
                let mut steering = runtime.ui.steering.lock().unwrap();
                steering.begin(&session, json!(1));
                assert!(steering.enqueue("wrong session", Some("other")).is_err());
                steering.enqueue("late input", Some(&session)).unwrap();
            }
            runtime.settle_request(&json!({"method":"send"}), &json!(1), Ok(json!({})));
            assert!(runtime.ui.take_steering().is_empty());
            assert!(
                runtime
                    .ui
                    .steering
                    .lock()
                    .unwrap()
                    .enqueue("after turn", None)
                    .is_err()
            );
            runtime
                .ui
                .steering
                .lock()
                .unwrap()
                .begin("next-session", json!(2));
            assert!(runtime.ui.take_steering().is_empty());
        },
    );
}

#[test]
fn panic_after_steering_begin_closes_admission_and_discards_pending_input() {
    let retained = RefCell::new(None);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        with_runtime(
            |_| {},
            |runtime| {
                let steering = runtime.ui.steering.clone();
                {
                    let mut state = steering.lock().unwrap();
                    state.begin(runtime.agent.session().id(), json!(1));
                    state.enqueue("pending input", None).unwrap();
                }
                let seq = runtime.worker_out.output.lock().unwrap().envelope.seq;
                *retained.borrow_mut() = Some((steering, runtime.worker_out.clone(), seq));
                panic!("panic after turn admission");
            },
        );
    }));
    assert!(outcome.is_err());
    let (steering, out, seq) = retained.into_inner().unwrap();
    assert_eq!(
        out.output.lock().unwrap().envelope.seq,
        seq + 1,
        "usable transport must publish the unconsumed-input notice"
    );
    let mut state = steering
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(
        state.pending.is_empty(),
        "pending steering must be settled during unwind"
    );
    assert!(state.enqueue("after panic", None).is_err());
}

#[test]
fn serving_setup_never_initializes_learning_when_disabled_and_honors_enabled_pin() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config {
        model: "original-model".into(),
        use_case: "coding".into(),
        state_dir: dir.path().join("learning"),
        ..Config::default()
    };
    config.improve.enabled = false;
    let (store, serving) = serving_setup(&config).unwrap();
    assert!(store.is_none());
    assert_eq!(serving.model, config.model);
    assert!(!config.state_dir.exists());

    config.improve.enabled = true;
    let (store, _) = serving_setup(&config).unwrap();
    store
        .unwrap()
        .set_meta(
            "serving_capabilities",
            &json!({"resolved_model":"pinned-model"}).to_string(),
        )
        .unwrap();
    let (store, serving) = serving_setup(&config).unwrap();
    assert!(store.is_some());
    assert_eq!(serving.model, "pinned-model");
    assert_eq!(config.model, "original-model");

    config.improve.enabled = false;
    let (store, serving) = serving_setup(&config).unwrap();
    assert!(store.is_none());
    assert_eq!(serving.model, "original-model");
}

#[test]
fn rpc_runtime_adopts_an_admitted_candidate_at_next_turn_without_rebuild() {
    with_runtime(
        |config| config.improve.enabled = true,
        |runtime| {
            let baseline = runtime.agent.version().clone();
            assert_ne!(baseline.id, 0);
            let store = crate::storage::store::Store::open(
                &runtime.config.state_dir,
                &runtime.config.model,
                &runtime.config.use_case,
                &runtime.config.harness,
            )
            .unwrap();
            let mut harness = baseline.harness.clone();
            harness
                .system_prompt
                .push_str("\nAccepted runtime regression marker.");
            let candidate = store
                .candidate(baseline.id, "harness", &harness, None)
                .unwrap();
            store
                .admit(&crate::config::types::Admission {
                    candidate: candidate.id,
                    baseline: baseline.id,
                    passed: true,
                    reason: "measured regression fixture".into(),
                    scores: [("primary", true), ("guard", false)]
                        .into_iter()
                        .map(|(name, primary)| crate::config::types::SuiteScore {
                            name: name.into(),
                            primary,
                            baseline: 50.0,
                            candidate: 60.0,
                            cases: 10,
                            candidate_only: 0,
                            baseline_only: 0,
                            tier: None,
                        })
                        .collect(),
                })
                .unwrap();
            assert_eq!(runtime.agent.version().id, baseline.id);
            // Admission runs before the interrupted model loop, so no provider or real model is
            // needed to observe the same between-turn refresh used by RPC prompts.
            runtime.flag.store(true, Ordering::SeqCst);
            runtime
                .agent
                .turn_preserving_interrupt("observe admitted version", &mut runtime.ui)
                .unwrap();
            assert_eq!(runtime.agent.version().id, candidate.id);
            assert_eq!(
                runtime.agent.version().harness.system_prompt,
                harness.system_prompt
            );
        },
    );
}

#[test]
fn move_rebuild_publishes_destination_to_queries_and_controller_administration() {
    with_runtime(
        |_| {},
        |runtime| {
            let destination = tempfile::tempdir().unwrap();
            let initial = runtime.layout.clone();
            let model = initial.user_dir.join("move-test-model");
            std::fs::create_dir_all(&model).unwrap();
            std::fs::write(model.join("config.json"), "{}").unwrap();
            std::fs::write(model.join("model.safetensors"), "weights").unwrap();
            let mut config = runtime.config.clone();
            config.model = model.to_string_lossy().into_owned();
            config.compute_locks = true;
            let mut overlay = toml::Table::new();
            overlay.insert("model".into(), toml::Value::String(config.model.clone()));
            overlay.insert("compute_locks".into(), toml::Value::Boolean(true));
            overlay.insert(
                "memory".into(),
                toml::Value::Table(toml::Table::from_iter([(
                    "backend".into(),
                    toml::Value::String("off".into()),
                )])),
            );
            overlay.insert(
                "codebase".into(),
                toml::Value::Table(toml::Table::from_iter([(
                    "enabled".into(),
                    toml::Value::Boolean(false),
                )])),
            );
            let published_config = Arc::new(std::sync::RwLock::new(config.clone()));
            let published_layout = Arc::new(std::sync::RwLock::new(initial.clone()));
            let admin = Admin {
                layout: published_layout.clone(),
                config: published_config.clone(),
                overlay: overlay.clone(),
            };
            let (requests, rx) = mpsc::channel();
            // Admission precedes branch range validation, so this acquires the initial lease
            // without running inference. The move must reattribute that retained lease.
            requests
                .send(json!({"id":"admit","method":"branch_session","params":{
                    "turns":0,"confirm":true,
                }}))
                .unwrap();
            requests
                .send(json!({"id":"move","method":"move_session","params":{
                    "path":destination.path(),"confirm":true,
                }}))
                .unwrap();
            drop(requests);
            let (_, approval_rx) = mpsc::channel();
            let (_, question_rx) = mpsc::channel();
            let (control_tx, _) = mpsc::sync_channel(4);
            let actor = Actor {
                layout: initial.clone(),
                config,
                overlay,
                acp: false,
                launch: Default::default(),
                rx,
                approval_rx,
                question_rx,
                control_tx,
                worker_out: runtime.worker_out.clone(),
                flag: runtime.flag.clone(),
                active: runtime.active.clone(),
                worker_closing: runtime.worker_closing.clone(),
                worker_prompt_queued: runtime.worker_prompt_queued.clone(),
                worker_pending: Default::default(),
                worker_host: runtime.worker_host.clone(),
                worker_uris: runtime.worker_uris.clone(),
                worker_questions: Default::default(),
                worker_disconnected: Default::default(),
                worker_query_cache: runtime.worker_query_cache.clone(),
                worker_live: runtime.worker_live.clone(),
                queue: Default::default(),
                worker_fs: Default::default(),
                admin_config: published_config.clone(),
                admin_layout: published_layout.clone(),
                sides: runtime.sides.clone(),
            };
            actor.run().unwrap();
            let target = destination.path().canonicalize().unwrap();
            assert_eq!(admin.layout().project_root, target);
            let config = published_config.read().unwrap();
            assert_eq!(config.project_root, target);
            assert_eq!(config.state_dir, config.learning_dir(&admin.layout()));
            let owner: Value = serde_json::from_slice(
                &std::fs::read(initial.user_dir.join("compute-owner.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(owner["project"], json!(target));
            assert_eq!(owner["state"], json!(config.state_dir));
            assert_eq!(owner["model"], json!(config.model));
            drop(config);
            let projection = runtime.worker_query_cache.lock().unwrap();
            assert_eq!(projection.state["workspace"]["root"], json!(target));
            let session_id = projection.state["session_id"].as_str().unwrap();
            let listed = admin
                .handle(
                    "list_sessions",
                    &json!({"include_empty":true}),
                    Admin::current(&projection),
                )
                .unwrap();
            assert!(
                listed["sessions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|row| row["session_id"] == session_id)
            );
            assert!(
                !Session::list_by_activity(&initial, usize::MAX)
                    .unwrap()
                    .iter()
                    .any(|session| session.id == session_id)
            );
        },
    );
}

#[test]
fn branching_waits_for_compute_before_changing_the_conversation() {
    with_runtime(
        |config| {
            config.compute_locks = true;
            config.model = "test-local-model".into();
        },
        |runtime| {
            runtime.acp = false;
            runtime
                .agent
                .session_mut()
                .append(json!({"role":"user","content":"keep history"}))
                .unwrap();
            let messages = runtime.agent.session().messages();
            runtime.flag.store(true, Ordering::SeqCst);
            assert!(
                runtime
                    .handle_request(&json!({"method":"branch_session","params":{
                        "turns":1,"confirm":true,
                    }}))
                    .is_err()
            );
            assert!(runtime.compute.is_none());
            assert_eq!(runtime.agent.session().messages(), messages);
        },
    );
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
                .handle_request(&json!({"method":"set_approval","params":{"mode":"nope"}}))
                .unwrap_err();
            assert_eq!(code(&error), ErrorCode::InvalidParams);
            assert_eq!(
                runtime.agent.approval(),
                crate::config::types::Approval::Readonly
            );
            // YOLO is chosen explicitly over RPC too, and reported with its own name.
            let result = runtime
                .handle_request(&json!({"method":"set_approval","params":{"mode":"yolo"}}))
                .unwrap();
            assert_eq!(result["approval"], json!("yolo"));
            assert_eq!(
                runtime.agent.approval(),
                crate::config::types::Approval::Yolo
            );
        },
    );
}

#[test]
fn permission_rules_list_add_and_remove_through_the_session_owner() {
    with_runtime(
        |_| {},
        |runtime| {
            runtime.acp = false;
            let listed = runtime
                .handle_request(&json!({"method":"list_permission_rules"}))
                .unwrap();
            assert_eq!(listed, json!({"rules":[]}));
            let error = runtime
                .handle_request(&json!({"method":"list_permission_rules","params":{"all":true}}))
                .unwrap_err();
            assert_eq!(code(&error), ErrorCode::InvalidParams);

            // Bash prefixes are parsed like `/permissions allow`: quotes and spacing normalise to argv.
            let added = runtime
                .handle_request(&json!({"method":"add_permission_rule","params":{
                    "effect":"allow","tool":"bash","command_prefix":["git"," status "]
                }}))
                .unwrap();
            assert_eq!(added["rule"]["index"], json!(0));
            assert_eq!(added["rule"]["effect"], json!("allow"));
            assert_eq!(added["rule"]["command_prefix"], json!(["git", "status"]));
            assert_eq!(added["persisted"], json!(false));
            assert_eq!(added["count"], json!(1));

            let added = runtime
                .handle_request(&json!({"method":"add_permission_rule","params":{
                    "effect":"deny","tool":"read","paths":["*.pem"]
                }}))
                .unwrap();
            assert_eq!(added["rule"]["index"], json!(1));
            assert_eq!(added["rule"]["effect"], json!("deny"));
            assert_eq!(added["rule"]["paths"], json!(["*.pem"]));
            assert_eq!(runtime.agent.permission_rules().len(), 2);

            let listed = runtime
                .handle_request(&json!({"method":"list_permission_rules"}))
                .unwrap();
            assert_eq!(listed["rules"][1]["index"], json!(1));
            assert_eq!(listed["rules"][1]["tool"], json!("read"));
            assert_eq!(listed["rules"][1]["scope"], json!("deny read on *.pem"));

            let removed = runtime
                .handle_request(&json!({"method":"remove_permission_rule","params":{"index":0}}))
                .unwrap();
            assert_eq!(removed["removed"]["tool"], json!("bash"));
            assert_eq!(removed["count"], json!(1));
            assert_eq!(runtime.agent.permission_rules()[0].tool, "read");

            let error = runtime
                .handle_request(&json!({"method":"remove_permission_rule","params":{"index":1}}))
                .unwrap_err();
            assert_eq!(code(&error), ErrorCode::NotFound);
            assert_eq!(runtime.agent.permission_rules().len(), 1);
        },
    );
}

#[test]
fn permission_rule_requests_refuse_invalid_rules_without_changing_them() {
    let cases = [
        json!({"effect":"ask","tool":"read"}),
        json!({"effect":"allow","tool":""}),
        json!({"effect":"allow","tool":"read worker"}),
        json!({"effect":"allow","tool":"bash"}),
        json!({"effect":"allow","tool":"bash","command_prefix":["git",""]}),
        json!({"effect":"allow","tool":"bash","command_prefix":["git","push","--force","origin"]}),
        json!({"effect":"allow","tool":"bash","command_prefix":["git","status","&&","rm","x"]}),
        json!({"effect":"allow","tool":"read","command_prefix":["git"]}),
        json!({"effect":"allow","tool":"bash","command_prefix":["git"],"paths":["*.rs"]}),
        json!({"effect":"allow","tool":"read","paths":[""]}),
        json!({"effect":"allow","tool":"read","unknown":true}),
    ];
    with_runtime(
        |_| {},
        |runtime| {
            runtime.acp = false;
            for params in cases {
                let error = runtime
                    .handle_request(&json!({"method":"add_permission_rule","params":params}))
                    .unwrap_err();
                assert_eq!(code(&error), ErrorCode::InvalidParams, "{params}");
            }
            assert!(runtime.agent.permission_rules().is_empty());
            let error = runtime
                .handle_request(&json!({"method":"remove_permission_rule","params":{"index":"0"}}))
                .unwrap_err();
            assert_eq!(code(&error), ErrorCode::InvalidParams);
        },
    );
}

#[test]
fn persisted_permission_rules_reach_the_project_store_and_leave_it_on_remove() {
    with_runtime(
        |_| {},
        |runtime| {
            runtime.acp = false;
            let store = runtime.layout.state_dir.join("permissions.toml");
            runtime
                .handle_request(&json!({"method":"add_permission_rule","params":{
                    "effect":"allow","tool":"bash","command_prefix":["cargo","test"],"persist":true
                }}))
                .unwrap();
            let saved = crate::agent::safety::approval::load(&store).unwrap();
            assert_eq!(saved.len(), 1);
            assert_eq!(saved[0].command_prefix, ["cargo", "test"]);
            let removed = runtime
                .handle_request(&json!({
                    "method":"remove_permission_rule",
                    "params":{"index":0,"persist":true}
                }))
                .unwrap();
            assert_eq!(removed["persisted"], json!(true));
            assert!(
                crate::agent::safety::approval::load(&store)
                    .unwrap()
                    .is_empty()
            );
            assert!(runtime.agent.permission_rules().is_empty());
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

#[test]
fn diagnostics_refuse_bad_params_with_coded_errors() {
    with_runtime(
        |_| {},
        |runtime| {
            runtime.acp = false;
            let refused = [
                ("evidence", json!({})),
                ("evidence", json!({"action":"bogus"})),
                ("evidence", json!({"action":"inspect"})),
                ("evidence", json!({"action":"inspect","reference":"a b"})),
                ("evidence", json!({"action":"lineage","references":[]})),
                (
                    "evidence",
                    json!({"action":"lineage","references":vec!["x"; 17]}),
                ),
                (
                    "evidence",
                    json!({"action":"compare","reference":"artifact://a"}),
                ),
                ("evidence_correct", json!({"reference":"artifact://a"})),
                (
                    "evidence_correct",
                    json!({"reference":"artifact://a","correction":"  "}),
                ),
            ];
            for (method, params) in refused {
                let error = runtime
                    .handle_request(&json!({"method":method,"params":params}))
                    .unwrap_err();
                assert_eq!(code(&error), ErrorCode::InvalidParams, "{method} {params}");
            }
            let error = runtime
                .handle_request(
                    &json!({"method":"evidence","params":{"action":"check","name":"fmt"}}),
                )
                .unwrap_err();
            assert_eq!(code(&error), ErrorCode::NotFound);
        },
    );
}

#[test]
fn diagnostics_answer_reads_for_a_session_without_recorded_checks() {
    with_runtime(
        |_| {},
        |runtime| {
            runtime.acp = false;
            let checks = runtime
                .handle_request(&json!({"method":"evidence","params":{"action":"checks"}}))
                .unwrap();
            assert_eq!(
                checks,
                json!({"checks_current": false, "recorded_checks": []})
            );
            let policy = runtime
                .handle_request(&json!({"method":"policy","params":{}}))
                .unwrap();
            assert!(policy.get("config").is_some() && policy.get("scope").is_some());
            let reliability = runtime
                .handle_request(&json!({"method":"reliability","params":{}}))
                .unwrap();
            assert!(reliability.get("max_contract_failures").is_some());
        },
    );
}

#[test]
fn actor_settlement_rebinds_side_threads_after_session_switch() {
    with_runtime(
        |_| {},
        |runtime| {
            runtime.acp = false;
            let old = runtime.agent.session().id().to_owned();
            let path =
                crate::storage::side_thread::path_for(runtime.agent.session().path().unwrap());
            let mut threads = crate::storage::side_thread::SideThreads::default();
            threads.create(None);
            crate::storage::side_thread::save(&path, &threads).unwrap();
            runtime.sides.bind(&runtime.agent, None).unwrap();
            assert_eq!(
                runtime.sides.handle(&json!({"action":"list"})).unwrap()["threads"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
            let (tx, rx) = mpsc::channel();
            tx.send(json!({"id":1,"method":"new_session","params":{}}))
                .unwrap();
            drop(tx);
            runtime.rx = rx;
            assert!(matches!(runtime.run(&mut None).unwrap(), Exit::Closed));
            assert_ne!(runtime.agent.session().id(), old);
            assert_eq!(
                runtime.sides.handle(&json!({"action":"list"})).unwrap()["threads"],
                json!([])
            );
        },
    );
}

#[test]
fn idle_recovery_handler_returns_history_and_authoritative_live_root_contract() {
    with_runtime(
        |_| {},
        |runtime| {
            runtime.acp = false;
            let message = json!({"role":"user","content":"saved prompt"});
            runtime.agent.session_mut().append(message.clone()).unwrap();
            let session_id = runtime.agent.session().id().to_owned();
            {
                let mut output = runtime.worker_out.mapping().unwrap();
                output
                    .envelope
                    .stamp(&mut json!({"event":"todo","tasks":[{"content":"stale event task"}]}));
            }
            let snapshot = runtime
                .handle_request(&json!({"id":1,"method":"recovery_snapshot","params":{}}))
                .unwrap();
            assert_eq!(
                snapshot,
                json!({
                    "session_id":session_id,"seq":1,"messages":[message],"busy":false,
                    "pending_approvals":[],"question":null,"todo":[]
                })
            );
        },
    );
}

#[test]
fn persistent_modes_rebuild_and_publish_effective_settings_without_resetting_session_modes() {
    with_runtime(
        |_| {},
        |runtime| {
            use crate::config::settings_store::{self, Change, Scope};
            let model = runtime.layout.user_dir.join("mode-test-model");
            std::fs::create_dir_all(&model).unwrap();
            std::fs::write(model.join("config.json"), "{}").unwrap();
            std::fs::write(model.join("model.safetensors"), "weights").unwrap();
            let change = |path: &str, value| Change::Set {
                path: path.into(),
                value,
            };
            settings_store::save_config(
                runtime.layout,
                Scope::Project,
                &[
                    change(
                        "model",
                        toml::Value::String(model.to_string_lossy().into_owned()),
                    ),
                    change("compute_locks", false.into()),
                    change("memory.backend", "off".into()),
                    change("codebase.enabled", false.into()),
                ],
            )
            .unwrap();
            settings_store::save_config(
                runtime.layout,
                Scope::Global,
                &[
                    change("fast_mode", true.into()),
                    change("slow_mode", true.into()),
                    change("service_tier", "ultrafast".into()),
                ],
            )
            .unwrap();
            let overlay = toml::Table::new();
            let config = settings_store::load_config(runtime.layout, &overlay)
                .unwrap()
                .for_coding();
            let published_config = Arc::new(std::sync::RwLock::new(config.clone()));
            let (requests, rx) = mpsc::channel();
            for (id, params) in [
                json!({"mode":"advisor","state":"completion"}),
                json!({"mode":"external_thinking","enabled":true}),
                json!({"mode":"plan_yolo","enabled":true,"target":"@main"}),
                json!({"mode":"drafter","enabled":true}),
                json!({"mode":"extended_context","enabled":true}),
                json!({"mode":"fast","state":"off"}),
                json!({"mode":"slow","enabled":false}),
            ]
            .into_iter()
            .enumerate()
            {
                requests
                    .send(json!({"id":id,"method":"set_mode","params":params}))
                    .unwrap();
            }
            drop(requests);
            let (_, approval_rx) = mpsc::channel();
            let (_, question_rx) = mpsc::channel();
            let (control_tx, _) = mpsc::sync_channel(16);
            let actor = Actor {
                layout: runtime.layout.clone(),
                config,
                overlay,
                acp: false,
                launch: Default::default(),
                rx,
                approval_rx,
                question_rx,
                control_tx,
                worker_out: runtime.worker_out.clone(),
                flag: runtime.flag.clone(),
                active: runtime.active.clone(),
                worker_closing: runtime.worker_closing.clone(),
                worker_prompt_queued: runtime.worker_prompt_queued.clone(),
                worker_pending: Default::default(),
                worker_host: runtime.worker_host.clone(),
                worker_uris: runtime.worker_uris.clone(),
                worker_questions: Default::default(),
                worker_disconnected: Default::default(),
                worker_query_cache: runtime.worker_query_cache.clone(),
                worker_live: runtime.worker_live.clone(),
                queue: Default::default(),
                worker_fs: Default::default(),
                admin_config: published_config.clone(),
                admin_layout: Arc::new(std::sync::RwLock::new(runtime.layout.clone())),
                sides: runtime.sides.clone(),
            };
            actor.run().unwrap();
            let config = published_config.read().unwrap();
            assert!(config.experimental.turn_optimizer);
            assert!(config.extended_context);
            assert!(!config.fast_mode);
            assert!(!config.slow_mode);
            assert_eq!(config.service_tier, "none");
            let projection = runtime.worker_query_cache.lock().unwrap();
            let state = &projection.state["working_modes"];
            assert_eq!(state["settings"]["turn_optimizer"], true);
            assert_eq!(state["settings"]["extended_context"], true);
            assert_eq!(state["settings"]["fast_mode"], false);
            assert_eq!(state["settings"]["slow_mode"], false);
            assert_eq!(state["settings"]["service_tier"], "none");
            assert_eq!(state["advisor"]["enabled"], true);
            assert_eq!(state["advisor"]["completion_only"], true);
            assert_eq!(state["external_thinking"], true);
            assert!(state["plan_yolo_target"].is_string());
            assert_eq!(state["plan"], true);
            assert!(!runtime.active.load(Ordering::SeqCst));
            let sessions: Vec<_> = Session::list_by_activity(runtime.layout, usize::MAX)
                .unwrap()
                .into_iter()
                .filter(|session| session.id != runtime.agent.session().id())
                .collect();
            assert_eq!(
                sessions.len(),
                1,
                "the real actor adds one session alongside the fixture; rebuilds must not fork it"
            );
            assert_eq!(projection.state["session_id"], sessions[0].id);
        },
    );
}

#[test]
fn persistent_mode_validation_refuses_before_publication_and_acp_remains_native_only() {
    with_runtime(
        |_| {},
        |runtime| {
            let params = json!({"mode":"drafter","enabled":true});
            let error = runtime.mode_change(&params).err().unwrap();
            assert_eq!(
                error.downcast_ref::<RequestError>().unwrap().code,
                ErrorCode::UnknownMethod
            );
            runtime.acp = false;
            let before: Vec<_> = runtime
                .layout
                .config_files()
                .iter()
                .map(std::fs::read)
                .collect();
            let error = runtime.mode_change(&params).err().unwrap();
            assert_eq!(
                error.downcast_ref::<RequestError>().unwrap().code,
                ErrorCode::InvalidParams
            );
            let after: Vec<_> = runtime
                .layout
                .config_files()
                .iter()
                .map(std::fs::read)
                .collect();
            for (before, after) in before.iter().zip(after.iter()) {
                assert_eq!(before.as_ref().ok(), after.as_ref().ok());
                assert_eq!(before.is_err(), after.is_err());
            }
            assert_eq!(
                workflows::modes(&runtime.agent)["settings"]["turn_optimizer"],
                false
            );
        },
    );
}

#[test]
fn internal_goal_prompts_are_literal_but_client_send_still_refuses_builtin_commands() {
    for params in [
        json!({"action":"resume"}),
        json!({"action":"budget","token_budget":20}),
        json!({"action":"set","objective":"/goal resume"}),
    ] {
        with_runtime(
            |_| {},
            |runtime| {
                runtime.acp = false;
                runtime
                    .agent
                    .goal_mutate(|state| {
                        state.start("finish the task", Some(10));
                        if params["action"] == "resume" {
                            state.pause().unwrap();
                        }
                    })
                    .unwrap();
                // Cancellation keeps the real turn off the fixture's unreachable backend.
                runtime.flag.store(true, Ordering::SeqCst);
                let result = runtime.handle_request(&json!({"method":"goal","params":params}));
                assert!(
                    result.is_ok(),
                    "goal continuation must enter the normal cancelled turn: {result:?}"
                );
                assert_eq!(result.unwrap()["interrupted"], true);
                let messages = runtime.agent.session().messages();
                let expected = if params["action"] == "budget" {
                    "/goal budget 20"
                } else {
                    "/goal resume"
                };
                assert!(
                    messages
                        .iter()
                        .any(|message| message["role"] == "user" && message["content"] == expected)
                );
                let error = runtime
                    .handle_request(&json!({"method":"send","params":{"text":"/goal resume"}}))
                    .unwrap_err();
                assert_eq!(
                    error.downcast_ref::<RequestError>().unwrap().code,
                    ErrorCode::InvalidParams
                );
            },
        );
    }
}

#[test]
fn goal_edits_acquire_local_compute_only_when_they_produce_a_prompt() {
    for params in [
        json!({"action":"get"}),
        json!({"action":"pause"}),
        json!({"action":"drop"}),
        json!({"action":"budget","token_budget":20}),
        json!({"action":"resume"}),
    ] {
        with_runtime(
            |config| {
                config.compute_locks = true;
                config.model = "test-local-model".into();
            },
            |runtime| {
                assert!(runtime.config.requires_compute_lock());
                runtime.acp = false;
                runtime
                    .agent
                    .goal_mutate(|state| {
                        state.start("finish the task", Some(10));
                        if matches!(params["action"].as_str(), Some("budget" | "resume")) {
                            state.pause().unwrap();
                        }
                    })
                    .unwrap();
                runtime.flag.store(true, Ordering::SeqCst);
                let result = runtime.handle_request(&json!({"method":"goal","params":params}));
                if params["action"] == "resume" {
                    assert!(
                        result.is_err(),
                        "cancelled compute admission must precede goal mutation"
                    );
                    assert_eq!(
                        runtime.agent.modes().goal.goal.as_ref().unwrap().status,
                        crate::agent::modes::goal::Status::Paused
                    );
                } else {
                    assert!(
                        result.is_ok(),
                        "nonprompt goal edits must not wait for cancelled local compute: {result:?}"
                    );
                }
                assert!(runtime.compute.is_none());
                assert!(runtime.agent.session().messages().is_empty());
            },
        );
    }
}

#[test]
fn recovery_publication_fence_orders_controller_events_and_closes_abandoned_tickets() {
    const CHILD: &str = "DIAL_TEST_RPC_RECOVERY_FENCE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        for case in ["success", "refused", "panic"] {
            with_runtime(
                |_| {},
                |runtime| {
                    runtime.acp = false;
                    runtime.worker_out.negotiate(2, false, true).unwrap();
                    let request = json!({"id":case,"method":"recovery_snapshot","params":{}});
                    let snapshot = runtime.handle_request(&request).unwrap();
                    // Deliberately publish a controller event after capture, before settlement.
                    // Its later ticket must wait behind the recovery response's reserved ticket.
                    assert!(
                        runtime
                            .worker_out
                            .emit(json!({"event":"side_state","publication_case":case}))
                    );
                    if case == "panic" {
                        let reply = runtime.recovery_reply.take().unwrap();
                        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let _reply = reply;
                            panic!("abandon reserved recovery response");
                        }));
                        assert!(panic.is_err());
                        runtime.worker_out.result(
                            &json!(case),
                            Err(coded(ErrorCode::InternalError, "actor panic")),
                        );
                    } else {
                        let result = if case == "refused" {
                            live_snapshot::bounded(snapshot, 1)
                        } else {
                            Ok(snapshot)
                        };
                        runtime.settle_request(&request, &json!(case), result);
                    }
                    runtime
                        .worker_out
                        .writer
                        .drain(Some(Duration::from_secs(5)))
                        .unwrap();
                },
            );
        }
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "rpc::actor::tests::recovery_publication_fence_orders_controller_events_and_closes_abandoned_tickets", "--nocapture", "--format", "terse"])
        .env(CHILD, "1")
        .output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let frames: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| {
            line.find('{')
                .and_then(|start| serde_json::from_str(&line[start..]).ok())
        })
        .collect();
    for case in ["success", "refused", "panic"] {
        let replies: Vec<_> = frames
            .iter()
            .enumerate()
            .filter(|(_, frame)| frame["id"] == case)
            .collect();
        assert_eq!(replies.len(), 1, "one response for {case}: {frames:?}");
        let (reply_position, reply) = replies[0];
        let event_position = frames
            .iter()
            .position(|frame| frame["publication_case"] == case)
            .unwrap();
        match case {
            "success" => {
                assert!(
                    reply_position < event_position,
                    "controller event overtook recovery: {frames:?}"
                );
                assert!(
                    frames[event_position]["seq"].as_u64().unwrap()
                        > reply["result"]["seq"].as_u64().unwrap()
                );
            }
            "refused" => {
                assert!(reply_position < event_position);
                assert_eq!(reply["error"]["code"], "refused");
                assert!(reply.get("result").is_none());
            }
            _ => assert_eq!(reply["error"]["code"], "internal_error"),
        }
    }
}

#[test]
fn retry_replays_literal_goal_and_already_expanded_command_text() {
    for initial in ["goal", "custom"] {
        with_runtime(
            |_| {},
            |runtime| {
                runtime.acp = false;
                runtime.flag.store(true, Ordering::SeqCst);
                let expected = if initial == "goal" {
                    runtime
                        .agent
                        .goal_mutate(|state| {
                            state.start("finish the task", None);
                            state.pause().unwrap();
                        })
                        .unwrap();
                    let result = runtime
                        .handle_request(&json!({"method":"goal","params":{"action":"resume"}}))
                        .unwrap();
                    assert_eq!(result["interrupted"], true);
                    "/goal resume"
                } else {
                    let commands = runtime.layout.project_dir.join("commands");
                    std::fs::create_dir_all(&commands).unwrap();
                    std::fs::write(
                        commands.join("retry-literal.md"),
                        "/compact expanded instruction",
                    )
                    .unwrap();
                    let result = runtime
                        .handle_request(
                            &json!({"method":"send","params":{"text":"/retry-literal"}}),
                        )
                        .unwrap();
                    assert_eq!(result["interrupted"], true);
                    // Replaying history must not rediscover the source command or expand its result.
                    std::fs::write(commands.join("retry-literal.md"), "changed instruction")
                        .unwrap();
                    "/compact expanded instruction"
                };
                let result = runtime.handle_request(&json!({"method":"retry","params":{}}));
                assert!(
                    result.is_ok(),
                    "retry must replay recorded text literally: {result:?}"
                );
                assert_eq!(result.unwrap()["interrupted"], true);
                let messages = runtime.agent.session().messages();
                let users: Vec<_> = messages
                    .iter()
                    .filter(|message| message["role"] == "user")
                    .collect();
                assert_eq!(
                    users.len(),
                    1,
                    "rewind must replace the interrupted attempt"
                );
                assert_eq!(users[0]["content"], expected);
                let error = runtime
                    .handle_request(&json!({"method":"send","params":{"text":expected}}))
                    .unwrap_err();
                assert_eq!(
                    error.downcast_ref::<RequestError>().unwrap().code,
                    ErrorCode::InvalidParams
                );
            },
        );
    }
}

#[test]
fn guest_retry_refuses_before_rewinding_local_history() {
    with_runtime(
        |_| {},
        |runtime| {
            use crate::collab::{guest::Guest, room};
            struct Cleanup;
            impl Drop for Cleanup {
                fn drop(&mut self) {
                    let _ = room::leave();
                }
            }
            let _cleanup = Cleanup;
            runtime.acp = false;
            runtime
                .agent
                .session_mut()
                .append(json!({"role":"user","content":"local interrupted instruction"}))
                .unwrap();
            let before = runtime.agent.session().messages();
            let session = runtime.agent.session().id().to_owned();
            room::with(|slot| {
                assert!(slot.is_none());
                *slot = Some(room::Room::Guest(Guest::fixture(&[])));
            });
            let error = runtime
                .handle_request(&json!({"method":"retry","params":{}}))
                .unwrap_err();
            assert_eq!(
                error.downcast_ref::<RequestError>().unwrap().code,
                ErrorCode::Refused
            );
            assert_eq!(runtime.agent.session().id(), session);
            assert_eq!(runtime.agent.session().messages(), before);
        },
    );
}

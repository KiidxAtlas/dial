use super::*;

#[test]
fn reviewed_and_verified_shell_epochs_are_reused_but_new_operations_invalidate_them() {
    let mut state = StepChecks {
        mutations: 1,
        uncheckpointed_shell_commands: 1,
        ..Default::default()
    };
    state.record_checked();
    assert!(
        !state.clean(),
        "verification cannot substitute for shell diff review"
    );
    state.disclosed_shell_commands = 1;
    assert!(
        state.clean(),
        "reviewed shell history must not force another check pass"
    );
    state.uncheckpointed_shell_commands += 1;
    state.disclosed_shell_commands += 1;
    assert!(!state.clean(), "reviewing a new command does not verify it");
    state.record_checked();
    assert!(state.clean());
    state.uncheckpointed_extension_calls += 1;
    state.disclosed_extension_calls += 1;
    assert!(
        !state.clean(),
        "extension side effects also invalidate verification"
    );
    state.record_checked();
    assert!(state.clean());
    state.mutations += 1;
    assert!(!state.clean());
    state.record_checked();
    state.failed = true;
    assert!(
        !state.clean(),
        "failed checks can never be reused as a clean pass"
    );
}

#[test]
fn harness_notes_merge_into_one_message_without_touching_history_or_marked_messages() {
    let mut messages = vec![
        json!({"role":"user","content":"earlier note","dial_transient":true}),
        json!({"role":"user","content":"first","dial_transient":true}),
        json!({"role":"user","content":"rules","dial_transient":true,"dial_portable_rules":true}),
        json!({"role":"user","content":"second","dial_transient":true}),
        json!({"role":"user","content":[{"type":"text","text":"image"}],"dial_transient":true}),
        json!({"role":"user","content":"third","dial_transient":true}),
    ];
    coalesce_harness_notes(&mut messages, 1);
    assert_eq!(messages.len(), 4);
    assert_eq!(
        messages[0]["content"], "earlier note",
        "history before start is untouched"
    );
    assert_eq!(messages[1]["content"], "first\n\nsecond\n\nthird");
    assert_eq!(messages[2]["dial_portable_rules"], true);
    assert!(messages[3]["content"].is_array());
}

#[test]
fn shared_wait_receives_steering_without_losing_the_queued_instruction() {
    struct Wait;
    impl tools::ext::Tool for Wait {
        fn name(&self) -> &str {
            "wait_probe"
        }
        fn schema(&self) -> Value {
            json!({"type":"function","function":{"name":"wait_probe","description":"fake","parameters":{"type":"object","properties":{}}}})
        }
        fn needs_approval(&self, _: &Value) -> bool {
            false
        }
        fn concurrency(&self, _: &Value) -> tools::ext::Concurrency {
            tools::ext::Concurrency::Shared
        }
        fn interruptible(&self, _: &Value) -> bool {
            true
        }
        fn call(&self, _: &ToolContext, _: &Value) -> ToolOutput {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !tools::signals::wait_interrupted() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(2));
            }
            ToolOutput {
                execution: Default::default(),
                data: None,
                text: "wait ended".into(),
                is_error: !tools::signals::wait_interrupted(),
            }
        }
    }
    struct Steering(bool);
    impl Ui for Steering {
        fn take_steering(&mut self) -> Vec<String> {
            if std::mem::take(&mut self.0) {
                vec!["new instruction".into()]
            } else {
                vec![]
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let mut registry = Registry::new();
    registry.register(Arc::new(Wait));
    let call = ToolCall {
        id: "wait".into(),
        name: "wait_probe".into(),
        arguments: json!({}),
        malformed_arguments: false,
    };
    let mut queued = Vec::new();
    let results = parallel_tools(
        &workspace,
        &Config::default(),
        &registry,
        &[call],
        &AtomicBool::new(false),
        &mut Steering(true),
        &mut queued,
    );
    assert!(!results[0].0.is_error);
    assert_eq!(queued, ["new instruction"]);
}

#[cfg(unix)]
#[test]
fn parallel_hooks_run_per_call_and_block_before_file_reads() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    std::fs::write(dir.path().join("evidence"), "must not be read").unwrap();
    let mut config = Config::default();
    config.hooks.push(crate::config::types::HookConfig {
            event:"pre_tool_use".into(), matcher:None,
            command:vec!["sh".into(),"-c".into(),r#"cat >/dev/null; printf x >> hooks; printf '{"decision":"block","reason":"test gate"}'"#.into()],
            timeout_secs:5, fail_closed:None,
        });
    let calls = ["a", "b"].map(|id| ToolCall {
        id: id.into(),
        name: "read".into(),
        arguments: json!({"path":"evidence"}),
        malformed_arguments: false,
    });
    let outputs = parallel_tools(
        &workspace,
        &config,
        &Registry::new(),
        &calls,
        &AtomicBool::new(false),
        &mut NullUi,
        &mut Vec::new(),
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("hooks")).unwrap(),
        "xx"
    );
    assert!(outputs.iter().all(|(output, _)| output.is_error
        && output.text.contains("test gate")
        && !output.text.contains("must not be read")
        && tools::ToolStatus::from_output(output) == tools::ToolStatus::Blocked));
}

#[test]
fn independent_slow_reads_overlap_and_results_stay_ordered() {
    struct SlowRead {
        entered: std::sync::atomic::AtomicUsize,
    }
    impl tools::ext::Tool for SlowRead {
        fn name(&self) -> &str {
            "harness_probe"
        }
        fn schema(&self) -> Value {
            json!({"type":"function","function":{"name":"harness_probe","description":"fake","parameters":{"type":"object","properties":{}}}})
        }
        fn concurrency(&self, _: &Value) -> tools::ext::Concurrency {
            tools::ext::Concurrency::Shared
        }
        fn needs_approval(&self, _: &Value) -> bool {
            false
        }
        fn call(&self, _: &ToolContext, args: &Value) -> ToolOutput {
            self.entered.fetch_add(1, Ordering::SeqCst);
            let deadline = Instant::now() + Duration::from_secs(2);
            while self.entered.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
            let overlap = self.entered.load(Ordering::SeqCst) >= 2;
            if args["index"] == 0 {
                thread::sleep(Duration::from_millis(30));
            }
            ToolOutput {
                execution: Default::default(),
                data: None,
                text: args["index"].to_string(),
                is_error: !overlap,
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let mut ext = Registry::new();
    let read = Arc::new(SlowRead {
        entered: std::sync::atomic::AtomicUsize::new(0),
    });
    ext.register(read.clone());
    let calls: Vec<_> = [0, 1, 0]
        .into_iter()
        .map(|index| ToolCall {
            id: index.to_string(),
            name: "harness_probe".into(),
            arguments: json!({"index":index}),
            malformed_arguments: false,
        })
        .collect();
    let mut calls = calls;
    calls[2].id = "duplicate".into();
    calls[2].arguments["i"] = json!("same observation, different presentation");
    assert!(approval::parallel_read(
        &ext,
        "harness_probe",
        &calls[0].arguments
    ));
    let outputs = parallel_tools(
        &workspace,
        &Config::default(),
        &ext,
        &calls,
        &AtomicBool::new(false),
        &mut NullUi,
        &mut Vec::new(),
    );
    assert_eq!(
        read.entered.load(Ordering::SeqCst),
        2,
        "only distinct independent inspections should execute"
    );
    assert_eq!(
        outputs[2].1, 0.0,
        "a reused result consumes no second execution time"
    );
    assert!(
        outputs.iter().all(|(out, _)| !out.is_error),
        "reads must overlap instead of timing out waiting for each other"
    );
    assert_eq!(
        outputs
            .iter()
            .map(|(out, _)| out.text.as_str())
            .collect::<Vec<_>>(),
        ["0", "1", "0"]
    );
}

#[test]
fn batched_read_only_bash_runs_through_parallel_tools_in_call_order() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let calls: Vec<_> = ["printf one", "printf two", "printf one"]
        .into_iter()
        .enumerate()
        .map(|(index, command)| ToolCall {
            id: index.to_string(),
            name: "bash".into(),
            arguments: json!({"command": command}),
            malformed_arguments: false,
        })
        .collect();
    assert!(calls.iter().all(|call| approval::parallel_read(
        &Registry::new(),
        &call.name,
        &call.arguments
    )));
    let outputs = parallel_tools(
        &workspace,
        &Config::default(),
        &Registry::new(),
        &calls,
        &AtomicBool::new(false),
        &mut NullUi,
        &mut Vec::new(),
    );
    assert_eq!(
        outputs
            .iter()
            .map(|(out, _)| (
                out.is_error,
                out.text.lines().next().unwrap_or_default().to_owned()
            ))
            .collect::<Vec<_>>(),
        [
            (false, "one".into()),
            (false, "two".into()),
            (false, "one".into())
        ],
        "batched bash results must stay in call order with their own output"
    );
}

#[test]
fn repair_hints_are_project_scoped_relevant_and_omit_old_values() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(
        dir.path(),
        "test-model",
        "test-usecase",
        &Default::default(),
    )
    .unwrap();
    let version = store.active().unwrap().id;
    let trajectory = store
        .begin_trajectory(version, "coding", "repair", "input")
        .unwrap();
    store.tool_recovery(trajectory, version, &json!({
            "project_root":"/workspace/a",
            "context":[{"role":"user","content":"Fix the parser for emoji filenames"}],
            "tools":[],
            "corrected":{"role":"assistant","tool_calls":[{
                "id":"call-1","type":"function",
                "function":{"name":"edit","arguments":"{\"path\":\"private/secret.txt\",\"input\":\"SECRET_VALUE\"}"}
            }]},
            "failure_kinds":["schema"],
            "rejected":[{"role":"assistant","content":"rejected"}]
        })).unwrap();
    let offered = vec!["edit".to_owned()];
    let hints = verified_repair_hints(
        &store,
        "/workspace/a",
        "Please fix the parser for emoji filenames",
        &offered,
    )
    .unwrap();
    assert_eq!(hints.len(), 1);
    assert!(hints[0].contains("edit") && hints[0].contains("path") && hints[0].contains("schema"));
    assert!(
        !hints[0].contains("secret.txt") && !hints[0].contains("SECRET_VALUE"),
        "hints must not replay historical argument values"
    );
    assert!(
        verified_repair_hints(
            &store,
            "/workspace/b",
            "fix the parser for emoji filenames",
            &offered
        )
        .unwrap()
        .is_empty()
    );
    assert!(
        verified_repair_hints(&store, "/workspace/a", "weather forecast paris", &offered)
            .unwrap()
            .is_empty()
    );
    assert!(
        verified_repair_hints(
            &store,
            "/workspace/a",
            "fix the parser for emoji filenames",
            &["read".into()]
        )
        .unwrap()
        .is_empty()
    );
}

#[test]
fn slow_streams_keep_the_ui_alive_before_the_first_token() {
    struct RecordingUi {
        heartbeats: usize,
        text: String,
    }
    impl Ui for RecordingUi {
        fn heartbeat(&mut self) {
            self.heartbeats += 1;
        }
        fn text_delta(&mut self, text: &str) {
            self.text.push_str(text);
        }
    }
    let delta = json!({"choices":[{"delta":{"content":"hello"}}]}).to_string();
    let finish = json!({"choices":[{"delta":{},"finish_reason":"stop"}]}).to_string();
    let server = crate::llm::fake::serve(vec![crate::llm::fake::Script::SseSlow {
        pieces: vec![
            ": waiting for first token\n\n".to_owned(),
            format!("data: {delta}\n\n"),
            format!("data: {finish}\n\ndata: [DONE]\n\n"),
        ],
        pause: Duration::from_millis(180),
    }]);
    let mut backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: format!("{}/v1", server.url),
            model: "test-model".to_owned(),
            api_key: None,
        },
        Path::new("/nonexistent/python").to_owned(),
        5,
    );
    let request = Chat {
        messages: vec![json!({"role":"user","content":"hello"})],
        tools: vec![],
        temperature: 0.2,
        max_tokens: 16,
        seed: None,
    };
    let mut ui = RecordingUi {
        heartbeats: 0,
        text: String::new(),
    };
    let reply = chat_with_ui(
        &mut backend,
        None,
        &request,
        ChatOptions {
            timeout: 5,
            render_text: true,
            context_tokens: None,
            policy: None,
            recovery: None,
        },
        Arc::new(AtomicBool::new(false)),
        &mut ui,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(reply.text, "hello");
    assert_eq!(ui.text, "hello");
    assert!(
        ui.heartbeats >= 1,
        "the UI should repaint during the fake model's initial delay"
    );
    assert_eq!(server.requests().len(), 1);
}

#[cfg(unix)]
#[test]
fn local_startup_is_visible_and_does_not_trigger_the_reasoning_gate() {
    #[derive(Default)]
    struct StartupUi {
        phases: Vec<llm::InferencePhase>,
        heartbeats: usize,
    }
    impl Ui for StartupUi {
        fn model_progress(&mut self, progress: &llm::ModelProgress) {
            self.phases.push(progress.phase);
        }
        fn heartbeat(&mut self) {
            self.heartbeats += 1;
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let python = llm::server::tests::fake_python(dir.path());
    let script = std::fs::read_to_string(&python).unwrap();
    let (shebang, rest) = script.split_once('\n').unwrap();
    std::fs::write(
        &python,
        format!("{shebang}\nimport time; time.sleep(1.3)\n{rest}"),
    )
    .unwrap();
    crate::platform::fs::set_mode(&python, 0o755).unwrap();
    let mut backend = Backend::new(
        ProviderKind::Local {
            model_dir: dir
                .path()
                .join("model-info.json")
                .to_string_lossy()
                .into_owned(),
        },
        python,
        10,
    );
    let request = Chat {
        messages: vec![json!({"role":"user","content":"hi"})],
        tools: vec![],
        temperature: 0.2,
        max_tokens: 16,
        seed: None,
    };
    let mut ui = StartupUi::default();
    let policy = crate::config::types::ReasoningGuardConfig {
        max_pre_action_secs: 1,
        ..Default::default()
    };
    let reply = chat_with_ui(
        &mut backend,
        None,
        &request,
        ChatOptions {
            timeout: 5,
            render_text: true,
            context_tokens: None,
            policy: Some(&policy),
            recovery: None,
        },
        Arc::new(AtomicBool::new(false)),
        &mut ui,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(reply.text, "pong");
    assert!(ui.phases.contains(&llm::InferencePhase::Loading));
    assert!(ui.phases.contains(&llm::InferencePhase::Processing));
    assert!(ui.phases.contains(&llm::InferencePhase::Generating));
    assert!(
        ui.heartbeats >= 5,
        "input must remain responsive during startup"
    );
}

#[test]
fn reconnect_resets_context_and_reasoning_budget_and_keeps_ui_responsive() {
    #[derive(Default)]
    struct RecordingUi {
        current: u64,
        reasoning_context: Vec<u64>,
        heartbeats: usize,
        notices: Vec<String>,
    }
    impl Ui for RecordingUi {
        fn context_usage(&mut self, tokens: u64, _: bool) {
            self.current = tokens;
        }
        fn reasoning_delta(&mut self, _: &str) {
            self.reasoning_context.push(self.current);
        }
        fn notice(&mut self, text: &str) {
            self.notices.push(text.to_owned());
        }
        fn heartbeat(&mut self) {
            self.heartbeats += 1;
        }
    }
    let reasoning = "r".repeat(96);
    let partial = format!(
        "data: {}\n\n",
        json!({"choices":[{"delta":{"reasoning_content":reasoning}}]})
    );
    let complete = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{"reasoning_content":reasoning,"content":"ok"},"finish_reason":"stop"}]})
    );
    let server = crate::llm::fake::serve(vec![
        crate::llm::fake::Script::SseDisconnect(partial),
        crate::llm::fake::Script::Sse(complete),
    ]);
    let mut backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: format!("{}/v1", server.url),
            model: "test".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    );
    let request = Chat {
        messages: vec![json!({"role":"user","content":"hi"})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 128,
        seed: None,
    };
    let policy = crate::config::types::ReasoningGuardConfig {
        max_reasoning_bytes: 128,
        max_pre_action_secs: 1,
        ..Default::default()
    };
    let mut ui = RecordingUi::default();
    let reply = chat_with_ui(
        &mut backend,
        None,
        &request,
        ChatOptions {
            timeout: 5,
            render_text: true,
            context_tokens: Some(1000),
            policy: Some(&policy),
            recovery: None,
        },
        Arc::new(AtomicBool::new(false)),
        &mut ui,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(reply.text, "ok");
    assert_eq!(reply.reasoning, reasoning);
    assert_eq!(
        ui.reasoning_context,
        [1024, 1024],
        "failed-attempt bytes must not inflate the next request"
    );
    assert!(ui.heartbeats > 0, "backoff must pump input and rendering");
    assert!(
        ui.notices
            .iter()
            .any(|n| n.contains("discarding the partial reply"))
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        crate::llm::fake::body(&requests[0]),
        crate::llm::fake::body(&requests[1])
    );
}

#[test]
fn context_updates_before_stream_callbacks_and_reconciles_reported_usage() {
    #[derive(Default)]
    struct RecordingUi {
        current: (u64, bool),
        observed: Vec<(u64, bool)>,
    }
    impl Ui for RecordingUi {
        fn context_usage(&mut self, tokens: u64, estimated: bool) {
            self.current = (tokens, estimated);
        }
        fn text_delta(&mut self, _: &str) {
            self.observed.push(self.current);
        }
        fn reasoning_delta(&mut self, _: &str) {
            self.observed.push(self.current);
        }
        fn tool_arguments_delta(&mut self, _: u64, _: &str, _: &str, _: &str) {
            self.observed.push(self.current);
        }
    }
    let content = "x".repeat(80);
    let arguments = json!({"path":content}).to_string();
    let chunks = [
        json!({"choices":[{"delta":{"reasoning_content":content}}]}),
        json!({"choices":[{"delta":{"content":content}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"r","type":"function","function":{"name":"read","arguments":arguments}}]}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":512,"completion_tokens":7,"total_tokens":519}}),
    ];
    let mut pieces: Vec<_> = chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect();
    pieces.push("data: [DONE]\n\n".into());
    let server = crate::llm::fake::serve(vec![crate::llm::fake::Script::SseSlow {
        pieces,
        pause: Duration::from_millis(90),
    }]);
    let mut backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: format!("{}/v1", server.url),
            model: "test-model".into(),
            api_key: None,
        },
        Path::new("/nonexistent/python").to_owned(),
        5,
    );
    let request = Chat {
        messages: vec![json!({"role":"user","content":"inspect"})],
        tools: vec![],
        temperature: 0.2,
        max_tokens: 128,
        seed: None,
    };
    let mut ui = RecordingUi::default();
    chat_with_ui(
        &mut backend,
        None,
        &request,
        ChatOptions {
            timeout: 5,
            render_text: true,
            context_tokens: Some(1000),
            policy: None,
            recovery: None,
        },
        Arc::new(AtomicBool::new(false)),
        &mut ui,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(ui.observed.len(), 3);
    assert_eq!(ui.observed[0], (1020, true));
    assert_eq!(ui.observed[1], (1040, true));
    assert!(ui.observed[2].0 > 1040 && ui.observed[2].1);
    assert_eq!(ui.current, (519, false));
}

#[test]
fn background_waits_keep_the_ui_pumping() {
    struct CountingUi(usize);
    impl Ui for CountingUi {
        fn heartbeat(&mut self) {
            self.0 += 1;
        }
    }
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        let _ = tx.send(());
    });
    let mut ui = CountingUi(0);
    receive_with_heartbeat(&rx, &mut ui).unwrap();
    assert!(
        ui.0 >= 1,
        "the UI must repaint while the operation is pending"
    );
}

#[test]
fn local_progress_reaches_ui_before_text_and_resets_between_requests() {
    #[derive(Default)]
    struct RecordingUi {
        progress: Vec<llm::ModelProgress>,
        text: String,
        before_text: bool,
        heartbeats: usize,
    }
    impl Ui for RecordingUi {
        fn model_progress(&mut self, p: &llm::ModelProgress) {
            if p.output_tokens == Some(12) && self.text.is_empty() {
                self.before_text = true;
            }
            self.progress.push(p.clone());
        }
        fn text_delta(&mut self, text: &str) {
            self.text.push_str(text);
        }
        fn heartbeat(&mut self) {
            self.heartbeats += 1;
        }
    }
    let events = [
        json!({"dial_progress":{"phase":"processing","processed_tokens":32,"prompt_tokens":64}}),
        json!({"dial_progress":{"phase":"generating","output_tokens":12,"tokens_per_second":30}}),
        json!({"choices":[{"delta":{"content":"ok"}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":64,"completion_tokens":14}}),
    ];
    let pieces = events
        .iter()
        .map(|e| format!("data: {e}\n\n"))
        .chain(["data: [DONE]\n\n".into()])
        .collect::<Vec<_>>();
    let server = crate::llm::fake::serve(vec![
            crate::llm::fake::Script::SseSlow { pieces, pause: Duration::from_millis(110) },
            crate::llm::fake::Script::Sse("data: {\"choices\":[{\"delta\":{\"content\":\"next\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".into()),
        ]);
    let mut backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: format!("{}/v1", server.url),
            model: "test".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    );
    let request = Chat {
        messages: vec![json!({"role":"user","content":"hello"})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 32,
        seed: None,
    };
    let mut ui = RecordingUi::default();
    let reply = chat_with_ui(
        &mut backend,
        None,
        &request,
        ChatOptions {
            timeout: 5,
            render_text: true,
            context_tokens: None,
            policy: None,
            recovery: None,
        },
        Arc::new(AtomicBool::new(false)),
        &mut ui,
        &mut |_| {},
    )
    .unwrap();
    assert!(ui.before_text);
    assert!(ui.heartbeats > 0);
    assert_eq!(ui.text, "ok");
    assert_eq!(reply.completion_tokens, Some(14));
    assert!(ui.progress.iter().any(|p| p.processed_tokens == Some(32)));
    assert_eq!(ui.progress.last().unwrap().output_tokens, Some(14));
    let mut ui = RecordingUi::default();
    chat_with_ui(
        &mut backend,
        None,
        &request,
        ChatOptions {
            timeout: 5,
            render_text: false,
            context_tokens: None,
            policy: None,
            recovery: None,
        },
        Arc::new(AtomicBool::new(false)),
        &mut ui,
        &mut |_| {},
    )
    .unwrap();
    assert!(ui.text.is_empty());
    assert_eq!(ui.progress[0].output_tokens, None);
    assert_eq!(ui.progress.last().unwrap().output_tokens, Some(1));
    assert!(ui.progress.last().unwrap().estimated);
}

#[test]
fn lmstudio_standard_sse_shows_processing_and_estimated_live_rate() {
    #[derive(Default)]
    struct RecordingUi(Vec<llm::ModelProgress>);
    impl Ui for RecordingUi {
        fn model_progress(&mut self, p: &llm::ModelProgress) {
            self.0.push(p.clone());
        }
    }
    // LM Studio's /v1 stream need not send Dial-specific progress events.
    let pieces = ["hello world", " more output", " and more"]
            .iter()
            .map(|text| format!("data: {}\n\n", json!({"choices":[{"delta":{"content":text}}]})))
            .chain(["data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":64,\"completion_tokens\":8}}\n\ndata: [DONE]\n\n".into()])
            .collect();
    let server = crate::llm::fake::serve(vec![crate::llm::fake::Script::SseSlow {
        pieces,
        pause: Duration::from_millis(110),
    }]);
    let profile = llm::profiles::ModelProfile {
        provider: "lm-studio".into(),
        model: "qwen".into(),
        ..Default::default()
    };
    let mut backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: format!("{}/v1", server.url),
            model: "qwen".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    )
    .with_profile(profile);
    let request = Chat {
        messages: vec![json!({"role":"user","content":"hello"})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 32,
        seed: None,
    };
    let mut ui = RecordingUi::default();
    let reply = chat_with_ui(
        &mut backend,
        None,
        &request,
        ChatOptions {
            timeout: 5,
            render_text: true,
            context_tokens: None,
            policy: None,
            recovery: None,
        },
        Arc::new(AtomicBool::new(false)),
        &mut ui,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(ui.0[0].phase, llm::InferencePhase::Processing);
    assert!(
        ui.0.iter()
            .any(|p| p.phase == llm::InferencePhase::Generating
                && p.estimated
                && p.tokens_per_second.is_some_and(|r| r > 0.0))
    );
    assert_eq!(reply.completion_tokens, Some(8));
    assert_eq!(ui.0.last().unwrap().output_tokens, Some(8));
    assert!(!ui.0.last().unwrap().estimated);
}

#[test]
fn internal_model_output_stays_out_of_the_transcript() {
    #[derive(Default)]
    struct RecordingUi {
        text: usize,
        reasoning: usize,
        arguments: usize,
    }
    impl Ui for RecordingUi {
        fn text_delta(&mut self, _: &str) {
            self.text += 1;
        }
        fn reasoning_delta(&mut self, _: &str) {
            self.reasoning += 1;
        }
        fn tool_arguments_delta(&mut self, _: u64, _: &str, _: &str, _: &str) {
            self.arguments += 1;
        }
    }
    let events = [
        json!({"choices":[{"delta":{"reasoning_content":"private maintenance reasoning"}}]}),
        json!({"choices":[{"delta":{"content":"saved result"}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call","type":"function","function":{"name":"unexpected","arguments":"{}"}}]}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
    ];
    let body = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n";
    let server = crate::llm::fake::serve(vec![crate::llm::fake::Script::Sse(body)]);
    let mut backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: format!("{}/v1", server.url),
            model: "test".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    );
    let request = Chat {
        messages: vec![json!({"role":"user","content":"maintain memory"})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 32,
        seed: None,
    };
    let mut ui = RecordingUi::default();
    let reply = chat_with_ui(
        &mut backend,
        None,
        &request,
        ChatOptions {
            timeout: 5,
            render_text: false,
            context_tokens: None,
            policy: None,
            recovery: None,
        },
        Arc::new(AtomicBool::new(false)),
        &mut ui,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(reply.text, "saved result");
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!((ui.text, ui.reasoning, ui.arguments), (0, 0, 0));
}

fn sse_reply(text: &str) -> crate::llm::fake::Script {
    let event = json!({"choices":[{"delta":{"content":text},"finish_reason":"stop"}]});
    crate::llm::fake::Script::Sse(format!("data: {event}\n\ndata: [DONE]\n\n"))
}

fn sse_read_call() -> crate::llm::fake::Script {
    let event = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_read","type":"function","function":{"name":"read","arguments":"{\"path\":\"notes.txt\"}"}}]},"finish_reason":"tool_calls"}]});
    crate::llm::fake::Script::Sse(format!("data: {event}\n\ndata: [DONE]\n\n"))
}

/// A tool call with its own id, so repeated calls keep distinct tool_call_id pairings.
fn sse_tool_call(id: &str, name: &str, arguments: &str) -> crate::llm::fake::Script {
    let event = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":arguments}}]},"finish_reason":"tool_calls"}]});
    crate::llm::fake::Script::Sse(format!("data: {event}\n\ndata: [DONE]\n\n"))
}

fn sent_messages(request: &[u8]) -> String {
    crate::llm::fake::body(request)["messages"].to_string()
}

/// Runs one hermetic turn against scripted replies and returns the text, outcome and raw requests.
fn scripted_turn(
    config: &Config,
    scripts: Vec<crate::llm::fake::Script>,
    prompt: &str,
) -> (String, TurnOutcome, Vec<Vec<u8>>) {
    scripted_turn_with(config, scripts, prompt, |_| {})
}

/// As [`scripted_turn`], with `configure` applied to the agent before its turn runs.
fn scripted_turn_with(
    config: &Config,
    scripts: Vec<crate::llm::fake::Script>,
    prompt: &str,
    configure: impl FnOnce(&mut Agent),
) -> (String, TurnOutcome, Vec<Vec<u8>>) {
    let workspace = tempfile::tempdir().unwrap();
    scripted_turn_at(config, scripts, prompt, workspace.path(), configure)
}

/// As [`scripted_turn_with`], in a caller-owned workspace whose files remain inspectable.
fn scripted_turn_at(
    config: &Config,
    scripts: Vec<crate::llm::fake::Script>,
    prompt: &str,
    root: &std::path::Path,
    configure: impl FnOnce(&mut Agent),
) -> (String, TurnOutcome, Vec<Vec<u8>>) {
    let server = crate::llm::fake::serve(scripts);
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: format!("{}/v1", server.url),
            model: "test".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    );
    std::fs::write(root.join("notes.txt"), "alpha\n").unwrap();
    let version = Version {
        id: 0,
        parent: None,
        kind: "config".into(),
        harness: config.harness.clone(),
        adapter: None,
        status: "active".into(),
    };
    let mut agent = Agent::hermetic(config, backend, &version, root, None, None).unwrap();
    configure(&mut agent);
    let report = agent.turn(prompt, &mut NullUi).unwrap();
    let text = report.text.clone();
    let outcome = report.outcome;
    drop(agent);
    (text, outcome, server.requests())
}

#[test]
fn an_empty_stop_is_retried_with_a_nudge_instead_of_ending_the_turn() {
    let config = Config::default();
    let (text, outcome, requests) = scripted_turn(
        &config,
        vec![
            crate::llm::fake::Script::Sse("data: [DONE]\n\n".into()),
            sse_reply("done"),
        ],
        "hi",
    );
    assert_eq!((text.as_str(), outcome), ("done", TurnOutcome::Completed));
    assert_eq!(requests.len(), 2);
    assert!(sent_messages(&requests[1]).contains("Stopped without actionable output"));
}

#[test]
fn an_announced_action_without_a_tool_call_is_retried() {
    let config = Config::default();
    let (text, outcome, requests) = scripted_turn(
        &config,
        vec![sse_reply("Let me read the file."), sse_reply("Done.")],
        "hi",
    );
    assert_eq!((text.as_str(), outcome), ("Done.", TurnOutcome::Completed));
    assert_eq!(requests.len(), 2);
    assert!(sent_messages(&requests[1]).contains("You said you would continue"));
}

#[test]
fn repeated_successful_tools_escalate_and_the_turn_completes() {
    // The model repeats one successful read eight times, then answers. Nothing stops the turn: stronger
    // guidance appears, and identical calls past the threshold are answered with their earlier result.
    let config = Config::default();
    let mut scripts: Vec<crate::llm::fake::Script> = (0..8)
        .map(|n| sse_tool_call(&format!("call_read_{n}"), "read", r#"{"path":"notes.txt"}"#))
        .collect();
    scripts.push(sse_reply("done"));
    let (text, outcome, requests) = scripted_turn(&config, scripts, "hi");
    assert_eq!((text.as_str(), outcome), ("done", TurnOutcome::Completed));
    assert_eq!(requests.len(), 9);
    let last = sent_messages(&requests[8]);
    assert!(last.contains("tool_call_loop_persisted"), "escalation guidance was not shown");
    assert!(last.contains("[Not executed:"), "identical repeats were not short-circuited");
}

#[test]
fn repeated_failures_escalate_and_never_stop_the_turn() {
    // Twelve identical failing reads, then an answer: failures warn, escalate and are withheld, but
    // the turn itself completes.
    let config = Config::default();
    let mut scripts: Vec<crate::llm::fake::Script> = (0..12)
        .map(|n| sse_tool_call(&format!("call_missing_{n}"), "read", r#"{"path":"missing.txt"}"#))
        .collect();
    scripts.push(sse_reply("done"));
    let (text, outcome, requests) = scripted_turn(&config, scripts, "hi");
    assert_eq!((text.as_str(), outcome), ("done", TurnOutcome::Completed));
    assert_eq!(requests.len(), 13);
    let body = crate::llm::fake::body(&requests[12]);
    let messages = body["messages"].as_array().unwrap();
    let results: Vec<_> = messages
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect();
    assert_eq!(
        results.len(),
        12,
        "every attempted call must receive a result"
    );
    assert!(results.iter().any(|message| message["is_error"] == true));
    // Recovery withholds unchanged calls before ten executions; assert escalation, not old prose.
    assert!(messages.iter().any(|message| message["role"] == "user"
        && message["content"]
            .as_str()
            .is_some_and(|text| text.contains("tool_call_loop_persisted"))));
}

#[test]
fn a_stalled_stream_after_partial_text_continues_the_turn() {
    let mut config = Config::default();
    config.request_recovery.silence_secs = 1;
    let partial = json!({"choices":[{"delta":{"content":"partial answer"}}]});
    let (text, outcome, requests) = scripted_turn(
        &config,
        vec![
            crate::llm::fake::Script::StallAfter {
                first: format!("data: {partial}\n\n"),
                silence: Duration::from_millis(1500),
            },
            sse_reply("resumed"),
        ],
        "hi",
    );
    assert_eq!(
        (text.as_str(), outcome),
        ("resumed", TurnOutcome::Completed)
    );
    assert_eq!(requests.len(), 2);
    assert!(sent_messages(&requests[1]).contains("partial answer"));
    assert!(sent_messages(&requests[1]).contains("Continue exactly where it stopped"));
}

#[test]
fn local_slow_first_event_is_not_cancelled_by_the_generation_watchdog() {
    // A local engine may load a model for longer than the silence limit before its first event.
    let mut config = Config::default();
    config.request_recovery.silence_secs = 1;
    let event = json!({"choices":[{"delta":{"content":"loaded"},"finish_reason":"stop"}]});
    let (text, outcome, requests) = scripted_turn(
        &config,
        vec![crate::llm::fake::Script::ColdStart {
            delay: Duration::from_millis(1500),
            body: format!("data: {event}\n\ndata: [DONE]\n\n"),
        }],
        "hi",
    );
    assert_eq!((text.as_str(), outcome), ("loaded", TurnOutcome::Completed));
    assert_eq!(
        requests.len(),
        1,
        "a slow local start was cancelled and retried"
    );
}

#[test]
fn local_prefill_silence_is_not_cancelled_by_the_generation_watchdog() {
    // Processing (prompt prefill) reports progress, then stays silent past the limit before output.
    let mut config = Config::default();
    config.request_recovery.silence_secs = 1;
    let progress =
        json!({"dial_progress":{"phase":"processing","prompt_tokens":4096,"processed_tokens":512}});
    let event = json!({"choices":[{"delta":{"content":"prefilled"},"finish_reason":"stop"}]});
    let (text, outcome, requests) = scripted_turn(
        &config,
        vec![crate::llm::fake::Script::SseSlow {
            pieces: vec![
                format!("data: {progress}\n\n"),
                format!("data: {event}\n\ndata: [DONE]\n\n"),
            ],
            pause: Duration::from_millis(1500),
        }],
        "hi",
    );
    assert_eq!(
        (text.as_str(), outcome),
        ("prefilled", TurnOutcome::Completed)
    );
    assert_eq!(
        requests.len(),
        1,
        "local prefill silence was cancelled and retried"
    );
}

#[test]
fn local_generation_progress_stall_is_still_recovered() {
    // Once the engine reports Generating, a silent stream is cancelled and the request resumes.
    let mut config = Config::default();
    config.request_recovery.silence_secs = 1;
    let progress = json!({"dial_progress":{"phase":"generating","output_tokens":3}});
    let (text, outcome, requests) = scripted_turn(
        &config,
        vec![
            crate::llm::fake::Script::StallAfter {
                first: format!("data: {progress}\n\n"),
                silence: Duration::from_millis(1500),
            },
            sse_reply("resumed"),
        ],
        "hi",
    );
    assert_eq!(
        (text.as_str(), outcome),
        ("resumed", TurnOutcome::Completed)
    );
    assert_eq!(
        requests.len(),
        2,
        "a generating local stall was not recovered"
    );
}

#[test]
fn silence_watch_is_armed_for_remote_streams_and_only_generating_local_ones() {
    assert!(silence_watch_armed(false, false));
    assert!(silence_watch_armed(false, true));
    assert!(!silence_watch_armed(true, false));
    assert!(silence_watch_armed(true, true));
}

#[test]
fn plan_model_serves_plan_turns_and_the_default_model_resumes_after() {
    let server = crate::llm::fake::serve(vec![sse_reply("planned"), sse_reply("executed")]);
    let base_url = format!("{}/v1", server.url);
    let mut config = Config {
        model: "default-model".into(),
        ..Config::default()
    };
    config.endpoint = Some(crate::config::types::EndpointConfig {
        headers: None,
        base_url: base_url.clone(),
        api_key_env: None,
        allow_insecure: false,
        protocol: None,
        account_id: None,
    });
    config.model_roles.insert(
        "planner".into(),
        crate::llm::model_config::ModelSelection {
            provider: "endpoint".into(),
            model: "plan-model".into(),
            base_url: Some(base_url.clone()),
            ..Default::default()
        },
    );
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url,
            model: "default-model".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    );
    let workspace = tempfile::tempdir().unwrap();
    let version = Version {
        id: 0,
        parent: None,
        kind: "config".into(),
        harness: config.harness.clone(),
        adapter: None,
        status: "active".into(),
    };
    let mut agent =
        Agent::hermetic(&config, backend, &version, workspace.path(), None, None).unwrap();
    agent.plan_mode = true;
    agent.turn("plan it", &mut NullUi).unwrap();
    agent.plan_mode = false;
    agent.turn("do it", &mut NullUi).unwrap();
    drop(agent);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(crate::llm::fake::body(&requests[0])["model"], "plan-model");
    assert_eq!(
        crate::llm::fake::body(&requests[1])["model"],
        "default-model"
    );
}

#[test]
fn a_stalled_partial_drops_external_thinking_before_it_is_kept() {
    let mut config = Config::default();
    config.request_recovery.silence_secs = 1;
    let partial = json!({"choices":[{"delta":{"content":"<scratchpad>zebraSecret</scratchpad>visible answer"}}]});
    let (text, outcome, requests) = scripted_turn_with(
        &config,
        vec![
            crate::llm::fake::Script::StallAfter {
                first: format!("data: {partial}\n\n"),
                silence: Duration::from_millis(1500),
            },
            sse_reply("resumed"),
        ],
        "hi",
        |agent| agent.modes.launch.external_thinking = true,
    );
    assert_eq!(
        (text.as_str(), outcome),
        ("resumed", TurnOutcome::Completed)
    );
    assert_eq!(requests.len(), 2);
    let messages = sent_messages(&requests[1]);
    assert!(messages.contains("visible answer"));
    assert!(!messages.contains("zebraSecret"));
}

#[test]
fn gemini_header_runaway_is_interrupted_with_the_reasoning_reminder() {
    struct Quiet;
    impl Ui for Quiet {}
    let runaway: String = (0..36)
        .map(|i| {
            format!(
                "data: {}\n\n",
                json!({"choices":[{"delta":{"reasoning_content":format!("## Plan step {i}\n")}}]})
            )
        })
        .collect();
    let answer = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]})
    );
    let server = crate::llm::fake::serve(vec![
        crate::llm::fake::Script::Sse(runaway),
        crate::llm::fake::Script::Sse(answer),
    ]);
    let mut backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: format!("{}/v1", server.url),
            model: "gemini-2.5-pro".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    );
    let request = Chat {
        messages: vec![json!({"role":"user","content":"hi"})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    };
    let options = ChatOptions {
        timeout: 5,
        render_text: true,
        context_tokens: None,
        policy: None,
        recovery: None,
    };
    let reply = chat_with_ui(
        &mut backend,
        None,
        &request,
        options,
        Arc::new(AtomicBool::new(false)),
        &mut Quiet,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(reply.text, "done");
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(!sent_messages(&requests[0]).contains("reasoning_without_tool_calls"));
    assert!(sent_messages(&requests[1]).contains("reasoning_without_tool_calls"));
}

#[test]
fn a_detected_loop_resamples_with_a_redirect_instead_of_ending_the_turn() {
    struct Quiet;
    impl Ui for Quiet {}
    let looping = format!(
        "data: {}\n\n",
        json!({"choices":[{"delta":{"reasoning_content":"I will re-check the plan now. ".repeat(12)}}]})
    );
    let answer = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]})
    );
    let server = crate::llm::fake::serve(vec![
        crate::llm::fake::Script::Sse(looping.clone() + &looping),
        crate::llm::fake::Script::Sse(answer),
    ]);
    let mut backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: format!("{}/v1", server.url),
            model: "test".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    );
    let request = Chat {
        messages: vec![json!({"role":"user","content":"hi"})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    };
    let options = ChatOptions {
        timeout: 5,
        render_text: true,
        context_tokens: None,
        policy: None,
        recovery: None,
    };
    let reply = chat_with_ui(
        &mut backend,
        None,
        &request,
        options,
        Arc::new(AtomicBool::new(false)),
        &mut Quiet,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(reply.text, "done");
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let retried = crate::llm::fake::body(&requests[1])["messages"].to_string();
    assert!(
        retried.contains("thinking_loop_detected"),
        "the re-sample carries the redirect"
    );
    assert!(
        !crate::llm::fake::body(&requests[0])["messages"]
            .to_string()
            .contains("thinking_loop_detected")
    );
}

#[test]
fn continuous_model_events_do_not_starve_ui_heartbeats() {
    struct RecordingUi {
        heartbeats: usize,
        events: usize,
    }
    impl Ui for RecordingUi {
        fn heartbeat(&mut self) {
            self.heartbeats += 1;
        }
        fn reasoning_delta(&mut self, _: &str) {
            self.events += 1;
            // Simulate rendering while the worker queues the rest of a fast response.
            thread::sleep(Duration::from_millis(3));
        }
    }
    // Distinct deltas: a verbatim repeated delta is a genuine loop the guard must stop.
    let body = (0..100)
            .map(|i| format!("data: {}\n\n", json!({"choices":[{"delta":{"reasoning_content":format!("step {i} considered; ")}}]})))
            .collect::<String>()
            + &format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"delta":{},"finish_reason":"stop"}]})
            );
    let server = crate::llm::fake::serve(vec![crate::llm::fake::Script::Sse(body)]);
    let mut backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: format!("{}/v1", server.url),
            model: "test".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    );
    let request = Chat {
        messages: vec![json!({"role":"user","content":"hello"})],
        tools: vec![],
        temperature: 0.2,
        max_tokens: 16,
        seed: None,
    };
    let mut ui = RecordingUi {
        heartbeats: 0,
        events: 0,
    };
    chat_with_ui(
        &mut backend,
        None,
        &request,
        ChatOptions {
            timeout: 5,
            render_text: true,
            context_tokens: None,
            policy: None,
            recovery: None,
        },
        Arc::new(AtomicBool::new(false)),
        &mut ui,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(ui.events, 100);
    assert!(
        ui.heartbeats >= 2,
        "a continuously occupied event queue must still pump input"
    );
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn only_real_check_commands_count_as_verification() {
    for yes in [
        "cargo test --lib",
        "python3 -m pytest -q",
        "npm test",
        "go test ./...",
        "python -m unittest",
    ] {
        assert!(is_check_command(yes), "{yes}");
    }
    for no in [
        "ls -la",
        "cat notes.txt",
        "echo hello",
        "git status",
        "python3 script.py",
    ] {
        assert!(!is_check_command(no), "{no}");
    }
}

fn sse_tool_calls(calls: &[(&str, serde_json::Value)]) -> crate::llm::fake::Script {
    let calls: Vec<_> = calls
        .iter()
        .enumerate()
        .map(|(index, (name, arguments))| {
            json!({"index":index,"id":format!("call_{index}"),"type":"function",
                "function":{"name":name,"arguments":arguments.to_string()}})
        })
        .collect();
    let event = json!({"choices":[{"delta":{"tool_calls":calls},"finish_reason":"tool_calls"}]});
    crate::llm::fake::Script::Sse(format!("data: {event}\n\ndata: [DONE]\n\n"))
}

fn replace_edits() -> Config {
    let mut config = Config::default();
    config.edit_mode_override = Some(crate::config::types::EditMode::Replace);
    config
}

fn replace(path: &str, old: &str, new: &str) -> (&'static str, serde_json::Value) {
    (
        "edit",
        json!({"path": path, "old_string": old, "new_string": new}),
    )
}

#[test]
fn edits_that_cycle_file_content_are_steered_and_the_turn_continues_to_completion() {
    let workspace = tempfile::tempdir().unwrap();
    // Committed versions: beta, alpha, beta, alpha, beta (third visit: the loop is reported).
    let mut batch: Vec<_> = ["alpha", "beta", "alpha", "beta", "alpha", "beta"]
        .windows(2)
        .map(|pair| replace("notes.txt", pair[0], pair[1]))
        .collect();
    // Repeating a loop edit is refused; a different edit still runs in the same batch.
    batch.push(replace("notes.txt", "beta", "alpha"));
    batch.push(replace("notes.txt", "beta", "gamma"));
    let (text, outcome, requests) = scripted_turn_at(
        &replace_edits(),
        vec![
            sse_tool_calls(&[("read", json!({"path": "notes.txt"}))]),
            sse_tool_calls(&batch),
            sse_reply("Done."),
        ],
        "toggle the note",
        workspace.path(),
        |_| {},
    );
    assert_eq!(outcome, TurnOutcome::Completed, "{text}");
    assert_eq!(requests.len(), 3, "the turn continued after the loop");
    let followup = String::from_utf8_lossy(&requests[2]);
    assert!(followup.contains("[Dial: edit loop]"), "{followup}");
    assert!(
        followup.contains("This call was not executed: it is one of the edits that kept returning"),
        "{followup}"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("notes.txt")).unwrap(),
        "gamma\n"
    );
}

#[test]
fn native_edits_work_inside_an_added_root_but_not_an_undeclared_sibling() {
    let base = tempfile::tempdir().unwrap();
    let base_path = base.path().canonicalize().unwrap();
    let (primary, added, sibling) = (
        base_path.join("primary"),
        base_path.join("added"),
        base_path.join("sibling"),
    );
    for dir in [&primary, &added, &sibling] {
        std::fs::create_dir(dir).unwrap();
        std::fs::write(dir.join("lib.txt"), "old\n").unwrap();
    }
    let target = |dir: &std::path::Path| dir.join("lib.txt").to_string_lossy().into_owned();
    let (added_file, sibling_file) = (target(&added), target(&sibling));
    let (text, outcome, _) = scripted_turn_at(
        &replace_edits(),
        vec![
            sse_tool_calls(&[
                ("read", json!({"path": added_file})),
                ("read", json!({"path": "lib.txt"})),
            ]),
            sse_tool_calls(&[
                replace(&added_file, "old", "new"),
                replace("lib.txt", "old", "new"),
            ]),
            sse_tool_calls(&[replace(&sibling_file, "old", "new")]),
            sse_reply("Done."),
        ],
        "edit both",
        &primary,
        |agent| {
            agent.workspace_mut().add_root(&added).unwrap();
        },
    );
    assert_eq!(outcome, TurnOutcome::Completed, "{text}");
    assert_eq!(
        std::fs::read_to_string(added.join("lib.txt")).unwrap(),
        "new\n"
    );
    assert_eq!(
        std::fs::read_to_string(primary.join("lib.txt")).unwrap(),
        "new\n"
    );
    assert_eq!(
        std::fs::read_to_string(sibling.join("lib.txt")).unwrap(),
        "old\n"
    );
}

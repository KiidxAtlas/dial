use super::*;
use crate::{
    config::types::{Config, HookConfig},
    llm::{
        self, Chat, fake,
        profiles::ModelProfile,
        provider::{Backend, ProviderKind},
    },
};
use serde_json::json;
use std::sync::{Arc, atomic::AtomicBool};

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
        PortableSummaryContext {
            config: Config::default(),
            layout: None,
            root: dir.path().to_path_buf(),
            control_state: json!({}),
            session_id: String::new(),
            hermetic: false,
        },
        vec![rewrite],
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
        &mut crate::agent::NullUi,
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
    let mut complete = crate::agent::compact::compaction_runtime::portable_summary_completion(
        PortableSummaryContext {
            config: crate::config::types::Config::default(),
            layout: None,
            root: dir.path().to_path_buf(),
            control_state: json!({}),
            session_id: String::new(),
            hermetic: false,
        },
        vec![],
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
fn large_window_background_preparation_starts_before_the_new_auto_trigger() {
    let policy = CompactionConfig::default();
    let runtime = CompactionRuntime::default();
    let identity = CompactionIdentity {
        provider: "p",
        model: "m",
        agent: None,
    };
    for (window, threshold) in [(200_000, 170_000), (1_000_000, 850_000)] {
        let lead = speculation_lead(threshold);
        let action = |tokens| {
            runtime
                .speculation_action(SpeculationContext {
                    policy: &policy,
                    identity,
                    session_id: "background-threshold",
                    window,
                    tokens,
                    mid_turn: false,
                    image_input: false,
                    fixed_tokens: 0,
                    parallel_supported: true,
                    native_supported: false,
                })
                .unwrap()
        };
        assert_eq!(action(threshold - lead - 1), SpeculationAction::None);
        assert_eq!(
            action(threshold - lead),
            SpeculationAction::Start(CompactionMethod::Soft)
        );
        assert_eq!(
            action(threshold - 1),
            SpeculationAction::Start(CompactionMethod::Soft)
        );
    }
}

#[test]
fn armed_background_summary_is_claimed_only_at_the_auto_trigger() {
    use crate::agent::compact::compaction::{CompactionOutcome, CompactionOutcomeKind};

    let config = Config::default();
    let policy = config.compaction.clone();
    let selection = config.selection();
    let source = Chat {
        messages: vec![json!({"role":"user","content":"history to summarize"})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    };
    for (window, threshold) in [(200_000, 170_000), (1_000_000, 850_000)] {
        let snapshot = CompactionSnapshot {
            source: source.clone(),
            native_request: source.clone(),
            visible_history: source.messages.clone(),
            policy: policy.clone(),
            model_selection: selection.clone(),
            main_config: config.clone(),
            native_profile: None,
            config_signature: "armed-threshold-config".into(),
            session_id: "armed-threshold".into(),
            branch_leaf: None,
            history_generation: 0,
            window,
            summary_window: window,
            summary_output_limit: None,
            timeout_secs: 5,
            user_dir: Default::default(),
            role_config: None,
            evidence_notes: String::new(),
            retained_overhead_tokens: 0,
            archive_migration: false,
        };
        let prepared = PreparedCompaction {
            summary: "summary".into(),
            prefix: vec![json!({"role":"user","content":"summary"})],
            first_kept: source.messages.len(),
            method: CompactionMethod::Soft,
            outcome: CompactionOutcome {
                kind: CompactionOutcomeKind::Compaction,
                method: Some(CompactionMethod::Soft),
                tokens_before: threshold - 1,
                tokens_after: 1,
                retained_messages: 0,
                summarized_messages: 1,
                continue_allowed: true,
                native_usage: None,
                handoff_path: None,
                handoff_error: None,
                reset_entry_id: None,
                reset_digest: None,
                journal_warning: None,
            },
        };
        let mut runtime = CompactionRuntime::default();
        runtime.slot = Some(Slot {
            snapshot: Arc::new(snapshot),
            options: CompactionOptions::default(),
            kind: WorkKind::Speculative,
            context_tokens_at_start: threshold - 1,
            cancel: Arc::new(AtomicBool::new(false)),
            timed_out: Arc::new(AtomicBool::new(false)),
            progress: mpsc::channel().1,
            result: mpsc::channel().1,
            worker: None,
            armed: Some(prepared),
        });
        assert!(runtime.is_armed());
        let action = |tokens| {
            runtime
                .speculation_action(SpeculationContext {
                    policy: &policy,
                    identity: CompactionIdentity {
                        provider: &selection.provider,
                        model: &selection.model,
                        agent: None,
                    },
                    session_id: "armed-threshold",
                    window,
                    tokens,
                    mid_turn: false,
                    image_input: false,
                    fixed_tokens: 0,
                    parallel_supported: true,
                    native_supported: false,
                })
                .unwrap()
        };
        assert_eq!(
            action(threshold - speculation_lead(threshold)),
            SpeculationAction::None
        );
        assert_eq!(action(threshold - 1), SpeculationAction::None);
        assert_eq!(action(threshold), SpeculationAction::Claim);
        assert_eq!(action(threshold + 1), SpeculationAction::Claim);
        assert!(
            runtime.is_armed(),
            "deciding must not consume the candidate"
        );
    }
}

fn sse_with_finish(text: &str, finish: &str) -> String {
    let event = json!({"choices":[{"delta":{"content":text},"finish_reason":finish}]});
    format!("data: {event}\n\ndata: [DONE]\n\n")
}

fn chat_backend(base_url: &str) -> Backend {
    Backend::new(
        ProviderKind::Endpoint {
            base_url: base_url.into(),
            model: "test".into(),
            api_key: None,
        },
        std::path::PathBuf::new(),
        5,
    )
    .with_session_id("compaction-runtime-wire")
    .with_profile(llm::profiles::ModelProfile {
        provider: "openai".into(),
        model: "test".into(),
        max_tokens: Some(1024),
        capabilities: llm::profiles::ModelCapabilities {
            prompt_cache: true,
            ..Default::default()
        },
        ..Default::default()
    })
}

fn portable_context(root: &std::path::Path, config: Config) -> PortableSummaryContext {
    PortableSummaryContext {
        config,
        layout: None,
        root: root.to_path_buf(),
        control_state: json!({}),
        session_id: String::new(),
        hermetic: false,
    }
}

fn plain_request() -> Chat {
    Chat {
        messages: vec![
            json!({"role":"system","content":"stable rules"}),
            json!({"role":"user","content":"original task"}),
        ],
        tools: vec![
            json!({"type":"function","function":{"name":"read","parameters":{"type":"object"}}}),
        ],
        temperature: 0.0,
        max_tokens: 256,
        seed: None,
    }
}

#[test]
fn live_handoff_request_forces_tool_choice_none_and_keeps_the_prefix() {
    let server = fake::serve(vec![
        fake::Script::Sse(sse_with_finish("handoff", "stop")),
        fake::Script::Sse(sse_with_finish("plain", "stop")),
    ]);
    let mut backend = chat_backend(&format!("{}/v1", server.url))
        .with_cache_namespace("compaction-live", &remote_config())
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut complete =
        portable_summary_completion(portable_context(dir.path(), Config::default()), vec![]);
    let cancel = Arc::new(AtomicBool::new(false));
    let mut handoff = plain_request();
    handoff.messages.push(json!({
        "role":"user", "dial_transient":true, "dial_compaction_handoff":true,
        "content":"[Harness maintenance: return only a continuation handoff.]"
    }));
    assert_eq!(
        complete(
            &mut backend,
            &handoff,
            &mut crate::agent::NullUi,
            cancel.clone(),
            5
        )
        .unwrap(),
        "handoff"
    );
    assert_eq!(
        complete(
            &mut backend,
            &plain_request(),
            &mut crate::agent::NullUi,
            cancel,
            5
        )
        .unwrap(),
        "plain"
    );
    let requests = server.requests();
    let live = fake::body(&requests[0]);
    let ordinary = fake::body(&requests[1]);
    assert_eq!(live["tool_choice"], "none", "{live}");
    assert_ne!(ordinary["tool_choice"], "none", "{ordinary}");
    assert_eq!(live["tools"], ordinary["tools"]);
    assert_eq!(live["tools"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        live["messages"].as_array().unwrap()[..2],
        ordinary["messages"].as_array().unwrap()[..2]
    );
}

#[test]
fn unusable_summary_replies_are_rejected_and_never_committed() {
    let tool_call = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"read","arguments":"{}"}}]},"finish_reason":"tool_calls"}]});
    let cases = [
        (
            "tool call",
            format!("data: {tool_call}\n\ndata: [DONE]\n\n"),
        ),
        ("truncated", sse_with_finish("partial", "length")),
        ("empty", sse_with_finish("   ", "stop")),
    ];
    for (label, body) in cases {
        let server = fake::serve(vec![fake::Script::Sse(body)]);
        let mut backend = chat_backend(&format!("{}/v1", server.url));
        let dir = tempfile::tempdir().unwrap();
        let mut complete =
            portable_summary_completion(portable_context(dir.path(), Config::default()), vec![]);
        let error = complete(
            &mut backend,
            &plain_request(),
            &mut crate::agent::NullUi,
            Arc::new(AtomicBool::new(false)),
            5,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("original context is unchanged"),
            "{label}: {error:#}"
        );
        assert_eq!(server.requests().len(), 1, "{label}");
    }
}

#[test]
fn cancelled_summary_request_is_interrupted_before_any_request() {
    let mut backend = chat_backend("http://127.0.0.1:9/v1");
    let dir = tempfile::tempdir().unwrap();
    let mut complete =
        portable_summary_completion(portable_context(dir.path(), Config::default()), vec![]);
    let error = complete(
        &mut backend,
        &plain_request(),
        &mut crate::agent::NullUi,
        Arc::new(AtomicBool::new(true)),
        5,
    )
    .unwrap_err();
    assert!(error.is::<llm::Interrupted>(), "{error:#}");
}

fn remote_config() -> Config {
    let mut config = Config {
        provider: Some("openai".into()),
        model: "gpt-4o".into(),
        ..Default::default()
    };
    config.endpoint = Some(crate::config::types::EndpointConfig {
        headers: None,
        base_url: "https://example.com/v1".into(),
        api_key_env: None,
        allow_insecure: false,
        protocol: None,
        account_id: None,
    });
    config
}

#[test]
fn queued_summary_request_cancels_without_reaching_the_provider() {
    use crate::agent::model::provider_slots::{QUEUED_NOTICE, acquire_key};
    struct QueueUi(std::sync::mpsc::Sender<()>);
    impl crate::agent::Ui for QueueUi {
        fn notice(&mut self, message: &str) {
            if message == QUEUED_NOTICE {
                let _ = self.0.send(());
            }
        }
    }
    let mut config = remote_config();
    config.task.provider_concurrency.insert("openai".into(), 1);
    let held = acquire_key("openai".into(), 1, &AtomicBool::new(false)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut complete = portable_summary_completion(portable_context(dir.path(), config), vec![]);
    let mut backend = chat_backend("http://127.0.0.1:9/v1");
    let cancel = Arc::new(AtomicBool::new(false));
    let (queued_tx, queued_rx) = std::sync::mpsc::channel();
    let worker_cancel = cancel.clone();
    let worker = std::thread::spawn(move || {
        let mut ui = QueueUi(queued_tx);
        complete(&mut backend, &plain_request(), &mut ui, worker_cancel, 5)
    });
    queued_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("request must queue behind the held slot");
    cancel.store(true, Ordering::SeqCst);
    let error = worker.join().unwrap().unwrap_err();
    assert!(error.is::<llm::Interrupted>(), "{error:#}");
    drop(held);
    assert!(
        acquire_key("openai".into(), 1, &AtomicBool::new(false)).is_some(),
        "cancelled queued wait leaked provider capacity"
    );
}

fn gate_snapshot(main_config: Config) -> CompactionSnapshot {
    let source = Chat {
        messages: vec![json!({"role":"user","content":"history to summarize"})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    };
    CompactionSnapshot {
        source: source.clone(),
        native_request: source.clone(),
        visible_history: source.messages.clone(),
        policy: CompactionConfig::default(),
        model_selection: main_config.selection(),
        main_config,
        native_profile: None,
        config_signature: "gate-config".into(),
        session_id: "gate-session".into(),
        branch_leaf: None,
        history_generation: 0,
        window: 200_000,
        summary_window: 200_000,
        summary_output_limit: None,
        timeout_secs: 5,
        user_dir: Default::default(),
        role_config: None,
        evidence_notes: String::new(),
        retained_overhead_tokens: 0,
        archive_migration: false,
    }
}

fn request_hook() -> HookConfig {
    HookConfig {
        event: "before_provider_request".into(),
        matcher: None,
        command: vec!["true".into()],
        timeout_secs: 30,
        fail_closed: None,
    }
}

#[test]
fn split_summary_fork_is_limited_to_idle_owned_cloud_routes() {
    let dir = tempfile::tempdir().unwrap();
    let mut serial = remote_config();
    serial.task.max_concurrency = 1;
    let mut wide = remote_config();
    wide.task.max_concurrency = 2;
    let local = chat_backend("http://127.0.0.1:1234/v1");
    let cloud = chat_backend("https://example.com/v1");
    type ForkCase<'a> = (
        &'static str,
        Config,
        &'a Backend,
        bool,
        Vec<HookConfig>,
        bool,
    );
    let cases: Vec<ForkCase<'_>> = vec![
        ("busy", wide.clone(), &cloud, true, vec![], false),
        ("local main", wide.clone(), &local, false, vec![], false),
        (
            "request hook",
            wide.clone(),
            &cloud,
            false,
            vec![request_hook()],
            false,
        ),
        ("serial task", serial, &cloud, false, vec![], false),
        ("owned cloud", wide, &cloud, false, vec![], true),
    ];
    for (name, config, main, busy, hooks, forked) in cases {
        let snapshot = gate_snapshot(config);
        let context = portable_context(dir.path(), snapshot.main_config.clone());
        let execution = summary_execution(&snapshot, context, &hooks, main, busy).unwrap();
        assert_eq!(execution.parallel.is_some(), forked, "{name}");
    }
}

#[test]
fn live_handoff_is_offered_only_to_the_unchanged_main_route() {
    let dir = tempfile::tempdir().unwrap();
    let cloud = chat_backend("https://example.com/v1");
    let local = chat_backend("http://127.0.0.1:1234/v1");
    type RouteCase<'a> = (
        &'static str,
        fn(&mut CompactionSnapshot),
        bool,
        &'a Backend,
        bool,
    );
    let cases: [RouteCase<'_>; 6] = [
        ("unchanged main route", |_| {}, false, &cloud, true),
        (
            "role config",
            |s| s.role_config = Some(remote_config()),
            false,
            &cloud,
            false,
        ),
        (
            "archive migration",
            |s| s.archive_migration = true,
            false,
            &cloud,
            false,
        ),
        (
            "portable rules",
            |s| {
                s.native_request
                    .messages
                    .push(json!({"role":"user","dial_portable_rules":true,"content":"rules"}))
            },
            false,
            &cloud,
            false,
        ),
        ("provider hook", |_| {}, true, &cloud, false),
        ("local main", |_| {}, false, &local, false),
    ];
    for (name, mutate, hook, main, live) in cases {
        let mut snapshot = gate_snapshot(remote_config());
        mutate(&mut snapshot);
        let hooks = if hook { vec![request_hook()] } else { vec![] };
        let context = portable_context(dir.path(), snapshot.main_config.clone());
        let execution = summary_execution(&snapshot, context, &hooks, main, false).unwrap();
        assert_eq!(execution.live_prefix, live, "{name}");
    }
}

#[test]
fn background_fallback_uses_the_side_cache_lane_and_restores_the_live_lane_on_error() {
    let config = remote_config();
    let mut backend = chat_backend("https://example.com/v1")
        .with_cache_namespace("live", &config)
        .unwrap();
    let live = backend.prompt_cache_key().map(str::to_owned);
    let side = chat_backend("https://example.com/v1")
        .with_cache_namespace("compaction-side", &config)
        .unwrap()
        .prompt_cache_key()
        .map(str::to_owned);
    assert!(live.is_some());
    assert!(side.is_some());
    assert_ne!(live, side);
    let expected_live = live.clone();
    let expected_side = side.clone();
    let mut complete = scoped_summary_completion(
        move |backend: &mut Backend,
              request: &Chat,
              _: &mut dyn Ui,
              _: Arc<AtomicBool>,
              _: u64|
              -> Result<String> {
            if request
                .messages
                .iter()
                .any(|m| m["dial_compaction_handoff"] == true)
            {
                assert_eq!(backend.prompt_cache_key(), expected_live.as_deref());
                Ok("handoff".into())
            } else {
                assert_eq!(backend.prompt_cache_key(), expected_side.as_deref());
                anyhow::bail!("portable summary failed")
            }
        },
        side,
    );
    let mut request = plain_request();
    request
        .messages
        .push(json!({"role":"user", "dial_compaction_handoff":true, "content":"handoff"}));
    let cancel = Arc::new(AtomicBool::new(false));
    assert_eq!(
        complete(
            &mut backend,
            &request,
            &mut crate::agent::NullUi,
            cancel.clone(),
            5
        )
        .unwrap(),
        "handoff"
    );
    assert_eq!(backend.prompt_cache_key(), live.as_deref());
    assert!(
        complete(
            &mut backend,
            &plain_request(),
            &mut crate::agent::NullUi,
            cancel,
            5
        )
        .is_err()
    );
    assert_eq!(backend.prompt_cache_key(), live.as_deref());
}

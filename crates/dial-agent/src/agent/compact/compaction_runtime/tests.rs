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

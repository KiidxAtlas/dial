use super::*;
use crate::{agent::NullUi, config::compaction::CompactionIdentity, llm::provider::ProviderKind};
use std::sync::atomic::Ordering;

fn backend(base_url: &str) -> Backend {
    Backend::new(
        ProviderKind::Endpoint {
            base_url: base_url.into(),
            model: "test".into(),
            api_key: None,
        },
        Default::default(),
        5,
    )
    .with_session_id("handoff-test")
}
fn source() -> Chat {
    Chat {
        messages: vec![
            json!({"role":"system","content":"stable rules"}),
            json!({"role":"user","content":"original task"}),
            json!({"role":"assistant","content":"evidence"}),
        ],
        tools: vec![
            json!({"type":"function","function":{"name":"read","parameters":{"type":"object"}}}),
        ],
        temperature: 0.3,
        max_tokens: 512,
        seed: None,
    }
}
fn context(source: &Chat) -> CompactionContext<'_> {
    CompactionContext {
        identity: CompactionIdentity {
            provider: "test",
            model: "test",
            agent: None,
        },
        window: 128_000,
        summary_window: 128_000,
        summary_output_limit: None,
        timeout_secs: 5,
        user_dir: std::path::Path::new("."),
        native_request: Some(source),
        retained_overhead_tokens: 0,
        supplemental_context: None,
    }
}

#[test]
fn live_handoff_preserves_prefix_and_tools_and_only_appends_maintenance() {
    let mut backend = backend("https://example.com/v1");
    let source = source();
    let original = (
        source.messages.clone(),
        source.tools.clone(),
        source.temperature,
        source.max_tokens,
        source.seed,
    );
    let mut called = false;
    let result = summarize(
        &mut backend,
        &CompactionConfig::default(),
        &context(&source),
        "keep decision",
        &Arc::new(AtomicBool::new(false)),
        &mut NullUi,
        &mut |_, request, _| {
            called = true;
            assert_eq!(request.tools, source.tools);
            assert_eq!(request.messages[..source.messages.len()], source.messages);
            let instruction = request.messages.last().unwrap();
            assert_eq!(instruction["dial_compaction_handoff"], true);
            assert!(
                instruction["content"]
                    .as_str()
                    .unwrap()
                    .contains("keep decision")
            );
            Ok("handoff".into())
        },
    )
    .unwrap();
    assert!(called);
    assert_eq!(result.as_deref(), Some("handoff"));
    assert_eq!(
        (
            source.messages,
            source.tools,
            source.temperature,
            source.max_tokens,
            source.seed
        ),
        original
    );
}

#[test]
fn local_and_oversized_handoff_do_not_spend_a_summary_request() {
    let source = source();
    let mut local = backend("http://127.0.0.1:1234/v1");
    let mut no_call = |_: &mut Backend, _: &Chat, _: &mut dyn Ui| -> Result<String> {
        panic!("ineligible live summary must not run")
    };
    assert!(
        summarize(
            &mut local,
            &CompactionConfig::default(),
            &context(&source),
            "",
            &Arc::new(AtomicBool::new(false)),
            &mut NullUi,
            &mut no_call
        )
        .unwrap()
        .is_none()
    );
    let mut remote = backend("https://example.com/v1");
    let mut small = context(&source);
    small.window = 16;
    assert!(
        summarize(
            &mut remote,
            &CompactionConfig::default(),
            &small,
            "",
            &Arc::new(AtomicBool::new(false)),
            &mut NullUi,
            &mut no_call
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn provider_errors_fall_back_but_cancellation_and_control_denial_are_never_hidden() {
    let mut backend = backend("https://example.com/v1");
    let source = source();
    let cancel = Arc::new(AtomicBool::new(false));
    assert!(
        summarize(
            &mut backend,
            &CompactionConfig::default(),
            &context(&source),
            "",
            &cancel,
            &mut NullUi,
            &mut |_, _, _| Err(anyhow::anyhow!(
                "context_length_exceeded: maximum context length exceeded"
            ))
        )
        .unwrap()
        .is_none()
    );
    assert!(
        summarize(
            &mut backend,
            &CompactionConfig::default(),
            &context(&source),
            "",
            &cancel,
            &mut NullUi,
            &mut |_, _, _| Err(anyhow::anyhow!("provider rejects tool_choice none"))
        )
        .unwrap()
        .is_none()
    );
    let error = summarize(
        &mut backend,
        &CompactionConfig::default(),
        &context(&source),
        "",
        &cancel,
        &mut NullUi,
        &mut |_, _, _| {
            Err(super::super::CompactionControlDenied(anyhow::anyhow!("control denied")).into())
        },
    )
    .unwrap_err();
    assert!(error.is::<super::super::CompactionControlDenied>());
    let error = summarize(
        &mut backend,
        &CompactionConfig::default(),
        &context(&source),
        "",
        &cancel,
        &mut NullUi,
        &mut |_, _, _| Err(llm::Interrupted.into()),
    )
    .unwrap_err();
    assert!(error.is::<llm::Interrupted>());
    assert!(
        summarize(
            &mut backend,
            &CompactionConfig::default(),
            &context(&source),
            "",
            &cancel,
            &mut NullUi,
            &mut |_, _, _| Ok(" ".into())
        )
        .unwrap()
        .is_none()
    );
    let error = summarize(
        &mut backend,
        &CompactionConfig::default(),
        &context(&source),
        "",
        &cancel,
        &mut NullUi,
        &mut |_, _, _| {
            cancel.store(true, Ordering::SeqCst);
            Ok("not committable".into())
        },
    )
    .unwrap_err();
    assert!(error.is::<llm::Interrupted>());
}

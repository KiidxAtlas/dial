use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;
use serde_json::json;

#[test]
fn prospective_queue_and_draft_are_not_last_request_or_admitted_history() {
    with_agent("", |agent| {
        agent.record_prompt_usage(999_999);
        let before = agent.session.message_count();
        let empty = agent.context_decision(&[]).unwrap();
        let pending = [
            json!({"role":"user", "content":"queued input ".repeat(1024)}),
            json!({"role":"user", "content":"draft text ".repeat(1024)}),
        ];
        let next = agent.context_decision(&pending).unwrap();
        assert!(next.next_tokens > empty.next_tokens);
        assert_eq!(next.pending_messages, 2);
        assert!(next.last_request.is_none());
        assert_ne!(next.next_tokens, agent.last_prompt_tokens);
        assert_eq!(agent.session.message_count(), before);
        assert_eq!(
            agent.context_decision(&[]).unwrap().next_tokens,
            empty.next_tokens
        );
    });
}

#[test]
fn small_window_and_fixed_overhead_use_selected_policy() {
    for window in [4096, 32768] {
        with_agent(&format!("context_window={window}"), |agent| {
            let report = agent.context_decision(&[]).unwrap();
            assert_eq!(report.window, window);
            assert_eq!(
                report.prompt_budget + report.response_reserve,
                u64::from(window)
            );
            let selection = agent.auth_config.selection();
            assert_eq!(
                report.trigger,
                Some(
                    agent
                        .config
                        .compaction
                        .effective_threshold(
                            CompactionIdentity {
                                provider: &selection.provider,
                                model: &selection.model,
                                agent: None
                            },
                            window
                        )
                        .unwrap()
                )
            );
            assert!(report.fixed_tokens > 0);
        });
    }
}

#[test]
fn media_cost_is_explicitly_uncertain_and_preview_is_read_only() {
    with_agent("", |agent| {
        let report = agent
            .context_decision(&[
                json!({"role":"user", "content":"look", "dial_images":[{"path":"a.png"}]}),
            ])
            .unwrap();
        assert!(report.approximate_media);
        let before = agent.session.message_count();
        let preview = agent
            .compaction_preflight(&CompactionOptions::default())
            .unwrap();
        assert_eq!(preview.eligible_messages, 0);
        let explicit = agent
            .compaction_preflight(&CompactionOptions::from_args("remote").unwrap())
            .unwrap();
        assert_eq!(explicit.methods.first(), Some(&CompactionMethod::Remote));
        assert_eq!(explicit.methods.last(), Some(&CompactionMethod::Soft));
        assert_eq!(agent.session.message_count(), before);
    });
}

/// The preview resolves native compaction through the same helper as the blocking path.
#[test]
fn compaction_preview_offers_derived_native_compaction_only_where_execution_would() {
    for (overlay, model, native) in [
        ("", "claude-opus-5-5", true),
        ("", "claude-opus-4-5", false),
        (
            "[compaction]\nremote_enabled=false",
            "claude-opus-5-5",
            false,
        ),
    ] {
        with_agent(overlay, |agent| {
            agent.auth_config.provider = Some("anthropic".into());
            agent.auth_config.model = model.into();
            let backend = crate::llm::provider::Backend::new(
                crate::llm::provider::ProviderKind::AnthropicEndpoint {
                    base_url: "https://api.anthropic.com/v1".into(),
                    model: model.into(),
                    api_key: Some("anth-secret".into()),
                },
                std::path::PathBuf::new(),
                5,
            );
            let policy = agent.config.compaction.clone();
            let selection = agent.auth_config.selection();
            let identity = CompactionIdentity {
                provider: &selection.provider,
                model: &selection.model,
                agent: None,
            };
            let supported = compaction::native_supported(&backend, &policy, identity);
            agent.backend = Some(backend);
            let methods = agent
                .compaction_preflight(&CompactionOptions::default())
                .unwrap()
                .methods;
            assert_eq!(
                methods,
                policy.available_methods(&CompactionOptions::default(), supported),
                "{overlay} {model}"
            );
            assert_eq!(
                methods.first() == Some(&CompactionMethod::Remote),
                native,
                "{overlay} {model}: {methods:?}"
            );
        });
    }
}

use super::*;

#[test]
fn fixed_overhead_recommends_capacity_change_not_compaction() {
    let report = ContextDecision {
        provider: "test".into(),
        model: "small".into(),
        window: 4096,
        model_window: 4096,
        response_reserve: 1024,
        prompt_budget: 3072,
        trigger: Some(2457),
        next_tokens: 4000,
        fixed_tokens: 3500,
        history_tokens: 500,
        pending_messages: 2,
        last_request: None,
        approximate_media: true,
    };
    let text = render_decision(&report);
    assert!(text.contains("History compaction alone cannot fit"));
    assert!(text.contains("Last sent request: none yet"));
    assert!(!text.contains("efficiency cap"));
    assert!(text.contains("2 queued/draft messages included"));
    assert!(text.contains("approximate/opaque"));
}

#[test]
fn capped_history_budget_is_explained_beside_the_model_window() {
    let report = ContextDecision {
        provider: "p".into(),
        model: "m".into(),
        window: 262_144,
        model_window: 1_000_000,
        response_reserve: 0,
        prompt_budget: 200_000,
        trigger: None,
        next_tokens: 14_000,
        fixed_tokens: 10_000,
        history_tokens: 4_000,
        pending_messages: 0,
        last_request: None,
        approximate_media: false,
    };
    let text = render_decision(&report);
    assert!(
        text.contains("1,000,000 tokens (model) · history planned against 262,144"),
        "{text}"
    );
    assert!(!text.contains("queued/draft"), "{text}");
}

#[test]
fn preflight_explains_noop_fallback_cost_and_cancel() {
    let report = CompactionPreflight {
        methods: vec![CompactionMethod::Remote, CompactionMethod::Soft],
        eligible_messages: 0,
        retained_messages: 1,
        keep_recent_tokens: 4096,
        notes_reset: false,
        summary_provider: "endpoint".into(),
        summary_model: "summary".into(),
        remote_endpoint: false,
    };
    let text = render_preflight(&report);
    assert!(text.contains("remote → soft"));
    assert!(text.contains("No eligible earlier complete history"));
    assert!(text.contains("hosted requests may incur charges"));
    assert!(text.contains("Cancel leaves history unchanged"));
    assert!(text.contains("Queued input and the composer draft remain pending"));
}

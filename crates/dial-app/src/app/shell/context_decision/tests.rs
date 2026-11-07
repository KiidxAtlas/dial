use super::*;

#[test]
fn fixed_overhead_recommends_capacity_change_not_compaction() {
    let report = ContextDecision {
        provider: "test".into(),
        model: "small".into(),
        window: 4096,
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
    assert!(text.contains("no request recorded"));
    assert!(text.contains("2 queued/draft messages included"));
    assert!(text.contains("approximate/opaque"));
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

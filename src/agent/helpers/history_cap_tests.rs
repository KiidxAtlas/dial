use super::capped_window;

#[test]
fn ceiling_limits_huge_windows_only_when_economy_is_on() {
    assert_eq!(capped_window(1_000_000, 120_000, true), 120_000);
    assert_eq!(capped_window(32_768, 120_000, true), 32_768);
    assert_eq!(capped_window(1_000_000, 120_000, false), 1_000_000);
    assert_eq!(capped_window(1_000_000, 100, true), 8192);
    assert_eq!(capped_window(1_000_000, 0, true), 1_000_000);
}

#[test]
fn default_ceiling_bounds_the_auto_trigger_on_huge_windows_only() {
    let cap = crate::config::types::EfficiencyConfig::default().history_token_cap;
    let policy = crate::config::compaction::CompactionConfig::default();
    let identity = crate::config::compaction::CompactionIdentity {
        provider: "anthropic",
        model: "claude-opus-5-5",
        agent: None,
    };
    let trigger = |window| {
        policy
            .effective_threshold(identity, capped_window(window, cap, true))
            .unwrap()
    };
    // A 1M window used to trigger near 680k; the default ceiling keeps requests under ~180k.
    assert_eq!(trigger(1_000_000), 178_258);
    assert!(
        policy
            .effective_threshold(identity, capped_window(1_000_000, cap, false))
            .unwrap()
            > 600_000
    );
    // Windows at or below the ceiling keep their own trigger.
    assert_eq!(
        trigger(200_000),
        policy.effective_threshold(identity, 200_000).unwrap()
    );
    // The summary budget and verbatim suffix are unchanged by the ceiling.
    assert_eq!(
        policy.summary_budget(capped_window(1_000_000, cap, true)),
        policy.summary_budget(1_000_000)
    );
    assert!(policy.keep_recent_tokens < trigger(1_000_000) as u32 / 4);
}

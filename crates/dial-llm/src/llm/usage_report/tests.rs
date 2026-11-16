use super::*;

fn limit(label: &str, used: f64, resets_at_ms: Option<u64>) -> UsageLimit {
    UsageLimit {
        id: label.into(),
        label: label.into(),
        scope: UsageScope {
            provider: "anthropic".into(),
            ..Default::default()
        },
        window: Some(UsageWindow {
            id: label.into(),
            label: label.into(),
            resets_at_ms,
            ..Default::default()
        }),
        amount: UsageAmount {
            used: Some(used),
            limit: Some(100.0),
            remaining: None,
            used_fraction: None,
            remaining_fraction: None,
            unit: UsageUnit::Percent,
        },
        status: None,
        notes: vec![],
    }
}

#[test]
fn fraction_precedence_matches_omp() {
    let mut l = limit("5h", 50.0, None);
    assert_eq!(resolve_used_fraction(&l), Some(0.5));
    l.amount.used_fraction = Some(0.9);
    assert_eq!(resolve_used_fraction(&l), Some(0.9));
    l.amount.used_fraction = None;
    l.amount.unit = UsageUnit::Requests;
    l.amount.used = Some(3.0);
    l.amount.limit = Some(4.0);
    assert_eq!(resolve_used_fraction(&l), Some(0.75));
    l.amount.limit = None;
    l.amount.used = None;
    l.amount.remaining_fraction = Some(0.25);
    assert_eq!(resolve_used_fraction(&l), Some(0.75));
}

#[test]
fn status_summary_picks_tightest_limit_and_countdown() {
    let now = 1_000_000_000;
    let report = UsageReport {
        provider: "anthropic".into(),
        fetched_at_ms: now,
        limits: vec![
            limit("5h", 80.0, Some(now + 2 * 3_600_000 + 5 * 60_000)),
            limit("7d", 10.0, None),
        ],
        reset_credits: None,
        notes: vec![],
        metadata: Default::default(),
    };
    assert_eq!(
        status_summary(&[report], now).as_deref(),
        Some("anthropic 5h 80% · resets 2h5m")
    );
    assert_eq!(status_summary(&[], now), None);
}

#[test]
fn plans_read_as_names_on_reports_and_the_status_line() {
    for (raw, label) in [
        ("plus", "Plus"),
        ("pro", "Pro"),
        ("claude_max", "Max"),
        ("max_20x", "Max 20x"),
        ("free_limited_copilot", "Free Limited Copilot"),
        ("Gemini Code Assist Standard", "Gemini Code Assist Standard"),
    ] {
        assert_eq!(plan_label(raw), label);
    }
    let now = 1_000_000_000;
    let mut report = UsageReport {
        provider: "openai-codex".into(),
        fetched_at_ms: now,
        limits: vec![limit("5h", 40.0, None)],
        reset_credits: None,
        notes: vec![],
        metadata: Default::default(),
    };
    assert_eq!(report.plan(), None);
    report.metadata.insert("planType".into(), "plus".into());
    assert_eq!(report.plan().as_deref(), Some("Plus"));
    assert_eq!(
        status_summary(std::slice::from_ref(&report), now).as_deref(),
        Some("openai-codex (Plus) 5h 40%")
    );
    let json = outcomes_json(&[
        Outcome {
            provider: "openai-codex",
            result: Ok(report),
        },
        Outcome {
            provider: "anthropic",
            result: Err("offline".into()),
        },
    ]);
    assert_eq!(json["providers"][0]["plan"], "Plus");
    assert_eq!(json["providers"][0]["report"]["provider"], "openai-codex");
    assert_eq!(json["providers"][1]["plan"], serde_json::Value::Null);
    assert_eq!(json["providers"][1]["error"], "offline");
}

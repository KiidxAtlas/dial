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

use super::*;
use serde_json::json;

/// 2026-05-01T00:00:00Z
const NOW: u64 = 1_777_593_600_000;

fn fixture() -> Value {
    json!({
        "usesTokenRateLimitsBilling": true,
        "limits": {
            "standard": {
                "fiveHour": { "usedPercent": 95, "windowEnd": "2026-05-01T02:00:00.000Z" },
                "weekly": { "usedPercent": 40, "windowEnd": "2026-04-30T00:00:00.000Z" }
            },
            "core": {
                "monthly": { "usedPercent": "12.5", "windowEnd": "2026-05-15T00:00:00Z" }
            }
        },
        "extraUsageBalanceCents": 1250
    })
}

#[test]
fn parses_windows_fractions_and_reset_times() {
    let report = parse_limits(&fixture(), NOW).unwrap();
    assert_eq!(report.provider, PROVIDER);
    assert_eq!(report.fetched_at_ms, NOW);
    let ids: Vec<&str> = report.limits.iter().map(|l| l.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "factory-droid:standard:5h",
            "factory-droid:standard:weekly",
            "factory-droid:core:monthly",
            "factory-droid:extra-balance",
        ]
    );

    let five = &report.limits[0];
    assert_eq!(five.amount.used, Some(95.0));
    assert_eq!(five.amount.limit, Some(100.0));
    assert_eq!(five.amount.remaining, Some(5.0));
    assert_eq!(five.amount.used_fraction, Some(0.95));
    assert!(matches!(five.amount.unit, UsageUnit::Percent));
    assert_eq!(five.status, Some(UsageStatus::Warning));
    let window = five.window.as_ref().unwrap();
    assert_eq!(window.id, "standard-5h");
    assert_eq!(window.duration_ms, Some(5 * HOUR_MS));
    assert_eq!(window.resets_at_ms, Some(NOW + 2 * HOUR_MS));
    assert_eq!(five.scope.window_id.as_deref(), Some("standard-5h"));

    let core = &report.limits[2];
    assert_eq!(core.amount.used, Some(12.5));
    assert_eq!(core.amount.used_fraction, Some(0.125));
    assert_eq!(core.status, Some(UsageStatus::Ok));
    assert_eq!(core.window.as_ref().unwrap().duration_ms, Some(30 * DAY_MS));
}

#[test]
fn lapsed_window_reads_as_unused_without_reset() {
    let report = parse_limits(&fixture(), NOW).unwrap();
    let weekly = &report.limits[1];
    assert_eq!(weekly.amount.used, Some(0.0));
    assert_eq!(weekly.amount.used_fraction, Some(0.0));
    assert_eq!(weekly.amount.remaining, Some(100.0));
    let window = weekly.window.as_ref().unwrap();
    assert_eq!(window.resets_at_ms, None);
    assert_eq!(window.duration_ms, Some(WEEK_MS));
    assert_eq!(weekly.status, Some(UsageStatus::Ok));
}

#[test]
fn extra_balance_is_usd_and_untouched() {
    let report = parse_limits(&fixture(), NOW).unwrap();
    let balance = report.limits.last().unwrap();
    assert_eq!(balance.id, "factory-droid:extra-balance");
    assert_eq!(balance.amount.limit, Some(12.5));
    assert_eq!(balance.amount.remaining, Some(12.5));
    assert_eq!(balance.amount.used, Some(0.0));
    assert!(matches!(balance.amount.unit, UsageUnit::Usd));
    assert_eq!(balance.status, Some(UsageStatus::Ok));
    assert!(balance.window.is_none());
}

#[test]
fn non_token_billing_account_reports_note_without_limits() {
    let report = parse_limits(&json!({ "usesTokenRateLimitsBilling": false }), NOW).unwrap();
    assert!(report.limits.is_empty());
    assert_eq!(report.notes.len(), 1);
    assert!(report.notes[0].contains("does not use token-rate-limit billing"));
}

#[test]
fn malformed_payloads_are_errors() {
    assert!(parse_limits(&json!([]), NOW).is_err());
    assert!(parse_limits(&json!({ "limits": "nope" }), NOW).is_err());
    assert!(parse_limits(&json!({ "limits": {} }), NOW).is_err());
    let bad_percent = json!({
        "limits": { "standard": { "fiveHour": { "usedPercent": "lots" } } }
    });
    assert!(parse_limits(&bad_percent, NOW).is_err());
}

#[test]
fn iso_timestamps_parse_with_fraction_and_offset() {
    assert_eq!(parse_iso_ms("2026-05-01T00:00:00Z"), Some(NOW));
    assert_eq!(parse_iso_ms("2026-05-01T00:00:00.000Z"), Some(NOW));
    assert_eq!(parse_iso_ms("2026-05-01T02:00:00+02:00"), Some(NOW));
    assert_eq!(parse_iso_ms("not a date"), None);
}

#[test]
fn fetch_without_stored_grant_is_none() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    assert!(fetch(dir.path(), &cancel).unwrap().is_none());
}

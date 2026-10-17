use super::*;
use serde_json::json;

/// 2026-05-01T00:00:00Z
const NOW: u64 = 1_777_593_600_000;

fn fixture() -> Value {
    json!({
        "usage": {
            "limit": 100,
            "used": 30,
            "resetTime": "2026-05-02T00:00:00Z"
        },
        "limits": [{
            "window": { "duration": 300, "timeUnit": "TIME_UNIT_MINUTE" },
            "detail": { "limit": 50, "remaining": 10, "name": "Burst" }
        }],
        "usages": {
            "limit_month_total": { "used_ratio": 0.5, "resetAt": "2026-06-01T00:00:00Z" },
            "limit_5h": { "used_ratio": 0.9 }
        }
    })
}

#[test]
fn parses_summary_limits_and_aggregates() {
    let rows = parse_usage_payload(&fixture(), NOW).unwrap();
    let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(labels, ["Weekly limit", "Burst", "Monthly total"]);

    let weekly = &rows[0];
    assert_eq!(weekly.used, Some(30.0));
    assert_eq!(weekly.limit, Some(100.0));
    let window = weekly.window.as_ref().unwrap();
    assert_eq!(window.id, "7d");
    assert_eq!(window.resets_at_ms, Some(1_777_680_000_000));

    let burst = &rows[1];
    assert_eq!(burst.used, Some(40.0));
    let window = burst.window.as_ref().unwrap();
    assert_eq!(window.id, "5h");
    assert_eq!(window.label, "5h limit");
    assert_eq!(window.duration_ms, Some(300 * 60_000));

    let monthly = &rows[2];
    assert_eq!(monthly.used, Some(50.0));
    assert_eq!(monthly.limit, Some(100.0));
    assert!(matches!(monthly.unit, UsageUnit::Percent));
}

#[test]
fn converts_rows_to_limits_with_fractions_and_status() {
    let rows = parse_usage_payload(&fixture(), NOW).unwrap();
    let limits: Vec<UsageLimit> = rows
        .iter()
        .enumerate()
        .filter_map(|(i, r)| to_usage_limit(r, i, Some("acct")))
        .collect();
    assert_eq!(limits[0].id, "kimi-code:0");
    assert_eq!(limits[0].amount.used_fraction, Some(0.3));
    assert_eq!(limits[0].amount.remaining, Some(70.0));
    assert_eq!(limits[0].status, Some(super::super::types::UsageStatus::Ok));
    assert_eq!(limits[0].scope.account_id.as_deref(), Some("acct"));
    assert_eq!(limits[0].scope.window_id.as_deref(), Some("7d"));
    assert_eq!(limits[1].amount.used_fraction, Some(0.8));
    assert_eq!(limits[1].amount.remaining_fraction, Some(0.2));
}

#[test]
fn relative_reset_fields_are_seconds_from_now() {
    let data = json!({ "resetIn": 60, "limit": 1 });
    let data = data.as_object().unwrap();
    assert_eq!(parse_reset_time(data, NOW), Some(NOW + 60_000));
    let absolute_ms = json!({ "reset_at": 1_777_680_000_000u64 });
    assert_eq!(
        parse_reset_time(absolute_ms.as_object().unwrap(), NOW),
        Some(1_777_680_000_000)
    );
    let absolute_secs = json!({ "reset_at": 1_777_680_000u64 });
    assert_eq!(
        parse_reset_time(absolute_secs.as_object().unwrap(), NOW),
        Some(1_777_680_000_000)
    );
}

#[test]
fn row_without_usage_or_limit_is_skipped() {
    let payload = json!({
        "limits": [{ "detail": { "name": "Empty" } }],
        "usages": { "limit_month_code": { "used_ratio": "n/a" } }
    });
    let rows = parse_usage_payload(&payload, NOW).unwrap();
    assert!(rows.is_empty());
}

#[test]
fn non_object_payload_is_rejected() {
    assert!(parse_usage_payload(&json!([1, 2]), NOW).is_none());
    assert!(parse_usage_payload(&json!("nope"), NOW).is_none());
}

#[test]
fn duration_labels_and_window_ids() {
    assert_eq!(
        format_duration_label(Some(120.0), Some("MINUTE")).as_deref(),
        Some("2h limit")
    );
    assert_eq!(
        format_duration_label(Some(30.0), Some("MINUTE")).as_deref(),
        Some("30m limit")
    );
    assert_eq!(
        format_duration_label(Some(2.0), Some("DAY")).as_deref(),
        Some("2d limit")
    );
    assert_eq!(format_duration_label(Some(2.0), Some("FORTNIGHT")), None);
    assert_eq!(canonical_window_id(2 * DAY_MS), "2d");
    assert_eq!(canonical_window_id(5 * 3_600_000), "5h");
    assert_eq!(canonical_window_id(90 * 60_000), "90m");
}

#[test]
fn base_url_precedence_is_credential_then_env_then_default() {
    assert_eq!(
        resolve_base_url(
            Some("https://cred.example/v1/"),
            Some("https://env.example")
        ),
        "https://cred.example/v1"
    );
    assert_eq!(
        resolve_base_url(Some("  "), Some("https://env.example/")),
        "https://env.example"
    );
    assert_eq!(resolve_base_url(None, None), DEFAULT_BASE_URL);
}

#[test]
fn fetch_without_stored_grant_is_none() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    assert!(fetch(dir.path(), &cancel).unwrap().is_none());
}

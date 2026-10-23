use super::*;
use crate::llm::usage_report::UsageStatus;
use serde_json::json;

/// 2027-01-15T08:00:00Z, a fixed clock for fixtures.
const NOW: u64 = 1_800_000_000_000;

fn payload(limits: Value) -> Value {
    json!({
        "success": true,
        "code": 200,
        "msg": "ok",
        "data": { "level": "pro", "limits": limits }
    })
}

fn assert_close(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
}

#[test]
fn primary_request_limit_parses_window_meter_and_plan() {
    let report = parse_quota(
        &payload(json!([{
            "type": "TIME_LIMIT",
            "unit": 3,
            "number": 5,
            "usage": 100,
            "currentValue": 40,
            "remaining": 60,
            "percentage": 40,
            "nextResetTime": 1_700_000_000_000u64
        }])),
        NOW,
    )
    .unwrap();

    assert_eq!(report.provider, "zai");
    assert_eq!(report.fetched_at_ms, NOW);
    assert_eq!(report.metadata.get("plan").map(String::as_str), Some("pro"));
    let limit = &report.limits[0];
    assert_eq!(limit.id, "zai:requests:5h");
    assert_eq!(limit.label, "ZAI Request Quota");
    assert_eq!(
        limit.scope,
        UsageScope {
            provider: "zai".into(),
            window_id: Some("5h".into()),
            shared: Some(true),
            ..UsageScope::default()
        }
    );
    let window = limit.window.as_ref().unwrap();
    assert_eq!(window.id, "5h");
    assert_eq!(window.label, "5 Hours");
    assert_eq!(window.duration_ms, Some(5 * HOUR_MS));
    assert_eq!(limit.amount.unit, UsageUnit::Requests);
    assert_eq!(limit.amount.used, Some(40.0));
    assert_eq!(limit.amount.limit, Some(100.0));
    assert_eq!(limit.amount.remaining, Some(60.0));
    assert_close(limit.amount.used_fraction.unwrap(), 0.4);
    assert_close(limit.amount.remaining_fraction.unwrap(), 0.6);
    assert_eq!(limit.status, Some(UsageStatus::Ok));
}

#[test]
fn percentage_meters_use_percent_while_absolute_credits_use_counters() {
    let report = parse_quota(
        &payload(json!([
            { "type": "TOKENS_LIMIT", "unit": 6, "percentage": 75 },
            { "type": "CREDIT_LIMIT", "unit": 3, "number": 5, "usage": 12000, "currentValue": 1438, "percentage": 11 },
            { "type": "CREDIT_LIMIT", "unit": 4, "number": 1, "usage": 0, "percentage": 11 }
        ])),
        NOW,
    )
    .unwrap();

    let tokens = &report.limits[0];
    assert_eq!(tokens.id, "zai:tokens:1w");
    assert_eq!(tokens.label, "ZAI Weekly Token Quota");
    assert_eq!(tokens.amount.unit, UsageUnit::Tokens);
    assert_eq!(tokens.amount.used, None);
    assert_close(tokens.amount.used_fraction.unwrap(), 0.75);
    assert_eq!(tokens.window.as_ref().unwrap().duration_ms, Some(WEEK_MS));
    assert_eq!(tokens.status, Some(UsageStatus::Ok));

    // Absolute counters win over the rounded percentage (1438/12000 ≈ 11.98%, not 11%).
    let credits = &report.limits[1];
    assert_eq!(credits.id, "zai:credits:5h");
    assert_eq!(credits.label, "ZAI 5 Hours Credit Quota");
    assert_eq!(credits.amount.unit, UsageUnit::Credits);
    assert_eq!(credits.amount.used, Some(1438.0));
    assert_eq!(credits.amount.limit, Some(12000.0));
    assert_close(credits.amount.used_fraction.unwrap(), 1438.0 / 12000.0);
    assert_eq!(credits.scope.shared, Some(true));

    // Without an allotment the percentage is the only source.
    let percent_only = &report.limits[2];
    assert_eq!(percent_only.id, "zai:credits:1d");
    assert_close(percent_only.amount.used_fraction.unwrap(), 0.11);
}

#[test]
fn overshooting_percentage_clamps_and_maps_status() {
    let report = parse_quota(
        &payload(json!([
            { "type": "TIME_LIMIT", "unit": 5, "number": 1, "percentage": 150 },
            { "type": "TOKENS_LIMIT", "unit": 6, "percentage": 92 }
        ])),
        NOW,
    )
    .unwrap();

    let monthly = &report.limits[0];
    assert_eq!(monthly.id, "zai:requests:1mo");
    assert_eq!(monthly.window.as_ref().unwrap().label, "Monthly");
    assert_eq!(monthly.window.as_ref().unwrap().duration_ms, Some(MONTH_MS));
    assert_close(monthly.amount.used_fraction.unwrap(), 1.0);
    assert_eq!(monthly.status, Some(UsageStatus::Exhausted));
    assert_eq!(report.limits[1].status, Some(UsageStatus::Warning));
}

#[test]
fn reset_times_accept_seconds_milliseconds_and_numeric_strings() {
    let report = parse_quota(
        &payload(json!([
            { "type": "TOKENS_LIMIT", "unit": 3, "number": 5, "percentage": 10, "nextResetTime": 1_700_000_000u64 },
            { "type": "TOKENS_LIMIT", "unit": 4, "number": 1, "percentage": 10, "nextResetTime": "1700000001" },
            { "type": "TOKENS_LIMIT", "unit": 6, "percentage": 10, "nextResetTime": 1_700_000_002_000u64 },
            { "type": "TOKENS_LIMIT", "unit": 5, "number": 1, "percentage": 10 }
        ])),
        NOW,
    )
    .unwrap();

    let resets: Vec<Option<u64>> = report
        .limits
        .iter()
        .map(|limit| limit.window.as_ref().unwrap().resets_at_ms)
        .collect();
    assert_eq!(
        resets,
        vec![
            Some(1_700_000_000_000),
            Some(1_700_000_001_000),
            Some(1_700_000_002_000),
            None,
        ]
    );
}

#[test]
fn zread_feature_quota_is_split_from_request_quota() {
    let report = parse_quota(
        &payload(json!([{
            "type": "TIME_LIMIT",
            "unit": 3,
            "number": 1,
            "usage": 50,
            "currentValue": 3,
            "usageDetails": [
                { "modelCode": "search-prime", "usage": 1 },
                { "modelCode": "web-reader" },
                { "modelCode": "zread" }
            ]
        }])),
        NOW,
    )
    .unwrap();

    let limit = &report.limits[0];
    assert_eq!(limit.id, "zai:features:zread:1h");
    assert_eq!(limit.label, "ZAI Zread Quota");
    assert_eq!(limit.scope.tier.as_deref(), Some("zread"));
    assert_eq!(limit.scope.shared, Some(false));
    assert_eq!(limit.window.as_ref().unwrap().label, "1 Hour");
}

#[test]
fn unknown_and_untyped_items_are_skipped() {
    let report = parse_quota(
        &payload(json!([
            { "type": "OTHER", "usage": 1 },
            7,
            { "usage": 1 },
            { "type": "TOKENS_LIMIT", "unit": 6, "percentage": 5 }
        ])),
        NOW,
    )
    .unwrap();

    assert_eq!(report.limits.len(), 1);
    assert_eq!(report.limits[0].id, "zai:tokens:1w");
}

#[test]
fn malformed_payloads_are_errors() {
    let failed = parse_quota(&json!({"success": false, "code": 500, "msg": "boom"}), NOW)
        .unwrap_err()
        .to_string();
    assert!(
        failed.contains("not successful (code 500): boom"),
        "{failed}"
    );
    assert!(parse_quota(&json!([1, 2]), NOW).is_err());
    assert!(parse_quota(&json!({"success": true, "code": 200}), NOW).is_err());
    assert!(parse_quota(&json!({"success": true, "data": {"limits": "nope"}}), NOW).is_err());
    assert!(parse_quota(&payload(json!([{ "type": "OTHER" }])), NOW).is_err());
}

#[test]
fn endpoint_uses_origin_of_base_url_or_default() {
    assert_eq!(endpoint_origin(None), "https://api.z.ai");
    assert_eq!(
        endpoint_origin(Some("https://api.z.ai/api/anthropic")),
        "https://api.z.ai"
    );
    assert_eq!(endpoint_origin(Some("not a url")), "https://api.z.ai");
}

#[test]
fn local_stamp_uses_query_format() {
    let stamp = local_stamp(1_700_000_000_000).unwrap();
    assert_eq!(stamp.len(), 19, "{stamp}");
    assert_eq!(&stamp[4..5], "-");
    assert_eq!(&stamp[10..11], " ");
}

#[test]
fn no_stored_grant_is_none() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    assert!(fetch(dir.path(), &cancel).unwrap().is_none());
}

use super::*;
use serde_json::json;
use std::sync::atomic::AtomicBool;

#[test]
fn monthly_reset_fallback_preserves_javascript_month_overflow() {
    for (start, end) in [
        ("2026-01-31T12:30:00Z", "2026-03-03T12:30:00Z"),
        ("2024-01-31T12:30:00Z", "2024-03-02T12:30:00Z"),
        ("2026-12-31T12:30:00Z", "2027-01-31T12:30:00Z"),
    ] {
        let payload = json!({"billingCycleStart":start});
        assert_eq!(
            derive_resets_at(payload.as_object().unwrap()),
            Some(parse_iso_ms(end).unwrap() as u64)
        );
    }
    assert_eq!(add_one_month_ms(i64::MAX), None);
}

fn obj(value: &Value) -> &Map<String, Value> {
    value.as_object().unwrap()
}

#[test]
fn cents_bucket_from_limit_and_used() {
    let value = json!({ "used": 500, "limit": 2000 });
    let amount = parse_cents_bucket(obj(&value)).unwrap();
    assert_eq!(amount.used, Some(5.0));
    assert_eq!(amount.limit, Some(20.0));
    assert_eq!(amount.remaining, Some(15.0));
    assert_eq!(amount.used_fraction, Some(0.25));
    assert_eq!(amount.unit, UsageUnit::Usd);
}

#[test]
fn cents_bucket_derives_used_from_remaining() {
    let value = json!({ "used": 0, "remaining": 1500, "limit": 2000 });
    let amount = parse_cents_bucket(obj(&value)).unwrap();
    assert_eq!(amount.used, Some(5.0));
}

#[test]
fn cents_bucket_without_limit_is_used_only() {
    let value = json!({ "used": 300, "limit": null });
    let amount = parse_cents_bucket(obj(&value)).unwrap();
    assert_eq!(amount.used, Some(3.0));
    assert_eq!(amount.limit, None);
    assert_eq!(amount.used_fraction, None);
}

#[test]
fn cents_bucket_rejects_disabled_and_non_positive_limits() {
    assert!(
        parse_cents_bucket(obj(&json!({ "enabled": false, "used": 1, "limit": 10 }))).is_none()
    );
    assert!(parse_cents_bucket(obj(&json!({ "used": 1, "limit": 0 }))).is_none());
    assert!(parse_cents_bucket(obj(&json!({ "used": 1, "limit": "x" }))).is_none());
}

#[test]
fn timestamps_seconds_ms_and_iso() {
    assert_eq!(
        parse_timestamp(&json!(1_700_000_000)),
        Some(1_700_000_000_000)
    );
    assert_eq!(
        parse_timestamp(&json!(1_700_000_000_000_i64)),
        Some(1_700_000_000_000)
    );
    let iso = parse_iso_ms("2026-05-01T00:00:00Z").unwrap();
    assert_eq!(parse_timestamp(&json!("2026-05-01T00:00:00Z")), Some(iso));
}

#[test]
fn reset_from_end_key_then_start_key() {
    let end = json!({ "billingCycleEnd": "2026-05-01T00:00:00Z" });
    let expected = parse_iso_ms("2026-05-01T00:00:00Z").unwrap() as u64;
    assert_eq!(derive_resets_at(obj(&end)), Some(expected));

    let start = json!({ "startOfMonth": "2026-04-01T00:00:00Z" });
    let expected = add_one_month_ms(parse_iso_ms("2026-04-01T00:00:00Z").unwrap()).unwrap() as u64;
    assert_eq!(derive_resets_at(obj(&start)), Some(expected));
}

#[test]
fn plan_rails_use_dashboard_percentages() {
    let summary = json!({
        "billingCycleEnd": "2026-05-01T00:00:00Z",
        "individualUsage": {
            "plan": { "enabled": true, "limit": 2000, "autoPercentUsed": 40, "apiPercentUsed": 10 }
        }
    });
    let report = parse_cursor_individual_usage(&summary, 1).unwrap();
    assert_eq!(report.provider, "cursor");
    assert_eq!(report.limits.len(), 2);
    let auto = report
        .limits
        .iter()
        .find(|l| l.id == "cursor:usd:individual-auto")
        .unwrap();
    assert_eq!(auto.label, "Cursor Models");
    assert_eq!(auto.amount.unit, UsageUnit::Percent);
    assert_eq!(auto.amount.used, Some(40.0));
    assert_eq!(auto.amount.used_fraction, Some(0.4));
    assert_eq!(auto.status, Some(usage_status_for(0.4)));
    let api = report
        .limits
        .iter()
        .find(|l| l.id == "cursor:usd:individual-api")
        .unwrap();
    assert_eq!(api.label, "Other Models");
    assert_eq!(api.amount.unit, UsageUnit::Usd);
    assert!((api.amount.used.unwrap() - 2.0).abs() < 1e-9);
    assert!((api.amount.limit.unwrap() - 20.0).abs() < 1e-9);
    assert_eq!(auto.window.as_ref().unwrap().id, "monthly");
    assert_eq!(
        auto.window.as_ref().unwrap().resets_at_ms,
        Some(parse_iso_ms("2026-05-01T00:00:00Z").unwrap() as u64)
    );
}

#[test]
fn overall_bucket_wins_over_plan_and_ondemand_is_kept() {
    let summary = json!({
        "individualUsage": {
            "overall": { "used": 1000, "limit": 5000 },
            "plan": { "limit": 2000, "autoPercentUsed": 90 },
            "onDemand": { "used": 0, "limit": 1000 }
        }
    });
    let report = parse_cursor_individual_usage(&summary, 1).unwrap();
    let ids: Vec<&str> = report.limits.iter().map(|l| l.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "cursor:usd:individual-overall",
            "cursor:usd:individual-ondemand"
        ]
    );
}

#[test]
fn disabled_overall_falls_through_to_plan_and_ondemand_needs_limit() {
    let summary = json!({
        "individualUsage": {
            "overall": { "enabled": false, "used": 1000, "limit": 5000 },
            "plan": { "limit": 2000, "totalPercentUsed": 25 },
            "onDemand": { "used": 50 }
        }
    });
    let report = parse_cursor_individual_usage(&summary, 1).unwrap();
    assert_eq!(report.limits.len(), 1);
    assert_eq!(report.limits[0].id, "cursor:usd:individual-plan");
    assert_eq!(report.limits[0].label, "Personal Usage");
    assert!((report.limits[0].amount.used_fraction.unwrap() - 0.25).abs() < 1e-9);
}

#[test]
fn summary_without_individual_usage_is_none() {
    assert!(parse_cursor_individual_usage(&json!({ "other": 1 }), 1).is_none());
    assert!(parse_cursor_individual_usage(&json!({ "individualUsage": {} }), 1).is_none());
}

#[test]
fn legacy_buckets_keep_uncapped_with_usage_and_map_units() {
    let payload = json!({
        "gpt-4": { "numRequests": 0, "maxRequestUsage": null },
        "premium": { "numRequests": 5, "maxRequestUsage": 50 },
        "planUsage": { "usdUsed": 300, "usdLimit": 2000 },
        "ignored": 3
    });
    let report = parse_cursor_usage(&payload, 1).unwrap();
    let find = |id: &str| report.limits.iter().find(|l| l.id == id).unwrap();

    let gpt = find("cursor:requests:gpt-4");
    assert_eq!(gpt.amount.limit, None);
    assert_eq!(gpt.amount.used, Some(0.0));
    assert_eq!(gpt.status, None);

    let premium = find("cursor:requests:premium");
    assert_eq!(premium.amount.used_fraction, Some(0.1));
    assert_eq!(premium.amount.unit, UsageUnit::Requests);
    assert_eq!(premium.label, "premium requests");

    let plan = find("cursor:usd:planusage");
    assert_eq!(plan.label, "planUsage spend");
    assert_eq!(plan.amount.unit, UsageUnit::Usd);
    assert_eq!(plan.amount.remaining, Some(1700.0));
    assert_eq!(report.limits.len(), 3);
}

#[test]
fn legacy_without_usable_buckets_is_none() {
    assert!(parse_cursor_usage(&json!({ "a": { "limit": 5 } }), 1).is_none());
    assert!(parse_cursor_usage(&json!([1, 2]), 1).is_none());
}

#[test]
fn combine_drops_zero_uncapped_legacy_buckets_when_summary_exists() {
    let legacy = parse_cursor_usage(
        &json!({
            "gpt-4": { "numRequests": 0, "maxRequestUsage": null },
            "premium": { "numRequests": 5, "maxRequestUsage": null }
        }),
        1,
    );
    let summary = parse_cursor_individual_usage(
        &json!({ "individualUsage": { "overall": { "used": 100, "limit": 1000 } } }),
        1,
    );
    let merged = combine(legacy.clone(), summary).unwrap();
    let ids: Vec<&str> = merged.limits.iter().map(|l| l.id.as_str()).collect();
    assert!(!ids.contains(&"cursor:requests:gpt-4"));
    assert!(ids.contains(&"cursor:requests:premium"));
    assert!(ids.contains(&"cursor:usd:individual-overall"));

    assert_eq!(combine(legacy.clone(), None), legacy);
    assert!(combine(None, None).is_none());
}

#[test]
fn session_cookie_encodes_user_and_token() {
    assert_eq!(
        session_cookie("u1", "a+b"),
        "WorkosCursorSessionToken=u1%3A%3Aa%2Bb"
    );
}

#[test]
fn no_stored_credentials_yields_none() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    assert!(fetch(dir.path(), &cancel).unwrap().is_none());
}

use std::sync::atomic::AtomicBool;

use serde_json::json;

use super::*;

#[test]
fn iso_timestamps_parse_with_zone() {
    assert_eq!(parse_iso_ms("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(
        parse_iso_ms("2026-01-01T00:00:00.500+01:00"),
        Some(1_767_222_000_500)
    );
    assert_eq!(parse_iso_ms("2026-01-01T00:00:00"), None);
    assert_eq!(parse_iso_ms("not a date"), None);
}

#[test]
fn usage_payload_maps_windows_scoped_limits_and_spend() {
    let payload = json!({
        "five_hour": { "utilization": 42.5, "resets_at": "1970-01-01T05:00:00Z" },
        "seven_day": { "utilization": 95, "resets_at": "1970-01-08T00:00:00Z" },
        "seven_day_opus": { "utilization": 10 },
        "limits": [{
            "kind": "weekly_scoped",
            "percent": 30,
            "scope": { "model": { "display_name": "Claude Design" } }
        }],
        "spend": {
            "enabled": true,
            "limit": { "amount_minor": 10000, "exponent": 2, "currency": "USD" },
            "used": { "amount_minor": 2550, "exponent": 2, "currency": "USD" }
        }
    });
    let report = parse_usage(&payload, 1).unwrap();
    let ids: Vec<&str> = report
        .limits
        .iter()
        .map(|limit| limit.id.as_str())
        .collect();
    assert_eq!(
        ids,
        [
            "anthropic:5h",
            "anthropic:7d",
            "anthropic:7d:opus",
            "anthropic:7d:claude-design",
            "anthropic:extra",
        ]
    );
    let five = &report.limits[0];
    assert_eq!(five.amount.used, Some(42.5));
    assert_eq!(five.status, Some(UsageStatus::Ok));
    assert_eq!(five.window.as_ref().unwrap().resets_at_ms, Some(18_000_000));
    assert_eq!(report.limits[1].status, Some(UsageStatus::Warning));
    assert_eq!(
        report.limits[3].scope.tier.as_deref(),
        Some("claude-design")
    );
    assert_eq!(report.limits[3].label, "Claude 7 Day (Claude Design)");
    let extra = &report.limits[4];
    assert_eq!(extra.amount.limit, Some(100.0));
    assert_eq!(extra.amount.used_fraction, Some(0.255));
    assert_eq!(extra.status, Some(UsageStatus::Ok));
}

#[test]
fn legacy_extra_usage_and_used_only_spend() {
    let legacy = json!({
        "five_hour": { "utilization": 1 },
        "extra_usage": { "is_enabled": true, "monthly_limit": 5000, "used_credits": 1250, "decimal_places": 2 }
    });
    let extra = parse_usage(&legacy, 1).unwrap().limits.pop().unwrap();
    assert_eq!(extra.id, "anthropic:extra");
    assert_eq!(extra.amount.used, Some(12.5));
    assert_eq!(extra.amount.limit, Some(50.0));

    let used_only = json!({
        "five_hour": { "utilization": 1 },
        "spend": {
            "enabled": true,
            "limit": null,
            "used": { "amount_minor": 700, "exponent": 2, "currency": "USD" }
        }
    });
    let extra = parse_usage(&used_only, 1).unwrap().limits.pop().unwrap();
    assert_eq!(extra.amount.used, Some(7.0));
    assert_eq!(extra.amount.limit, None);
    assert_eq!(extra.status, None);
}

#[test]
fn non_usd_spend_is_rejected() {
    let payload = json!({
        "five_hour": { "utilization": 1 },
        "spend": {
            "enabled": true,
            "limit": null,
            "used": { "amount_minor": 700, "exponent": 2, "currency": "EUR" }
        }
    });
    let report = parse_usage(&payload, 1).unwrap();
    assert!(
        report
            .limits
            .iter()
            .all(|limit| limit.id != "anthropic:extra")
    );
}

#[test]
fn cedar_reset_credits_parse_with_normalized_ids() {
    let payload = json!({
        "cedar_ember": {
            "eligible": true,
            "next_grant_id": "g1",
            "grants": [{
                "id": "g1",
                "resets_left": 2,
                "label": "Weekly",
                "starts_at": "1970-01-01T00:00:00Z",
                "ends_at": "2099-01-01T00:00:00Z",
                "clears": ["five_hour"],
                "blocking": ["seven_day_opus"],
                "percent_used": { "five_hour": 80 },
                "usable_now": true
            }]
        }
    });
    let credits = parse_reset_credits(&payload, 1_000).unwrap();
    assert_eq!(credits.available_count, 2);
    assert_eq!(credits.redeemable_count, Some(2));
    assert_eq!(credits.next_credit_id.as_deref(), Some("g1"));
    assert_eq!(credits.eligible, Some(true));
    let grant = &credits.credits[0];
    assert_eq!(grant.title.as_deref(), Some("Weekly"));
    assert_eq!(grant.usable, Some(true));
    assert_eq!(grant.clears, ["anthropic:5h"]);
    assert_eq!(grant.blocking, ["anthropic:7d:opus"]);
    assert_eq!(grant.used_fractions.get("anthropic:5h"), Some(&0.8));
}

#[test]
fn juniper_cooldown_reports_next_available_time() {
    let payload = json!({
        "juniper_tide": {
            "eligible": true,
            "arm": "reset",
            "available": false,
            "next_available_at": "2099-01-01T00:00:00Z"
        }
    });
    let credits = parse_reset_credits(&payload, 1_000).unwrap();
    assert_eq!(credits.available_count, 0);
    assert_eq!(credits.eligible, Some(true));
    assert_eq!(
        credits.cooldown_until.as_deref(),
        Some("2099-01-01T00:00:00Z")
    );
    assert_eq!(credits.credits[0].usable, Some(false));
}

#[test]
fn identity_and_profile_fields_extract() {
    let usage = json!({ "account": { "uuid": "u-1", "email": "a@example.com" } });
    let identity = usage_identity(&usage);
    assert_eq!(identity.account_id.as_deref(), Some("u-1"));
    assert_eq!(identity.email.as_deref(), Some("a@example.com"));

    let profile = json!({ "uuid": " u-2 ", "organization": { "uuid": "org_1" } });
    let identity = profile_identity(&profile);
    assert_eq!(identity.account_id.as_deref(), Some("u-2"));
    assert_eq!(identity.org_id.as_deref(), Some("org_1"));
    assert_eq!(
        profile_identity(&json!({ "organization": { "uuid": "bad id!" } })).org_id,
        None
    );
}

#[test]
fn fetch_without_credentials_is_none() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    assert!(fetch(dir.path(), &cancel).unwrap().is_none());
}

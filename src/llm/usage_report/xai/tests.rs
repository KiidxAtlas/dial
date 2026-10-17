use super::*;
use serde_json::json;

/// 2026-05-01T00:00:00Z
const NOW: u64 = 1_777_593_600_000;

fn weekly_payload() -> Value {
    json!({ "config": {
        "currentPeriod": {
            "start": "2026-04-27T00:00:00Z",
            "end": "2026-05-04T00:00:00Z",
            "type": "SUPERGROK_WEEKLY"
        },
        "creditUsagePercent": 42,
        "productUsage": [
            { "product": "GrokBuild", "usagePercent": 30 },
            { "product": "Api", "usagePercent": null },
            { "product": "  ", "usagePercent": 5 },
            { "product": "Chat", "usagePercent": "bad" }
        ],
        "onDemandCap": { "val": 500 },
        "onDemandUsed": { "val": 125 }
    }})
}

fn monthly_payload(used: f64, limit: f64) -> Value {
    json!({ "config": {
        "billingPeriodStart": "2026-04-01T00:00:00Z",
        "billingPeriodEnd": "2026-05-01T00:00:00Z",
        "monthlyLimit": { "val": limit },
        "used": { "val": used }
    }})
}

/// Unified credits payload with no creditUsagePercent: inferred 0% while the period is active.
fn unified_inferred_credits() -> Value {
    json!({ "config": {
        "isUnifiedBillingUser": true,
        "currentPeriod": {
            "start": "2026-04-27T00:00:00Z",
            "end": "2026-05-04T00:00:00Z",
            "type": "WEEK"
        }
    }})
}

#[test]
fn weekly_credits_map_overall_products_and_on_demand() {
    let payload = weekly_payload();
    let report = assemble(Some(&payload), None, Some("acct-1"), NOW).unwrap();
    assert_eq!(report.provider, PROVIDER);
    assert_eq!(report.fetched_at_ms, NOW);
    let ids: Vec<&str> = report.limits.iter().map(|l| l.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "xai-oauth:credits:1w",
            "xai-oauth:product:grokbuild:1w",
            "xai-oauth:product:api:1w",
            "xai-oauth:on-demand",
        ]
    );

    let overall = &report.limits[0];
    assert_eq!(overall.label, "SuperGrok Weekly Credits");
    assert_eq!(overall.amount.used, Some(42.0));
    assert_eq!(overall.amount.limit, Some(100.0));
    assert_eq!(overall.amount.used_fraction, Some(0.42));
    assert_eq!(overall.status, Some(UsageStatus::Ok));
    assert_eq!(overall.scope.account_id.as_deref(), Some("acct-1"));
    assert_eq!(overall.scope.shared, Some(true));
    assert_eq!(overall.scope.window_id.as_deref(), Some("1w"));
    let window = overall.window.as_ref().unwrap();
    assert_eq!(window.id, "1w");
    assert_eq!(window.duration_ms, Some(WEEK_MS));
    assert_eq!(window.resets_at_ms, Some(NOW + 3 * DAY_MS));

    let build = &report.limits[1];
    assert_eq!(build.label, "Grok Build (Weekly)");
    assert_eq!(build.amount.used, Some(30.0));
    let api = &report.limits[2];
    assert_eq!(api.label, "API (Weekly)");
    assert_eq!(api.amount.used, Some(0.0));

    let on_demand = &report.limits[3];
    assert_eq!(on_demand.amount.used, Some(125.0));
    assert_eq!(on_demand.amount.limit, Some(500.0));
    assert_eq!(on_demand.amount.used_fraction, Some(0.25));
    assert_eq!(on_demand.amount.remaining, Some(375.0));
    assert!(matches!(on_demand.amount.unit, UsageUnit::Unknown));
    assert_eq!(on_demand.scope.window_id, None);
    assert_eq!(report.metadata["billingKind"], "weekly");
    assert_eq!(report.metadata["endpoint"], CREDITS_URL);
    assert_eq!(report.metadata["source"], SOURCE);
    assert_eq!(report.metadata["accountId"], "acct-1");
}

#[test]
fn monthly_included_quota_uses_observed_period_length() {
    let payload = monthly_payload(250.0, 1000.0);
    let report = assemble(None, Some(&payload), None, NOW).unwrap();
    assert_eq!(report.limits.len(), 1);
    let limit = &report.limits[0];
    assert_eq!(limit.id, "xai-oauth:included:1mo");
    assert_eq!(limit.label, "SuperGrok Monthly Included");
    let window = limit.window.as_ref().unwrap();
    assert_eq!(window.label, "Monthly");
    assert_eq!(window.duration_ms, Some(30 * DAY_MS));
    assert_eq!(window.resets_at_ms, Some(NOW));
    assert_eq!(limit.amount.used, Some(250.0));
    assert_eq!(limit.amount.limit, Some(1000.0));
    assert_eq!(limit.amount.used_fraction, Some(0.25));
    assert_eq!(limit.amount.remaining, Some(750.0));
    assert!(matches!(limit.amount.unit, UsageUnit::Unknown));
    assert_eq!(limit.status, Some(UsageStatus::Ok));
    assert_eq!(report.metadata["billingKind"], "monthly");
}

#[test]
fn exhausted_monthly_quota_reports_exhausted_status() {
    let payload = monthly_payload(1000.0, 1000.0);
    let report = assemble(None, Some(&payload), None, NOW).unwrap();
    assert_eq!(report.limits[0].amount.used_fraction, Some(1.0));
    assert_eq!(report.limits[0].status, Some(UsageStatus::Exhausted));
}

#[test]
fn over_limit_monthly_beside_inferred_unified_credits_is_advisory() {
    let credits = unified_inferred_credits();
    let monthly = monthly_payload(1000.0, 1000.0);
    let report = assemble(Some(&credits), Some(&monthly), None, NOW).unwrap();
    // Monthly wins: weekly is dropped and the monthly counter stays visible as unknown.
    assert_eq!(report.limits.len(), 1);
    let limit = &report.limits[0];
    assert_eq!(limit.id, "xai-oauth:included:1mo");
    assert_eq!(limit.status, Some(UsageStatus::Unknown));
    assert_eq!(limit.notes.len(), 1);
    assert!(limit.notes[0].contains("enforcement uncertain"));
    assert_eq!(report.metadata["monthlyQuotaAdvisory"], "true");
    assert_eq!(report.metadata["billingKind"], "monthly");
}

#[test]
fn inferred_unified_credits_confirmed_zero_monthly_keeps_weekly() {
    let credits = unified_inferred_credits();
    let monthly = json!({ "config": { "monthlyLimit": { "val": 0 } } });
    let report = assemble(Some(&credits), Some(&monthly), None, NOW).unwrap();
    assert!(report.limits.iter().any(|l| l.id == "xai-oauth:credits:1w"));
    assert_eq!(report.limits[0].amount.used, Some(0.0));
    assert_eq!(report.metadata["billingKind"], "weekly");
    assert!(!report.metadata.contains_key("monthlyQuotaAdvisory"));
}

#[test]
fn inferred_unified_credits_without_monthly_confirmation_are_rejected() {
    let credits = unified_inferred_credits();
    assert!(assemble(Some(&credits), None, None, NOW).is_none());
}

#[test]
fn expired_or_non_weekly_periods_yield_no_report() {
    let expired = json!({ "config": {
        "currentPeriod": {
            "start": "2026-04-01T00:00:00Z",
            "end": "2026-04-08T00:00:00Z",
            "type": "WEEK"
        }
    }});
    assert!(assemble(Some(&expired), None, None, NOW).is_none());

    let monthly_period = json!({ "config": {
        "currentPeriod": {
            "start": "2026-04-27T00:00:00Z",
            "end": "2026-05-04T00:00:00Z",
            "type": "MONTH"
        },
        "creditUsagePercent": 10
    }});
    assert!(assemble(Some(&monthly_period), None, None, NOW).is_none());
}

#[test]
fn malformed_billing_payloads_yield_no_report() {
    assert!(assemble(Some(&json!({ "x": 1 })), None, None, NOW).is_none());

    let bad_products = json!({ "config": {
        "currentPeriod": {
            "start": "2026-04-27T00:00:00Z",
            "end": "2026-05-04T00:00:00Z",
            "type": "WEEK"
        },
        "creditUsagePercent": 5,
        "productUsage": "nope"
    }});
    assert!(assemble(Some(&bad_products), None, None, NOW).is_none());

    let zero_limit = monthly_payload(0.0, 0.0);
    assert!(assemble(None, Some(&zero_limit), None, NOW).is_none());

    let out_of_range = json!({ "config": {
        "currentPeriod": {
            "start": "2026-04-27T00:00:00Z",
            "end": "2026-05-04T00:00:00Z",
            "type": "WEEK"
        },
        "creditUsagePercent": 150
    }});
    assert!(assemble(Some(&out_of_range), None, None, NOW).is_none());
}

#[test]
fn slugs_and_endpoints_are_stable() {
    assert_eq!(slugify("GrokBuild"), "grokbuild");
    assert_eq!(slugify(" Grok Build Pro!! "), "grok-build-pro");
    assert!(CREDITS_URL.ends_with("/v1/billing?format=credits"));
    assert_eq!(MONTHLY_URL, "https://cli-chat-proxy.grok.com/v1/billing");
}

#[test]
fn fetch_without_stored_grant_is_none() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    assert!(fetch(dir.path(), &cancel).unwrap().is_none());
}

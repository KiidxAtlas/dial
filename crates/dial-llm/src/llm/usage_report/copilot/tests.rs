use super::*;
use serde_json::json;

#[test]
fn api_base_maps_enterprise_hosts() {
    assert_eq!(api_base(None), "https://api.github.com");
    assert_eq!(api_base(Some("  ")), "https://api.github.com");
    assert_eq!(
        api_base(Some("https://ghe.example.com/")),
        "https://ghe.example.com"
    );
    assert_eq!(
        api_base(Some("api.ghe.example.com")),
        "https://api.ghe.example.com"
    );
    assert_eq!(
        api_base(Some("ghe.example.com")),
        "https://api.ghe.example.com"
    );
}

#[test]
fn parses_premium_quota_with_monthly_reset() {
    let value = json!({
        "copilot_plan": "individual",
        "quota_reset_date": "2026-06-01",
        "quota_snapshots": {
            "premium_interactions": {
                "entitlement": 300,
                "remaining": 210,
                "unlimited": false,
                "overage_count": 0,
                "overage_permitted": false
            },
            "chat": {"entitlement": 0, "remaining": 0, "unlimited": true},
            "completions": {"entitlement": 0, "remaining": 0, "unlimited": true}
        }
    });
    let report = parse_quota(&value, 5).unwrap();
    assert_eq!(report.provider, "copilot");
    assert_eq!(report.fetched_at_ms, 5);
    assert_eq!(report.metadata["plan"], "individual");
    assert_eq!(report.metadata["quotaResetDate"], "2026-06-01");
    assert_eq!(report.limits.len(), 1);
    let premium = &report.limits[0];
    assert_eq!(premium.id, "copilot:premium");
    assert_eq!(premium.label, "Premium Requests");
    assert_eq!(premium.scope.tier.as_deref(), Some("individual"));
    assert_eq!(premium.scope.window_id.as_deref(), Some("monthly"));
    assert_eq!(premium.amount.used, Some(90.0));
    assert_eq!(premium.amount.limit, Some(300.0));
    assert_eq!(premium.amount.remaining, Some(210.0));
    assert!(matches!(premium.amount.unit, UsageUnit::Requests));
    assert_eq!(
        premium.window.as_ref().unwrap().resets_at_ms,
        Some(1_780_272_000_000)
    );
    assert!(matches!(premium.status, Some(UsageStatus::Ok)));
}

#[test]
fn metered_chat_is_included_and_unlimited_is_not() {
    let value = json!({
        "quota_snapshots": {
            "premium_interactions": {"entitlement": 10, "remaining": 10, "unlimited": false},
            "chat": {"entitlement": 100, "remaining": 50, "unlimited": false, "overage_count": 3},
            "completions": {"entitlement": 0, "remaining": 0, "unlimited": true}
        }
    });
    let report = parse_quota(&value, 0).unwrap();
    let ids: Vec<&str> = report.limits.iter().map(|l| l.id.as_str()).collect();
    assert_eq!(ids, ["copilot:premium", "copilot:chat"]);
    let chat = &report.limits[1];
    assert_eq!(chat.amount.used, Some(50.0));
    assert_eq!(chat.amount.used_fraction, Some(0.5));
    assert_eq!(chat.notes, ["Overage requests: 3"]);
    assert!(chat.window.is_none());
}

#[test]
fn status_bands_follow_remaining_fraction() {
    let snapshot = |remaining: f64| Snapshot {
        entitlement: 300.0,
        remaining,
        unlimited: false,
        overage_count: 0.0,
    };
    let status = |remaining: f64| {
        quota_limit(
            "copilot:premium",
            "Premium Requests",
            &snapshot(remaining),
            None,
            None,
        )
        .status
    };
    assert!(matches!(status(0.0), Some(UsageStatus::Exhausted)));
    assert!(matches!(status(30.0), Some(UsageStatus::Warning)));
    assert!(matches!(status(200.0), Some(UsageStatus::Ok)));
    assert!(matches!(status(-5.0), Some(UsageStatus::Exhausted)));

    let zero_entitlement = Snapshot {
        entitlement: 0.0,
        remaining: 0.0,
        unlimited: false,
        overage_count: 0.0,
    };
    let limit = quota_limit(
        "copilot:chat",
        "Chat Requests",
        &zero_entitlement,
        None,
        None,
    );
    assert!(matches!(limit.status, Some(UsageStatus::Unknown)));
    assert!(limit.amount.remaining_fraction.is_none());
}

#[test]
fn unlimited_premium_reports_no_usage() {
    let value = json!({
        "quota_snapshots": {
            "premium_interactions": {"entitlement": 0, "remaining": 0, "unlimited": true}
        }
    });
    let report = parse_quota(&value, 0).unwrap();
    let premium = &report.limits[0];
    assert!(premium.amount.used.is_none());
    assert!(matches!(premium.status, Some(UsageStatus::Ok)));
    assert_eq!(premium.notes, ["Unlimited"]);
}

#[test]
fn payload_without_snapshots_is_rejected() {
    assert!(parse_quota(&json!({"copilot_plan": "business"}), 0).is_none());
    assert!(parse_quota(&json!([]), 0).is_none());
}

#[test]
fn signed_out_returns_none() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    assert!(fetch(dir.path(), &cancel).unwrap().is_none());
}

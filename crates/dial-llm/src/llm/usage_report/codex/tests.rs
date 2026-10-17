use super::*;
use serde_json::json;

const NOW: u64 = 1_000_000;

fn payload() -> Value {
    json!({
        "plan_type": "plus",
        "rate_limit": {
            "allowed": true,
            "limit_reached": false,
            "primary_window": {
                "used_percent": 42,
                "limit_window_seconds": 18000,
                "reset_after_seconds": 600
            },
            "secondary_window": {
                "used_percent": 10,
                "limit_window_seconds": 604800,
                "reset_at": 1767225600
            }
        },
        "additional_rate_limits": [{
            "limit_name": "GPT-5.3-Codex-Spark",
            "metered_feature": "codex_bengalfox",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 5,
                    "limit_window_seconds": 18000,
                    "reset_after_seconds": 60
                }
            }
        }],
        "credits": {"has_credits": false, "unlimited": false, "overage_limit_reached": false},
        "spend_control": {"reached": false}
    })
}

#[test]
fn parses_plan_and_additional_windows() {
    let report = parse_usage(&payload(), NOW).unwrap();
    assert_eq!(report.provider, "openai-codex");
    assert_eq!(report.metadata["planType"], "plus");
    let ids: Vec<&str> = report.limits.iter().map(|l| l.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "openai-codex:primary",
            "openai-codex:secondary",
            "openai-codex:spark:primary"
        ]
    );
    let primary = &report.limits[0];
    assert_eq!(primary.label, "5 hours");
    assert_eq!(primary.window.as_ref().unwrap().id, "5h");
    assert_eq!(
        primary.window.as_ref().unwrap().duration_ms,
        Some(18_000_000)
    );
    assert_eq!(
        primary.window.as_ref().unwrap().resets_at_ms,
        Some(1_600_000)
    );
    assert_eq!(primary.amount.used_fraction, Some(0.42));
    assert!(matches!(primary.status, Some(UsageStatus::Ok)));
    assert_eq!(
        report.limits[1].window.as_ref().unwrap().resets_at_ms,
        Some(1_767_225_600_000)
    );
    assert_eq!(report.limits[1].label, "7 days");
    let spark = &report.limits[2];
    assert_eq!(spark.label, "5 hours (Spark)");
    assert_eq!(spark.scope.tier.as_deref(), Some("spark"));
    assert_eq!(spark.scope.model_id.as_deref(), Some("GPT-5.3-Codex-Spark"));
}

#[test]
fn full_plan_with_credit_overage_is_warning_not_exhausted() {
    let value = json!({
        "rate_limit": {
            "allowed": false,
            "limit_reached": true,
            "primary_window": {"used_percent": 100, "limit_window_seconds": 18000}
        },
        "credits": {"has_credits": true, "unlimited": false, "overage_limit_reached": false},
        "spend_control": {"reached": false}
    });
    let report = parse_usage(&value, NOW).unwrap();
    assert!(matches!(
        report.limits[0].status,
        Some(UsageStatus::Warning)
    ));

    let without_credits = json!({
        "rate_limit": {
            "allowed": false,
            "limit_reached": true,
            "primary_window": {"used_percent": 100, "limit_window_seconds": 18000}
        },
        "credits": {"has_credits": false, "unlimited": false}
    });
    let report = parse_usage(&without_credits, NOW).unwrap();
    assert!(matches!(
        report.limits[0].status,
        Some(UsageStatus::Exhausted)
    ));
    assert_eq!(report.metadata["planState"], "limitReached");
}

#[test]
fn unknown_window_duration_uses_key_label() {
    let value = json!({
        "rate_limit": {"primary_window": {"used_percent": 1}, "secondary_window": {}}
    });
    let report = parse_usage(&value, NOW).unwrap();
    assert_eq!(report.limits[0].label, "Primary window");
    let window = report.limits[0].window.as_ref().unwrap();
    assert_eq!(window.id, "primary");
    assert_eq!(window.duration_ms, None);
    assert_eq!(report.limits[1].label, "Secondary window");
    assert!(report.limits[1].amount.used.is_none());
}

#[test]
fn payload_without_rate_or_additional_limits_is_rejected() {
    assert!(parse_usage(&json!({"plan_type": "plus"}), NOW).is_none());
}

#[test]
fn reset_credits_keep_only_available_entries() {
    let list = json!({
        "available_count": 2,
        "credits": [
            {"id": "c1", "title": "Reset", "status": "available", "granted_at": "2026-01-01T00:00:00Z", "expires_at": "2026-02-01T00:00:00Z"},
            {"id": "c2", "status": "redeemed"},
            {"title": "missing id"},
            {"id": "c3"}
        ]
    });
    let credits = parse_reset_credits(&list, 9);
    assert_eq!(credits.available_count, 2);
    assert_eq!(credits.next_credit_id.as_deref(), Some("c1"));
    let ids: Vec<&str> = credits
        .credits
        .iter()
        .filter_map(|c| c.id.as_deref())
        .collect();
    assert_eq!(ids, ["c1", "c3"]);
    assert_eq!(credits.credits[0].title.as_deref(), Some("Reset"));
}

#[test]
fn reset_credit_count_is_truncated_and_clamped() {
    assert_eq!(
        reset_credit_count(&json!({"rate_limit_reset_credits": {"available_count": 2.9}})),
        2
    );
    assert_eq!(
        reset_credit_count(&json!({"rate_limit_reset_credits": {"available_count": -4}})),
        0
    );
    assert_eq!(reset_credit_count(&json!({})), 0);
}

#[test]
fn daybreak_requires_cyber_program_that_is_not_inactive_or_granted() {
    assert!(daybreak_enabled(&json!({
        "programs": [{"program": "cyber", "state": "active"}]
    })));
    assert!(!daybreak_enabled(&json!({
        "programs": [{"program": "cyber", "state": "inactive"}]
    })));
    assert!(daybreak_enabled(&json!({
        "programs": [{"program": "cyber", "state": "inactive", "grants": [{"id": "g"}]}]
    })));
    assert!(!daybreak_enabled(&json!({
        "programs": [{"program": "other", "state": "active"}]
    })));
}

#[test]
fn meter_identity_recognises_spark_and_slugs_other_meters() {
    assert_eq!(
        meter_identity(Some("GPT-5.3-Codex-Spark"), None),
        ("spark".to_owned(), "Spark".to_owned())
    );
    assert_eq!(
        meter_identity(Some("Deep Research"), Some("codex_deep__research")),
        ("deep-research".to_owned(), "Deep Research".to_owned())
    );
    assert_eq!(
        meter_identity(None, None),
        ("extra".to_owned(), "Extra".to_owned())
    );
}

#[test]
fn fetch_without_accounts_returns_none() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    assert!(fetch(dir.path(), &cancel).unwrap().is_none());
}

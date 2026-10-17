use super::*;
use serde_json::json;

fn quota_fixture() -> Value {
    json!({
        "buckets": [
            {
                "modelId": "gemini-2.5-pro",
                "remainingFraction": 0.75,
                "resetTime": "2026-01-02T03:04:05Z"
            },
            {
                "modelId": "gemini-3-flash-preview",
                "remainingFraction": 0,
                "resetTime": "2026-01-02T03:04:05.500+01:00"
            },
            { "remainingFraction": 1.5 }
        ]
    })
}

#[test]
fn parses_quota_buckets_with_fractions_tiers_and_resets() {
    let report = parse_quota(&quota_fixture(), ID, Some("acct-1"), Some("proj-1"), 42).unwrap();
    assert_eq!(report.provider, ID);
    assert_eq!(report.fetched_at_ms, 42);
    assert_eq!(report.limits.len(), 3);

    let pro = &report.limits[0];
    assert_eq!(pro.id, "gemini-2.5-pro:reset-1767323045000");
    assert_eq!(pro.label, "Gemini gemini-2.5-pro");
    assert_eq!(pro.scope.tier.as_deref(), Some("Pro"));
    assert_eq!(pro.scope.account_id.as_deref(), Some("acct-1"));
    assert_eq!(pro.scope.project_id.as_deref(), Some("proj-1"));
    assert_eq!(pro.scope.model_id.as_deref(), Some("gemini-2.5-pro"));
    assert_eq!(pro.scope.window_id.as_deref(), Some("reset-1767323045000"));
    let window = pro.window.as_ref().unwrap();
    assert_eq!(window.resets_at_ms, Some(1_767_323_045_000));
    assert_eq!(window.label, "Quota window");
    assert_eq!(pro.amount.unit, UsageUnit::Percent);
    assert_eq!(pro.amount.used, Some(25.0));
    assert_eq!(pro.amount.remaining, Some(75.0));
    assert_eq!(pro.amount.limit, Some(100.0));
    assert_eq!(pro.amount.used_fraction, Some(0.25));
    assert_eq!(pro.amount.remaining_fraction, Some(0.75));
    assert_eq!(pro.status, None);

    let exhausted = &report.limits[1];
    assert_eq!(exhausted.scope.tier.as_deref(), Some("3-Flash"));
    assert_eq!(
        exhausted.window.as_ref().unwrap().resets_at_ms,
        Some(1_767_319_445_500)
    );
    assert_eq!(exhausted.amount.used, Some(100.0));
    assert_eq!(exhausted.amount.remaining, Some(0.0));
    assert_eq!(exhausted.amount.used_fraction, Some(1.0));

    let unnamed = &report.limits[2];
    assert_eq!(unnamed.id, "unknown:quota");
    assert_eq!(unnamed.label, "Gemini quota");
    assert_eq!(unnamed.scope.tier, None);
    assert_eq!(unnamed.window.as_ref().unwrap().resets_at_ms, None);
    assert_eq!(unnamed.amount.used_fraction, Some(0.0));
    assert_eq!(unnamed.amount.used, Some(0.0));
}

#[test]
fn missing_fraction_yields_unknown_percent_amount() {
    let report = parse_quota(
        &json!({"buckets": [{"modelId": "gemini-2.5-flash"}]}),
        ID,
        None,
        None,
        0,
    )
    .unwrap();
    let amount = &report.limits[0].amount;
    assert_eq!(amount.unit, UsageUnit::Percent);
    assert_eq!(amount.used, None);
    assert_eq!(amount.remaining_fraction, None);
    assert_eq!(report.limits[0].scope.tier.as_deref(), Some("Flash"));
}

#[test]
fn missing_buckets_is_an_empty_report() {
    let report = parse_quota(&json!({}), ID, None, None, 0).unwrap();
    assert!(report.limits.is_empty());
}

#[test]
fn malformed_quota_payloads_are_errors() {
    assert!(parse_quota(&json!([]), ID, None, None, 0).is_err());
    assert!(parse_quota(&json!({"buckets": "nope"}), ID, None, None, 0).is_err());
    assert!(parse_quota(&json!({"buckets": [7]}), ID, None, None, 0).is_err());
    assert!(
        parse_quota(
            &json!({"buckets": [{"remainingFraction": "0.5"}]}),
            ID,
            None,
            None,
            0
        )
        .is_err()
    );
    assert!(parse_quota(&json!({"buckets": [{"modelId": 3}]}), ID, None, None, 0).is_err());
}

#[test]
fn remaining_fraction_is_clamped() {
    let report = parse_quota(
        &json!({"buckets": [{"modelId": "m", "remainingFraction": 1.5}]}),
        ID,
        None,
        None,
        0,
    )
    .unwrap();
    let amount = &report.limits[0].amount;
    assert_eq!(amount.remaining_fraction, Some(1.0));
    assert_eq!(amount.used_fraction, Some(0.0));
    assert_eq!(amount.remaining, Some(100.0));
}

#[test]
fn quota_tiers_follow_exact_members_then_substrings() {
    assert_eq!(quota_tier("gemini-3-flash-preview"), Some("3-Flash"));
    assert_eq!(quota_tier("gemini-2.5-flash-lite"), Some("Flash"));
    assert_eq!(quota_tier("gemini-3.1-pro-preview"), Some("Pro"));
    assert_eq!(quota_tier("gemini-9-flash-experimental"), Some("Flash"));
    assert_eq!(quota_tier("gemini-9-pro-experimental"), Some("Pro"));
    assert_eq!(quota_tier("embedding-001"), None);
}

#[test]
fn project_and_tier_metadata_are_read_from_load_response() {
    assert_eq!(
        project_from_load(&json!({"cloudaicompanionProject": "p-1"})),
        Some("p-1".to_owned())
    );
    assert_eq!(
        project_from_load(&json!({"cloudaicompanionProject": {"id": "p-2"}})),
        Some("p-2".to_owned())
    );
    assert_eq!(project_from_load(&json!({})), None);

    let load = json!({"currentTier": {"id": "standard-tier", "name": "Standard"}});
    let metadata = current_tier_metadata(Some(&load));
    assert_eq!(
        metadata.get("currentTierId").map(String::as_str),
        Some("standard-tier")
    );
    assert_eq!(
        metadata.get("currentTierName").map(String::as_str),
        Some("Standard")
    );
    assert!(current_tier_metadata(None).is_empty());
}

#[test]
fn iso_timestamps_parse_offsets_fractions_and_reject_garbage() {
    assert_eq!(parse_iso_ms("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(parse_iso_ms("1970-01-02T01:00:00+01:00"), Some(86_400_000));
    assert_eq!(parse_iso_ms("1970-01-02"), Some(86_400_000));
    assert_eq!(parse_iso_ms("1970-01-01T00:00:00.5Z"), Some(500));
    assert_eq!(parse_iso_ms("1970-01-01T00:00:00+0130"), Some(-5_400_000));
    assert_eq!(parse_iso_ms("2026-02-30T00:00:00Z"), None);
    assert_eq!(parse_iso_ms("not a date"), None);
    assert_eq!(parse_iso_ms("1970-01-01T00:00:00Zjunk"), None);
}

#[test]
fn no_stored_credentials_yields_none() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    assert!(fetch(dir.path(), &cancel).unwrap().is_none());
}

use super::*;

const NOW_MS: u64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z
const RESET_JAN2: &str = "2026-01-02T03:04:05Z";

fn ctx() -> Ctx<'static> {
    Ctx {
        provider: ID,
        account_id: Some("acct-1"),
        project_id: "proj-1",
    }
}

fn summary_fixture() -> Value {
    json!({
        "groups": [
            {
                "displayName": "Gemini",
                "buckets": [
                    {
                        "bucketId": "gemini-2.5-pro",
                        "window": "WEEKLY",
                        "remainingFraction": 0.25,
                        "resetTime": RESET_JAN2
                    }
                ]
            },
            {
                "displayName": "Claude & GPT",
                "buckets": [
                    {
                        "bucketId": "3p-shared",
                        "window": "5h",
                        "remainingFraction": 0,
                        "resetTime": "2026-01-01T00:00:00Z"
                    },
                    {
                        "bucketId": "3p-off",
                        "disabled": true,
                        "remainingFraction": 0.5
                    }
                ]
            }
        ]
    })
}

#[test]
fn summary_groups_split_shared_and_skip_disabled() {
    let report = parse_summary(&summary_fixture(), ctx(), "https://e", NOW_MS)
        .unwrap()
        .unwrap();
    assert_eq!(report.limits.len(), 3);
    // Sorted by remaining fraction: exhausted shared buckets first, then Gemini.
    assert_eq!(
        report.limits[0].id,
        "google-antigravity:anthropic:default:3p-shared"
    );
    assert_eq!(
        report.limits[1].id,
        "google-antigravity:openai:default:3p-shared"
    );
    assert_eq!(
        report.limits[2].id,
        "google-antigravity:google:default:gemini-2.5-pro"
    );

    let shared = &report.limits[0];
    assert_eq!(shared.label, "Claude & GPT (shared)");
    assert_eq!(shared.scope.shared, Some(true));
    assert_eq!(shared.scope.shared_group.as_deref(), Some("3p-shared:5h"));
    assert_eq!(shared.status, Some(UsageStatus::Exhausted));
    let window = shared.window.as_ref().unwrap();
    assert_eq!(window.id, "5h");
    assert_eq!(window.duration_ms, Some(5 * 3_600_000));
    assert_eq!(window.resets_at_ms, Some(1_767_225_600_000));

    let gemini = &report.limits[2];
    assert_eq!(gemini.label, "Gemini");
    assert_eq!(gemini.scope.shared, None);
    assert_eq!(gemini.scope.account_id.as_deref(), Some("acct-1"));
    assert_eq!(gemini.scope.project_id.as_deref(), Some("proj-1"));
    assert_eq!(gemini.amount.remaining, Some(25.0));
    assert_eq!(gemini.amount.used, Some(75.0));
    assert_eq!(gemini.window.as_ref().unwrap().id, "weekly");
    assert_eq!(gemini.status, Some(UsageStatus::Ok));
    assert_eq!(
        report.metadata.get("project_id").map(String::as_str),
        Some("proj-1")
    );
}

#[test]
fn summary_top_level_buckets_used_without_groups() {
    let payload = json!({
        "buckets": [
            { "bucketId": "gemini-flash", "displayName": "Flash", "remainingFraction": 0.05 }
        ]
    });
    let report = parse_summary(&payload, ctx(), "https://e", NOW_MS)
        .unwrap()
        .unwrap();
    assert_eq!(report.limits.len(), 1);
    assert_eq!(
        report.limits[0].id,
        "google-antigravity:google:default:gemini-flash"
    );
    assert_eq!(report.limits[0].status, Some(UsageStatus::Warning));
}

#[test]
fn summary_without_buckets_is_none() {
    let payload = json!({ "groups": [{ "displayName": "Gemini", "buckets": [] }] });
    assert!(
        parse_summary(&payload, ctx(), "https://e", NOW_MS)
            .unwrap()
            .is_none()
    );
}

#[test]
fn summary_remaining_amount_string_is_unknown_unit() {
    let payload = json!({
        "buckets": [
            { "bucketId": "gemini-x", "remainingAmount": "0" }
        ]
    });
    let report = parse_summary(&payload, ctx(), "https://e", NOW_MS)
        .unwrap()
        .unwrap();
    let limit = &report.limits[0];
    assert_eq!(limit.amount.unit, UsageUnit::Unknown);
    assert_eq!(limit.amount.remaining, Some(0.0));
    assert_eq!(limit.status, Some(UsageStatus::Exhausted));
}

#[test]
fn summary_rejects_non_array_buckets() {
    let payload = json!({ "buckets": "nope" });
    assert!(parse_summary(&payload, ctx(), "https://e", NOW_MS).is_err());
}

#[test]
fn legacy_models_dedupe_merge_and_filter_metered() {
    let payload = json!({
        "models": {
            "gemini-2.5-flash": {
                "apiProvider": "API_PROVIDER_GOOGLE_GEMINI",
                "quotaInfo": { "remainingFraction": 0.0, "resetTime": "2026-01-01T06:00:00Z" }
            },
            "gemini-2.5-pro": {
                "apiProvider": "API_PROVIDER_GOOGLE_GEMINI",
                "quotaInfo": { "remainingFraction": 0.5, "resetTime": RESET_JAN2 }
            },
            "gemini-3-flash": {
                "apiProvider": "API_PROVIDER_GOOGLE_GEMINI",
                "quotaInfo": { "remainingFraction": 0.2, "resetTime": RESET_JAN2 }
            },
            "gemini-tab": {
                "apiProvider": "API_PROVIDER_GOOGLE_GEMINI",
                "quotaInfo": { "remainingFraction": 1.0, "windowId": "5h" }
            }
        }
    });
    let report = parse_models(&payload, ctx(), "https://e", NOW_MS).unwrap();
    // The unmetered 5h entry sits under the metered google|default counter and is dropped.
    assert_eq!(report.limits.len(), 2);
    assert!(
        report
            .limits
            .iter()
            .all(|l| l.window.as_ref().unwrap().id != "5h")
    );

    let daily = &report.limits[0];
    assert_eq!(daily.id, "google-antigravity:google:default:daily");
    assert_eq!(daily.label, "Usage (Google)");
    assert_eq!(daily.amount.remaining, Some(0.0));
    assert_eq!(daily.status, Some(UsageStatus::Exhausted));

    let weekly = &report.limits[1];
    assert_eq!(weekly.id, "google-antigravity:google:default:weekly");
    // Min of merged 0.5 and 0.2 wins.
    assert_eq!(weekly.amount.remaining_fraction, Some(0.2));
    assert_eq!(weekly.scope.window_id.as_deref(), Some("weekly"));
    assert_eq!(weekly.scope.tier, None);
    assert_eq!(weekly.status, Some(UsageStatus::Ok));
}

#[test]
fn legacy_reset_without_fraction_is_zero() {
    let payload = json!({
        "models": {
            "claude-x": {
                "modelProvider": "MODEL_PROVIDER_ANTHROPIC",
                "quotaInfo": { "resetTime": "2026-01-01T06:00:00Z", "windowId": "daily" }
            }
        }
    });
    let report = parse_models(&payload, ctx(), "https://e", NOW_MS).unwrap();
    assert_eq!(report.limits.len(), 1);
    assert_eq!(report.limits[0].label, "Usage (Anthropic)");
    assert_eq!(report.limits[0].amount.remaining_fraction, Some(0.0));
    assert_eq!(report.limits[0].status, Some(UsageStatus::Exhausted));
}

#[test]
fn legacy_models_by_tier_and_window_keys() {
    let payload = json!({
        "models": {
            "m": {
                "apiProvider": "API_PROVIDER_OPENAI_VERTEX",
                "quotaInfoByTier": { "Pro": { "remainingFraction": 0.4, "resetTime": RESET_JAN2 } },
                "quotaInfoByWindow": { "5h": { "remainingFraction": 0.9 } }
            }
        }
    });
    let report = parse_models(&payload, ctx(), "https://e", NOW_MS).unwrap();
    let ids: Vec<&str> = report.limits.iter().map(|l| l.id.as_str()).collect();
    assert!(
        ids.contains(&"google-antigravity:openai:pro:weekly"),
        "{ids:?}"
    );
    assert!(
        ids.contains(&"google-antigravity:openai:default:5h"),
        "{ids:?}"
    );
}

#[test]
fn legacy_non_object_models_is_error() {
    assert!(parse_models(&json!({ "models": [] }), ctx(), "https://e", NOW_MS).is_err());
}

#[test]
fn legacy_non_numeric_fraction_is_error() {
    let payload = json!({
        "models": { "m": { "quotaInfo": { "remainingFraction": "0.5" } } }
    });
    assert!(parse_models(&payload, ctx(), "https://e", NOW_MS).is_err());
}

#[test]
fn fraction_is_clamped() {
    let amount = percent_amount(clamp_fraction(Some(-0.3)));
    assert_eq!(amount.remaining_fraction, Some(0.0));
    assert_eq!(amount.used_fraction, Some(1.0));
    let high = percent_amount(clamp_fraction(Some(2.0)));
    assert_eq!(high.remaining_fraction, Some(1.0));
}

#[test]
fn classify_window_maps_known_cadences() {
    assert_eq!(
        classify_window(Some("WINDOW_WEEKLY"), None).unwrap().id,
        "weekly"
    );
    assert_eq!(classify_window(Some("7d"), None).unwrap().id, "weekly");
    assert_eq!(classify_window(Some("7 days"), None).unwrap().id, "weekly");
    assert_eq!(classify_window(Some("5-hour"), None).unwrap().id, "5h");
    assert_eq!(classify_window(Some("Daily"), None).unwrap().id, "daily");
    assert_eq!(classify_window(Some("24h"), None).unwrap().id, "daily");
    let custom = classify_window(Some("custom"), Some("Custom Label")).unwrap();
    assert_eq!(custom.id, "custom");
    assert_eq!(custom.label, "Custom Label");
    assert_eq!(custom.duration_ms, None);
    assert!(classify_window(None, None).is_none());
}

#[test]
fn usage_status_thresholds() {
    assert_eq!(get_usage_status(None), UsageStatus::Unknown);
    assert_eq!(get_usage_status(Some(0.0)), UsageStatus::Exhausted);
    assert_eq!(get_usage_status(Some(0.1)), UsageStatus::Warning);
    assert_eq!(get_usage_status(Some(0.11)), UsageStatus::Ok);
}

#[test]
fn retry_classification() {
    assert!(is_transient(429));
    assert!(is_transient(503));
    assert!(!is_transient(404));
    assert!(!is_transient(200));
}

#[test]
fn fetch_without_credentials_is_none() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    assert!(fetch(dir.path(), &cancel).unwrap().is_none());
}

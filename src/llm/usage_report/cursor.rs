//! Cursor usage: the request-count API and the web usage-summary dashboard.
//!
//! Two sources are merged: the legacy `{base}/auth/usage` request-count API and the web
//! `usage-summary` dashboard API (cookie session, only for the default base URL with a user id).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};
use serde_json::{Map, Value};

use super::gemini::{add_one_month_ms, flow_base_url, parse_iso_ms, usage_status_for};
use super::{UsageAmount, UsageLimit, UsageReport, UsageScope, UsageUnit, UsageWindow};
use crate::llm::oauth::{self, util};

const ID: &str = "cursor";
const SUMMARY_URL: &str = "https://cursor.com/api/usage-summary";
const PROFILE_URL: &str = "https://cursor.com/api/auth/me";

pub(super) fn fetch(user_dir: &Path, cancel: &Arc<AtomicBool>) -> Result<Option<UsageReport>> {
    if !oauth::signed_in(user_dir, ID) {
        return Ok(None);
    }
    let access = oauth::access(user_dir, ID, cancel)?;
    let token = access.token.as_str();
    let default_base = flow_base_url(ID)?;
    let base = match access
        .base_url
        .as_deref()
        .map(|base| base.trim_end_matches('/'))
        .filter(|base| !base.is_empty())
    {
        Some(base) => base.to_owned(),
        None => default_base.clone(),
    };
    let now = util::now_ms();
    let bearer = format!("Bearer {token}");
    let legacy = fetch_json(
        &format!("{base}/auth/usage"),
        &[
            ("accept", "application/json"),
            ("authorization", bearer.as_str()),
        ],
        cancel,
    )?
    .and_then(|payload| parse_cursor_usage(&payload, now));

    let mut summary = None;
    let mut email = None;
    if base == default_base
        && let Some(user_id) = access.account_id.as_deref().filter(|id| !id.is_empty())
    {
        let cookie = session_cookie(user_id, token);
        summary = fetch_json(
            SUMMARY_URL,
            &[("accept", "application/json"), ("cookie", cookie.as_str())],
            cancel,
        )?
        .and_then(|payload| parse_cursor_individual_usage(&payload, now));
        email = profile_email(user_id, &cookie, cancel);
    }

    if cancel.load(Ordering::Relaxed) {
        bail!("Cursor usage fetch cancelled");
    }
    let Some(mut report) = combine(legacy, summary) else {
        bail!("Cursor usage returned no usable data");
    };
    if let Some(account) = email
        .or_else(|| oauth::identity(user_dir, ID))
        .or_else(|| access.account_id.clone())
    {
        report.metadata.insert("account".to_owned(), account);
    }
    Ok(Some(report))
}

/// GET a JSON body. Non-2xx and transport failures are tolerated (`None`);
/// cancellation is propagated.
fn fetch_json(
    url: &str,
    headers: &[(&str, &str)],
    cancel: &Arc<AtomicBool>,
) -> Result<Option<Value>> {
    match util::request("GET", url, headers, util::Body::Empty, cancel) {
        Ok(response) if response.ok() => Ok(response.json().ok()),
        Ok(_) => Ok(None),
        Err(error) if cancel.load(Ordering::Relaxed) => Err(error),
        Err(_) => Ok(None),
    }
}

/// Best-effort account email from the web session; mismatched `sub` yields `None`.
fn profile_email(user_id: &str, cookie: &str, cancel: &Arc<AtomicBool>) -> Option<String> {
    let response = util::request(
        "GET",
        PROFILE_URL,
        &[("accept", "application/json"), ("cookie", cookie)],
        util::Body::Empty,
        cancel,
    )
    .ok()?;
    if !response.ok() {
        return None;
    }
    let profile = response.json().ok()?;
    if profile["sub"].as_str() != Some(user_id) {
        return None;
    }
    profile["email"].as_str().map(str::to_owned)
}

/// Session cookie presenting the access token as the user's web session (`cursorSessionHeaders`).
fn session_cookie(user_id: &str, token: &str) -> String {
    format!(
        "WorkosCursorSessionToken={}",
        util::encode(&format!("{user_id}::{token}"))
    )
}

/// Merge the legacy and summary reports. Legacy uncapped buckets with no recorded usage are
/// dropped when a summary exists; legacy alone is the fallback.
fn combine(legacy: Option<UsageReport>, summary: Option<UsageReport>) -> Option<UsageReport> {
    match (legacy, summary) {
        (Some(mut legacy), Some(summary)) => {
            legacy.limits.retain(|limit| {
                limit.amount.limit.is_some() || limit.amount.used.unwrap_or(0.0) > 0.0
            });
            legacy.limits.extend(summary.limits);
            Some(legacy)
        }
        (legacy, summary) => legacy.or(summary),
    }
}

/// Number from a JSON value (`toNumber`): finite numbers and numeric strings.
fn to_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64().filter(|n| n.is_finite()),
        Value::String(text) => text.trim().parse::<f64>().ok().filter(|n| n.is_finite()),
        _ => None,
    }
}

fn field(object: &Map<String, Value>, key: &str) -> Option<f64> {
    object.get(key).and_then(to_number)
}

fn first_number(object: &Map<String, Value>, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| field(object, key))
}

/// Epoch seconds (below 1e12) are scaled to ms; ISO strings are parsed.
fn parse_timestamp(value: &Value) -> Option<i64> {
    if let Some(numeric) = to_number(value) {
        let ms = if numeric < 1_000_000_000_000.0 {
            numeric * 1000.0
        } else {
            numeric
        };
        return Some(ms as i64);
    }
    value.as_str().and_then(parse_iso_ms)
}

/// Reset time from end-of-cycle keys, else start-of-cycle plus one month.
fn derive_resets_at(payload: &Map<String, Value>) -> Option<u64> {
    const END_KEYS: [&str; 4] = ["billingCycleEnd", "endOfMonth", "resetsAt", "nextReset"];
    const START_KEYS: [&str; 3] = ["startOfMonth", "billingCycleStart", "startOfBillingCycle"];
    if let Some(ms) = END_KEYS
        .iter()
        .find_map(|key| payload.get(*key).and_then(parse_timestamp))
    {
        return u64::try_from(ms).ok();
    }
    START_KEYS
        .iter()
        .find_map(|key| payload.get(*key).and_then(parse_timestamp))
        .and_then(|ms| add_one_month_ms(ms).and_then(|ms| u64::try_from(ms).ok()))
}

fn monthly_window(resets_at_ms: Option<u64>) -> UsageWindow {
    UsageWindow {
        id: "monthly".to_owned(),
        label: "Monthly".to_owned(),
        duration_ms: None,
        resets_at_ms,
        reset_label: None,
    }
}

/// A USD-cents bucket (`used`/`limit`/`remaining`). `None` for disabled or unusable buckets.
fn parse_cents_bucket(bucket: &Map<String, Value>) -> Option<UsageAmount> {
    if bucket.get("enabled") == Some(&Value::Bool(false)) {
        return None;
    }
    let reported_used = field(bucket, "used");
    let reported_remaining = field(bucket, "remaining");
    let limit_value = bucket.get("limit");
    if limit_value.is_none_or(|value| value.is_null()) {
        let used = reported_used.filter(|used| *used >= 0.0)?;
        return Some(UsageAmount {
            used: Some(used / 100.0),
            limit: None,
            remaining: None,
            used_fraction: None,
            remaining_fraction: None,
            unit: UsageUnit::Usd,
        });
    }
    let limit = field(bucket, "limit").filter(|limit| *limit > 0.0)?;
    let used = match (reported_used, reported_remaining) {
        (Some(used), _) if used > 0.0 => used,
        (_, Some(remaining)) if remaining >= 0.0 && remaining < limit => {
            (limit - remaining).max(0.0)
        }
        (Some(used), _) if used >= 0.0 => used,
        _ => return None,
    };
    let remaining = (limit - used).max(0.0);
    Some(UsageAmount {
        used: Some(used / 100.0),
        limit: Some(limit / 100.0),
        remaining: Some(remaining / 100.0),
        used_fraction: Some(used / limit),
        remaining_fraction: Some(remaining / limit),
        unit: UsageUnit::Usd,
    })
}

#[derive(Default)]
struct PlanRails {
    auto: Option<UsageAmount>,
    api: Option<UsageAmount>,
    fallback: Option<UsageAmount>,
}

/// Dashboard quota pools for Pro+ plans. Percent fields win over cents; cents are a fallback
/// only when neither pool percent is present.
fn parse_plan_rails(bucket: &Map<String, Value>) -> PlanRails {
    if bucket.get("enabled") == Some(&Value::Bool(false)) {
        return PlanRails::default();
    }
    let limit_usd = field(bucket, "limit")
        .filter(|cents| *cents > 0.0)
        .map(|cents| cents / 100.0);
    let from_percent = |pct: f64, with_limit: bool| -> UsageAmount {
        let used_fraction = pct.max(0.0) / 100.0;
        match limit_usd {
            Some(limit) if with_limit => {
                let used = limit * used_fraction;
                UsageAmount {
                    used: Some(used),
                    limit: Some(limit),
                    remaining: Some((limit - used).max(0.0)),
                    used_fraction: Some(used_fraction),
                    remaining_fraction: Some((1.0 - used_fraction).max(0.0)),
                    unit: UsageUnit::Usd,
                }
            }
            _ => UsageAmount {
                used: Some(used_fraction * 100.0),
                limit: None,
                remaining: None,
                used_fraction: Some(used_fraction),
                remaining_fraction: None,
                unit: UsageUnit::Percent,
            },
        }
    };
    let auto = field(bucket, "autoPercentUsed").map(|pct| from_percent(pct, false));
    let api = field(bucket, "apiPercentUsed").map(|pct| from_percent(pct, true));
    let fallback = if auto.is_none() && api.is_none() {
        match field(bucket, "totalPercentUsed") {
            Some(total) => Some(from_percent(total, true)),
            None => parse_cents_bucket(bucket),
        }
    } else {
        None
    };
    PlanRails {
        auto,
        api,
        fallback,
    }
}

fn window_limit(id: &str, label: &str, window: &UsageWindow, amount: UsageAmount) -> UsageLimit {
    let status = amount.used_fraction.map(usage_status_for);
    UsageLimit {
        id: id.to_owned(),
        label: label.to_owned(),
        scope: UsageScope {
            provider: ID.to_owned(),
            window_id: Some(window.id.clone()),
            ..UsageScope::default()
        },
        window: Some(window.clone()),
        amount,
        status,
        notes: Vec::new(),
    }
}

fn push_plan_rails(
    limits: &mut Vec<UsageLimit>,
    bucket: &Map<String, Value>,
    window: &UsageWindow,
) {
    let rails = parse_plan_rails(bucket);
    if let Some(amount) = rails.auto {
        limits.push(window_limit(
            "cursor:usd:individual-auto",
            "Cursor Models",
            window,
            amount,
        ));
    }
    if let Some(amount) = rails.api {
        limits.push(window_limit(
            "cursor:usd:individual-api",
            "Other Models",
            window,
            amount,
        ));
    }
    if let Some(amount) = rails.fallback {
        limits.push(window_limit(
            "cursor:usd:individual-plan",
            "Personal Usage",
            window,
            amount,
        ));
    }
}

fn report(limits: Vec<UsageLimit>, fetched_at_ms: u64) -> Option<UsageReport> {
    if limits.is_empty() {
        return None;
    }
    Some(UsageReport {
        provider: ID.to_owned(),
        fetched_at_ms,
        limits,
        reset_credits: None,
        notes: Vec::new(),
        metadata: BTreeMap::new(),
    })
}

/// Parse `/api/usage-summary`. Prefers a usable overall bucket, else plan rails; on-demand is
/// kept whenever it has a positive limit.
fn parse_cursor_individual_usage(payload: &Value, fetched_at_ms: u64) -> Option<UsageReport> {
    let payload = payload.as_object()?;
    let individual = payload.get("individualUsage")?.as_object()?;
    let window = monthly_window(derive_resets_at(payload));
    let mut limits = Vec::new();

    let overall = individual
        .get("overall")
        .and_then(Value::as_object)
        .and_then(parse_cents_bucket);
    let plan = individual.get("plan").and_then(Value::as_object);
    match (overall, plan) {
        (Some(amount), _) => limits.push(window_limit(
            "cursor:usd:individual-overall",
            "Personal Usage",
            &window,
            amount,
        )),
        (None, Some(plan)) => push_plan_rails(&mut limits, plan, &window),
        (None, None) => {}
    }

    if let Some(on_demand) = individual.get("onDemand").and_then(Value::as_object)
        && let Some(amount) =
            parse_cents_bucket(on_demand).filter(|amount| amount.limit.is_some_and(|l| l > 0.0))
    {
        limits.push(window_limit(
            "cursor:usd:individual-ondemand",
            "On-Demand Usage",
            &window,
            amount,
        ));
    }

    report(limits, fetched_at_ms)
}

/// Parse the legacy `/auth/usage` map: each object bucket is a request or USD meter.
fn parse_cursor_usage(payload: &Value, fetched_at_ms: u64) -> Option<UsageReport> {
    let payload = payload.as_object()?;
    let window = monthly_window(derive_resets_at(payload));
    let mut limits = Vec::new();
    for (key, value) in payload {
        let Some(bucket) = value.as_object() else {
            continue;
        };
        let Some(used) = first_number(bucket, &["numRequests", "used", "amountUsed", "usdUsed"])
        else {
            continue;
        };
        let limit = first_number(
            bucket,
            &["maxRequestUsage", "limit", "amountLimit", "usdLimit"],
        );
        let lower = key.to_lowercase();
        let is_usd = key == "planUsage"
            || lower.contains("usd")
            || lower.contains("billing")
            || lower.contains("stripe");
        let unit = if is_usd {
            UsageUnit::Usd
        } else {
            UsageUnit::Requests
        };
        let clean = lower.trim();
        let id = if is_usd {
            format!("cursor:usd:{clean}")
        } else {
            format!("cursor:requests:{clean}")
        };
        let label = if is_usd {
            format!("{key} spend")
        } else {
            format!("{key} requests")
        };
        let amount = match limit {
            None => UsageAmount {
                used: Some(used),
                limit: None,
                remaining: None,
                used_fraction: None,
                remaining_fraction: None,
                unit,
            },
            Some(limit) => {
                let remaining = (limit - used).max(0.0);
                let (used_fraction, remaining_fraction) = if limit > 0.0 {
                    (used / limit, remaining / limit)
                } else {
                    (0.0, 0.0)
                };
                UsageAmount {
                    used: Some(used),
                    limit: Some(limit),
                    remaining: Some(remaining),
                    used_fraction: Some(used_fraction),
                    remaining_fraction: Some(remaining_fraction),
                    unit,
                }
            }
        };
        limits.push(window_limit(&id, &label, &window, amount));
    }
    report(limits, fetched_at_ms)
}

#[cfg(test)]
mod tests {
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
        let expected =
            add_one_month_ms(parse_iso_ms("2026-04-01T00:00:00Z").unwrap()).unwrap() as u64;
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
}

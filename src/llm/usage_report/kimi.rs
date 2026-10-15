//! Kimi Code (`kimi-code`) usage.
//!
//! The `GET {base}/usages` payload carries a weekly `usage` summary, `limits` rows with
//! per-window durations, and `usages` aggregate ratios. Rows are normalized into
//! [`UsageLimit`]s.
use super::factory::{parse_iso_ms, to_number, usage_status};
use super::types::{
    UsageAmount, UsageLimit, UsageReport, UsageScope, UsageStatus, UsageUnit, UsageWindow,
};
use crate::llm::oauth::{self, util};
use anyhow::{Result, bail};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::LazyLock;
use std::sync::{Arc, atomic::AtomicBool};

const PROVIDER: &str = "kimi-code";
const FLOW_ID: &str = "kimi-code";
const DEFAULT_BASE_URL: &str = "https://api.kimi.com/coding/v1";
const BASE_URL_ENV: &str = "KIMI_CODE_BASE_URL";
const DAY_MS: u64 = 86_400_000;
const WEEK_MS: u64 = 7 * DAY_MS;

/// One normalized usage row before conversion to [`UsageLimit`].
#[derive(Debug, Clone)]
struct Row {
    label: String,
    limit: Option<f64>,
    used: Option<f64>,
    remaining: Option<f64>,
    unit: UsageUnit,
    resets_at_ms: Option<u64>,
    window: Option<UsageWindow>,
}

/// Fetch the Kimi Code usage report. `Ok(None)` when no grant is stored for this provider.
pub(super) fn fetch(user_dir: &Path, cancel: &Arc<AtomicBool>) -> Result<Option<UsageReport>> {
    if !oauth::signed_in(user_dir, FLOW_ID) {
        return Ok(None);
    }
    let access = oauth::access(user_dir, FLOW_ID, cancel)?;
    let env_base = std::env::var(BASE_URL_ENV).ok();
    let base = resolve_base_url(access.base_url.as_deref(), env_base.as_deref());
    let url = format!("{base}/usages");

    let bearer = format!("Bearer {}", access.token.as_str());
    let mut headers: Vec<(&str, &str)> = access
        .headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    headers.push(("Authorization", bearer.as_str()));
    headers.push(("Accept", "application/json"));

    let now = util::now_ms();
    let response = util::request("GET", &url, &headers, util::Body::Empty, cancel)?
        .require_ok("Kimi usage endpoint")?;
    let payload = response.json()?;
    let rows = parse_usage_payload(&payload, now).unwrap_or_default();
    if rows.is_empty() {
        bail!("Kimi usage response has no usable rows");
    }

    let account = access.account_id.clone();
    let limits: Vec<UsageLimit> = rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| to_usage_limit(row, index, account.as_deref()))
        .collect();
    if limits.is_empty() {
        bail!("Kimi usage response has no usable rows");
    }

    let mut metadata = BTreeMap::new();
    metadata.insert("endpoint".into(), url);
    if let Some(identity) = oauth::identity(user_dir, FLOW_ID) {
        metadata.insert("account".into(), identity);
    }
    Ok(Some(UsageReport {
        provider: PROVIDER.into(),
        fetched_at_ms: now,
        limits,
        reset_credits: None,
        notes: Vec::new(),
        metadata,
    }))
}

/// Credential base URL, then `KIMI_CODE_BASE_URL`, then the default; trailing slashes dropped.
fn resolve_base_url(configured: Option<&str>, env: Option<&str>) -> String {
    let candidate = non_empty(configured)
        .or_else(|| non_empty(env))
        .unwrap_or(DEFAULT_BASE_URL);
    candidate.trim_end_matches('/').to_owned()
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// Parse the `/usages` payload into rows. `None` when the payload is not an object.
fn parse_usage_payload(payload: &Value, now: u64) -> Option<Vec<Row>> {
    let root = payload.as_object()?;
    let mut rows = Vec::new();

    if let Some(usage) = root.get("usage").and_then(Value::as_object)
        && let Some(mut row) = build_usage_row(usage, "Weekly limit", now)
    {
        row.window = Some(UsageWindow {
            id: "7d".into(),
            label: "7 Day".into(),
            duration_ms: None,
            resets_at_ms: parse_reset_time(usage, now),
            reset_label: None,
        });
        rows.push(row);
    }

    if let Some(total) = root.get("totalQuota").and_then(Value::as_object)
        && let Some(mut row) = build_usage_row(total, "Total quota", now)
    {
        row.window = build_window(window_data(total), now);
        rows.push(row);
    }

    if let Some(items) = root.get("limits").and_then(Value::as_array) {
        for (index, item) in items.iter().enumerate() {
            let Some(item) = item.as_object() else {
                continue;
            };
            let detail = item
                .get("detail")
                .and_then(Value::as_object)
                .unwrap_or(item);
            let window = window_data(item);
            let label = first_string(item, &["name", "title", "scope"])
                .or_else(|| first_string(detail, &["name", "title"]))
                .or_else(|| {
                    format_duration_label(
                        number(window.get("duration")),
                        string(window.get("timeUnit")),
                    )
                })
                .unwrap_or_else(|| format!("Limit #{}", index + 1));
            if let Some(mut row) = build_usage_row(detail, &label, now) {
                row.window = build_window(window, now);
                rows.push(row);
            }
        }
    }

    if let Some(usages) = root.get("usages").and_then(Value::as_object) {
        for (key, value) in usages {
            if key == "limit_5h" {
                continue;
            }
            if let Some(data) = value.as_object()
                && let Some(row) = build_aggregate_row(key, data, now)
            {
                rows.push(row);
            }
        }
    }
    Some(rows)
}

/// Nested `window` object, or an empty map when absent.
fn window_data(parent: &Map<String, Value>) -> &Map<String, Value> {
    static EMPTY: LazyLock<Map<String, Value>> = LazyLock::new(Map::new);
    parent
        .get("window")
        .and_then(Value::as_object)
        .unwrap_or(&*EMPTY)
}

fn first_string(data: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| data.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_owned)
}

fn number(value: Option<&Value>) -> Option<f64> {
    value.and_then(to_number)
}

fn string(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Reset instant from absolute (ISO string or epoch number) or relative (seconds) keys.
fn parse_reset_time(data: &Map<String, Value>, now: u64) -> Option<u64> {
    for key in ["reset_at", "resetAt", "reset_time", "resetTime"] {
        match data.get(key) {
            Some(Value::String(text)) => {
                if let Some(ms) = parse_iso_ms(text) {
                    return Some(ms);
                }
            }
            Some(value) => {
                if let Some(n) = to_number(value).filter(|n| *n >= 0.0) {
                    // Values above 1e12 are already milliseconds; smaller ones are seconds.
                    return Some(if n > 1e12 {
                        n as u64
                    } else {
                        (n * 1000.0) as u64
                    });
                }
            }
            None => {}
        }
    }
    for key in ["reset_in", "resetIn", "ttl", "window"] {
        if let Some(seconds) = number(data.get(key)).filter(|s| *s >= 0.0) {
            return Some(now.saturating_add((seconds * 1000.0) as u64));
        }
    }
    None
}

fn unit_ms(unit: &str) -> Option<u64> {
    match unit.to_uppercase().as_str() {
        "SECOND" | "TIME_UNIT_SECOND" => Some(1_000),
        "MINUTE" | "TIME_UNIT_MINUTE" => Some(60_000),
        "HOUR" | "TIME_UNIT_HOUR" => Some(3_600_000),
        "DAY" | "TIME_UNIT_DAY" => Some(DAY_MS),
        "WEEK" | "TIME_UNIT_WEEK" => Some(WEEK_MS),
        _ => None,
    }
}

/// Human label for a duration such as `5h limit`, used when a row has no name.
fn format_duration_label(duration: Option<f64>, unit: Option<&str>) -> Option<String> {
    let n = duration.filter(|d| *d > 0.0)? as u64;
    match unit?.to_uppercase().as_str() {
        "MINUTE" | "TIME_UNIT_MINUTE" if n.is_multiple_of(60) && n >= 60 => {
            Some(format!("{}h limit", n / 60))
        }
        "MINUTE" | "TIME_UNIT_MINUTE" => Some(format!("{n}m limit")),
        "HOUR" | "TIME_UNIT_HOUR" => Some(format!("{n}h limit")),
        "DAY" | "TIME_UNIT_DAY" => Some(format!("{n}d limit")),
        "SECOND" | "TIME_UNIT_SECOND" => Some(format!("{n}s limit")),
        _ => None,
    }
}

/// Stable window id from a duration: `Nd`, `Nh`, or `Nm`.
fn canonical_window_id(ms: u64) -> String {
    if ms == 0 {
        return "default".into();
    }
    if ms.is_multiple_of(DAY_MS) {
        return format!("{}d", ms / DAY_MS);
    }
    if ms.is_multiple_of(3_600_000) {
        return format!("{}h", ms / 3_600_000);
    }
    let minutes = (ms as f64 / 60_000.0).round() as u64;
    if minutes > 0 {
        format!("{minutes}m")
    } else {
        "default".into()
    }
}

fn build_window(data: &Map<String, Value>, now: u64) -> Option<UsageWindow> {
    let duration = number(data.get("duration")).filter(|d| *d > 0.0);
    let unit = string(data.get("timeUnit"));
    let label = format_duration_label(duration, unit);
    let resets_at_ms = parse_reset_time(data, now);
    if duration.is_none() && label.is_none() && resets_at_ms.is_none() {
        return None;
    }
    let duration_ms = match (duration, unit.and_then(unit_ms)) {
        (Some(d), Some(ms)) => Some((d as u64).saturating_mul(ms)),
        _ => None,
    };
    let id = duration_ms.map_or_else(|| "default".into(), canonical_window_id);
    Some(UsageWindow {
        id,
        label: label.unwrap_or_else(|| "Usage window".into()),
        duration_ms,
        resets_at_ms,
        reset_label: None,
    })
}

fn build_usage_row(data: &Map<String, Value>, default_label: &str, now: u64) -> Option<Row> {
    let limit = number(data.get("limit"));
    let remaining = number(data.get("remaining"));
    let used = number(data.get("used")).or_else(|| {
        limit
            .zip(remaining)
            .map(|(limit, remaining)| limit - remaining)
    });
    if used.is_none() && limit.is_none() {
        return None;
    }
    let label = first_string(data, &["name", "title"]).unwrap_or_else(|| default_label.to_owned());
    Some(Row {
        label,
        limit,
        used,
        remaining,
        unit: UsageUnit::Unknown,
        resets_at_ms: parse_reset_time(data, now),
        window: None,
    })
}

/// Aggregate ratio row (`usages.<key>.used_ratio` in [0, 1]), reported as a percent.
fn build_aggregate_row(key: &str, data: &Map<String, Value>, now: u64) -> Option<Row> {
    let ratio = number(data.get("used_ratio"))?.clamp(0.0, 1.0);
    let label = match key {
        "limit_month_total" => "Monthly total".to_owned(),
        "limit_month_code" => "Monthly code".to_owned(),
        other => other.to_owned(),
    };
    let resets_at_ms = parse_reset_time(data, now);
    Some(Row {
        label: label.clone(),
        limit: Some(100.0),
        used: Some(ratio * 100.0),
        remaining: Some((1.0 - ratio) * 100.0),
        unit: UsageUnit::Percent,
        resets_at_ms,
        window: Some(UsageWindow {
            id: key.to_owned(),
            label,
            duration_ms: None,
            resets_at_ms,
            reset_label: None,
        }),
    })
}

fn to_usage_limit(row: &Row, index: usize, account: Option<&str>) -> Option<UsageLimit> {
    let window = match (&row.window, row.resets_at_ms) {
        (Some(window), resets) => {
            let mut window = window.clone();
            window.resets_at_ms = window.resets_at_ms.or(resets);
            Some(window)
        }
        (None, Some(resets)) => Some(UsageWindow {
            id: "default".into(),
            label: "Usage window".into(),
            duration_ms: None,
            resets_at_ms: Some(resets),
            reset_label: None,
        }),
        (None, None) => None,
    };

    let used = row.used;
    let limit = row.limit;
    let remaining = row
        .remaining
        .or_else(|| limit.zip(used).map(|(limit, used)| limit - used));
    let (used_fraction, remaining_fraction) = match (limit, used) {
        (Some(limit), Some(used)) if limit > 0.0 => (
            Some((used / limit).clamp(0.0, 1.0)),
            Some(((limit - used) / limit).clamp(0.0, 1.0)),
        ),
        _ => (None, None),
    };

    let status = Some(match used_fraction {
        Some(fraction) => usage_status(fraction),
        None => UsageStatus::Unknown,
    });
    Some(UsageLimit {
        id: format!("{PROVIDER}:{index}"),
        label: row.label.clone(),
        scope: UsageScope {
            provider: PROVIDER.into(),
            account_id: account.map(str::to_owned),
            window_id: window.as_ref().map(|w| w.id.clone()),
            shared: Some(true),
            ..Default::default()
        },
        window,
        amount: UsageAmount {
            used,
            limit,
            remaining,
            used_fraction,
            remaining_fraction,
            unit: row.unit,
        },
        status,
        notes: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
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
}

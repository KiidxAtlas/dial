//! Gemini CLI (Cloud Code Assist) quota usage.
//!
//! The shared helpers at the bottom (timestamps, status thresholds, JSON field readers) are
//! also used by the Antigravity usage module.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use super::{
    UsageAmount, UsageLimit, UsageReport, UsageScope, UsageStatus, UsageUnit, UsageWindow,
};
use crate::llm::oauth::{self, util};

const ID: &str = "google-gemini-cli";
const CLIENT_METADATA: &str =
    "ideType=IDE_UNSPECIFIED,platform=PLATFORM_UNSPECIFIED,pluginType=GEMINI";

/// Exact model memberships per quota tier, in declaration order.
const TIERS: &[(&str, &[&str])] = &[
    (
        "3-Flash",
        &[
            "gemini-3-flash-preview",
            "gemini-3-flash",
            "gemini-3.5-flash",
        ],
    ),
    (
        "Flash",
        &[
            "gemini-2.5-flash",
            "gemini-2.5-flash-lite",
            "gemini-2.0-flash",
            "gemini-1.5-flash",
        ],
    ),
    (
        "Pro",
        &[
            "gemini-2.5-pro",
            "gemini-3-pro-preview",
            "gemini-3.1-pro-preview",
            "gemini-3-pro",
            "gemini-3.1-pro",
            "gemini-pro-agent",
            "gemini-1.5-pro",
        ],
    ),
];

/// Substring fallbacks used when a model is not an exact member, in declaration order.
const TIER_FALLBACKS: &[(&str, &str)] = &[("Flash", "flash"), ("Pro", "pro")];

pub(super) fn fetch(user_dir: &Path, cancel: &Arc<AtomicBool>) -> Result<Option<UsageReport>> {
    if !oauth::signed_in(user_dir, ID) {
        return Ok(None);
    }
    let access = oauth::access(user_dir, ID, cancel)?;
    let base = access
        .base_url
        .clone()
        .map_or_else(|| flow_base_url(ID), Ok)?;
    let auth = format!("Bearer {}", access.token.as_str());
    let user_agent = gemini_user_agent();
    let headers = [
        ("Authorization", auth.as_str()),
        ("Content-Type", "application/json"),
        ("User-Agent", user_agent.as_str()),
        ("Client-Metadata", CLIENT_METADATA),
    ];

    // loadCodeAssist only supplies the project and tier metadata, so a non-2xx response is
    // tolerated and the quota call proceeds without it.
    let load_body = {
        let mut body = json!({
            "metadata": {
                "ideType": "IDE_UNSPECIFIED",
                "platform": "PLATFORM_UNSPECIFIED",
                "pluginType": "GEMINI",
            }
        });
        if let Some(project) = access.project_id.as_deref() {
            body["cloudaicompanionProject"] = json!(project);
        }
        body
    };
    let load = util::request(
        "POST",
        &format!("{base}/v1internal:loadCodeAssist"),
        &headers,
        util::Body::Json(&load_body),
        cancel,
    )?;
    let load_json = if load.ok() { Some(load.json()?) } else { None };

    let project = access
        .project_id
        .clone()
        .or_else(|| load_json.as_ref().and_then(project_from_load));
    let quota_body = match project.as_deref() {
        Some(project) => json!({ "project": project }),
        None => json!({}),
    };
    let quota = util::request(
        "POST",
        &format!("{base}/v1internal:retrieveUserQuota"),
        &headers,
        util::Body::Json(&quota_body),
        cancel,
    )?;
    if !quota.ok() {
        bail!(
            "Gemini CLI retrieveUserQuota failed (HTTP {})",
            quota.status
        );
    }

    let mut report = parse_quota(
        &quota.json()?,
        ID,
        access.account_id.as_deref(),
        project.as_deref(),
        util::now_ms(),
    )?;
    report
        .metadata
        .extend(current_tier_metadata(load_json.as_ref()));
    if let Some(account) = oauth::identity(user_dir, ID) {
        report.metadata.insert("account".into(), account);
    }
    Ok(Some(report))
}

/// Parse a `retrieveUserQuota` body. Buckets without a model or reset are kept.
pub(super) fn parse_quota(
    payload: &Value,
    provider: &str,
    account_id: Option<&str>,
    project_id: Option<&str>,
    fetched_at_ms: u64,
) -> Result<UsageReport> {
    let object = payload
        .as_object()
        .context("Gemini quota response is not a JSON object")?;
    let empty = Vec::new();
    let buckets = match object.get("buckets") {
        None | Some(Value::Null) => &empty,
        Some(Value::Array(buckets)) => buckets,
        Some(_) => bail!("Gemini quota response `buckets` is not an array"),
    };

    let mut limits = Vec::with_capacity(buckets.len());
    for bucket in buckets {
        let bucket = bucket
            .as_object()
            .context("Gemini quota bucket is not a JSON object")?;
        let model_id = opt_str(bucket, "modelId")?;
        let remaining = opt_f64(bucket, "remainingFraction")?;
        let window = quota_window(opt_str(bucket, "resetTime")?);
        let window_id = window.id.clone();
        let tier = model_id.and_then(quota_tier);
        let label = match model_id {
            Some(model) => format!("Gemini {model}"),
            None => "Gemini quota".to_owned(),
        };
        limits.push(UsageLimit {
            id: format!("{}:{}", model_id.unwrap_or("unknown"), window_id),
            label,
            scope: UsageScope {
                provider: provider.to_owned(),
                account_id: account_id.map(str::to_owned),
                project_id: project_id.map(str::to_owned),
                model_id: model_id.map(str::to_owned),
                tier: tier.map(str::to_owned),
                window_id: Some(window_id),
                ..UsageScope::default()
            },
            window: Some(window),
            amount: quota_amount(remaining),
            status: None,
            notes: Vec::new(),
        });
    }

    Ok(UsageReport {
        provider: provider.to_owned(),
        fetched_at_ms,
        limits,
        reset_credits: None,
        notes: Vec::new(),
        metadata: BTreeMap::new(),
    })
}

/// Quota tier for a Gemini model (`quotaTierFor("google-gemini-cli", …)`).
pub(super) fn quota_tier(model_id: &str) -> Option<&'static str> {
    let id = model_id.to_ascii_lowercase();
    if let Some((tier, _)) = TIERS
        .iter()
        .find(|(_, members)| members.contains(&id.as_str()))
    {
        return Some(tier);
    }
    TIER_FALLBACKS
        .iter()
        .find(|(_, needle)| id.contains(needle))
        .map(|(tier, _)| *tier)
}

/// Project id from a `loadCodeAssist` body: either a string or `{ "id": … }`.
pub(super) fn project_from_load(load: &Value) -> Option<String> {
    let project = match load.get("cloudaicompanionProject")? {
        Value::String(project) => project.as_str(),
        Value::Object(object) => object.get("id")?.as_str()?,
        _ => return None,
    };
    (!project.is_empty()).then(|| project.to_owned())
}

fn current_tier_metadata(load: Option<&Value>) -> BTreeMap<String, String> {
    let mut metadata = BTreeMap::new();
    let Some(tier) = load
        .and_then(|load| load.get("currentTier"))
        .and_then(Value::as_object)
    else {
        return metadata;
    };
    if let Some(id) = tier.get("id").and_then(Value::as_str) {
        metadata.insert("currentTierId".to_owned(), id.to_owned());
    }
    if let Some(name) = tier.get("name").and_then(Value::as_str) {
        metadata.insert("currentTierName".to_owned(), name.to_owned());
    }
    metadata
}

fn quota_window(reset_time: Option<&str>) -> UsageWindow {
    let resets_at_ms = reset_time
        .and_then(parse_iso_ms)
        .and_then(|ms| u64::try_from(ms).ok());
    match resets_at_ms {
        Some(ms) => UsageWindow {
            id: format!("reset-{ms}"),
            label: "Quota window".to_owned(),
            resets_at_ms: Some(ms),
            ..UsageWindow::default()
        },
        None => UsageWindow {
            id: "quota".to_owned(),
            label: "Quota window".to_owned(),
            ..UsageWindow::default()
        },
    }
}

/// Percent amount from a remaining fraction; clamped to [0, 1] and rounded to one decimal.
fn quota_amount(remaining_fraction: Option<f64>) -> UsageAmount {
    let Some(remaining) = remaining_fraction.filter(|f| f.is_finite()) else {
        return UsageAmount {
            used: None,
            limit: None,
            remaining: None,
            used_fraction: None,
            remaining_fraction: None,
            unit: UsageUnit::Percent,
        };
    };
    let remaining = remaining.clamp(0.0, 1.0);
    let used = (1.0 - remaining).clamp(0.0, 1.0);
    UsageAmount {
        used: Some((used * 1000.0).round() / 10.0),
        limit: Some(100.0),
        remaining: Some((remaining * 1000.0).round() / 10.0),
        used_fraction: Some(used),
        remaining_fraction: Some(remaining),
        unit: UsageUnit::Percent,
    }
}

fn gemini_user_agent() -> String {
    format!(
        "GeminiCLI/0.46.0/gemini-3.1-pro-preview ({}; {}; terminal)",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// Configured base URL of a flow, without a trailing slash.
pub(super) fn flow_base_url(id: &str) -> Result<String> {
    let flow = oauth::flow(id).with_context(|| format!("unknown OAuth flow `{id}`"))?;
    Ok(flow.base_url.trim_end_matches('/').to_owned())
}

/// Usage status from a used fraction.
pub(super) fn usage_status_for(used_fraction: f64) -> UsageStatus {
    if used_fraction >= 1.0 {
        UsageStatus::Exhausted
    } else if used_fraction >= 0.9 {
        UsageStatus::Warning
    } else {
        UsageStatus::Ok
    }
}

/// Optional string field. Absent or `null` is `None`; any other type is an error.
pub(super) fn opt_str<'a>(object: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(_) => bail!("usage field `{key}` must be a string"),
    }
}

/// Optional numeric field. Absent or `null` is `None`; any other type is an error.
pub(super) fn opt_f64(object: &Map<String, Value>, key: &str) -> Result<Option<f64>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_f64()
            .map(Some)
            .with_context(|| format!("usage field `{key}` must be a number")),
    }
}

/// ISO-8601 / RFC 3339 timestamp to Unix ms (`Date.parse` for the shapes Google sends):
/// `YYYY-MM-DD`, optionally followed by `T`/space, `HH:MM[:SS[.fff]]`, and `Z` or `±HH[:MM]`.
/// Missing offsets are treated as UTC. Fractions beyond milliseconds are truncated.
pub(super) fn parse_iso_ms(text: &str) -> Option<i64> {
    let bytes = text.trim().as_bytes();
    let mut at = 0;
    let year = digits(bytes, &mut at, 4)?;
    eat(bytes, &mut at, b'-')?;
    let month = digits(bytes, &mut at, 2)?;
    eat(bytes, &mut at, b'-')?;
    let day = digits(bytes, &mut at, 2)?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }

    let (mut hour, mut minute, mut second, mut millis, mut offset_min) = (0, 0, 0, 0, 0);
    if at < bytes.len() {
        if !matches!(bytes[at], b'T' | b't' | b' ') {
            return None;
        }
        at += 1;
        hour = digits(bytes, &mut at, 2)?;
        eat(bytes, &mut at, b':')?;
        minute = digits(bytes, &mut at, 2)?;
        if bytes.get(at).copied() == Some(b':') {
            at += 1;
            second = digits(bytes, &mut at, 2)?;
            if matches!(bytes.get(at).copied(), Some(b'.' | b',')) {
                at += 1;
                let start = at;
                while bytes.get(at).is_some_and(u8::is_ascii_digit) {
                    at += 1;
                }
                if at == start {
                    return None;
                }
                for k in 0..3 {
                    let digit = if start + k < at {
                        i64::from(bytes[start + k] - b'0')
                    } else {
                        0
                    };
                    millis = millis * 10 + digit;
                }
            }
        }
        match bytes.get(at).copied() {
            None => {}
            Some(b'Z' | b'z') => at += 1,
            Some(sign @ (b'+' | b'-')) => {
                at += 1;
                let offset_hours = digits(bytes, &mut at, 2)?;
                if bytes.get(at).copied() == Some(b':') {
                    at += 1;
                }
                let offset_minutes = digits(bytes, &mut at, 2)?;
                let total = offset_hours * 60 + offset_minutes;
                offset_min = if sign == b'-' { -total } else { total };
            }
            Some(_) => return None,
        }
    }
    if at != bytes.len() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(
        (days * 86_400 + hour * 3_600 + minute * 60 + second) * 1_000 + millis
            - offset_min * 60_000,
    )
}

fn digits(bytes: &[u8], at: &mut usize, count: usize) -> Option<i64> {
    let slice = bytes.get(*at..*at + count)?;
    if !slice.iter().all(u8::is_ascii_digit) {
        return None;
    }
    *at += count;
    Some(
        slice
            .iter()
            .fold(0, |acc, b| acc * 10 + i64::from(b - b'0')),
    )
}

fn eat(bytes: &[u8], at: &mut usize, want: u8) -> Option<()> {
    if bytes.get(*at).copied() == Some(want) {
        *at += 1;
        Some(())
    } else {
        None
    }
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(year) => 29,
        _ => 28,
    }
}

/// `Date.prototype.setUTCMonth(getUTCMonth() + 1)` on a Unix ms value. Day overflow rolls into
/// the following month, matching JavaScript (Jan 31 → Mar 3 in a non-leap year).
pub(super) fn add_one_month_ms(ms: i64) -> Option<i64> {
    const DAY: i64 = 86_400_000;
    let days = ms.div_euclid(DAY);
    let time = ms.rem_euclid(DAY);
    let (year, month, day) = civil_from_days(days);
    let total = year * 12 + (month - 1) + 1;
    let first = days_from_civil(total.div_euclid(12), total.rem_euclid(12) + 1, 1);
    first
        .checked_add(day - 1)?
        .checked_mul(DAY)?
        .checked_add(time)
}

/// Calendar date `(year, month, day)` for days since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Days since 1970-01-01 (proleptic Gregorian). `day` may overflow the month linearly.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests;

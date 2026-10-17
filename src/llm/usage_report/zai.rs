//! Z.AI (GLM Coding Plan) quota windows.
//!
//! `GET {origin}/api/monitor/usage/quota/limit` returns `data.limits[]` with three meter kinds:
//! `TOKENS_LIMIT` (token quota), `TIME_LIMIT` (request quota, or the zread feature quota) and
//! `CREDIT_LIMIT` (GLM plan credits). Each entry carries a window (`unit` + `number`), an optional
//! reset time, and either absolute counters or a server-rounded `percentage` (0-100, may overshoot).
//! The grant's access key is sent raw in `Authorization` (no Bearer prefix). The last week of model
//! usage is fetched best-effort and kept as a JSON string in `metadata["model_usage"]`.

use super::types::{
    UsageAmount, UsageLimit, UsageReport, UsageScope, UsageStatus, UsageUnit, UsageWindow,
};
use crate::llm::oauth::{self, util};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

const PROVIDER: &str = "zai";
const DEFAULT_ORIGIN: &str = "https://api.z.ai";
const QUOTA_PATH: &str = "/api/monitor/usage/quota/limit";
const MODEL_USAGE_PATH: &str = "/api/monitor/usage/model-usage";
const MODEL_USAGE_UNAVAILABLE: &str = "Z.AI model usage unavailable";
const USER_AGENT: &str = concat!(
    "dial-agent/",
    env!("CARGO_PKG_VERSION"),
    " (coding agent; usage)"
);

const HOUR_MS: u64 = 60 * 60 * 1000;
const DAY_MS: u64 = 24 * HOUR_MS;
const WEEK_MS: u64 = 7 * DAY_MS;
const MONTH_MS: u64 = 30 * DAY_MS;

/// Account-wide Z.AI quota windows for the stored sign-in. `Ok(None)` when Z.AI is not signed in.
pub(super) fn fetch(user_dir: &Path, cancel: &Arc<AtomicBool>) -> Result<Option<UsageReport>> {
    if !oauth::signed_in(user_dir, PROVIDER) {
        return Ok(None);
    }
    let access = oauth::access(user_dir, PROVIDER, cancel)?;
    let origin = endpoint_origin(access.base_url.as_deref());
    let url = format!("{origin}{QUOTA_PATH}");
    let mut headers: Vec<(&str, &str)> = vec![
        ("Authorization", access.token.as_str()),
        ("Content-Type", "application/json"),
        ("User-Agent", USER_AGENT),
    ];
    headers.extend(
        access
            .headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str())),
    );

    let response = util::request("GET", &url, &headers, util::Body::Empty, cancel)?;
    ensure!(
        response.ok(),
        "Z.AI quota request returned HTTP {}",
        response.status
    );
    let now = util::now_ms();
    let mut report = parse_quota(&response.json()?, now)?;
    report.metadata.insert("endpoint".into(), url);
    if let Some(account) = oauth::identity(user_dir, PROVIDER) {
        report.metadata.insert("account".into(), account);
    }
    attach_model_usage(&mut report, &origin, &headers, now, cancel)?;
    Ok(Some(report))
}

/// Best-effort model usage: failures become a report note, never a failed fetch.
/// Only a cancellation is propagated.
fn attach_model_usage(
    report: &mut UsageReport,
    origin: &str,
    headers: &[(&str, &str)],
    now: u64,
    cancel: &Arc<AtomicBool>,
) -> Result<()> {
    let Some((start, end)) = local_stamp(now.saturating_sub(WEEK_MS)).zip(local_stamp(now)) else {
        report.notes.push(MODEL_USAGE_UNAVAILABLE.into());
        return Ok(());
    };
    let url = format!(
        "{origin}{MODEL_USAGE_PATH}?startTime={}&endTime={}",
        util::encode(&start),
        util::encode(&end)
    );
    match util::request("GET", &url, headers, util::Body::Empty, cancel) {
        Ok(response) if response.ok() => match response.json_or_null() {
            value @ Value::Object(_) => {
                report
                    .metadata
                    .insert("model_usage".into(), value.to_string());
            }
            _ => report.notes.push(MODEL_USAGE_UNAVAILABLE.into()),
        },
        Ok(_) => report.notes.push(MODEL_USAGE_UNAVAILABLE.into()),
        Err(error) if cancel.load(Ordering::Relaxed) => return Err(error),
        Err(_) => report.notes.push(MODEL_USAGE_UNAVAILABLE.into()),
    }
    Ok(())
}

/// Map a quota payload into a normalized report. Pure; `fetched_at_ms` is stamped onto the report.
///
/// Errors: a non-object payload, `success` not `true`, a missing `data` object or `data.limits`
/// array, or a payload with no recognized limit entries.
pub(super) fn parse_quota(payload: &Value, fetched_at_ms: u64) -> Result<UsageReport> {
    let root = payload
        .as_object()
        .context("Z.AI quota response is not a JSON object")?;
    if root.get("success") != Some(&Value::Bool(true)) {
        let code = root.get("code").map_or_else(
            || "none".to_owned(),
            |code| util::sanitize(&code.to_string()),
        );
        let message = root
            .get("msg")
            .and_then(Value::as_str)
            .map(util::sanitize)
            .unwrap_or_default();
        bail!("Z.AI quota request was not successful (code {code}): {message}");
    }
    let data = root
        .get("data")
        .and_then(Value::as_object)
        .context("Z.AI quota response has no `data` object")?;
    let items = data
        .get("limits")
        .and_then(Value::as_array)
        .context("Z.AI quota response has no `data.limits` array")?;
    let limits: Vec<UsageLimit> = items
        .iter()
        .filter_map(parse_limit_item)
        .filter_map(|item| build_limit(&item))
        .collect();
    ensure!(
        !limits.is_empty(),
        "Z.AI quota response contained no recognized limits"
    );

    let mut metadata = BTreeMap::new();
    if let Some(plan) = data
        .get("level")
        .and_then(Value::as_str)
        .filter(|level| !level.is_empty())
    {
        metadata.insert("plan".to_owned(), plan.to_owned());
    }
    Ok(UsageReport {
        provider: PROVIDER.into(),
        fetched_at_ms,
        limits,
        reset_credits: None,
        notes: Vec::new(),
        metadata,
    })
}

/// One raw `data.limits[]` entry after type-tolerant extraction.
struct LimitItem {
    kind: String,
    usage: Option<f64>,
    current_value: Option<f64>,
    percentage: Option<f64>,
    remaining: Option<f64>,
    next_reset_ms: Option<u64>,
    unit: Option<f64>,
    number: Option<f64>,
    model_codes: Vec<String>,
}

fn parse_limit_item(value: &Value) -> Option<LimitItem> {
    let object = value.as_object()?;
    let kind = object
        .get("type")?
        .as_str()
        .filter(|kind| !kind.is_empty())?
        .to_owned();
    let model_codes = object
        .get("usageDetails")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|detail| {
            detail
                .get("modelCode")?
                .as_str()
                .filter(|code| !code.is_empty())
                .map(str::to_owned)
        })
        .collect();
    Some(LimitItem {
        kind,
        usage: number(object.get("usage")),
        current_value: number(object.get("currentValue")),
        percentage: number(object.get("percentage")),
        remaining: number(object.get("remaining")),
        next_reset_ms: millis(object.get("nextResetTime")),
        unit: number(object.get("unit")),
        number: number(object.get("number")),
        model_codes,
    })
}

/// A finite number from a JSON number or numeric string.
fn number(value: Option<&Value>) -> Option<f64> {
    let parsed = match value? {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    parsed.is_finite().then_some(parsed)
}

/// Epoch milliseconds from a value that may be seconds or milliseconds (values above 1e12 are ms).
fn millis(value: Option<&Value>) -> Option<u64> {
    let parsed = number(value)?;
    if parsed < 0.0 {
        return None;
    }
    let ms = if parsed > 1_000_000_000_000.0 {
        parsed
    } else {
        parsed * 1000.0
    };
    Some(ms as u64)
}

/// Z.AI `percentage` is 0-100 and may overshoot; clamp it to a used fraction.
fn percentage_fraction(percentage: Option<f64>) -> Option<f64> {
    percentage.map(|percentage| (percentage / 100.0).clamp(0.0, 1.0))
}

/// Standard quota status thresholds.
fn status_for(used_fraction: f64) -> UsageStatus {
    if used_fraction >= 1.0 {
        UsageStatus::Exhausted
    } else if used_fraction >= 0.9 {
        UsageStatus::Warning
    } else {
        UsageStatus::Ok
    }
}

/// Absolute counters, or an explicit fraction. Without an explicit fraction the fraction is
/// derived from `used / limit` (capped at 1).
fn build_amount(
    used: Option<f64>,
    limit: Option<f64>,
    remaining: Option<f64>,
    used_fraction: Option<f64>,
    unit: UsageUnit,
) -> UsageAmount {
    let used_fraction = used_fraction.or_else(|| match (used, limit) {
        (Some(used), Some(limit)) if limit > 0.0 => Some((used / limit).min(1.0)),
        _ => None,
    });
    UsageAmount {
        used,
        limit,
        remaining,
        used_fraction,
        remaining_fraction: used_fraction.map(|fraction| (1.0 - fraction).max(0.0)),
        unit,
    }
}

fn build_window(item: &LimitItem) -> UsageWindow {
    let count = item.number.filter(|count| *count > 0.0).unwrap_or(1.0);
    let count_text = count.to_string();
    let (id, label, duration_ms) = match item.unit {
        Some(3.0) => (
            format!("{count_text}h"),
            counted(count, "Hour"),
            Some(scaled(count, HOUR_MS)),
        ),
        Some(4.0) => (
            format!("{count_text}d"),
            counted(count, "Day"),
            Some(scaled(count, DAY_MS)),
        ),
        Some(5.0) => (
            format!("{count_text}mo"),
            if count == 1.0 {
                "Monthly".to_owned()
            } else {
                counted(count, "Month")
            },
            Some(scaled(count, MONTH_MS)),
        ),
        Some(6.0) => ("1w".to_owned(), "Weekly".to_owned(), Some(WEEK_MS)),
        Some(unit) => (format!("{count_text}u{unit}"), "Quota".to_owned(), None),
        None => ("quota".to_owned(), "Quota".to_owned(), None),
    };
    UsageWindow {
        id,
        label,
        duration_ms,
        resets_at_ms: item.next_reset_ms,
        reset_label: None,
    }
}

fn counted(count: f64, singular: &str) -> String {
    let plural = if count == 1.0 { "" } else { "s" };
    format!("{count} {singular}{plural}")
}

fn scaled(count: f64, unit_ms: u64) -> u64 {
    (count * unit_ms as f64) as u64
}

/// The `zread` feature quota carries the search, reader and zread model codes together.
fn is_feature_limit(item: &LimitItem) -> bool {
    ["search-prime", "web-reader", "zread"]
        .iter()
        .all(|code| item.model_codes.iter().any(|model| model.as_str() == *code))
}

fn build_limit(item: &LimitItem) -> Option<UsageLimit> {
    let window = build_window(item);
    let (id, label, shared, tier, amount) = match item.kind.as_str() {
        "TOKENS_LIMIT" => {
            let amount = build_amount(
                item.current_value,
                item.usage,
                item.remaining,
                percentage_fraction(item.percentage),
                UsageUnit::Tokens,
            );
            (
                format!("zai:tokens:{}", window.id),
                format!("ZAI {} Token Quota", window.label),
                true,
                None,
                amount,
            )
        }
        "TIME_LIMIT" => {
            let amount = build_amount(
                item.current_value,
                item.usage,
                item.remaining,
                percentage_fraction(item.percentage),
                UsageUnit::Requests,
            );
            if is_feature_limit(item) {
                (
                    format!("zai:features:zread:{}", window.id),
                    "ZAI Zread Quota".to_owned(),
                    false,
                    Some("zread".to_owned()),
                    amount,
                )
            } else {
                (
                    format!("zai:requests:{}", window.id),
                    "ZAI Request Quota".to_owned(),
                    true,
                    None,
                    amount,
                )
            }
        }
        "CREDIT_LIMIT" => {
            // GLM Coding Plan windows (e.g. 12k credits / 5h): `usage` is the allotment and
            // `currentValue` the spend. `percentage` is server-rounded (11 for 1438/12000 ≈ 11.98%),
            // so prefer the exact ratio and fall back to the percentage only without absolutes.
            let absolute =
                item.current_value.is_some() && item.usage.is_some_and(|usage| usage > 0.0);
            let fraction = if absolute {
                None
            } else {
                percentage_fraction(item.percentage)
            };
            let amount = build_amount(
                item.current_value,
                item.usage,
                item.remaining,
                fraction,
                UsageUnit::Credits,
            );
            (
                format!("zai:credits:{}", window.id),
                format!("ZAI {} Credit Quota", window.label),
                true,
                None,
                amount,
            )
        }
        _ => return None,
    };
    let status = amount.used_fraction.map(status_for);
    let window_id = window.id.clone();
    Some(UsageLimit {
        id,
        label,
        scope: UsageScope {
            provider: PROVIDER.into(),
            window_id: Some(window_id),
            shared: Some(shared),
            tier,
            ..UsageScope::default()
        },
        window: Some(window),
        amount,
        status,
        notes: Vec::new(),
    })
}

/// Origin of the configured base URL, or the Z.AI default when absent or unusable.
fn endpoint_origin(base_url: Option<&str>) -> String {
    base_url
        .map(str::trim)
        .filter(|base| !base.is_empty())
        .and_then(|base| url::Url::parse(base).ok())
        .map(|parsed| parsed.origin().ascii_serialization())
        .filter(|origin| origin != "null")
        .unwrap_or_else(|| DEFAULT_ORIGIN.into())
}

/// Local-time `YYYY-MM-DD HH:MM:SS` for a Unix-ms instant (the model-usage query format; the space is percent-encoded to `%20`).
fn local_stamp(unix_ms: u64) -> Option<String> {
    let (year, month, day, hour, minute, second) = local_fields(unix_ms)?;
    Some(format!(
        "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}"
    ))
}

/// `(year, month, day, hour, minute, second)` in the local time zone.
#[cfg(unix)]
fn local_fields(unix_ms: u64) -> Option<(i32, i32, i32, i32, i32, i32)> {
    let seconds = libc::time_t::try_from(unix_ms / 1000).ok()?;
    // SAFETY: `tm` is plain old data and is zero-initialised before `localtime_r` writes into it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers reference live locals; a null return is checked below.
    if unsafe { libc::localtime_r(&seconds, &mut tm) }.is_null() {
        return None;
    }
    Some((
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
    ))
}

#[cfg(windows)]
fn local_fields(unix_ms: u64) -> Option<(i32, i32, i32, i32, i32, i32)> {
    use windows_sys::Win32::{
        Foundation::{FILETIME, SYSTEMTIME},
        System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime},
    };
    /// FILETIME counts 100-ns ticks from 1601-01-01 UTC; this is the offset to the Unix epoch.
    const UNIX_EPOCH_AS_FILETIME: u64 = 116_444_736_000_000_000;
    let ticks = unix_ms
        .checked_mul(10_000)?
        .checked_add(UNIX_EPOCH_AS_FILETIME)?;
    let utc_filetime = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    // SAFETY: both pointers reference live locals of the Win32 types the APIs expect; a null
    // zone selects the current time zone. Zero return means failure and is checked below.
    unsafe {
        if FileTimeToSystemTime(&utc_filetime, &mut utc) == 0
            || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) == 0
        {
            return None;
        }
    }
    Some((
        local.wYear.into(),
        local.wMonth.into(),
        local.wDay.into(),
        local.wHour.into(),
        local.wMinute.into(),
        local.wSecond.into(),
    ))
}

#[cfg(test)]
mod tests;

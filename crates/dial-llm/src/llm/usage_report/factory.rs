//! Factory Droid usage.
//!
//! Reads `GET {base}/api/billing/limits`: per-pool (Standard credits / Droid Core) × per-window
//! percent-used limits, plus the extra usage balance when present.
use super::types::{
    UsageAmount, UsageLimit, UsageReport, UsageScope, UsageStatus, UsageUnit, UsageWindow,
};
use crate::llm::oauth::{self, util};
use anyhow::{Result, bail};
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, atomic::AtomicBool};

const PROVIDER: &str = "factory-droid";
const FLOW_ID: &str = "factory-droid";
const DEFAULT_BASE_URL: &str = "https://api.factory.ai";
const HOUR_MS: u64 = 3_600_000;
const DAY_MS: u64 = 86_400_000;
const WEEK_MS: u64 = 7 * DAY_MS;

/// (wire key, window id, window label, duration ms).
const WINDOWS: [(&str, &str, &str, u64); 3] = [
    ("fiveHour", "5h", "5 Hour", 5 * HOUR_MS),
    ("weekly", "weekly", "Weekly", WEEK_MS),
    ("monthly", "monthly", "Monthly", 30 * DAY_MS),
];

/// (wire key, label).
const POOLS: [(&str, &str); 2] = [("standard", "Standard credits"), ("core", "Droid Core")];

/// Fetch the Factory Droid quota report. `Ok(None)` when no grant is stored for this provider.
pub(super) fn fetch(user_dir: &Path, cancel: &Arc<AtomicBool>) -> Result<Option<UsageReport>> {
    if !oauth::signed_in(user_dir, FLOW_ID) {
        return Ok(None);
    }
    let access = oauth::access(user_dir, FLOW_ID, cancel)?;
    let base = access
        .base_url
        .as_deref()
        .unwrap_or(DEFAULT_BASE_URL)
        .trim_end_matches('/');
    let url = format!("{base}/api/billing/limits");
    let bearer = format!("Bearer {}", access.token.as_str());
    let mut owned: Vec<(String, String)> = vec![
        ("Accept".into(), "application/json".into()),
        ("Authorization".into(), bearer),
        ("X-Client-Version".into(), "0.230.0".into()),
        ("X-Factory-Client".into(), "cli".into()),
    ];
    owned.extend(access.headers.iter().cloned());
    let headers: Vec<(&str, &str)> = owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let response = util::request("GET", &url, &headers, util::Body::Empty, cancel)?
        .require_ok("Factory Droid usage endpoint")?;
    let payload = response.json()?;
    let mut report = parse_limits(&payload, util::now_ms())?;
    if let Some(account) = oauth::identity(user_dir, FLOW_ID) {
        report.metadata.insert("account".into(), account);
    }
    Ok(Some(report))
}

/// Parse a `/api/billing/limits` payload. Errors on a non-object payload, a missing `limits`
/// object, or a payload that exposes no quota windows at all.
fn parse_limits(payload: &Value, fetched_at_ms: u64) -> Result<UsageReport> {
    let Some(root) = payload.as_object() else {
        bail!("Factory Droid usage response is not a JSON object");
    };
    if root.get("usesTokenRateLimitsBilling") == Some(&Value::Bool(false)) {
        return Ok(UsageReport {
            provider: PROVIDER.into(),
            fetched_at_ms,
            limits: Vec::new(),
            reset_credits: None,
            notes: vec![
                "This Factory account does not use token-rate-limit billing; no quota windows are exposed."
                    .into(),
            ],
            metadata: Default::default(),
        });
    }
    let Some(limits) = root.get("limits").and_then(Value::as_object) else {
        bail!("Factory Droid usage response has no limits object");
    };

    let mut out = Vec::new();
    for (pool_key, pool_label) in POOLS {
        let Some(pool) = limits.get(pool_key).and_then(Value::as_object) else {
            continue;
        };
        for (window_key, window_id_short, window_label, duration_ms) in WINDOWS {
            let Some(entry) = pool.get(window_key).and_then(Value::as_object) else {
                continue;
            };
            let Some(used_percent) = entry.get("usedPercent").and_then(to_number) else {
                continue;
            };
            // Factory freezes a lapsed window at its last-used state; the droid CLI treats
            // windowEnd >= now as active. Mirror that: an inactive window reads as 0% used with
            // no reset countdown.
            let window_end = entry
                .get("windowEnd")
                .and_then(Value::as_str)
                .and_then(parse_iso_ms);
            let active = window_end.is_some_and(|end| end >= fetched_at_ms);
            let window_id = format!("{pool_key}-{window_id_short}");
            let effective = if active { used_percent } else { 0.0 };
            let used_fraction = (effective / 100.0).clamp(0.0, 1.0);
            out.push(UsageLimit {
                id: format!("{PROVIDER}:{pool_key}:{window_id_short}"),
                label: format!("{pool_label} {window_label} window"),
                scope: UsageScope {
                    provider: PROVIDER.into(),
                    window_id: Some(window_id.clone()),
                    ..Default::default()
                },
                window: Some(UsageWindow {
                    id: window_id,
                    label: format!("{pool_label} {window_label}"),
                    duration_ms: Some(duration_ms),
                    resets_at_ms: if active { window_end } else { None },
                    reset_label: None,
                }),
                amount: UsageAmount {
                    used: Some(effective),
                    limit: Some(100.0),
                    remaining: Some((100.0 - effective).max(0.0)),
                    used_fraction: Some(used_fraction),
                    remaining_fraction: Some((1.0 - used_fraction).max(0.0)),
                    unit: UsageUnit::Percent,
                },
                status: Some(usage_status(used_fraction)),
                notes: Vec::new(),
            });
        }
    }

    if let Some(balance_cents) = root.get("extraUsageBalanceCents").and_then(to_number)
        && balance_cents > 0.0
    {
        let balance_usd = balance_cents / 100.0;
        out.push(UsageLimit {
            id: format!("{PROVIDER}:extra-balance"),
            label: "Extra usage balance".into(),
            scope: UsageScope {
                provider: PROVIDER.into(),
                ..Default::default()
            },
            window: None,
            amount: UsageAmount {
                used: Some(0.0),
                limit: Some(balance_usd),
                remaining: Some(balance_usd),
                used_fraction: Some(0.0),
                remaining_fraction: Some(1.0),
                unit: UsageUnit::Usd,
            },
            status: Some(UsageStatus::Ok),
            notes: Vec::new(),
        });
    }

    if out.is_empty() {
        bail!("Factory Droid usage response exposes no quota windows");
    }
    Ok(UsageReport {
        provider: PROVIDER.into(),
        fetched_at_ms,
        limits: out,
        reset_credits: None,
        notes: Vec::new(),
        metadata: Default::default(),
    })
}

/// Numeric JSON value or numeric string (finite only).
pub(super) fn to_number(value: &Value) -> Option<f64> {
    let number = match value {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    number.is_finite().then_some(number)
}

/// Status from a used fraction: >=1 exhausted, >=0.9 warning, else ok.
pub(super) fn usage_status(used_fraction: f64) -> UsageStatus {
    if used_fraction >= 1.0 {
        UsageStatus::Exhausted
    } else if used_fraction >= 0.9 {
        UsageStatus::Warning
    } else {
        UsageStatus::Ok
    }
}

/// `YYYY-MM-DDTHH:MM:SS[.fraction](Z|±HH:MM)` -> Unix milliseconds.
pub(super) fn parse_iso_ms(text: &str) -> Option<u64> {
    let (date, rest) = text.trim().split_once('T')?;
    let mut parts = date.splitn(3, '-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    let zone_at = rest.find(['Z', '+', '-'])?;
    let (clock, zone) = rest.split_at(zone_at);
    let (hms, fraction) = clock.split_once('.').unwrap_or((clock, ""));
    let mut clock_parts = hms.split(':');
    let hour: i64 = clock_parts.next()?.parse().ok()?;
    let minute: i64 = clock_parts.next()?.parse().ok()?;
    let second: i64 = clock_parts.next()?.parse().ok()?;
    if clock_parts.next().is_some() {
        return None;
    }
    let millis: i64 = if fraction.is_empty() {
        0
    } else {
        fraction
            .chars()
            .chain(std::iter::repeat('0'))
            .take(3)
            .collect::<String>()
            .parse()
            .ok()?
    };
    let offset_minutes: i128 = match zone {
        "Z" => 0,
        _ => {
            let sign: i128 = match zone.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let (oh, om) = zone[1..].split_once(':')?;
            let oh = i128::from(oh.parse::<i64>().ok()?);
            let om = i128::from(om.parse::<i64>().ok()?);
            sign * (oh * 60 + om)
        }
    };
    let seconds = days_from_civil(year, month, day) * 86_400
        + i128::from(hour) * 3_600
        + i128::from(minute) * 60
        + i128::from(second)
        - offset_minutes * 60;
    u64::try_from(seconds * 1_000 + i128::from(millis)).ok()
}

/// Proleptic Gregorian date -> days since 1970-01-01 (H. Hinnant).
/// Wide arithmetic: the fields are arbitrary parsed i64s, so intermediate products must not overflow.
fn days_from_civil(year: i64, month: i64, day: i64) -> i128 {
    let (year, month, day) = (i128::from(year), i128::from(month), i128::from(day));
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests;

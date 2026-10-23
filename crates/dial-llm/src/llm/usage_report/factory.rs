//! Factory Droid usage.
//!
//! Reads `GET {base}/api/billing/limits`: per-pool (Standard credits / Droid Core) × per-window
//! percent-used limits, plus the extra usage balance when present.
use super::common::{parse_iso_ms, to_number, usage_status};
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

#[cfg(test)]
mod tests;

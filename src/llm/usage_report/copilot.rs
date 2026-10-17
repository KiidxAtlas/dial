//! GitHub Copilot usage: `GET /copilot_internal/user` for the monthly premium, chat, and
//! completion quotas, with the GitHub account from `GET /user`. Only the OAuth grant is
//! supported because Dial's Copilot provider is OAuth-only.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Result, bail};
use serde_json::{Map, Value};

use super::claude::parse_iso_ms;
use super::types::{
    UsageAmount, UsageLimit, UsageReport, UsageScope, UsageStatus, UsageUnit, UsageWindow,
};
use crate::llm::oauth::{
    self,
    util::{self, Body, now_ms},
};

const PROVIDER: &str = "copilot";
const DEFAULT_API_BASE: &str = "https://api.github.com";
const COPILOT_USER_AGENT: &str = "copilot/1.0.82";
const GITHUB_USER_AGENT: &str = concat!(
    "dial-agent/",
    env!("CARGO_PKG_VERSION"),
    " (coding agent; usage)"
);
const MONTHLY_WINDOW_ID: &str = "monthly";

pub(super) fn fetch(user_dir: &Path, cancel: &Arc<AtomicBool>) -> Result<Option<UsageReport>> {
    if !oauth::signed_in(user_dir, PROVIDER) {
        return Ok(None);
    }
    let access = oauth::access(user_dir, PROVIDER, cancel)?;
    let token = access.token.as_str();
    let base = api_base(oauth::enterprise_url(user_dir, PROVIDER).as_deref());
    let bearer = format!("Bearer {token}");

    let quota = get_json(
        &format!("{base}/copilot_internal/user"),
        &[
            ("Content-Type", "application/json"),
            ("Accept", "application/json"),
            ("Authorization", bearer.as_str()),
            ("Copilot-Integration-Id", "copilot-chat"),
            ("User-Agent", COPILOT_USER_AGENT),
            ("Editor-Version", COPILOT_USER_AGENT),
            ("Copilot-Harness-Id", "copilot-sdk"),
            ("Openai-Intent", "conversation-agent"),
            ("X-GitHub-Api-Version", "2026-08-01"),
        ],
        cancel,
    )?;

    let mut notes = Vec::new();
    let (login, email) = match get_json(
        &format!("{base}/user"),
        &[
            ("Accept", "application/vnd.github+json"),
            ("Authorization", bearer.as_str()),
            ("User-Agent", GITHUB_USER_AGENT),
            ("X-GitHub-Api-Version", "2022-11-28"),
        ],
        cancel,
    ) {
        Ok(user) => user_fields(&user),
        Err(error) => {
            notes.push(format!("GitHub account unavailable: {error:#}"));
            (None, None)
        }
    };

    let Some(mut report) = parse_quota(&quota, now_ms()) else {
        bail!("GitHub Copilot quota response had no quota snapshots");
    };
    let account_id = login.or_else(|| oauth::identity(user_dir, PROVIDER));
    for limit in &mut report.limits {
        limit.scope.account_id = account_id.clone();
    }
    if let Some(id) = &account_id {
        report.metadata.insert("accountId".to_owned(), id.clone());
        report.metadata.insert("account".to_owned(), id.clone());
    }
    if let Some(email) = email {
        report.metadata.insert("email".to_owned(), email);
    }
    report.notes.extend(notes);
    Ok(Some(report))
}

fn get_json(url: &str, headers: &[(&str, &str)], cancel: &Arc<AtomicBool>) -> Result<Value> {
    let response = util::request("GET", url, headers, Body::Empty, cancel)?;
    if !response.ok() {
        bail!("GitHub Copilot endpoint returned HTTP {}", response.status);
    }
    response.json()
}

/// API base for the stored grant. A GitHub Enterprise host maps to its `api.` host; an explicit
/// URL is used as-is.
fn api_base(enterprise: Option<&str>) -> String {
    match enterprise.map(str::trim).filter(|value| !value.is_empty()) {
        None => DEFAULT_API_BASE.to_owned(),
        Some(url) if url.starts_with("http://") || url.starts_with("https://") => {
            url.trim_end_matches('/').to_owned()
        }
        Some(host) => {
            let host = host.trim_end_matches('/');
            if host.starts_with("api.") {
                format!("https://{host}")
            } else {
                format!("https://api.{host}")
            }
        }
    }
}

fn user_fields(user: &Value) -> (Option<String>, Option<String>) {
    let Some(object) = user.as_object() else {
        return (None, None);
    };
    (text(object, "login"), text(object, "email"))
}

/// Pure parser for `/copilot_internal/user`. Returns None when no quota snapshot parses.
pub(crate) fn parse_quota(value: &Value, fetched_at_ms: u64) -> Option<UsageReport> {
    let object = value.as_object()?;
    let plan = text(object, "copilot_plan");
    let quota_reset_date = text(object, "quota_reset_date");
    let reset_ms = quota_reset_date.as_deref().and_then(parse_reset);
    let snapshots = object.get("quota_snapshots").and_then(Value::as_object)?;

    let mut limits = Vec::new();
    for (key, id, label) in [
        (
            "premium_interactions",
            "copilot:premium",
            "Premium Requests",
        ),
        ("chat", "copilot:chat", "Chat Requests"),
        ("completions", "copilot:completions", "Completions"),
    ] {
        let Some(snapshot) = snapshots.get(key).and_then(parse_snapshot) else {
            continue;
        };
        // Premium is always shown; chat and completions only when they are metered.
        if key != "premium_interactions" && snapshot.unlimited {
            continue;
        }
        limits.push(quota_limit(id, label, &snapshot, plan.as_deref(), reset_ms));
    }
    if limits.is_empty() {
        return None;
    }

    let mut metadata = BTreeMap::new();
    if let Some(plan) = plan {
        metadata.insert("plan".to_owned(), plan);
    }
    if let Some(date) = quota_reset_date {
        metadata.insert("quotaResetDate".to_owned(), date);
    }
    Some(UsageReport {
        provider: PROVIDER.to_owned(),
        fetched_at_ms,
        limits,
        reset_credits: None,
        notes: Vec::new(),
        metadata,
    })
}

struct Snapshot {
    entitlement: f64,
    remaining: f64,
    unlimited: bool,
    overage_count: f64,
}

fn parse_snapshot(value: &Value) -> Option<Snapshot> {
    let object = value.as_object()?;
    Some(Snapshot {
        entitlement: object.get("entitlement").and_then(Value::as_f64)?,
        remaining: object.get("remaining").and_then(Value::as_f64)?,
        unlimited: object.get("unlimited").and_then(Value::as_bool) == Some(true),
        overage_count: object
            .get("overage_count")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
    })
}

/// `quota_reset_date` is either a date (`YYYY-MM-DD`, UTC midnight) or a full ISO timestamp.
fn parse_reset(raw: &str) -> Option<u64> {
    if raw.len() == 10 {
        parse_iso_ms(&format!("{raw}T00:00:00Z"))
    } else {
        parse_iso_ms(raw)
    }
}

fn quota_limit(
    id: &str,
    label: &str,
    snapshot: &Snapshot,
    plan: Option<&str>,
    reset_ms: Option<u64>,
) -> UsageLimit {
    let entitlement = snapshot.entitlement;
    let (used, used_fraction, remaining_fraction) = if snapshot.unlimited {
        (None, None, None)
    } else {
        let used = (entitlement - snapshot.remaining).max(0.0);
        if entitlement > 0.0 {
            (
                Some(used),
                Some(used / entitlement),
                Some(snapshot.remaining / entitlement),
            )
        } else {
            (Some(used), None, None)
        }
    };
    let status = if snapshot.unlimited {
        UsageStatus::Ok
    } else {
        match remaining_fraction {
            None => UsageStatus::Unknown,
            Some(fraction) if fraction <= 0.0 => UsageStatus::Exhausted,
            Some(fraction) if fraction <= 0.1 => UsageStatus::Warning,
            Some(_) => UsageStatus::Ok,
        }
    };
    let mut notes = Vec::new();
    if snapshot.unlimited {
        notes.push("Unlimited".to_owned());
    }
    if snapshot.overage_count > 0.0 {
        notes.push(format!(
            "Overage requests: {}",
            snapshot.overage_count as u64
        ));
    }
    UsageLimit {
        id: id.to_owned(),
        label: label.to_owned(),
        scope: UsageScope {
            provider: PROVIDER.to_owned(),
            tier: plan.map(str::to_owned),
            window_id: reset_ms.map(|_| MONTHLY_WINDOW_ID.to_owned()),
            ..Default::default()
        },
        window: reset_ms.map(|resets_at_ms| UsageWindow {
            id: MONTHLY_WINDOW_ID.to_owned(),
            label: "Monthly".to_owned(),
            duration_ms: None,
            resets_at_ms: Some(resets_at_ms),
            reset_label: None,
        }),
        amount: UsageAmount {
            used,
            limit: Some(entitlement),
            remaining: Some(snapshot.remaining),
            used_fraction,
            remaining_fraction,
            unit: UsageUnit::Requests,
        },
        status: Some(status),
        notes,
    }
}

fn text(object: &Map<String, Value>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests;

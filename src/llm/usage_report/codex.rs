//! OpenAI Codex (ChatGPT OAuth) usage: `GET /wham/usage` for the plan windows, additional metered
//! limits, and credits; the reset-credit inventory and consume POST; and the Daybreak access badge.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value};

use super::claude::usage_status;
use super::types::{
    UsageAmount, UsageLimit, UsageReport, UsageResetCreditDetail, UsageResetCredits, UsageScope,
    UsageStatus, UsageUnit, UsageWindow,
};
use crate::llm::chatgpt;
use crate::llm::oauth::util::{self, Body, now_ms};

const PROVIDER: &str = "openai-codex";
const USER_AGENT: &str = concat!(
    "dial-agent/",
    env!("CARGO_PKG_VERSION"),
    " (coding agent; usage)"
);
const ORIGINATOR: &str = "dial";
const CLIENT_VERSION: &str = "0.159.0";
const DAY_SECONDS: u64 = 86_400;
const HOUR_SECONDS: u64 = 3_600;
/// Plan windows reported on `rate_limit`, with their stable window keys.
const PLAN_WINDOWS: [(&str, &str); 2] = [
    ("primary_window", "primary"),
    ("secondary_window", "secondary"),
];

pub(super) fn fetch(user_dir: &Path, cancel: &Arc<AtomicBool>) -> Result<Option<UsageReport>> {
    if !chatgpt::accounts(user_dir)?
        .iter()
        .any(|account| account.active && account.signed_in)
    {
        return Ok(None);
    }
    let (account_id, token) = chatgpt::account_access(user_dir, Arc::clone(cancel))?;
    let access = token.as_str();

    let body = get_json("/wham/usage", access, &account_id, cancel)?;
    let mut report = parse_usage(&body, now_ms())
        .ok_or_else(|| anyhow!("OpenAI Codex usage response had no rate-limit data"))?;

    let email = body
        .get("email")
        .and_then(Value::as_str)
        .map(|email| email.trim().to_ascii_lowercase())
        .filter(|email| !email.is_empty());
    for limit in &mut report.limits {
        limit.scope.account_id = Some(account_id.clone());
    }
    report
        .metadata
        .insert("accountId".to_owned(), account_id.clone());
    report.metadata.insert(
        "account".to_owned(),
        email.clone().unwrap_or_else(|| account_id.clone()),
    );
    if let Some(email) = email {
        report.metadata.insert("email".to_owned(), email);
    }

    let available = reset_credit_count(&body);
    if available > 0 {
        let credits = match get_json(
            "/wham/rate-limit-reset-credits",
            access,
            &account_id,
            cancel,
        ) {
            Ok(list) => parse_reset_credits(&list, available),
            Err(error) => {
                report
                    .notes
                    .push(format!("Reset credits unavailable: {error:#}"));
                UsageResetCredits {
                    available_count: available,
                    ..Default::default()
                }
            }
        };
        report.reset_credits = Some(credits);
    }

    // Daybreak is a display badge only: a failed lookup omits it and says so in the report notes.
    match get_json("/accounts/verified_access", access, &account_id, cancel) {
        Ok(verified) => {
            if daybreak_enabled(&verified) {
                report
                    .metadata
                    .insert("daybreak".to_owned(), "true".to_owned());
            }
        }
        Err(error) => report
            .notes
            .push(format!("Daybreak access unavailable: {error:#}")),
    }

    Ok(Some(report))
}

fn get_json(path: &str, token: &str, account_id: &str, cancel: &Arc<AtomicBool>) -> Result<Value> {
    let bearer = format!("Bearer {token}");
    let url = format!("{}{path}", chatgpt::BASE_URL);
    let response = util::request(
        "GET",
        &url,
        &[
            ("Authorization", bearer.as_str()),
            ("ChatGPT-Account-Id", account_id),
            ("Accept", "application/json"),
            ("User-Agent", USER_AGENT),
            ("originator", ORIGINATOR),
            ("version", CLIENT_VERSION),
        ],
        Body::Empty,
        cancel,
    )?;
    if !response.ok() {
        bail!(
            "OpenAI Codex usage endpoint returned HTTP {}",
            response.status
        );
    }
    response.json()
}

/// Outcome of `POST /wham/rate-limit-reset-credits/consume`. `ok` is true only for `reset`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexResetConsume {
    pub ok: bool,
    pub code: String,
    pub status: u16,
}

/// Spend one saved reset credit. `redeem_request_id` is the idempotency key: retrying with the
/// same id cannot double-spend. Backend business outcomes (`already_redeemed`, `no_credit`,
/// `nothing_to_reset`, or unknown future codes) are returned verbatim in `code`.
pub fn consume_reset_credit(
    user_dir: &Path,
    credit_id: &str,
    redeem_request_id: &str,
    cancel: &Arc<AtomicBool>,
) -> Result<CodexResetConsume> {
    if credit_id.trim().is_empty() {
        bail!("OpenAI Codex reset credit id is empty");
    }
    let (account_id, token) = chatgpt::account_access(user_dir, Arc::clone(cancel))?;
    let bearer = format!("Bearer {}", token.as_str());
    let url = format!(
        "{}/wham/rate-limit-reset-credits/consume",
        chatgpt::BASE_URL
    );
    let payload = serde_json::json!({
        "credit_id": credit_id,
        "redeem_request_id": redeem_request_id,
        "account_id": account_id,
    });
    let response = util::request(
        "POST",
        &url,
        &[
            ("Authorization", bearer.as_str()),
            ("ChatGPT-Account-Id", account_id.as_str()),
            ("Accept", "application/json"),
            ("User-Agent", USER_AGENT),
            ("originator", ORIGINATOR),
            ("version", CLIENT_VERSION),
        ],
        Body::Json(&payload),
        cancel,
    )?;
    let body = response.json_or_null();
    let code = body
        .get("code")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| {
            if response.ok() {
                "reset".to_owned()
            } else {
                format!("http_{}", response.status)
            }
        });
    Ok(CodexResetConsume {
        ok: code == "reset",
        code,
        status: response.status,
    })
}

/// Pure parser for `/wham/usage`. Returns None when the payload has neither `rate_limit` nor
/// `additional_rate_limits`. Account id and email are added by the caller.
pub(crate) fn parse_usage(value: &Value, fetched_at_ms: u64) -> Option<UsageReport> {
    let object = value.as_object()?;
    let rate = object.get("rate_limit").and_then(Value::as_object);
    let extras = object
        .get("additional_rate_limits")
        .and_then(Value::as_array);
    if rate.is_none() && extras.is_none() {
        return None;
    }
    let now = fetched_at_ms;

    let plan_meter = rate.map(meter_of).unwrap_or_default();
    let credits = object.get("credits").and_then(Value::as_object);
    let spend_reached = object
        .get("spend_control")
        .and_then(Value::as_object)
        .and_then(|spend| spend.get("reached"))
        .and_then(Value::as_bool);
    let credit_overage = plan_meter.limit_reached == Some(true)
        && credits.is_some_and(|c| flag(c, "unlimited") || flag(c, "has_credits"))
        && !credits.is_some_and(|c| flag(c, "overage_limit_reached"))
        && spend_reached != Some(true);

    let mut limits = Vec::new();
    if let Some(rate) = rate {
        for (key, window_key) in PLAN_WINDOWS {
            let Some(window) = rate.get(key).and_then(|value| parse_window(value, now)) else {
                continue;
            };
            let info = window_label(&window, window_key);
            limits.push(window_limit(
                format!("{PROVIDER}:{window_key}"),
                info.label.clone(),
                UsageScope {
                    provider: PROVIDER.to_owned(),
                    window_id: Some(window_key.to_owned()),
                    shared: Some(true),
                    ..Default::default()
                },
                &window,
                info,
                plan_meter,
                credit_overage,
            ));
        }
    }

    for entry in extras.into_iter().flatten() {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let limit_name = text(entry, "limit_name");
        let metered = text(entry, "metered_feature");
        let Some(rate) = entry.get("rate_limit").and_then(Value::as_object) else {
            continue;
        };
        let meter = meter_of(rate);
        let (slug, display) = meter_identity(limit_name.as_deref(), metered.as_deref());
        for (key, window_key) in PLAN_WINDOWS {
            let Some(window) = rate.get(key).and_then(|value| parse_window(value, now)) else {
                continue;
            };
            let info = window_label(&window, window_key);
            let label = format!("{} ({display})", info.label);
            limits.push(window_limit(
                format!("{PROVIDER}:{slug}:{window_key}"),
                label,
                UsageScope {
                    provider: PROVIDER.to_owned(),
                    tier: Some(slug.clone()),
                    model_id: limit_name.clone(),
                    window_id: Some(window_key.to_owned()),
                    shared: Some(true),
                    ..Default::default()
                },
                &window,
                info,
                meter,
                false,
            ));
        }
    }

    let mut metadata = BTreeMap::new();
    if let Some(plan) = text(object, "plan_type") {
        metadata.insert("planType".to_owned(), plan);
    }
    if plan_meter.limit_reached == Some(true) {
        metadata.insert("planState".to_owned(), "limitReached".to_owned());
    } else if plan_meter.allowed == Some(true) {
        metadata.insert("planState".to_owned(), "allowed".to_owned());
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

/// Reset-credit count from `/wham/usage`: truncated and clamped at zero.
fn reset_credit_count(value: &Value) -> u64 {
    value
        .get("rate_limit_reset_credits")
        .and_then(|credits| credits.get("available_count"))
        .and_then(Value::as_f64)
        .map(|count| count.trunc().max(0.0) as u64)
        .unwrap_or(0)
}

/// Pure parser for `/wham/rate-limit-reset-credits`. Only credits with status `available` (default
/// when absent) are kept. `available_count` comes from the list when present, else `fallback_count`.
pub(crate) fn parse_reset_credits(value: &Value, fallback_count: u64) -> UsageResetCredits {
    let credits: Vec<UsageResetCreditDetail> = value
        .get("credits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_reset_credit)
        .collect();
    let available_count = value
        .get("available_count")
        .and_then(Value::as_f64)
        .map(|count| count.trunc().max(0.0) as u64)
        .unwrap_or(fallback_count);
    UsageResetCredits {
        available_count,
        next_credit_id: credits.first().and_then(|credit| credit.id.clone()),
        credits,
        ..Default::default()
    }
}

fn parse_reset_credit(value: &Value) -> Option<UsageResetCreditDetail> {
    let object = value.as_object()?;
    let id = text(object, "id")?;
    let status = text(object, "status").unwrap_or_else(|| "available".to_owned());
    if status != "available" {
        return None;
    }
    Some(UsageResetCreditDetail {
        id: Some(id),
        title: text(object, "title"),
        status: Some(status),
        granted_at: text(object, "granted_at"),
        expires_at: text(object, "expires_at"),
        ..Default::default()
    })
}

/// Daybreak access: a `cyber` program that is not inactive, or that has grants.
pub(crate) fn daybreak_enabled(value: &Value) -> bool {
    value
        .get("programs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .any(|program| {
            text(program, "program").as_deref() == Some("cyber")
                && (text(program, "state").as_deref() != Some("inactive")
                    || program
                        .get("grants")
                        .and_then(Value::as_array)
                        .is_some_and(|grants| !grants.is_empty()))
        })
}

#[derive(Clone, Copy, Default)]
struct Meter {
    allowed: Option<bool>,
    limit_reached: Option<bool>,
}

fn meter_of(rate: &Map<String, Value>) -> Meter {
    Meter {
        allowed: rate.get("allowed").and_then(Value::as_bool),
        limit_reached: rate.get("limit_reached").and_then(Value::as_bool),
    }
}

#[derive(Clone, Copy)]
struct Window {
    used_percent: Option<f64>,
    seconds: Option<u64>,
    reset_at_ms: Option<u64>,
}

fn parse_window(value: &Value, now: u64) -> Option<Window> {
    let object = value.as_object()?;
    let used_percent = object.get("used_percent").and_then(Value::as_f64);
    let seconds = object
        .get("limit_window_seconds")
        .and_then(Value::as_f64)
        .filter(|seconds| *seconds > 0.0)
        .map(|seconds| seconds.round() as u64);
    let reset_at_ms = object
        .get("reset_at")
        .and_then(Value::as_f64)
        .filter(|time| *time > 0.0)
        .map(|time| {
            if time > 1e12 {
                time as u64
            } else {
                (time * 1000.0) as u64
            }
        })
        .or_else(|| {
            object
                .get("reset_after_seconds")
                .and_then(Value::as_f64)
                .filter(|seconds| *seconds >= 0.0)
                .map(|seconds| now.saturating_add((seconds * 1000.0) as u64))
        });
    Some(Window {
        used_percent,
        seconds,
        reset_at_ms,
    })
}

struct WindowLabel {
    id: String,
    label: String,
    duration_ms: Option<u64>,
}

/// `5h`/`5 hours` for sub-day windows, `7d`/`7 days` for day-or-longer windows. Without a
/// duration, the plan window key names the window.
fn window_label(window: &Window, window_key: &str) -> WindowLabel {
    match window.seconds {
        Some(seconds) if seconds >= DAY_SECONDS => {
            let days = ((seconds as f64) / DAY_SECONDS as f64).round().max(1.0) as u64;
            WindowLabel {
                id: format!("{days}d"),
                label: format!("{days} {}", plural(days, "day")),
                duration_ms: Some(seconds.saturating_mul(1000)),
            }
        }
        Some(seconds) => {
            let hours = ((seconds as f64) / HOUR_SECONDS as f64).round().max(1.0) as u64;
            WindowLabel {
                id: format!("{hours}h"),
                label: format!("{hours} {}", plural(hours, "hour")),
                duration_ms: Some(seconds.saturating_mul(1000)),
            }
        }
        None => {
            let (id, label) = if window_key == "primary" {
                ("primary", "Primary window")
            } else {
                ("secondary", "Secondary window")
            };
            WindowLabel {
                id: id.to_owned(),
                label: label.to_owned(),
                duration_ms: None,
            }
        }
    }
}

fn plural(count: u64, word: &str) -> String {
    if count == 1 {
        word.to_owned()
    } else {
        format!("{word}s")
    }
}

#[allow(clippy::too_many_arguments)]
fn window_limit(
    id: String,
    label: String,
    scope: UsageScope,
    window: &Window,
    info: WindowLabel,
    meter: Meter,
    credit_overage: bool,
) -> UsageLimit {
    let amount = match window.used_percent.map(|used| used.clamp(0.0, 100.0)) {
        Some(used) => UsageAmount {
            used: Some(used),
            limit: Some(100.0),
            remaining: Some(100.0 - used),
            used_fraction: Some(used / 100.0),
            remaining_fraction: Some((100.0 - used) / 100.0),
            unit: UsageUnit::Percent,
        },
        None => UsageAmount {
            used: None,
            limit: None,
            remaining: None,
            used_fraction: None,
            remaining_fraction: None,
            unit: UsageUnit::Percent,
        },
    };
    let status = meter_status(amount.used_fraction, meter, credit_overage);
    UsageLimit {
        id,
        label,
        scope,
        window: Some(UsageWindow {
            id: info.id,
            label: info.label,
            duration_ms: info.duration_ms,
            resets_at_ms: window.reset_at_ms,
            reset_label: None,
        }),
        amount,
        status,
        notes: Vec::new(),
    }
}

/// A window at 100% is still usable (Warning, not Exhausted) when the request is allowed or the
/// plan is funded by paid credits.
fn meter_status(fraction: Option<f64>, meter: Meter, credit_overage: bool) -> Option<UsageStatus> {
    let status = usage_status(fraction);
    let still_usable = (credit_overage && meter.limit_reached == Some(true))
        || (meter.allowed == Some(true) && meter.limit_reached == Some(false));
    if matches!(status, Some(UsageStatus::Exhausted)) && still_usable {
        Some(UsageStatus::Warning)
    } else {
        status
    }
}

/// Stable slug and display name for an additional meter. Spark meters are recognised by name.
fn meter_identity(limit_name: Option<&str>, metered: Option<&str>) -> (String, String) {
    let haystack =
        format!("{} {}", limit_name.unwrap_or(""), metered.unwrap_or("")).to_ascii_lowercase();
    if haystack.contains("spark") || haystack.contains("bengalfox") {
        return ("spark".to_owned(), "Spark".to_owned());
    }
    let source = metered
        .or(limit_name)
        .unwrap_or("extra")
        .to_ascii_lowercase();
    let stripped = source
        .strip_prefix("codex-")
        .or_else(|| source.strip_prefix("codex_"))
        .unwrap_or(source.as_str());
    let slug = slugify(stripped);
    let display = limit_name
        .map(str::to_owned)
        .unwrap_or_else(|| title_case(&slug));
    (slug, display)
}

fn slugify(text: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    let trimmed = out.trim_end_matches('-');
    if trimmed.is_empty() {
        "extra".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn title_case(slug: &str) -> String {
    slug.split('-')
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn flag(object: &Map<String, Value>, key: &str) -> bool {
    object.get(key).and_then(Value::as_bool) == Some(true)
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
mod tests {
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
}

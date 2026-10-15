//! Claude (Anthropic OAuth) usage: `GET /api/oauth/usage` for windows, extra spend, and the
//! reset-credit inventory, plus `/profile` for identity. Reset consumption is the
//! `reset_rate_limits` POST in [`consume_reset`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value};

use super::types::{
    UsageAmount, UsageLimit, UsageReport, UsageResetCreditDetail, UsageResetCredits, UsageScope,
    UsageStatus, UsageUnit, UsageWindow,
};
use crate::llm::anthropic_oauth;
use crate::llm::oauth::util::{self, Body, now_ms};

const PROVIDER: &str = "anthropic";
const API_ROOT: &str = "https://api.anthropic.com/api/oauth";
const USER_AGENT: &str = "claude-cli/2.1.280 (external, cli)";
const USAGE_BETAS: &str = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,advanced-tool-use-2025-11-20,effort-2025-11-24,extended-cache-ttl-2025-04-11";
const MAX_ATTEMPTS: u32 = 3;
const BASE_RETRY_DELAY_MS: u64 = 500;
const MAX_RETRY_AFTER_MS: u64 = 10_000;
const FIVE_HOUR_MS: u64 = 5 * 60 * 60 * 1_000;
const SEVEN_DAY_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
/// Top-level keys that mark a body as a usage payload (vs. some unrelated JSON document).
const USAGE_KEYS: [&str; 7] = [
    "five_hour",
    "seven_day",
    "limits",
    "extra_usage",
    "spend",
    "cedar_ember",
    "juniper_tide",
];

pub(super) fn fetch(user_dir: &Path, cancel: &Arc<AtomicBool>) -> Result<Option<UsageReport>> {
    if !anthropic_oauth::signed_in(user_dir) {
        return Ok(None);
    }
    let token = anthropic_oauth::access_token(user_dir, Arc::clone(cancel))?;
    let token = token.as_str();
    let fetched_at_ms = now_ms();

    let usage = match fetch_json(&format!("{API_ROOT}/usage"), token, cancel)? {
        Some(value) if is_usage_shaped(&value) => value,
        _ => bail!("Claude usage endpoint is unavailable"),
    };
    let mut report = parse_usage(&usage, fetched_at_ms)
        .ok_or_else(|| anyhow!("Claude usage response contained no usage data"))?;

    let mut notes = Vec::new();
    let mut identity = usage_identity(&usage);
    match fetch_json(&format!("{API_ROOT}/profile"), token, cancel) {
        Ok(Some(profile)) => {
            let from_profile = profile_identity(&profile);
            identity.account_id = identity.account_id.or(from_profile.account_id);
            identity.email = identity.email.or(from_profile.email);
            identity.org_id = from_profile.org_id;
        }
        Ok(None) => {}
        Err(error) => notes.push(format!("Claude profile unavailable: {error}")),
    }

    report.reset_credits = match parse_reset_credits(&usage, fetched_at_ms) {
        Some(credits) => Some(credits),
        None => discover_reset_credits(token, cancel, fetched_at_ms, &mut notes),
    };
    report.notes = notes;
    if let Some(email) = &identity.email {
        report.metadata.insert("email".to_owned(), email.clone());
    }
    if let Some(account_id) = &identity.account_id {
        report
            .metadata
            .insert("accountId".to_owned(), account_id.clone());
    }
    if let Some(org_id) = &identity.org_id {
        report.metadata.insert("orgId".to_owned(), org_id.clone());
    }
    if let Some(account) = identity.email.as_ref().or(identity.account_id.as_ref()) {
        report
            .metadata
            .insert("account".to_owned(), account.clone());
    }
    report
        .metadata
        .insert("endpoint".to_owned(), API_ROOT.to_owned());
    Ok(Some(report))
}

/// GETs a Claude OAuth endpoint, retrying transient failures. `Ok(None)` means the endpoint is
/// absent on this host (404/405/410/501, an empty body, or a non-JSON body).
fn fetch_json(url: &str, token: &str, cancel: &Arc<AtomicBool>) -> Result<Option<Value>> {
    let authorization = format!("Bearer {token}");
    for attempt in 0..MAX_ATTEMPTS {
        let headers = [
            ("accept", "application/json, text/plain, */*"),
            ("anthropic-beta", USAGE_BETAS),
            ("authorization", authorization.as_str()),
            ("user-agent", USER_AGENT),
        ];
        let response = util::request("GET", url, &headers, Body::Empty, cancel)?;
        let status = response.status;
        if status == 429 {
            bail!("Claude usage request was rate limited");
        }
        if (500..600).contains(&status) && status != 501 {
            if attempt + 1 == MAX_ATTEMPTS {
                bail!("Claude usage request failed with HTTP {status}");
            }
            let retry_after_ms = response
                .header("retry-after")
                .and_then(|value| value.trim().parse::<u64>().ok())
                .map_or(0, |secs| secs.saturating_mul(1_000).min(MAX_RETRY_AFTER_MS));
            pause(attempt, retry_after_ms, cancel)?;
            continue;
        }
        if matches!(status, 404 | 405 | 410 | 501) {
            return Ok(None);
        }
        if !(200..300).contains(&status) {
            bail!("Claude usage request failed with HTTP {status}");
        }
        if response.text.trim().is_empty() {
            return Ok(None);
        }
        return Ok(serde_json::from_str::<Value>(&response.text).ok());
    }
    bail!("Claude usage request retries exhausted")
}

const CEDAR_PROGRAM: &str = "cedar_ember";
pub(super) const JUNIPER_PROGRAM: &str = "juniper_tide";
const CLAUDE_OAUTH_BETA: &str = "oauth-2025-04-20";
/// Account routes (`/api/organizations/...`) live at the API root, not under `/api/oauth`.
const CLAUDE_API_HOST: &str = "https://api.anthropic.com";

/// Outcome of a Claude reset consume. `ok` is true only when the server reports `reset`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeResetConsume {
    pub ok: bool,
    pub code: String,
    pub status: u16,
    pub reason: Option<String>,
}

/// Spend one Claude saved reset. `program` and `credit_id` come from the reset inventory
/// (`UsageResetCreditDetail`). Cedar needs its grant id and an idempotency `redeem_request_id`
/// (generated when omitted); Juniper uses its fixed program id. Mutations are never retried.
/// Backend outcomes (`already_used`, `not_limited`, unknown codes) are returned in `code`.
pub fn consume_reset(
    user_dir: &Path,
    program: &str,
    credit_id: &str,
    redeem_request_id: Option<&str>,
    cancel: &Arc<AtomicBool>,
) -> Result<ClaudeResetConsume> {
    let body = match program {
        CEDAR_PROGRAM => {
            if !valid_cedar_grant_id(credit_id) {
                bail!("Claude Cedar reset grant id is invalid");
            }
            let request_id = redeem_request_id.map_or_else(util::uuid_v4, str::to_owned);
            if !valid_redeem_request_id(&request_id) {
                bail!("Claude reset request id is invalid");
            }
            serde_json::json!({ "program": CEDAR_PROGRAM, "grant_id": credit_id, "request_id": request_id })
        }
        JUNIPER_PROGRAM => {
            if credit_id != JUNIPER_PROGRAM {
                bail!("Claude Juniper reset credit id is invalid");
            }
            serde_json::json!({ "program": JUNIPER_PROGRAM })
        }
        other => bail!("unknown Claude reset program `{other}`"),
    };
    if !anthropic_oauth::signed_in(user_dir) {
        bail!("Claude is not signed in");
    }
    let token = anthropic_oauth::access_token(user_dir, Arc::clone(cancel))?;
    let token = token.as_str();
    let org_id = fetch_json(&format!("{API_ROOT}/profile"), token, cancel)?
        .and_then(|profile| profile_identity(&profile).org_id)
        .ok_or_else(|| anyhow!("Claude organization is unavailable"))?;

    let authorization = format!("Bearer {token}");
    let url = format!("{CLAUDE_API_HOST}/api/organizations/{org_id}/reset_rate_limits");
    let response = util::request(
        "POST",
        &url,
        &[
            ("accept", "application/json"),
            ("anthropic-beta", CLAUDE_OAUTH_BETA),
            ("authorization", authorization.as_str()),
            ("user-agent", USER_AGENT),
        ],
        Body::Json(&body),
        cancel,
    )?;
    let status = response.status;
    if !response.ok() {
        let code = match status {
            401 | 403 => "auth_error".to_owned(),
            429 => "rate_limited".to_owned(),
            _ => format!("http_{status}"),
        };
        return Ok(ClaudeResetConsume {
            ok: false,
            code,
            status,
            reason: None,
        });
    }
    let payload = response.json_or_null();
    let Some(result) = payload
        .get("result")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|result| !result.is_empty())
    else {
        return Ok(ClaudeResetConsume {
            ok: false,
            code: "malformed_response".to_owned(),
            status,
            reason: None,
        });
    };
    let reason = match payload.get("reason") {
        None | Some(Value::Null) => None,
        Some(Value::String(reason)) => Some(reason.clone()),
        Some(_) => {
            return Ok(ClaudeResetConsume {
                ok: false,
                code: "malformed_response".to_owned(),
                status,
                reason: None,
            });
        }
    };
    let code = match result {
        "already_used" => "already_redeemed",
        "not_limited" => "nothing_to_reset",
        other => other,
    }
    .to_owned();
    Ok(ClaudeResetConsume {
        ok: code == "reset",
        code,
        status,
        reason,
    })
}

fn valid_cedar_grant_id(id: &str) -> bool {
    (1..=40).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn valid_redeem_request_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn pause(attempt: u32, retry_after_ms: u64, cancel: &AtomicBool) -> Result<()> {
    let wait = Duration::from_millis((BASE_RETRY_DELAY_MS << attempt).max(retry_after_ms));
    let end = Instant::now() + wait;
    loop {
        if cancel.load(Ordering::Relaxed) {
            bail!("Claude usage request cancelled");
        }
        let now = Instant::now();
        if now >= end {
            return Ok(());
        }
        // Sleep in short slices so a cancel during a long Retry-After is honored promptly.
        std::thread::sleep((end - now).min(Duration::from_millis(100)));
    }
}

/// Discovery of reset credits when the main payload carries none: Cedar first, then Juniper.
fn discover_reset_credits(
    token: &str,
    cancel: &Arc<AtomicBool>,
    now_ms: u64,
    notes: &mut Vec<String>,
) -> Option<UsageResetCredits> {
    for (label, query) in [
        ("Cedar", "cedar_ember=1&skip_spend=1"),
        ("Juniper", "at_wall=1&skip_spend=1"),
    ] {
        match fetch_json(&format!("{API_ROOT}/usage?{query}"), token, cancel) {
            Ok(Some(value)) => {
                if let Some(credits) = parse_reset_credits(&value, now_ms) {
                    return Some(credits);
                }
            }
            Ok(None) => {}
            Err(error) => notes.push(format!("Claude {label} reset credits unavailable: {error}")),
        }
    }
    None
}

fn is_usage_shaped(value: &Value) -> bool {
    USAGE_KEYS
        .iter()
        .any(|key| value.get(*key).is_some_and(|field| !field.is_null()))
}

/// Builds the usage report (limits and extra spend) from a usage payload. Pure.
pub(crate) fn parse_usage(payload: &Value, fetched_at_ms: u64) -> Option<UsageReport> {
    let limits = build_limits(payload);
    (!limits.is_empty()).then(|| UsageReport {
        provider: PROVIDER.to_owned(),
        fetched_at_ms,
        limits,
        reset_credits: None,
        notes: Vec::new(),
        metadata: BTreeMap::new(),
    })
}

fn build_limits(payload: &Value) -> Vec<UsageLimit> {
    let api = parse_api_limits(payload.get("limits"));
    let mut limits = Vec::new();

    if let Some(bucket) =
        parse_bucket(payload.get("five_hour")).or_else(|| api_bucket(&api, "session"))
    {
        limits.push(bucket_limit(
            &BucketSpec {
                id: "anthropic:5h",
                label: "Claude 5 Hour",
                tier: None,
                window_id: "5h",
                window_label: "5 hours",
                duration_ms: FIVE_HOUR_MS,
                shared: Some(true),
            },
            bucket,
        ));
    }
    if let Some(bucket) =
        parse_bucket(payload.get("seven_day")).or_else(|| api_bucket(&api, "weekly_all"))
    {
        limits.push(bucket_limit(
            &seven_day_spec("anthropic:7d", "Claude 7 Day", None, Some(true)),
            bucket,
        ));
    }
    for (key, id, label, tier) in [
        (
            "seven_day_opus",
            "anthropic:7d:opus",
            "Claude 7 Day (Opus)",
            "opus",
        ),
        (
            "seven_day_sonnet",
            "anthropic:7d:sonnet",
            "Claude 7 Day (Sonnet)",
            "sonnet",
        ),
    ] {
        if let Some(bucket) = parse_bucket(payload.get(key)) {
            limits.push(bucket_limit(
                &seven_day_spec(id, label, Some(tier), None),
                bucket,
            ));
        }
    }

    let mut used_ids: BTreeSet<String> = limits.iter().map(|limit| limit.id.clone()).collect();
    for entry in api.iter().filter(|entry| entry.kind == "weekly_scoped") {
        let Some(name) = entry.display_name.as_deref() else {
            continue;
        };
        let base_slug = slugify(name);
        if base_slug.is_empty() {
            continue;
        }
        let mut slug = base_slug.clone();
        let mut suffix = 2;
        while used_ids.contains(&format!("anthropic:7d:{slug}")) {
            slug = format!("{base_slug}-{suffix}");
            suffix += 1;
        }
        let id = format!("anthropic:7d:{slug}");
        let label = format!("Claude 7 Day ({name})");
        used_ids.insert(id.clone());
        limits.push(bucket_limit(
            &seven_day_spec(&id, &label, Some(slug.as_str()), None),
            Bucket {
                utilization: entry.percent,
                resets_at_ms: entry.resets_at_ms,
            },
        ));
    }

    if let Some(extra) = parse_extra(payload) {
        limits.push(extra_limit(extra));
    }
    limits
}

#[derive(Clone, Copy)]
struct Bucket {
    utilization: f64,
    resets_at_ms: Option<u64>,
}

struct BucketSpec<'a> {
    id: &'a str,
    label: &'a str,
    tier: Option<&'a str>,
    window_id: &'a str,
    window_label: &'a str,
    duration_ms: u64,
    shared: Option<bool>,
}

fn seven_day_spec<'a>(
    id: &'a str,
    label: &'a str,
    tier: Option<&'a str>,
    shared: Option<bool>,
) -> BucketSpec<'a> {
    BucketSpec {
        id,
        label,
        tier,
        window_id: "7d",
        window_label: "7 days",
        duration_ms: SEVEN_DAY_MS,
        shared,
    }
}

fn bucket_limit(spec: &BucketSpec<'_>, bucket: Bucket) -> UsageLimit {
    let amount = percent_amount(bucket.utilization);
    UsageLimit {
        id: spec.id.to_owned(),
        label: spec.label.to_owned(),
        scope: UsageScope {
            provider: PROVIDER.to_owned(),
            window_id: Some(spec.window_id.to_owned()),
            tier: spec.tier.map(str::to_owned),
            shared: spec.shared,
            ..UsageScope::default()
        },
        window: Some(UsageWindow {
            id: spec.window_id.to_owned(),
            label: spec.window_label.to_owned(),
            duration_ms: Some(spec.duration_ms),
            resets_at_ms: bucket.resets_at_ms,
            reset_label: None,
        }),
        status: usage_status(amount.used_fraction),
        amount,
        notes: Vec::new(),
    }
}

fn percent_amount(percent: f64) -> UsageAmount {
    let used = percent.clamp(0.0, 100.0);
    let fraction = used / 100.0;
    UsageAmount {
        used: Some(used),
        limit: Some(100.0),
        remaining: Some(100.0 - used),
        used_fraction: Some(fraction),
        remaining_fraction: Some((1.0 - fraction).max(0.0)),
        unit: UsageUnit::Percent,
    }
}

/// Status band for a used fraction: ok below 90%, warning below 100%, exhausted at 100%+.
pub(super) fn usage_status(used_fraction: Option<f64>) -> Option<UsageStatus> {
    Some(match used_fraction {
        None => UsageStatus::Unknown,
        Some(fraction) if fraction >= 1.0 => UsageStatus::Exhausted,
        Some(fraction) if fraction >= 0.9 => UsageStatus::Warning,
        Some(_) => UsageStatus::Ok,
    })
}

fn parse_bucket(value: Option<&Value>) -> Option<Bucket> {
    let object = value?.as_object()?;
    Some(Bucket {
        utilization: number(object.get("utilization"))?,
        resets_at_ms: object
            .get("resets_at")
            .and_then(Value::as_str)
            .and_then(parse_iso_ms),
    })
}

struct ApiLimit {
    kind: String,
    percent: f64,
    resets_at_ms: Option<u64>,
    display_name: Option<String>,
}

fn parse_api_limits(value: Option<&Value>) -> Vec<ApiLimit> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parse_api_limit).collect())
        .unwrap_or_default()
}

fn parse_api_limit(value: &Value) -> Option<ApiLimit> {
    let object = value.as_object()?;
    Some(ApiLimit {
        kind: object.get("kind")?.as_str()?.to_owned(),
        percent: number(object.get("percent"))?,
        resets_at_ms: object
            .get("resets_at")
            .and_then(Value::as_str)
            .and_then(parse_iso_ms),
        display_name: object
            .get("scope")
            .and_then(|scope| scope.get("model"))
            .and_then(|model| model.get("display_name"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
    })
}

fn api_bucket(limits: &[ApiLimit], kind: &str) -> Option<Bucket> {
    limits
        .iter()
        .find(|limit| limit.kind == kind)
        .map(|limit| Bucket {
            utilization: limit.percent,
            resets_at_ms: limit.resets_at_ms,
        })
}

fn slugify(name: &str) -> String {
    let mut slug = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_owned()
}

/// Extra-usage spend: `spend` (current) wins over the legacy `extra_usage` block.
struct Extra {
    used: f64,
    limit: Option<f64>,
}

fn parse_extra(payload: &Value) -> Option<Extra> {
    parse_spend(payload.get("spend")).or_else(|| parse_legacy_extra(payload.get("extra_usage")))
}

fn parse_spend(value: Option<&Value>) -> Option<Extra> {
    let spend = value?.as_object()?;
    if spend.get("enabled") != Some(&Value::Bool(true)) || !spend.contains_key("limit") {
        return None;
    }
    let used = money(spend.get("used")?.as_object()?, true)?;
    match spend.get("limit")? {
        Value::Null => Some(Extra { used, limit: None }),
        limit => {
            let limit = money(limit.as_object()?, true)?;
            (limit > 0.0).then_some(Extra {
                used,
                limit: Some(limit),
            })
        }
    }
}

fn parse_legacy_extra(value: Option<&Value>) -> Option<Extra> {
    let extra = value?.as_object()?;
    if extra.get("is_enabled") != Some(&Value::Bool(true)) || !extra.contains_key("monthly_limit") {
        return None;
    }
    let decimals = match extra.get("decimal_places") {
        None => 2,
        Some(value) => value.as_u64()?,
    };
    let used = dollars(
        extra.get("used_credits"),
        decimals,
        extra.get("currency"),
        false,
    )?;
    match extra.get("monthly_limit") {
        None | Some(Value::Null) => Some(Extra { used, limit: None }),
        Some(value) => {
            let limit = dollars(Some(value), decimals, extra.get("currency"), false)?;
            (limit > 0.0).then_some(Extra {
                used,
                limit: Some(limit),
            })
        }
    }
}

/// Reads a `{amount_minor, exponent, currency}` money object as dollars.
fn money(object: &Map<String, Value>, currency_required: bool) -> Option<f64> {
    let exponent = object.get("exponent")?.as_u64()?;
    dollars(
        object.get("amount_minor"),
        exponent,
        object.get("currency"),
        currency_required,
    )
}

fn dollars(
    amount_minor: Option<&Value>,
    exponent: u64,
    currency: Option<&Value>,
    currency_required: bool,
) -> Option<f64> {
    let minor = amount_minor?.as_f64()?;
    if !minor.is_finite() || minor < 0.0 || exponent > 300 {
        return None;
    }
    match currency {
        None if currency_required => return None,
        None => {}
        Some(Value::String(code)) if code.eq_ignore_ascii_case("USD") => {}
        Some(_) => return None,
    }
    let dollars = minor / 10f64.powi(exponent as i32);
    dollars.is_finite().then_some(dollars)
}

fn extra_limit(extra: Extra) -> UsageLimit {
    let (amount, status) = match extra.limit {
        None => (
            UsageAmount {
                used: Some(extra.used),
                limit: None,
                remaining: None,
                used_fraction: None,
                remaining_fraction: None,
                unit: UsageUnit::Usd,
            },
            None,
        ),
        Some(limit) => {
            let remaining = (limit - extra.used).max(0.0);
            let fraction = extra.used / limit;
            (
                UsageAmount {
                    used: Some(extra.used),
                    limit: Some(limit),
                    remaining: Some(remaining),
                    used_fraction: Some(fraction),
                    remaining_fraction: Some(remaining / limit),
                    unit: UsageUnit::Usd,
                },
                usage_status(Some(fraction)),
            )
        }
    };
    UsageLimit {
        id: "anthropic:extra".to_owned(),
        label: "Claude Extra Usage".to_owned(),
        scope: UsageScope {
            provider: PROVIDER.to_owned(),
            window_id: Some("extra".to_owned()),
            ..UsageScope::default()
        },
        window: None,
        amount,
        status,
        notes: Vec::new(),
    }
}

/// Reset-credit inventory from a usage payload: Cedar block, then Juniper. Pure.
pub(crate) fn parse_reset_credits(payload: &Value, now_ms: u64) -> Option<UsageResetCredits> {
    if let Some(block) = payload.get("cedar_ember").filter(|value| value.is_object()) {
        let cedar = parse_cedar(block, now_ms);
        if cedar
            .as_ref()
            .is_some_and(|credits| credits.available_count > 0 || credits.next_credit_id.is_some())
        {
            return cedar;
        }
        if let Some(juniper) = payload.get("juniper_tide").and_then(parse_juniper) {
            return Some(juniper);
        }
        return cedar;
    }
    payload.get("juniper_tide").and_then(parse_juniper)
}

fn parse_cedar(block: &Value, now_ms: u64) -> Option<UsageResetCredits> {
    let object = block.as_object()?;
    let eligible = object.get("eligible")?.as_bool()?;
    let reason = str_field(object, "ineligible_reason");
    let cooldown_until = str_field(object, "cooldown_until");
    let next_grant = str_field(object, "next_grant_id");
    let grants: Vec<UsageResetCreditDetail> = object
        .get("grants")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|grant| parse_grant(grant, now_ms))
                .collect()
        })
        .unwrap_or_default();
    let available_count: u64 = grants
        .iter()
        .filter(|grant| grant.status.as_deref() != Some("expired"))
        .fold(0, |total, grant| {
            total.saturating_add(grant.remaining_count.unwrap_or(0))
        });
    let redeemable_count = next_grant
        .as_deref()
        .and_then(|id| grants.iter().find(|grant| grant.id.as_deref() == Some(id)))
        .filter(|grant| grant.usable == Some(true))
        .and_then(|grant| grant.remaining_count)
        .unwrap_or(0);
    let cooldown_active = cooldown_until
        .as_deref()
        .and_then(parse_iso_ms)
        .is_some_and(|until| until > now_ms);
    Some(UsageResetCredits {
        available_count,
        redeemable_count: Some(redeemable_count),
        next_credit_id: if eligible { next_grant } else { None },
        eligible: Some(eligible),
        reason: reason.or_else(|| cooldown_active.then(|| "cooldown".to_owned())),
        cooldown_until,
        credits: grants,
    })
}

fn parse_grant(value: &Value, now_ms: u64) -> Option<UsageResetCreditDetail> {
    let object = value.as_object()?;
    let id = str_field(object, "id").filter(|id| valid_grant_id(id))?;
    let resets_left = object.get("resets_left")?.as_u64()?;
    let expires_at = str_field(object, "ends_at");
    let expired = expires_at
        .as_deref()
        .and_then(parse_iso_ms)
        .is_some_and(|end| end <= now_ms);
    let paused = object
        .get("paused")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let usable_now = object
        .get("usable_now")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let status = if expired {
        "expired"
    } else if resets_left == 0 {
        "redeemed"
    } else if paused {
        "paused"
    } else if usable_now {
        "available"
    } else {
        "unavailable"
    };
    let used_fractions = object
        .get("percent_used")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| {
                    let percent = value.as_u64().filter(|percent| *percent <= 100)?;
                    Some((normalize_limit_id(key), percent as f64 / 100.0))
                })
                .collect()
        })
        .unwrap_or_default();
    Some(UsageResetCreditDetail {
        id: Some(id),
        title: Some(str_field(object, "label").unwrap_or_else(|| "Claude limit reset".to_owned())),
        program: Some("cedar_ember".to_owned()),
        remaining_count: Some(resets_left),
        usable: Some(status == "available"),
        requires_limit: Some(
            object
                .get("use_requires_limit")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        ),
        clears: limit_ids(object.get("clears")),
        blocking: limit_ids(object.get("blocking")),
        used_fractions,
        granted_at: str_field(object, "starts_at"),
        expires_at,
        status: Some(status.to_owned()),
    })
}

fn parse_juniper(block: &Value) -> Option<UsageResetCredits> {
    let object = block.as_object()?;
    let eligible_flag = object.get("eligible")?.as_bool()?;
    let eligible = eligible_flag && object.get("arm").and_then(Value::as_str) == Some("reset");
    let available = object
        .get("available")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let usable = eligible && available;
    let next_available_at = str_field(object, "next_available_at");
    let credit = UsageResetCreditDetail {
        id: Some("juniper_tide".to_owned()),
        title: Some("Claude session limit reset".to_owned()),
        program: Some("juniper_tide".to_owned()),
        remaining_count: Some(u64::from(usable)),
        usable: Some(usable),
        requires_limit: Some(true),
        clears: vec!["anthropic:5h".to_owned()],
        blocking: Vec::new(),
        used_fractions: BTreeMap::new(),
        granted_at: None,
        expires_at: None,
        status: Some(if usable { "available" } else { "unavailable" }.to_owned()),
    };
    Some(UsageResetCredits {
        available_count: u64::from(usable),
        redeemable_count: Some(u64::from(usable)),
        next_credit_id: usable.then(|| "juniper_tide".to_owned()),
        eligible: Some(eligible),
        reason: str_field(object, "ineligible_reason"),
        cooldown_until: if usable { None } else { next_available_at },
        credits: vec![credit],
    })
}

fn valid_grant_id(id: &str) -> bool {
    (1..=40).contains(&id.len())
        && id
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
}

fn normalize_limit_id(raw: &str) -> String {
    match raw {
        "five_hour" => "anthropic:5h".to_owned(),
        "seven_day" => "anthropic:7d".to_owned(),
        "seven_day_opus" => "anthropic:7d:opus".to_owned(),
        "seven_day_sonnet" => "anthropic:7d:sonnet".to_owned(),
        other => format!("anthropic:reset:{other}"),
    }
}

fn limit_ids(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(normalize_limit_id)
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Default)]
struct Identity {
    account_id: Option<String>,
    email: Option<String>,
    org_id: Option<String>,
}

fn usage_identity(value: &Value) -> Identity {
    Identity {
        account_id: first_string(
            value,
            &[
                "account_id",
                "accountId",
                "user_id",
                "userId",
                "account.uuid",
                "account.id",
                "user.uuid",
                "user.id",
            ],
        ),
        email: first_string(
            value,
            &[
                "email",
                "user_email",
                "userEmail",
                "account.email",
                "user.email",
            ],
        ),
        org_id: None,
    }
}

fn profile_identity(value: &Value) -> Identity {
    Identity {
        account_id: first_string(value, &["uuid", "account.uuid"]),
        email: first_string(value, &["email", "account.email"]),
        org_id: first_string(value, &["organization.uuid", "organization_uuid"])
            .filter(|id| valid_org_id(id)),
    }
}

fn valid_org_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

/// First non-empty trimmed string at any dotted path (e.g. `account.uuid`).
fn first_string(value: &Value, paths: &[&str]) -> Option<String> {
    paths.iter().find_map(|path| {
        let found = path
            .split('.')
            .try_fold(value, |current, key| current.get(key))?;
        found
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    })
}

fn str_field(object: &Map<String, Value>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// Finite number from a JSON number or numeric string.
fn number(value: Option<&Value>) -> Option<f64> {
    let parsed = match value? {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    parsed.is_finite().then_some(parsed)
}

/// Parses an ISO-8601 timestamp with an explicit zone (`Z` or `±HH:MM`) into Unix milliseconds.
pub(super) fn parse_iso_ms(text: &str) -> Option<u64> {
    let text = text.trim();
    let (date, time) = text.split_once(['T', 't', ' '])?;
    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (clock, offset_minutes) = split_zone(time)?;
    let mut fields = clock.split(':');
    let hour: i64 = fields.next()?.parse().ok()?;
    let minute: i64 = fields.next()?.parse().ok()?;
    let seconds = fields.next()?;
    if fields.next().is_some() {
        return None;
    }
    let (second_text, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    let second: i64 = second_text.parse().ok()?;
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0..=60).contains(&second) {
        return None;
    }
    let millis: u64 = if fraction.is_empty() {
        0
    } else {
        let digits: String = fraction.chars().take(3).collect();
        if !digits.chars().all(|ch| ch.is_ascii_digit()) {
            return None;
        }
        format!("{digits:0<3}").parse().ok()?
    };
    let days = days_from_civil(year, month, day);
    let total =
        days * 86_400 + i128::from(hour) * 3_600 + i128::from(minute) * 60 + i128::from(second)
            - i128::from(offset_minutes) * 60;
    u64::try_from(total)
        .ok()?
        .checked_mul(1_000)?
        .checked_add(millis)
}

fn split_zone(time: &str) -> Option<(&str, i64)> {
    if let Some(clock) = time.strip_suffix(['Z', 'z']) {
        return Some((clock, 0));
    }
    let index = time.rfind(['+', '-'])?;
    let (clock, offset) = time.split_at(index);
    let sign = if offset.starts_with('-') { -1 } else { 1 };
    let mut parts = offset[1..].split(':');
    let hours: i64 = parts.next()?.parse().ok()?;
    let minutes: i64 = parts.next().unwrap_or("0").parse().ok()?;
    Some((clock, sign * hours.checked_mul(60)?.checked_add(minutes)?))
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i128 {
    let year = i128::from(year);
    let year = if month <= 2 { year - 1 } else { year };
    let era = (if year >= 0 { year } else { year - 399 }) / 400;
    let year_of_era = year - era * 400;
    let month = i128::from(month);
    let day_of_year =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i128::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use serde_json::json;

    use super::*;

    #[test]
    fn iso_timestamps_parse_with_zone() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_iso_ms("2026-01-01T00:00:00.500+01:00"),
            Some(1_767_222_000_500)
        );
        assert_eq!(parse_iso_ms("2026-01-01T00:00:00"), None);
        assert_eq!(parse_iso_ms("not a date"), None);
    }

    #[test]
    fn usage_payload_maps_windows_scoped_limits_and_spend() {
        let payload = json!({
            "five_hour": { "utilization": 42.5, "resets_at": "1970-01-01T05:00:00Z" },
            "seven_day": { "utilization": 95, "resets_at": "1970-01-08T00:00:00Z" },
            "seven_day_opus": { "utilization": 10 },
            "limits": [{
                "kind": "weekly_scoped",
                "percent": 30,
                "scope": { "model": { "display_name": "Claude Design" } }
            }],
            "spend": {
                "enabled": true,
                "limit": { "amount_minor": 10000, "exponent": 2, "currency": "USD" },
                "used": { "amount_minor": 2550, "exponent": 2, "currency": "USD" }
            }
        });
        let report = parse_usage(&payload, 1).unwrap();
        let ids: Vec<&str> = report
            .limits
            .iter()
            .map(|limit| limit.id.as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "anthropic:5h",
                "anthropic:7d",
                "anthropic:7d:opus",
                "anthropic:7d:claude-design",
                "anthropic:extra",
            ]
        );
        let five = &report.limits[0];
        assert_eq!(five.amount.used, Some(42.5));
        assert_eq!(five.status, Some(UsageStatus::Ok));
        assert_eq!(five.window.as_ref().unwrap().resets_at_ms, Some(18_000_000));
        assert_eq!(report.limits[1].status, Some(UsageStatus::Warning));
        assert_eq!(
            report.limits[3].scope.tier.as_deref(),
            Some("claude-design")
        );
        assert_eq!(report.limits[3].label, "Claude 7 Day (Claude Design)");
        let extra = &report.limits[4];
        assert_eq!(extra.amount.limit, Some(100.0));
        assert_eq!(extra.amount.used_fraction, Some(0.255));
        assert_eq!(extra.status, Some(UsageStatus::Ok));
    }

    #[test]
    fn legacy_extra_usage_and_used_only_spend() {
        let legacy = json!({
            "five_hour": { "utilization": 1 },
            "extra_usage": { "is_enabled": true, "monthly_limit": 5000, "used_credits": 1250, "decimal_places": 2 }
        });
        let extra = parse_usage(&legacy, 1).unwrap().limits.pop().unwrap();
        assert_eq!(extra.id, "anthropic:extra");
        assert_eq!(extra.amount.used, Some(12.5));
        assert_eq!(extra.amount.limit, Some(50.0));

        let used_only = json!({
            "five_hour": { "utilization": 1 },
            "spend": {
                "enabled": true,
                "limit": null,
                "used": { "amount_minor": 700, "exponent": 2, "currency": "USD" }
            }
        });
        let extra = parse_usage(&used_only, 1).unwrap().limits.pop().unwrap();
        assert_eq!(extra.amount.used, Some(7.0));
        assert_eq!(extra.amount.limit, None);
        assert_eq!(extra.status, None);
    }

    #[test]
    fn non_usd_spend_is_rejected() {
        let payload = json!({
            "five_hour": { "utilization": 1 },
            "spend": {
                "enabled": true,
                "limit": null,
                "used": { "amount_minor": 700, "exponent": 2, "currency": "EUR" }
            }
        });
        let report = parse_usage(&payload, 1).unwrap();
        assert!(
            report
                .limits
                .iter()
                .all(|limit| limit.id != "anthropic:extra")
        );
    }

    #[test]
    fn cedar_reset_credits_parse_with_normalized_ids() {
        let payload = json!({
            "cedar_ember": {
                "eligible": true,
                "next_grant_id": "g1",
                "grants": [{
                    "id": "g1",
                    "resets_left": 2,
                    "label": "Weekly",
                    "starts_at": "1970-01-01T00:00:00Z",
                    "ends_at": "2099-01-01T00:00:00Z",
                    "clears": ["five_hour"],
                    "blocking": ["seven_day_opus"],
                    "percent_used": { "five_hour": 80 },
                    "usable_now": true
                }]
            }
        });
        let credits = parse_reset_credits(&payload, 1_000).unwrap();
        assert_eq!(credits.available_count, 2);
        assert_eq!(credits.redeemable_count, Some(2));
        assert_eq!(credits.next_credit_id.as_deref(), Some("g1"));
        assert_eq!(credits.eligible, Some(true));
        let grant = &credits.credits[0];
        assert_eq!(grant.title.as_deref(), Some("Weekly"));
        assert_eq!(grant.usable, Some(true));
        assert_eq!(grant.clears, ["anthropic:5h"]);
        assert_eq!(grant.blocking, ["anthropic:7d:opus"]);
        assert_eq!(grant.used_fractions.get("anthropic:5h"), Some(&0.8));
    }

    #[test]
    fn juniper_cooldown_reports_next_available_time() {
        let payload = json!({
            "juniper_tide": {
                "eligible": true,
                "arm": "reset",
                "available": false,
                "next_available_at": "2099-01-01T00:00:00Z"
            }
        });
        let credits = parse_reset_credits(&payload, 1_000).unwrap();
        assert_eq!(credits.available_count, 0);
        assert_eq!(credits.eligible, Some(true));
        assert_eq!(
            credits.cooldown_until.as_deref(),
            Some("2099-01-01T00:00:00Z")
        );
        assert_eq!(credits.credits[0].usable, Some(false));
    }

    #[test]
    fn identity_and_profile_fields_extract() {
        let usage = json!({ "account": { "uuid": "u-1", "email": "a@example.com" } });
        let identity = usage_identity(&usage);
        assert_eq!(identity.account_id.as_deref(), Some("u-1"));
        assert_eq!(identity.email.as_deref(), Some("a@example.com"));

        let profile = json!({ "uuid": " u-2 ", "organization": { "uuid": "org_1" } });
        let identity = profile_identity(&profile);
        assert_eq!(identity.account_id.as_deref(), Some("u-2"));
        assert_eq!(identity.org_id.as_deref(), Some("org_1"));
        assert_eq!(
            profile_identity(&json!({ "organization": { "uuid": "bad id!" } })).org_id,
            None
        );
    }

    #[test]
    fn fetch_without_credentials_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        assert!(fetch(dir.path(), &cancel).unwrap().is_none());
    }
}

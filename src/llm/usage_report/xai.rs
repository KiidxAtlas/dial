//! SuperGrok (`xai-oauth`) usage.
//!
//! Two billing shapes are read from `cli-chat-proxy.grok.com/v1/billing`:
//! the legacy weekly credits shape (`?format=credits`) and the unified-billing monthly included
//! quota (default URL). Accounts may expose both, and both are reconciled.
use super::factory::{parse_iso_ms, to_number, usage_status};
use super::types::{
    UsageAmount, UsageLimit, UsageReport, UsageScope, UsageStatus, UsageUnit, UsageWindow,
};
use crate::llm::oauth::{self, util};
use anyhow::{Result, bail};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::{Arc, atomic::AtomicBool};

const PROVIDER: &str = "xai-oauth";
const FLOW_ID: &str = "xai-oauth";
const SOURCE: &str = "cli-chat-proxy.grok.com/v1/billing";
const CREDITS_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
const MONTHLY_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing";
const DAY_MS: u64 = 86_400_000;
const WEEK_MS: u64 = 7 * DAY_MS;

/// Legacy SuperGrok weekly credits (`config` of the `?format=credits` payload).
#[derive(Debug)]
struct Weekly {
    end_ms: u64,
    credit_percent: f64,
    /// True when `creditUsagePercent` was absent (fresh period or zero usage).
    inferred: bool,
    /// (trimmed product name, usage percent).
    products: Vec<(String, f64)>,
    on_demand_cap: Option<f64>,
    on_demand_used: Option<f64>,
}

/// Unified-billing monthly included quota (`config` of the default billing payload).
#[derive(Debug)]
struct Monthly {
    start_ms: u64,
    end_ms: u64,
    used: f64,
    limit: f64,
    on_demand_cap: Option<f64>,
    on_demand_used: Option<f64>,
}

/// Fetch the SuperGrok quota report. `Ok(None)` when no grant is stored for this provider.
pub(super) fn fetch(user_dir: &Path, cancel: &Arc<AtomicBool>) -> Result<Option<UsageReport>> {
    if !oauth::signed_in(user_dir, FLOW_ID) {
        return Ok(None);
    }
    let access = oauth::access(user_dir, FLOW_ID, cancel)?;
    let token = access.token.as_str();
    let account = access.account_id.clone().or_else(|| {
        util::jwt_claims(token)
            .and_then(|claims| claims.get("sub").and_then(Value::as_str).map(str::to_owned))
    });
    let now = util::now_ms();

    // Always probe the weekly credits shape first.
    let credits = fetch_billing(CREDITS_URL, token, cancel)?;
    let credits_config = config_of(credits.as_ref());
    let weekly_usable = credits_config
        .and_then(|config| parse_weekly(config, now))
        .is_some();
    // Unified accounts expose monthly included quota on the default URL; probe it when credits
    // is unusable or marks the account unified.
    let probe_monthly = !weekly_usable || credits_config.is_some_and(is_unified);
    let monthly = if probe_monthly {
        fetch_billing(MONTHLY_URL, token, cancel)?
    } else {
        None
    };

    let Some(mut report) = assemble(credits.as_ref(), monthly.as_ref(), account.as_deref(), now)
    else {
        bail!("SuperGrok billing endpoints returned no usable quota data");
    };
    if let Some(identity) = oauth::identity(user_dir, FLOW_ID) {
        report.metadata.insert("account".into(), identity);
    }
    Ok(Some(report))
}

/// One billing GET. Non-2xx (other than auth rejection) means this shape is absent; 401/403 and
/// transport/JSON failures are real errors.
fn fetch_billing(url: &str, token: &str, cancel: &Arc<AtomicBool>) -> Result<Option<Value>> {
    let bearer = format!("Bearer {token}");
    let response = util::request(
        "GET",
        url,
        &[
            ("Authorization", bearer.as_str()),
            ("Accept", "application/json"),
            ("X-XAI-Token-Auth", "xai-grok-cli"),
        ],
        util::Body::Empty,
        cancel,
    )?;
    match response.status {
        401 | 403 => bail!(
            "SuperGrok billing endpoint returned HTTP {}",
            response.status
        ),
        _ if !response.ok() => Ok(None),
        _ => Ok(Some(response.json()?)),
    }
}

/// Reconcile the two billing payloads into one report. Pure, so tests can drive it directly.
fn assemble(
    credits: Option<&Value>,
    monthly_payload: Option<&Value>,
    account: Option<&str>,
    now: u64,
) -> Option<UsageReport> {
    let credits_config = config_of(credits);
    let weekly = credits_config.and_then(|config| parse_weekly(config, now));
    let credits_unified = credits_config.is_some_and(is_unified);
    let monthly_config = config_of(monthly_payload);
    let monthly = monthly_config.and_then(parse_monthly);

    // A positive monthly quota normally wins over an inferred weekly percentage. Without a
    // confirming monthly payload, inferred weekly usage is rejected rather than trusted.
    let weekly_inferred = weekly.as_ref().is_some_and(|w| w.inferred);
    let mut effective_weekly = weekly.as_ref();
    if weekly_inferred
        && credits_unified
        && (monthly.is_some()
            || !monthly_config.is_some_and(|config| confirms_no_monthly_quota(config, now)))
    {
        effective_weekly = None;
    }
    // Monthly counter over its limit while weekly credits are active: the two shapes cannot say
    // which gates requests, so keep the counter visible with unknown status.
    let advisory =
        weekly_inferred && credits_unified && monthly.as_ref().is_some_and(|m| m.used >= m.limit);
    if effective_weekly.is_none() && monthly.is_none() {
        return None;
    }

    let mut limits = Vec::new();
    if let Some(weekly) = effective_weekly {
        limits.extend(weekly_limits(weekly, account));
    }
    if let Some(monthly) = &monthly {
        let mut rows = monthly_limits(monthly, account);
        if advisory && let Some(first) = rows.first_mut() {
            first.status = Some(UsageStatus::Unknown);
            first.notes = vec![
                "Monthly counter exceeds its limit, but active weekly credits leave enforcement uncertain."
                    .into(),
            ];
        }
        limits.extend(rows);
    }
    let mut seen = HashSet::new();
    limits.retain(|limit| seen.insert(limit.id.clone()));
    if limits.is_empty() {
        return None;
    }

    let (billing_kind, endpoint) = match (effective_weekly.is_some(), monthly.is_some()) {
        (true, true) => ("unified", format!("{CREDITS_URL} + {MONTHLY_URL}")),
        (true, false) => ("weekly", CREDITS_URL.to_owned()),
        _ => ("monthly", MONTHLY_URL.to_owned()),
    };
    let mut metadata = BTreeMap::new();
    metadata.insert("endpoint".into(), endpoint);
    metadata.insert("source".into(), SOURCE.into());
    metadata.insert("billingKind".into(), billing_kind.into());
    if advisory {
        metadata.insert("monthlyQuotaAdvisory".into(), "true".into());
    }
    if let Some(account) = account {
        metadata.insert("accountId".into(), account.into());
    }
    Some(UsageReport {
        provider: PROVIDER.into(),
        fetched_at_ms: now,
        limits,
        reset_credits: None,
        notes: Vec::new(),
        metadata,
    })
}

/// `payload.config` when both the payload and config are objects.
fn config_of(payload: Option<&Value>) -> Option<&Map<String, Value>> {
    payload?.get("config")?.as_object()
}

fn is_unified(config: &Map<String, Value>) -> bool {
    config.get("isUnifiedBillingUser") == Some(&Value::Bool(true))
}

/// `{ "val": number }` money object, non-negative.
fn money(value: Option<&Value>) -> Option<f64> {
    let amount = to_number(value?.as_object()?.get("val")?)?;
    (amount >= 0.0).then_some(amount)
}

/// Percent in [0, 100].
fn percent(value: &Value) -> Option<f64> {
    to_number(value).filter(|p| (0.0..=100.0).contains(p))
}

fn parse_weekly(config: &Map<String, Value>, now: u64) -> Option<Weekly> {
    let period = config.get("currentPeriod")?.as_object()?;
    let start = period
        .get("start")
        .and_then(Value::as_str)
        .and_then(parse_iso_ms)?;
    let end = period
        .get("end")
        .and_then(Value::as_str)
        .and_then(parse_iso_ms)?;
    let kind = period.get("type").and_then(Value::as_str).unwrap_or("");
    if end <= start || !kind.to_uppercase().contains("WEEK") {
        return None;
    }

    // Fresh weekly periods (or zero usage) omit creditUsagePercent; default to 0 only while the
    // period is active. Expired periods without usage data are rejected.
    let raw = config.get("creditUsagePercent").filter(|v| !v.is_null());
    let inferred = raw.is_none();
    let credit_percent = match raw {
        None if end > now => 0.0,
        None => return None,
        Some(value) => percent(value)?,
    };

    let mut products = Vec::new();
    match config.get("productUsage") {
        None => {}
        Some(Value::Array(items)) => {
            for item in items {
                let Some(item) = item.as_object() else {
                    continue;
                };
                let product = item
                    .get("product")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                let usage = match item.get("usagePercent") {
                    None | Some(Value::Null) => Some(0.0),
                    Some(value) => percent(value),
                };
                if let Some(usage) = usage
                    && !product.is_empty()
                {
                    products.push((product.to_owned(), usage));
                }
            }
        }
        Some(_) => return None,
    }

    Some(Weekly {
        end_ms: end,
        credit_percent,
        inferred,
        products,
        on_demand_cap: money(config.get("onDemandCap")),
        on_demand_used: money(config.get("onDemandUsed")),
    })
}

fn parse_monthly(config: &Map<String, Value>) -> Option<Monthly> {
    let start = config
        .get("billingPeriodStart")
        .and_then(Value::as_str)
        .and_then(parse_iso_ms)?;
    let end = config
        .get("billingPeriodEnd")
        .and_then(Value::as_str)
        .and_then(parse_iso_ms)?;
    if end <= start {
        return None;
    }
    // A positive included quota is required; zero or missing is not a usable report.
    let limit = money(config.get("monthlyLimit"))?;
    if limit <= 0.0 {
        return None;
    }
    let used = money(config.get("used"))?;
    Some(Monthly {
        start_ms: start,
        end_ms: end,
        used,
        limit,
        on_demand_cap: money(config.get("onDemandCap")),
        on_demand_used: money(config.get("onDemandUsed")),
    })
}

fn confirms_no_monthly_quota(config: &Map<String, Value>, now: u64) -> bool {
    match money(config.get("monthlyLimit")) {
        Some(limit) => limit == 0.0,
        // Some weekly accounts return the credits shape from the default endpoint too.
        None => parse_weekly(config, now).is_some_and(|w| w.inferred),
    }
}

fn percent_limit(
    id: String,
    label: String,
    percent: f64,
    scope: UsageScope,
    window: UsageWindow,
) -> UsageLimit {
    let fraction = percent / 100.0;
    UsageLimit {
        id,
        label,
        scope,
        window: Some(window),
        amount: UsageAmount {
            used: Some(percent),
            limit: Some(100.0),
            remaining: Some(100.0 - percent),
            used_fraction: Some(fraction),
            remaining_fraction: Some(1.0 - fraction),
            unit: UsageUnit::Percent,
        },
        status: Some(usage_status(fraction)),
        notes: Vec::new(),
    }
}

fn scope(account: Option<&str>, window_id: Option<&str>) -> UsageScope {
    UsageScope {
        provider: PROVIDER.into(),
        account_id: account.map(str::to_owned),
        window_id: window_id.map(str::to_owned),
        shared: Some(true),
        ..Default::default()
    }
}

/// Lowercase, non-alphanumeric runs collapsed to `-`, edges trimmed.
fn slugify(product: &str) -> String {
    let mut slug = String::new();
    for c in product.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_owned()
}

fn on_demand_limit(
    cap: Option<f64>,
    used: Option<f64>,
    account: Option<&str>,
) -> Option<UsageLimit> {
    let (cap, used) = (cap?, used?);
    if cap <= 0.0 {
        return None;
    }
    let fraction = (used / cap).min(1.0);
    Some(UsageLimit {
        id: format!("{PROVIDER}:on-demand"),
        label: "On-demand".into(),
        scope: scope(account, None),
        window: None,
        amount: UsageAmount {
            used: Some(used),
            limit: Some(cap),
            remaining: Some((cap - used).max(0.0)),
            used_fraction: Some(fraction),
            remaining_fraction: Some(1.0 - fraction),
            unit: UsageUnit::Unknown,
        },
        status: Some(usage_status(fraction)),
        notes: Vec::new(),
    })
}

fn weekly_limits(weekly: &Weekly, account: Option<&str>) -> Vec<UsageLimit> {
    let window = UsageWindow {
        id: "1w".into(),
        label: "Weekly".into(),
        duration_ms: Some(WEEK_MS),
        resets_at_ms: Some(weekly.end_ms),
        reset_label: None,
    };
    let mut limits = vec![percent_limit(
        format!("{PROVIDER}:credits:1w"),
        "SuperGrok Weekly Credits".into(),
        weekly.credit_percent,
        scope(account, Some("1w")),
        window.clone(),
    )];
    for (product, usage) in &weekly.products {
        let slug = slugify(product);
        if slug.is_empty() {
            continue;
        }
        let name = match product.as_str() {
            "GrokBuild" => "Grok Build",
            "Api" => "API",
            other => other,
        };
        limits.push(percent_limit(
            format!("{PROVIDER}:product:{slug}:1w"),
            format!("{name} (Weekly)"),
            *usage,
            scope(account, Some("1w")),
            window.clone(),
        ));
    }
    limits.extend(on_demand_limit(
        weekly.on_demand_cap,
        weekly.on_demand_used,
        account,
    ));
    limits
}

fn monthly_limits(monthly: &Monthly, account: Option<&str>) -> Vec<UsageLimit> {
    // Real calendar months vary; use the observed period length from the API.
    let duration = monthly.end_ms - monthly.start_ms;
    let approx_days = ((duration as f64) / (DAY_MS as f64)).round().max(1.0) as u64;
    let label = if approx_days == 30 || approx_days == 31 {
        "Monthly".to_owned()
    } else {
        format!("{approx_days}d")
    };
    let window = UsageWindow {
        id: "1mo".into(),
        label,
        duration_ms: Some(duration),
        resets_at_ms: Some(monthly.end_ms),
        reset_label: None,
    };
    let fraction = (monthly.used / monthly.limit).min(1.0);
    let mut limits = vec![UsageLimit {
        id: format!("{PROVIDER}:included:1mo"),
        label: "SuperGrok Monthly Included".into(),
        scope: scope(account, Some("1mo")),
        window: Some(window),
        amount: UsageAmount {
            used: Some(monthly.used),
            limit: Some(monthly.limit),
            remaining: Some((monthly.limit - monthly.used).max(0.0)),
            used_fraction: Some(fraction),
            remaining_fraction: Some(1.0 - fraction),
            // xAI does not label the unit; amounts match the dashboard quota points.
            unit: UsageUnit::Unknown,
        },
        status: Some(usage_status(fraction)),
        notes: Vec::new(),
    }];
    limits.extend(on_demand_limit(
        monthly.on_demand_cap,
        monthly.on_demand_used,
        account,
    ));
    limits
}

#[cfg(test)]
mod tests;

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
mod tests {
    use super::*;
    use serde_json::json;

    /// 2026-05-01T00:00:00Z
    const NOW: u64 = 1_777_593_600_000;

    fn weekly_payload() -> Value {
        json!({ "config": {
            "currentPeriod": {
                "start": "2026-04-27T00:00:00Z",
                "end": "2026-05-04T00:00:00Z",
                "type": "SUPERGROK_WEEKLY"
            },
            "creditUsagePercent": 42,
            "productUsage": [
                { "product": "GrokBuild", "usagePercent": 30 },
                { "product": "Api", "usagePercent": null },
                { "product": "  ", "usagePercent": 5 },
                { "product": "Chat", "usagePercent": "bad" }
            ],
            "onDemandCap": { "val": 500 },
            "onDemandUsed": { "val": 125 }
        }})
    }

    fn monthly_payload(used: f64, limit: f64) -> Value {
        json!({ "config": {
            "billingPeriodStart": "2026-04-01T00:00:00Z",
            "billingPeriodEnd": "2026-05-01T00:00:00Z",
            "monthlyLimit": { "val": limit },
            "used": { "val": used }
        }})
    }

    /// Unified credits payload with no creditUsagePercent: inferred 0% while the period is active.
    fn unified_inferred_credits() -> Value {
        json!({ "config": {
            "isUnifiedBillingUser": true,
            "currentPeriod": {
                "start": "2026-04-27T00:00:00Z",
                "end": "2026-05-04T00:00:00Z",
                "type": "WEEK"
            }
        }})
    }

    #[test]
    fn weekly_credits_map_overall_products_and_on_demand() {
        let payload = weekly_payload();
        let report = assemble(Some(&payload), None, Some("acct-1"), NOW).unwrap();
        assert_eq!(report.provider, PROVIDER);
        assert_eq!(report.fetched_at_ms, NOW);
        let ids: Vec<&str> = report.limits.iter().map(|l| l.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "xai-oauth:credits:1w",
                "xai-oauth:product:grokbuild:1w",
                "xai-oauth:product:api:1w",
                "xai-oauth:on-demand",
            ]
        );

        let overall = &report.limits[0];
        assert_eq!(overall.label, "SuperGrok Weekly Credits");
        assert_eq!(overall.amount.used, Some(42.0));
        assert_eq!(overall.amount.limit, Some(100.0));
        assert_eq!(overall.amount.used_fraction, Some(0.42));
        assert_eq!(overall.status, Some(UsageStatus::Ok));
        assert_eq!(overall.scope.account_id.as_deref(), Some("acct-1"));
        assert_eq!(overall.scope.shared, Some(true));
        assert_eq!(overall.scope.window_id.as_deref(), Some("1w"));
        let window = overall.window.as_ref().unwrap();
        assert_eq!(window.id, "1w");
        assert_eq!(window.duration_ms, Some(WEEK_MS));
        assert_eq!(window.resets_at_ms, Some(NOW + 3 * DAY_MS));

        let build = &report.limits[1];
        assert_eq!(build.label, "Grok Build (Weekly)");
        assert_eq!(build.amount.used, Some(30.0));
        let api = &report.limits[2];
        assert_eq!(api.label, "API (Weekly)");
        assert_eq!(api.amount.used, Some(0.0));

        let on_demand = &report.limits[3];
        assert_eq!(on_demand.amount.used, Some(125.0));
        assert_eq!(on_demand.amount.limit, Some(500.0));
        assert_eq!(on_demand.amount.used_fraction, Some(0.25));
        assert_eq!(on_demand.amount.remaining, Some(375.0));
        assert!(matches!(on_demand.amount.unit, UsageUnit::Unknown));
        assert_eq!(on_demand.scope.window_id, None);
        assert_eq!(report.metadata["billingKind"], "weekly");
        assert_eq!(report.metadata["endpoint"], CREDITS_URL);
        assert_eq!(report.metadata["source"], SOURCE);
        assert_eq!(report.metadata["accountId"], "acct-1");
    }

    #[test]
    fn monthly_included_quota_uses_observed_period_length() {
        let payload = monthly_payload(250.0, 1000.0);
        let report = assemble(None, Some(&payload), None, NOW).unwrap();
        assert_eq!(report.limits.len(), 1);
        let limit = &report.limits[0];
        assert_eq!(limit.id, "xai-oauth:included:1mo");
        assert_eq!(limit.label, "SuperGrok Monthly Included");
        let window = limit.window.as_ref().unwrap();
        assert_eq!(window.label, "Monthly");
        assert_eq!(window.duration_ms, Some(30 * DAY_MS));
        assert_eq!(window.resets_at_ms, Some(NOW));
        assert_eq!(limit.amount.used, Some(250.0));
        assert_eq!(limit.amount.limit, Some(1000.0));
        assert_eq!(limit.amount.used_fraction, Some(0.25));
        assert_eq!(limit.amount.remaining, Some(750.0));
        assert!(matches!(limit.amount.unit, UsageUnit::Unknown));
        assert_eq!(limit.status, Some(UsageStatus::Ok));
        assert_eq!(report.metadata["billingKind"], "monthly");
    }

    #[test]
    fn exhausted_monthly_quota_reports_exhausted_status() {
        let payload = monthly_payload(1000.0, 1000.0);
        let report = assemble(None, Some(&payload), None, NOW).unwrap();
        assert_eq!(report.limits[0].amount.used_fraction, Some(1.0));
        assert_eq!(report.limits[0].status, Some(UsageStatus::Exhausted));
    }

    #[test]
    fn over_limit_monthly_beside_inferred_unified_credits_is_advisory() {
        let credits = unified_inferred_credits();
        let monthly = monthly_payload(1000.0, 1000.0);
        let report = assemble(Some(&credits), Some(&monthly), None, NOW).unwrap();
        // Monthly wins: weekly is dropped and the monthly counter stays visible as unknown.
        assert_eq!(report.limits.len(), 1);
        let limit = &report.limits[0];
        assert_eq!(limit.id, "xai-oauth:included:1mo");
        assert_eq!(limit.status, Some(UsageStatus::Unknown));
        assert_eq!(limit.notes.len(), 1);
        assert!(limit.notes[0].contains("enforcement uncertain"));
        assert_eq!(report.metadata["monthlyQuotaAdvisory"], "true");
        assert_eq!(report.metadata["billingKind"], "monthly");
    }

    #[test]
    fn inferred_unified_credits_confirmed_zero_monthly_keeps_weekly() {
        let credits = unified_inferred_credits();
        let monthly = json!({ "config": { "monthlyLimit": { "val": 0 } } });
        let report = assemble(Some(&credits), Some(&monthly), None, NOW).unwrap();
        assert!(report.limits.iter().any(|l| l.id == "xai-oauth:credits:1w"));
        assert_eq!(report.limits[0].amount.used, Some(0.0));
        assert_eq!(report.metadata["billingKind"], "weekly");
        assert!(!report.metadata.contains_key("monthlyQuotaAdvisory"));
    }

    #[test]
    fn inferred_unified_credits_without_monthly_confirmation_are_rejected() {
        let credits = unified_inferred_credits();
        assert!(assemble(Some(&credits), None, None, NOW).is_none());
    }

    #[test]
    fn expired_or_non_weekly_periods_yield_no_report() {
        let expired = json!({ "config": {
            "currentPeriod": {
                "start": "2026-04-01T00:00:00Z",
                "end": "2026-04-08T00:00:00Z",
                "type": "WEEK"
            }
        }});
        assert!(assemble(Some(&expired), None, None, NOW).is_none());

        let monthly_period = json!({ "config": {
            "currentPeriod": {
                "start": "2026-04-27T00:00:00Z",
                "end": "2026-05-04T00:00:00Z",
                "type": "MONTH"
            },
            "creditUsagePercent": 10
        }});
        assert!(assemble(Some(&monthly_period), None, None, NOW).is_none());
    }

    #[test]
    fn malformed_billing_payloads_yield_no_report() {
        assert!(assemble(Some(&json!({ "x": 1 })), None, None, NOW).is_none());

        let bad_products = json!({ "config": {
            "currentPeriod": {
                "start": "2026-04-27T00:00:00Z",
                "end": "2026-05-04T00:00:00Z",
                "type": "WEEK"
            },
            "creditUsagePercent": 5,
            "productUsage": "nope"
        }});
        assert!(assemble(Some(&bad_products), None, None, NOW).is_none());

        let zero_limit = monthly_payload(0.0, 0.0);
        assert!(assemble(None, Some(&zero_limit), None, NOW).is_none());

        let out_of_range = json!({ "config": {
            "currentPeriod": {
                "start": "2026-04-27T00:00:00Z",
                "end": "2026-05-04T00:00:00Z",
                "type": "WEEK"
            },
            "creditUsagePercent": 150
        }});
        assert!(assemble(Some(&out_of_range), None, None, NOW).is_none());
    }

    #[test]
    fn slugs_and_endpoints_are_stable() {
        assert_eq!(slugify("GrokBuild"), "grokbuild");
        assert_eq!(slugify(" Grok Build Pro!! "), "grok-build-pro");
        assert!(CREDITS_URL.ends_with("/v1/billing?format=credits"));
        assert_eq!(MONTHLY_URL, "https://cli-chat-proxy.grok.com/v1/billing");
    }

    #[test]
    fn fetch_without_stored_grant_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        assert!(fetch(dir.path(), &cancel).unwrap().is_none());
    }
}

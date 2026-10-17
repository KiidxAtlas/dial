//! Google Antigravity (Cloud Code Assist) quota usage. Prefers `retrieveUserQuotaSummary` and falls back to
//! the legacy `fetchAvailableModels` payload.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use super::gemini::{opt_str, parse_iso_ms};
use super::types::{
    UsageAmount, UsageLimit, UsageReport, UsageScope, UsageStatus, UsageUnit, UsageWindow,
};
use crate::llm::oauth::{self, util};

const ID: &str = "google-antigravity";
const DEFAULT_ENDPOINTS: [&str; 2] = [
    "https://daily-cloudcode-pa.googleapis.com",
    "https://daily-cloudcode-pa.sandbox.googleapis.com",
];
const SUMMARY_PATH: &str = "/v1internal:retrieveUserQuotaSummary";
const MODELS_PATH: &str = "/v1internal:fetchAvailableModels";

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;
const WEEK_MS: i64 = 7 * DAY_MS;

/// Account/project context shared by every emitted limit.
#[derive(Clone, Copy)]
struct Ctx<'a> {
    provider: &'a str,
    account_id: Option<&'a str>,
    project_id: &'a str,
}

/// Window descriptor inferred from a window id/label or reset cadence.
#[derive(Clone)]
struct Descriptor {
    id: String,
    label: String,
    duration_ms: Option<i64>,
}

struct Attempt {
    endpoint: String,
    response: util::Response,
}

#[derive(Debug)]
struct Bucket {
    bucket_id: Option<String>,
    display_name: Option<String>,
    window: Option<String>,
    remaining_fraction: Option<f64>,
    remaining_amount: Option<f64>,
    disabled: bool,
    reset_time: Option<String>,
}

struct Group {
    display_name: Option<String>,
    buckets: Vec<Bucket>,
}

/// One normalized legacy quota entry.
#[derive(Debug)]
struct QuotaInfo {
    remaining_fraction: Option<f64>,
    reset_time: Option<String>,
    tier: Option<String>,
    window_id: Option<String>,
    window_label: Option<String>,
    api_provider: Option<String>,
    model_provider: Option<String>,
}

/// Provider/model source metadata inherited by each quota entry.
struct ModelSource {
    api_provider: Option<String>,
    model_provider: Option<String>,
}

/// Deduplicated legacy quota entry keyed by counter, tier, and window.
struct Entry {
    amount: UsageAmount,
    window: Option<UsageWindow>,
    tier: Option<String>,
    tier_key: String,
    window_id: String,
    counter_name: Option<&'static str>,
    counter_key: String,
}

pub(super) fn fetch(user_dir: &Path, cancel: &Arc<AtomicBool>) -> Result<Option<UsageReport>> {
    if !oauth::signed_in(user_dir, ID) {
        return Ok(None);
    }
    let access = oauth::access(user_dir, ID, cancel)?;
    let Some(project) = access.project_id.as_deref().filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    let authorization = format!("Bearer {}", access.token.as_str());
    let endpoints: Vec<String> = match access
        .base_url
        .as_deref()
        .map(|url| url.trim_end_matches('/'))
        .filter(|url| !url.is_empty())
    {
        Some(base) => vec![base.to_owned()],
        None => DEFAULT_ENDPOINTS.iter().map(|s| (*s).to_owned()).collect(),
    };
    let now_ms = util::now_ms();
    let ctx = Ctx {
        provider: ID,
        account_id: access.account_id.as_deref(),
        project_id: project,
    };

    let summary = post_first_available(&endpoints, SUMMARY_PATH, project, &authorization, cancel)?;
    let summary_report = if summary.response.ok() {
        parse_summary(&summary.response.json()?, ctx, &summary.endpoint, now_ms)?
    } else {
        None
    };
    let mut report = match summary_report {
        Some(report) => report,
        None => {
            let legacy =
                post_first_available(&endpoints, MODELS_PATH, project, &authorization, cancel)?;
            if !legacy.response.ok() {
                bail!(
                    "Antigravity fetchAvailableModels failed (HTTP {})",
                    legacy.response.status
                );
            }
            parse_models(&legacy.response.json()?, ctx, &legacy.endpoint, now_ms)?
        }
    };
    if let Some(account) = oauth::identity(user_dir, ID) {
        report.metadata.insert("account".to_owned(), account);
    }
    Ok(Some(report))
}

fn post_first_available(
    endpoints: &[String],
    path: &str,
    project: &str,
    authorization: &str,
    cancel: &Arc<AtomicBool>,
) -> Result<Attempt> {
    let body = json!({ "project": project });
    let user_agent = antigravity_user_agent();
    let headers = [
        ("Authorization", authorization),
        ("Content-Type", "application/json"),
        ("User-Agent", user_agent.as_str()),
    ];
    let last = endpoints.len().saturating_sub(1);
    for (index, endpoint) in endpoints.iter().enumerate() {
        let url = format!("{endpoint}{path}");
        match util::request("POST", &url, &headers, util::Body::Json(&body), cancel) {
            Ok(response) if response.ok() || !is_transient(response.status) || index == last => {
                return Ok(Attempt {
                    endpoint: endpoint.clone(),
                    response,
                });
            }
            Ok(_) => {}
            Err(error) if index == last => return Err(error),
            Err(_) => {}
        }
    }
    bail!("no Antigravity endpoints configured")
}

fn is_transient(status: u16) -> bool {
    matches!(status, 408 | 429 | 500..=599)
}

/// Mirrors the reference Antigravity client's user agent: darwin/arm64 regardless of host
/// platform, overridable via DIAL_ANTIGRAVITY_*.
fn antigravity_user_agent() -> String {
    let env_or = |key: &str, default: &str| {
        std::env::var(key)
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| default.to_owned())
    };
    format!(
        "antigravity/hub/{} (aidev_client; os_type={}; arch={}; cl={})",
        env_or("DIAL_ANTIGRAVITY_VERSION", "2.19.1"),
        env_or("DIAL_ANTIGRAVITY_OS", "darwin"),
        env_or("DIAL_ANTIGRAVITY_ARCH", "arm64"),
        env_or("DIAL_ANTIGRAVITY_CL", "963137146"),
    )
}

fn array_field<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a [Value]> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(items)) => Ok(items.as_slice()),
        Some(_) => bail!("Antigravity quota field `{key}` is not an array"),
    }
}

fn parse_buckets(object: &Map<String, Value>) -> Result<Vec<Bucket>> {
    array_field(object, "buckets")?
        .iter()
        .map(parse_bucket)
        .collect()
}

fn parse_bucket(value: &Value) -> Result<Bucket> {
    let object = value
        .as_object()
        .context("Antigravity quota bucket is not a JSON object")?;
    let remaining_amount = match object.get("remainingAmount") {
        None | Some(Value::Null) => None,
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
        Some(_) => bail!("Antigravity quota bucket `remainingAmount` has an unexpected type"),
    };
    Ok(Bucket {
        bucket_id: opt_str(object, "bucketId")?.map(str::to_owned),
        display_name: opt_str(object, "displayName")?.map(str::to_owned),
        window: opt_str(object, "window")?.map(str::to_owned),
        remaining_fraction: object
            .get("remainingFraction")
            .filter(|v| !v.is_null())
            .map(|v| {
                v.as_f64()
                    .context("Antigravity remainingFraction must be numeric")
            })
            .transpose()?,
        remaining_amount,
        disabled: object.get("disabled").and_then(Value::as_bool) == Some(true),
        reset_time: opt_str(object, "resetTime")?.map(str::to_owned),
    })
}

/// Parses `retrieveUserQuotaSummary`. Returns `None` when the payload has no buckets.
fn parse_summary(
    payload: &Value,
    ctx: Ctx<'_>,
    endpoint: &str,
    now_ms: u64,
) -> Result<Option<UsageReport>> {
    let object = payload
        .as_object()
        .context("Antigravity quota summary is not a JSON object")?;
    let mut groups = Vec::new();
    for group in array_field(object, "groups")? {
        let group = group
            .as_object()
            .context("Antigravity quota group is not a JSON object")?;
        groups.push(Group {
            display_name: opt_str(group, "displayName")?.map(str::to_owned),
            buckets: parse_buckets(group)?,
        });
    }
    let top_buckets = parse_buckets(object)?;
    let has_grouped = groups.iter().any(|group| !group.buckets.is_empty());
    if !has_grouped && top_buckets.is_empty() {
        return Ok(None);
    }
    let mut limits = Vec::new();
    if has_grouped {
        for group in &groups {
            for bucket in &group.buckets {
                push_summary_limits(&mut limits, ctx, bucket, Some(group));
            }
        }
    } else {
        for bucket in &top_buckets {
            push_summary_limits(&mut limits, ctx, bucket, None);
        }
    }
    sort_by_remaining(&mut limits);
    Ok(Some(build_report(ctx, endpoint, now_ms, limits)))
}

fn push_summary_limits(
    limits: &mut Vec<UsageLimit>,
    ctx: Ctx<'_>,
    bucket: &Bucket,
    group: Option<&Group>,
) {
    if bucket.disabled {
        return;
    }
    let descriptor = classify_window(bucket.window.as_deref(), bucket.display_name.as_deref());
    let window = build_window(
        bucket.window.as_deref(),
        descriptor.as_ref().map(|d| d.label.as_str()),
        bucket.reset_time.as_deref(),
        descriptor.as_ref(),
    );
    let amount = summary_amount(bucket);
    let status = match amount.remaining_fraction {
        Some(fraction) => get_usage_status(Some(fraction)),
        None if amount.remaining == Some(0.0) => UsageStatus::Exhausted,
        None => UsageStatus::Unknown,
    };
    let keys = summary_counter_keys(group, bucket);
    let shared_group = (keys.len() > 1).then(|| {
        let owner = bucket
            .bucket_id
            .as_deref()
            .or(group.and_then(|g| g.display_name.as_deref()))
            .unwrap_or("third-party");
        let window_part = window
            .as_ref()
            .map(|w| w.id.as_str())
            .or(bucket.window.as_deref())
            .unwrap_or("default");
        format!("{owner}:{window_part}")
    });
    for key in keys {
        let window_id = window
            .as_ref()
            .map(|w| w.id.clone())
            .or_else(|| bucket.window.clone())
            .or_else(|| bucket.bucket_id.clone())
            .unwrap_or_else(|| "default".to_owned());
        let label = if shared_group.is_some() {
            Some("Claude & GPT (shared)")
        } else if *key == "google" {
            Some("Gemini")
        } else {
            counter_name(key)
        };
        let label = label
            .map(str::to_owned)
            .or_else(|| group.and_then(|g| g.display_name.clone()))
            .or_else(|| bucket.display_name.clone())
            .unwrap_or_else(|| "Usage".to_owned());
        let bucket_part = bucket
            .bucket_id
            .clone()
            .unwrap_or_else(|| window_id.clone());
        limits.push(UsageLimit {
            id: format!("{}:{key}:default:{bucket_part}", ctx.provider),
            label,
            scope: UsageScope {
                provider: ctx.provider.to_owned(),
                account_id: ctx.account_id.map(str::to_owned),
                project_id: Some(ctx.project_id.to_owned()),
                window_id: Some(window_id),
                shared: shared_group.as_ref().map(|_| true),
                shared_group: shared_group.clone(),
                ..UsageScope::default()
            },
            window: window.clone(),
            amount: amount.clone(),
            status: Some(status),
            notes: Vec::new(),
        });
    }
}

/// Counter keys a summary bucket is reported under. Third-party buckets split into both.
fn summary_counter_keys(group: Option<&Group>, bucket: &Bucket) -> &'static [&'static str] {
    let bucket_id = bucket.bucket_id.as_deref().unwrap_or("").to_lowercase();
    let group_name = group
        .and_then(|g| g.display_name.as_deref())
        .unwrap_or("")
        .to_lowercase();
    if bucket_id.starts_with("gemini-") || group_name.contains("gemini") {
        &["google"]
    } else if bucket_id.starts_with("3p-")
        || group_name.contains("claude")
        || group_name.contains("gpt")
        || group_name.contains("third party")
    {
        &["anthropic", "openai"]
    } else {
        &["default"]
    }
}

fn counter_name(key: &str) -> Option<&'static str> {
    match key {
        "google" => Some("Google"),
        "anthropic" => Some("Anthropic"),
        "openai" => Some("OpenAI"),
        _ => None,
    }
}

fn summary_amount(bucket: &Bucket) -> UsageAmount {
    if let Some(fraction) = clamp_fraction(bucket.remaining_fraction) {
        return percent_amount(Some(fraction));
    }
    unknown_amount(bucket.remaining_amount.filter(|value| value.is_finite()))
}

/// Parses the legacy `fetchAvailableModels` payload.
fn parse_models(payload: &Value, ctx: Ctx<'_>, endpoint: &str, now_ms: u64) -> Result<UsageReport> {
    let object = payload
        .as_object()
        .context("Antigravity models response is not a JSON object")?;
    let empty = Map::new();
    let models = match object.get("models") {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(models)) => models,
        Some(_) => bail!("Antigravity models response `models` is not an object"),
    };
    let now = i64::try_from(now_ms).unwrap_or(i64::MAX);
    let mut entries: Vec<(String, Entry)> = Vec::new();
    for model in models.values() {
        let model = model
            .as_object()
            .context("Antigravity model entry is not a JSON object")?;
        let infos = normalize_quota_infos(model)?;
        let descriptors = infer_window_descriptors(&infos, now);
        for (info, descriptor) in infos.iter().zip(&descriptors) {
            let amount = build_amount(info);
            let window = build_window(
                info.window_id.as_deref(),
                info.window_label.as_deref(),
                info.reset_time.as_deref(),
                descriptor.as_ref(),
            );
            let counter_name = format_counter_name(info);
            let counter_key =
                counter_name.map_or_else(|| "default".to_owned(), str::to_ascii_lowercase);
            let tier_key = info
                .tier
                .as_deref()
                .unwrap_or("default")
                .to_ascii_lowercase();
            let window_id = window
                .as_ref()
                .map(|w| w.id.clone())
                .or_else(|| info.window_id.clone())
                .unwrap_or_else(|| "default".to_owned());
            let key = format!("{counter_key}|{tier_key}|{window_id}");
            let candidate = Entry {
                amount,
                window,
                tier: info.tier.clone(),
                tier_key,
                window_id,
                counter_name,
                counter_key,
            };
            match entries
                .iter_mut()
                .find(|(existing_key, _)| *existing_key == key)
            {
                Some((_, existing)) => merge_entry(existing, candidate),
                None => entries.push((key, candidate)),
            }
        }
    }

    let metered: Vec<String> = entries
        .iter()
        .filter(|(_, entry)| has_reset(entry.window.as_ref()))
        .map(|(_, entry)| meter_key(entry))
        .collect();
    let mut limits = Vec::new();
    for (_, entry) in &entries {
        if !has_reset(entry.window.as_ref()) && metered.contains(&meter_key(entry)) {
            continue;
        }
        let label = match entry.counter_name {
            Some(name) => format!("Usage ({name})"),
            None => "Usage".to_owned(),
        };
        limits.push(UsageLimit {
            id: format!(
                "{}:{}:{}:{}",
                ctx.provider, entry.counter_key, entry.tier_key, entry.window_id
            ),
            label,
            scope: UsageScope {
                provider: ctx.provider.to_owned(),
                account_id: ctx.account_id.map(str::to_owned),
                project_id: Some(ctx.project_id.to_owned()),
                tier: entry.tier.clone(),
                window_id: Some(entry.window_id.clone()),
                ..UsageScope::default()
            },
            window: entry.window.clone(),
            amount: entry.amount.clone(),
            status: Some(get_usage_status(entry.amount.remaining_fraction)),
            notes: Vec::new(),
        });
    }
    sort_by_remaining(&mut limits);
    Ok(build_report(ctx, endpoint, now_ms, limits))
}

fn meter_key(entry: &Entry) -> String {
    format!("{}|{}", entry.counter_key, entry.tier_key)
}

fn has_reset(window: Option<&UsageWindow>) -> bool {
    window.and_then(|w| w.resets_at_ms).is_some()
}

fn merge_entry(existing: &mut Entry, candidate: Entry) {
    let existing_fraction = existing.amount.remaining_fraction;
    let candidate_fraction = candidate.amount.remaining_fraction;
    let mut best_amount = existing.amount.clone();
    let mut best_tier = existing.tier.clone().or_else(|| candidate.tier.clone());
    let mut best_window = if has_reset(existing.window.as_ref()) {
        existing.window.clone()
    } else {
        candidate.window.clone().or_else(|| existing.window.clone())
    };
    if existing_fraction.is_none() && candidate_fraction.is_some() {
        best_amount = candidate.amount.clone();
        best_tier = candidate.tier.clone().or_else(|| existing.tier.clone());
    } else if let (Some(e), Some(c)) = (existing_fraction, candidate_fraction)
        && c < e
    {
        best_amount = candidate.amount.clone();
        best_tier = candidate.tier.clone().or_else(|| existing.tier.clone());
    }
    if !has_reset(best_window.as_ref()) && has_reset(candidate.window.as_ref()) {
        best_window = candidate.window;
    }
    existing.amount = best_amount;
    existing.window = best_window;
    existing.tier = best_tier;
}

fn normalize_quota_infos(model: &Map<String, Value>) -> Result<Vec<QuotaInfo>> {
    let source = ModelSource {
        api_provider: opt_str(model, "apiProvider")?.map(str::to_owned),
        model_provider: opt_str(model, "modelProvider")?.map(str::to_owned),
    };
    let mut out = Vec::new();
    push_quota_values(&mut out, model.get("quotaInfo"), &source, None, None)?;
    push_quota_values(&mut out, model.get("quotaInfos"), &source, None, None)?;
    let daily_window = daily();
    let weekly_window = weekly();
    push_quota_values(
        &mut out,
        model.get("dailyQuotaInfo"),
        &source,
        None,
        Some(&daily_window),
    )?;
    push_quota_values(
        &mut out,
        model.get("dailyQuotaInfos"),
        &source,
        None,
        Some(&daily_window),
    )?;
    push_quota_values(
        &mut out,
        model.get("weeklyQuotaInfo"),
        &source,
        None,
        Some(&weekly_window),
    )?;
    push_quota_values(
        &mut out,
        model.get("weeklyQuotaInfos"),
        &source,
        None,
        Some(&weekly_window),
    )?;
    if let Some(by_tier) = model.get("quotaInfoByTier") {
        push_keyed_quota_values(&mut out, by_tier, &source, KeyKind::Tier)?;
    }
    for field in ["quotaInfoByWindow", "quotaInfosByWindow"] {
        if let Some(by_window) = model.get(field) {
            push_keyed_quota_values(&mut out, by_window, &source, KeyKind::Window)?;
        }
    }
    Ok(out)
}

#[derive(Clone, Copy)]
enum KeyKind {
    Tier,
    Window,
}

fn push_keyed_quota_values(
    out: &mut Vec<QuotaInfo>,
    value: &Value,
    source: &ModelSource,
    kind: KeyKind,
) -> Result<()> {
    let Value::Object(map) = value else {
        return Ok(());
    };
    for (key, entry) in map {
        let (tier, descriptor) = match kind {
            KeyKind::Tier => (Some(key.as_str()), None),
            KeyKind::Window => (None, classify_window(Some(key.as_str()), None)),
        };
        push_quota_values(out, Some(entry), source, tier, descriptor.as_ref())?;
    }
    Ok(())
}

fn push_quota_values(
    out: &mut Vec<QuotaInfo>,
    value: Option<&Value>,
    source: &ModelSource,
    tier: Option<&str>,
    descriptor: Option<&Descriptor>,
) -> Result<()> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(());
    };
    match value {
        Value::Array(items) => {
            for item in items {
                out.push(parse_quota_info(item, source, tier, descriptor)?);
            }
        }
        other => out.push(parse_quota_info(other, source, tier, descriptor)?),
    }
    Ok(())
}

fn parse_quota_info(
    value: &Value,
    source: &ModelSource,
    tier: Option<&str>,
    descriptor: Option<&Descriptor>,
) -> Result<QuotaInfo> {
    let object = value
        .as_object()
        .context("Antigravity quota info is not a JSON object")?;
    let mut info = QuotaInfo {
        remaining_fraction: object
            .get("remainingFraction")
            .filter(|v| !v.is_null())
            .map(|v| {
                v.as_f64()
                    .context("Antigravity remainingFraction must be numeric")
            })
            .transpose()?,
        reset_time: opt_str(object, "resetTime")?.map(str::to_owned),
        tier: opt_str(object, "tier")?.map(str::to_owned),
        window_id: opt_str(object, "windowId")?.map(str::to_owned),
        window_label: opt_str(object, "windowLabel")?.map(str::to_owned),
        api_provider: opt_str(object, "apiProvider")?
            .map(str::to_owned)
            .or_else(|| source.api_provider.clone()),
        model_provider: opt_str(object, "modelProvider")?
            .map(str::to_owned)
            .or_else(|| source.model_provider.clone()),
    };
    if let Some(descriptor) = descriptor {
        info.window_id.get_or_insert_with(|| descriptor.id.clone());
        info.window_label
            .get_or_insert_with(|| descriptor.label.clone());
    }
    if let Some(tier) = tier {
        info.tier = Some(tier.to_owned());
    }
    Ok(info)
}

/// Infers a window descriptor for each entry lacking an explicit window. Entries that
/// share a model/provider/tier group and have a single distinct reset time use the
/// weekly cadence when the latest reset is more than a day out; otherwise daily.
fn infer_window_descriptors(infos: &[QuotaInfo], now: i64) -> Vec<Option<Descriptor>> {
    let mut out: Vec<Option<Descriptor>> = vec![None; infos.len()];
    let mut groups: std::collections::BTreeMap<String, Vec<(usize, Option<i64>)>> =
        std::collections::BTreeMap::new();
    for (index, info) in infos.iter().enumerate() {
        if let Some(descriptor) =
            classify_window(info.window_id.as_deref(), info.window_label.as_deref())
        {
            out[index] = Some(descriptor);
            continue;
        }
        let key = format!(
            "{}|{}|{}",
            info.model_provider.as_deref().unwrap_or(""),
            info.api_provider.as_deref().unwrap_or(""),
            info.tier.as_deref().unwrap_or("")
        );
        let reset = info.reset_time.as_deref().and_then(parse_iso_ms);
        groups.entry(key).or_default().push((index, reset));
    }
    for entries in groups.values() {
        let mut times: Vec<i64> = entries.iter().filter_map(|(_, reset)| *reset).collect();
        times.sort_unstable();
        times.dedup();
        let latest = if times.len() > 1 {
            times.last().copied()
        } else {
            None
        };
        for (index, reset) in entries {
            out[*index] = Some(match (latest, *reset) {
                (Some(latest), Some(reset)) if reset == latest => weekly(),
                _ => infer_from_reset(*reset, now),
            });
        }
    }
    out
}

fn infer_from_reset(reset: Option<i64>, now: i64) -> Descriptor {
    if reset.is_some_and(|reset| reset - now > DAY_MS) {
        weekly()
    } else {
        daily()
    }
}

fn weekly() -> Descriptor {
    Descriptor {
        id: "weekly".to_owned(),
        label: "Weekly".to_owned(),
        duration_ms: Some(WEEK_MS),
    }
}

fn daily() -> Descriptor {
    Descriptor {
        id: "daily".to_owned(),
        label: "Daily".to_owned(),
        duration_ms: Some(DAY_MS),
    }
}

fn five_hour() -> Descriptor {
    Descriptor {
        id: "5h".to_owned(),
        label: "5 Hour".to_owned(),
        duration_ms: Some(5 * HOUR_MS),
    }
}

/// Maps a window id/label onto a known cadence. Unknown windows keep their own id and label.
fn classify_window(id: Option<&str>, label: Option<&str>) -> Option<Descriptor> {
    let source = format!("{} {}", id.unwrap_or(""), label.unwrap_or("")).to_lowercase();
    if source.contains("week") || source.contains("7d") || has_gap(&source, '7', "day") {
        return Some(weekly());
    }
    if source.contains("5h") || source.contains("five hour") || has_gap(&source, '5', "hour") {
        return Some(five_hour());
    }
    if source.contains("day") || source.contains("daily") || source.contains("24h") {
        return Some(daily());
    }
    let id = id.filter(|s| !s.is_empty());
    let label = label.filter(|s| !s.is_empty());
    let resolved_id = id.or(label)?;
    Some(Descriptor {
        id: resolved_id.to_owned(),
        label: label.or(id).unwrap_or("Default").to_owned(),
        duration_ms: None,
    })
}

/// True when `digit` is followed by optional whitespace/`_`/`-` and then `unit`.
fn has_gap(source: &str, digit: char, unit: &str) -> bool {
    let mut rest = source;
    while let Some(position) = rest.find(digit) {
        let after = &rest[position + digit.len_utf8()..];
        let trimmed = after.trim_start_matches(|c: char| c.is_whitespace() || c == '_' || c == '-');
        if trimmed.starts_with(unit) {
            return true;
        }
        rest = after;
    }
    false
}

fn build_window(
    window_id: Option<&str>,
    window_label: Option<&str>,
    reset_time: Option<&str>,
    descriptor: Option<&Descriptor>,
) -> Option<UsageWindow> {
    let resets_at = reset_time.and_then(parse_iso_ms);
    if descriptor.is_none() && resets_at.is_none() {
        return None;
    }
    Some(UsageWindow {
        id: descriptor
            .map(|d| d.id.clone())
            .or_else(|| window_id.map(str::to_owned))
            .unwrap_or_else(|| "default".to_owned()),
        label: window_label
            .map(str::to_owned)
            .or_else(|| descriptor.map(|d| d.label.clone()))
            .unwrap_or_else(|| "Default".to_owned()),
        duration_ms: descriptor
            .and_then(|d| d.duration_ms)
            .and_then(|ms| u64::try_from(ms).ok()),
        resets_at_ms: resets_at.and_then(|ms| u64::try_from(ms).ok()),
        reset_label: None,
    })
}

fn build_amount(info: &QuotaInfo) -> UsageAmount {
    let fraction = clamp_fraction(info.remaining_fraction).or_else(|| {
        info.reset_time
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|_| 0.0)
    });
    percent_amount(fraction)
}

/// Counter name for a quota entry, from the model provider first, then the API provider.
fn format_counter_name(info: &QuotaInfo) -> Option<&'static str> {
    match info
        .model_provider
        .as_deref()
        .or(info.api_provider.as_deref())
    {
        Some("MODEL_PROVIDER_ANTHROPIC" | "API_PROVIDER_ANTHROPIC_VERTEX") => Some("Anthropic"),
        Some("MODEL_PROVIDER_GOOGLE" | "API_PROVIDER_GOOGLE_GEMINI") => Some("Google"),
        Some("MODEL_PROVIDER_OPENAI" | "API_PROVIDER_OPENAI_VERTEX") => Some("OpenAI"),
        _ => None,
    }
}

fn clamp_fraction(value: Option<f64>) -> Option<f64> {
    value.filter(|v| v.is_finite()).map(|v| v.clamp(0.0, 1.0))
}

fn percent_amount(fraction: Option<f64>) -> UsageAmount {
    match fraction {
        None => UsageAmount {
            used: None,
            limit: None,
            remaining: None,
            used_fraction: None,
            remaining_fraction: None,
            unit: UsageUnit::Percent,
        },
        Some(remaining) => {
            let used = 1.0 - remaining;
            UsageAmount {
                used: Some(used * 100.0),
                limit: Some(100.0),
                remaining: Some(remaining * 100.0),
                used_fraction: Some(used),
                remaining_fraction: Some(remaining),
                unit: UsageUnit::Percent,
            }
        }
    }
}

fn unknown_amount(remaining: Option<f64>) -> UsageAmount {
    UsageAmount {
        used: None,
        limit: None,
        remaining,
        used_fraction: None,
        remaining_fraction: None,
        unit: UsageUnit::Unknown,
    }
}

/// Status for an Antigravity remaining fraction: `<=0` exhausted, `<=0.1` warning.
fn get_usage_status(remaining_fraction: Option<f64>) -> UsageStatus {
    match remaining_fraction {
        None => UsageStatus::Unknown,
        Some(fraction) if fraction <= 0.0 => UsageStatus::Exhausted,
        Some(fraction) if fraction <= 0.1 => UsageStatus::Warning,
        Some(_) => UsageStatus::Ok,
    }
}

fn sort_by_remaining(limits: &mut [UsageLimit]) {
    limits.sort_by(|a, b| {
        a.amount
            .remaining_fraction
            .unwrap_or(1.0)
            .total_cmp(&b.amount.remaining_fraction.unwrap_or(1.0))
    });
}

fn build_report(ctx: Ctx<'_>, endpoint: &str, now_ms: u64, limits: Vec<UsageLimit>) -> UsageReport {
    let mut metadata = std::collections::BTreeMap::new();
    metadata.insert("endpoint".to_owned(), endpoint.to_owned());
    metadata.insert("project_id".to_owned(), ctx.project_id.to_owned());
    UsageReport {
        provider: ctx.provider.to_owned(),
        fetched_at_ms: now_ms,
        limits,
        reset_credits: None,
        notes: Vec::new(),
        metadata,
    }
}

#[cfg(test)]
mod tests;

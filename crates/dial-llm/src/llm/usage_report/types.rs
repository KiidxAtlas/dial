//! Normalized provider usage/quota reports.
//!
//! Every provider module maps its wire response into these types. Times are Unix milliseconds.
//! Fractions are 0..1 where values above 1 mean overage.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageUnit {
    Percent,
    Tokens,
    Requests,
    Credits,
    Usd,
    Minutes,
    Bytes,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageStatus {
    Ok,
    Warning,
    Exhausted,
    Unknown,
}

/// Time window for a limit (e.g. 5h, 7d, monthly).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    /// Stable identifier (e.g. "5h", "7d", "monthly").
    pub id: String,
    /// Human label (e.g. "5 Hour", "7 Day").
    pub label: String,
    pub duration_ms: Option<u64>,
    /// Absolute reset timestamp in Unix milliseconds.
    pub resets_at_ms: Option<u64>,
    /// Verb before the reset countdown ("tick", "regen"); `None` means "resets".
    pub reset_label: Option<String>,
}

/// Quantitative usage data.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageAmount {
    pub used: Option<f64>,
    pub limit: Option<f64>,
    pub remaining: Option<f64>,
    pub used_fraction: Option<f64>,
    pub remaining_fraction: Option<f64>,
    pub unit: UsageUnit,
}

/// What a limit applies to.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageScope {
    pub provider: String,
    pub account_id: Option<String>,
    pub project_id: Option<String>,
    pub org_id: Option<String>,
    pub model_id: Option<String>,
    pub tier: Option<String>,
    pub window_id: Option<String>,
    pub shared: Option<bool>,
    /// Stable identity shared by routing-specific copies of one upstream quota.
    pub shared_group: Option<String>,
}

/// Normalized limit entry for a single window or quota bucket.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageLimit {
    pub id: String,
    pub label: String,
    pub scope: UsageScope,
    pub window: Option<UsageWindow>,
    pub amount: UsageAmount,
    pub status: Option<UsageStatus>,
    #[serde(default)]
    pub notes: Vec<String>,
}

/// Per-credit detail for a saved/banked rate-limit reset.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageResetCreditDetail {
    pub id: Option<String>,
    pub title: Option<String>,
    pub program: Option<String>,
    pub remaining_count: Option<u64>,
    pub usable: Option<bool>,
    pub requires_limit: Option<bool>,
    /// Normalized [`UsageLimit::id`]s this credit resets.
    #[serde(default)]
    pub clears: Vec<String>,
    /// Normalized limit ids currently preventing redemption.
    #[serde(default)]
    pub blocking: Vec<String>,
    /// Used fractions for covered limits, keyed by normalized limit id.
    #[serde(default)]
    pub used_fractions: BTreeMap<String, f64>,
    /// ISO-8601 grant time, as reported.
    pub granted_at: Option<String>,
    /// ISO-8601 expiry, as reported.
    pub expires_at: Option<String>,
    pub status: Option<String>,
}

/// Saved/banked rate-limit resets an account can redeem on demand.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageResetCredits {
    /// Banked resets, including grants that are not currently usable.
    pub available_count: u64,
    /// Resets the provider says can be redeemed now.
    pub redeemable_count: Option<u64>,
    pub next_credit_id: Option<String>,
    pub eligible: Option<bool>,
    pub reason: Option<String>,
    /// ISO-8601 time until which redemption is cooling down.
    pub cooldown_until: Option<String>,
    #[serde(default)]
    pub credits: Vec<UsageResetCreditDetail>,
}

/// Aggregated usage report for one provider account.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageReport {
    pub provider: String,
    /// Unix ms the report was fetched.
    pub fetched_at_ms: u64,
    pub limits: Vec<UsageLimit>,
    pub reset_credits: Option<UsageResetCredits>,
    /// Provider-wide disclaimers shown once above per-account sections.
    #[serde(default)]
    pub notes: Vec<String>,
    /// Small non-secret facts (plan name, account email, …).
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

impl UsageReport {
    /// The account's plan or tier for display (`Plus`, `Max 20x`), from whichever metadata key
    /// the provider fills: `plan`, `planType` (ChatGPT) or `currentTierName` (Gemini).
    pub fn plan(&self) -> Option<String> {
        ["plan", "planType", "currentTierName"]
            .iter()
            .find_map(|key| self.metadata.get(*key))
            .map(|raw| plan_label(raw))
            .filter(|label| !label.is_empty())
    }
}

/// A readable plan name from a provider identifier: `plus` → `Plus`, `claude_max` → `Max`,
/// `max_20x` → `Max 20x`. A name that already has capitals or spaces is kept as sent.
pub fn plan_label(raw: &str) -> String {
    let raw = raw.trim();
    if raw.chars().any(|ch| ch.is_uppercase() || ch == ' ') {
        return raw.to_owned();
    }
    let raw = ["claude_", "chatgpt_"]
        .iter()
        .find_map(|prefix| raw.strip_prefix(prefix))
        .unwrap_or(raw);
    raw.split(['_', '-'])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Used fraction (0..1; >1 means overage) from whichever amount fields are populated.
/// Precedence: explicit fraction > used/limit > percent-unit used > inverted remaining.
pub fn resolve_used_fraction(limit: &UsageLimit) -> Option<f64> {
    let amount = &limit.amount;
    if let Some(fraction) = amount.used_fraction {
        return Some(fraction);
    }
    if let (Some(used), Some(max)) = (amount.used, amount.limit)
        && max > 0.0
    {
        return Some(used / max);
    }
    if amount.unit == UsageUnit::Percent
        && let Some(used) = amount.used
    {
        return Some(used / 100.0);
    }
    amount
        .remaining_fraction
        .map(|remaining| (1.0 - remaining).max(0.0))
}

/// One recorded limit snapshot: a single window of one account at one time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageHistoryEntry {
    /// Unix ms the report was fetched.
    pub recorded_at_ms: u64,
    pub provider: String,
    /// Stable credential identity (account id, email, or project).
    pub account_key: String,
    pub limit_id: String,
    pub label: String,
    pub window_label: Option<String>,
    pub used_fraction: Option<f64>,
    pub status: Option<UsageStatus>,
    pub resets_at_ms: Option<u64>,
}

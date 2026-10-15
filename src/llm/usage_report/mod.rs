//! Provider subscription/quota reporting.
//!
//! Public API for `/usage` and the status line:
//! - [`fetch`]: one provider's report. `Ok(None)` when the provider has no stored credentials;
//!   a successful report is appended to the usage history.
//! - [`fetch_all`]: every supported provider that is signed in, with per-provider errors kept.
//! - [`status_summary`]: one short line (tightest limit per provider) for the status line.
//! - [`history::read`]: recorded snapshots, newest-last.
//!
//! Each provider module exposes `fetch(user_dir, cancel) -> Result<Option<UsageReport>>`, and a
//! pure `parse(...)` used by fixture tests.
mod antigravity;
mod claude;
mod codex;
mod copilot;
mod cursor;
mod factory;
mod gemini;
pub mod history;
mod kimi;
pub mod resets;
pub mod types;
mod xai;
mod zai;

pub use types::*;

use anyhow::{Result, bail};
use std::{
    path::Path,
    sync::{Arc, atomic::AtomicBool},
};

/// Dial provider ids with a usage endpoint, in display order.
pub const PROVIDERS: &[&str] = &[
    "anthropic",
    "openai-codex",
    "copilot",
    "google-gemini-cli",
    "google-antigravity",
    "kimi-code",
    "zai",
    "xai-oauth",
    "factory-droid",
    "cursor",
];

/// Fetch one provider's current report. `Ok(None)` when no credentials are stored.
pub fn fetch(
    provider: &str,
    user_dir: &Path,
    cancel: &Arc<AtomicBool>,
) -> Result<Option<UsageReport>> {
    let report = match provider {
        "anthropic" => claude::fetch(user_dir, cancel)?,
        "openai-codex" => codex::fetch(user_dir, cancel)?,
        "copilot" => copilot::fetch(user_dir, cancel)?,
        "google-gemini-cli" => gemini::fetch(user_dir, cancel)?,
        "google-antigravity" => antigravity::fetch(user_dir, cancel)?,
        "kimi-code" => kimi::fetch(user_dir, cancel)?,
        "zai" => zai::fetch(user_dir, cancel)?,
        "xai-oauth" => xai::fetch(user_dir, cancel)?,
        "factory-droid" => factory::fetch(user_dir, cancel)?,
        "cursor" => cursor::fetch(user_dir, cancel)?,
        other => bail!("usage reporting is not supported for provider {other:?}"),
    };
    let mut report = report;
    if let Some(report) = report.as_mut()
        && let Err(error) = history::append(user_dir, report)
    {
        report
            .notes
            .push(format!("usage history not recorded: {error}"));
    }
    Ok(report)
}

/// Outcome of one provider in [`fetch_all`].
pub struct Outcome {
    pub provider: &'static str,
    pub result: Result<UsageReport, String>,
}

/// Fetch every supported provider that has stored credentials. Providers without credentials
/// are omitted; failures are reported per provider rather than aborting the rest.
pub fn fetch_all(user_dir: &Path, cancel: &Arc<AtomicBool>) -> Vec<Outcome> {
    PROVIDERS
        .iter()
        .filter_map(|&provider| match fetch(provider, user_dir, cancel) {
            Ok(Some(report)) => Some(Outcome {
                provider,
                result: Ok(report),
            }),
            Ok(None) => None,
            Err(error) => Some(Outcome {
                provider,
                result: Err(format!("{error:#}")),
            }),
        })
        .collect()
}

/// One status-line fragment per report: the tightest limit, e.g. `anthropic 5h 80% · resets 2h`.
/// `now_ms` is used to render the reset countdown.
pub fn status_summary(reports: &[UsageReport], now_ms: u64) -> Option<String> {
    let parts: Vec<String> = reports
        .iter()
        .filter_map(|report| {
            let limit = report
                .limits
                .iter()
                .filter_map(|limit| resolve_used_fraction(limit).map(|used| (used, limit)))
                .max_by(|a, b| a.0.total_cmp(&b.0))?;
            let (used, limit) = limit;
            let mut part = format!(
                "{} {} {:.0}%",
                report.provider,
                limit
                    .window
                    .as_ref()
                    .map_or(limit.label.as_str(), |w| w.label.as_str()),
                used * 100.0
            );
            if let Some(resets_at) = limit.window.as_ref().and_then(|w| w.resets_at_ms)
                && resets_at > now_ms
            {
                part.push_str(&format!(" · resets {}", humanize_ms(resets_at - now_ms)));
            }
            Some(part)
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(" | "))
}

fn humanize_ms(ms: u64) -> String {
    let minutes = ms / 60_000;
    match minutes {
        0 => "<1m".into(),
        m if m < 60 => format!("{m}m"),
        m if m < 60 * 48 => format!("{}h{}m", m / 60, m % 60),
        m => format!("{}d", m / (60 * 24)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit(label: &str, used: f64, resets_at_ms: Option<u64>) -> UsageLimit {
        UsageLimit {
            id: label.into(),
            label: label.into(),
            scope: UsageScope {
                provider: "anthropic".into(),
                ..Default::default()
            },
            window: Some(UsageWindow {
                id: label.into(),
                label: label.into(),
                resets_at_ms,
                ..Default::default()
            }),
            amount: UsageAmount {
                used: Some(used),
                limit: Some(100.0),
                remaining: None,
                used_fraction: None,
                remaining_fraction: None,
                unit: UsageUnit::Percent,
            },
            status: None,
            notes: vec![],
        }
    }

    #[test]
    fn fraction_precedence_matches_omp() {
        let mut l = limit("5h", 50.0, None);
        assert_eq!(resolve_used_fraction(&l), Some(0.5));
        l.amount.used_fraction = Some(0.9);
        assert_eq!(resolve_used_fraction(&l), Some(0.9));
        l.amount.used_fraction = None;
        l.amount.unit = UsageUnit::Requests;
        l.amount.used = Some(3.0);
        l.amount.limit = Some(4.0);
        assert_eq!(resolve_used_fraction(&l), Some(0.75));
        l.amount.limit = None;
        l.amount.used = None;
        l.amount.remaining_fraction = Some(0.25);
        assert_eq!(resolve_used_fraction(&l), Some(0.75));
    }

    #[test]
    fn status_summary_picks_tightest_limit_and_countdown() {
        let now = 1_000_000_000;
        let report = UsageReport {
            provider: "anthropic".into(),
            fetched_at_ms: now,
            limits: vec![
                limit("5h", 80.0, Some(now + 2 * 3_600_000 + 5 * 60_000)),
                limit("7d", 10.0, None),
            ],
            reset_credits: None,
            notes: vec![],
            metadata: Default::default(),
        };
        assert_eq!(
            status_summary(&[report], now).as_deref(),
            Some("anthropic 5h 80% · resets 2h5m")
        );
        assert_eq!(status_summary(&[], now), None);
    }
}

//! Usage history: one JSON line per limit per fetched report, kept under the user dir.
//! Bounded to the newest `MAX_ENTRIES` lines so the file stays well under the private-store size cap.

use std::path::Path;

use anyhow::Result;

use super::types::{UsageHistoryEntry, UsageReport, resolve_used_fraction};
use crate::config::settings_store::{locked, write_private};

const FILE: &str = "usage-history.jsonl";
const MAX_ENTRIES: usize = 2_048;

/// Appends one history entry per limit in `report`, trimming the oldest entries past the cap.
pub fn append(user_dir: &Path, report: &UsageReport) -> Result<()> {
    let fresh = entries_for(report);
    if fresh.is_empty() {
        return Ok(());
    }
    let path = user_dir.join(FILE);
    let _lock = locked(&path)?;
    let mut all = read_entries(&path)?;
    all.extend(fresh);
    if all.len() > MAX_ENTRIES {
        let excess = all.len() - MAX_ENTRIES;
        all.drain(..excess);
    }
    write_private(&path, &encode(&all)?)
}

fn encode(entries: &[UsageHistoryEntry]) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for entry in entries {
        serde_json::to_writer(&mut bytes, entry)?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}

/// Reads history oldest-first, optionally restricted to one provider. Corrupt lines are skipped.
#[cfg(test)]
pub fn read(user_dir: &Path, provider: Option<&str>) -> Result<Vec<UsageHistoryEntry>> {
    let mut entries = read_entries(&user_dir.join(FILE))?;
    if let Some(provider) = provider {
        entries.retain(|entry| entry.provider == provider);
    }
    Ok(entries)
}

fn read_entries(path: &Path) -> Result<Vec<UsageHistoryEntry>> {
    let Some(bytes) = crate::platform::atomic::read_private(path)? else {
        return Ok(Vec::new());
    };
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| serde_json::from_str::<UsageHistoryEntry>(line).ok())
        .collect())
}

/// Builds the history entries a report contributes (pure; no I/O).
pub(crate) fn entries_for(report: &UsageReport) -> Vec<UsageHistoryEntry> {
    let account_key = account_key(report);
    report
        .limits
        .iter()
        .map(|limit| UsageHistoryEntry {
            recorded_at_ms: report.fetched_at_ms,
            provider: report.provider.clone(),
            account_key: account_key.clone(),
            limit_id: limit.id.clone(),
            label: limit.label.clone(),
            window_label: limit.window.as_ref().map(|window| window.label.clone()),
            used_fraction: resolve_used_fraction(limit),
            status: limit.status,
            resets_at_ms: limit.window.as_ref().and_then(|window| window.resets_at_ms),
        })
        .collect()
}

fn account_key(report: &UsageReport) -> String {
    ["account", "accountId", "email"]
        .iter()
        .find_map(|key| {
            report
                .metadata
                .get(*key)
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
        })
        .unwrap_or("")
        .to_owned()
}

#[cfg(test)]
mod tests;

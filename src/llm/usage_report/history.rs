//! Usage history: one JSON line per limit per fetched report, kept under the user dir.
//! Bounded to the newest `MAX_ENTRIES` lines so the file stays well under the private-store size cap.

use std::path::Path;

use anyhow::Result;

use super::types::{UsageHistoryEntry, UsageReport, resolve_used_fraction};
use crate::storage::settings_store::{locked, read_private, write_private};

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
    let Some(bytes) = read_private(path)? else {
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
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::llm::usage_report::types::{
        UsageAmount, UsageLimit, UsageScope, UsageStatus, UsageUnit, UsageWindow,
    };

    fn report(provider: &str, account: &str, fetched_at_ms: u64, used: f64) -> UsageReport {
        let mut metadata = BTreeMap::new();
        if !account.is_empty() {
            metadata.insert("account".to_owned(), account.to_owned());
        }
        UsageReport {
            provider: provider.to_owned(),
            fetched_at_ms,
            limits: vec![UsageLimit {
                id: "lim".to_owned(),
                label: "Limit".to_owned(),
                scope: UsageScope {
                    provider: provider.to_owned(),
                    ..UsageScope::default()
                },
                window: Some(UsageWindow {
                    id: "5h".to_owned(),
                    label: "5 hours".to_owned(),
                    resets_at_ms: Some(fetched_at_ms + 1_000),
                    ..UsageWindow::default()
                }),
                amount: UsageAmount {
                    used: Some(used),
                    limit: Some(100.0),
                    remaining: None,
                    used_fraction: Some(used / 100.0),
                    remaining_fraction: None,
                    unit: UsageUnit::Percent,
                },
                status: Some(UsageStatus::Ok),
                notes: Vec::new(),
            }],
            reset_credits: None,
            notes: Vec::new(),
            metadata,
        }
    }

    #[test]
    fn append_then_read_round_trips_and_filters_by_provider() {
        let dir = tempfile::tempdir().unwrap();
        append(
            dir.path(),
            &report("anthropic", "a@example.com", 1_000, 10.0),
        )
        .unwrap();
        append(dir.path(), &report("copilot", "gh-user", 2_000, 20.0)).unwrap();

        let all = read(dir.path(), None).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].provider, "anthropic");
        assert_eq!(all[0].account_key, "a@example.com");
        assert_eq!(all[0].used_fraction, Some(0.1));
        assert_eq!(all[0].window_label.as_deref(), Some("5 hours"));
        assert_eq!(all[0].resets_at_ms, Some(2_000));
        assert_eq!(all[1].provider, "copilot");

        let only_copilot = read(dir.path(), Some("copilot")).unwrap();
        assert_eq!(only_copilot.len(), 1);
        assert_eq!(only_copilot[0].account_key, "gh-user");
    }

    #[test]
    fn corrupt_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &report("anthropic", "", 1_000, 5.0)).unwrap();
        let path = dir.path().join(FILE);
        let mut bytes = read_private(&path).unwrap().unwrap();
        bytes.extend_from_slice(b"{not json\n");
        write_private(&path, &bytes).unwrap();
        append(dir.path(), &report("anthropic", "", 2_000, 6.0)).unwrap();

        let entries = read(dir.path(), None).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].recorded_at_ms, 2_000);
    }

    #[test]
    fn history_is_capped_to_newest_entries() {
        let dir = tempfile::tempdir().unwrap();
        let seeded: Vec<UsageHistoryEntry> = (0..(MAX_ENTRIES as u64 + 5))
            .flat_map(|i| entries_for(&report("anthropic", "", i, 1.0)))
            .collect();
        write_private(&dir.path().join(FILE), &encode(&seeded).unwrap()).unwrap();
        append(
            dir.path(),
            &report("anthropic", "", MAX_ENTRIES as u64 + 5, 1.0),
        )
        .unwrap();
        let entries = read(dir.path(), None).unwrap();
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert_eq!(entries[0].recorded_at_ms, 6);
        assert_eq!(
            entries.last().unwrap().recorded_at_ms,
            MAX_ENTRIES as u64 + 5
        );
    }

    #[test]
    fn missing_history_reads_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(dir.path(), None).unwrap().is_empty());
    }
}

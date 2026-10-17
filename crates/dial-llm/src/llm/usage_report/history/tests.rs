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
    let mut bytes = crate::platform::atomic::read_private(&path)
        .unwrap()
        .unwrap();
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

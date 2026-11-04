use super::*;

#[test]
fn stats_tabs_use_persisted_cross_session_counts_and_keep_the_legacy_report() {
    let home = tempfile::tempdir().unwrap();
    let mut db = Db::open_default(home.path()).unwrap();
    db.record_request(&stats::RequestRecord {
        ts_ms: 1,
        session: "fixture".into(),
        project: "fixture".into(),
        model: "fixture".into(),
        input: 123,
        output: 45,
        cache_read: 12,
        cache_write: 3,
        reasoning: 6,
        reported: true,
        cost_usd: Some(0.5),
        ..Default::default()
    })
    .unwrap();
    let report = db.report(None, 1000).unwrap();
    let tabs = stats_tabs(&report);
    assert_eq!(tabs[0].1, stats::render_text(&report));
    let tokens = &tabs.iter().find(|(name, _)| *name == "Tokens").unwrap().1;
    for value in [
        "Input: 123",
        "Output: 45",
        "Cache read: 12",
        "Cache write: 3",
        "Reasoning: 6",
        "1 / 1",
    ] {
        assert!(tokens.contains(value), "{tokens}");
    }
    let tools = &tabs.iter().find(|(name, _)| *name == "Tools").unwrap().1;
    assert!(tools.contains("No timed tool calls recorded."));
}

use super::lessons::{MAX_TRAJECTORY_REFS, META_KEY, load, record, summary};
use super::*;
use crate::config::types::Config;
use crate::storage::store::Store;

fn open() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::default();
    let store = Store::open(dir.path(), "test-model", "test", &config.harness).unwrap();
    (dir, store)
}

fn signals(category: &str, ids: &[i64]) -> Signals {
    Signals {
        recurring_trajectories: BTreeMap::from([(category.to_owned(), ids.to_vec())]),
        ..Signals::default()
    }
}

#[test]
fn recurrence_threshold_counts_distinct_trajectories_only() {
    let cases: &[(&str, &[i64], usize)] = &[
        ("config_precedence", &[7], 0),
        ("config_precedence", &[7, 7, 7], 0),
        ("config_precedence", &[7, 8], 1),
        ("negative_search", &[1, 2, 3], 1),
        ("unknown_category", &[1, 2, 3], 0),
    ];
    for (category, ids, expected) in cases {
        let (_dir, store) = open();
        let report = record(&store, &signals(category, ids)).unwrap();
        assert_eq!(
            report["proposed"].as_array().unwrap().len(),
            *expected,
            "{category} {ids:?}"
        );
    }
}

#[test]
fn proposals_are_review_gated_deduplicated_and_never_activate() {
    let (_dir, store) = open();
    let active = store.active().unwrap().id;
    record(&store, &signals("semantic_validation", &[1, 2])).unwrap();
    record(&store, &signals("semantic_validation", &[2, 3])).unwrap();
    let stored = load(&store).unwrap();
    assert_eq!(stored.len(), 1, "one candidate per category");
    assert_eq!(stored[0]["status"], "proposed");
    assert_eq!(stored[0]["requires_review"], true);
    assert_eq!(stored[0]["trajectories"], json!([3, 2, 1]));
    assert_eq!(stored[0]["distinct_trajectories"], 3);
    assert_eq!(store.active().unwrap().id, active);
}

#[test]
fn references_are_bounded_and_store_no_private_text() {
    let (_dir, store) = open();
    let ids: Vec<i64> = (1..=100).collect();
    record(&store, &signals("config_precedence", &ids)).unwrap();
    let stored = load(&store).unwrap();
    assert_eq!(
        stored[0]["trajectories"].as_array().unwrap().len(),
        MAX_TRAJECTORY_REFS
    );
    let raw = store.get_meta(META_KEY).unwrap().unwrap();
    assert!(!raw.contains("PRIVATE"));
    assert_eq!(stored[0]["summary"], summary("config_precedence").unwrap());
}

#[test]
fn malformed_metadata_is_reported_instead_of_silently_erasing_proposals() {
    let (_dir, store) = open();
    for raw in ["not json", r#"[{"category":"unknown","status":"active"}]"#] {
        store.set_meta(META_KEY, raw).unwrap();
        assert!(lesson_proposals(&store).is_err());
        assert_eq!(store.get_meta(META_KEY).unwrap().as_deref(), Some(raw));
    }
}

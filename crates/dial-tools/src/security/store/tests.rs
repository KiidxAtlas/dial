use super::*;

#[test]
fn format_utc_matches_known_epoch_values() {
    assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
    assert_eq!(format_utc(1_700_000_000), "2023-11-14T22:13:20Z");
}

#[test]
fn invalid_ids_are_rejected_before_touching_disk() {
    assert!(!is_valid_id("secplan_", "secplan_"));
}

#[test]
fn atomic_write_is_private_and_replaces() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a").join("doc.json");
    write_private_atomic(&path, b"one").unwrap();
    write_private_atomic(&path, b"two").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"two");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn store_roundtrips_index_and_rejects_foreign_index() {
    let user = tempfile::tempdir().unwrap();
    let store = Store::open(user.path(), "/repo/one").unwrap();
    assert!(store.index().unwrap().scan_ids.is_empty());
    let other = Store::open(user.path(), "/repo/two").unwrap();
    let index = Index {
        schema_version: 1,
        project_key: other.project_key().into(),
        ..Index::default()
    };
    fs::write(
        store.dir().join(INDEX_FILE),
        serde_json::to_vec(&index).unwrap(),
    )
    .unwrap();
    let error = store.index().unwrap_err().to_string();
    assert!(error.starts_with("Security scan project key"), "{error}");
}

#[test]
fn missing_plans_are_unknown_but_unreadable_ones_keep_their_cause() {
    let user = tempfile::tempdir().unwrap();
    let store = Store::open(user.path(), "/repo/one").unwrap();
    let missing = store.load_plan("secplan_abc").unwrap_err().to_string();
    assert_eq!(missing, "Unknown security plan: secplan_abc");
    // A directory where the plan document belongs is a read failure, not an unknown id.
    fs::create_dir_all(store.dir().join("plans").join("secplan_dir.json")).unwrap();
    let error = format!("{:#}", store.load_plan("secplan_dir").unwrap_err());
    assert!(
        error.starts_with("cannot read security plan secplan_dir: "),
        "{error}"
    );
}

#[test]
fn corrupt_index_reports_the_parse_cause() {
    let user = tempfile::tempdir().unwrap();
    let store = Store::open(user.path(), "/repo/one").unwrap();
    fs::write(store.dir().join(INDEX_FILE), b"{\"schema_version\":").unwrap();
    let error = format!("{:#}", store.index().unwrap_err());
    assert!(
        error.starts_with("Invalid security store index at "),
        "{error}"
    );
    assert!(error.contains("EOF"), "{error}");
}

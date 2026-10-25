use super::*;
use serde_json::json;

#[test]
fn absent_schema_version_is_legacy_and_current_is_accepted() {
    assert_eq!(
        header_version(&json!({"type":"session","version":"0.9.1"})).unwrap(),
        0
    );
    assert_eq!(
        header_version(&json!({"schema_version": CURRENT_SCHEMA_VERSION})).unwrap(),
        CURRENT_SCHEMA_VERSION
    );
}

#[test]
fn newer_or_malformed_schema_versions_are_refused() {
    let newer = header_version(&json!({"schema_version": CURRENT_SCHEMA_VERSION + 1}))
        .unwrap_err()
        .to_string();
    assert!(
        newer.contains("newer") && newer.contains("update Dial"),
        "{newer}"
    );
    for bad in [
        json!("1"),
        json!(-1),
        json!(1.5),
        json!(null),
        json!(u64::MAX),
    ] {
        assert!(
            header_version(&json!({ "schema_version": bad })).is_err(),
            "{bad} must be refused"
        );
    }
}

fn step_one(record: &mut Value) {
    record["trail"] = json!(format!("{}1", record["trail"].as_str().unwrap_or("")));
}
fn step_two(record: &mut Value) {
    record["trail"] = json!(format!("{}2", record["trail"].as_str().unwrap_or("")));
}

#[test]
fn record_steps_run_in_order_from_the_journals_version_only() {
    let steps = [
        RecordStep {
            from: 0,
            run: step_one,
        },
        RecordStep {
            from: 1,
            run: step_two,
        },
    ];
    let upgrade = |from| {
        let mut record = json!({});
        apply(&steps, from, 2, &mut record);
        record["trail"].as_str().unwrap_or("").to_owned()
    };
    assert_eq!(upgrade(0), "12");
    assert_eq!(upgrade(1), "2");
    assert_eq!(upgrade(2), "");
}

#[test]
fn current_journals_pass_through_unchanged() {
    let original = json!({"type":"message","id":"a","message":{"role":"user","content":"x"}});
    let mut record = original.clone();
    upgrade_record(0, &mut record);
    upgrade_record(CURRENT_SCHEMA_VERSION, &mut record);
    assert_eq!(record, original);
}

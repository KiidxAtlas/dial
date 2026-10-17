use super::*;

#[test]
fn context_overrides_accept_auto_without_restricting_catalog_capacities() {
    for value in [serde_json::Value::Null, serde_json::json!("auto")] {
        let row: ModelSelection =
            serde_json::from_value(serde_json::json!({"context_window":value})).unwrap();
        assert_eq!(row.context_window, None);
    }
    let row: ModelSelection =
        serde_json::from_value(serde_json::json!({"context_window":2_000_000})).unwrap();
    assert_eq!(row.context_window, Some(2_000_000));
    assert_eq!(
        serde_json::from_value::<ModelSelection>(serde_json::to_value(&row).unwrap()).unwrap(),
        row
    );
    assert!(
        serde_json::from_value::<ModelSelection>(serde_json::json!({"context_window":"invalid"}))
            .is_err()
    );
}

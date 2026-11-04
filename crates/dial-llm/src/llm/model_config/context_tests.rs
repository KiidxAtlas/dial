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

#[test]
fn role_explanations_keep_assignment_defaults_and_runtime_distinct() {
    for (role, source, expected) in [
        ("main", RoleSource::Main, "current session"),
        ("task", RoleSource::Assigned, "explicit role assignment"),
        ("decision", RoleSource::FollowsSmall, "follows Fast helpers"),
        (
            "tester",
            RoleSource::FollowsMain,
            "Review & tests → Advisor → Task workers",
        ),
        (
            "memory",
            RoleSource::NotSet,
            "model-backed memory needs assignment",
        ),
        ("embedding", RoleSource::NotSet, "lexical recall only"),
    ] {
        let status = RoleStatus {
            role,
            assigned: None,
            effective: None,
            source,
        };
        let explanation = status.explanation();
        assert!(explanation.contains(expected), "{explanation}");
        assert!(explanation.contains("runtime fallback/promotion not reported"));
        assert_ne!(role_label(role), role);
    }
    assert_eq!(role_label("future-role"), "future-role");
}

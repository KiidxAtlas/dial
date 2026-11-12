use super::*;

fn layout() -> (tempfile::TempDir, tempfile::TempDir, Layout) {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
    (project, home, layout)
}

#[test]
fn schema_lists_declared_settings() {
    let schema = schema();
    let settings = schema["settings"].as_array().unwrap();
    assert!(
        settings
            .iter()
            .any(|setting| setting["path"] == "harness.max_steps")
    );
}

#[test]
fn set_get_and_reset_round_trip_through_the_config_owner() {
    let (_project, _home, layout) = layout();
    let saved = set(&layout, &json!({"key":"harness.max_steps","value":7})).unwrap();
    assert_eq!(
        saved,
        json!({"key":"harness.max_steps","scope":"global","saved":true,"restart_required":true})
    );
    assert_eq!(
        get(&layout, &json!({"key":"harness.max_steps"})).unwrap()["value"],
        7
    );
    let all = get(&layout, &json!({})).unwrap();
    assert!(
        all["settings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["key"] == "harness.max_steps" && entry["value"] == 7),
        "{all}"
    );
    // Out-of-range values are refused by the schema, as `dial config set` refuses them.
    assert!(set(&layout, &json!({"key":"harness.max_steps","value":1000})).is_err());
    assert!(set(&layout, &json!({"key":"no.such.setting","value":1})).is_err());
    assert!(set(&layout, &json!({"key":"harness.max_steps"})).is_err());
    assert!(
        set(
            &layout,
            &json!({"key":"harness.max_steps","value":7,"scope":"team"})
        )
        .is_err()
    );

    let removed = reset(&layout, &json!({"key":"harness.max_steps"})).unwrap();
    assert_eq!(removed["removed"], true);
    let again = reset(&layout, &json!({"key":"harness.max_steps"})).unwrap();
    assert_eq!(again["removed"], false);
    assert_eq!(again["restart_required"], false);
}

#[test]
fn project_scope_writes_the_project_layer() {
    let (_project, _home, layout) = layout();
    let saved = set(
        &layout,
        &json!({"key":"harness.max_steps","value":9,"scope":"project"}),
    )
    .unwrap();
    assert_eq!(saved["scope"], "project");
    assert_eq!(
        reset(&layout, &json!({"key":"harness.max_steps"})).unwrap()["removed"],
        false
    );
    assert_eq!(
        reset(
            &layout,
            &json!({"key":"harness.max_steps","scope":"project"})
        )
        .unwrap()["removed"],
        true
    );
}

#[test]
fn malformed_settings_requests_are_invalid_params() {
    let (_project, _home, layout) = layout();
    let code = |error: anyhow::Error| {
        error
            .downcast_ref::<super::super::RequestError>()
            .map_or("failed", |error| error.code.as_str())
    };
    assert_eq!(
        code(set(&layout, &json!({"value":1})).unwrap_err()),
        "invalid_params"
    );
    assert_eq!(
        code(set(&layout, &json!({"key":" ","value":1})).unwrap_err()),
        "invalid_params"
    );
    assert_eq!(
        code(set(&layout, &json!({"key":"model"})).unwrap_err()),
        "invalid_params"
    );
    assert_eq!(
        code(set(&layout, &json!({"key":"model","value":"x","scope":"team"})).unwrap_err()),
        "invalid_params"
    );
    assert_eq!(
        code(reset(&layout, &json!({})).unwrap_err()),
        "invalid_params"
    );
}

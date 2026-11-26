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

#[test]
fn get_reports_the_most_specific_layer_that_sets_each_key() {
    let (_project, _home, layout) = layout();
    set(&layout, &json!({"key":"harness.max_steps","value":7})).unwrap();
    set(&layout, &json!({"key":"model","value":"global-model"})).unwrap();
    set(
        &layout,
        &json!({"key":"model","value":"project-model","scope":"project"}),
    )
    .unwrap();
    let all = get(&layout, &json!({})).unwrap();
    let entry = |key: &str| {
        all["settings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["key"] == key)
            .cloned()
            .unwrap_or_else(|| panic!("{key} missing from {all}"))
    };
    // Project overrides global; a global-only key reports global.
    for (key, scope) in [("model", "project"), ("harness.max_steps", "global")] {
        assert_eq!(entry(key)["scope"], scope, "{key}");
        assert_eq!(
            get(&layout, &json!({"key": key})).unwrap()["scope"],
            scope,
            "{key}"
        );
    }
    assert_eq!(entry("model")["value"], "project-model");
    // A key neither layer sets carries no scope.
    let unset = all["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["key"] != "model" && entry["key"] != "harness.max_steps");
    let unset = unset.expect("defaults list keys neither layer sets");
    assert!(unset.get("scope").is_none(), "{unset}");
}

#[test]
fn default_thinking_is_model_checked_persistent_and_scoped() {
    let (_project, _home, layout) = layout();
    set(&layout, &json!({"key":"model", "value":"claude-opus-5-5"})).unwrap();
    set(&layout, &json!({"key":"provider", "value":"anthropic"})).unwrap();
    let edit = |level, scope| {
        default_thinking(
            &layout,
            &json!({"provider":"anthropic", "model_id":"claude-opus-5-5", "level":level, "scope":scope}),
            true,
        )
    };
    assert_eq!(edit("high", "global").unwrap()["level"], "high");
    assert_eq!(edit("medium", "project").unwrap()["level"], "medium");
    assert!(edit("unknown", "global").is_err());
    assert_eq!(
        default_thinking(&layout, &json!({}), false).unwrap()["level"],
        "high"
    );
    assert_eq!(edit("default", "project").unwrap()["level"], "high");
    assert!(edit("default", "global").unwrap()["level"].is_null());
    set(
        &layout,
        &json!({"key":"model", "value":"other", "scope":"project"}),
    )
    .unwrap();
    assert!(edit("high", "project").is_err());
    let other = default_thinking(&layout, &json!({"scope":"project"}), false).unwrap();
    assert!(other["levels"].as_array().unwrap().is_empty());
    assert!(
        default_thinking(
            &layout,
            &json!({"scope":"project", "provider":"anthropic", "model_id":"other", "level":"high"}),
            true
        )
        .is_err()
    );
}

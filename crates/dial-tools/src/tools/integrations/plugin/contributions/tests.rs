use super::*;

fn registrations() -> Contributions {
    Contributions::parse(json!({"views":[{"id":"main","title":"Main","location":"tab","elements":[{"type":"text","text":"Hello"}]}]})).unwrap()
}
#[test]
fn view_updates_are_revisioned_owner_scoped_and_unloaded() {
    let mut state = ContributionState::default();
    let contributions = registrations();
    state.register("one", &contributions).unwrap();
    let first = state.views().remove(0);
    assert!(
        state
            .update_view("two", first.view.clone(), first.revision)
            .is_err()
    );
    let next = state
        .update_view("one", first.view.clone(), first.revision)
        .unwrap();
    assert!(next > first.revision);
    assert!(
        state
            .update_view("one", first.view.clone(), first.revision)
            .is_err()
    );
    assert!(state.remove_view("two", "main").is_err());
    state.remove_view("one", "main").unwrap();
    state.register("one", &contributions).unwrap();
    assert!(state.views()[0].revision > next);
    assert!(state.update_view("one", first.view, next).is_err());
    state.remove_owner("one");
    assert!(state.views().is_empty());
}
#[test]
fn settings_layer_defaults_validate_and_never_persist_secrets() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    fs::create_dir(&project).unwrap();
    let layout = Layout::with_user_dir(&project, temp.path().join("private")).unwrap();
    let store = SettingsStore::new(&layout);
    let schema = json!({"type":"object","properties":{"count":{"type":"integer","default":2,"minimum":1},"token":{"type":"string","writeOnly":true,"default":"never-export"}},"required":["count","token"],"additionalProperties":false});
    store.register_schema("one", &schema).unwrap();
    assert_eq!(store.schema("one").unwrap(), schema);
    assert_eq!(store.load("one", &schema).unwrap(), json!({"count":2}));
    store
        .set("one", &schema, SettingsScope::Global, json!({"count":3}))
        .unwrap();
    store
        .set("one", &schema, SettingsScope::Workspace, json!({"count":4}))
        .unwrap();
    assert_eq!(store.load("one", &schema).unwrap(), json!({"count":4}));
    assert_eq!(store.load("two", &schema).unwrap(), json!({"count":2}));
    for value in [
        json!({"count":0}),
        json!({"count":"bad"}),
        json!({"token":"never-save"}),
        json!({"unknown":true}),
    ] {
        assert!(
            store
                .set("one", &schema, SettingsScope::Workspace, value)
                .is_err()
        );
    }
    assert_eq!(store.export("one", &schema).unwrap(), json!({"count":4}));
    store.reset("one", SettingsScope::Workspace).unwrap();
    assert_eq!(store.load("one", &schema).unwrap(), json!({"count":3}));
    store
        .set_secret("one", "token", "test-secret-value")
        .unwrap();
    assert_eq!(
        store.secret("one", "token").unwrap().as_deref(),
        Some("test-secret-value")
    );
    assert!(store.secret("two", "token").unwrap().is_none());
    store.remove_secret("one", "token").unwrap();
    assert!(store.secret("one", "token").unwrap().is_none());
    assert!(!project.join("extension-settings").exists());
    assert!(store.path("../escape", SettingsScope::Global).is_err());
    store
        .set_secret("one", "token", "purge-this-value")
        .unwrap();
    store.set_secret("two", "token", "keep-this-value").unwrap();
    store.purge("one").unwrap();
    assert!(store.secret("one", "token").unwrap().is_none());
    assert_eq!(
        store.secret("two", "token").unwrap().as_deref(),
        Some("keep-this-value")
    );
    assert!(store.schema("one").is_err());
    assert_eq!(store.load("one", &schema).unwrap(), json!({"count":2}));
    store.purge("two").unwrap();
}

#[test]
fn patch_preserves_layer_boundaries_and_inherits_required_values() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    fs::create_dir(&project).unwrap();
    let layout = Layout::with_user_dir(&project, temp.path().join("private")).unwrap();
    let store = SettingsStore::new(&layout);
    let schema = json!({"type":"object","properties":{"name":{"type":"string"},"count":{"type":"integer","default":1}},"required":["name"],"additionalProperties":false});
    store
        .set(
            "one",
            &schema,
            SettingsScope::Global,
            json!({"name":"global"}),
        )
        .unwrap();
    store
        .patch(
            "one",
            &schema,
            SettingsScope::Workspace,
            "name",
            json!("workspace"),
        )
        .unwrap();
    store
        .patch("one", &schema, SettingsScope::Global, "count", json!(3))
        .unwrap();
    assert_eq!(
        store.load("one", &schema).unwrap(),
        json!({"name":"workspace","count":3})
    );
    store.reset("one", SettingsScope::Workspace).unwrap();
    assert_eq!(
        store.load("one", &schema).unwrap(),
        json!({"name":"global","count":3})
    );
}

#[test]
fn raw_layers_do_not_import_defaults_or_other_scopes() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    fs::create_dir(&project).unwrap();
    let layout = Layout::with_user_dir(&project, temp.path().join("private")).unwrap();
    let store = SettingsStore::new(&layout);
    let schema = json!({"type":"object","properties":{"count":{"type":"integer","default":1},"name":{"type":"string"}},"required":["name"],"additionalProperties":false});
    assert_eq!(
        store.layer("one", SettingsScope::Global).unwrap(),
        json!({})
    );
    store
        .set(
            "one",
            &schema,
            SettingsScope::Global,
            json!({"name":"global"}),
        )
        .unwrap();
    store
        .set(
            "one",
            &schema,
            SettingsScope::Workspace,
            json!({"name":"workspace","count":4}),
        )
        .unwrap();
    assert_eq!(
        store.layer("one", SettingsScope::Global).unwrap(),
        json!({"name":"global"})
    );
    assert_eq!(
        store.layer("one", SettingsScope::Workspace).unwrap(),
        json!({"name":"workspace","count":4})
    );
    assert!(store.layer("../escape", SettingsScope::Global).is_err());
    write(&store.path("one", SettingsScope::Global).unwrap(), b"[]").unwrap();
    assert!(store.layer("one", SettingsScope::Global).is_err());
}

#[test]
fn indirect_secret_schemas_fail_closed_at_registration_and_storage_boundaries() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    fs::create_dir(&project).unwrap();
    let layout = Layout::with_user_dir(&project, temp.path().join("private")).unwrap();
    let store = SettingsStore::new(&layout);
    let schemas = [
        json!({"type":"object","properties":{"token":{"$ref":"#/$defs/token"}},"$defs":{"token":{"type":"string","writeOnly":true}}}),
        json!({"type":"object","properties":{"token":{"allOf":[{"type":"string","writeOnly":true}]}}}),
        json!({"type":"object","patternProperties":{"^token$":{"type":"string","format":"password"}}}),
        json!({"type":"object","additionalProperties":{"type":"string","writeOnly":true}}),
        json!({"type":"object","properties":{"token":{"type":"array","prefixItems":[{"type":"string","writeOnly":true}]}}}),
        json!({"type":"object","writeOnly":true}),
    ];
    for schema in schemas {
        // These are valid general JSON schemas, but cannot safely classify public settings.
        check_schema(&schema).unwrap();
        let contributions = Contributions {
            settings: schema.clone(),
            ..Default::default()
        };
        assert!(validate(&contributions).is_err(), "{schema}");
        assert!(store.register_schema("one", &schema).is_err());
        assert!(validate_values(&schema, &json!({"token":"private"})).is_err());
        assert!(
            store
                .set(
                    "one",
                    &schema,
                    SettingsScope::Global,
                    json!({"token":"private"})
                )
                .is_err()
        );
        assert!(!store.path("one", SettingsScope::Global).unwrap().exists());
        // Previously persisted schemas/documents must also fail closed before UI or export.
        write(
            &store.global.join("schemas/one.json"),
            &serde_json::to_vec(&schema).unwrap(),
        )
        .unwrap();
        write(
            &store.path("one", SettingsScope::Global).unwrap(),
            br#"{"token":"private"}"#,
        )
        .unwrap();
        assert!(store.schema("one").is_err());
        assert!(store.load("one", &schema).is_err());
        assert!(store.export("one", &schema).is_err());
        assert!(
            store
                .patch(
                    "one",
                    &schema,
                    SettingsScope::Global,
                    "token",
                    json!("private")
                )
                .is_err()
        );
        store.reset("one", SettingsScope::Global).unwrap();
    }
}

#[test]
fn literal_nested_settings_remain_usable_without_exposing_secret_values() {
    let schema = json!({"type":"object","properties":{
        "nested":{"type":"object","properties":{"count":{"type":"integer"},"token":{"type":"string","writeOnly":true}}},
        "rows":{"type":"array","items":{"type":"object","properties":{"name":{"type":"string"},"token":{"type":"string","format":"password"}}}},
        "tokens":{"type":"array","items":{"type":"string","writeOnly":true}}
    }});
    check_settings_schema(&schema).unwrap();
    validate_values(
        &schema,
        &json!({"nested":{"count":2},"rows":[{"name":"public"}],"tokens":[]}),
    )
    .unwrap();
    for value in [
        json!({"nested":{"token":"private"}}),
        json!({"rows":[{"token":"private"}]}),
        json!({"tokens":["private"]}),
    ] {
        assert!(validate_values(&schema, &value).is_err());
    }
    let mut defaulted = schema.clone();
    defaulted["properties"]["nested"]["default"] = json!({"token":"private"});
    assert!(validate_values(&defaulted, &defaults(&defaulted)).is_err());
    // The stronger admission is settings-specific, not a general-schema restriction.
    let public_ref = json!({"type":"object","properties":{"count":{"$ref":"#/$defs/count"}},"$defs":{"count":{"type":"integer"}}});
    validate_values(&public_ref, &json!({"count":3})).unwrap();
}

use super::*;

#[test]
fn home_workspace_protects_training_data_without_requiring_external_storage() {
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(home.path(), home.path().join(".dial")).unwrap();
    let mut config = Config::load(&layout, None).unwrap();
    config.training.train_data = layout.state_dir.join("training/train.jsonl");
    config.validate_protected().unwrap();
    config.training.train_data = layout.project_root.join("editable-train.jsonl");
    assert!(config.validate_protected().is_err());
}

#[test]
fn operator_harness_pins_override_stored_harness_only_when_they_differ_from_defaults() {
    let mut config = Config::default();
    let stored = Harness {
        tools: vec!["read".into()],
        temperature: 0.9,
        ..Harness::default()
    };
    assert_eq!(config.effective_harness(&stored).unwrap(), stored);
    config.harness.tools = vec!["bash".into()];
    let pinned = config.effective_harness(&stored).unwrap();
    assert_eq!(pinned.tools, vec!["bash".to_string()]);
    assert_eq!(pinned.temperature, 0.9);
}

#[test]
fn explicit_temperature_pin_is_recorded_from_layers_and_never_serialized() {
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(home.path(), home.path().join(".dial")).unwrap();
    let stored = Harness {
        tools: vec!["read".into()],
        temperature: 0.9,
        ..Harness::default()
    };
    let cases = [
        ("", false, 0.9),
        ("[harness]\ntemperature = 0.2\n", true, 0.2),
        ("[harness]\ntools = [\"bash\"]\n", false, 0.9),
    ];
    for (text, pinned, expected) in cases {
        let project: toml::Table = toml::from_str(text).unwrap();
        let config =
            Config::from_layers(&layout, &toml::Table::new(), &project, &toml::Table::new())
                .unwrap();
        assert_eq!(config.harness_temperature_pinned, pinned, "{text:?}");
        assert_eq!(
            config.clone().harness_temperature_pinned,
            pinned,
            "{text:?}"
        );
        assert_eq!(
            config.effective_harness(&stored).unwrap().temperature,
            expected,
            "{text:?}"
        );
        let table = toml::Table::try_from(&config).unwrap();
        assert!(
            !table.contains_key("harness_temperature_pinned"),
            "{text:?}"
        );
    }
}

/// A project-file `[edit] mode` is recorded as explicit; on a local route the differing local
/// default replaces it and is reported once, while `inherit`, a matching mode or an unset mode
/// report nothing. Never serialized.
#[test]
fn local_edit_mode_replacing_an_explicit_edit_mode_is_reported() {
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(home.path(), home.path().join(".dial")).unwrap();
    let local = "model = 'test'\n[endpoint]\nbase_url = 'http://127.0.0.1:9/v1'\n";
    let resolve = |extra: &str| {
        let project: toml::Table = toml::from_str(&format!("{local}{extra}")).unwrap();
        Config::from_layers(&layout, &toml::Table::new(), &project, &toml::Table::new()).unwrap()
    };
    let harness = Harness::default();
    let explicit = resolve("[edit]\nmode = 'hashline'\n");
    assert!(explicit.local_profile_applies());
    assert!(explicit.edit_mode_explicit);
    assert_eq!(
        explicit.local_edit_mode_replacing_configured(&harness),
        Some((types::EditMode::Replace, types::EditMode::Hashline))
    );
    assert_eq!(
        explicit.resolved_edit_mode(&harness),
        types::EditMode::Replace,
        "the local default still wins"
    );
    assert!(
        !toml::Table::try_from(&explicit)
            .unwrap()
            .contains_key("edit_mode_explicit")
    );
    for extra in [
        "",
        "[edit]\nmode = 'replace'\n",
        "[edit]\nmode = 'hashline'\n[assistance.local]\nedit_mode = 'inherit'\n",
    ] {
        assert_eq!(
            resolve(extra).local_edit_mode_replacing_configured(&harness),
            None,
            "{extra:?}"
        );
    }
}

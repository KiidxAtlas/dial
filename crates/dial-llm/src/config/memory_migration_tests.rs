use super::*;

#[test]
fn harness_migration_is_idempotent_and_keeps_unrelated_duplicates_for_validation() {
    let mut harness = Harness {
        tools: vec![
            "read".into(),
            "recall".into(),
            "retain".into(),
            "read".into(),
            "memory_update".into(),
            "learn".into(),
        ],
        ..Default::default()
    };
    assert!(types::migrate_memory_tools(&mut harness));
    assert_eq!(harness.tools, ["read", "memory", "read"]);
    let pointer = harness.tools.as_ptr();
    assert!(!types::migrate_memory_tools(&mut harness));
    assert_eq!(harness.tools.as_ptr(), pointer);
    let mut present = vec!["memory".to_owned(), "forget".to_owned()];
    assert!(types::migrate_memory_tool_names(&mut present));
    assert_eq!(present, ["memory"]);
    // artifact_read merged into the built-in read; a saved selection drops it.
    let mut merged = vec!["artifact_read".to_owned(), "grep".to_owned()];
    assert!(types::migrate_memory_tool_names(&mut merged));
    assert_eq!(merged, ["grep"]);
    assert!(!types::migrate_memory_tool_names(&mut merged));
}

#[test]
fn toml_migration_changes_harness_and_profiles_not_other_values_or_invalid_arrays() {
    let mut table: toml::Table = toml::from_str("[harness]\ntools=['recall','read','retain']\n[model_profiles.deep]\ntools=['reflect']\n[model_profiles.invalid]\ntools=['recall',17]\n[tools.operator]\nprogram='recall'\n").unwrap();
    assert!(migrate_memory_tool_table(&mut table));
    assert_eq!(
        table["tools"]["operator"]["program"].as_str(),
        Some("recall")
    );
    assert_eq!(
        table["model_profiles"]["invalid"]["tools"][0].as_str(),
        Some("recall")
    );
    assert_eq!(
        table["harness"]["tools"],
        toml::Value::Array(vec!["memory".into(), "read".into()])
    );
    assert_eq!(
        table["model_profiles"]["deep"]["tools"],
        toml::Value::Array(vec!["memory".into()])
    );
    assert!(!migrate_memory_tool_table(&mut table));
}

#[test]
fn loading_persists_validated_memory_selector_cutover() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    let path = layout.config_files()[0].clone();
    fs::write(
        &path,
        "use_case='preserved'\n[harness]\ntools=['recall','read','retain']\n",
    )
    .unwrap();
    let config = Config::load(&layout, None).unwrap();
    assert_eq!(config.harness.tools, ["memory", "read"]);
    let saved: toml::Table = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(saved["use_case"].as_str(), Some("preserved"));
    assert_eq!(
        saved["harness"]["tools"],
        toml::Value::Array(vec!["memory".into(), "read".into()])
    );
    assert_eq!(
        Config::load(&layout, None).unwrap().harness.tools,
        config.harness.tools
    );
}

#[test]
fn zero_harness_steps_means_unlimited() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    let path = layout.config_files()[0].clone();
    fs::write(&path, "[harness]\nmax_steps=0\n").unwrap();

    let config = Config::load(&layout, None).unwrap();
    assert_eq!(config.harness.max_steps, 0);
}

#[test]
fn invalid_configuration_is_not_rewritten_during_migration() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    let path = layout.config_files()[0].clone();
    let original = "[harness]\ntools=['recall']\nmax_steps=65\n";
    fs::write(&path, original).unwrap();
    assert!(Config::load(&layout, None).is_err());
    assert_eq!(fs::read_to_string(path).unwrap(), original);
}

#[test]
fn zero_step_limit_saves_and_reloads_as_unlimited() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    let saved = crate::config::settings_store::save_config(
        &layout,
        crate::config::settings_store::Scope::Global,
        &[crate::config::settings_store::Change::Set {
            path: "harness.max_steps".into(),
            value: 0.into(),
        }],
    )
    .unwrap();
    assert_eq!(saved.harness.max_steps, 0);
    assert_eq!(Config::load(&layout, None).unwrap().harness.max_steps, 0);
}
#[test]
fn stale_selected_profile_is_ignored_without_deleting_its_definition() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    let config_path = layout.config_files()[0].clone();
    fs::write(
        &config_path,
        r#"
provider = "mlx"
model = "current-model"
model_profile = "old"
[model_profiles.old]
provider = "mlx"
model = "previous-model"
max_tokens = 4096
"#,
    )
    .unwrap();
    let config = Config::load(&layout, None).unwrap();
    assert_eq!(
        config.model_profile, None,
        "stale profile selector is ignored"
    );
    assert!(
        config.model_profiles.contains_key("old"),
        "profile remains available when switching back"
    );
    assert_eq!(Config::load(&layout, None).unwrap().model_profile, None);
}

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

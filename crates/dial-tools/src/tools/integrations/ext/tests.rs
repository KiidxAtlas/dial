use super::*;

#[test]
fn notebook_is_registered_only_when_listed_as_integration() {
    let mut config = Config {
        integrations: vec!["notebook".into()],
        ..Config::default()
    };
    assert!(Registry::standard(&config, Vec::new()).contains("notebook"));

    config.integrations.clear();
    assert!(!Registry::standard(&config, Vec::new()).contains("notebook"));
}
#[test]
fn legacy_integration_lists_keep_always_registered_tools() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config {
        project_root: dir.path().to_path_buf(),
        user_dir: dir.path().join("user"),
        state_dir: dir.path().join("state"),
        integrations: Vec::new(),
        ..Config::default()
    };
    config.extensions.enabled = false;
    // `image_search` is always registered, but only while its model role is assigned.
    config.model_roles.insert(
        crate::tools::vision::embedding::ROLE.into(),
        crate::llm::model_config::ModelSelection::new("mlx", "test/embedder"),
    );
    let registry = Registry::standard(&config, Vec::new());
    for entry in crate::config::tools::integrations::INTEGRATIONS {
        if entry.always_registered {
            assert!(registry.contains(entry.name), "{}", entry.name);
        }
    }
    assert!(!registry.contains("computer"));
    assert!(!registry.contains("notebook"));
}

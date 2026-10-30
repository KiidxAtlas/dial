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

#[test]
fn video_registry_requires_opt_in_and_integration_but_investigation_tools_are_discoverable() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config {
        project_root: dir.path().to_path_buf(),
        user_dir: dir.path().join("user"),
        state_dir: dir.path().join("state"),
        integrations: vec!["video".into()],
        ..Config::default()
    };
    config.extensions.enabled = false;
    let registry = Registry::standard(&config, Vec::new());
    assert!(!registry.contains("video"));
    for name in ["document", "audio", "data", "investigate", "workbench"] {
        assert!(registry.contains(name), "{name}");
    }
    config.experimental.video_analysis = true;
    assert!(Registry::standard(&config, Vec::new()).contains("video"));
    config.integrations.clear();
    assert!(!Registry::standard(&config, Vec::new()).contains("video"));
}

#[test]
fn system_investigation_tools_are_read_only_discoverable_and_have_valid_schemas() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config {
        project_root: dir.path().to_path_buf(),
        user_dir: dir.path().join("user"),
        state_dir: dir.path().join("state"),
        integrations: Vec::new(),
        ..Config::default()
    };
    config.extensions.enabled = false;
    let registry = Registry::standard(&config, Vec::new());
    for name in ["runtime_inspect", "network_inspect", "database"] {
        let tool = registry
            .get(name)
            .expect("system investigation tool registered");
        assert!(tool.read_only_hint(), "{name}");
        assert!(!tool.needs_approval(&serde_json::json!({})), "{name}");
        assert!(!tool.may_modify_workspace(&serde_json::json!({})), "{name}");
        let schema = tool.schema();
        assert_eq!(
            crate::tools::surface::catalog::validate_definition(&schema).unwrap(),
            name
        );
        assert!(
            crate::tools::surface::catalog::validate_arguments(
                &schema,
                &serde_json::json!({"action":"invented", "path":"fixture"})
            )
            .is_err(),
            "{name} accepted invalid action"
        );
    }
}

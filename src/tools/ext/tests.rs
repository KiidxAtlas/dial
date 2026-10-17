use super::*;

#[test]
fn notebook_is_registered_only_when_listed_as_integration() {
    let mut config = Config {
        integrations: vec!["notebook".into()],
        ..Config::default()
    };
    assert!(Registry::standard(&config).contains("notebook"));

    config.integrations.clear();
    assert!(!Registry::standard(&config).contains("notebook"));
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
    let registry = Registry::standard(&config);
    for entry in super::super::catalog::INTEGRATIONS {
        if entry.always_registered {
            assert!(registry.contains(entry.name), "{}", entry.name);
        }
    }
    assert!(!registry.contains("computer"));
    assert!(!registry.contains("notebook"));
}

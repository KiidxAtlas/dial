use super::*;

#[test]
fn startup_preparation_is_inert_by_default_and_round_trips() {
    let config = PreparationConfig::default();
    assert!(!config.local_prefix && !config.cargo_check && config.lsp_paths.is_empty());
    config.validate().unwrap();
    let mut configured = config;
    configured.lsp_paths = vec!["src/main.rs".into()];
    assert_eq!(
        toml::from_str::<PreparationConfig>(&toml::to_string(&configured).unwrap()).unwrap(),
        configured
    );
    assert!(toml::from_str::<PreparationConfig>("unknown = true").is_err());
}

#[test]
fn invalid_preparation_scopes_and_deadlines_are_rejected() {
    for path in [
        "",
        "../outside.rs",
        "/tmp/outside.rs",
        "https://host/source.rs",
        "src/\nfile.rs",
    ] {
        let config = PreparationConfig {
            lsp_paths: vec![path.into()],
            ..Default::default()
        };
        assert!(config.validate().is_err(), "path {path:?}");
    }
    for timeout_secs in [0, 301] {
        assert!(
            PreparationConfig {
                timeout_secs,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }
    assert!(
        PreparationConfig {
            lsp_paths: vec!["src/main.rs".into(); 9],
            ..Default::default()
        }
        .validate()
        .is_err()
    );
}

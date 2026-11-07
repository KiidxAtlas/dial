use super::*;

#[test]
fn progress_file_is_off_by_default_and_parses_as_a_relative_path() {
    assert_eq!(GoalConfig::default().progress_file, "");
    let config: GoalConfig = toml::from_str("progress_file = \".dial/progress.md\"").unwrap();
    assert_eq!(config.progress_file, ".dial/progress.md");
    config.validate().unwrap();
    GoalConfig::default().validate().unwrap();
    assert!(toml::from_str::<GoalConfig>("progress = \"x\"").is_err());
}

#[test]
fn progress_file_outside_the_workspace_is_refused() {
    for path in [
        "../progress.md",
        "/tmp/progress.md",
        ".",
        "notes/../../x.md",
        "a\nb.md",
    ] {
        let config = GoalConfig {
            progress_file: path.into(),
        };
        assert!(config.validate().is_err(), "{path:?}");
    }
    for path in ["PROGRESS.md", "./docs/progress.md", ".dial/progress.md"] {
        let config = GoalConfig {
            progress_file: path.into(),
        };
        config.validate().unwrap();
    }
}

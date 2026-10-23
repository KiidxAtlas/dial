use super::*;
use dial::config::types::McpServerConfig;

#[test]
fn core_default_and_exact_mcp_allowlist_survive_both_isolation_points() {
    let mut config = Config::default();
    config.mcp.insert(
        "fixture".into(),
        McpServerConfig {
            program: PathBuf::from("missing-fixture-service"),
            ..Default::default()
        },
    );
    config.mcp.insert(
        "ambient".into(),
        McpServerConfig {
            program: PathBuf::from("ambient-service"),
            ..Default::default()
        },
    );
    config
        .subagents
        .insert("ambient-agent".into(), "must not leak".into());
    let core = isolate(&config, &Default::default()).unwrap();
    assert!(core.mcp.is_empty());
    assert!(core.subagents.is_empty());
    assert!(core.agents.is_empty());
    assert!(!core.sandbox.allow_network);
    let lane = IntegrationOverrides {
        mode: IntegrationMode::Mcp,
        mcp_servers: vec!["fixture".into()],
    };
    let selected = isolate(&config, &lane).unwrap();
    assert_eq!(selected.mcp.keys().collect::<Vec<_>>(), vec!["fixture"]);
    assert!(selected.sandbox.allow_network);
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let operator = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(root.path(), operator.path().to_owned()).unwrap();
    prepare_integration_home(&layout, &selected, &home, &lane).unwrap();
    let child: Config =
        toml::from_str(&fs::read_to_string(home.path().join("config.toml")).unwrap()).unwrap();
    assert_eq!(child.mcp.keys().collect::<Vec<_>>(), vec!["fixture"]);
    assert!(child.subagents.is_empty());
    assert!(child.agents.is_empty());
    assert!(child.tools.is_empty());
    assert!(
        prerequisite(&selected, &lane, root.path())
            .unwrap_err()
            .is::<AgentUnavailable>()
    );
    assert!(config.mcp.contains_key("ambient"));
}

#[test]
fn missing_disabled_and_install_launcher_mcp_prerequisites_are_unavailable_without_starting() {
    let root = tempfile::tempdir().unwrap();
    let lane = IntegrationOverrides {
        mode: IntegrationMode::Mcp,
        mcp_servers: vec!["fixture".into()],
    };
    let mut config = Config::default();
    assert!(
        prerequisite(&config, &lane, root.path())
            .unwrap_err()
            .is::<AgentUnavailable>()
    );
    config.mcp.insert(
        "fixture".into(),
        McpServerConfig {
            program: PathBuf::from("npx"),
            ..Default::default()
        },
    );
    assert!(
        prerequisite(&config, &lane, root.path())
            .unwrap_err()
            .to_string()
            .contains("install/fetch")
    );
    config.mcp.get_mut("fixture").unwrap().enabled = false;
    assert!(
        prerequisite(&config, &lane, root.path())
            .unwrap_err()
            .to_string()
            .contains("disabled")
    );
}

#[test]
fn delegation_uses_builtin_agents_and_selected_model_not_operator_presets() {
    let mut config = Config {
        model: "same-model".into(),
        ..Config::default()
    };
    config
        .subagents
        .insert("ambient".into(), "not benchmark evidence".into());
    let lane = IntegrationOverrides {
        mode: IntegrationMode::Delegation,
        ..Default::default()
    };
    let selected = isolate(&config, &lane).unwrap();
    assert_eq!(selected.model, "same-model");
    assert!(selected.subagents.is_empty());
    assert!(selected.agents.is_empty());
    assert!(selected.model_roles.is_empty());
    assert!(selected.mcp.is_empty());
    assert!(!selected.integrations.contains(&"browser".into()));
}

#[test]
fn browser_lane_is_headless_cache_only_and_does_not_attach_to_operator_sessions() {
    let mut config = Config::default();
    config.browser.profile = Some("private".into());
    config.browser.allow_attach = true;
    config.browser.cdp_url = Some("http://127.0.0.1:9222".into());
    let lane = IntegrationOverrides {
        mode: IntegrationMode::Browser,
        ..Default::default()
    };
    let selected = isolate(&config, &lane).unwrap();
    let value = serde_json::to_value(&selected.browser).unwrap();
    assert_eq!(value["allow_install"], false);
    assert!(selected.browser.headless);
    assert!(!selected.browser.allow_attach);
    assert!(selected.browser.cdp_url.is_none());
    assert!(selected.browser.profile.is_none());
    assert!(!selected.browser.relay && !selected.browser.cmux);
    assert_eq!(
        selected
            .integrations
            .iter()
            .filter(|name| name.as_str() == "browser")
            .count(),
        1
    );
}

#[test]
fn executable_lookup_uses_injected_path_and_windows_suffixes() {
    use std::ffi::OsStr;

    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("missing");
    let installed = root.path().join("installed");
    fs::create_dir(&installed).unwrap();
    for program in ["node.EXE", "python.COM", "service.CMD", "exact"] {
        fs::write(installed.join(program), "fixture, never launched").unwrap();
    }
    fs::create_dir(installed.join("directory.EXE")).unwrap();
    let path = std::env::join_paths([&missing, &installed]).unwrap();
    let suffixes = Some(OsStr::new(";.EXE;.COM;.CMD;"));
    let lookup =
        |program: &Path, extensions| executable_exists_in(program, Some(&path), extensions);
    for program in ["node", "python", "service", "node.EXE", "exact"] {
        assert!(lookup(Path::new(program), suffixes), "{program}");
    }
    for program in ["absent", "directory", "node.other", "installed/node"] {
        assert!(!lookup(Path::new(program), suffixes), "{program}");
    }
    assert!(!lookup(Path::new("node"), None));
    assert!(lookup(Path::new("exact"), None));
    assert!(!executable_exists_in(Path::new("node"), None, suffixes));
    assert!(executable_exists_in(
        &installed.join("node"),
        None,
        suffixes
    ));
    assert!(executable_exists_in(
        &installed.join("node.EXE"),
        None,
        None
    ));
    assert!(!executable_exists_in(&installed.join("node"), None, None));
}

#[test]
fn executable_lookup_does_not_interpret_pathext_entries_as_paths() {
    use std::ffi::OsStr;

    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("node.evil")).unwrap();
    fs::write(root.path().join("node.evil/outside"), "fixture").unwrap();
    let path = std::env::join_paths([root.path()]).unwrap();
    assert!(!executable_exists_in(
        Path::new("node"),
        Some(&path),
        Some(OsStr::new(".;.evil/outside;.evil\\outside")),
    ));
}

use super::*;
use crate::tools::mcp::catalog::Install;

#[test]
fn snapshot_offers_uninstalled_curated_servers_only() {
    let mut config = Config {
        user_dir: PathBuf::from("/tmp/x"),
        ..Default::default()
    };
    config.mcp.insert("time".into(), McpServerConfig::default());
    let rows = mcp_rows(&config, None);
    assert!(
        rows.iter()
            .any(|r| r.id == "time" && r.origin == Origin::Installed)
    );
    assert!(
        !rows
            .iter()
            .any(|r| r.id == "time" && r.origin == Origin::Curated)
    );
    assert!(
        rows.iter()
            .any(|r| r.id == "memory" && r.origin == Origin::Curated)
    );
}

#[test]
fn servers_from_other_tools_are_offered_until_configured() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join(".claude.json"),
        r#"{"mcpServers":{"codegraph":{"command":"codegraph","args":["serve"]}}}"#,
    )
    .unwrap();
    let mut config = Config {
        user_dir: PathBuf::from("/tmp/x"),
        project_root: home.path().join("project"),
        ..Default::default()
    };
    let rows = mcp_rows(&config, Some(home.path()));
    let row = rows
        .iter()
        .find(|r| r.id == "codegraph")
        .expect("found server is listed");
    assert_eq!(row.origin, Origin::Foreign);
    assert_eq!(row.state, State::Available);
    config
        .mcp
        .insert("codegraph".into(), McpServerConfig::default());
    let rows = mcp_rows(&config, Some(home.path()));
    assert!(
        rows.iter()
            .filter(|r| r.id == "codegraph")
            .all(|r| r.origin == Origin::Installed)
    );
}

/// A home directory holding one other-tool config with the given server entries.
fn home_with(servers: &str) -> (tempfile::TempDir, Config) {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join(".claude.json"),
        format!(r#"{{"mcpServers":{servers}}}"#),
    )
    .unwrap();
    let config = Config {
        user_dir: home.path().join("dial"),
        project_root: home.path().join("project"),
        ..Default::default()
    };
    (home, config)
}

#[test]
fn importing_copies_the_other_tools_entry_into_one_config_change() {
    // A disabled server skips the start-up health check, so no process is spawned.
    let (home, config) =
        home_with(r#"{"notes":{"command":"notes-server","args":["--stdio"],"disabled":true}}"#);
    let out = import_mcp(
        &config,
        Some(home.path()),
        "notes",
        &SharedProgress::default(),
    )
    .unwrap();
    assert!(out.ok, "{}", out.message);
    assert!(out.message.contains("Imported `notes`"), "{}", out.message);
    assert_eq!(out.changes.len(), 1);
}

#[test]
fn importing_refuses_configured_vanished_and_missing_programs_without_saving() {
    let (home, mut config) = home_with(r#"{"ghost":{"command":"dial-no-such-mcp-program-4f1c"}}"#);
    let progress = SharedProgress::default();
    let run =
        |config: &Config, id: &str| import_mcp(config, Some(home.path()), id, &progress).unwrap();
    let missing = run(&config, "ghost");
    assert!(!missing.ok && missing.changes.is_empty());
    assert!(
        missing.message.contains("not on PATH"),
        "{}",
        missing.message
    );
    let vanished = run(&config, "never-defined");
    assert!(!vanished.ok && vanished.changes.is_empty());
    assert!(
        vanished.message.contains("no longer defined"),
        "{}",
        vanished.message
    );
    config
        .mcp
        .insert("ghost".into(), McpServerConfig::default());
    let configured = run(&config, "ghost");
    assert!(!configured.ok && configured.changes.is_empty());
    assert!(
        configured.message.contains("already configured"),
        "{}",
        configured.message
    );
}

fn listed(name: &str, source: Source) -> Candidate {
    Candidate {
        name: name.into(),
        title: name.into(),
        description: "a listed server".into(),
        sources: vec![source],
        badges: Vec::new(),
        install: Install::Blocked("test".into()),
        inputs: Vec::new(),
        repository: None,
    }
}

#[test]
fn a_server_listed_twice_is_one_row_tagged_with_both_listings() {
    let found = catalog::merge(vec![
        listed("io.github.acme/notes", Source::Official),
        listed("@acme/notes", Source::Smithery),
    ]);
    let rows: Vec<Row> = found.iter().map(candidate_row).collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "io.github.acme/notes");
    assert!(rows[0].tags.iter().any(|t| t == "official"));
    assert!(rows[0].tags.iter().any(|t| t == "smithery"));
    assert_eq!(rows[0].origin, Origin::Remote);
}

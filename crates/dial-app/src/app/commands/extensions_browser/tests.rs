use super::*;
use crate::tools::integrations::mcp::catalog::Install;

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

fn input(key: &str, required: bool, secret: bool) -> catalog::Input {
    catalog::Input {
        key: key.into(),
        description: "declared setting".into(),
        required,
        secret,
    }
}

#[test]
fn credential_requirements_are_distinct_from_non_secret_configuration() {
    let mut candidate = listed("configured", Source::Official);
    candidate.sources.push(Source::Smithery);
    candidate.badges.push("verified".into());
    candidate.inputs = vec![
        input("API_TOKEN", true, true),
        input("BACKUP_TOKEN", false, true),
        input("WORKSPACE_ID", true, false),
    ];
    let row = candidate_row(&candidate);
    assert_eq!(row.tags[0], "official");
    assert_eq!(row.tags[1], "API key/token required");
    assert!(row.tags.iter().any(|tag| tag == "API key/token optional"));
    assert!(row.tags.iter().any(|tag| tag == "configuration required"));
    assert!(row.tags.iter().any(|tag| tag == "smithery"));
    assert!(!row.tags.iter().any(|tag| tag == "Smithery key required"));
    assert!(
        row.details
            .iter()
            .any(|line| line.contains("required credential API_TOKEN"))
    );
    assert!(
        row.details
            .iter()
            .any(|line| line.contains("required configuration WORKSPACE_ID"))
    );

    candidate.inputs = vec![input("WORKSPACE_ID", true, false)];
    let row = candidate_row(&candidate);
    assert!(row.tags.iter().any(|tag| tag == "configuration required"));
    assert!(row.tags.iter().any(|tag| tag == "auth unknown"));
    assert!(!row.tags.iter().any(|tag| tag == "API key/token required"));
}

#[test]
fn optional_credentials_do_not_claim_complete_authentication_metadata() {
    let mut candidate = listed("optional", Source::Official);
    candidate.inputs = vec![input("API_KEY", false, true)];
    let row = candidate_row(&candidate);
    assert_eq!(row.tags[1], "API key/token optional");
    assert!(row.tags.iter().any(|tag| tag == "auth unknown"));
    assert!(
        row.details
            .iter()
            .any(|line| line.contains("unknown beyond the declared inputs"))
    );
}

#[test]
fn missing_deferred_blocked_and_remote_metadata_never_implies_no_key() {
    for source in [Source::Official, Source::Npm, Source::Github] {
        let mut candidate = listed("unknown", source);
        for install in [
            Install::Blocked("remote requires unsupported headers".into()),
            Install::Deferred("package manifest".into()),
            Install::Server(Box::new(McpServerConfig::default())),
            Install::Server(Box::new(McpServerConfig {
                url: Some("https://example.com/mcp".into()),
                ..Default::default()
            })),
        ] {
            candidate.install = install;
            let row = candidate_row(&candidate);
            assert_eq!(row.tags[0], source.label());
            assert_eq!(row.tags[1], "auth unknown");
            assert!(!row.tags.iter().any(|tag| tag.contains("no key")));
            assert!(
                row.details
                    .iter()
                    .any(|line| line.contains("no API-key-free guarantee"))
            );
        }
    }
}

#[test]
fn smithery_platform_key_is_separate_from_unknown_server_credentials() {
    let row = candidate_row(&listed("smithery-server", Source::Smithery));
    assert_eq!(
        &row.tags[..3],
        &["smithery", "Smithery key required", "server auth unknown"]
    );
    assert!(
        row.details
            .iter()
            .any(|line| line.contains("separate from the server's own credentials"))
    );
    assert!(
        row.details
            .iter()
            .any(|line| line.contains("unknown until Smithery"))
    );
}

#[test]
fn initial_and_searched_curated_rows_share_credential_labels() {
    let config = Config::default();
    let rows = mcp_rows(&config, None);
    for item in CURATED {
        let initial = rows.iter().find(|row| row.id == item.id).unwrap();
        let searched = candidate_row(&catalog::curated_candidate(
            item,
            &config.user_dir,
            &config.project_root,
        ));
        assert_eq!(initial.tags, searched.tags);
    }
    let github = rows.iter().find(|row| row.id == "github").unwrap();
    assert_eq!(github.tags[1], "API key/token required");
    let notion = rows.iter().find(|row| row.id == "notion").unwrap();
    assert_eq!(notion.tags[1], "API key/token required");
    assert!(is_secret_key("OPENAPI_MCP_HEADERS"));
    let memory = rows.iter().find(|row| row.id == "memory").unwrap();
    assert_eq!(memory.tags[1], "no key declared");
}

#[test]
fn credential_labels_do_not_echo_configured_secret_values() {
    let mut candidate = listed("secret", Source::Official);
    candidate.inputs = vec![input("API_KEY", true, true)];
    candidate.install = Install::Server(Box::new(McpServerConfig {
        env: BTreeMap::from([("API_KEY".into(), "never-show-this-value".into())]),
        ..Default::default()
    }));
    let row = candidate_row(&candidate);
    assert!(!format!("{:?} {:?}", row.tags, row.details).contains("never-show-this-value"));
}

#[test]
fn server_display_hides_all_arguments_and_url_credentials_without_mutation() {
    let mut server = McpServerConfig {
        program: "/private/location/notes-server".into(),
        args: vec!["--token=secret-argument".into(), "secret-positional".into()],
        ..Default::default()
    };
    assert_eq!(server_command(&server), "notes-server (2 arguments hidden)");
    assert_eq!(server.args[1], "secret-positional");
    server.url = Some(
        "https://user:password@example.test/secret-path?key=secret-query#secret-fragment".into(),
    );
    assert_eq!(server_summary(&server), "remote MCP endpoint (URL hidden)");
    assert_eq!(server_command(&server), server_summary(&server));
    assert!(server.url.as_ref().unwrap().contains("secret-query"));
}

fn catalog_intent() -> Intent {
    Intent::CatalogBrowse {
        tab: Tab::Mcp,
        query: "notes".into(),
        page: 3,
    }
}

fn assert_catalog_failure(progress: &SharedProgress, expected: &str) {
    let shown = progress.lock().unwrap();
    assert!(matches!(&shown.result, Some((false, message)) if message.contains(expected)));
    let update = shown
        .catalog
        .as_ref()
        .expect("catalog request must leave loading state");
    assert_eq!(update.tab, Tab::Mcp);
    assert_eq!(update.query, "notes");
    assert_eq!(update.page, 3);
    assert!(update.error.as_ref().unwrap().contains(expected));
}

#[test]
fn rejected_queued_catalog_request_publishes_its_identity_and_error() {
    let mut pending = None;
    let first = queue(
        &mut pending,
        Intent::TestMcp {
            id: "existing".into(),
        },
        Scope::Global,
    );
    let rejected = queue(&mut pending, catalog_intent(), Scope::Global);
    assert_catalog_failure(&rejected, "still starting");
    assert!(first.lock().unwrap().result.is_none());
    assert!(matches!(pending.unwrap().intent, Intent::TestMcp { .. }));
}

#[test]
fn catalog_start_reload_and_disconnect_failures_publish_errors() {
    let (_home, _project, layout, config) = crate::app::tests::config();
    let app = App::new(
        layout.clone(),
        config,
        toml::Table::new(),
        Default::default(),
        vec![],
        Default::default(),
    )
    .unwrap();
    let (tx, rx) = channel();
    let running_progress = SharedProgress::default();
    let mut job = Some(InstallJob {
        outcome: rx,
        progress: running_progress.clone(),
        scope: Scope::Global,
        intent: catalog_intent(),
    });
    let rejected = SharedProgress::default();
    start(
        &mut job,
        Pending {
            intent: catalog_intent(),
            progress: rejected.clone(),
            scope: Scope::Global,
        },
        &app,
    );
    assert_catalog_failure(&rejected, "still running");
    drop(tx);
    let finished = step(&mut None, &mut job, &app).unwrap();
    assert!(!finished.ok);
    assert_catalog_failure(&running_progress, "stopped unexpectedly");

    std::fs::create_dir_all(layout.config_files()[0].parent().unwrap()).unwrap();
    crate::config::settings_store::write_private(&layout.config_files()[0], b"broken TOML [")
        .unwrap();
    let reload_progress = SharedProgress::default();
    start(
        &mut job,
        Pending {
            intent: catalog_intent(),
            progress: reload_progress.clone(),
            scope: Scope::Global,
        },
        &app,
    );
    assert!(job.is_none());
    assert_catalog_failure(&reload_progress, "could not read settings");
}

#[test]
fn import_changes_use_insert_preconditions_and_keep_original_execution_values() {
    let server = McpServerConfig {
        args: vec!["secret-positional".into()],
        ..Default::default()
    };
    let change = smithery::config_change("notes", &server).unwrap();
    let Change::Insert { path, value } = change else {
        panic!("install must not overwrite")
    };
    assert_eq!(path, "mcp.notes");
    assert_eq!(value["args"][0].as_str(), Some("secret-positional"));
}

#[test]
fn existing_mcp_changes_follow_owning_scope_not_install_destination() {
    let project = tempfile::tempdir().unwrap();
    let user = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), user.path().to_path_buf()).unwrap();
    let files = layout.config_files();
    std::fs::create_dir_all(files[1].parent().unwrap()).unwrap();
    std::fs::write(&files[0], "[mcp.global]\nprogram = 'global-server'\n").unwrap();
    std::fs::write(&files[1], "[mcp.project]\nprogram = 'project-server'\n").unwrap();
    for intent in [
        Intent::RemoveMcp {
            id: "global".into(),
        },
        Intent::ToggleMcp {
            id: "global".into(),
            enable: false,
        },
    ] {
        assert_eq!(
            intent_scope(&layout, &intent, Scope::Project).unwrap(),
            Scope::Global
        );
    }
    for intent in [
        Intent::RemoveMcp {
            id: "project".into(),
        },
        Intent::ToggleMcp {
            id: "project".into(),
            enable: false,
        },
    ] {
        assert_eq!(
            intent_scope(&layout, &intent, Scope::Global).unwrap(),
            Scope::Project
        );
    }
    let install = Intent::InstallCuratedMcp { id: "new".into() };
    assert_eq!(
        intent_scope(&layout, &install, Scope::Global).unwrap(),
        Scope::Global
    );
    assert_eq!(
        intent_scope(&layout, &install, Scope::Project).unwrap(),
        Scope::Project
    );
}

#[test]
fn browser_mcp_changes_persist_to_owner_and_update_effective_configuration() {
    use crate::config::settings_store::{load_config, save_config};

    for selected in [Scope::Global, Scope::Project] {
        for (id, owner) in [("global", Scope::Global), ("project", Scope::Project)] {
            let project = tempfile::tempdir().unwrap();
            let user = tempfile::tempdir().unwrap();
            let layout = Layout::with_user_dir(project.path(), user.path().to_path_buf()).unwrap();
            let files = layout.config_files();
            std::fs::create_dir_all(files[1].parent().unwrap()).unwrap();
            std::fs::write(&files[0], "[mcp.global]\nprogram = 'global-server'\n").unwrap();
            std::fs::write(&files[1], "[mcp.project]\nprogram = 'project-server'\n").unwrap();
            let untouched = if owner == Scope::Global { 1 } else { 0 };
            let original = std::fs::read(&files[untouched]).unwrap();
            for intent in [
                Intent::ToggleMcp {
                    id: id.into(),
                    enable: false,
                },
                Intent::ToggleMcp {
                    id: id.into(),
                    enable: true,
                },
                Intent::RemoveMcp { id: id.into() },
            ] {
                let config = load_config(&layout, &toml::Table::new()).unwrap();
                let scope = intent_scope(&layout, &intent, selected).unwrap();
                let progress = SharedProgress::default();
                let outcome = run(&layout, &config, &intent, &progress).unwrap();
                assert!(outcome.ok);
                finish(
                    Finished {
                        ok: outcome.ok,
                        message: outcome.message,
                        changes: outcome.changes,
                        scope,
                        progress,
                    },
                    |changes, scope| save_config(&layout, scope, changes).map(|_| ()),
                );
                let effective = load_config(&layout, &toml::Table::new()).unwrap();
                match intent {
                    Intent::ToggleMcp { enable, .. } => {
                        assert_eq!(effective.mcp[id].enabled, enable);
                        assert_eq!(effective.mcp[id].program, config.mcp[id].program);
                    }
                    Intent::RemoveMcp { .. } => assert!(!effective.mcp.contains_key(id)),
                    _ => unreachable!(),
                }
                assert_eq!(std::fs::read(&files[untouched]).unwrap(), original);
                assert_eq!(
                    effective.mcp.len(),
                    if matches!(intent, Intent::RemoveMcp { .. }) {
                        1
                    } else {
                        2
                    }
                );
            }
        }
    }
}

use super::*;

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

#[test]
fn reads_every_supported_format_and_the_first_definition_wins() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        project.path(),
        ".mcp.json",
        r#"{"mcpServers":{"Code.Graph":{"command":"codegraph","args":["serve"],"env":{"K":"v"}}}}"#,
    );
    write(
        home.path(),
        ".claude.json",
        &format!(
            r#"{{"mcpServers":{{"remote":{{"type":"sse","url":"https://example.com/sse","headers":{{"X":"1"}}}}}},
                "projects":{{"{}":{{"mcpServers":{{"scoped":{{"command":"scoped"}}}}}}}}}}"#,
            project.path().display()
        ),
    );
    write(
        home.path(),
        ".codex/config.toml",
        r#"
[mcp_servers.codegraph]
command = "other"
[mcp_servers.node]
command = "node"
args = ["a"]
startup_timeout_sec = 90
enabled = false
[mcp_servers.web]
url = "https://example.com/mcp"
bearer_token_env_var = "TOKEN"
"#,
    );
    write(home.path(), ".cursor/mcp.json", "{ not json");

    let found = discover(Some(home.path()), project.path());
    let by = |name: &str| found.iter().find(|f| f.name == name).expect(name);

    let graph = by("code-graph");
    assert_eq!(graph.provider, "Claude");
    assert_eq!(graph.server.program, PathBuf::from("codegraph"));
    assert_eq!(graph.server.env["K"], "v");
    // `codegraph` (Codex) is a different name from `Code.Graph` (Claude); both stay.
    assert_eq!(by("codegraph").provider, "Codex");
    let remote = by("remote");
    assert_eq!(remote.server.transport, Some(McpTransport::Sse));
    assert_eq!(remote.server.headers["X"], "1");
    assert_eq!(by("scoped").server.program, PathBuf::from("scoped"));
    let node = by("node");
    assert!(!node.server.enabled);
    assert_eq!(node.server.connect_timeout_secs, 90);
    assert_eq!(by("web").server.bearer_env.as_deref(), Some("TOKEN"));
    assert_eq!(found.len(), 6);
}

#[test]
fn duplicate_names_record_the_other_providers() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        home.path(),
        ".claude.json",
        r#"{"mcpServers":{"codegraph":{"command":"codegraph"}}}"#,
    );
    write(
        home.path(),
        ".codex/config.toml",
        "[mcp_servers.codegraph]\ncommand = \"codegraph\"\n",
    );
    let found = discover(Some(home.path()), project.path());
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].provider, "Claude");
    assert_eq!(found[0].also_in, ["Codex"]);
}

#[test]
fn entries_without_a_command_or_url_and_missing_files_are_skipped() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        project.path(),
        ".mcp.json",
        r#"{"mcpServers":{"empty":{},"bad":5}}"#,
    );
    assert!(discover(Some(home.path()), project.path()).is_empty());
    assert!(discover(None, project.path()).is_empty());
}

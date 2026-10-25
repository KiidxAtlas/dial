use super::*;

fn inventory() -> Inventory {
    Inventory::new(&Config::default(), &McpLoad::default())
}

#[test]
fn only_install_needs_approval() {
    let tool = inventory();
    assert!(!tool.needs_approval(&json!({"action": "list"})));
    assert!(!tool.needs_approval(&json!({"action": "search", "query": "git"})));
    assert!(!tool.needs_approval(&json!({})));
    assert!(tool.needs_approval(&json!({"action": "install", "id": "memory"})));
    assert!(!tool.may_modify_workspace(&json!({"action": "install"})));
}

#[test]
fn install_refuses_an_empty_id_and_unknown_ids_without_starting_anything() {
    let config = Config::default();
    let empty = install(&config, "  ");
    assert!(empty.is_error);
    assert!(empty.text.contains("needs `id`"), "{}", empty.text);
}

#[test]
fn install_asks_for_the_values_a_curated_server_needs_instead_of_guessing() {
    let config = Config {
        user_dir: std::path::PathBuf::from("/tmp/dial-test-home"),
        project_root: std::path::PathBuf::from("/tmp"),
        ..Default::default()
    };
    let out = install(&config, "github");
    assert!(!out.is_error);
    let data = out.data.expect("structured");
    assert_eq!(data["status"], "needs_input");
    assert_eq!(data["required"][0]["name"], "GITHUB_PERSONAL_ACCESS_TOKEN");
    assert_eq!(data["required"][0]["secret"], true);
}

#[test]
fn install_refuses_a_server_that_is_already_configured() {
    let mut config = Config {
        user_dir: std::path::PathBuf::from("/tmp/dial-test-home"),
        project_root: std::path::PathBuf::from("/tmp"),
        ..Default::default()
    };
    config.mcp.insert("time".into(), McpServerConfig::default());
    let out = install(&config, "time");
    assert!(out.is_error);
    assert!(out.text.contains("already configured"), "{}", out.text);
}

/// An official-registry fixture listing one remote server, `io.github.acme/notes` at `remote`.
fn registry(remote: &str) -> String {
    use std::io::{Read, Write};
    let body = json!({
        "servers": [{
            "server": {
                "name": "io.github.acme/notes",
                "title": "Notes",
                "description": "Notes over MCP",
                "version": "1.0.0",
                "remotes": [{"type": "streamable-http", "url": remote}],
            },
            "_meta": {"io.modelcontextprotocol.registry/official": {"status": "active", "isLatest": true}},
        }],
        "metadata": {},
    })
    .to_string();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let (mut head, mut chunk) = (Vec::new(), [0u8; 1024]);
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.extend_from_slice(&chunk[..n]),
                }
            }
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    base
}

fn temp_config(project: &tempfile::TempDir, user: &tempfile::TempDir) -> Config {
    Config {
        project_root: project.path().to_path_buf(),
        user_dir: user.path().to_path_buf(),
        ..Default::default()
    }
}

fn saved(config: &Config) -> Config {
    let layout = Layout::with_user_dir(&config.project_root, config.user_dir.clone()).unwrap();
    settings_store::load_config(&layout, &toml::Table::new()).unwrap()
}

#[test]
fn install_saves_a_server_that_asks_for_an_oauth_sign_in_and_points_to_mcp_login() {
    let (project, user) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let config = temp_config(&project, &user);
    let (url, _) = crate::tools::integrations::mcp::tests::http_server_with(
        "granted",
        "Bearer resource_metadata=\"http://127.0.0.1/.well-known/oauth-protected-resource\"",
    );
    let endpoints = Endpoints::at(&registry(&url));
    let out = install_at(&endpoints, &config, "io.github.acme/notes");
    assert!(!out.is_error, "{}", out.text);
    let data = out.data.expect("structured");
    assert_eq!(data["status"], "needs_sign_in");
    assert_eq!(data["name"], "notes");
    let message = data["message"].as_str().unwrap();
    assert!(message.contains("/mcp login notes"), "{message}");
    let entry = &saved(&config).mcp["notes"];
    assert_eq!(entry.url.as_deref(), Some(url.as_str()));
    assert!(entry.enabled);
}

#[test]
fn install_saves_nothing_when_a_server_refuses_without_an_oauth_challenge() {
    let (project, user) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let config = temp_config(&project, &user);
    let (url, _) = crate::tools::integrations::mcp::tests::http_server_with("granted", "");
    let endpoints = Endpoints::at(&registry(&url));
    let out = install_at(&endpoints, &config, "io.github.acme/notes");
    assert!(out.is_error);
    assert!(out.text.contains("nothing saved"), "{}", out.text);
    assert!(!saved(&config).mcp.contains_key("notes"));
}

use super::*;
use crate::ui::overlay::extensions::model::FormKind;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;

/// `(request head, path and query)` to `(status, body)`.
type Handler = Arc<dyn Fn(&str, &str) -> (u16, String) + Send + Sync>;

/// Loopback HTTP fixture: each connection gets one response from `handler`.
fn serve(handler: Handler) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let head = String::from_utf8_lossy(&buf).into_owned();
            let target = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let (status, body) = handler(&head, &target);
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://{addr}")
}

/// A registry with one server, `@acme/notes`, whose connection is `connection`.
/// A bearer key of `bad-key` is rejected, like Smithery does for a bad key.
fn registry(connection: Value) -> String {
    let details = json!({
        "qualifiedName": "@acme/notes",
        "displayName": "Notes",
        "description": "Notes over MCP",
        "connections": [connection],
    })
    .to_string();
    let handler: Handler = Arc::new(move |head: &str, target: &str| {
        if head.contains("Bearer bad-key") {
            (401, "{}".to_owned())
        } else if target.starts_with("/servers?") {
            let servers = json!({"servers": [{
                "id": "acme/notes",
                "qualifiedName": "@acme/notes",
                "displayName": "Notes",
                "description": "Notes over MCP",
                "verified": true,
                "isDeployed": true,
                "useCount": 5,
            }]});
            (200, servers.to_string())
        } else if target == "/servers/acme/notes" {
            (200, details.clone())
        } else {
            (404, "{}".to_owned())
        }
    });
    serve(handler)
}

/// A direct HTTP connection to the loopback MCP endpoint below.
fn fake_connection() -> Value {
    json!({
        "type": "http",
        "deploymentUrl": mcp_endpoint(),
        "configSchema": {
            "properties": {
                "apiKey": {"type": "string", "format": "password"},
                "root": {"type": "string", "default": "/tmp"},
            },
            "required": ["apiKey"],
        },
    })
}

/// Streamable-HTTP MCP fixture: JSON replies to requests, 202 to notifications.
fn mcp_endpoint() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            answer_mcp(stream);
        }
    });
    format!("http://{addr}/mcp")
}

fn answer_mcp(mut stream: std::net::TcpStream) {
    let Ok(clone) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(clone);
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let message: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let result = match message.get("method").and_then(Value::as_str) {
        Some("initialize") => json!({
            "protocolVersion": message["params"]["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fake", "version": "1"},
        }),
        Some("tools/list") => json!({"tools": [
            {"name": "search", "description": "Search notes", "inputSchema": {"type": "object"}},
            {"name": "read", "description": "Read a note", "inputSchema": {"type": "object"}},
        ]}),
        _ => {
            let _ = stream.write_all(
                b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            return;
        }
    };
    let text = json!({"jsonrpc": "2.0", "id": message["id"], "result": result}).to_string();
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
}

fn stage(
    base: &str,
    config: &Config,
    progress: &SharedProgress,
    intent: Intent,
) -> (Outcome, Option<Form>) {
    let layout = Layout::with_user_dir(&config.user_dir, config.user_dir.clone()).expect("layout");
    let endpoints = Endpoints::at(base);
    let outcome = run_at(&endpoints, &layout, config, &intent, progress).expect("stage runs");
    let form = progress.lock().expect("progress").form.take();
    (outcome, form)
}

#[test]
fn smithery_install_asks_for_key_then_shows_settings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        user_dir: dir.path().into(),
        project_root: dir.path().into(),
        ..Default::default()
    };
    let base = registry(fake_connection());
    let progress = SharedProgress::default();
    let name = "@acme/notes";
    let go = |intent| stage(&base, &config, &progress, intent);

    // No key yet: the overlay asks for one.
    let (out, form) = go(Intent::SmitheryInstall { name: name.into() });
    assert!(out.ok);
    assert_eq!(
        form.expect("key form").kind,
        FormKind::Key { name: name.into() }
    );

    // A rejected key stays on the key form with a plain message.
    let (out, form) = go(Intent::SmitheryKey {
        name: name.into(),
        key: "bad-key".into(),
    });
    assert!(out.ok);
    let error = form.expect("key form again").error.expect("error");
    assert!(error.contains("did not accept that key"), "{error}");

    // A good key is saved, then the settings form opens from the registry's schema.
    let (_, form) = go(Intent::SmitheryKey {
        name: name.into(),
        key: "good-key".into(),
    });
    let form = form.expect("settings form");
    assert!(matches!(form.kind, FormKind::Config { .. }));
    let keys: Vec<&str> = form.fields.iter().map(|f| f.key.as_str()).collect();
    assert_eq!(keys, ["apiKey", "root"]);
    assert!(form.fields[0].required && form.fields[0].secret);
    assert_eq!(form.fields[1].value, "/tmp");
    assert!(smithery::key_file(dir.path()).expect("key file").exists());

    // A missing required setting reopens the form with the reason.
    let (out, form) = go(Intent::SmitheryDeploy {
        name: name.into(),
        values: BTreeMap::new(),
    });
    assert!(out.ok);
    let error = form.expect("form again").error.expect("error");
    assert!(error.contains("apiKey"), "{error}");
}

#[test]
fn smithery_deploy_connects_a_direct_server_and_reports_its_tools() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        user_dir: dir.path().into(),
        project_root: dir.path().into(),
        ..Default::default()
    };
    smithery::save_key(
        &smithery::key_file(dir.path()).expect("key file"),
        "good-key",
    )
    .expect("save key");
    // No settings: the direct endpoint connects straight away.
    let base = registry(json!({
        "type": "http",
        "deploymentUrl": mcp_endpoint(),
        "configSchema": {"properties": {}, "required": []},
    }));
    let progress = SharedProgress::default();
    let (out, form) = stage(
        &base,
        &config,
        &progress,
        Intent::SmitheryDeploy {
            name: "@acme/notes".into(),
            values: BTreeMap::new(),
        },
    );
    assert!(form.is_none());
    assert!(out.ok, "{}", out.message);
    assert!(
        out.message.starts_with("Connected, 2 tools"),
        "{}",
        out.message
    );
    assert!(out.message.contains("Saved as `notes`"), "{}", out.message);
    assert!(
        matches!(&out.changes[..], [Change::Insert { path, .. }] if path.as_str() == "mcp.notes")
    );
}

#[test]
fn smithery_deploy_reports_a_failed_connection_and_saves_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        user_dir: dir.path().into(),
        project_root: dir.path().into(),
        ..Default::default()
    };
    smithery::save_key(
        &smithery::key_file(dir.path()).expect("key file"),
        "good-key",
    )
    .expect("save key");
    let base = registry(json!({
        "type": "http",
        "deploymentUrl": "http://127.0.0.1:9/mcp",
        "configSchema": {"properties": {}, "required": []},
    }));
    let progress = SharedProgress::default();
    let (out, form) = stage(
        &base,
        &config,
        &progress,
        Intent::SmitheryDeploy {
            name: "@acme/notes".into(),
            values: BTreeMap::new(),
        },
    );
    assert!(form.is_none());
    assert!(!out.ok);
    assert!(
        out.message.starts_with("Nothing was saved"),
        "{}",
        out.message
    );
    assert!(out.changes.is_empty());
}

#[test]
fn mcp_catalog_search_keeps_nuxt_when_smithery_details_are_unavailable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        user_dir: dir.path().into(),
        project_root: dir.path().into(),
        ..Default::default()
    };
    let layout = Layout::with_user_dir(dir.path(), dir.path().into()).expect("layout");
    let official = serve(Arc::new(|_, _| (503, "{}".into())));
    let smithery = serve(Arc::new(|_, target| {
        if target.starts_with("/servers?") {
            (
                200,
                json!({
                    "servers": [{"qualifiedName":"@nuxt/mcp", "displayName":"Nuxt"}],
                    "pagination": {"totalPages":1}
                })
                .to_string(),
            )
        } else {
            (404, "{}".into())
        }
    }));
    let update = catalog_update(
        &Endpoints {
            official,
            smithery,
            ..Endpoints::at("http://127.0.0.1:9")
        },
        &layout,
        &config,
        Tab::Mcp,
        "nuxt",
        1,
    );
    assert!(update.error.is_none());
    assert!(update.rows.iter().any(|row| row.id == "nuxt"));
    let remote = update
        .rows
        .iter()
        .find(|row| row.id == "@nuxt/mcp")
        .unwrap();
    assert!(remote.tags.iter().any(|tag| tag == "settings on install"));
    assert!(update.message.contains("unavailable: official registry"));
    assert!(!update.more);
}

#[test]
fn curated_servers_that_need_values_open_a_form_before_anything_starts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        user_dir: dir.path().into(),
        project_root: dir.path().into(),
        ..Default::default()
    };
    let endpoints = Endpoints::at("http://127.0.0.1:9");
    let progress = SharedProgress::default();
    let out =
        install_mcp(&endpoints, &config, "github", &BTreeMap::new(), &progress).expect("runs");
    assert!(out.ok && out.changes.is_empty());
    let form = progress
        .lock()
        .expect("progress")
        .form
        .take()
        .expect("values form");
    assert_eq!(
        form.kind,
        FormKind::McpInputs {
            id: "github".into()
        }
    );
    assert!(form.fields[0].required && form.fields[0].secret);

    // An id that no listing knows fails with the reason and saves nothing.
    let missing = install_mcp(
        &endpoints,
        &config,
        "io.example/none",
        &BTreeMap::new(),
        &progress,
    )
    .expect("runs");
    assert!(!missing.ok && missing.changes.is_empty());
}

/// An MCP endpoint that refuses every request with HTTP 401 and `challenge` (none when empty).
fn refusing_endpoint(challenge: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let Ok(clone) = stream.try_clone() else {
                continue;
            };
            let mut reader = BufReader::new(clone);
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
            }
            let _ = reader.read_exact(&mut vec![0; length]);
            let extra = if challenge.is_empty() {
                String::new()
            } else {
                format!("WWW-Authenticate: {challenge}\r\n")
            };
            let _ = write!(
                stream,
                "HTTP/1.1 401 Unauthorized\r\n{extra}Content-Length: 0\r\nConnection: close\r\n\r\n"
            );
        }
    });
    format!("http://{addr}/mcp")
}

/// An official registry listing one remote server, `io.github.acme/notes`, at `remote`.
fn official_listing(remote: String) -> String {
    serve(Arc::new(move |_: &str, target: &str| {
        if target.starts_with("/servers?") {
            let listing = json!({
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
            });
            (200, listing.to_string())
        } else {
            (404, "{}".to_owned())
        }
    }))
}

#[test]
fn servers_that_ask_for_an_oauth_sign_in_are_saved_and_point_to_mcp_login() {
    use crate::config::settings_store::{load_config, save_config};
    let project = tempfile::tempdir().expect("tempdir");
    let user = tempfile::tempdir().expect("tempdir");
    let layout = Layout::with_user_dir(project.path(), user.path().to_path_buf()).expect("layout");
    let config = load_config(&layout, &toml::Table::new()).expect("config");
    let remote = refusing_endpoint("Bearer resource_metadata=\"http://127.0.0.1/.well-known/x\"");
    let endpoints = Endpoints::at(&official_listing(remote.clone()));
    let intent = Intent::InstallCuratedMcp {
        id: "io.github.acme/notes".into(),
    };
    let progress = SharedProgress::default();
    let outcome = run_at(&endpoints, &layout, &config, &intent, &progress).expect("runs");
    assert!(outcome.ok, "{}", outcome.message);
    assert!(
        outcome.message.contains("/mcp login notes"),
        "{}",
        outcome.message
    );
    assert_eq!(outcome.changes.len(), 1);
    let scope = intent_scope(&layout, &intent, Scope::Global).expect("scope");
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
    let saved = load_config(&layout, &toml::Table::new()).expect("saved config");
    assert_eq!(saved.mcp["notes"].url.as_deref(), Some(remote.as_str()));
    assert!(saved.mcp["notes"].enabled);
}

#[test]
fn servers_refusing_without_an_oauth_challenge_still_save_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        user_dir: dir.path().into(),
        project_root: dir.path().into(),
        ..Default::default()
    };
    let endpoints = Endpoints::at(&official_listing(refusing_endpoint("")));
    let progress = SharedProgress::default();
    let out = install_mcp(
        &endpoints,
        &config,
        "io.github.acme/notes",
        &BTreeMap::new(),
        &progress,
    )
    .expect("runs");
    assert!(!out.ok && out.changes.is_empty());
    assert!(out.message.contains("nothing saved"), "{}", out.message);
}

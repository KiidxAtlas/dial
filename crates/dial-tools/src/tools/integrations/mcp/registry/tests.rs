use super::*;
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

fn user() -> &'static Path {
    Path::new("/home/u/.dial")
}

fn entry(server: Value, meta: Value) -> Value {
    json!({"server": server, "_meta": {LATEST_KEY: meta}})
}

#[test]
fn npm_package_is_pinned_and_its_required_env_becomes_inputs() {
    let listing = entry(
        json!({
            "name": "com.example/notes", "title": "Notes", "description": "Notes  over\nMCP",
            "version": "1.2.0",
            "packages": [{
                "registryType": "npm", "identifier": "@example/notes-mcp", "version": "1.2.0",
                "transport": {"type": "stdio"},
                "environmentVariables": [
                    {"name": "NOTES_TOKEN", "description": "API token", "isRequired": true},
                    {"name": "NOTES_REGION", "description": "optional region"}
                ]
            }]
        }),
        json!({"status": "active", "isLatest": true}),
    );
    let found = from_entry(&listing, user()).expect("usable listing");
    assert_eq!(found.title, "Notes");
    assert_eq!(found.description, "Notes over MCP");
    assert_eq!(found.sources, vec![Source::Official]);
    let Install::Server(server) = &found.install else {
        panic!("npm packages are installable");
    };
    assert_eq!(server.program, PathBuf::from("npx"));
    assert_eq!(server.args, ["-y", "@example/notes-mcp@1.2.0"]);
    assert_eq!(
        server.env.get("npm_config_cache").map(String::as_str),
        Some("/home/u/.dial/mcp/npm")
    );
    assert_eq!(found.inputs.len(), 2);
    assert!(found.inputs[0].required && found.inputs[0].secret);
    assert!(!found.inputs[1].required && !found.inputs[1].secret);
}

#[test]
fn pypi_and_oci_packages_run_through_uvx_and_docker() {
    let pypi = entry(
        json!({"name": "ai.adeu/adeu", "description": "d", "version": "1.5.2",
            "packages": [{"registryType": "pypi", "identifier": "adeu", "version": "1.5.2",
                "transport": {"type": "stdio"}}]}),
        json!({"status": "active", "isLatest": true}),
    );
    let Install::Server(server) = from_entry(&pypi, user()).unwrap().install else {
        panic!("pypi is installable");
    };
    assert_eq!(server.program, PathBuf::from("uvx"));
    assert_eq!(server.args, ["--from", "adeu==1.5.2", "adeu"]);

    let oci = entry(
        json!({"name": "io.example/tool", "description": "d", "version": "2.0.0",
            "packages": [{"registryType": "oci", "identifier": "ghcr.io/example/tool:2.0.0",
                "transport": {"type": "stdio"},
                "environmentVariables": [{"name": "EXAMPLE_KEY", "isRequired": true}]}]}),
        json!({"status": "active", "isLatest": true}),
    );
    let found = from_entry(&oci, user()).unwrap();
    let Install::Server(server) = found.install else {
        panic!("oci is installable");
    };
    assert_eq!(server.program, PathBuf::from("docker"));
    assert_eq!(
        server.args,
        [
            "run",
            "-i",
            "--rm",
            "-e",
            "EXAMPLE_KEY",
            "ghcr.io/example/tool:2.0.0"
        ]
    );
    assert_eq!(found.inputs[0].key, "EXAMPLE_KEY");
}

#[test]
fn remote_without_required_headers_is_a_url_and_sse_keeps_its_transport() {
    let listing = entry(
        json!({"name": "io.example/web", "description": "d",
            "remotes": [{"type": "sse", "url": "https://mcp.example.com/sse"}]}),
        json!({"status": "active", "isLatest": true}),
    );
    let Install::Server(server) = from_entry(&listing, user()).unwrap().install else {
        panic!("plain remote is installable");
    };
    assert_eq!(server.url.as_deref(), Some("https://mcp.example.com/sse"));
    assert_eq!(server.transport, Some(McpTransport::Sse));
    assert!(server.program.as_os_str().is_empty());
}

#[test]
fn remotes_needing_a_header_or_variables_are_blocked_with_the_reason() {
    let header = entry(
        json!({"name": "io.example/auth", "description": "d",
            "remotes": [{"type": "streamable-http", "url": "https://x.example/mcp",
                "headers": [{"name": "Authorization", "isRequired": true}]}]}),
        json!({"status": "active", "isLatest": true}),
    );
    let found = from_entry(&header, user()).unwrap();
    assert!(!found.installable());
    assert!(found.install_summary().contains("Authorization"));

    let templated = entry(
        json!({"name": "io.example/tpl", "description": "d",
            "remotes": [{"type": "streamable-http", "url": "https://{tenant}.example/mcp"}]}),
        json!({"status": "active", "isLatest": true}),
    );
    assert!(!from_entry(&templated, user()).unwrap().installable());
}

#[test]
fn superseded_and_deleted_listings_are_skipped() {
    let old = entry(
        json!({"name": "io.example/old", "description": "d",
            "remotes": [{"type": "sse", "url": "https://a.example/sse"}]}),
        json!({"status": "active", "isLatest": false}),
    );
    assert!(from_entry(&old, user()).is_none());
    let gone = entry(
        json!({"name": "io.example/gone", "description": "d"}),
        json!({"status": "deleted", "isLatest": true}),
    );
    assert!(from_entry(&gone, user()).is_none());
    let nameless = json!({"server": {"description": "no name"}});
    assert!(from_entry(&nameless, user()).is_none());
}

#[test]
fn packages_with_unknown_types_fall_through_to_remotes() {
    let listing = entry(
        json!({"name": "io.example/mixed", "description": "d",
            "packages": [{"registryType": "mcpb", "identifier": "x"}],
            "remotes": [{"type": "streamable-http", "url": "https://m.example/mcp"}]}),
        json!({"status": "active", "isLatest": true}),
    );
    assert!(from_entry(&listing, user()).unwrap().installable());
}

/// Loopback registry with latest-version filtering and configurable cursor pagination.
fn serve_pages(total: usize, repeat_cursor: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let Ok(clone) = stream.try_clone() else {
                continue;
            };
            let mut reader = BufReader::new(clone);
            let mut target = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if target.is_empty() {
                    target = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
                }
            }
            let url = url::Url::parse(&format!("http://localhost{target}")).unwrap();
            let params: std::collections::BTreeMap<_, _> = url.query_pairs().collect();
            let page: usize = params
                .get("cursor")
                .map(|v| v.parse().unwrap())
                .unwrap_or(1);
            let body = if params.get("version").map(|v| v.as_ref()) != Some("latest") {
                json!({"error": "version=latest is required"})
            } else {
                let next = (page < total).then(|| {
                    if repeat_cursor {
                        "2".to_owned()
                    } else {
                        (page + 1).to_string()
                    }
                });
                json!({
                    "servers": [{"server": {
                        "name": format!("io.example/page-{page}"),
                        "description": format!("page {page}")
                    }}],
                    "metadata": {"nextCursor": next}
                })
            }
            .to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    format!("http://{addr}/v0")
}

#[test]
fn paging_follows_cursors_and_reports_the_end() {
    let base = serve_pages(2, false);
    let (first, more) = search_page_at(&base, "", 1, user()).expect("page 1");
    assert_eq!(
        first.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["io.example/page-1"]
    );
    assert!(more);
    let (second, more) = search_page_at(&base, "", 2, user()).expect("page 2");
    assert_eq!(
        second.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["io.example/page-2"]
    );
    assert!(!more);
    let found = find_at(&base, "io.example/page-2", user())
        .expect("find")
        .expect("listed");
    assert_eq!(found.description, "page 2");
    assert!(
        find_at(&base, "io.example/missing", user())
            .expect("find")
            .is_none()
    );
}

#[test]
fn browsing_reaches_pages_after_ten_and_stops_at_the_end() {
    let base = serve_pages(12, false);
    let (rows, more) = search_page_at(&base, "nuxt", 11, user()).expect("page 11");
    assert_eq!(rows[0].name, "io.example/page-11");
    assert!(more);
    let (rows, more) = search_page_at(&base, "nuxt", 12, user()).expect("page 12");
    assert_eq!(rows[0].name, "io.example/page-12");
    assert!(!more);
    let (rows, more) = search_page_at(&base, "nuxt", 13, user()).expect("past end");
    assert!(rows.is_empty());
    assert!(!more);
}

#[test]
fn repeated_registry_cursor_is_an_error_not_an_endless_page() {
    let base = serve_pages(12, true);
    let error = search_page_at(&base, "", 3, user()).expect_err("repeated cursor");
    assert!(error.to_string().contains("repeated a pagination cursor"));
}

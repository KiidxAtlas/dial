use super::*;
use serde_json::json;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

/// Loopback host serving fixed bodies by path; anything else is 404.
fn serve(routes: BTreeMap<&'static str, String>) -> String {
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
            let path = target.split('?').next().unwrap_or("/");
            let (status, body) = match routes.get(path) {
                Some(body) => ("200 OK", body.clone()),
                None => ("404 Not Found", "{}".to_owned()),
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    format!("http://{addr}")
}

fn endpoints(base: &str) -> Endpoints {
    Endpoints::at(base)
}

fn repo_info(full_name: &str) -> String {
    json!({"full_name": full_name, "description": "d", "default_branch": "main",
        "stargazers_count": 1234})
    .to_string()
}

#[test]
fn search_lists_repositories_as_deferred_github_rows() {
    let base = serve(BTreeMap::from([(
        "/search/repositories",
        json!({"total_count": 2, "items": [
            {"full_name": "mksglu/context-mode", "description": "Context window optimization",
                "stargazers_count": 30224},
            {"full_name": "old/archived", "archived": true}
        ]})
        .to_string(),
    )]));
    let (rows, more) = search_page_at(&base, "context-mode", 1).expect("search");
    assert!(!more);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "github:mksglu/context-mode");
    assert_eq!(rows[0].title, "context-mode");
    assert_eq!(rows[0].badges, vec!["30.2k stars".to_owned()]);
    assert!(rows[0].installable());
    assert!(matches!(rows[0].install, Install::Deferred(_)));
    assert_eq!(
        rows[0].repository.as_deref(),
        Some("https://github.com/mksglu/context-mode")
    );
}

#[test]
fn a_repository_whose_npm_package_points_back_installs_through_npx() {
    let base = serve(BTreeMap::from([
        ("/repos/acme/notes", repo_info("acme/notes")),
        (
            "/acme/notes/main/package.json",
            json!({"name": "@acme/notes", "bin": "cli.js"}).to_string(),
        ),
        (
            "/@acme/notes/latest",
            json!({"name": "@acme/notes", "version": "1.4.0", "bin": "cli.js",
                "repository": "github:acme/notes"})
            .to_string(),
        ),
    ]));
    let found = resolve(&endpoints(&base), "acme/notes", Path::new("/u")).expect("resolve");
    let Install::Server(server) = found.install else {
        panic!("npm package is installable: {}", found.install_summary());
    };
    assert_eq!(server.args, ["-y", "@acme/notes@1.4.0"]);
    assert_eq!(found.name, "github:acme/notes");
}

#[test]
fn a_package_name_published_by_someone_else_is_not_trusted() {
    let base = serve(BTreeMap::from([
        ("/repos/acme/notes", repo_info("acme/notes")),
        (
            "/acme/notes/main/package.json",
            json!({"name": "notes", "bin": "cli.js"}).to_string(),
        ),
        (
            "/notes/latest",
            json!({"name": "notes", "version": "9.0.0", "bin": "cli.js",
                "repository": "https://github.com/squatter/notes"})
            .to_string(),
        ),
    ]));
    let found = resolve(&endpoints(&base), "acme/notes", Path::new("/u")).expect("resolve");
    assert!(matches!(found.install, Install::Blocked(_)));
    assert!(
        found
            .install_summary()
            .contains("does not name this repository")
    );
}

#[test]
fn a_python_project_installs_through_uvx_or_explains_unclear_commands() {
    let pyproject = |scripts: &str| {
        format!("[project]\nname = \"notes-mcp\"\n\n[project.scripts]\n{scripts}\n")
    };
    let pypi = json!({"info": {"version": "0.3.1", "project_urls": {
        "Repository": "https://github.com/acme/notes"}}})
    .to_string();
    let base = serve(BTreeMap::from([
        ("/repos/acme/notes", repo_info("acme/notes")),
        (
            "/acme/notes/main/pyproject.toml",
            pyproject("notes-mcp = \"notes.server:main\"\nnotes-admin = \"notes.cli:main\""),
        ),
        ("/notes-mcp/json", pypi.clone()),
        // Shaped like headroom: the main CLI is named after the repository but serves MCP
        // only through a subcommand, so it must not be picked.
        ("/repos/acme/tools", repo_info("acme/tools")),
        (
            "/acme/tools/main/pyproject.toml",
            pyproject("tools = \"a:main\"\ntools-cache = \"b:main\"")
                .replace("notes-mcp", "tools-ai"),
        ),
        ("/tools-ai/json", pypi.replace("acme/notes", "acme/tools")),
    ]));
    let found = resolve(&endpoints(&base), "acme/notes", Path::new("/u")).expect("resolve");
    let Install::Server(server) = found.install else {
        panic!("pypi package is installable: {}", found.install_summary());
    };
    assert_eq!(server.program, PathBuf::from("uvx"));
    assert_eq!(server.args, ["--from", "notes-mcp==0.3.1", "notes-mcp"]);

    let unclear = resolve(&endpoints(&base), "acme/tools", Path::new("/u")).expect("resolve");
    assert!(!matches!(unclear.install, Install::Server(_)));
    assert!(unclear.install_summary().contains("tools, tools-cache"));
    assert!(
        unclear
            .install_summary()
            .contains("https://github.com/acme/tools")
    );
}

#[test]
fn a_repository_without_a_package_links_to_itself() {
    let base = serve(BTreeMap::from([(
        "/repos/acme/docs",
        repo_info("acme/docs"),
    )]));
    let found = resolve(&endpoints(&base), "acme/docs", Path::new("/u")).expect("resolve");
    assert!(
        found
            .install_summary()
            .contains("https://github.com/acme/docs")
    );
    assert!(resolve(&endpoints(&base), "acme/missing", Path::new("/u")).is_err());
    assert!(resolve(&endpoints(&base), "../etc", Path::new("/u")).is_err());
}

#[test]
fn tokens_are_never_sent_to_other_hosts() {
    assert!(token("http://127.0.0.1:9").is_none());
}

/// Loopback host answering 401 to any request carrying `Authorization` and 200 otherwise,
/// counting signed requests.
fn serve_rejecting_tokens() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let signed = std::sync::Arc::new(AtomicUsize::new(0));
    let seen = signed.clone();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let Ok(clone) = stream.try_clone() else {
                continue;
            };
            let mut reader = BufReader::new(clone);
            let mut authorized = false;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                authorized |= line.to_ascii_lowercase().starts_with("authorization:");
            }
            let (status, body) = if authorized {
                seen.fetch_add(1, Ordering::SeqCst);
                ("401 Unauthorized", r#"{"message":"Bad credentials"}"#)
            } else {
                ("200 OK", r#"{"total_count":0,"items":[]}"#)
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (format!("http://{addr}"), signed)
}

#[test]
fn a_rejected_token_falls_back_to_an_anonymous_request() {
    let (base, signed) = serve_rejecting_tokens();
    let url = format!("{base}/search/repositories?q=supabase");
    let reply = api_get_as(&url, "GitHub search", Some("Bearer stale")).expect("anonymous retry");
    assert_eq!(reply.status, 200);
    assert_eq!(signed.load(std::sync::atomic::Ordering::SeqCst), 1);
}

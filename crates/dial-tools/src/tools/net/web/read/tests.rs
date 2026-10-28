//! `read` of URLs against an in-process loopback server. Loopback is only reachable through
//! [`Client::allowing_loopback`]; the shipping client refuses it.

use super::*;
use crate::tools::testkit;
use std::io::{Read, Write};
use std::net::TcpListener;

struct Served {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

/// Serve `handler(path)` on a free loopback port until the process exits; returns the base URL.
fn serve(handler: impl Fn(&str) -> Served + Send + Sync + 'static) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let handler = std::sync::Arc::new(handler);
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let handler = handler.clone();
            std::thread::spawn(move || {
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&request);
                let path = text
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_owned();
                let reply = handler(&path);
                let head = format!(
                    "HTTP/1.1 {} X\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    reply.status,
                    reply.content_type,
                    reply.body.len()
                );
                let _ = stream
                    .write_all(head.as_bytes())
                    .and_then(|()| stream.write_all(&reply.body));
            });
        }
    });
    base
}

fn page(content_type: &'static str, body: &str) -> Served {
    Served {
        status: 200,
        content_type,
        body: body.as_bytes().to_vec(),
    }
}

/// A workspace whose sessions allow web network access, reading through a loopback client.
fn open_workspace() -> (tempfile::TempDir, Workspace) {
    let (dir, ws) = testkit::workspace();
    (dir, ws.with_web_network_block(None))
}

fn read_url(client: &Client, ws: &Workspace, path: &str) -> Result<String, String> {
    let args = serde_json::json!({ "path": path });
    read_with(client, ws, &args, &AtomicBool::new(false))
}

#[test]
fn url_scheme_is_matched_case_insensitively() {
    assert!(is_url("HTTPS://example.com/a"));
    assert!(is_url("http://127.0.0.1:1/"));
    assert!(!is_url("https:example.com"));
    assert!(!is_url("file:///tmp/a"));
    assert!(!is_url("notes/http://x"));
}

#[test]
fn port_and_selector_split_only_after_the_path_starts() {
    let target = split_url("https://host:8080/dl/a.txt:2-3").unwrap();
    assert_eq!(target.url, "https://host:8080/dl/a.txt");
    assert_eq!(target.member, None);
    assert!(target.selector.selects_lines());

    let bare = split_url("https://host:8080").unwrap();
    assert_eq!(bare.url, "https://host:8080");
}

#[test]
fn archive_member_is_split_from_the_url_and_keeps_its_selector() {
    let target = split_url("https://host:8080/dl/bundle.zip:docs/readme.txt:2-3").unwrap();
    assert_eq!(target.url, "https://host:8080/dl/bundle.zip");
    assert_eq!(target.member, Some("docs/readme.txt"));
    assert!(target.selector.selects_lines());
}

#[test]
fn line_selector_returns_only_the_selected_lines() {
    let base = serve(|_| page("text/plain", "alpha\nbravo\ncharlie\ndelta\n"));
    let (_dir, ws) = open_workspace();
    let shown = read_url(
        &Client::allowing_loopback(),
        &ws,
        &format!("{base}/n.txt:2-3"),
    )
    .unwrap();
    assert!(shown.contains("bravo"), "{shown}");
    assert!(shown.contains("charlie"), "{shown}");
    assert!(!shown.contains("alpha"), "{shown}");
    assert!(!shown.contains("delta"), "{shown}");
}

#[test]
fn offset_and_limit_select_a_window() {
    let base = serve(|_| page("text/plain", "alpha\nbravo\ncharlie\ndelta\n"));
    let (_dir, ws) = open_workspace();
    let args = serde_json::json!({ "path": format!("{base}/n.txt"), "offset": 3, "limit": 1 });
    let shown = read_with(
        &Client::allowing_loopback(),
        &ws,
        &args,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(shown.contains("charlie"), "{shown}");
    assert!(!shown.contains("bravo"), "{shown}");
    assert!(!shown.contains("delta"), "{shown}");
}

#[test]
fn json_content_type_is_pretty_printed() {
    let base = serve(|_| page("application/json", r#"{"k":1}"#));
    let (_dir, ws) = open_workspace();
    let shown = read_url(&Client::allowing_loopback(), &ws, &format!("{base}/data")).unwrap();
    assert!(shown.contains(r#""k": 1"#), "{shown}");
}

#[test]
fn html_content_type_is_converted_to_text() {
    let base = serve(|_| page("text/html", "<html><body><p>hi there</p></body></html>"));
    let (_dir, ws) = open_workspace();
    let shown = read_url(&Client::allowing_loopback(), &ws, &format!("{base}/page")).unwrap();
    assert!(shown.contains("hi there"), "{shown}");
    assert!(!shown.contains("<p>"), "{shown}");
}

#[test]
fn binary_content_is_refused_with_an_explicit_error() {
    let base = serve(|_| page("image/png", "\u{89}PNG\r\n\u{1a}\n\0\0"));
    let (_dir, ws) = open_workspace();
    let err = read_url(&Client::allowing_loopback(), &ws, &format!("{base}/x.png")).unwrap_err();
    assert!(err.contains("binary content"), "{err}");
}

#[test]
fn http_error_status_is_reported() {
    let base = serve(|_| Served {
        status: 404,
        content_type: "text/plain",
        body: b"missing".to_vec(),
    });
    let (_dir, ws) = open_workspace();
    let err = read_url(&Client::allowing_loopback(), &ws, &format!("{base}/gone")).unwrap_err();
    assert!(err.contains("HTTP 404"), "{err}");
}

#[test]
fn cancelled_read_stops_before_returning_content() {
    let base = serve(|_| page("text/plain", "alpha\n"));
    let (_dir, ws) = open_workspace();
    let args = serde_json::json!({ "path": format!("{base}/n.txt") });
    let err = read_with(
        &Client::allowing_loopback(),
        &ws,
        &args,
        &AtomicBool::new(true),
    )
    .unwrap_err();
    assert!(err.contains("cancelled"), "{err}");
}

#[test]
fn default_workspace_refuses_url_reads_before_any_request() {
    let base = serve(|_| page("text/plain", "alpha\n"));
    let (_dir, ws) = testkit::workspace();
    let err = read_url(&Client::allowing_loopback(), &ws, &format!("{base}/n.txt")).unwrap_err();
    assert!(err.contains("network access is disabled"), "{err}");
    assert!(err.contains("no session network policy"), "{err}");
}

#[test]
fn session_network_block_reason_is_reported() {
    let base = serve(|_| page("text/plain", "alpha\n"));
    let (_dir, ws) = testkit::workspace();
    let ws = ws.with_web_network_block(Some("sandbox.allow_network is false"));
    let err = read_url(&Client::allowing_loopback(), &ws, &format!("{base}/n.txt")).unwrap_err();
    assert!(err.contains("sandbox.allow_network is false"), "{err}");
}

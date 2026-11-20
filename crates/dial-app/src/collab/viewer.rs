//! The relay's built-in browser viewer. `dial relay` answers a plain `GET /` with the static
//! viewer from `assets/collab-web/`, so a room can be watched without hosting the page elsewhere.
//! The room key stays in the URL fragment, which browsers never send, so the relay still sees
//! only metadata. WebSocket upgrades are left untouched for the relay itself.
use dial_llm::page_policy::{page_headers, page_policy};
use std::sync::OnceLock;

const PAGE: &str = include_str!("../../../../assets/collab-web/index.html");
/// Longest request head inspected before deciding between the viewer and the relay.
pub(super) const MAX_HEAD_BYTES: usize = 8 * 1024;

/// What a peeked request head asks for.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Request {
    /// The head is not complete yet; peek again.
    Incomplete,
    /// A WebSocket upgrade or anything else the relay handshake should answer.
    Relay,
    /// A plain HTTP request answered here; the bytes are the full response.
    Http(Vec<u8>),
}

/// Classify a request head peeked from a fresh connection.
pub(super) fn classify(head: &[u8]) -> Request {
    let Some(end) = head.windows(4).position(|window| window == b"\r\n\r\n") else {
        return if head.len() >= MAX_HEAD_BYTES {
            Request::Relay
        } else {
            Request::Incomplete
        };
    };
    let Ok(text) = std::str::from_utf8(&head[..end]) else {
        return Request::Relay;
    };
    let mut lines = text.split("\r\n");
    let mut request_line = lines.next().unwrap_or("").split(' ');
    let (method, target) = (
        request_line.next().unwrap_or(""),
        request_line.next().unwrap_or(""),
    );
    let upgrade = lines.any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.trim().eq_ignore_ascii_case("upgrade")
                && value.trim().eq_ignore_ascii_case("websocket")
        })
    });
    if upgrade {
        return Request::Relay;
    }
    let path = target.split(['?', '#']).next().unwrap_or("");
    Request::Http(match (method, path) {
        ("GET" | "HEAD", "/" | "/index.html") => {
            response("200 OK", "text/html; charset=utf-8", PAGE, method == "HEAD")
        }
        ("GET" | "HEAD", _) => response(
            "404 Not Found",
            "text/plain",
            "not found\n",
            method == "HEAD",
        ),
        _ => response(
            "405 Method Not Allowed",
            "text/plain",
            "method not allowed\n",
            false,
        ),
    })
}

/// Content Security Policy for the viewer page: its inline script and style are pinned by hash,
/// and it may connect only back to this relay.
pub(super) fn content_security_policy() -> &'static str {
    static POLICY: OnceLock<String> = OnceLock::new();
    POLICY.get_or_init(|| page_policy(PAGE))
}

fn response(status: &str, kind: &str, body: &str, head_only: bool) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n{}X-Content-Type-Options: nosniff\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len(),
        page_headers(content_security_policy())
    )
    .into_bytes();
    if !head_only {
        out.extend_from_slice(body.as_bytes());
    }
    out
}

#[cfg(test)]
mod tests;

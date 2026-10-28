//! Loopback-only statistics dashboard and per-session trace routes.
//!
//! Routes: `/` (HTML page; `#/traces?s=<session file>` renders one session), `/api/stats`
//! (all-time report), `/api/version`, `/api/live`, `/api/models`, `/api/tools`, `/api/requests`,
//! `/api/request/<id>`, `/api/frustration`, all with an optional `range` (`24h`, `7d`, `all`),
//! and `/api/trace?session=<session file>` (one session; files outside
//! `<user_dir>/projects/*/sessions/` are refused with 403).
use super::{Db, now_ms, parse_range, session_file_in_tree};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::{
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    path::{Path, PathBuf},
    time::Duration,
};

const PAGE: &str = r#"<!doctype html><meta charset=utf-8><meta name=viewport content="width=device-width"><title>Dial stats</title><style>body{font:16px system-ui;max-width:1000px;margin:2rem auto;padding:0 1rem;background:#111;color:#ddd}pre{white-space:pre-wrap;background:#222;padding:1rem;border-radius:8px}</style><h1 id=title>Dial usage</h1><p id=sub>Local session and model usage</p><pre id=data>Loading…</pre><script>const hash=location.hash,trace=hash.indexOf('#/traces')===0;const session=new URLSearchParams(hash.split('?')[1]||'').get('s')||'';if(trace){title.textContent='Dial session trace';sub.textContent=session||'No session selected'}const url=trace?(session?'/api/trace?session='+encodeURIComponent(session):null):'/api/stats';if(!url){data.textContent='No session selected. Open a trace with /trace.'}else{fetch(url).then(r=>{if(!r.ok)return r.text().then(t=>{throw Error(r.status+' '+t)});return r.json()}).then(x=>data.textContent=JSON.stringify(x,null,2)).catch(e=>data.textContent='Unable to load: '+e)}</script>"#;

const TEXT: &str = "text/plain; charset=utf-8";
const HTML: &str = "text/html; charset=utf-8";
const JSON: &str = "application/json";

/// Binds the dashboard listener on 127.0.0.1 (`port` 0 picks an ephemeral port).
pub fn bind_loopback(port: u16) -> Result<TcpListener> {
    TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
        .with_context(|| format!("binding local stats dashboard on 127.0.0.1:{port}"))
}

pub fn serve(user_dir: PathBuf, port: u16) -> Result<()> {
    let listener = bind_loopback(port)?;
    eprintln!("Dial stats dashboard: http://{}", listener.local_addr()?);
    serve_listener(listener, user_dir)
}

/// Serves requests on `listener` until it fails. Connections are handled one at a time.
pub fn serve_listener(listener: TcpListener, user_dir: PathBuf) -> Result<()> {
    let port = listener.local_addr()?.port();
    for incoming in listener.incoming() {
        let mut stream = match incoming {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("stats dashboard connection failed: {error}");
                continue;
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
        // One client that never reads must not stall the single-threaded accept loop.
        stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
        // Read until the header block ends: one read can return only part of a request, and
        // answering with bytes still queued makes the kernel reset the connection.
        let mut request = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => request.extend_from_slice(&chunk[..n]),
            }
            if request.windows(4).any(|w| w == b"\r\n\r\n") || request.len() >= 8192 {
                break;
            }
        }
        if request.is_empty() {
            continue;
        }
        let line = String::from_utf8_lossy(&request);
        let (status, content_type, body) = respond(&user_dir, port, &line);
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
    }
    bail!("stats dashboard listener stopped")
}

/// Only loopback host names reach the routes: a page on another origin whose DNS name rebinds
/// to 127.0.0.1 sends its own name as `Host`, so it cannot read session transcripts.
fn loopback_host(request: &str, port: u16) -> bool {
    request
        .lines()
        .skip(1)
        .take_while(|line| !line.is_empty())
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("host")
                .then(|| value.trim())
        })
        .is_some_and(|host| {
            // The port may be omitted; when present it must be this listener's.
            let name = match host.rsplit_once(':') {
                Some((name, given)) if given == port.to_string() => name,
                Some(_) => return false,
                None => host,
            };
            name == "127.0.0.1" || name.eq_ignore_ascii_case("localhost")
        })
}

type Response = (&'static str, &'static str, String);

/// Answers one raw HTTP request: the `Host` gate first, then routing. `port` is the listener's.
fn respond(user_dir: &Path, port: u16, request: &str) -> Response {
    if !loopback_host(request, port) {
        return ("403 Forbidden", TEXT, "unexpected Host header\n".to_owned());
    }
    let target = request
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/");
    route(user_dir, target)
}

/// Maps a request target (`/path?query`) to `(status, content type, body)`.
fn route(user_dir: &Path, target: &str) -> Response {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if let Some(id) = path.strip_prefix("/api/request/") {
        return api_request(user_dir, id);
    }
    match path {
        "/" => ("200 OK", HTML, PAGE.to_owned()),
        "/api/version" => json_response(Ok(dial_build_info::build_info::report())),
        "/api/stats" => json_response(with_db(user_dir, |db| db.report(None, now_ms()))),
        "/api/live" => optional_json(
            with_db(user_dir, |db| db.live(user_dir, now_ms())),
            "no live session: the stats database has no session yet",
        ),
        "/api/models" => match range_since(query, now_ms()) {
            Ok(since) => json_response(with_db(user_dir, |db| db.models(since))),
            Err(error) => bad_request(error),
        },
        "/api/tools" => match range_since(query, now_ms()) {
            Ok(since) => json_response(with_db(user_dir, |db| db.tools(since))),
            Err(error) => bad_request(error),
        },
        "/api/requests" => api_requests(user_dir, query),
        "/api/frustration" => match range_since(query, now_ms()) {
            Ok(since) => json_response(with_db(user_dir, |db| db.frustration(user_dir, since))),
            Err(error) => bad_request(error),
        },
        "/api/trace" => api_trace(user_dir, query),
        _ => ("404 Not Found", TEXT, "Not found".into()),
    }
}

/// Largest `limit` accepted by `/api/requests`; the default when none is given.
const REQUEST_LIMIT_MAX: usize = 500;
const REQUEST_LIMIT_DEFAULT: usize = 50;

fn bad_request(error: impl std::fmt::Display) -> Response {
    ("400 Bad Request", TEXT, format!("{error:#}"))
}

/// Serializes `result` as JSON (500 with the error text when it failed).
fn json_response<T: Serialize>(result: Result<T>) -> Response {
    match result.and_then(|value| Ok(serde_json::to_string(&value)?)) {
        Ok(body) => ("200 OK", JSON, body),
        Err(error) => ("500 Internal Server Error", TEXT, format!("{error:#}")),
    }
}

/// Like [`json_response`], but `Ok(None)` is a 404 with `missing` as its text.
fn optional_json<T: Serialize>(result: Result<Option<T>>, missing: &str) -> Response {
    match result {
        Ok(Some(value)) => json_response(Ok(value)),
        Ok(None) => ("404 Not Found", TEXT, missing.to_owned()),
        Err(error) => ("500 Internal Server Error", TEXT, format!("{error:#}")),
    }
}

/// Opens the stats database, folds in new transcript activity, then runs `read`.
fn with_db<T>(user_dir: &Path, read: impl FnOnce(&Db) -> Result<T>) -> Result<T> {
    let mut db = Db::open_default(user_dir)?;
    db.sync_sessions(user_dir)?;
    read(&db)
}

/// The `range` query value (`24h`, `7d`, `all`, ...) as a lookback start; absent means all time.
fn range_since(query: &str, now: i64) -> Result<Option<i64>> {
    let range = query_param(query, "range").unwrap_or_else(|| "all".to_owned());
    Ok(parse_range(&range)?.map(|window| now - window))
}

fn api_requests(user_dir: &Path, query: &str) -> Response {
    let limit = match query_param(query, "limit") {
        None => REQUEST_LIMIT_DEFAULT,
        Some(text) => match text.parse::<usize>() {
            Ok(n) if (1..=REQUEST_LIMIT_MAX).contains(&n) => n,
            _ => {
                return bad_request(format!(
                    "limit must be an integer from 1 to {REQUEST_LIMIT_MAX}"
                ));
            }
        },
    };
    match range_since(query, now_ms()) {
        Ok(since) => json_response(with_db(user_dir, |db| db.recent_requests(since, limit))),
        Err(error) => bad_request(error),
    }
}

fn api_request(user_dir: &Path, id: &str) -> Response {
    let Ok(id) = id.parse::<i64>() else {
        return bad_request("request id must be an integer");
    };
    optional_json(with_db(user_dir, |db| db.request(id)), "request not found")
}

fn api_trace(user_dir: &Path, query: &str) -> (&'static str, &'static str, String) {
    let Some(session) = query_param(query, "session") else {
        return ("400 Bad Request", TEXT, "missing session parameter".into());
    };
    let Some((project, file)) = session_file_in_tree(user_dir, Path::new(&session)) else {
        return (
            "403 Forbidden",
            TEXT,
            "session file is outside the stats sessions tree".into(),
        );
    };
    let result = (|| -> Result<Option<String>> {
        let mut db = Db::open_default(user_dir)?;
        db.sync_sessions(user_dir)?;
        match db.trace(user_dir, &project, &file)? {
            Some(trace) => Ok(Some(serde_json::to_string(&trace)?)),
            None => Ok(None),
        }
    })();
    match result {
        Ok(Some(body)) => ("200 OK", JSON, body),
        Ok(None) => (
            "404 Not Found",
            TEXT,
            "session has no stats record (its transcript has no session header)".into(),
        ),
        Err(error) => ("500 Internal Server Error", TEXT, format!("{error:#}")),
    }
}

/// First value of `key` in an `a=b&c=d` query string, percent-decoded.
fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (percent_decode(k) == key).then(|| percent_decode(v))
    })
}

/// Percent-encodes exactly like JavaScript `encodeURIComponent`.
pub fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Decodes `%XX` escapes; malformed escapes are kept verbatim. `+` is literal (as `encodeURIComponent` emits it).
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(value) = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(value);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests;

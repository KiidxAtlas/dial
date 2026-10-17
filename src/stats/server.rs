//! Loopback-only statistics dashboard and per-session trace routes.
//!
//! Routes: `/` (HTML page; `#/traces?s=<session file>` renders one session), `/api/stats`
//! (all-time report) and `/api/trace?session=<session file>` (one session, files outside
//! `<user_dir>/projects/*/sessions/` are refused with 403).
use super::{Db, now_ms, session_file_in_tree};
use anyhow::{Context, Result, bail};
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
    for incoming in listener.incoming() {
        let mut stream = match incoming {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("stats dashboard connection failed: {error}");
                continue;
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
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
        let target = line
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("/");
        let (status, content_type, body) = route(&user_dir, target);
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
    }
    bail!("stats dashboard listener stopped")
}

/// Maps a request target (`/path?query`) to `(status, content type, body)`.
fn route(user_dir: &Path, target: &str) -> (&'static str, &'static str, String) {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    match path {
        "/" => ("200 OK", HTML, PAGE.to_owned()),
        "/api/stats" => match (|| -> Result<String> {
            let mut db = Db::open_default(user_dir)?;
            db.sync_sessions(user_dir)?;
            Ok(serde_json::to_string(&db.report(None, now_ms())?)?)
        })() {
            Ok(body) => ("200 OK", JSON, body),
            Err(error) => ("500 Internal Server Error", TEXT, format!("{error:#}")),
        },
        "/api/trace" => api_trace(user_dir, query),
        _ => ("404 Not Found", TEXT, "Not found".into()),
    }
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

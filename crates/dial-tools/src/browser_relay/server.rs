//! Loopback HTTP and WebSocket server for the browser relay.
//!
//! One port serves everything:
//! - `GET /json/version`, `/json/list`, `/json`: discovery, answered like Chrome's DevTools endpoints.
//! - `GET /cdp` (WebSocket): CDP clients such as Playwright.
//! - `GET /ext` (WebSocket): the Dial Chrome extension. It must come from a `chrome-extension://` origin.
//!
//! Every route requires the relay token, either as the `x-dial-relay-token` header or as a `token`
//! query parameter (the form Playwright uses inside `webSocketDebuggerUrl`).

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tungstenite::handshake::derive_accept_key;
use tungstenite::protocol::{Role, WebSocketConfig};
use tungstenite::{Message, WebSocket};

use super::bridge::{Relay, Snapshot};
use super::outbox::{self, OutboxReceiver};
use super::protocol::{ExtMessage, PROTOCOL_VERSION, TOKEN_HEADER};

/// Longest request head accepted before the connection is refused.
const MAX_HEAD_BYTES: usize = 16 * 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(5);
/// Socket poll interval: outbound queues drain at least this often.
const POLL: Duration = Duration::from_millis(20);
/// An extension that sends nothing for this long is considered gone. The extension pings every 20s.
const EXTENSION_SILENCE: Duration = Duration::from_secs(60);
const EXTENSION_ORIGIN_PREFIX: &str = "chrome-extension://";
const NOT_CONNECTED_MESSAGE: &str = "Dial browser extension is not connected. Run `dial browser-relay install`, then load the printed folder in chrome://extensions.";
/// Inbound limit per WebSocket message and per frame. Outbound limits live in `outbox`.
const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
/// CDP requests one client may have running at once. Requests past the cap get an error reply.
const MAX_IN_FLIGHT_PER_CLIENT: usize = 64;
const BUSY_MESSAGE: &str =
    "too many requests are running for this client; wait for earlier responses";
/// A peer that stops reading for this long loses its connection instead of pinning its queue.
const WRITE_STALL: Duration = Duration::from_secs(10);

/// Everything a connection thread needs. Shared through `Arc`.
pub struct Shared {
    pub relay: Arc<Relay>,
    pub token: String,
    pub port: u16,
}

/// Accepts connections forever, one thread per connection.
pub fn serve(listener: TcpListener, shared: Arc<Shared>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let shared = Arc::clone(&shared);
                thread::spawn(move || handle_connection(stream, &shared));
            }
            Err(_) => thread::sleep(POLL),
        }
    }
}

struct Head {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn query_value(&self, name: &str) -> Option<&str> {
        self.query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key == name).then_some(value)
        })
    }

    fn is_websocket_upgrade(&self) -> bool {
        self.header("upgrade")
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
    }
}

fn handle_connection(mut stream: TcpStream, shared: &Shared) {
    let _ = stream.set_read_timeout(Some(HEAD_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WRITE_STALL));
    let head = match read_head(&mut stream) {
        Ok(head) => head,
        Err(_) => {
            respond_json(
                &mut stream,
                400,
                "Bad Request",
                &json!({"error": "malformed request"}),
            );
            return;
        }
    };
    if !authorized(&head, &shared.token) {
        respond_json(
            &mut stream,
            401,
            "Unauthorized",
            &json!({"error": "missing or wrong Dial relay token"}),
        );
        return;
    }
    match (head.method.as_str(), head.path.as_str()) {
        ("GET", "/json/version") => json_version(&mut stream, shared),
        ("GET", "/json/list" | "/json") => json_list(&mut stream, shared),
        ("GET", "/cdp") => cdp_endpoint(stream, &head, shared),
        ("GET", "/ext") => extension_endpoint(stream, &head, shared),
        _ => respond_json(
            &mut stream,
            404,
            "Not Found",
            &json!({"error": "not found"}),
        ),
    }
}

fn authorized(head: &Head, token: &str) -> bool {
    let presented = head
        .header(TOKEN_HEADER)
        .or_else(|| head.query_value("token"));
    presented.is_some_and(|presented| same_token(presented, token))
}

/// Compares without an early exit so timing does not reveal how much of the token matched.
fn same_token(presented: &str, expected: &str) -> bool {
    presented.len() == expected.len()
        && presented
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// Reads one byte at a time up to the blank line. Byte-at-a-time keeps any WebSocket frame that
/// follows the head unread in the socket.
fn read_head(stream: &mut TcpStream) -> io::Result<Head> {
    let mut raw = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    while !raw.ends_with(b"\r\n\r\n") {
        if raw.len() >= MAX_HEAD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request head too large",
            ));
        }
        if stream.read(&mut byte)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed",
            ));
        }
        raw.push(byte[0]);
    }
    let text = std::str::from_utf8(&raw)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "request head is not UTF-8"))?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let target = parts.next().unwrap_or("");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let headers = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        })
        .collect();
    if method.is_empty() || !target.starts_with('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid request line",
        ));
    }
    Ok(Head {
        method,
        path: path.to_owned(),
        query: query.to_owned(),
        headers,
    })
}

fn json_version(stream: &mut TcpStream, shared: &Shared) {
    let snapshot = shared.relay.snapshot();
    if !snapshot.connected {
        respond_not_connected(stream, &snapshot);
        return;
    }
    let browser = if snapshot.info.product.is_empty() {
        "Chrome".to_owned()
    } else {
        snapshot.info.product.clone()
    };
    respond_json(
        stream,
        200,
        "OK",
        &json!({
            "Browser": browser,
            "Protocol-Version": "1.3",
            "User-Agent": snapshot.info.user_agent,
            "V8-Version": "",
            "WebKit-Version": "",
            "webSocketDebuggerUrl": format!("ws://127.0.0.1:{}/cdp?token={}", shared.port, shared.token),
            "dialRelayVersion": crate::build_info::VERSION,
            "dialRelayProtocol": PROTOCOL_VERSION,
            "dialExtensionVersion": snapshot.info.version,
        }),
    );
}

fn json_list(stream: &mut TcpStream, shared: &Shared) {
    let snapshot = shared.relay.snapshot();
    if !snapshot.connected {
        respond_not_connected(stream, &snapshot);
        return;
    }
    respond_json(stream, 200, "OK", &Value::Array(snapshot.targets));
}

fn respond_not_connected(stream: &mut TcpStream, snapshot: &Snapshot) {
    let mut body = json!({
        "error": NOT_CONNECTED_MESSAGE,
        "dialRelayVersion": crate::build_info::VERSION,
        "dialRelayProtocol": PROTOCOL_VERSION,
        "extensionSeen": snapshot.extension_seen,
        "uptimeMs": snapshot.uptime.as_millis() as u64,
        "disconnectedMs": snapshot.disconnected_for.map(|d| d.as_millis() as u64),
    });
    if let Some(last) = &snapshot.last_error {
        body["lastError"] = json!(last);
    }
    respond_json(stream, 503, "Service Unavailable", &body);
}

fn cdp_endpoint(mut stream: TcpStream, head: &Head, shared: &Shared) {
    if head.header("origin").is_some() {
        respond_json(
            &mut stream,
            403,
            "Forbidden",
            &json!({"error": "CDP clients must not send an Origin header"}),
        );
        return;
    }
    if !shared.relay.snapshot().connected {
        respond_not_connected(&mut stream, &shared.relay.snapshot());
        return;
    }
    let Some(mut ws) = upgrade(&mut stream, head) else {
        respond_json(
            &mut stream,
            426,
            "Upgrade Required",
            &json!({"error": "/cdp is a WebSocket endpoint"}),
        );
        return;
    };
    serve_cdp_client(&mut ws, shared);
}

fn extension_endpoint(mut stream: TcpStream, head: &Head, shared: &Shared) {
    let origin_ok = head
        .header("origin")
        .is_some_and(|origin| origin.starts_with(EXTENSION_ORIGIN_PREFIX));
    if !origin_ok {
        respond_json(
            &mut stream,
            403,
            "Forbidden",
            &json!({"error": "only the Dial browser extension may connect to /ext"}),
        );
        return;
    }
    let Some(mut ws) = upgrade(&mut stream, head) else {
        respond_json(
            &mut stream,
            426,
            "Upgrade Required",
            &json!({"error": "/ext is a WebSocket endpoint"}),
        );
        return;
    };
    serve_extension(&mut ws, shared);
}

/// Completes the WebSocket handshake and switches the socket to short poll timeouts.
fn upgrade(stream: &mut TcpStream, head: &Head) -> Option<WebSocket<TcpStream>> {
    if !head.is_websocket_upgrade() {
        return None;
    }
    let key = head.header("sec-websocket-key")?;
    let accept = derive_accept_key(key.as_bytes());
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    stream.write_all(response.as_bytes()).ok()?;
    stream.set_read_timeout(Some(POLL)).ok()?;
    // Blocked writes return after one poll so queued output is measured rather than pinning the thread.
    stream.set_write_timeout(Some(POLL)).ok()?;
    let _ = stream.set_nodelay(true);
    Some(WebSocket::from_raw_socket(
        stream.try_clone().ok()?,
        Role::Server,
        Some(socket_config()),
    ))
}

enum Incoming {
    Text(String),
    Idle,
    Closed,
}

fn poll(ws: &mut WebSocket<TcpStream>) -> Incoming {
    match ws.read() {
        Ok(Message::Text(text)) => Incoming::Text(text.to_string()),
        Ok(Message::Close(_)) => Incoming::Closed,
        Ok(_) => Incoming::Idle,
        Err(tungstenite::Error::Io(error)) if is_would_block(&error) => Incoming::Idle,
        Err(_) => Incoming::Closed,
    }
}

fn is_would_block(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

/// Queues one text frame. A full socket buffer counts as queued; `flush` pushes it out later.
fn send_text(ws: &mut WebSocket<TcpStream>, text: String) -> Result<(), tungstenite::Error> {
    match ws.send(Message::Text(text.into())) {
        Err(tungstenite::Error::Io(error)) if is_would_block(&error) => Ok(()),
        other => other,
    }
}

/// Pushes buffered output. `Ok(false)` means the peer is not reading yet.
fn flush(ws: &mut WebSocket<TcpStream>) -> Result<bool, tungstenite::Error> {
    match ws.flush() {
        Ok(()) => Ok(true),
        Err(tungstenite::Error::Io(error)) if is_would_block(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

/// Inbound limits for one socket. Outbound limits are the bounded queue in `outbox`.
fn socket_config() -> WebSocketConfig {
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(MAX_MESSAGE_BYTES);
    config.max_frame_size = Some(MAX_MESSAGE_BYTES);
    config.max_write_buffer_size = outbox::MAX_QUEUED_BYTES + 1024 * 1024;
    config
}

/// Writes queued frames while the socket accepts them. Returns false when this connection must
/// end: its queue overflowed, a write failed, or the peer stopped reading for `WRITE_STALL`.
fn pump(ws: &mut WebSocket<TcpStream>, rx: &OutboxReceiver, stalled: &mut Option<Instant>) -> bool {
    loop {
        if rx.is_overloaded() {
            return false;
        }
        match flush(ws) {
            Ok(true) => *stalled = None,
            Ok(false) => {
                // Output the peer is not reading stays buffered; take nothing more until it drains.
                let since = *stalled.get_or_insert_with(Instant::now);
                return since.elapsed() < WRITE_STALL;
            }
            Err(_) => return false,
        }
        let Ok(frame) = rx.try_recv() else {
            return true;
        };
        if send_text(ws, frame).is_err() {
            return false;
        }
    }
}

/// One running CDP request. It counts against the client's cap until the request ends, including
/// when its thread panics.
struct InFlight(Arc<AtomicUsize>);

impl InFlight {
    fn claim(counter: &Arc<AtomicUsize>) -> Option<Self> {
        if counter.fetch_add(1, Ordering::SeqCst) >= MAX_IN_FLIGHT_PER_CLIENT {
            counter.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Self(Arc::clone(counter)))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn serve_cdp_client(ws: &mut WebSocket<TcpStream>, shared: &Shared) {
    let (tx, rx) = outbox::channel();
    let client = shared.relay.connect_client(tx.clone());
    let in_flight = Arc::new(AtomicUsize::new(0));
    let mut stalled = None;
    loop {
        if !pump(ws, &rx, &mut stalled) {
            break;
        }
        match poll(ws) {
            Incoming::Text(text) => match serde_json::from_str::<Value>(&text) {
                Ok(request) => {
                    let Some(slot) = InFlight::claim(&in_flight) else {
                        // Explicit overload: the request never starts, and the caller learns why.
                        if let Some(id) = request.get("id").cloned() {
                            let error = json!({"code": -32000, "message": BUSY_MESSAGE});
                            let _ = tx.send(json!({"id": id, "error": error}).to_string());
                        }
                        continue;
                    };
                    // Runs on its own thread. A command such as `Page.navigate` waits for the
                    // extension, whose answer arrives on this same socket, so the reader must keep
                    // reading. Replies share the event queue, which keeps them in order.
                    let relay = Arc::clone(&shared.relay);
                    let reply = tx.clone();
                    thread::spawn(move || {
                        let _slot = slot;
                        if let Some(response) = relay.handle_cdp(client, &request) {
                            let _ = reply.send(response);
                        }
                    });
                }
                Err(error) => {
                    let _ = tx.send(
                        json!({
                            "id": null,
                            "error": {"code": -32700, "message": format!("invalid JSON: {error}")},
                        })
                        .to_string(),
                    );
                }
            },
            Incoming::Idle => {}
            Incoming::Closed => break,
        }
    }
    shared.relay.disconnect_client(client);
    let _ = ws.close(None);
    let _ = ws.flush();
}

fn serve_extension(ws: &mut WebSocket<TcpStream>, shared: &Shared) {
    let (tx, rx) = outbox::channel();
    let mut link: Option<u64> = None;
    let mut last_seen = Instant::now();
    let mut stalled = None;
    loop {
        if !pump(ws, &rx, &mut stalled) {
            break;
        }
        match poll(ws) {
            Incoming::Text(text) => {
                last_seen = Instant::now();
                let message = match serde_json::from_str::<ExtMessage>(&text) {
                    Ok(message) => message,
                    Err(_) => continue,
                };
                match link {
                    None => match shared.relay.extension_hello(tx.clone(), message) {
                        Ok(id) => {
                            link = Some(id);
                            let relay = Arc::clone(&shared.relay);
                            thread::spawn(move || relay.auto_attach_tabs(None));
                        }
                        Err(reason) => {
                            let _ = ws.close(Some(tungstenite::protocol::CloseFrame {
                                code: tungstenite::protocol::frame::coding::CloseCode::Policy,
                                reason: reason.into(),
                            }));
                            let _ = ws.flush();
                            return;
                        }
                    },
                    Some(id) => {
                        let tabs = shared.relay.extension_message(id, message);
                        if !tabs.is_empty() {
                            let relay = Arc::clone(&shared.relay);
                            thread::spawn(move || relay.auto_attach_tabs(Some(tabs)));
                        }
                    }
                }
            }
            Incoming::Idle => {
                if last_seen.elapsed() > EXTENSION_SILENCE {
                    break;
                }
            }
            Incoming::Closed => break,
        }
    }
    if let Some(id) = link {
        shared.relay.extension_disconnected(id);
    }
    let _ = ws.close(None);
    let _ = ws.flush();
}

fn respond_json(stream: &mut TcpStream, status: u16, reason: &str, body: &Value) {
    let text = body.to_string();
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        text.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(text.as_bytes());
    let _ = stream.flush();
}

#[cfg(test)]
mod tests;

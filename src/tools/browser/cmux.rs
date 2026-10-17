//! cmux browser-host client.
//!
//! cmux backend: kind resolution and the socket client
//! (wire protocol). The protocol is newline-delimited JSON over a Unix socket.
//! Loopback `127.0.0.1:<port>` / `localhost:<port>` targets use TCP and must pass
//! the HMAC relay handshake. Requests are serialized over one connection, and a
//! connection that times out or desyncs is dropped and re-established on the next
//! request.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use ring::hmac;
use serde_json::{Map, Value, json};
use thiserror::Error;

/// Connect (and relay/password handshake) budget.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default per-request budget.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

const MAX_LINE_BYTES: usize = 16 << 20;
const AUTH_UNKNOWN_COMMAND: &str = "Unknown command 'auth'";
const RELAY_PROTOCOL: &str = "cmux-relay-auth";

/// Parses a boolean env/setting flag: missing or
/// empty values yield `default`; only the truthy spellings yield `true`.
pub fn parse_flag(value: Option<&str>, default: bool) -> bool {
    match value {
        None | Some("") => default,
        Some(v) => matches!(
            v,
            "1" | "Y" | "y" | "TRUE" | "true" | "YES" | "yes" | "ON" | "on"
        ),
    }
}

/// A resolved cmux browser host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmuxKind {
    /// Unix socket path, or `127.0.0.1:<port>` / `localhost:<port>` for relay TCP.
    pub socket_path: String,
    /// Socket password (`CMUX_SOCKET_PASSWORD`), sent as `auth <password>`.
    pub password: Option<String>,
    /// Default surface for this tab, if the caller already has one.
    pub surface: Option<String>,
}

/// Inputs to [`resolve_cmux_kind`].
#[derive(Debug, Clone, Default)]
pub struct ResolveCmuxKindOptions {
    pub surface: Option<String>,
    /// Setting value; `None` means enabled (the default).
    pub setting_enabled: Option<bool>,
}

/// Resolves the cmux host from `DIAL_BROWSER_CMUX`, `CMUX_SOCKET_PATH`, and
/// `CMUX_SOCKET_PASSWORD`. Returns `None` when disabled or no socket path is set.
pub fn resolve_cmux_kind(
    options: &ResolveCmuxKindOptions,
    env: impl Fn(&str) -> Option<String>,
) -> Option<CmuxKind> {
    let enabled = parse_flag(
        env("DIAL_BROWSER_CMUX").as_deref(),
        options.setting_enabled.unwrap_or(true),
    );
    if !enabled {
        return None;
    }
    let socket_path = env("CMUX_SOCKET_PATH").filter(|value| !value.is_empty())?;
    let password = env("CMUX_SOCKET_PASSWORD").filter(|value| !value.is_empty());
    Some(CmuxKind {
        socket_path,
        password,
        surface: options.surface.clone(),
    })
}

/// Everything the client needs to reach a cmux socket.
#[derive(Clone)]
pub struct CmuxClientConfig {
    pub socket_path: String,
    pub password: Option<String>,
    /// `CMUX_RELAY_ID`, used only for loopback TCP relay targets.
    pub relay_id: Option<String>,
    /// `CMUX_RELAY_TOKEN` (hex), used only for loopback TCP relay targets.
    pub relay_token: Option<String>,
    /// Home directory for the `~/.cmux/relay/<port>.auth` fallback.
    pub home_dir: Option<PathBuf>,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
}

impl CmuxClientConfig {
    /// Builds the config from a resolved kind plus `CMUX_RELAY_ID`, `CMUX_RELAY_TOKEN`, `HOME`.
    pub fn from_kind(kind: &CmuxKind, env: impl Fn(&str) -> Option<String>) -> Self {
        Self {
            socket_path: kind.socket_path.clone(),
            password: kind.password.clone(),
            relay_id: env("CMUX_RELAY_ID").filter(|value| !value.is_empty()),
            relay_token: env("CMUX_RELAY_TOKEN").filter(|value| !value.is_empty()),
            home_dir: env("HOME").map(PathBuf::from),
            connect_timeout: CONNECT_TIMEOUT,
            request_timeout: REQUEST_TIMEOUT,
        }
    }
}

/// A cmux-side error reply (`{"ok": false, "error": {...}}`).
#[derive(Debug, Clone, PartialEq)]
pub struct CmuxRemoteError {
    pub code: String,
    pub message: String,
    pub details: Option<Value>,
}

impl fmt::Display for CmuxRemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)?;
        if let Some(details) = &self.details {
            write!(f, " details={details}")?;
        }
        Ok(())
    }
}

impl std::error::Error for CmuxRemoteError {}

#[derive(Debug, Error)]
pub enum CmuxError {
    #[error("Failed to connect to cmux socket at {path}: {message}")]
    Connect { path: String, message: String },
    #[error("{0}")]
    Auth(String),
    #[error("Timed out waiting for cmux socket response")]
    Timeout,
    #[error("cmux socket closed before responding")]
    Closed,
    #[error("cmux socket I/O failed: {0}")]
    Io(String),
    #[error("{0}")]
    Protocol(String),
    /// An `ERROR:` line from the server (plain-text protocol error).
    #[error("{0}")]
    Socket(String),
    #[error(transparent)]
    Remote(#[from] CmuxRemoteError),
}

impl CmuxError {
    /// Errors after which the request/response stream can no longer be trusted.
    fn drops_connection(&self) -> bool {
        matches!(
            self,
            Self::Timeout | Self::Closed | Self::Io(_) | Self::Protocol(_)
        )
    }
}

/// Result of `browser.open_split`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSplit {
    pub surface_id: String,
    pub url: Option<String>,
}

enum Stream {
    #[cfg(unix)]
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Stream {
    fn set_timeouts(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => {
                stream.set_read_timeout(timeout)?;
                stream.set_write_timeout(timeout)
            }
            Self::Tcp(stream) => {
                stream.set_read_timeout(timeout)?;
                stream.set_write_timeout(timeout)
            }
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.read(buf),
            Self::Tcp(stream) => stream.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.write(buf),
            Self::Tcp(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.flush(),
            Self::Tcp(stream) => stream.flush(),
        }
    }
}

struct Connection {
    stream: Stream,
    pending: Vec<u8>,
}

impl Connection {
    fn write_line(&mut self, line: &str, deadline: Instant) -> Result<(), CmuxError> {
        self.stream
            .set_timeouts(Some(remaining_time(deadline)?))
            .map_err(io_error)?;
        let mut framed = Vec::with_capacity(line.len() + 1);
        framed.extend_from_slice(line.as_bytes());
        framed.push(b'\n');
        self.stream.write_all(&framed).map_err(io_error)?;
        self.stream.flush().map_err(io_error)
    }

    fn read_line(&mut self, deadline: Instant) -> Result<String, CmuxError> {
        loop {
            if let Some(pos) = self.pending.iter().position(|byte| *byte == b'\n') {
                let rest = self.pending.split_off(pos + 1);
                let mut line = std::mem::replace(&mut self.pending, rest);
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return String::from_utf8(line).map_err(|_| {
                    CmuxError::Protocol("cmux socket response is not valid UTF-8".to_owned())
                });
            }
            if self.pending.len() > MAX_LINE_BYTES {
                return Err(CmuxError::Protocol(
                    "cmux socket response exceeds 16 MiB".to_owned(),
                ));
            }
            self.stream
                .set_timeouts(Some(remaining_time(deadline)?))
                .map_err(io_error)?;
            let mut chunk = [0u8; 8192];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err(CmuxError::Closed),
                Ok(n) => self.pending.extend_from_slice(&chunk[..n]),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted
                            | io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => return Err(CmuxError::Io(error.to_string())),
            }
        }
    }

    fn exchange(&mut self, line: &str, deadline: Instant) -> Result<Value, CmuxError> {
        self.write_line(line, deadline)?;
        let reply = self.read_line(deadline)?;
        parse_reply(&reply)
    }

    fn authenticate_password(
        &mut self,
        password: &str,
        deadline: Instant,
    ) -> Result<(), CmuxError> {
        self.write_line(&format!("auth {password}"), deadline)?;
        let reply = self.read_line(deadline)?;
        let trimmed = reply.trim();
        if trimmed.starts_with("ERROR:") && !trimmed.contains(AUTH_UNKNOWN_COMMAND) {
            return Err(CmuxError::Auth(trimmed.to_owned()));
        }
        Ok(())
    }

    fn authenticate_relay(
        &mut self,
        credentials: &RelayCredentials,
        port: u16,
        deadline: Instant,
    ) -> Result<(), CmuxError> {
        let endpoint = format!("127.0.0.1:{port}");
        let invalid = || {
            CmuxError::Auth(format!(
                "Invalid cmux relay authentication challenge from {endpoint}"
            ))
        };

        let challenge_line = self.read_line(deadline)?;
        let challenge: Value = serde_json::from_str(&challenge_line).map_err(|_| invalid())?;
        let protocol = challenge.get("protocol").and_then(Value::as_str);
        let version = challenge.get("version").and_then(Value::as_i64);
        let relay_id = challenge.get("relay_id").and_then(Value::as_str);
        let nonce = challenge
            .get("nonce")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        let (Some(version), Some(relay_id), Some(nonce)) = (version, relay_id, nonce) else {
            return Err(invalid());
        };
        if protocol != Some(RELAY_PROTOCOL) || relay_id != credentials.relay_id {
            return Err(invalid());
        }

        let message = format!("relay_id={relay_id}\nnonce={nonce}\nversion={version}");
        let key = hmac::Key::new(hmac::HMAC_SHA256, &credentials.relay_token);
        let tag = hmac::sign(&key, message.as_bytes());
        let auth_line = json!({
            "relay_id": credentials.relay_id,
            "mac": crate::hex(tag.as_ref()),
        })
        .to_string();
        self.write_line(&auth_line, deadline)?;
        let reply = self.read_line(deadline)?;
        let accepted = serde_json::from_str::<Value>(&reply)
            .ok()
            .and_then(|value| value.get("ok").and_then(Value::as_bool))
            == Some(true);
        if accepted {
            Ok(())
        } else {
            Err(CmuxError::Auth(format!(
                "Cmux relay authentication failed for {endpoint}"
            )))
        }
    }
}

/// Relay HMAC credentials. Deliberately has no `Debug` so the token never gets logged.
struct RelayCredentials {
    relay_id: String,
    relay_token: Vec<u8>,
}

fn parse_relay_credentials(
    relay_id: Option<&str>,
    relay_token_hex: Option<&str>,
) -> Option<RelayCredentials> {
    let relay_id = relay_id?.trim();
    let relay_token_hex = relay_token_hex?.trim();
    if relay_id.is_empty() || relay_token_hex.is_empty() {
        return None;
    }
    let relay_token = decode_hex(relay_token_hex)?;
    Some(RelayCredentials {
        relay_id: relay_id.to_owned(),
        relay_token,
    })
}

fn load_relay_credentials(
    config: &CmuxClientConfig,
    port: u16,
) -> Result<RelayCredentials, CmuxError> {
    if let Some(credentials) =
        parse_relay_credentials(config.relay_id.as_deref(), config.relay_token.as_deref())
    {
        return Ok(credentials);
    }
    let Some(home) = config.home_dir.as_ref() else {
        return Err(CmuxError::Auth(format!(
            "Missing cmux relay auth metadata for 127.0.0.1:{port}; set CMUX_RELAY_ID/CMUX_RELAY_TOKEN"
        )));
    };
    let path = home
        .join(".cmux")
        .join("relay")
        .join(format!("{port}.auth"));
    let text = std::fs::read_to_string(&path).map_err(|_| {
        CmuxError::Auth(format!(
            "Missing cmux relay auth metadata for 127.0.0.1:{port}; set CMUX_RELAY_ID/CMUX_RELAY_TOKEN or restore {}",
            path.display()
        ))
    })?;
    let invalid = || {
        CmuxError::Auth(format!(
            "Invalid cmux relay auth metadata in {}",
            path.display()
        ))
    };
    let payload: Value = serde_json::from_str(&text).map_err(|_| invalid())?;
    parse_relay_credentials(
        payload.get("relay_id").and_then(Value::as_str),
        payload.get("relay_token").and_then(Value::as_str),
    )
    .ok_or_else(invalid)
}

/// Connects to cmux's Unix domain socket. Windows has no Unix sockets in `std`, so there the
/// connection is refused with a `Connect` error; loopback TCP relay targets are unaffected.
#[cfg(unix)]
fn connect_unix(path: &str) -> Result<Stream, CmuxError> {
    UnixStream::connect(path)
        .map(Stream::Unix)
        .map_err(|error| CmuxError::Connect {
            path: path.to_owned(),
            message: error.to_string(),
        })
}

#[cfg(windows)]
fn connect_unix(path: &str) -> Result<Stream, CmuxError> {
    Err(CmuxError::Connect {
        path: path.to_owned(),
        message: "Unix domain sockets are not available on Windows".to_owned(),
    })
}

/// Port of a loopback `127.0.0.1:<port>` or `localhost:<port>` target, which is relay TCP.
fn loopback_tcp_port(target: &str) -> Option<u16> {
    let (host, port) = target.rsplit_once(':')?;
    if !matches!(host, "127.0.0.1" | "localhost") {
        return None;
    }
    port.parse::<u16>().ok().filter(|port| *port > 0)
}

/// Synchronous cmux socket client. Requests are serialized over one connection.
pub struct CmuxClient {
    config: CmuxClientConfig,
    connection: Mutex<Option<Connection>>,
}

impl CmuxClient {
    pub fn new(config: CmuxClientConfig) -> Self {
        Self {
            config,
            connection: Mutex::new(None),
        }
    }

    /// Sends `{"id", "method", "params"}` and returns the `result` object.
    pub fn request(&self, method: &str, params: Value) -> Result<Value, CmuxError> {
        self.request_with_timeout(method, params, self.config.request_timeout)
    }

    /// Like [`CmuxClient::request`] with an explicit response budget.
    pub fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CmuxError> {
        let mut guard = self.connection.lock();
        if guard.is_none() {
            *guard = Some(self.connect()?);
        }
        let id = format!("{:032x}", rand::random::<u128>());
        let line = json!({ "id": id, "method": method, "params": params }).to_string();
        let deadline = Instant::now() + timeout;
        let outcome = match guard.as_mut() {
            Some(connection) => connection.exchange(&line, deadline),
            None => Err(CmuxError::Closed),
        };
        if let Err(error) = &outcome
            && error.drops_connection()
        {
            *guard = None;
        }
        outcome
    }

    /// Like [`CmuxClient::request`], with `surface_id` merged into `params`.
    pub fn surface_request(
        &self,
        surface_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, CmuxError> {
        self.surface_request_with_timeout(surface_id, method, params, self.config.request_timeout)
    }

    pub fn surface_request_with_timeout(
        &self,
        surface_id: &str,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CmuxError> {
        let mut merged = match params {
            Value::Object(map) => map,
            Value::Null => Map::new(),
            _ => {
                return Err(CmuxError::Protocol(
                    "cmux request params must be a JSON object".to_owned(),
                ));
            }
        };
        merged.insert(
            "surface_id".to_owned(),
            Value::String(surface_id.to_owned()),
        );
        self.request_with_timeout(method, Value::Object(merged), timeout)
    }

    /// `browser.open_split`; `workspace_id` / `surface_id` are sent only when given.
    pub fn open_split(
        &self,
        url: &str,
        workspace_id: Option<&str>,
        surface_id: Option<&str>,
    ) -> Result<OpenSplit, CmuxError> {
        let mut params = Map::new();
        params.insert("url".to_owned(), json!(url));
        params.insert("focus".to_owned(), json!(false));
        if let Some(workspace_id) = workspace_id {
            params.insert("workspace_id".to_owned(), json!(workspace_id));
        }
        if let Some(surface_id) = surface_id {
            params.insert("surface_id".to_owned(), json!(surface_id));
        }
        let result = self.request("browser.open_split", Value::Object(params))?;
        let surface_id = result
            .get("surface_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                CmuxError::Protocol(
                    "cmux browser.open_split did not return a surface_id".to_owned(),
                )
            })?
            .to_owned();
        let url = result.get("url").and_then(Value::as_str).map(str::to_owned);
        Ok(OpenSplit { surface_id, url })
    }

    /// `surface.close`.
    pub fn close_surface(&self, surface_id: &str) -> Result<Value, CmuxError> {
        self.request("surface.close", json!({ "surface_id": surface_id }))
    }

    /// `browser.navigate`.
    pub fn navigate(&self, surface_id: &str, url: &str) -> Result<Value, CmuxError> {
        self.surface_request(surface_id, "browser.navigate", json!({ "url": url }))
    }

    /// `browser.wait` with `load_state` (`"interactive"` or `"complete"`).
    pub fn wait_for_load(
        &self,
        surface_id: &str,
        load_state: &str,
        timeout_ms: u64,
    ) -> Result<Value, CmuxError> {
        self.surface_request(
            surface_id,
            "browser.wait",
            json!({ "load_state": load_state, "timeout_ms": timeout_ms }),
        )
    }

    /// `browser.url.get`.
    pub fn url(&self, surface_id: &str) -> Result<Value, CmuxError> {
        self.surface_request(surface_id, "browser.url.get", json!({}))
    }

    /// `browser.eval`; the caller reads `value` from the result.
    pub fn eval(&self, surface_id: &str, script: &str) -> Result<Value, CmuxError> {
        self.surface_request(surface_id, "browser.eval", json!({ "script": script }))
    }

    /// `browser.snapshot`.
    pub fn snapshot(
        &self,
        surface_id: &str,
        interactive: bool,
        max_depth: Option<u32>,
    ) -> Result<Value, CmuxError> {
        self.surface_request(
            surface_id,
            "browser.snapshot",
            json!({ "interactive": interactive, "max_depth": max_depth }),
        )
    }

    /// `browser.screenshot`; the result carries `png_base64`.
    pub fn screenshot(&self, surface_id: &str) -> Result<Value, CmuxError> {
        self.surface_request(surface_id, "browser.screenshot", json!({}))
    }

    /// `browser.click` on a selector.
    pub fn click(&self, surface_id: &str, selector: &str) -> Result<Value, CmuxError> {
        self.surface_request(surface_id, "browser.click", json!({ "selector": selector }))
    }

    /// `browser.type` into a selector.
    pub fn type_text(
        &self,
        surface_id: &str,
        selector: &str,
        text: &str,
    ) -> Result<Value, CmuxError> {
        self.surface_request(
            surface_id,
            "browser.type",
            json!({ "selector": selector, "text": text }),
        )
    }

    fn connect(&self) -> Result<Connection, CmuxError> {
        let timeout = self.config.connect_timeout;
        let deadline = Instant::now() + timeout;
        let (stream, relay_port) = match loopback_tcp_port(&self.config.socket_path) {
            Some(port) => {
                let address = SocketAddr::from(([127, 0, 0, 1], port));
                let stream = TcpStream::connect_timeout(&address, timeout).map_err(|error| {
                    CmuxError::Connect {
                        path: self.config.socket_path.clone(),
                        message: error.to_string(),
                    }
                })?;
                (Stream::Tcp(stream), Some(port))
            }
            None => (connect_unix(&self.config.socket_path)?, None),
        };
        let mut connection = Connection {
            stream,
            pending: Vec::new(),
        };
        if let Some(port) = relay_port {
            let credentials = load_relay_credentials(&self.config, port)?;
            connection.authenticate_relay(&credentials, port, deadline)?;
        }
        if let Some(password) = &self.config.password {
            connection.authenticate_password(password, deadline)?;
        }
        Ok(connection)
    }
}

fn remaining_time(deadline: Instant) -> Result<Duration, CmuxError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(CmuxError::Timeout)
}

fn io_error(error: io::Error) -> CmuxError {
    if matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    ) {
        CmuxError::Timeout
    } else {
        CmuxError::Io(error.to_string())
    }
}

/// Parses one response line. `ERROR:` lines are plain-text errors; JSON replies carry `ok`.
fn parse_reply(reply: &str) -> Result<Value, CmuxError> {
    let trimmed = reply.trim();
    if trimmed.starts_with("ERROR:") {
        return Err(CmuxError::Socket(trimmed.to_owned()));
    }
    let parsed: Value = serde_json::from_str(trimmed).map_err(|error| {
        CmuxError::Protocol(format!("Invalid cmux socket JSON response: {error}"))
    })?;
    let Value::Object(reply) = parsed else {
        return Err(CmuxError::Protocol(
            "Invalid cmux socket response: expected object".to_owned(),
        ));
    };
    match reply.get("ok") {
        Some(Value::Bool(true)) => Ok(reply
            .get("result")
            .filter(|value| !value.is_null())
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()))),
        Some(Value::Bool(false)) => Err(CmuxError::Remote(remote_error(&reply))),
        _ => Err(CmuxError::Protocol(
            "Invalid cmux socket response: missing ok flag".to_owned(),
        )),
    }
}

fn remote_error(reply: &Map<String, Value>) -> CmuxRemoteError {
    let payload = reply.get("error").and_then(Value::as_object);
    CmuxRemoteError {
        code: field_text(payload.and_then(|p| p.get("code")), "error"),
        message: field_text(payload.and_then(|p| p.get("message")), "cmux error"),
        details: payload
            .and_then(|p| p.get("details"))
            .filter(|value| !value.is_null())
            .cloned(),
    }
}

fn field_text(value: Option<&Value>, fallback: &str) -> String {
    match value {
        None | Some(Value::Null) => fallback.to_owned(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    #[cfg(unix)]
    use std::os::unix::net::UnixListener;
    use std::thread;

    fn read_line(reader: &mut impl BufRead) -> String {
        let mut line = String::new();
        reader.read_line(&mut line).expect("read request line");
        line
    }

    fn client(socket_path: &str, password: Option<&str>) -> CmuxClient {
        CmuxClient::new(CmuxClientConfig {
            socket_path: socket_path.to_owned(),
            password: password.map(str::to_owned),
            relay_id: None,
            relay_token: None,
            home_dir: None,
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(5),
        })
    }

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn resolve_requires_socket_and_respects_flag() {
        let options = ResolveCmuxKindOptions::default();
        let kind = resolve_cmux_kind(&options, env_of(&[("CMUX_SOCKET_PATH", "/tmp/cmux.sock")]))
            .expect("kind");
        assert_eq!(kind.socket_path, "/tmp/cmux.sock");
        assert_eq!(kind.password, None);
        assert!(resolve_cmux_kind(&options, env_of(&[])).is_none());
        assert!(
            resolve_cmux_kind(
                &options,
                env_of(&[("DIAL_BROWSER_CMUX", "0"), ("CMUX_SOCKET_PATH", "/x")])
            )
            .is_none()
        );
        let disabled_setting = ResolveCmuxKindOptions {
            surface: None,
            setting_enabled: Some(false),
        };
        assert!(
            resolve_cmux_kind(&disabled_setting, env_of(&[("CMUX_SOCKET_PATH", "/x")])).is_none()
        );
        let empty_password = resolve_cmux_kind(
            &options,
            env_of(&[("CMUX_SOCKET_PATH", "/x"), ("CMUX_SOCKET_PASSWORD", "")]),
        )
        .expect("kind");
        assert_eq!(empty_password.password, None);
    }

    #[test]
    fn parse_flag_truthy_set() {
        assert!(parse_flag(Some("yes"), false));
        assert!(parse_flag(Some("1"), false));
        assert!(!parse_flag(Some("0"), true));
        assert!(parse_flag(None, true));
        assert!(!parse_flag(Some(""), false));
    }

    #[cfg(unix)]
    #[test]
    fn request_round_trips_method_params_and_result() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cmux.sock").to_string_lossy().into_owned();
        let listener = UnixListener::bind(&path).expect("bind");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut writer = stream;
            let request: Value =
                serde_json::from_str(&read_line(&mut reader)).expect("json request");
            assert_eq!(request["method"], "browser.url.get");
            assert_eq!(request["params"]["surface_id"], "surf-1");
            assert!(request["id"].as_str().is_some_and(|id| !id.is_empty()));
            writeln!(
                writer,
                r#"{{"id":"ignored","ok":true,"result":{{"url":"https://example.test/"}}}}"#
            )
            .expect("reply");
        });
        let result = client(&path, None)
            .surface_request("surf-1", "browser.url.get", json!({}))
            .expect("result");
        assert_eq!(result["url"], "https://example.test/");
        server.join().expect("server");
    }

    #[cfg(unix)]
    #[test]
    fn error_reply_surfaces_as_typed_remote_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cmux.sock").to_string_lossy().into_owned();
        let listener = UnixListener::bind(&path).expect("bind");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut writer = stream;
            let _ = read_line(&mut reader);
            writeln!(
                writer,
                r#"{{"ok":false,"error":{{"code":"not_found","message":"no surface","details":{{"surface_id":"surf-9"}}}}}}"#
            )
            .expect("reply");
        });
        let error = client(&path, None)
            .surface_request("surf-9", "browser.snapshot", json!({}))
            .expect_err("error");
        match error {
            CmuxError::Remote(remote) => {
                assert_eq!(remote.code, "not_found");
                assert_eq!(remote.message, "no surface");
                assert_eq!(
                    remote.to_string(),
                    r#"not_found: no surface details={"surface_id":"surf-9"}"#
                );
            }
            other => panic!("expected remote error, got {other:?}"),
        }
        server.join().expect("server");
    }

    #[cfg(unix)]
    #[test]
    fn password_is_sent_before_first_request_and_err_lines_are_socket_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cmux.sock").to_string_lossy().into_owned();
        let listener = UnixListener::bind(&path).expect("bind");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut writer = stream;
            assert_eq!(read_line(&mut reader).trim_end(), "auth s3cret");
            writeln!(writer, "OK").expect("auth reply");
            let _ = read_line(&mut reader);
            writeln!(writer, "ERROR: surface missing").expect("error reply");
        });
        let error = client(&path, Some("s3cret"))
            .request("surface.close", json!({"surface_id": "gone"}))
            .expect_err("socket error");
        assert!(matches!(&error, CmuxError::Socket(text) if text == "ERROR: surface missing"));
        server.join().expect("server");
    }

    #[test]
    fn relay_tcp_target_completes_hmac_handshake() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind tcp");
        let port = listener.local_addr().expect("addr").port();
        let token = "00ff10";
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut writer = stream;
            writeln!(
                writer,
                r#"{{"protocol":"cmux-relay-auth","version":1,"relay_id":"relay-1","nonce":"n-42"}}"#
            )
            .expect("challenge");
            let auth: Value = serde_json::from_str(&read_line(&mut reader)).expect("auth json");
            assert_eq!(auth["relay_id"], "relay-1");
            let mac = decode_hex(auth["mac"].as_str().expect("mac")).expect("mac hex");
            let key = hmac::Key::new(hmac::HMAC_SHA256, &decode_hex(token).expect("token"));
            hmac::verify(&key, b"relay_id=relay-1\nnonce=n-42\nversion=1", &mac)
                .expect("valid mac");
            writeln!(writer, r#"{{"ok":true}}"#).expect("auth ok");
            let _ = read_line(&mut reader);
            writeln!(writer, r#"{{"ok":true,"result":{{"surface_id":"s-1"}}}}"#).expect("result");
        });
        let config = CmuxClientConfig {
            socket_path: format!("127.0.0.1:{port}"),
            password: None,
            relay_id: Some("relay-1".to_owned()),
            relay_token: Some(token.to_owned()),
            home_dir: None,
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(5),
        };
        let result = CmuxClient::new(config)
            .request("browser.open_split", json!({"url": "about:blank"}))
            .expect("result");
        assert_eq!(result["surface_id"], "s-1");
        server.join().expect("server");
    }

    #[cfg(unix)]
    #[test]
    fn open_split_requires_surface_id_in_result() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cmux.sock").to_string_lossy().into_owned();
        let listener = UnixListener::bind(&path).expect("bind");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut writer = stream;
            let request: Value = serde_json::from_str(&read_line(&mut reader)).expect("json");
            assert_eq!(request["params"]["focus"], false);
            writeln!(
                writer,
                r#"{{"ok":true,"result":{{"surface_id":"s-7","url":"about:blank"}}}}"#
            )
            .expect("reply");
        });
        let opened = client(&path, None)
            .open_split("about:blank", None, None)
            .expect("open");
        assert_eq!(
            opened,
            OpenSplit {
                surface_id: "s-7".to_owned(),
                url: Some("about:blank".to_owned())
            }
        );
        server.join().expect("server");
    }
}

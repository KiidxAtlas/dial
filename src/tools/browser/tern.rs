//! Tern browser-host client (`TERN_PANE_SOCKET`).
//!
//! Frames are a `u32` little-endian length followed by one UTF-8 JSON object.
//!
//! All I/O is blocking std I/O. Call from a blocking context.

use std::io::{self, ErrorKind, Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Value, json};

use super::cmux::parse_flag;

/// Windows has no Unix domain socket in `std`. This stand-in is uninhabited, so no connection can
/// exist there: `connect` fails with `Unsupported` before any value is built.
#[cfg(windows)]
enum UnixStream {}

#[cfg(windows)]
impl UnixStream {
    fn connect<P: AsRef<std::path::Path>>(_path: P) -> io::Result<Self> {
        Err(io::Error::new(
            ErrorKind::Unsupported,
            "Unix domain sockets are not available on Windows",
        ))
    }

    fn set_read_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
        match *self {}
    }

    fn set_write_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
        match *self {}
    }
}

#[cfg(windows)]
impl Read for UnixStream {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        match *self {}
    }
}

#[cfg(windows)]
impl Write for UnixStream {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        match *self {}
    }

    fn flush(&mut self) -> io::Result<()> {
        match *self {}
    }
}

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Frames larger than this are a protocol error (the daemon's own cap).
pub const MAX_FRAME_BYTES: usize = 256 << 20;
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// The Tern pane Dial runs in: `TERN_PANE_SOCKET` plus `TERN_PANE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TernKind {
    /// The Tern daemon socket (`TERN_PANE_SOCKET`).
    pub socket_path: String,
    /// The pane Dial runs in (`TERN_PANE`), which owns the PiPs.
    pub pane: u64,
}

/// Inputs of [`resolve_tern_kind`].
#[derive(Debug, Default, Clone, Copy)]
pub struct ResolveTernKindOptions {
    /// `browser.tern` setting value; `None` means enabled (the default).
    pub setting_enabled: Option<bool>,
}

/// Resolves the Tern pane from `TERN_PANE_SOCKET` and `TERN_PANE`.
/// Returns `None` when either is missing or `TERN_PANE` is not a block id.
pub fn resolve_tern_pane(env: impl Fn(&str) -> Option<String>) -> Option<TernKind> {
    let socket_path = env("TERN_PANE_SOCKET")?.trim().to_owned();
    let pane = env("TERN_PANE")?;
    let pane = pane.trim();
    if socket_path.is_empty() || pane.is_empty() || !pane.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let pane: u64 = pane.parse().ok()?;
    if pane > MAX_SAFE_INTEGER {
        return None;
    }
    Some(TernKind { socket_path, pane })
}

/// Resolves the Tern browser kind. `DIAL_BROWSER_TERN` overrides the setting in both
/// directions. Returns `None` when Tern mode is off or Dial is not in a Tern pane.
pub fn resolve_tern_kind(
    options: &ResolveTernKindOptions,
    env: impl Fn(&str) -> Option<String>,
) -> Option<TernKind> {
    if !parse_flag(
        env("DIAL_BROWSER_TERN").as_deref(),
        options.setting_enabled.unwrap_or(true),
    ) {
        return None;
    }
    resolve_tern_pane(env)
}

/// Error kinds a Tern request answers with, plus the client's own failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TernErrorKind {
    Invalid,
    NotFound,
    NotAgent,
    NoWindow,
    WindowClosed,
    Unsupported,
    Js,
    Failed,
    /// The daemon is not reachable, or it cannot read Dial's hello.
    Connect,
    /// The connection dropped.
    Closed,
    Timeout,
    Protocol,
}

impl TernErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::NotFound => "not_found",
            Self::NotAgent => "not_agent",
            Self::NoWindow => "no_window",
            Self::WindowClosed => "window_closed",
            Self::Unsupported => "unsupported",
            Self::Js => "js",
            Self::Failed => "failed",
            Self::Connect => "connect",
            Self::Closed => "closed",
            Self::Timeout => "timeout",
            Self::Protocol => "protocol",
        }
    }

    /// Maps the protocol's `error.kind`. Unknown kinds are `failed`.
    fn from_protocol(kind: &str) -> Self {
        match kind {
            "invalid" => Self::Invalid,
            "not_found" => Self::NotFound,
            "not_agent" => Self::NotAgent,
            "no_window" => Self::NoWindow,
            "window_closed" => Self::WindowClosed,
            "unsupported" => Self::Unsupported,
            "js" => Self::Js,
            _ => Self::Failed,
        }
    }
}

/// A failed Tern request or connection, carrying the protocol's error kind.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct TernError {
    pub kind: TernErrorKind,
    pub message: String,
}

impl TernError {
    fn new(kind: TernErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Whether this error says Tern cannot host a browser right now
    /// (auto mode falls back to Chromium).
    pub fn is_unavailable(&self) -> bool {
        matches!(
            self.kind,
            TernErrorKind::NoWindow | TernErrorKind::Unsupported | TernErrorKind::Connect
        )
    }
}

/// Direction of a `fork` request: where the new pane goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkDir {
    Right,
    Down,
}

impl ForkDir {
    fn as_str(self) -> &'static str {
        match self {
            Self::Right => "right",
            Self::Down => "down",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Channel {
    Browser,
    Fork,
}

impl Channel {
    fn key(self) -> &'static str {
        match self {
            Self::Browser => "browser",
            Self::Fork => "fork",
        }
    }
}

#[derive(Debug)]
enum Reply {
    Welcome {
        ops: Vec<String>,
    },
    Answer {
        channel: Channel,
        id: u64,
        answer: Value,
    },
    Other,
}

fn io_error(error: io::Error) -> TernError {
    TernError::new(
        TernErrorKind::Closed,
        format!("Tern connection failed: {error}"),
    )
}

fn remaining(deadline: Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
}

/// `message` as a frame: `u32` LE length, then its JSON.
fn encode_frame(message: &Value) -> Result<Vec<u8>, TernError> {
    let text = serde_json::to_vec(message).map_err(|error| {
        TernError::new(
            TernErrorKind::Invalid,
            format!("Tern request cannot be encoded: {error}"),
        )
    })?;
    if text.len() > MAX_FRAME_BYTES {
        return Err(TernError::new(
            TernErrorKind::Invalid,
            format!(
                "Tern request is {} bytes (limit {MAX_FRAME_BYTES})",
                text.len()
            ),
        ));
    }
    let mut frame = Vec::with_capacity(4 + text.len());
    frame.extend_from_slice(&(text.len() as u32).to_le_bytes());
    frame.extend_from_slice(&text);
    Ok(frame)
}

/// Removes and returns one complete payload from `buffered`, if present.
fn take_frame(buffered: &mut Vec<u8>) -> Result<Option<Vec<u8>>, TernError> {
    if buffered.len() < 4 {
        return Ok(None);
    }
    let length = u32::from_le_bytes([buffered[0], buffered[1], buffered[2], buffered[3]]) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(TernError::new(
            TernErrorKind::Protocol,
            format!("Tern sent a {length}-byte frame (limit {MAX_FRAME_BYTES})"),
        ));
    }
    if buffered.len() < 4 + length {
        return Ok(None);
    }
    let rest = buffered.split_off(4 + length);
    let mut frame = std::mem::replace(buffered, rest);
    Ok(Some(frame.split_off(4)))
}

fn decode_reply(payload: &[u8]) -> Result<Reply, TernError> {
    let reply: Value = serde_json::from_slice(payload).map_err(|error| {
        TernError::new(
            TernErrorKind::Protocol,
            format!("Tern sent a reply that is not JSON: {error}"),
        )
    })?;
    let Some(object) = reply.as_object() else {
        return Err(TernError::new(
            TernErrorKind::Protocol,
            "Tern sent a reply that is not a JSON object",
        ));
    };
    if let Some(welcome) = object.get("welcome") {
        let ops = welcome
            .get("ops")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        return Ok(Reply::Welcome { ops });
    }
    let channel = if object.contains_key("browser") {
        Channel::Browser
    } else if object.contains_key("fork") {
        Channel::Fork
    } else {
        return Ok(Reply::Other);
    };
    let id = object.get("id").and_then(Value::as_u64).ok_or_else(|| {
        TernError::new(
            TernErrorKind::Protocol,
            format!("Tern sent a {} answer without an id", channel.key()),
        )
    })?;
    let answer = object.get(channel.key()).cloned().unwrap_or(Value::Null);
    Ok(Reply::Answer {
        channel,
        id,
        answer,
    })
}

/// The answer's `ok` result. Its `error` (or a malformed answer) becomes a [`TernError`].
fn answer_of(label: &str, answer: &Value) -> Result<Value, TernError> {
    if let Some(ok) = answer.get("ok") {
        return Ok(ok.clone());
    }
    if let Some(error) = answer.get("error").and_then(Value::as_object) {
        let kind = error
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("failed");
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("failed");
        return Err(TernError::new(
            TernErrorKind::from_protocol(kind),
            format!("Tern {label} failed ({kind}): {message}"),
        ));
    }
    Err(TernError::new(
        TernErrorKind::Protocol,
        format!("Tern answered {label} without \"ok\" or \"error\""),
    ))
}

fn block_of(result: &Value, label: &str) -> Result<u64, TernError> {
    result.get("block").and_then(Value::as_u64).ok_or_else(|| {
        TernError::new(
            TernErrorKind::Protocol,
            format!("Tern answered {label} without a \"block\""),
        )
    })
}

/// One Unix-socket connection with its read buffer.
struct Connection {
    stream: UnixStream,
    buffered: Vec<u8>,
}

impl Connection {
    fn send(&mut self, message: &Value, deadline: Instant) -> Result<(), TernError> {
        let frame = encode_frame(message)?;
        let Some(wait) = remaining(deadline) else {
            return Err(TernError::new(
                TernErrorKind::Timeout,
                "Tern request timed out before it was sent",
            ));
        };
        self.stream
            .set_write_timeout(Some(wait))
            .map_err(io_error)?;
        self.stream.write_all(&frame).map_err(io_error)
    }

    /// The next complete payload, or `None` when `deadline` passes first.
    fn next_payload(&mut self, deadline: Instant) -> Result<Option<Vec<u8>>, TernError> {
        let mut chunk = vec![0u8; READ_CHUNK_BYTES];
        loop {
            if let Some(payload) = take_frame(&mut self.buffered)? {
                return Ok(Some(payload));
            }
            let Some(wait) = remaining(deadline) else {
                return Ok(None);
            };
            self.stream.set_read_timeout(Some(wait)).map_err(io_error)?;
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    return Err(TernError::new(
                        TernErrorKind::Closed,
                        "Tern daemon closed the connection",
                    ));
                }
                Ok(read) => self.buffered.extend_from_slice(&chunk[..read]),
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(io_error(error)),
            }
        }
    }
}

/// Waits for the welcome. `None` means the deadline passed first.
fn wait_for_welcome(
    connection: &mut Connection,
    deadline: Instant,
) -> Result<Option<Vec<String>>, TernError> {
    loop {
        let Some(payload) = connection.next_payload(deadline)? else {
            return Ok(None);
        };
        if let Reply::Welcome { ops } = decode_reply(&payload)? {
            return Ok(Some(ops));
        }
    }
}

/// A greeted connection and the request ids it has issued.
struct Session {
    connection: Connection,
    ops: Vec<String>,
    next_id: u64,
}

impl Session {
    /// Sends one request and waits for its answer. Answers for other ids (abandoned ops) are skipped.
    fn run(
        &mut self,
        channel: Channel,
        id: u64,
        body: Value,
        label: &str,
    ) -> Result<Value, TernError> {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let frame = json!({ "id": id, (channel.key()): body });
        self.connection.send(&frame, deadline)?;
        loop {
            let Some(payload) = self.connection.next_payload(deadline)? else {
                return Err(TernError::new(
                    TernErrorKind::Timeout,
                    format!(
                        "Tern {label} timed out after {}ms",
                        REQUEST_TIMEOUT.as_millis()
                    ),
                ));
            };
            match decode_reply(&payload)? {
                Reply::Answer {
                    channel: answered,
                    id: answered_id,
                    answer,
                } if answered_id == id => {
                    if answered != channel {
                        return Err(TernError::new(
                            TernErrorKind::Protocol,
                            format!("Tern answered {label} with a {} answer", answered.key()),
                        ));
                    }
                    return answer_of(label, &answer);
                }
                Reply::Answer { .. } | Reply::Welcome { .. } | Reply::Other => {}
            }
        }
    }
}

/// Client for the Tern daemon. One connection, reused across requests and
/// re-established after a dropped connection or a framing error.
pub struct TernClient {
    socket_path: String,
    session: Mutex<Option<Session>>,
}

impl TernClient {
    pub fn new(kind: &TernKind) -> Self {
        Self {
            socket_path: kind.socket_path.clone(),
            session: Mutex::new(None),
        }
    }

    /// Whether the daemon's welcome lists request kind `op`. Connects if needed.
    pub fn supports(&self, op: &str) -> Result<bool, TernError> {
        let mut guard = self.session.lock();
        self.ensure_session(&mut guard)?;
        Ok(guard
            .as_ref()
            .is_some_and(|session| session.ops.iter().any(|name| name == op)))
    }

    /// Sends one browser op (an object with an `op` field) and returns its `ok` result.
    pub fn request(&self, op: Value) -> Result<Value, TernError> {
        let label = match op.get("op").and_then(Value::as_str) {
            Some(name) => format!("browser {name}"),
            None => "browser op".to_owned(),
        };
        self.exchange(Channel::Browser, op, &label)
    }

    /// Forks the session in pane `block` into a new pane beside it.
    pub fn fork(&self, block: u64, dir: Option<ForkDir>) -> Result<u64, TernError> {
        let mut body = json!({ "block": block });
        if let Some(dir) = dir {
            body["dir"] = json!(dir.as_str());
        }
        let result = self.exchange(Channel::Fork, body, "fork")?;
        block_of(&result, "fork")
    }

    /// Opens a browser picture-in-picture owned by `owner` (the Dial pane). Returns its block.
    pub fn open_tab(&self, owner: u64, width: u32, height: u32) -> Result<u64, TernError> {
        let result = self.request(json!({
            "op": "open",
            "owner": owner,
            "url": "about:blank",
            "width": width,
            "height": height,
        }))?;
        block_of(&result, "browser open")
    }

    pub fn close_tab(&self, block: u64) -> Result<(), TernError> {
        self.request(json!({ "op": "close", "block": block }))?;
        Ok(())
    }

    pub fn goto(&self, block: u64, url: &str) -> Result<Value, TernError> {
        self.request(json!({ "op": "goto", "block": block, "url": url }))
    }

    /// `go` is `back`, `forward`, or `reload`.
    pub fn nav(&self, block: u64, go: &str) -> Result<Value, TernError> {
        self.request(json!({ "op": "nav", "block": block, "go": go }))
    }

    pub fn state(&self, block: u64) -> Result<Value, TernError> {
        self.request(json!({ "op": "state", "block": block }))
    }

    /// `world` is `page` or `isolated`.
    pub fn eval(
        &self,
        block: u64,
        function: &str,
        args: Value,
        world: &str,
    ) -> Result<Value, TernError> {
        self.request(json!({
            "op": "eval",
            "block": block,
            "function": function,
            "args": args,
            "world": world,
        }))
    }

    /// Captures the page as base64 image data.
    pub fn capture(
        &self,
        block: u64,
        format: &str,
        quality: Option<u32>,
    ) -> Result<String, TernError> {
        let mut op = json!({ "op": "capture", "block": block, "format": format, "scale": 1 });
        if let Some(quality) = quality {
            op["quality"] = json!(quality);
        }
        let result = self.request(op)?;
        result
            .get("data")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                TernError::new(
                    TernErrorKind::Protocol,
                    "Tern answered browser capture without \"data\"",
                )
            })
    }

    pub fn events(&self, block: u64, after: u64) -> Result<Value, TernError> {
        self.request(json!({ "op": "events", "block": block, "after": after }))
    }

    fn exchange(&self, channel: Channel, body: Value, label: &str) -> Result<Value, TernError> {
        let mut guard = self.session.lock();
        self.ensure_session(&mut guard)?;
        let outcome = match guard.as_mut() {
            Some(session) => {
                let id = session.next_id;
                session.next_id += 1;
                session.run(channel, id, body, label)
            }
            None => Err(TernError::new(
                TernErrorKind::Closed,
                "Tern connection closed",
            )),
        };
        // A closed or desynchronised stream cannot be trusted again; reconnect next time.
        // A timeout keeps the stream, since later answers are matched by id.
        if outcome.as_ref().is_err_and(|error| {
            matches!(error.kind, TernErrorKind::Closed | TernErrorKind::Protocol)
        }) {
            *guard = None;
        }
        outcome
    }

    fn ensure_session(&self, slot: &mut Option<Session>) -> Result<(), TernError> {
        if slot.is_none() {
            *slot = Some(self.open()?);
        }
        Ok(())
    }

    /// Connects, sends hello, and waits for the welcome.
    fn open(&self) -> Result<Session, TernError> {
        let path = &self.socket_path;
        let stream = UnixStream::connect(path).map_err(|error| {
            TernError::new(
                TernErrorKind::Connect,
                format!("Cannot reach the Tern daemon at {path}: {error}"),
            )
        })?;
        let mut connection = Connection {
            stream,
            buffered: Vec::new(),
        };
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let greeting = connection
            .send(&json!({ "hello": {} }), deadline)
            .and_then(|()| wait_for_welcome(&mut connection, deadline));
        let ops = match greeting {
            Ok(Some(ops)) => ops,
            Ok(None) => {
                return Err(TernError::new(
                    TernErrorKind::Timeout,
                    format!("Tern daemon at {path} did not answer the greeting in time"),
                ));
            }
            Err(error) => {
                let kind = error.kind;
                return Err(match kind {
                    TernErrorKind::Closed => TernError::new(
                        TernErrorKind::Connect,
                        format!(
                            "Tern daemon at {path} closed the connection before greeting: a Tern without Dial's JSON protocol cannot read its hello (update Tern)"
                        ),
                    ),
                    _ => error,
                });
            }
        };
        Ok(Session {
            connection,
            ops,
            next_id: 1,
        })
    }
}

#[cfg(test)]
mod tests;

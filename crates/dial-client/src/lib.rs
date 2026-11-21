//! Typed client for Dial's plain `dial rpc` control protocol.
//!
//! A frontend (terminal, desktop or test harness) launches `dial rpc` as a child process and drives
//! it through [`Client`]. The client owns process launch, the `ready` handshake, protocol
//! negotiation, request correlation and disconnect reporting. It does not depend on any engine
//! crate: everything it knows about Dial comes from the documented wire protocol
//! (`docs/rpc-and-acp.md`, `docs/rpc-schema.json`, `docs/client-contracts.md`).
//!
//! The API is blocking and toolkit-agnostic. A GUI typically sends requests with
//! [`Client::request`] and drains [`Client::poll`] from a background thread, forwarding each
//! [`Message`] to its own event loop. [`Client::call`] is a convenience for short queries; it
//! buffers every other message it sees so nothing is lost.
//!
//! # Recovery
//!
//! Dial does not replay events or deduplicate retried requests. When the process exits or its
//! output breaks, [`Message::Disconnected`] lists every request that never got a response and
//! whether it was [`Effect::ReadOnly`] (safe to repeat on a new connection) or
//! [`Effect::MayHaveApplied`] (its outcome is unknown and it must not be retried automatically).
//! Reconnecting means launching a new process, negotiating again and resuming the persisted
//! session; approval and question ids from the old connection are void.

use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{RecvTimeoutError, TrySendError};
use std::thread;
use std::time::{Duration, Instant};
use transport_queue::{Receiver, Sender as SyncSender};

mod conversation;
mod transport_queue;
pub use conversation::{
    AssistantEntry, Conversation, Entry, NoticeEntry, PendingApproval, ToolCard, ToolEntry,
    ToolOutcome, UserEntry,
};

#[cfg(test)]
mod tests;

/// The shared wire vocabulary (method table, error codes, event names), re-exported so a frontend
/// needs only this crate.
pub use dial_protocol as protocol;
pub use dial_protocol::ErrorCode;

/// Plain RPC protocol versions this client implements, oldest first. Deliberately listed here
/// rather than taken from `dial_protocol`: a new engine version must not be claimed before this
/// client implements it.
pub const CLIENT_PROTOCOL_VERSIONS: &[u32] = &[1, 2];

/// Largest single frame the client accepts. A longer line ends the connection instead of growing
/// memory without bound.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Frames buffered between the reader thread and the owner before the reader waits.
const QUEUED_FRAMES: usize = 1024;

/// Most unrelated messages one [`Client::call`] buffers before it returns [`ClientError::Backlog`].
const DEFERRED_MESSAGES: usize = QUEUED_FRAMES;

/// Approximate bytes of unrelated messages one [`Client::call`] buffers before it returns
/// [`ClientError::Backlog`]. A call stops reading once the bound is reached, so the buffer can
/// exceed it by at most one frame (itself at most [`MAX_FRAME_BYTES`]).
const DEFERRED_BYTES: usize = 16 * 1024 * 1024;

/// Bound queued writes as well as each frame; a full queue refuses admission without blocking.
const QUEUED_REQUESTS: usize = 8;

/// Wire payload budget per transport queue; one larger legal frame occupies a queue alone.
const TRANSPORT_BYTES: usize = 16 * 1024 * 1024;

/// Grace the background reaper gives a dropped client's engine after stdin closes.
const DROP_GRACE: Duration = Duration::from_secs(5);

/// Whether `method` only reads state, so repeating it after a lost response is safe. Every other
/// method, including one this build does not know, may have taken effect when its response was
/// lost. The classification comes from the shared method table.
pub fn is_read_only(method: &str) -> bool {
    dial_protocol::is_read_only(method)
}

/// Where the child's diagnostic stream goes. Stdout is always the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Diagnostics {
    /// Share the frontend's stderr (useful while developing).
    #[default]
    Inherit,
    /// Discard diagnostics (a GUI without a console).
    Discard,
}

/// How to start the engine process.
#[derive(Debug, Clone)]
pub struct Launch {
    program: PathBuf,
    args: Vec<OsString>,
    current_dir: Option<PathBuf>,
    env: Vec<(OsString, OsString)>,
    diagnostics: Diagnostics,
    interactive: bool,
}

impl Launch {
    /// Launch `program rpc [args…]`. Arguments added with [`Launch::arg`] follow the `rpc`
    /// subcommand (for example `--model`, `--endpoint` or `--resume`).
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            current_dir: None,
            env: Vec::new(),
            diagnostics: Diagnostics::default(),
            interactive: false,
        }
    }

    /// Add an argument after the `rpc` subcommand.
    #[must_use]
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Add several arguments after the `rpc` subcommand.
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Workspace directory the engine runs in.
    #[must_use]
    pub fn current_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(dir.into());
        self
    }

    /// Set one environment variable for the engine process.
    #[must_use]
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Choose where the engine's diagnostics go.
    #[must_use]
    pub fn diagnostics(mut self, diagnostics: Diagnostics) -> Self {
        self.diagnostics = diagnostics;
        self
    }

    /// Declare that a person answers this client (a GUI rather than a script), so the engine may
    /// ask interactive questions such as directory-access grants. Sent with `hello`, so it takes
    /// effect on protocol 2 and later.
    #[must_use]
    pub fn interactive(mut self, interactive: bool) -> Self {
        self.interactive = interactive;
        self
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .arg("rpc")
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(match self.diagnostics {
                Diagnostics::Inherit => Stdio::inherit(),
                Diagnostics::Discard => Stdio::null(),
            });
        if let Some(dir) = &self.current_dir {
            command.current_dir(dir);
        }
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command
    }
}

/// The engine's `ready` frame: identity, negotiation data and enforced limits.
#[derive(Debug, Clone, PartialEq)]
pub struct Ready {
    /// Session the connection starts in.
    pub session_id: String,
    /// Engine build version, for display and mismatch diagnostics; capabilities come from
    /// `methods`, never from comparing versions. `None` from builds before it was announced.
    pub engine_version: Option<String>,
    /// Active model name.
    pub model: String,
    /// Protocol versions the engine serves.
    pub supported_protocol_versions: Vec<u32>,
    /// Request methods the engine serves on this transport.
    pub methods: Vec<String>,
    /// Capability groups the engine serves on this transport.
    pub capabilities: Vec<String>,
    /// The complete frame, for fields this client does not model.
    pub raw: Value,
}

impl Ready {
    fn parse(frame: Value) -> Result<Self, ClientError> {
        if frame["event"] != "ready" {
            return Err(ClientError::Handshake(format!(
                "first frame was not `ready`: {}",
                clip(&frame.to_string())
            )));
        }
        let strings = |key: &str| -> Vec<String> {
            frame[key]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        };
        let mut supported: Vec<u32> = frame["supported_protocol_versions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_u64().and_then(|v| u32::try_from(v).ok()))
            .collect();
        if supported.is_empty() {
            // Builds from before negotiation announce only their single version.
            supported.extend(
                frame["protocol_version"]
                    .as_u64()
                    .and_then(|v| u32::try_from(v).ok()),
            );
        }
        Ok(Self {
            session_id: frame["session_id"].as_str().unwrap_or_default().to_owned(),
            engine_version: frame["engine_version"].as_str().map(str::to_owned),
            model: frame["model"].as_str().unwrap_or_default().to_owned(),
            supported_protocol_versions: supported,
            methods: strings("methods"),
            capabilities: strings("capabilities"),
            raw: frame,
        })
    }

    /// Highest protocol version both sides speak, if any.
    pub fn common_version(&self) -> Option<u32> {
        common_version(&self.supported_protocol_versions)
    }
}

fn common_version(served: &[u32]) -> Option<u32> {
    served
        .iter()
        .copied()
        .filter(|v| CLIENT_PROTOCOL_VERSIONS.contains(v))
        .max()
}

/// Correlates a [`Message::Response`] with the request that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(u64);

impl RequestId {
    /// The numeric id sent on the wire.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// An error reply from the engine. Protocol 2 adds a stable machine `code`; protocol 1 and
/// transport-level errors carry only the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    /// Stable machine code, such as `busy`, `invalid_params`, `not_pending`, `unknown_method`.
    pub code: Option<String>,
    /// Human-readable explanation.
    pub message: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.code {
            Some(code) => write!(f, "{code}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for RpcError {}

impl RpcError {
    fn parse(error: &Value) -> Self {
        Self {
            code: error["code"].as_str().map(str::to_owned),
            message: error["message"]
                .as_str()
                .map_or_else(|| error.to_string(), str::to_owned),
        }
    }

    /// The typed code. Protocol 1 errors, transport errors and codes from a newer engine are
    /// [`ErrorCode::Failed`]; branch on this rather than on the message text.
    pub fn kind(&self) -> ErrorCode {
        self.code
            .as_deref()
            .map_or(ErrorCode::Failed, ErrorCode::parse)
    }
}

/// An unsolicited engine event such as `text`, `tool_call` or `approval`.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// The `event` field.
    pub kind: String,
    /// The complete frame.
    pub body: Value,
}

impl Event {
    /// The approval request this event carries, when it is an `approval` event.
    pub fn approval(&self) -> Option<ApprovalRequest> {
        (self.kind == "approval").then(|| ApprovalRequest {
            approval_id: self.body["approval_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            tool: self.body["tool"].as_str().unwrap_or_default().to_owned(),
            summary: self.body["summary"].as_str().unwrap_or_default().to_owned(),
        })
    }

    /// Streamed assistant text, when this is a `text` event.
    pub fn text_delta(&self) -> Option<&str> {
        (self.kind == "text")
            .then(|| self.body["delta"].as_str())
            .flatten()
    }

    /// Per-connection sequence number (gap-free, in arrival order); `None` from engines that
    /// predate the event envelope.
    pub fn seq(&self) -> Option<u64> {
        self.body["seq"].as_u64()
    }

    /// Session the engine was serving when it produced this event.
    pub fn session_id(&self) -> Option<&str> {
        self.body["session_id"].as_str()
    }

    /// Id of the request whose processing produced this event, when it was one this client sent;
    /// `None` for background work between requests.
    pub fn request_id(&self) -> Option<RequestId> {
        self.body["request_id"].as_u64().map(RequestId)
    }
}

/// A pending tool approval. Its id is valid only on the connection that announced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    /// Id to answer with [`Client::approve`].
    pub approval_id: String,
    /// Tool waiting for approval.
    pub tool: String,
    /// Short description of the call.
    pub summary: String,
}

/// An approval answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Allow this call.
    Allow,
    /// Allow this kind of call from now on.
    Always,
    /// Allow this kind of call for the project.
    Project,
    /// Refuse this call.
    Deny,
}

impl Decision {
    fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Always => "always",
            Self::Project => "project",
            Self::Deny => "deny",
        }
    }
}

/// Whether a request lost to a disconnect could have changed the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Read-only; repeating it on a new connection is safe.
    ReadOnly,
    /// May have taken effect; show the outcome as unknown and never retry it automatically.
    MayHaveApplied,
}

/// A request that never received a response before the connection ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outstanding {
    /// The request.
    pub id: RequestId,
    /// Its method.
    pub method: String,
    /// Whether it could have changed the session.
    pub effect: Effect,
}

/// One item from the engine, in arrival order.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// The reply to a request this client sent.
    Response {
        /// Request it answers.
        id: RequestId,
        /// Method of that request.
        method: String,
        /// Its result.
        result: Result<Value, RpcError>,
    },
    /// An unsolicited event.
    Event(Event),
    /// A frame the client could not attribute: a malformed line, a protocol error without a
    /// request id, or a reply for an id this client never sent.
    ProtocolError(RpcError),
    /// The connection ended. Delivered once; later polls return `None`.
    Disconnected {
        /// Requests left without a response.
        outstanding: Vec<Outstanding>,
    },
}

/// A client-side failure.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The engine process could not be started.
    #[error("could not start {program}: {source}")]
    Spawn {
        /// Program that failed to start.
        program: String,
        /// Underlying error.
        source: io::Error,
    },
    /// The connection did not complete the `ready`/`hello` handshake.
    #[error("handshake failed: {0}")]
    Handshake(String),
    /// The engine answered with an error.
    #[error("{method} failed: {error}")]
    Rpc {
        /// Method that failed.
        method: String,
        /// The engine's error.
        error: RpcError,
    },
    /// The engine does not serve this method on this connection.
    #[error("the engine does not serve `{0}` on this connection")]
    Unsupported(String),
    /// No response arrived in time. The request is still outstanding.
    #[error("{method} did not respond within {timeout:?}")]
    Timeout {
        /// Method awaited.
        method: String,
        /// Bound that elapsed.
        timeout: Duration,
    },
    /// The bound on unrelated messages one call buffers was reached before the reply. The request
    /// is still outstanding: its reply, and every message deferred so far, arrive through
    /// [`Client::poll`].
    #[error(
        "{method} is behind more than {messages} unread messages or {bytes} bytes; poll before waiting again"
    )]
    Backlog {
        /// Method awaited.
        method: String,
        /// Message bound one call buffers at most.
        messages: usize,
        /// Byte bound one call buffers at most, before one overshooting frame.
        bytes: usize,
    },
    /// The connection ended. `effect` says whether the request could have taken effect.
    #[error("connection closed before {method} completed ({effect:?})")]
    Disconnected {
        /// Method in flight, or the attempted method.
        method: String,
        /// Whether it could have changed the session.
        effect: Effect,
    },
    /// Local I/O failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// The old connection after a restart. It still owns the messages it produced and did not have
/// polled, and hands them out one step at a time through [`Handoff::poll`]. Nothing is copied
/// ahead of time, so the connection's own bounds keep applying. Its sequence numbers belong to
/// that connection: deliver these messages before the new connection's, and call
/// [`Conversation::new_connection`] between the two streams.
///
/// Dropping an unfinished handoff discards the messages it still holds, so poll it until
/// [`HandoffPoll::Finished`].
#[derive(Debug)]
#[must_use = "unpolled messages of the old connection are discarded on drop; poll until Finished"]
pub struct Handoff {
    connection: Box<Client>,
    ended: bool,
}

/// One step of [`Handoff::poll`].
#[derive(Debug, Clone, PartialEq)]
pub enum HandoffPoll {
    /// The next message of the old connection, in arrival order. Its end is delivered as
    /// [`Message::Disconnected`], the last message the handoff produces.
    Message(Message),
    /// Nothing arrived within the timeout and the old connection has not ended: poll again.
    /// This is the explicit incomplete state; no message was lost.
    TimedOut,
    /// The old connection's end was already delivered; nothing more will come.
    Finished,
}

impl Handoff {
    /// The next unpolled message, waiting at most `timeout`. See [`HandoffPoll`].
    pub fn poll(&mut self, timeout: Duration) -> HandoffPoll {
        if self.ended {
            return HandoffPoll::Finished;
        }
        match self.connection.poll(timeout) {
            Some(message @ Message::Disconnected { .. }) => {
                self.ended = true;
                HandoffPoll::Message(message)
            }
            Some(message) => HandoffPoll::Message(message),
            None if self.connection.is_closed() => {
                self.ended = true;
                HandoffPoll::Finished
            }
            None => HandoffPoll::TimedOut,
        }
    }

    /// Whether the old connection's end has been delivered.
    pub fn is_finished(&self) -> bool {
        self.ended
    }

    /// Sequence numbers the old connection skipped ([`Client::lost_events`]). Non-zero means its
    /// view needs a reload.
    pub fn lost_events(&self) -> u64 {
        self.connection.lost_events()
    }
}

/// A successful [`Client::restart_and_resume`].
#[derive(Debug)]
pub struct Restarted {
    /// The replacement connection, resumed on the same session.
    pub client: Client,
    /// The old connection's unpolled messages. Deliver them before `client`'s messages.
    pub handoff: Handoff,
}

/// A failed [`Client::restart_and_resume`]. `client` hands back a running engine whenever one
/// exists, so a refused restart loses neither the engine nor its buffered messages.
#[derive(Debug, thiserror::Error)]
#[error("restart failed: {error}")]
pub struct RestartError {
    /// Why the restart did not complete.
    pub error: ClientError,
    /// The connection that still owns an engine, or `None` once no engine is running. After a
    /// [`ClientError::Backlog`] refusal the engine has been asked to shut down and its stdin is
    /// closed, so this connection takes no requests: poll it to drain, then [`Client::shutdown`].
    /// Boxed so that the error stays small on the `Result` path.
    pub client: Option<Box<Client>>,
    /// The old connection's unpolled messages, present once the old engine has exited or the
    /// restart was stopped after it began to exit. `None` while `client` is still the old
    /// connection, which keeps its own buffer.
    pub handoff: Option<Handoff>,
}

impl RestartError {
    fn running(client: Client, error: ClientError) -> Self {
        Self {
            error,
            client: Some(Box::new(client)),
            handoff: None,
        }
    }

    fn stopped(error: ClientError, handoff: Handoff) -> Self {
        Self {
            error,
            client: None,
            handoff: Some(handoff),
        }
    }
}

enum Line {
    Frame(Value),
    Malformed(String),
    End,
}

/// A connected engine process.
#[derive(Debug)]
pub struct Client {
    child: Option<Child>,
    /// Failed handshakes kill immediately; only established connections get graceful EOF time.
    drop_grace: Duration,
    stdin: Option<SyncSender<String>>,
    lines: Receiver<LineSlot>,
    next_id: u64,
    pending: BTreeMap<u64, String>,
    /// Messages a call deferred, each with its approximate size. Drained before any new line.
    buffered: VecDeque<(Message, usize)>,
    ready: Ready,
    protocol: u32,
    closed: bool,
    /// `seq` of the last event seen, and how many sequence numbers were skipped.
    last_seq: Option<u64>,
    lost_events: u64,
}

/// Opaque wrapper so `Client` can derive `Debug` without exposing the reader's line type.
struct LineSlot(Line, usize);

impl std::fmt::Debug for LineSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LineSlot")
    }
}

impl Client {
    /// Start the engine, wait for `ready`, and negotiate the highest common protocol version.
    ///
    /// # Errors
    /// Fails when the process cannot start, `ready` does not arrive within `timeout`, or the
    /// engine serves no protocol version this client implements.
    pub fn connect(launch: &Launch, timeout: Duration) -> Result<Self, ClientError> {
        let mut child = launch
            .command()
            .spawn()
            .map_err(|source| ClientError::Spawn {
                program: launch.program.display().to_string(),
                source,
            })?;
        let (stdin, stdout) = match (child.stdin.take(), child.stdout.take()) {
            (Some(stdin), Some(stdout)) => (stdin, stdout),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ClientError::Handshake("child pipes unavailable".into()));
            }
        };
        let (stdin, lines) = spawn_transport(stdin, stdout, MAX_FRAME_BYTES);
        let mut client = Self {
            child: Some(child),
            drop_grace: Duration::ZERO,
            stdin: Some(stdin),
            lines,
            next_id: 1,
            pending: BTreeMap::new(),
            buffered: VecDeque::new(),
            ready: Ready {
                session_id: String::new(),
                model: String::new(),
                engine_version: None,
                supported_protocol_versions: Vec::new(),
                methods: Vec::new(),
                capabilities: Vec::new(),
                raw: Value::Null,
            },
            protocol: 1,
            closed: false,
            last_seq: None,
            lost_events: 0,
        };
        let deadline = Instant::now() + timeout;
        let first = match client.lines.recv_timeout(timeout) {
            Ok(LineSlot(Line::Frame(frame), _)) => frame,
            Ok(LineSlot(Line::Malformed(error), _)) => {
                return Err(ClientError::Handshake(format!(
                    "malformed first frame: {error}"
                )));
            }
            Ok(LineSlot(Line::End, _)) | Err(RecvTimeoutError::Disconnected) => {
                return Err(ClientError::Handshake(
                    "engine exited before `ready`".into(),
                ));
            }
            Err(RecvTimeoutError::Timeout) => {
                return Err(ClientError::Handshake(format!(
                    "no `ready` within {timeout:?}"
                )));
            }
        };
        client.ready = Ready::parse(first)?;
        client.last_seq = client.ready.raw["seq"].as_u64();
        let version = client.ready.common_version().ok_or_else(|| {
            ClientError::Handshake(format!(
                "engine serves protocol {:?}; client implements {CLIENT_PROTOCOL_VERSIONS:?}",
                client.ready.supported_protocol_versions
            ))
        })?;
        if version > 1 {
            let left = deadline.saturating_duration_since(Instant::now());
            let mut hello = json!({"protocol_version": version});
            if launch.interactive {
                hello["interactive"] = json!(true);
            }
            let reply = client.call("hello", hello, left.max(Duration::from_millis(1)))?;
            client.protocol = reply["protocol_version"]
                .as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .filter(|v| CLIENT_PROTOCOL_VERSIONS.contains(v))
                .ok_or_else(|| {
                    ClientError::Handshake(format!("unexpected hello reply: {reply}"))
                })?;
        }
        client.drop_grace = DROP_GRACE;
        Ok(client)
    }

    /// The engine's `ready` frame.
    pub fn ready(&self) -> &Ready {
        &self.ready
    }

    /// Negotiated protocol version.
    pub fn protocol_version(&self) -> u32 {
        self.protocol
    }

    /// Whether the engine serves `method` on this connection.
    pub fn supports(&self, method: &str) -> bool {
        self.ready.methods.iter().any(|m| m == method)
    }

    /// Whether the connection has ended.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Event sequence numbers skipped so far. Non-zero means frames were lost (a broken engine
    /// or transport), so the GUI should refresh `state` and `messages_page` rather than trust its
    /// incrementally built view.
    pub fn lost_events(&self) -> u64 {
        self.lost_events
    }

    /// Requests still waiting for a response.
    pub fn outstanding(&self) -> Vec<Outstanding> {
        self.pending
            .iter()
            .map(|(id, method)| outstanding(*id, method))
            .collect()
    }

    /// Queue a request without waiting for pipe I/O. Its reply arrives as a [`Message::Response`].
    ///
    /// # Errors
    /// [`ClientError::Unsupported`] when the engine does not serve `method`;
    /// [`ClientError::Io`] with `WouldBlock` when the bounded writer queue is full, or
    /// `InvalidInput` when the frame exceeds [`MAX_FRAME_BYTES`]. These refusals send nothing.
    /// [`ClientError::Disconnected`] when the connection is closed. An admitted request stays
    /// outstanding until a response or disconnect; write failures are reported by [`Client::poll`]
    /// and leave mutating outcomes unknown.
    pub fn request(&mut self, method: &str, params: Value) -> Result<RequestId, ClientError> {
        if !self.supports(method) {
            return Err(ClientError::Unsupported(method.to_owned()));
        }
        let Some(stdin) = self.stdin.as_ref().filter(|_| !self.closed) else {
            return Err(ClientError::Disconnected {
                method: method.to_owned(),
                effect: Effect::ReadOnly,
            });
        };
        let id = self.next_id;
        self.next_id += 1;
        let mut line = json!({"id": id, "method": method, "params": params}).to_string();
        line.push('\n');
        if line.len() > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "request frame exceeds client limit",
            )
            .into());
        }
        match stdin.try_send(line) {
            Ok(()) => {
                // The writer may already have sent a prefix. Only the owner settles replies.
                self.pending.insert(id, method.to_owned());
                Ok(RequestId(id))
            }
            Err(TrySendError::Full(_)) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "RPC request queue is full; poll responses before retrying admission",
            )
            .into()),
            Err(TrySendError::Disconnected(_)) => {
                self.stdin = None;
                Err(ClientError::Disconnected {
                    method: method.to_owned(),
                    effect: Effect::ReadOnly,
                })
            }
        }
    }

    /// Next message, waiting at most `timeout`. Returns `None` on timeout, and after
    /// [`Message::Disconnected`] has been delivered.
    pub fn poll(&mut self, timeout: Duration) -> Option<Message> {
        if let Some((message, _)) = self.buffered.pop_front() {
            return Some(message);
        }
        if self.closed {
            return None;
        }
        let line = match self.lines.recv_timeout(timeout) {
            Ok(LineSlot(line, _)) => line,
            Err(RecvTimeoutError::Timeout) => return None,
            Err(RecvTimeoutError::Disconnected) => Line::End,
        };
        Some(self.classify(line))
    }

    fn classify(&mut self, line: Line) -> Message {
        match line {
            Line::End => {
                self.closed = true;
                self.stdin = None;
                let outstanding = self.outstanding();
                self.pending.clear();
                Message::Disconnected { outstanding }
            }
            Line::Malformed(message) => Message::ProtocolError(RpcError {
                code: None,
                message,
            }),
            Line::Frame(frame) => {
                self.lost_events += skipped_events(&mut self.last_seq, &frame);
                classify_frame(frame, &mut self.pending)
            }
        }
    }

    /// Send a request and wait for its reply, buffering every other message for [`Client::poll`].
    ///
    /// Meant for quick queries. Long operations such as `send` stream events while they run; a
    /// GUI should use [`Client::request`] and [`Client::poll`] for them instead.
    ///
    /// One call buffers at most `DEFERRED_MESSAGES` messages and about `DEFERRED_BYTES` bytes.
    /// Past either bound it stops reading and returns [`ClientError::Backlog`]: nothing is
    /// dropped, and only a single frame can overshoot the byte bound.
    ///
    /// # Errors
    /// [`ClientError::Rpc`] for an engine error reply, [`ClientError::Timeout`] when no reply
    /// arrives in time, [`ClientError::Backlog`] when too many other messages arrive first (both
    /// leave the request outstanding), and [`ClientError::Disconnected`] when the connection ends
    /// first.
    pub fn call(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ClientError> {
        let deadline = Instant::now() + timeout;
        let wanted = self.request(method, params)?;
        let mut deferred: Vec<(Message, usize)> = Vec::new();
        let mut deferred_bytes = 0usize;
        let outcome = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break Err(ClientError::Timeout {
                    method: method.to_owned(),
                    timeout,
                });
            }
            let next = match self.buffered.pop_front() {
                Some(entry) => Some(entry),
                None if self.closed => None,
                None if deferred.len() >= DEFERRED_MESSAGES || deferred_bytes >= DEFERRED_BYTES => {
                    break Err(ClientError::Backlog {
                        method: method.to_owned(),
                        messages: DEFERRED_MESSAGES,
                        bytes: DEFERRED_BYTES,
                    });
                }
                None => match self.lines.recv_timeout(left) {
                    Ok(LineSlot(line, _)) => Some(sized(self.classify(line))),
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => Some(sized(self.classify(Line::End))),
                },
            };
            match next {
                Some((Message::Response { id, result, .. }, _)) if id == wanted => {
                    break result.map_err(|error| ClientError::Rpc {
                        method: method.to_owned(),
                        error,
                    });
                }
                Some((Message::Disconnected { outstanding }, bytes)) => {
                    let lost = outstanding
                        .iter()
                        .find(|o| o.id == wanted)
                        .map_or(Effect::ReadOnly, |o| o.effect);
                    deferred.push((Message::Disconnected { outstanding }, bytes));
                    break Err(ClientError::Disconnected {
                        method: method.to_owned(),
                        effect: lost,
                    });
                }
                Some(entry) => {
                    deferred_bytes += entry.1;
                    deferred.push(entry);
                }
                None => {
                    break Err(ClientError::Disconnected {
                        method: method.to_owned(),
                        effect: effect(method),
                    });
                }
            }
        };
        // Keep arrival order: messages deferred here precede anything still buffered.
        for entry in deferred.into_iter().rev() {
            self.buffered.push_front(entry);
        }
        outcome
    }

    /// Answer a pending approval.
    ///
    /// # Errors
    /// As [`Client::call`]; a stale or already answered id fails with code `not_pending`.
    pub fn approve(
        &mut self,
        approval_id: &str,
        decision: Decision,
        timeout: Duration,
    ) -> Result<Value, ClientError> {
        self.call(
            "approve",
            json!({"approval_id": approval_id, "decision": decision.as_str()}),
            timeout,
        )
    }

    /// Ask the active turn, tasks and pending prompts to stop.
    ///
    /// # Errors
    /// As [`Client::call`].
    pub fn abort(&mut self, timeout: Duration) -> Result<Value, ClientError> {
        self.call("abort", json!({}), timeout)
    }

    /// Current session snapshot.
    ///
    /// # Errors
    /// As [`Client::call`].
    pub fn state(&mut self, timeout: Duration) -> Result<Value, ClientError> {
        self.call("state", json!({}), timeout)
    }

    /// Start a turn with `text`. The turn streams events; its reply arrives as a
    /// [`Message::Response`] when the turn settles.
    ///
    /// # Errors
    /// As [`Client::request`].
    pub fn send(&mut self, text: &str) -> Result<RequestId, ClientError> {
        self.request("send", json!({"text": text}))
    }

    /// Queue guidance for the running turn. Fails with code `busy` when the queue is full and
    /// `not_pending` when no turn is running.
    ///
    /// # Errors
    /// As [`Client::call`].
    pub fn steer(&mut self, text: &str, timeout: Duration) -> Result<Value, ClientError> {
        self.call("steer", json!({"text": text}), timeout)
    }

    /// Switch to a persisted session of this project.
    ///
    /// # Errors
    /// As [`Client::call`]; an unknown id fails with code `not_found`.
    pub fn resume(&mut self, session_id: &str, timeout: Duration) -> Result<Value, ClientError> {
        self.call("resume", json!({"session_id": session_id}), timeout)
    }

    /// Save one setting. The running engine keeps its launch configuration; when the reply says
    /// `restart_required`, apply it with [`Client::apply_settings`] (or
    /// [`Client::restart_and_resume`] on engines that do not serve it).
    ///
    /// # Errors
    /// As [`Client::call`]; a bad key, value or scope fails with code `invalid_params`.
    pub fn set_setting(
        &mut self,
        key: &str,
        value: Value,
        project_scope: bool,
        timeout: Duration,
    ) -> Result<Value, ClientError> {
        let scope = if project_scope { "project" } else { "global" };
        self.call(
            "set_setting",
            json!({"key": key, "value": value, "scope": scope}),
            timeout,
        )
    }

    /// Checkpointed turns of the active session that changed files, newest first.
    ///
    /// # Errors
    /// As [`Client::call`].
    pub fn list_checkpoints(&mut self, timeout: Duration) -> Result<Value, ClientError> {
        self.call("list_checkpoints", json!({}), timeout)
    }

    /// The diff restoring `path` from `turn` would apply, and its review token.
    ///
    /// # Errors
    /// As [`Client::call`]; an unknown turn or path fails with code `not_found`.
    pub fn checkpoint_diff(
        &mut self,
        turn: u64,
        path: &str,
        timeout: Duration,
    ) -> Result<Value, ClientError> {
        self.call(
            "checkpoint_diff",
            json!({"turn": turn, "path": path}),
            timeout,
        )
    }

    /// Undo the last `turns` user turns: restore their files, then rewind the conversation.
    ///
    /// # Errors
    /// As [`Client::call`]; a hook veto fails with code `refused`.
    pub fn undo(&mut self, turns: usize, timeout: Duration) -> Result<Value, ClientError> {
        self.call("undo", json!({"turns": turns}), timeout)
    }

    /// Start a turn with `text` and image attachments: `images` are file paths (absolute, `~`, or
    /// relative to the workspace root) and `inline_images` are base64 image bytes.
    ///
    /// # Errors
    /// As [`Client::request`]; the turn's reply fails with code `invalid_params` when an image
    /// cannot be read or the active model does not accept image input.
    pub fn send_with_images(
        &mut self,
        text: &str,
        images: &[&str],
        inline_images: &[&str],
    ) -> Result<RequestId, ClientError> {
        self.request(
            "send",
            json!({"text": text, "images": images, "inline_images": inline_images}),
        )
    }

    /// Apply the saved settings to the running engine in place, keeping the session. The reply's
    /// `applied` is `live` (nothing to rebuild) or `rebuild` (the agent was rebuilt).
    ///
    /// # Errors
    /// As [`Client::call`]; fails with code `busy` while workers run and `invalid_params` when the
    /// saved settings are invalid.
    pub fn apply_settings(&mut self, timeout: Duration) -> Result<Value, ClientError> {
        self.call("apply_settings", json!({}), timeout)
    }

    /// Set the session's approval mode: `ask`, `write`, `auto`, `readonly` or `yolo`.
    ///
    /// # Errors
    /// As [`Client::call`]; another mode fails with code `invalid_params`.
    pub fn set_approval(&mut self, mode: &str, timeout: Duration) -> Result<Value, ClientError> {
        self.call("set_approval", json!({"mode": mode}), timeout)
    }

    /// Run one `/extensions` action (`intent` is tagged by `op`, for example
    /// `{"op":"toggle_mcp","id":"github","enable":true}`). Actions that change files, run a
    /// downloaded command, or remove or disable something need `confirm`. Saved changes apply on
    /// [`Client::apply_settings`]. Progress arrives as `extension_progress` events.
    ///
    /// # Errors
    /// As [`Client::call`]; an unconfirmed risky action fails with code `invalid_params`, a second
    /// concurrent action with `busy`.
    pub fn extension_action(
        &mut self,
        intent: Value,
        confirm: bool,
        timeout: Duration,
    ) -> Result<Value, ClientError> {
        self.call(
            "extension_action",
            json!({"intent": intent, "confirm": confirm}),
            timeout,
        )
    }

    /// Start signing in to the configured MCP server `server`.
    ///
    /// Drain [`Client::poll`] while the request is outstanding: display `auth_url` events and
    /// answer `auth_prompt` events with an `auth_code` request. The response with the returned id
    /// completes login. Apply the grant with `apply_settings {reconnect: true}`.
    ///
    /// # Errors
    /// As [`Client::request`]; the eventual reply uses `not_found` for an unconfigured server.
    pub fn mcp_login(&mut self, server: &str) -> Result<RequestId, ClientError> {
        self.request("mcp_login", json!({"server": server}))
    }

    /// The workspace roots: `{root, dirs}`.
    ///
    /// # Errors
    /// As [`Client::call`].
    pub fn list_dirs(&mut self, timeout: Duration) -> Result<Value, ClientError> {
        self.call("list_dirs", json!({}), timeout)
    }

    /// Add (`add` true) or remove a workspace directory; the change is persisted.
    ///
    /// # Errors
    /// As [`Client::call`]; a missing directory fails with code `invalid_params`.
    pub fn workspace_dir(
        &mut self,
        path: &str,
        add: bool,
        timeout: Duration,
    ) -> Result<Value, ClientError> {
        let method = if add { "add_dir" } else { "remove_dir" };
        self.call(method, json!({"path": path}), timeout)
    }

    /// The session's background jobs, also while a turn runs.
    ///
    /// # Errors
    /// As [`Client::call`].
    pub fn list_jobs(&mut self, timeout: Duration) -> Result<Value, ClientError> {
        self.call("list_jobs", json!({}), timeout)
    }

    /// Subscription usage and plan for every signed-in provider, as `/usage` shows.
    ///
    /// # Errors
    /// As [`Client::call`].
    pub fn usage(&mut self, timeout: Duration) -> Result<Value, ClientError> {
        self.call("usage", json!({}), timeout)
    }

    /// Stop one background job by name.
    ///
    /// # Errors
    /// As [`Client::call`]; an unknown job fails with code `not_found`.
    pub fn stop_job(&mut self, name: &str, timeout: Duration) -> Result<Value, ClientError> {
        self.call("stop_job", json!({"name": name}), timeout)
    }

    /// The protocol's JSON Schema, for generating client types in other languages.
    ///
    /// # Errors
    /// As [`Client::call`].
    pub fn schema(&mut self, timeout: Duration) -> Result<Value, ClientError> {
        self.call("get_schema", json!({}), timeout)
    }

    /// Apply saved settings by replacing this engine process: read the active session, shut
    /// this engine down, launch a new one with `launch` and resume that session there.
    ///
    /// Refuses while requests are outstanding, so a running turn is never cut off; abort or let
    /// it settle first. Approval, question and turn ids from this connection are void afterwards;
    /// [`Conversation::new_connection`] drops them on the view side.
    ///
    /// The old engine is not killed while output it wrote is unread. The restart first sends
    /// `shutdown` and closes the engine's stdin, then reads its output into the same bounded queue
    /// [`Client::call`] uses, until it exits. If that queue fills while the engine still runs, the
    /// restart is refused with [`ClientError::Backlog`]. That refusal is not a rollback: the
    /// engine is winding down, so [`RestartError::client`] takes no requests (they fail as
    /// `Disconnected`). Poll it until its end arrives, reap it with [`Client::shutdown`], then
    /// reconnect with [`Client::connect`] and [`Client::resume`] to the session you read before
    /// restarting. Calling `restart_and_resume` again on that client cannot work, because its
    /// `state` call fails. An engine that exited is never refused for its buffered messages:
    /// those come back through [`Restarted::handoff`] (or [`RestartError::handoff`]), read in
    /// bounded steps by [`Handoff::poll`]. An inherited stdout held open by a grandchild cannot
    /// stall the restart.
    ///
    /// Failures before the wind-down begins keep the running engine reachable:
    /// [`RestartError::client`] is the old connection (a refusal because requests are
    /// outstanding, or a failed `state`), and retrying is possible. A replacement that started
    /// but did not resume the session is returned as `client`, so `resume` can be retried. When
    /// the old engine has exited and no replacement runs, `client` is `None` and
    /// [`RestartError::handoff`] holds the old connection's messages.
    ///
    /// # Errors
    /// [`RestartError`] wrapping [`ClientError::Unsupported`] while requests are outstanding,
    /// [`ClientError::Backlog`] as described above, or the error from [`Client::state`],
    /// [`Client::connect`] or [`Client::resume`].
    pub fn restart_and_resume(
        mut self,
        launch: &Launch,
        timeout: Duration,
    ) -> Result<Restarted, RestartError> {
        if !self.pending.is_empty() {
            return Err(RestartError::running(
                self,
                ClientError::Unsupported(
                    "restart_and_resume while requests are outstanding".into(),
                ),
            ));
        }
        let state = match self.state(timeout) {
            Ok(state) => state,
            Err(error) => return Err(RestartError::running(self, error)),
        };
        let session_id = state["session_id"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| self.ready.session_id.clone());
        if let Err(error) = self.wind_down_for_restart(timeout) {
            return Err(RestartError::running(self, error));
        }
        let old = Handoff {
            connection: Box::new(self),
            ended: false,
        };
        let mut next = match Client::connect(launch, timeout) {
            Ok(next) => next,
            Err(error) => return Err(RestartError::stopped(error, old)),
        };
        if next.ready.session_id != session_id
            && let Err(error) = next.resume(&session_id, timeout)
        {
            return Err(RestartError {
                error,
                client: Some(Box::new(next)),
                handoff: Some(old),
            });
        }
        Ok(Restarted {
            client: next,
            handoff: old,
        })
    }

    /// Request shutdown, then read the engine's output into the bounded queue until the process
    /// exits. The exit is checked before the queue's bound: an engine that already exited hands
    /// its buffered messages over, and nothing is refused or read further. A full queue while the
    /// engine still runs refuses with `Backlog`. Once `timeout` passes, the engine is killed; what
    /// it already wrote stays readable.
    fn wind_down_for_restart(&mut self, timeout: Duration) -> Result<(), ClientError> {
        let deadline = Instant::now() + timeout;
        if !self.closed && self.supports("shutdown") {
            // The reply and everything before it are read by the loop below, so the call's own
            // outcome (a timeout, a backlog or a refusal) needs no separate handling.
            let _ = self.call("shutdown", json!({}), timeout);
        }
        self.stdin = None;
        loop {
            if self
                .child
                .as_mut()
                .expect("owned child")
                .try_wait()?
                .is_some()
            {
                return Ok(());
            }
            if self.buffer_full() {
                return Err(ClientError::Backlog {
                    method: "shutdown".into(),
                    messages: DEFERRED_MESSAGES,
                    bytes: DEFERRED_BYTES,
                });
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                let _ = self.child.as_mut().expect("owned child").kill();
                self.child.as_mut().expect("owned child").wait()?;
                return Ok(());
            }
            let step = left.min(Duration::from_millis(20));
            if self.closed {
                // The reader has finished; only the process exit remains to wait for.
                thread::sleep(step);
                continue;
            }
            match self.lines.recv_timeout(step) {
                Ok(LineSlot(line, _)) => self.buffer_next(line),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => self.buffer_next(Line::End),
            }
        }
    }

    /// Classify one line and queue the message for [`Client::poll`].
    fn buffer_next(&mut self, line: Line) {
        let message = self.classify(line);
        self.buffered.push_back(sized(message));
    }

    /// Whether the deferred queue has reached the bound a single [`Client::call`] stops at.
    fn buffer_full(&self) -> bool {
        self.buffered.len() >= DEFERRED_MESSAGES
            || self.buffered.iter().map(|(_, bytes)| *bytes).sum::<usize>() >= DEFERRED_BYTES
    }

    /// Ask the engine to close, then wait for the process to exit; kills it when it does not exit
    /// within `timeout`.
    ///
    /// # Errors
    /// Fails only when waiting for or killing the process fails.
    pub fn shutdown(mut self, timeout: Duration) -> Result<ExitStatus, ClientError> {
        self.shutdown_in_place(timeout)
    }

    /// Shared by [`Client::shutdown`] and [`Client::restart_and_resume`].
    fn shutdown_in_place(&mut self, timeout: Duration) -> Result<ExitStatus, ClientError> {
        let deadline = Instant::now() + timeout;
        if !self.closed && self.supports("shutdown") {
            let _ = self.call("shutdown", json!({}), timeout);
        }
        self.stdin = None;
        let left = deadline.saturating_duration_since(Instant::now());
        Ok(self.wait_or_kill(left)?)
    }

    /// Terminate the engine immediately. Pending requests are reported by the next
    /// [`Message::Disconnected`].
    ///
    /// # Errors
    /// Fails when the process cannot be signalled.
    pub fn kill(&mut self) -> io::Result<()> {
        self.stdin = None;
        match self.child.as_mut().expect("owned child").kill() {
            Err(error) if error.kind() != io::ErrorKind::InvalidInput => Err(error),
            _ => Ok(()),
        }
    }

    fn wait_or_kill(&mut self, grace: Duration) -> io::Result<ExitStatus> {
        let deadline = Instant::now() + grace;
        loop {
            if let Some(status) = self.child.as_mut().expect("owned child").try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                let _ = self.child.as_mut().expect("owned child").kill();
                return self.child.as_mut().expect("owned child").wait();
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // Do not stall a frontend's event loop. The reaper owns the process until it has been
        // waited on; explicit shutdown still performs the graceful wait on the caller's thread.
        self.stdin = None;
        if let Some(mut child) = self.child.take() {
            let grace = self.drop_grace;
            thread::spawn(move || {
                let deadline = Instant::now() + grace;
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => return,
                        Ok(None) if Instant::now() < deadline => {
                            thread::sleep(Duration::from_millis(20));
                        }
                        _ => {
                            let _ = child.kill();
                            let _ = child.wait();
                            return;
                        }
                    }
                }
            });
        }
    }
}

/// Pair a message with its approximate size, the unit `call` bounds its deferred backlog by.
fn sized(message: Message) -> (Message, usize) {
    let bytes = message_bytes(&message);
    (message, bytes)
}

/// Approximate size of a message: its payload text plus a fixed overhead for its envelope.
fn message_bytes(message: &Message) -> usize {
    const OVERHEAD: usize = 64;
    OVERHEAD
        + match message {
            Message::Event(event) => event.body.to_string().len(),
            Message::Response { method, result, .. } => {
                method.len()
                    + match result {
                        Ok(value) => value.to_string().len(),
                        Err(error) => error.message.len(),
                    }
            }
            Message::ProtocolError(error) => error.message.len(),
            Message::Disconnected { outstanding } => {
                outstanding.iter().map(|o| OVERHEAD + o.method.len()).sum()
            }
        }
}

fn effect(method: &str) -> Effect {
    if is_read_only(method) {
        Effect::ReadOnly
    } else {
        Effect::MayHaveApplied
    }
}

fn outstanding(id: u64, method: &str) -> Outstanding {
    Outstanding {
        id: RequestId(id),
        method: method.to_owned(),
        effect: effect(method),
    }
}

/// Sequence numbers an event frame skipped since the previous event; replies and frames from
/// engines without the envelope skip none.
fn skipped_events(last: &mut Option<u64>, frame: &Value) -> u64 {
    let Some(seq) = frame["event"]
        .is_string()
        .then(|| frame["seq"].as_u64())
        .flatten()
    else {
        return 0;
    };
    let skipped = last.map_or(0, |last| seq.saturating_sub(last + 1));
    *last = Some(seq);
    skipped
}

/// Turn one decoded frame into a message, settling its request when it is a reply.
fn classify_frame(frame: Value, pending: &mut BTreeMap<u64, String>) -> Message {
    if let Some(kind) = frame["event"].as_str() {
        return Message::Event(Event {
            kind: kind.to_owned(),
            body: frame,
        });
    }
    let is_reply = frame.get("result").is_some() || frame.get("error").is_some();
    match frame["id"].as_u64() {
        Some(id) if is_reply => match pending.remove(&id) {
            Some(method) => Message::Response {
                id: RequestId(id),
                method,
                result: match frame.get("error") {
                    Some(error) => Err(RpcError::parse(error)),
                    None => Ok(frame["result"].clone()),
                },
            },
            None => Message::ProtocolError(RpcError {
                code: None,
                message: format!("reply for unknown request id {id}"),
            }),
        },
        _ => Message::ProtocolError(match frame.get("error") {
            Some(error) => RpcError::parse(error),
            None => RpcError {
                code: None,
                message: format!("unrecognised frame: {}", clip(&frame.to_string())),
            },
        }),
    }
}

fn spawn_transport(
    mut stdin: ChildStdin,
    stdout: ChildStdout,
    limit: usize,
) -> (SyncSender<String>, Receiver<LineSlot>) {
    let (tx, rx) = transport_queue::channel(QUEUED_FRAMES, TRANSPORT_BYTES, line_bytes);
    let (requests, writes) =
        transport_queue::channel::<String>(QUEUED_REQUESTS, TRANSPORT_BYTES, String::len);
    let reader = tx.clone();
    thread::spawn(move || read_frames(BufReader::new(stdout), limit, &reader));
    // Only this thread owns the pipe: complete frames stay FIFO and never interleave. Blocking
    // writes cannot trap the caller's timeout or shutdown; killing the child releases the pipe.
    thread::spawn(move || {
        while let Ok(line) = writes.recv() {
            if stdin
                .write_all(line.as_bytes())
                .and_then(|()| stdin.flush())
                .is_err()
            {
                let _ = tx.send(LineSlot(Line::End, 1));
                break;
            }
        }
    });
    (requests, rx)
}

/// Decode newline-delimited JSON until EOF, an oversized line or a closed receiver; always ends
/// with [`Line::End`] when the receiver is still listening.
fn read_frames(mut reader: impl BufRead, limit: usize, tx: &SyncSender<LineSlot>) {
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        let read = (&mut reader)
            .take(limit as u64 + 1)
            .read_until(b'\n', &mut buffer);
        let line = match read {
            Ok(0) | Err(_) => break,
            Ok(_) if buffer.len() > limit => {
                let _ = tx.send(LineSlot(
                    Line::Malformed(format!("frame exceeds {limit} bytes; connection closed")),
                    64,
                ));
                break;
            }
            Ok(_) => {
                let text = buffer.strip_suffix(b"\n").unwrap_or(&buffer);
                let text = text.strip_suffix(b"\r").unwrap_or(text);
                if text.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                match serde_json::from_slice(text) {
                    Ok(frame) => Line::Frame(frame),
                    Err(error) => Line::Malformed(format!("non-JSON frame: {error}")),
                }
            }
        };
        if tx.send(LineSlot(line, buffer.len().max(1))).is_err() {
            return;
        }
    }
    let _ = tx.send(LineSlot(Line::End, 1));
}

fn clip(text: &str) -> String {
    const MAX: usize = 200;
    if text.len() <= MAX {
        return text.to_owned();
    }
    let mut end = MAX;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

// The parser records the original wire length; avoid serializing every frame a second time
// merely to account for queue admission. Decoded JSON has additional bounded overhead.
fn line_bytes(slot: &LineSlot) -> usize {
    slot.1
}

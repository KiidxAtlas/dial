//! A JSON-RPC 2.0 client for one MCP server, over stdio (newline-delimited messages), streamable
//! HTTP or legacy HTTP+SSE.
//!
//! Incoming messages, whatever the transport, go through [`Shared::handle`]: responses are routed
//! to the waiting request by id, progress notifications to the request that asked for them,
//! `list_changed`/`resources/updated` notifications become [`Signal`]s for the connection's
//! supervisor, and server requests are answered (`ping`, `roots/list` with the workspace root;
//! anything else is "method not found").
//!
//! For stdio, helper threads keep the caller from ever blocking on the server: the **reader** parses
//! stdout lines, the **writer** owns stdin (a server that stops reading cannot stall a caller) and
//! the **stderr drain** keeps the last few KiB for error messages. Remote requests are POSTed from a
//! bounded, owned worker. A waiting request polls cancellation; cancellation stops queued
//! dispatch, interrupts active HTTP I/O and joins the worker before returning. Dispatched remote
//! effects remain unknown even after a cancellation notification. When the connection ends every waiting request
//! fails with the reason, so a crash surfaces as a clear error, never a hang.

use super::Signal;
use crate::runtime::integration::{IntegrationFailure, Settlement};
pub(super) mod lifecycle;
use crate::config::types::McpServerConfig;
use crate::runtime::execution::Effects;
use crate::tools::exec::sandbox::is_secret_env;
use anyhow::{Context, Result, anyhow, bail};
use lifecycle::{MAX_IN_FLIGHT, MAX_REQUEST_LIFETIME, Operation, Outgoing, OwnedSend, Phase};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

/// A sent request either received a terminal protocol reply or lost its settlement.
/// Absence of this marker means the request was never admitted to the transport.
#[derive(Debug)]
pub(super) struct RequestEffects(pub Effects);

impl std::fmt::Display for RequestEffects {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.0 {
            Effects::Unknown => "MCP request settlement is unknown",
            _ => "MCP request received a terminal reply",
        })
    }
}
impl std::error::Error for RequestEffects {}

#[derive(Debug)]
pub(super) struct RequestFailure {
    pub dispatched: bool,
    pub settlement: Settlement,
    pub failure: IntegrationFailure,
}
impl std::fmt::Display for RequestFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MCP lifecycle: {:?}/{:?}", self.settlement, self.failure)
    }
}
impl std::error::Error for RequestFailure {}

/// Newest first; the first entry is what `initialize` offers.
const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
/// Longest single message accepted from a server; longer lines are dropped.
const MAX_LINE: usize = 16 * 1024 * 1024;
/// Server stderr kept for error messages.
const STDERR_TAIL: usize = 4096;
/// Characters of server stderr quoted in an error.
const STDERR_QUOTE: usize = 600;
/// Longest the reader waits, after stdout closes, for the stderr drain to catch up.
const STDERR_SETTLE: Duration = Duration::from_millis(250);
/// How often a waiting request checks for cancellation.
const POLL: Duration = Duration::from_millis(50);
/// Bound for notifications and answers to server requests.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(30);

enum Failure {
    /// The server answered with a JSON-RPC error.
    Rpc(String),
    /// The connection ended before an answer.
    Closed(String),
    /// A remote request failed below JSON-RPC (HTTP status, network); the text names the server.
    Transport(String, IntegrationFailure),
}

enum Event {
    Reply(Result<Value, Failure>),
}

struct Waiter {
    reply: Sender<Event>,
    /// Progress is lossy: retain only the newest update, independently of the terminal reply.
    progress: Option<Arc<Mutex<Option<(Instant, String)>>>>,
}

#[derive(Default)]
struct State {
    pending: HashMap<u64, Waiter>,
    closed: Option<String>,
}

/// State shared by the client and every transport thread.
pub(super) struct Shared {
    state: Mutex<State>,
    stderr: Mutex<Vec<u8>>,
    /// Set once the server's stderr reached EOF.
    stderr_done: AtomicBool,
    /// Answer to `roots/list`.
    roots: Vec<PathBuf>,
    /// Where signals for the supervisor go.
    signals: Option<Sender<Signal>>,
}

pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Shared {
    pub(super) fn new(roots: Vec<PathBuf>, signals: Option<Sender<Signal>>) -> Self {
        Shared {
            state: Mutex::default(),
            stderr: Mutex::default(),
            stderr_done: AtomicBool::new(false),
            roots,
            signals,
        }
    }

    /// Records why the connection ended (first reason wins) and fails every waiting request.
    pub(super) fn close(&self, reason: String) {
        let first = {
            let mut state = lock(&self.state);
            let first = state.closed.is_none();
            let reason = state.closed.get_or_insert(reason).clone();
            for (_, waiter) in state.pending.drain() {
                let _ = waiter
                    .reply
                    .send(Event::Reply(Err(Failure::Closed(reason.clone()))));
            }
            first
        };
        if first {
            self.signal(Signal::Closed);
        }
    }

    pub(super) fn is_closed(&self) -> bool {
        lock(&self.state).closed.is_some()
    }

    pub(super) fn signal(&self, signal: Signal) {
        if let Some(sender) = &self.signals {
            let _ = sender.send(signal);
        }
    }

    /// Fails one waiting request (a remote POST that broke below JSON-RPC).
    fn fail(&self, id: u64, text: String, failure: IntegrationFailure) {
        if let Some(waiter) = lock(&self.state).pending.remove(&id) {
            let _ = waiter
                .reply
                .send(Event::Reply(Err(Failure::Transport(text, failure))));
        }
    }

    fn stderr_tail(&self) -> String {
        let tail = lock(&self.stderr);
        let text = String::from_utf8_lossy(&tail);
        // Server output reaches the model through errors: drop terminal escapes.
        let text = super::tool::strip_controls(text.trim());
        let skip = text.chars().count().saturating_sub(STDERR_QUOTE);
        text.chars().skip(skip).collect()
    }

    /// Routes one incoming message. Returns the answer when it is a request from the server.
    pub(super) fn handle(&self, message: &Value) -> Option<Value> {
        let id = message.get("id").filter(|id| !id.is_null());
        match (message.get("method").and_then(Value::as_str), id) {
            (Some(method), Some(id)) => Some(match method {
                "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                "roots/list" => {
                    json!({"jsonrpc": "2.0", "id": id, "result": {"roots": self.roots_json()}})
                }
                _ => {
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not supported by dial"}})
                }
            }),
            (Some(method), None) => {
                self.notification(method, message.get("params").unwrap_or(&Value::Null));
                None
            }
            (None, Some(id)) => {
                let id = id.as_u64()?;
                let outcome = if let Some(error) = message.get("error") {
                    let text = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error");
                    let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
                    Err(Failure::Rpc(format!(
                        "{} (code {code})",
                        super::tool::strip_controls(text)
                    )))
                } else {
                    match message.get("result") {
                        Some(result) => Ok(result.clone()),
                        None => Err(Failure::Rpc(
                            "malformed response: neither result nor error".to_owned(),
                        )),
                    }
                };
                if let Some(waiter) = lock(&self.state).pending.remove(&id) {
                    let _ = waiter.reply.send(Event::Reply(outcome));
                }
                None
            }
            _ => None,
        }
    }

    fn notification(&self, method: &str, params: &Value) {
        match method {
            "notifications/progress" => {
                let Some(token) = params.get("progressToken").and_then(Value::as_u64) else {
                    return;
                };
                if let Some(waiter) = lock(&self.state).pending.get(&token)
                    && let Some(progress) = &waiter.progress
                {
                    *lock(progress) = Some((Instant::now(), progress_text(params)));
                }
            }
            "notifications/tools/list_changed" => self.signal(Signal::ToolsChanged),
            "notifications/prompts/list_changed" => self.signal(Signal::PromptsChanged),
            "notifications/resources/list_changed" => self.signal(Signal::ResourcesChanged),
            "notifications/resources/updated"
                if params.get("uri").is_some_and(Value::is_string) =>
            {
                self.signal(Signal::ResourceUpdated);
            }
            _ => {} // logging and anything else carry nothing dial acts on
        }
    }

    fn roots_json(&self) -> Vec<Value> {
        self.roots
            .iter()
            .filter_map(|root| {
                let uri = url::Url::from_file_path(root).ok()?;
                let name = root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                Some(json!({"uri": uri.as_str(), "name": name}))
            })
            .collect()
    }
}

/// `3/10 message` from a progress notification; server text, so control characters are dropped.
pub(super) fn progress_text(params: &Value) -> String {
    let number = |value: f64| {
        if value.fract() == 0.0 && value.abs() < 1e15 {
            format!("{}", value as i64)
        } else {
            format!("{value:.1}")
        }
    };
    let progress = params.get("progress").and_then(Value::as_f64);
    let total = params.get("total").and_then(Value::as_f64);
    let mut text = match (progress, total) {
        (Some(done), Some(total)) if total > 0.0 => format!("{}/{}", number(done), number(total)),
        (Some(done), _) => number(done),
        _ => String::new(),
    };
    if let Some(message) = params.get("message").and_then(Value::as_str) {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(message);
    }
    let text: String = super::tool::strip_controls(&text)
        .replace('\n', " ")
        .chars()
        .take(300)
        .collect();
    text
}

/// A remote transport: sends one JSON-RPC message and feeds every message of the reply to `sink`.
pub(super) trait Remote: Send + Sync {
    fn post(
        &self,
        message: &Value,
        timeout: Duration,
        operation: &Arc<Operation>,
        sink: &mut dyn FnMut(Value),
    ) -> Result<()>;
    /// Called once the handshake completed (streamable HTTP opens its server-message stream).
    fn after_initialize(self: Arc<Self>, _version: &str, _shared: Arc<Shared>) {}
    /// True when replies come on a stream rather than the POST response (legacy SSE): a 202 for a
    /// POST then leaves the request waiting for its reply.
    fn replies_on_stream(&self) -> bool {
        false
    }
    /// The negotiated protocol version, for transports that echo it in headers.
    fn set_protocol(&self, _version: &str) {}
    /// The `WWW-Authenticate` challenge of the last 401, when authorization is missing.
    fn auth_challenge(&self) -> Option<String> {
        None
    }
    /// Whether the last refusal was an HTTP 401 with an OAuth (Bearer) challenge.
    fn sign_in_offered(&self) -> bool {
        false
    }
    fn shutdown(&self) {}
}

enum Transport {
    Stdio {
        child: Mutex<Child>,
        outgoing: mpsc::SyncSender<Outgoing>,
        workers: Mutex<Vec<thread::JoinHandle<()>>>,
    },
    Remote(Arc<dyn Remote>),
}

pub struct Client {
    server: String,
    transport: Transport,
    shared: Arc<Shared>,
    next_id: AtomicU64,
    capabilities: Mutex<Value>,
    instructions: Mutex<Option<String>>,
    admitted: Mutex<usize>,
}

impl Client {
    /// Starts `program` and its helper threads. No protocol traffic yet; see [`Client::initialize`].
    pub(super) fn spawn(server: &str, config: &McpServerConfig, shared: Shared) -> Result<Client> {
        let mut command = Command::new(&config.program);
        command
            .args(&config.args)
            .env_clear()
            .envs(server_env(
                std::env::vars_os().filter_map(|(name, value)| {
                    Some((name.into_string().ok()?, value.into_string().ok()?))
                }),
                &config.env,
            ))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &config.cwd {
            command.current_dir(cwd);
        }
        let mut child = crate::platform::process::spawn_group(&mut command)
            .with_context(|| format!("starting `{}`", config.program.display()))?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = crate::platform::process::kill_group(child.id());
            let _ = child.kill();
            let _ = child.wait();
            bail!("could not open the server's stdio");
        };

        let shared = Arc::new(shared);
        let (outgoing, queue) = mpsc::sync_channel::<Outgoing>(MAX_IN_FLIGHT * 2);
        // Any thread failure tears the server down: the child is otherwise dropped unreaped
        // and keeps running with its pipes closed.
        let mut workers = Vec::new();
        let spawned = (|| -> std::io::Result<()> {
            workers.push(
                thread::Builder::new()
                    .name(format!("mcp-{server}-out"))
                    .spawn({
                        let shared = shared.clone();
                        move || write_loop(stdin, queue, &shared)
                    })?,
            );
            workers.push(
                thread::Builder::new()
                    .name(format!("mcp-{server}-in"))
                    .spawn({
                        let (shared, replies) = (shared.clone(), outgoing.clone());
                        move || read_loop(stdout, &shared, &replies)
                    })?,
            );
            workers.push(
                thread::Builder::new()
                    .name(format!("mcp-{server}-err"))
                    .spawn({
                        let shared = shared.clone();
                        move || drain_stderr(stderr, &shared)
                    })?,
            );
            Ok(())
        })();
        if let Err(error) = spawned {
            shared.close("starting MCP server threads failed".into());
            let _ = crate::platform::process::kill_group(child.id());
            let _ = child.kill();
            let _ = child.wait();
            for worker in workers {
                let _ = worker.join();
            }
            return Err(error).context("starting MCP server threads");
        }

        Ok(Client::new(
            server,
            Transport::Stdio {
                child: Mutex::new(child),
                outgoing,
                workers: Mutex::new(workers),
            },
            shared,
        ))
    }

    /// A remote server; nothing is sent until [`Client::initialize`].
    pub(super) fn remote(server: &str, remote: Arc<dyn Remote>, shared: Arc<Shared>) -> Client {
        Client::new(server, Transport::Remote(remote), shared)
    }

    fn new(server: &str, transport: Transport, shared: Arc<Shared>) -> Client {
        Client {
            server: server.to_owned(),
            transport,
            shared,
            next_id: AtomicU64::new(1),
            capabilities: Mutex::new(Value::Null),
            instructions: Mutex::default(),
            admitted: Mutex::new(0),
        }
    }

    /// The `initialize` handshake: offers the newest protocol version, accepts any version this
    /// client knows, then sends `notifications/initialized`.
    pub fn initialize(&self, timeout: Duration, cancel: &dyn Fn() -> bool) -> Result<()> {
        let deadline = (!timeout.is_zero()).then(|| Instant::now() + timeout);
        let result = self.request_with(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSIONS[0],
                "capabilities": {"roots": {"listChanged": false}},
                "clientInfo": {"name": "dial", "version": env!("CARGO_PKG_VERSION")},
            }),
            timeout,
            cancel,
            None,
        )?;
        let version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !PROTOCOL_VERSIONS.contains(&version) {
            bail!(
                "server speaks unsupported protocol version {version:?} (supported: {})",
                PROTOCOL_VERSIONS.join(", ")
            );
        }
        if let Transport::Remote(remote) = &self.transport {
            remote.set_protocol(version);
        }
        let remaining = deadline.map_or(NOTIFY_TIMEOUT, |deadline| {
            deadline.saturating_duration_since(Instant::now())
        });
        if remaining.is_zero() {
            bail!("MCP initialize timed out before initialized notification delivery");
        }
        self.notify_with("notifications/initialized", json!({}), remaining, cancel)?;
        *lock(&self.capabilities) = result.get("capabilities").cloned().unwrap_or(Value::Null);
        *lock(&self.instructions) = result
            .get("instructions")
            .and_then(Value::as_str)
            .map(str::to_owned);

        if let Transport::Remote(remote) = &self.transport {
            remote
                .clone()
                .after_initialize(version, self.shared.clone());
        }
        Ok(())
    }

    /// Whether the server declared `capability` (`tools`, `prompts`, `resources`, ...).
    pub fn has_capability(&self, capability: &str) -> bool {
        lock(&self.capabilities)
            .get(capability)
            .is_some_and(Value::is_object)
    }

    /// Whether `resources.subscribe` was declared.
    pub fn can_subscribe(&self) -> bool {
        lock(&self.capabilities)
            .pointer("/resources/subscribe")
            .and_then(Value::as_bool)
            == Some(true)
    }

    /// The server's `initialize` capabilities object (`Null` before initialization).
    pub fn capabilities(&self) -> Value {
        lock(&self.capabilities).clone()
    }

    pub fn instructions(&self) -> Option<String> {
        lock(&self.instructions).clone()
    }

    pub fn is_closed(&self) -> bool {
        self.shared.is_closed()
    }

    pub fn auth_challenge(&self) -> Option<String> {
        match &self.transport {
            Transport::Remote(remote) => remote.auth_challenge(),
            Transport::Stdio { .. } => None,
        }
    }

    /// Whether the server refused with an HTTP 401 that `/mcp login` can answer. The request
    /// thread reports failures as text, so this is read from the transport, not the error.
    pub fn sign_in_offered(&self) -> bool {
        match &self.transport {
            Transport::Remote(remote) => remote.sign_in_offered(),
            Transport::Stdio { .. } => false,
        }
    }

    /// Sends a request with a progress-inactivity `timeout` (zero disables inactivity expiry).
    /// Every request has a separate one-hour absolute lifetime, including authentication/dispatch.
    pub fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        self.request_with(method, params, timeout, &|| false, None)
    }

    /// [`Client::request`] that gives up when `cancel` turns true, and reports progress
    /// notifications to `progress` (asking the server for them by sending a progress token).
    pub fn request_with(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        cancel: &dyn Fn() -> bool,
        progress: Option<&dyn Fn(&str)>,
    ) -> Result<Value> {
        self.request_with_limit(
            method,
            params,
            timeout,
            cancel,
            progress,
            MAX_REQUEST_LIFETIME,
        )
    }

    fn request_with_limit(
        &self,
        method: &str,
        mut params: Value,
        timeout: Duration,
        cancel: &dyn Fn() -> bool,
        progress: Option<&dyn Fn(&str)>,
        absolute_limit: Duration,
    ) -> Result<Value> {
        let operation = Operation::new(absolute_limit);
        if cancel() {
            return Err(anyhow!(
                "MCP request `{method}` to `{}` was cancelled before sending",
                self.server
            )
            .context(RequestFailure {
                dispatched: false,
                settlement: Settlement::NotStarted,
                failure: IntegrationFailure::Cancelled,
            }));
        }
        let _admission = self.admit()?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if progress.is_some()
            && let Some(object) = params.as_object_mut()
        {
            let meta = object.entry("_meta").or_insert_with(|| json!({}));
            if let Some(meta) = meta.as_object_mut() {
                meta.insert("progressToken".into(), json!(id));
            }
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        let latest_progress = progress.map(|_| Arc::new(Mutex::new(None)));
        {
            let mut state = lock(&self.shared.state);
            if let Some(reason) = &state.closed {
                return Err(self.closed_error(reason).context(RequestFailure {
                    dispatched: false,
                    settlement: Settlement::NotStarted,
                    failure: IntegrationFailure::Connection,
                }));
            }
            state.pending.insert(
                id,
                Waiter {
                    reply: reply_tx,
                    progress: latest_progress.clone(),
                },
            );
        }
        struct Pending<'a>(&'a Client, u64, Arc<Operation>);
        impl Drop for Pending<'_> {
            fn drop(&mut self) {
                self.0.forget(self.1);
                // Every return and unwind settles stdio work too, not only explicit abandon.
                self.0.stop_operation(&self.2);
            }
        }
        let _pending = Pending(self, id, operation.clone());
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let owned = match self.send(message, timeout, operation.clone(), None) {
            Ok(owned) => owned,
            Err(error) => {
                self.forget(id);
                return Err(error.context(RequestFailure {
                    dispatched: false,
                    settlement: Settlement::NotStarted,
                    failure: IntegrationFailure::Admission,
                }));
            }
        };
        let _owned = owned;
        let mut deadline = (!timeout.is_zero()).then(|| Instant::now() + timeout);
        loop {
            let wait = deadline.map_or(POLL, |end| {
                end.saturating_duration_since(Instant::now()).min(POLL)
            });
            match reply_rx.recv_timeout(wait) {
                Ok(Event::Reply(Ok(value))) => return Ok(value),
                Ok(Event::Reply(Err(Failure::Rpc(message)))) => {
                    return Err(anyhow!(
                        "MCP server `{}` returned an error: {message}",
                        self.server
                    )
                    .context(RequestEffects(Effects::Known))
                    .context(RequestFailure {
                        dispatched: true,
                        settlement: Settlement::Reply,
                        failure: IntegrationFailure::Remote,
                    }));
                }
                Ok(Event::Reply(Err(Failure::Closed(reason)))) => {
                    return Err(self
                        .closed_error(&reason)
                        .context(RequestEffects(
                            if matches!(operation.phase(), Phase::Queued | Phase::Cancelled) {
                                Effects::None
                            } else {
                                Effects::Unknown
                            },
                        ))
                        .context(RequestFailure {
                            dispatched: matches!(
                                operation.phase(),
                                Phase::Dispatching | Phase::Delivered
                            ),
                            settlement: Settlement::TransportLost,
                            failure: IntegrationFailure::Connection,
                        }));
                }
                Ok(Event::Reply(Err(Failure::Transport(text, failure)))) => {
                    return Err(anyhow!(text)
                        .context(RequestEffects(
                            if matches!(operation.phase(), Phase::Queued | Phase::Cancelled) {
                                Effects::None
                            } else {
                                Effects::Unknown
                            },
                        ))
                        .context(RequestFailure {
                            dispatched: matches!(
                                operation.phase(),
                                Phase::Dispatching | Phase::Delivered
                            ),
                            settlement: if matches!(
                                operation.phase(),
                                Phase::Queued | Phase::Cancelled
                            ) {
                                Settlement::NotStarted
                            } else {
                                Settlement::TransportLost
                            },
                            failure,
                        }));
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(self
                        .closed_error("the connection is closed")
                        .context(RequestEffects(
                            if matches!(operation.phase(), Phase::Queued | Phase::Cancelled) {
                                Effects::None
                            } else {
                                Effects::Unknown
                            },
                        ))
                        .context(RequestFailure {
                            dispatched: matches!(
                                operation.phase(),
                                Phase::Dispatching | Phase::Delivered
                            ),
                            settlement: Settlement::TransportLost,
                            failure: IntegrationFailure::Connection,
                        }));
                }
            }
            // The reply channel contains terminal replies only; progress cannot queue ahead of
            // settlement or block the transport reader, even when the callback is slow.
            let update = latest_progress.as_ref().and_then(|slot| lock(slot).take());
            if let Some((received, text)) = update {
                // Use arrival time, not callback time: stale progress cannot extend a timeout.
                deadline = (!timeout.is_zero()).then(|| received + timeout);
                if let Some(progress) = progress {
                    progress(&text);
                }
            }
            // Checked on every wake-up: steady progress must not starve cancel or the deadline.
            if cancel() {
                let dispatched = self.abandon(id, "cancelled by the user", &operation);
                return Err(
                    anyhow!("MCP request `{method}` to `{}` was cancelled", self.server)
                        .context(RequestEffects(if dispatched {
                            Effects::Unknown
                        } else {
                            Effects::None
                        }))
                        .context(RequestFailure {
                            dispatched,
                            settlement: Settlement::Cancelled,
                            failure: IntegrationFailure::Cancelled,
                        }),
                );
            }
            if deadline.is_some_and(|end| Instant::now() >= end)
                || Instant::now() >= operation.deadline
            {
                let dispatched = self.abandon(id, "timed out", &operation);
                return Err(anyhow!(
                    "MCP server `{}` did not answer `{method}` within {}s",
                    self.server,
                    timeout.as_secs().max(1)
                )
                .context(RequestEffects(if dispatched {
                    Effects::Unknown
                } else {
                    Effects::None
                }))
                .context(RequestFailure {
                    dispatched,
                    settlement: Settlement::TimedOut,
                    failure: IntegrationFailure::Timeout,
                }));
            }
        }
    }

    pub fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.notify_with(method, params, NOTIFY_TIMEOUT, &|| false)
    }

    fn notify_with(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        cancel: &dyn Fn() -> bool,
    ) -> Result<()> {
        if cancel() {
            bail!("MCP notification `{method}` cancelled before admission");
        }
        let _admission = self.admit()?;
        let lifetime = if timeout.is_zero() {
            NOTIFY_TIMEOUT
        } else {
            timeout.min(NOTIFY_TIMEOUT)
        };
        let operation = Operation::new(lifetime);
        let (tx, rx) = mpsc::channel();
        let _owned = self.send(
            json!({"jsonrpc": "2.0", "method": method, "params": params}),
            lifetime,
            operation.clone(),
            Some(tx),
        )?;
        loop {
            if cancel() || operation.stopped() {
                self.stop_operation(&operation);
                bail!("MCP notification `{method}` cancelled or timed out before delivery");
            }
            match rx.recv_timeout(POLL) {
                Ok(result) => return result,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("MCP notification delivery worker ended")
                }
            }
        }
    }

    fn send(
        &self,
        message: Value,
        timeout: Duration,
        operation: Arc<Operation>,
        delivered: Option<Sender<Result<()>>>,
    ) -> Result<OwnedSend> {
        let worker = match &self.transport {
            Transport::Stdio { outgoing, .. } => {
                outgoing
                    .try_send(Outgoing {
                        message,
                        operation: operation.clone(),
                        delivered,
                    })
                    .map_err(|_| self.closed_error("the outgoing queue is full or closed"))?;
                None
            }
            Transport::Remote(remote) => {
                let (remote, shared, operation) =
                    (remote.clone(), self.shared.clone(), operation.clone());
                let id = message.get("id").and_then(Value::as_u64);
                Some(
                    thread::Builder::new()
                        .name(format!("mcp-{}-post", self.server))
                        .spawn(move || {
                            // Transport performs its dispatch transition after cancellable authentication.
                            let result = if operation.stopped() {
                                Err(anyhow!("request cancelled before dispatch"))
                            } else {
                                remote.post(&message, timeout, &operation, &mut |reply| {
                                    if let Some(answer) = shared.handle(&reply) {
                                        let _ = remote.post(
                                            &answer,
                                            NOTIFY_TIMEOUT,
                                            &operation,
                                            &mut |_| {},
                                        );
                                    }
                                })
                            };
                            if let Some(delivered) = delivered {
                                let _ = delivered.send(result);
                                return;
                            }
                            match result {
                                Err(error)
                                    if error
                                        .downcast_ref::<super::http::SessionEnded>()
                                        .is_some() =>
                                {
                                    shared.close(format!("{error:#}"))
                                }
                                Err(error) => {
                                    if let Some(id) = id {
                                        let failure = if error
                                            .downcast_ref::<super::http::AuthRequired>()
                                            .is_some()
                                        {
                                            IntegrationFailure::Authentication
                                        } else {
                                            IntegrationFailure::Connection
                                        };
                                        shared.fail(id, format!("{error:#}"), failure);
                                    }
                                }
                                Ok(()) if remote.replies_on_stream() => {}
                                Ok(()) => {
                                    if let Some(id) = id {
                                        shared.fail(
                                            id,
                                            "MCP server closed the response without answering"
                                                .to_owned(),
                                            IntegrationFailure::Protocol,
                                        );
                                    }
                                }
                            }
                        })
                        .context("starting request thread")?,
                )
            }
        };
        Ok(OwnedSend { operation, worker })
    }

    fn stop_operation(&self, operation: &Operation) -> Phase {
        let previous = operation.cancel();
        // A blocked stdio write cannot be independently interrupted. Tear down this connection
        // rather than leave a writer capable of dispatching after cancellation returns.
        if previous == Phase::Dispatching {
            if let Transport::Stdio { child, .. } = &self.transport {
                let mut child = lock(child);
                self.shared
                    .close("cancelled during a blocked stdio write".into());
                let _ = crate::platform::process::kill_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                while operation.phase() == Phase::Dispatching {
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
        previous
    }

    fn abandon(&self, id: u64, reason: &str, operation: &Operation) -> bool {
        self.forget(id);
        let phase = self.stop_operation(operation);
        let dispatched = matches!(phase, Phase::Dispatching | Phase::Delivered);
        if dispatched && !self.is_closed() {
            // Best effort, bounded and settled; this does not assert remote rollback.
            let _ = self.notify_with(
                "notifications/cancelled",
                json!({"requestId": id, "reason": reason}),
                Duration::from_secs(2),
                &|| false,
            );
        }
        dispatched
    }

    fn admit(&self) -> Result<lifecycle::Admission<'_>> {
        lifecycle::Admission::acquire(&self.admitted).ok_or_else(|| {
            anyhow!("MCP request admission limit reached").context(RequestFailure {
                dispatched: false,
                settlement: Settlement::NotStarted,
                failure: IntegrationFailure::Admission,
            })
        })
    }

    fn forget(&self, id: u64) {
        lock(&self.shared.state).pending.remove(&id);
    }

    fn closed_error(&self, reason: &str) -> anyhow::Error {
        let tail = self.shared.stderr_tail();
        if tail.is_empty() {
            anyhow!("MCP server `{}` is not running: {reason}", self.server)
        } else {
            anyhow!(
                "MCP server `{}` is not running: {reason}; its stderr ended with: {tail}",
                self.server
            )
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        match &self.transport {
            Transport::Stdio { child, workers, .. } => {
                self.shared.close("the client was dropped".into());
                let mut child = lock(child);
                let _ = crate::platform::process::kill_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                for worker in lock(workers).drain(..) {
                    let _ = worker.join();
                }
            }
            Transport::Remote(remote) => remote.shutdown(),
        }
    }
}

/// The server's environment: this process's, minus likely credentials ([`is_secret_env`]), plus the
/// operator's explicit `env` entries (which win and are passed through untouched).
pub(super) fn server_env(
    inherited: impl Iterator<Item = (String, String)>,
    explicit: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut env: BTreeMap<String, String> = inherited
        .filter(|(name, value)| !is_secret_env(name, value))
        .collect();
    env.extend(
        explicit
            .iter()
            .map(|(name, value)| (name.clone(), value.clone())),
    );
    env.into_iter().collect()
}

fn write_loop(mut stdin: ChildStdin, queue: mpsc::Receiver<Outgoing>, shared: &Shared) {
    while !shared.is_closed() {
        let outgoing = match queue.recv_timeout(POLL) {
            Ok(outgoing) => outgoing,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if !outgoing.operation.dispatch() {
            continue;
        }
        let line = format!("{}\n", outgoing.message);
        let result = stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush());
        outgoing.operation.delivered();
        if let Some(delivered) = outgoing.delivered {
            let _ = delivered.send(
                result
                    .as_ref()
                    .map(|_| ())
                    .map_err(|error| anyhow!("writing to server failed: {error}")),
            );
        }
        if let Err(error) = result {
            shared.close(format!("writing to the server failed ({error})"));
            return;
        }
    }
}

fn drain_stderr(mut stderr: impl Read, shared: &Shared) {
    let mut chunk = [0u8; 2048];
    while let Ok(n) = stderr.read(&mut chunk) {
        if n == 0 {
            break;
        }
        let mut tail = lock(&shared.stderr);
        tail.extend_from_slice(&chunk[..n]);
        let excess = tail.len().saturating_sub(STDERR_TAIL);
        tail.drain(..excess);
    }
    shared.stderr_done.store(true, Ordering::Release);
}

fn read_loop(stdout: impl Read, shared: &Shared, replies: &mpsc::SyncSender<Outgoing>) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    let mut skipping = false;
    loop {
        let before = line.len();
        // `take` bounds how much of one line is buffered; the rest of an oversized line is skipped.
        let read = (&mut reader)
            .take((MAX_LINE - before) as u64 + 1)
            .read_until(b'\n', &mut line);
        match read {
            Ok(0) => break,
            Ok(_) if line.ends_with(b"\n") => {
                if !skipping
                    && let Ok(message) = serde_json::from_slice::<Value>(&line)
                    && let Some(answer) = shared.handle(&message)
                {
                    if replies
                        .try_send(Outgoing {
                            message: answer,
                            operation: Operation::new(NOTIFY_TIMEOUT),
                            delivered: None,
                        })
                        .is_err()
                    {
                        shared.close("outgoing server-answer queue is full".into());
                        return;
                    }
                }
                skipping = false;
                line.clear();
            }
            Ok(_) if line.len() > MAX_LINE => {
                skipping = true;
                line.clear();
            }
            Ok(_) => {} // a partial line (the take limit was not hit, so this is EOF next)
            Err(_) => break,
        }
    }
    // Let the stderr drain catch up so the error can quote why the server died.
    let settle_until = Instant::now() + STDERR_SETTLE;
    while !shared.stderr_done.load(Ordering::Acquire) && Instant::now() < settle_until {
        thread::sleep(Duration::from_millis(5));
    }
    shared.close("its output closed (the process exited)".into());
}

#[cfg(test)]
mod tests;

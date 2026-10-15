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
//! short-lived thread. A waiting request polls its cancellation check, so Esc and timeouts both
//! send `notifications/cancelled` and return at once. When the connection ends every waiting request
//! fails with the reason, so a crash surfaces as a clear error, never a hang.

use super::Signal;
use crate::config::types::McpServerConfig;
use crate::runtime::execution::Effects;
use crate::tools::sandbox::is_secret_env;
use anyhow::{Context, Result, anyhow, bail};
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
    Transport(String),
}

enum Event {
    Reply(Result<Value, Failure>),
    Progress(String),
}

#[derive(Default)]
struct State {
    pending: HashMap<u64, Sender<Event>>,
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
    /// Where signals for the supervisor go, tagged with this connection's epoch.
    signals: Option<(u64, Sender<(u64, Signal)>)>,
}

pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Shared {
    pub(super) fn new(roots: Vec<PathBuf>, signals: Option<(u64, Sender<(u64, Signal)>)>) -> Self {
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
                let _ = waiter.send(Event::Reply(Err(Failure::Closed(reason.clone()))));
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

    fn signal(&self, signal: Signal) {
        if let Some((epoch, sender)) = &self.signals {
            let _ = sender.send((*epoch, signal));
        }
    }

    /// Fails one waiting request (a remote POST that broke below JSON-RPC).
    fn fail(&self, id: u64, text: String) {
        if let Some(waiter) = lock(&self.state).pending.remove(&id) {
            let _ = waiter.send(Event::Reply(Err(Failure::Transport(text))));
        }
    }

    fn stderr_tail(&self) -> String {
        let tail = lock(&self.stderr);
        let text = String::from_utf8_lossy(&tail);
        let text = text.trim();
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
                    Err(Failure::Rpc(format!("{text} (code {code})")))
                } else {
                    match message.get("result") {
                        Some(result) => Ok(result.clone()),
                        None => Err(Failure::Rpc(
                            "malformed response: neither result nor error".to_owned(),
                        )),
                    }
                };
                if let Some(waiter) = lock(&self.state).pending.remove(&id) {
                    let _ = waiter.send(Event::Reply(outcome));
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
                if let Some(waiter) = lock(&self.state).pending.get(&token) {
                    let _ = waiter.send(Event::Progress(progress_text(params)));
                }
            }
            "notifications/tools/list_changed" => self.signal(Signal::ToolsChanged),
            "notifications/prompts/list_changed" => self.signal(Signal::PromptsChanged),
            "notifications/resources/list_changed" => self.signal(Signal::ResourcesChanged),
            "notifications/resources/updated" => {
                if params.get("uri").is_some_and(Value::is_string) {
                    self.signal(Signal::ResourceUpdated);
                }
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
    fn post(&self, message: &Value, timeout: Duration, sink: &mut dyn FnMut(Value)) -> Result<()>;
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
    fn shutdown(&self) {}
}

enum Transport {
    Stdio {
        child: Mutex<Child>,
        outgoing: Sender<String>,
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
        let (outgoing, queue) = mpsc::channel::<String>();
        // Any thread failure tears the server down: the child is otherwise dropped unreaped
        // and keeps running with its pipes closed.
        let spawned = (|| -> std::io::Result<()> {
            thread::Builder::new()
                .name(format!("mcp-{server}-out"))
                .spawn({
                    let shared = shared.clone();
                    move || write_loop(stdin, queue, &shared)
                })?;
            thread::Builder::new()
                .name(format!("mcp-{server}-in"))
                .spawn({
                    let (shared, replies) = (shared.clone(), outgoing.clone());
                    move || read_loop(stdout, &shared, &replies)
                })?;
            thread::Builder::new()
                .name(format!("mcp-{server}-err"))
                .spawn({
                    let shared = shared.clone();
                    move || drain_stderr(stderr, &shared)
                })?;
            Ok(())
        })();
        if let Err(error) = spawned {
            let _ = crate::platform::process::kill_group(child.id());
            let _ = child.kill();
            let _ = child.wait();
            return Err(error).context("starting MCP server threads");
        }

        Ok(Client::new(
            server,
            Transport::Stdio {
                child: Mutex::new(child),
                outgoing,
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
        }
    }

    /// The `initialize` handshake: offers the newest protocol version, accepts any version this
    /// client knows, then sends `notifications/initialized`.
    pub fn initialize(&self, timeout: Duration, cancel: &dyn Fn() -> bool) -> Result<()> {
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
        *lock(&self.capabilities) = result.get("capabilities").cloned().unwrap_or(Value::Null);
        *lock(&self.instructions) = result
            .get("instructions")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Transport::Remote(remote) = &self.transport {
            remote.set_protocol(version);
        }
        self.notify("notifications/initialized", json!({}))?;
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
            .is_some_and(|value| !value.is_null())
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

    /// Sends a request and waits for its response, at most `timeout` (zero waits indefinitely).
    pub fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        self.request_with(method, params, timeout, &|| false, None)
    }

    /// [`Client::request`] that gives up when `cancel` turns true, and reports progress
    /// notifications to `progress` (asking the server for them by sending a progress token).
    pub fn request_with(
        &self,
        method: &str,
        mut params: Value,
        timeout: Duration,
        cancel: &dyn Fn() -> bool,
        progress: Option<&dyn Fn(&str)>,
    ) -> Result<Value> {
        if cancel() {
            bail!(
                "MCP request `{method}` to `{}` was cancelled before sending",
                self.server
            );
        }
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
        {
            let mut state = lock(&self.shared.state);
            if let Some(reason) = &state.closed {
                return Err(self.closed_error(reason));
            }
            state.pending.insert(id, reply_tx);
        }
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(error) = self.send(message, timeout) {
            self.forget(id);
            return Err(error);
        }
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
                    .context(RequestEffects(Effects::Known)));
                }
                Ok(Event::Reply(Err(Failure::Closed(reason)))) => {
                    return Err(self
                        .closed_error(&reason)
                        .context(RequestEffects(Effects::Unknown)));
                }
                Ok(Event::Reply(Err(Failure::Transport(text)))) => {
                    return Err(anyhow!(text).context(RequestEffects(Effects::Unknown)));
                }
                Ok(Event::Progress(text)) => {
                    // Progress shows the server is working; it restarts the inactivity clock.
                    deadline = (!timeout.is_zero()).then(|| Instant::now() + timeout);
                    if let Some(progress) = progress {
                        progress(&text);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(self
                        .closed_error("the connection is closed")
                        .context(RequestEffects(Effects::Unknown)));
                }
            }
            // Checked on every wake-up: steady progress must not starve cancel or the deadline.
            if cancel() {
                self.abandon(id, "cancelled by the user");
                return Err(
                    anyhow!("MCP request `{method}` to `{}` was cancelled", self.server)
                        .context(RequestEffects(Effects::Unknown)),
                );
            }
            if deadline.is_some_and(|end| Instant::now() >= end) {
                self.abandon(id, "timed out");
                return Err(anyhow!(
                    "MCP server `{}` did not answer `{method}` within {}s",
                    self.server,
                    timeout.as_secs().max(1)
                )
                .context(RequestEffects(Effects::Unknown)));
            }
        }
    }

    pub fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send(
            json!({"jsonrpc": "2.0", "method": method, "params": params}),
            NOTIFY_TIMEOUT,
        )
    }

    fn send(&self, message: Value, timeout: Duration) -> Result<()> {
        match &self.transport {
            Transport::Stdio { outgoing, .. } => outgoing
                .send(message.to_string())
                .map_err(|_| self.closed_error("the connection is closed")),
            Transport::Remote(remote) => {
                let (remote, shared) = (remote.clone(), self.shared.clone());
                let id = message.get("id").and_then(Value::as_u64);
                thread::Builder::new()
                    .name(format!("mcp-{}-post", self.server))
                    .spawn(move || {
                        let mut answers = Vec::new();
                        let result = remote.post(&message, timeout, &mut |reply| {
                            answers.extend(shared.handle(&reply));
                        });
                        for answer in answers {
                            let _ = remote.post(&answer, NOTIFY_TIMEOUT, &mut |_| {});
                        }
                        match result {
                            Err(error)
                                if error.downcast_ref::<super::http::SessionEnded>().is_some() =>
                            {
                                shared.close(format!("{error:#}"))
                            }
                            Err(error) => {
                                if let Some(id) = id {
                                    shared.fail(id, format!("{error:#}"));
                                }
                            }
                            Ok(()) if remote.replies_on_stream() => {}
                            Ok(()) => {
                                if let Some(id) = id {
                                    shared.fail(
                                        id,
                                        "MCP server closed the response without answering"
                                            .to_owned(),
                                    );
                                }
                            }
                        }
                    })
                    .context("starting request thread")?;
                Ok(())
            }
        }
    }

    /// Stops waiting for `id` and tells the server, so it can stop working on it.
    fn abandon(&self, id: u64, reason: &str) {
        self.forget(id);
        let _ = self.notify(
            "notifications/cancelled",
            json!({"requestId": id, "reason": reason}),
        );
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
            Transport::Stdio { child, .. } => {
                let mut child = lock(child);
                let _ = crate::platform::process::kill_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
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

fn write_loop(mut stdin: ChildStdin, queue: mpsc::Receiver<String>, shared: &Shared) {
    for mut line in queue {
        line.push('\n');
        if let Err(error) = stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush())
        {
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

fn read_loop(stdout: impl Read, shared: &Shared, replies: &Sender<String>) {
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
                    let _ = replies.send(answer.to_string());
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

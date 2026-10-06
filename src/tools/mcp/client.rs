//! A JSON-RPC 2.0 client over a child process's stdio (newline-delimited messages).
//!
//! Three helper threads keep the caller from ever blocking on the server:
//! * the **reader** parses stdout lines and routes each response to the waiting request by id; it
//!   answers server-initiated requests (`ping`, anything else is "method not found") and ignores
//!   notifications and lines that are not JSON;
//! * the **writer** owns stdin, so a server that stops reading cannot stall a caller;
//! * the **stderr drain** keeps the last few KiB for error messages and discards the rest.
//!
//! When the server's stdout closes every waiting request fails at once and later requests fail
//! immediately with the reason, so a crash surfaces as a clear error, never a hang.

use crate::config::types::McpServerConfig;
use crate::tools::sandbox::is_secret_env;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

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

enum Failure {
    /// The server answered with a JSON-RPC error.
    Rpc(String),
    /// The connection ended before an answer.
    Closed(String),
}

type Reply = Result<Value, Failure>;

#[derive(Default)]
struct State {
    pending: HashMap<u64, Sender<Reply>>,
    closed: Option<String>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    stderr: Mutex<Vec<u8>>,
    /// Set once the server's stderr reached EOF.
    stderr_done: AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Shared {
    /// Records why the connection ended (first reason wins) and fails every waiting request.
    fn close(&self, reason: String) {
        let mut state = lock(&self.state);
        let reason = state.closed.get_or_insert(reason).clone();
        for (_, waiter) in state.pending.drain() {
            let _ = waiter.send(Err(Failure::Closed(reason.clone())));
        }
    }

    fn stderr_tail(&self) -> String {
        let tail = lock(&self.stderr);
        let text = String::from_utf8_lossy(&tail);
        let text = text.trim();
        let skip = text.chars().count().saturating_sub(STDERR_QUOTE);
        text.chars().skip(skip).collect()
    }
}

pub struct Client {
    server: String,
    /// Stdio transport: the child and its writer queue. `None` for HTTP servers.
    child: Option<Mutex<Child>>,
    outgoing: Option<Sender<String>>,
    http: Option<super::http::Http>,
    shared: Arc<Shared>,
    next_id: AtomicU64,
}

impl Client {
    /// Starts `program` and its helper threads. No protocol traffic yet; see [`Client::initialize`].
    pub fn spawn(server: &str, config: &McpServerConfig) -> Result<Client> {
        let mut command = Command::new(&config.program);
        command
            .args(&config.args)
            .env_clear()
            .envs(server_env(std::env::vars(), &config.env))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group, so dropping the client also kills what an `npx`/shell wrapper started.
            .process_group(0);
        let mut child = command
            .spawn()
            .with_context(|| format!("starting `{}`", config.program.display()))?;
        let (stdin, stdout, stderr) =
            match (child.stdin.take(), child.stdout.take(), child.stderr.take()) {
                (Some(stdin), Some(stdout), Some(stderr)) => (stdin, stdout, stderr),
                _ => bail!("could not open the server's stdio"),
            };

        let shared = Arc::new(Shared::default());
        let (outgoing, queue) = mpsc::channel::<String>();
        thread::Builder::new()
            .name(format!("mcp-{server}-out"))
            .spawn({
                let shared = shared.clone();
                move || write_loop(stdin, queue, &shared)
            })
            .context("starting writer thread")?;
        thread::Builder::new()
            .name(format!("mcp-{server}-in"))
            .spawn({
                let (shared, replies) = (shared.clone(), outgoing.clone());
                move || read_loop(stdout, &shared, &replies)
            })
            .context("starting reader thread")?;
        thread::Builder::new()
            .name(format!("mcp-{server}-err"))
            .spawn({
                let shared = shared.clone();
                move || drain_stderr(stderr, &shared)
            })
            .context("starting stderr thread")?;

        Ok(Client {
            server: server.to_owned(),
            child: Some(Mutex::new(child)),
            outgoing: Some(outgoing),
            http: None,
            shared,
            next_id: AtomicU64::new(1),
        })
    }

    /// A streamable-HTTP server; nothing is sent until [`Client::initialize`].
    pub fn connect_http(server: &str, config: &McpServerConfig) -> Result<Client> {
        let url = config
            .url
            .as_deref()
            .context("HTTP MCP server needs a url")?;
        let http =
            super::http::Http::new(server, url, config.bearer_env.as_deref(), &config.headers)?;
        Ok(Client {
            server: server.to_owned(),
            child: None,
            outgoing: None,
            http: Some(http),
            shared: Arc::new(Shared::default()),
            next_id: AtomicU64::new(1),
        })
    }

    /// The `initialize` handshake: offers the newest protocol version, accepts any version this
    /// client knows, then sends `notifications/initialized`.
    pub fn initialize(&self, timeout: Duration) -> Result<()> {
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSIONS[0],
                "capabilities": {},
                "clientInfo": {"name": "dial", "version": env!("CARGO_PKG_VERSION")},
            }),
            timeout,
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
        if let Some(http) = &self.http {
            http.set_protocol(version);
        }
        self.notify("notifications/initialized", json!({}))
    }

    /// Sends a request and waits for its response, at most `timeout`.
    pub fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if let Some(http) = &self.http {
            let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
            return match http.request(id, &message, timeout)? {
                Ok(value) => Ok(value),
                Err(text) => Err(anyhow!(
                    "MCP server `{}` returned an error: {text}",
                    self.server
                )),
            };
        }
        let Some(outgoing) = &self.outgoing else {
            return Err(self.closed_error("the connection is closed"));
        };
        let (reply_tx, reply_rx) = mpsc::channel();
        {
            let mut state = lock(&self.shared.state);
            if let Some(reason) = &state.closed {
                return Err(self.closed_error(reason));
            }
            state.pending.insert(id, reply_tx);
        }
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if outgoing.send(message.to_string()).is_err() {
            self.forget(id);
            return Err(self.closed_error("the connection is closed"));
        }
        match reply_rx.recv_timeout(timeout) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(Failure::Rpc(message))) => Err(anyhow!(
                "MCP server `{}` returned an error: {message}",
                self.server
            )),
            Ok(Err(Failure::Closed(reason))) => Err(self.closed_error(&reason)),
            Err(RecvTimeoutError::Timeout) => {
                self.forget(id);
                let _ = self.notify(
                    "notifications/cancelled",
                    json!({"requestId": id, "reason": "timed out"}),
                );
                Err(anyhow!(
                    "MCP server `{}` did not answer `{method}` within {}s",
                    self.server,
                    timeout.as_secs().max(1)
                ))
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(self.closed_error("the connection is closed"))
            }
        }
    }

    pub fn notify(&self, method: &str, params: Value) -> Result<()> {
        let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
        if let Some(http) = &self.http {
            return http.notify(&message);
        }
        self.outgoing
            .as_ref()
            .ok_or_else(|| self.closed_error("the connection is closed"))?
            .send(message.to_string())
            .map_err(|_| self.closed_error("the connection is closed"))
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
        let Some(child) = &self.child else { return };
        let mut child = child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // The leader is not reaped until after killpg, so its PID cannot be reused in between.
        unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
        let _ = child.kill();
        let _ = child.wait();
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
                if !skipping {
                    handle_line(&line, shared, replies);
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

fn handle_line(line: &[u8], shared: &Shared, replies: &Sender<String>) {
    let Ok(message) = serde_json::from_slice::<Value>(line) else {
        return;
    };
    let id = message.get("id").filter(|id| !id.is_null());
    match (message.get("method").and_then(Value::as_str), id) {
        (Some(method), Some(id)) => {
            let reply = match method {
                "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                _ => {
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not supported by dial"}})
                }
            };
            let _ = replies.send(reply.to_string());
        }
        (None, Some(id)) => {
            let Some(id) = id.as_u64() else { return };
            let outcome = if let Some(error) = message.get("error") {
                let text = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
                Err(Failure::Rpc(format!("{text} (code {code})")))
            } else if let Some(result) = message.get("result") {
                Ok(result.clone())
            } else {
                return;
            };
            if let Some(waiter) = lock(&shared.state).pending.remove(&id) {
                let _ = waiter.send(outcome);
            }
        }
        _ => {} // notifications (logging, progress, list_changed) carry nothing dial acts on
    }
}

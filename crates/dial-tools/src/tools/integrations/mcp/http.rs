//! MCP "streamable HTTP" transport: every message is a POST to one endpoint.
//!
//! The server answers a request with either a JSON body or a `text/event-stream` carrying the
//! response (and possibly progress notifications or requests, read as they arrive). A session id
//! handed out on `initialize` is echoed on every later call, and once initialized a GET stream is
//! opened for server-initiated messages (`list_changed`, resource updates); servers that do not
//! offer one answer 405 and nothing more is tried. Credentials come from `bearer_env` (an
//! environment variable name, so the secret never sits in the config file), non-secret `headers`,
//! or tokens from an OAuth sign-in (`/mcp login`, see `oauth`).
//!
//! Legacy HTTP+SSE (`transport = "sse"`) uses this module too: the GET stream is opened first, its
//! `endpoint` event names the POST target, and replies arrive on that stream.
use crate::config::tools::mcp::validate_url;

use super::client::{Remote, Shared};
use super::oauth::TokenSource;
use crate::config::types::{McpServerConfig, McpTransport};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{
    Arc, Mutex,
    mpsc::{self, RecvTimeoutError},
};
use std::time::{Duration, Instant};

/// Largest response body accepted.
const MAX_BODY: u64 = 16 * 1024 * 1024;
/// Bound for answers posted back to the server from the GET stream.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);
/// How often a waiting connect checks for cancellation.
const CANCEL_POLL: Duration = Duration::from_millis(50);
/// Longest single line accepted from an event stream, including the unbounded GET stream.
const MAX_EVENT_LINE: u64 = 16 * 1024 * 1024;
/// Most data one event may carry, counted across its `data:` lines as they arrive.
const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;
/// How long a streamable-HTTP GET stream runs before it is reopened on the same session.
const STREAMABLE_LISTEN_LIFETIME: Duration = Duration::from_secs(600);
/// How long a legacy SSE stream runs; it carries every reply, so its end closes the session.
const LEGACY_LISTEN_LIFETIME: Duration = Duration::from_secs(3600);

/// The server dropped the session (HTTP 404 for a known session id); the connection must be redone.
#[derive(Debug)]
pub struct SessionEnded(pub String);

impl std::fmt::Display for SessionEnded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MCP server `{}` ended the session (HTTP 404)", self.0)
    }
}

impl std::error::Error for SessionEnded {}

/// A GET event stream reached its lifetime and was closed; the connection must be redone.
#[derive(Debug)]
pub struct StreamExpired(pub String);

impl std::fmt::Display for StreamExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "MCP server `{}` closed its event stream after its lifetime; reconnect to resume",
            self.0
        )
    }
}

impl std::error::Error for StreamExpired {}

/// Whether a read failed because a timeout elapsed (ureq reports body timeouts as `Error::Timeout`).
fn is_timeout(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::TimedOut
        || matches!(
            error
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<ureq::Error>()),
            Some(ureq::Error::Timeout(_))
        )
}

/// The server wants authorization dial does not have (HTTP 401/403 without static credentials).
#[derive(Debug)]
pub struct AuthRequired(pub String, pub u16);

impl std::fmt::Display for AuthRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "MCP server `{0}` requires authorization (HTTP {1}); run `/mcp login {0}` to sign in",
            self.0, self.1
        )
    }
}

impl std::error::Error for AuthRequired {}

/// Request credentials for one remote server.
pub(super) struct Auth {
    server: String,
    headers: Vec<(String, String)>,
    /// `bearer_env` or an explicit `Authorization` header: a 401 means they are wrong.
    fixed: bool,
    oauth: Option<Arc<TokenSource>>,
    challenge: Mutex<Option<String>>,
}

impl Auth {
    pub(super) fn new(
        server: &str,
        config: &McpServerConfig,
        oauth: Option<Arc<TokenSource>>,
    ) -> Result<Self> {
        let mut headers: Vec<(String, String)> = config
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if let Some(var) = &config.bearer_env {
            let token = std::env::var(var)
                .map_err(|_| anyhow!("environment variable {var} (bearer_env) is not set"))?;
            headers.push(("Authorization".into(), format!("Bearer {}", token.trim())));
        }
        let fixed = headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("authorization"));
        Ok(Auth {
            server: server.to_owned(),
            headers,
            fixed,
            oauth: if fixed { None } else { oauth },
            challenge: Mutex::default(),
        })
    }

    /// Every header to send, plus the OAuth access token they carry (None without a usable grant).
    pub(super) fn headers(&self) -> (Vec<(String, String)>, Option<String>) {
        let mut headers = self.headers.clone();
        let bearer = self.oauth.as_ref().and_then(|oauth| oauth.bearer());
        if let Some(token) = &bearer {
            headers.push(("Authorization".into(), format!("Bearer {token}")));
        }
        (headers, bearer)
    }

    /// Like [`Auth::headers`] but never refreshes the grant; used when dropping a connection.
    pub(super) fn headers_stored(&self) -> Vec<(String, String)> {
        let mut headers = self.headers.clone();
        if let Some(token) = self.oauth.as_ref().and_then(|oauth| oauth.stored_access()) {
            headers.push(("Authorization".into(), format!("Bearer {token}")));
        }
        headers
    }

    /// The error for a 401/403, remembering the challenge so the manager can offer a sign-in.
    pub(super) fn refused(&self, status: u16, challenge: Option<&str>) -> anyhow::Error {
        if self.fixed {
            return anyhow!(
                "MCP server `{}` refused the credentials (HTTP {status}); set bearer_env to a valid token",
                self.server
            );
        }
        *self.challenge.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(challenge.unwrap_or("").to_owned());
        anyhow::Error::new(AuthRequired(self.server.clone(), status))
    }

    /// After a 401 that carried `stale`: refresh the grant unless another request already did.
    pub(super) fn refresh(&self, stale: &str) -> bool {
        self.oauth
            .as_ref()
            .is_some_and(|oauth| oauth.refresh(stale))
    }

    pub(super) fn challenge(&self) -> Option<String> {
        self.challenge
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        // Redirects must not forward custom credentials or session IDs to an unvalidated URL.
        .max_redirects(0)
        .build()
        .into()
}

pub struct Http {
    server: String,
    url: String,
    auth: Auth,
    agent: ureq::Agent,
    session: Mutex<Option<String>>,
    protocol: Mutex<Option<String>>,
    stop: AtomicBool,
    sse: bool,
    endpoint: Mutex<Option<String>>,
    /// How long one GET event stream may run before Dial closes it (see `read_stream`).
    stream_lifetime: Duration,
}

impl Http {
    pub fn new(
        server: &str,
        url: &str,
        config: &McpServerConfig,
        oauth: Option<Arc<TokenSource>>,
    ) -> Result<Self> {
        validate_url(url)?;
        Ok(Self {
            server: server.to_owned(),
            url: url.to_owned(),
            auth: Auth::new(server, config, oauth)?,
            agent: agent(),
            session: Mutex::default(),
            protocol: Mutex::default(),
            stop: AtomicBool::new(false),
            sse: config.transport == Some(McpTransport::Sse),
            endpoint: Mutex::default(),
            stream_lifetime: if config.transport == Some(McpTransport::Sse) {
                LEGACY_LISTEN_LIFETIME
            } else {
                STREAMABLE_LISTEN_LIFETIME
            },
        })
    }

    fn session(&self) -> Option<String> {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Where requests are POSTed: the configured URL, or for legacy SSE the announced endpoint.
    fn target(&self) -> Result<String> {
        if !self.sse {
            return Ok(self.url.clone());
        }
        self.endpoint
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| {
                anyhow!(
                    "MCP server `{}` has not announced its SSE endpoint",
                    self.server
                )
            })
    }

    fn protocol(&self) -> Option<String> {
        self.protocol
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn post_once(
        &self,
        message: &Value,
        timeout: Duration,
        sink: &mut dyn FnMut(Value),
        auth: &[(String, String)],
    ) -> Result<()> {
        let target = self.target()?;
        let mut request = self
            .agent
            .post(&target)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json");
        for (name, value) in auth {
            request = request.header(name.as_str(), value.as_str());
        }
        if let Some(id) = self.session() {
            request = request.header("Mcp-Session-Id", id.as_str());
        }
        if let Some(version) = self.protocol() {
            request = request.header("MCP-Protocol-Version", version.as_str());
        }
        let mut response = request
            .config()
            .timeout_global((!timeout.is_zero()).then_some(timeout))
            .build()
            .send(message.to_string())
            .map_err(|e| anyhow!("MCP server `{}` request failed: {e}", self.server))?;
        let status = response.status().as_u16();
        if let Some(id) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *self.session.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.to_owned());
        }
        if status == 401 || status == 403 {
            let challenge = response
                .headers()
                .get("www-authenticate")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            return Err(self.auth.refused(status, challenge.as_deref()));
        }
        if status == 404 && self.session().is_some() {
            return Err(anyhow::Error::new(SessionEnded(self.server.clone())));
        }
        let is_sse = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| t.starts_with("text/event-stream"));
        let reader = response.body_mut().with_config().limit(MAX_BODY).reader();
        if !(200..300).contains(&status) {
            let mut excerpt = String::new();
            let _ = reader.take(300).read_to_string(&mut excerpt);
            bail!(
                "MCP server `{}` returned HTTP {status}: {}",
                self.server,
                excerpt.trim()
            );
        }
        if is_sse {
            read_events(
                BufReader::new(reader),
                &|| !self.stop.load(Ordering::Relaxed),
                &mut |_, data| {
                    if self.stop.load(Ordering::Relaxed) {
                        return false;
                    }
                    if let Ok(value) = serde_json::from_str::<Value>(data) {
                        for message in flatten(value) {
                            sink(message);
                        }
                    }
                    true
                },
            )
            .map_err(|e| {
                anyhow!(
                    "MCP server `{}`: reading the event stream failed: {e}",
                    self.server
                )
            })?;
            return Ok(());
        }
        let mut body = String::new();
        BufReader::new(reader)
            .read_to_string(&mut body)
            .map_err(|e| {
                anyhow!(
                    "MCP server `{}`: reading the response failed: {e}",
                    self.server
                )
            })?;
        if body.trim().is_empty() {
            return Ok(());
        }
        let value: Value = serde_json::from_str(&body).with_context(|| {
            format!("MCP server `{}` sent a body that is not JSON", self.server)
        })?;
        for message in flatten(value) {
            sink(message);
        }
        Ok(())
    }

    /// The stream for server-initiated messages. Streamable HTTP keeps an optional GET stream that
    /// ends quietly when the server has none. Legacy SSE has no other channel: the stream names the
    /// POST endpoint and carries every reply, so its end is a lost connection.
    fn listen(self: Arc<Self>, shared: Arc<Shared>, ready: Option<mpsc::Sender<Result<()>>>) {
        let mut ready = ready;
        let result = loop {
            let result = self.read_stream(&shared, &mut ready);
            // A streamable GET stream that reaches its lifetime is reopened on the same session.
            let expired = result
                .as_ref()
                .err()
                .is_some_and(|error| error.downcast_ref::<StreamExpired>().is_some());
            if self.sse || !expired || self.stop.load(Ordering::Relaxed) {
                break result;
            }
        };
        // The optional GET stream may be unsupported or end normally, but an explicit
        // session termination invalidates POST requests too and must reach the supervisor.
        let session_ended = result
            .as_ref()
            .err()
            .is_some_and(|error| error.downcast_ref::<SessionEnded>().is_some());
        if !self.sse && !session_ended {
            return;
        }
        let error = result
            .err()
            .unwrap_or_else(|| anyhow!("MCP server `{}` closed its event stream", self.server));
        let reason = format!("{error:#}");
        if let Some(tx) = ready.take() {
            let _ = tx.send(Err(error));
        }
        if !self.stop.load(Ordering::Relaxed) {
            shared.close(reason);
        }
    }

    /// Opens the GET stream and reads it until it ends. A 401 with a refreshable grant is retried once.
    fn read_stream(
        &self,
        shared: &Shared,
        ready: &mut Option<mpsc::Sender<Result<()>>>,
    ) -> Result<()> {
        let mut refreshed = false;
        let mut response = loop {
            let (headers, sent) = self.auth.headers();
            let mut request = self
                .agent
                .get(&self.url)
                .header("Accept", "text/event-stream");
            for (name, value) in &headers {
                request = request.header(name.as_str(), value.as_str());
            }
            if let Some(id) = self.session() {
                request = request.header("Mcp-Session-Id", id.as_str());
            }
            if let Some(version) = self.protocol() {
                request = request.header("MCP-Protocol-Version", version.as_str());
            }
            let response = request
                .config()
                .timeout_recv_body(Some(self.stream_lifetime))
                .build()
                .call()
                .map_err(|e| anyhow!("MCP server `{}` event stream failed: {e}", self.server))?;
            let status = response.status().as_u16();
            if status == 401 || status == 403 {
                if status == 401
                    && !refreshed
                    && sent
                        .as_deref()
                        .is_some_and(|stale| self.auth.refresh(stale))
                {
                    refreshed = true;
                    continue;
                }
                let challenge = response
                    .headers()
                    .get("www-authenticate")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                return Err(self.auth.refused(status, challenge.as_deref()));
            }
            if status == 404 && self.session().is_some() {
                return Err(anyhow::Error::new(SessionEnded(self.server.clone())));
            }
            let is_sse = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|t| t.starts_with("text/event-stream"));
            if status != 200 || !is_sse {
                if self.sse {
                    bail!(
                        "MCP server `{}` did not open an event stream (HTTP {status})",
                        self.server
                    );
                }
                return Ok(());
            }
            break response;
        };
        // The stream lives at most `stream_lifetime`: the transport read fails with a timeout when
        // that elapses, so a server that stalls cannot hold this thread and its socket forever.
        // `shutdown` and `Drop` only set `stop`, which is checked before every line; nothing joins
        // this thread, so no caller waits on it.
        let reader = BufReader::new(response.body_mut().with_config().limit(u64::MAX).reader());
        read_events(
            reader,
            &|| !self.stop.load(Ordering::Relaxed),
            &mut |event, data| {
                if self.stop.load(Ordering::Relaxed) {
                    return false;
                }
                if self.sse && event == "endpoint" {
                    let outcome = self.set_endpoint(data);
                    let stop_reading = outcome.is_err();
                    if let Some(tx) = ready.take() {
                        let _ = tx.send(outcome);
                    }
                    return !stop_reading;
                }
                if let Ok(value) = serde_json::from_str::<Value>(data) {
                    for message in flatten(value) {
                        if let Some(answer) = shared.handle(&message) {
                            let _ = self.post(&answer, ANSWER_TIMEOUT, &mut |_| {});
                        }
                    }
                }
                true
            },
        )
        .map_err(|e| {
            if is_timeout(&e) {
                anyhow::Error::new(StreamExpired(self.server.clone()))
            } else {
                anyhow!(
                    "MCP server `{}`: reading the event stream failed: {e}",
                    self.server
                )
            }
        })?;
        Ok(())
    }

    /// Legacy SSE: the `endpoint` event names where requests are POSTed. It must pass the same URL
    /// validation and stay on the configured server's origin.
    fn set_endpoint(&self, data: &str) -> Result<()> {
        let base = url::Url::parse(&self.url).context("parsing the MCP server URL")?;
        let endpoint = base.join(data.trim()).with_context(|| {
            format!(
                "MCP server `{}` sent an invalid endpoint `{data}`",
                self.server
            )
        })?;
        if endpoint.origin() != base.origin() {
            bail!(
                "MCP server `{}` announced an endpoint on another origin: {endpoint}",
                self.server
            );
        }
        validate_url(endpoint.as_str())?;
        *self.endpoint.lock().unwrap_or_else(|e| e.into_inner()) = Some(endpoint.to_string());
        Ok(())
    }

    /// Legacy HTTP+SSE: opens the event stream before `initialize` and waits up to `wait` (zero
    /// waits indefinitely) for the endpoint. Replies to requests then arrive on this stream.
    pub(super) fn connect_sse(
        self: Arc<Self>,
        shared: Arc<Shared>,
        wait: Duration,
        cancel: &dyn Fn() -> bool,
    ) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        let worker = self.clone();
        std::thread::Builder::new()
            .name(format!("mcp-{}-sse", self.server))
            .spawn(move || worker.listen(shared, Some(tx)))
            .context("starting the event stream thread")?;
        let deadline = (!wait.is_zero()).then(|| Instant::now() + wait);
        let outcome = loop {
            match rx.recv_timeout(CANCEL_POLL) {
                Ok(result) => break Some(result),
                Err(RecvTimeoutError::Disconnected) => break None,
                Err(RecvTimeoutError::Timeout) => {}
            }
            if cancel() {
                self.stop.store(true, Ordering::Relaxed);
                bail!("connecting to MCP server `{}` was cancelled", self.server);
            }
            if deadline.is_some_and(|end| Instant::now() >= end) {
                break None;
            }
        };
        match outcome {
            Some(result) => result,
            None => {
                self.stop.store(true, Ordering::Relaxed);
                bail!(
                    "MCP server `{}` did not announce its SSE endpoint in time",
                    self.server
                )
            }
        }
    }
}

impl Remote for Http {
    fn post(&self, message: &Value, timeout: Duration, sink: &mut dyn FnMut(Value)) -> Result<()> {
        let (headers, sent) = self.auth.headers();
        match self.post_once(message, timeout, sink, &headers) {
            Err(error)
                if error
                    .downcast_ref::<AuthRequired>()
                    .is_some_and(|error| error.1 == 401)
                    && sent
                        .as_deref()
                        .is_some_and(|stale| self.auth.refresh(stale)) =>
            {
                let (headers, _) = self.auth.headers();
                self.post_once(message, timeout, sink, &headers)
            }
            other => other,
        }
    }

    fn replies_on_stream(&self) -> bool {
        self.sse
    }

    fn after_initialize(self: Arc<Self>, _version: &str, shared: Arc<Shared>) {
        // Legacy SSE opened its stream before `initialize`; streamable HTTP opens it here.
        if self.sse {
            return;
        }
        let name = format!("mcp-{}-listen", self.server);
        let _ = std::thread::Builder::new()
            .name(name)
            .spawn(move || self.listen(shared, None));
    }

    fn set_protocol(&self, version: &str) {
        *self.protocol.lock().unwrap_or_else(|e| e.into_inner()) = Some(version.to_owned());
    }

    fn auth_challenge(&self) -> Option<String> {
        self.auth.challenge()
    }

    fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        // Spec: a client that is done with a session SHOULD end it. Best effort, off the caller.
        let Some(session) = self.session() else {
            return;
        };
        let (agent, url, headers) = (
            self.agent.clone(),
            self.url.clone(),
            self.auth.headers_stored(),
        );
        let _ = std::thread::Builder::new()
            .name(format!("mcp-{}-close", self.server))
            .spawn(move || {
                let mut request = agent
                    .delete(&url)
                    .header("Mcp-Session-Id", session.as_str());
                for (name, value) in &headers {
                    request = request.header(name.as_str(), value.as_str());
                }
                let _ = request
                    .config()
                    .timeout_global(Some(Duration::from_secs(5)))
                    .build()
                    .call();
            });
    }
}

/// A JSON-RPC batch is an array; anything else is one message.
pub(super) fn flatten(value: Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items,
        other => vec![other],
    }
}

/// Reads server-sent events as they arrive, calling `on_event(event_name, data)` per event (the
/// name defaults to `message`; multi-line data is joined with newlines). Stops when it returns
/// false, when `keep_going` returns false (checked before every line), or when the stream ends.
/// Comments and fields other than `event`/`data` are ignored. The data of one event may not exceed
/// [`MAX_EVENT_BYTES`]; that is counted as the lines arrive, so an event that never ends is refused.
pub(super) fn read_events(
    mut reader: impl BufRead,
    keep_going: &dyn Fn() -> bool,
    on_event: &mut dyn FnMut(&str, &str) -> bool,
) -> std::io::Result<()> {
    let mut event = String::new();
    let mut data: Vec<String> = Vec::new();
    let mut data_bytes = 0usize;
    let mut raw: Vec<u8> = Vec::new();
    loop {
        if !keep_going() {
            return Ok(());
        }
        raw.clear();
        // Bounded: a server that never sends a newline cannot grow this buffer without limit.
        let read = (&mut reader)
            .take(MAX_EVENT_LINE + 1)
            .read_until(b'\n', &mut raw)?;
        if read as u64 > MAX_EVENT_LINE && !raw.ends_with(b"\n") {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "server-sent event line is too long",
            ));
        }
        let line = std::str::from_utf8(&raw)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let text = line.trim_end_matches(['\r', '\n']);
        if read == 0 || text.is_empty() {
            if !data.is_empty() {
                let name = if event.is_empty() { "message" } else { &event };
                if !on_event(name, &data.join("\n")) {
                    return Ok(());
                }
            }
            data.clear();
            data_bytes = 0;
            event.clear();
            if read == 0 {
                return Ok(());
            }
        } else if let Some(rest) = text.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            // The joined event gets one newline between lines; count it as it is added.
            let added = rest.len() + usize::from(!data.is_empty());
            if data_bytes + added > MAX_EVENT_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "server-sent event exceeds the size limit",
                ));
            }
            data_bytes += added;
            data.push(rest.to_owned());
        } else if let Some(rest) = text.strip_prefix("event:") {
            event = rest.trim().to_owned();
        }
    }
}

/// The JSON `data:` payloads of a complete server-sent-event body, in order. Non-JSON events are skipped.
#[cfg(test)]
pub fn parse_sse(body: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let _ = read_events(body.as_bytes(), &|| true, &mut |_, data| {
        if let Ok(value) = serde_json::from_str::<Value>(data) {
            out.push(value);
        }
        true
    });
    out
}

#[cfg(test)]
mod tests;

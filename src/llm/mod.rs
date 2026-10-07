//! Blocking OpenAI-compatible chat client for `mlx_lm.server` and remote
//! OpenAI-style endpoints.
//!
//! The client never retries (see `provider` for bounded endpoint retries) and
//! never echoes request content or credentials into errors: prompts may
//! contain private data, so error text carries only the HTTP status and a
//! short excerpt of the *server's* reply, with the bearer token redacted.
//!
//! Besides plain and streaming (SSE) completions this module lifts tool calls
//! that a server returned as Hermes-style text (`<tool_call>…</tool_call>`)
//! into structured [`ToolCall`]s when the request offered tools.

pub(crate) mod anthropic;
pub(crate) mod anthropic_oauth;
pub mod auth;
mod cancel;
pub mod catalog;
pub mod chatgpt;
mod hermes;
pub mod images;
mod json_repair;
pub(crate) mod messages;
pub mod model_config;
pub mod profiles;
pub mod provider;
mod responses;
mod retry;
pub mod server;
mod stream;
pub mod usage;
pub(crate) use cancel::Patience;
#[cfg(test)]
pub(crate) mod fake;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::fmt;
use std::io::Read;
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Duration;

/// `mlx_lm.server` maps this model name to the model/adapter it was started
/// with. Any other value makes the server try to load that path or repo on
/// demand, so callers talking to a `ModelServer` must always send this label.
pub const SERVER_MODEL: &str = "default_model";

/// Largest accepted response body (streamed responses count raw SSE bytes).
pub const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// Longest server error body excerpt placed in an error message.
const ERROR_EXCERPT_BYTES: usize = 512;

/// The caller's text callback returned false; the stream was abandoned and the
/// connection dropped. Match with `error.downcast_ref::<Interrupted>()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interrupted;

impl fmt::Display for Interrupted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("interrupted")
    }
}

impl std::error::Error for Interrupted {}

/// The server answered with a non-2xx status.
#[derive(Debug)]
pub struct HttpError {
    pub status: u16,
    excerpt: String,
    pub retry_after: Option<Duration>,
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "model server returned HTTP {}: {}",
            self.status, self.excerpt
        )
    }
}

impl std::error::Error for HttpError {}

/// The request could not be sent or no response head arrived.
#[derive(Debug)]
pub struct TransportError {
    message: String,
    /// A connection or timeout failure before the response body was available.
    pub connection: bool,
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TransportError {}

/// Whether retrying is reasonable before a response body has been consumed.
pub fn is_transient(error: &anyhow::Error) -> bool {
    if let Some(http) = error.downcast_ref::<HttpError>() {
        return matches!(
            http.status,
            408 | 409 | 425 | 429 | 500 | 502 | 503 | 504 | 529
        );
    }
    error
        .downcast_ref::<TransportError>()
        .is_some_and(|t| t.connection)
}

#[derive(Debug, Clone)]
pub struct Chat {
    pub messages: Vec<Value>,
    /// OpenAI function schemas: `{"type":"function","function":{...}}`.
    pub tools: Vec<Value>,
    pub temperature: f64,
    pub max_tokens: u32,
    pub seed: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    /// Empty when the server omitted the function name.
    pub name: String,
    /// A JSON object, or `Value::Null` when `malformed_arguments` is set.
    pub arguments: Value,
    /// The model produced arguments that are not a JSON object. The engine
    /// should report this back to the model instead of aborting.
    pub malformed_arguments: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reply {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: String,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub reasoning: String,
    pub reasoning_tokens: Option<u64>,
    pub cached_prompt_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
    /// Native content blocks (including signed/redacted thinking) required for tool-turn replay.
    pub provider_content: Vec<Value>,
}

impl Reply {
    /// `mlx_lm.server` silently drops a tool call whose JSON it cannot parse
    /// (seen with small models emitting `{{...}}`), leaving
    /// `finish_reason == "tool_calls"` with no calls and no text. The engine
    /// should tell the model its call was unreadable rather than treat the turn
    /// as an empty final answer.
    pub fn dropped_tool_call(&self) -> bool {
        self.finish_reason == "tool_calls" && self.tool_calls.is_empty()
    }
}
/// Stages reported by a local inference engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InferencePhase {
    Loading,
    Processing,
    Generating,
}

/// Request-local telemetry. It never becomes conversation content or billable usage.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ModelProgress {
    pub phase: InferencePhase,
    #[serde(default)]
    pub processed_tokens: Option<u64>,
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub tokens_per_second: Option<f64>,
    #[serde(default)]
    pub estimated: bool,
}

impl ModelProgress {
    pub fn new(phase: InferencePhase) -> Self {
        Self {
            phase,
            processed_tokens: None,
            prompt_tokens: None,
            output_tokens: None,
            tokens_per_second: None,
            estimated: false,
        }
    }

    pub fn normalize(&mut self) {
        self.tokens_per_second = self.tokens_per_second.filter(|n| n.is_finite() && *n > 0.0);
        if let (Some(done), Some(total)) = (self.processed_tokens, self.prompt_tokens) {
            self.processed_tokens = Some(done.min(total));
        }
    }
}

/// Incremental provider events, separate from visible answer text and retry notices.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    Progress(ModelProgress),
    Reasoning(String),
    ToolArguments {
        index: u64,
        id: String,
        name: String,
        delta: String,
    },
}

/// Agent for loopback calls: no proxy from the environment, no redirects, and
/// HTTP error statuses are returned as responses so bodies can be inspected.
pub(crate) fn agent(timeout_secs: u64) -> ureq::Agent {
    agent_with(timeout_secs, false, None, None)
}

/// Ceiling on one streamed reply. Not a policy: silence is bounded by [`Patience`]; this only keeps a stream
/// that never ends from running forever.
const STREAM_CEILING: Duration = Duration::from_secs(6 * 3600);

/// Redirects stay off for every target: following one would replay the bearer
/// token to whatever host the server names. Remote targets honour the
/// environment's proxy settings; loopback and LAN targets never use a proxy.
///
/// A non-streaming call is bounded as a whole by `timeout_secs`. A streaming call (`patience`) is not: opening the
/// connection and sending the request are bounded, then the transport enforces only how long the server may stay
/// silent (see [`cancel::Patience`]).
pub(crate) fn agent_with(
    timeout_secs: u64,
    env_proxy: bool,
    interrupt: Option<Arc<AtomicBool>>,
    patience: Option<Patience>,
) -> ureq::Agent {
    let builder = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0);
    let builder = match patience {
        None => builder.timeout_global(Some(Duration::from_secs(timeout_secs))),
        // `recv_*` ceilings are set (not left unset) so ureq reports the phase in each timeout; the transport reads
        // that to apply the first-event and idle limits.
        Some(_) => builder
            .timeout_global(None)
            .timeout_resolve(Some(Duration::from_secs(15)))
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_send_request(Some(Duration::from_secs(60)))
            .timeout_send_body(Some(Duration::from_secs(120)))
            .timeout_recv_response(Some(STREAM_CEILING))
            .timeout_recv_body(Some(STREAM_CEILING)),
    };
    let builder = if env_proxy {
        builder
    } else {
        builder.proxy(None)
    };
    match (patience, interrupt) {
        (Some(patience), interrupt) => {
            cancel::patient_agent(builder.build(), interrupt.unwrap_or_default(), patience)
        }
        (None, Some(interrupt)) => cancel::agent(builder.build(), interrupt),
        (None, None) => ureq::Agent::new_with_config(builder.build()),
    }
}

/// Poll cancellation below TLS while retaining a caller's guarded DNS resolver.
/// The caller still owns bounded DNS, connect and write deadlines.
pub(crate) fn cancellable_agent_with_resolver(
    config: ureq::config::Config,
    interrupt: Arc<AtomicBool>,
    resolver: impl ureq::unversioned::resolver::Resolver,
) -> ureq::Agent {
    cancel::agent_with_resolver(config, interrupt, resolver)
}

/// Hosts that are reachable only locally: loopback, RFC 1918 / link-local /
/// unique-local addresses, `localhost` and mDNS `.local` names.
pub(crate) fn is_local_host(host: &str) -> bool {
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
            std::net::IpAddr::V6(v6) => {
                v6.is_loopback()
                    || (v6.segments()[0] & 0xfe00) == 0xfc00
                    || (v6.segments()[0] & 0xffc0) == 0xfe80
            }
        };
    }
    host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local")
}

pub(crate) fn host_of(url: &str) -> Option<String> {
    url.parse::<ureq::http::Uri>()
        .ok()?
        .host()
        .map(str::to_string)
}

/// The chat-completions URL for `base_url`: `{base}/chat/completions` when the
/// base already has a path (`…/v1`, `…/openai/v1`), and
/// `{base}/v1/chat/completions` for a bare `scheme://host[:port]`, which is
/// how `mlx_lm.server`, Ollama, LM Studio and vLLM are addressed. A base that
/// already ends in `/chat/completions` is used as is.
pub fn completions_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/chat/completions") {
        return base.to_string();
    }
    let after_scheme = base.split_once("://").map_or(base, |(_, rest)| rest);
    if after_scheme.contains('/') {
        format!("{base}/chat/completions")
    } else {
        format!("{base}/v1/chat/completions")
    }
}

/// One non-streaming chat completion. `model_label` should be [`SERVER_MODEL`]
/// for servers started by [`crate::llm::server::ModelServer`].
pub fn chat(base_url: &str, model_label: &str, req: &Chat, timeout_secs: u64) -> Result<Reply> {
    chat_with_auth(base_url, None, model_label, req, timeout_secs)
}

/// [`chat`] with an optional bearer token, sent as `Authorization: Bearer`.
pub fn chat_with_auth(
    base_url: &str,
    auth: Option<&str>,
    model_label: &str,
    req: &Chat,
    timeout_secs: u64,
) -> Result<Reply> {
    chat_at(
        &completions_url(base_url),
        auth,
        model_label,
        req,
        timeout_secs,
    )
}

/// Streaming chat completion over SSE. `on_text` receives each non-empty
/// content delta (text of Hermes-style `<tool_call>` blocks is withheld when
/// the request offers tools) and returns false to abandon the stream: the
/// connection is dropped and the call fails with [`Interrupted`].
///
/// The whole call, not each read, is bounded by `timeout_secs`. A stream that
/// ends before a `finish_reason` or `[DONE]` is an error and its partial text
/// is discarded (the caller already saw it through `on_text`).
pub fn chat_stream(
    base_url: &str,
    auth: Option<&str>,
    model_label: &str,
    req: &Chat,
    timeout_secs: u64,
    on_text: &mut dyn FnMut(&str) -> bool,
) -> Result<Reply> {
    chat_stream_at(
        &completions_url(base_url),
        auth,
        model_label,
        req,
        timeout_secs,
        on_text,
    )
}

/// The provider route is kept together when forwarding a request through transports.
pub(super) struct Endpoint<'a> {
    pub url: &'a str,
    pub auth: Option<&'a str>,
    pub model: &'a str,
}

pub(crate) fn chat_at(
    url: &str,
    auth: Option<&str>,
    model_label: &str,
    req: &Chat,
    timeout_secs: u64,
) -> Result<Reply> {
    chat_at_controlled(
        Endpoint {
            url,
            auth,
            model: model_label,
        },
        req,
        timeout_secs,
        None,
        None,
        &mut |_| true,
    )
}

fn chat_at_controlled(
    endpoint: Endpoint<'_>,
    req: &Chat,
    timeout_secs: u64,
    interrupt: Option<Arc<AtomicBool>>,
    profile: Option<&profiles::ModelProfile>,
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
) -> Result<Reply> {
    let Endpoint {
        url,
        auth,
        model: model_label,
    } = endpoint;
    let mut body = request_body(model_label, req, timeout_secs, false)?;
    if let Some(profile) = profile {
        profile.validate_request(req, false)?;
        profile.apply_openai(&mut body);
    }
    let mut response = send(url, auth, &body, timeout_secs, None, interrupt)?;
    let status = response.status().as_u16();
    let retry_after = retry::server_delay(response.headers());
    let success = (200..300).contains(&status);
    let bytes = read_bounded(
        response.body_mut().as_reader(),
        if success {
            MAX_RESPONSE_BYTES
        } else {
            ERROR_EXCERPT_BYTES
        },
    )
    .context("reading chat response")?;
    if !success {
        return Err(http_error(status, &bytes, auth, retry_after));
    }
    ensure!(
        !bytes.truncated,
        "chat response exceeds {MAX_RESPONSE_BYTES} bytes"
    );
    let mut reply = parse_reply(&bytes.data).map_err(|error| response_error(error, auth))?;
    if !reply.reasoning.is_empty() && !on_event(&StreamEvent::Reasoning(reply.reasoning.clone())) {
        return Err(Interrupted.into());
    }
    if !req.tools.is_empty() {
        hermes::apply(&mut reply);
    }
    Ok(reply)
}

pub(crate) fn chat_stream_at(
    url: &str,
    auth: Option<&str>,
    model_label: &str,
    req: &Chat,
    timeout_secs: u64,
    on_text: &mut dyn FnMut(&str) -> bool,
) -> Result<Reply> {
    chat_stream_at_controlled(
        Endpoint {
            url,
            auth,
            model: model_label,
        },
        req,
        timeout_secs,
        on_text,
        None,
        None,
        &mut |_| true,
    )
}

fn chat_stream_at_controlled(
    endpoint: Endpoint<'_>,
    req: &Chat,
    timeout_secs: u64,
    on_text: &mut dyn FnMut(&str) -> bool,
    interrupt: Option<Arc<AtomicBool>>,
    profile: Option<&profiles::ModelProfile>,
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
) -> Result<Reply> {
    let Endpoint {
        url,
        auth,
        model: model_label,
    } = endpoint;
    let mut body = request_body(model_label, req, timeout_secs, true)?;
    let patience = Patience::for_stream(
        timeout_secs,
        host_of(url).is_some_and(|host| is_local_host(&host)),
    );
    if let Some(profile) = profile {
        profile.validate_request(req, false)?;
        profile.apply_openai(&mut body);
    }
    let mut response = send(
        url,
        auth,
        &body,
        timeout_secs,
        Some(patience.clone()),
        interrupt,
    )?;
    let status = response.status().as_u16();
    let retry_after = retry::server_delay(response.headers());
    if !(200..300).contains(&status) {
        let bytes = read_bounded(response.body_mut().as_reader(), ERROR_EXCERPT_BYTES)
            .context("reading chat response")?;
        return Err(http_error(status, &bytes, auth, retry_after));
    }
    let is_sse = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/event-stream"));
    let mut hider = hermes::Hider::new(on_text, !req.tools.is_empty());
    let mut reply = if is_sse {
        let mut accumulator = stream::Accumulator::default();
        let mut forward = |piece: &str| hider.push(piece);
        stream::read_events(
            response.body_mut().as_reader(),
            MAX_RESPONSE_BYTES,
            stream::MAX_LINE_BYTES,
            patience,
            |data| accumulator.feed_with_events(data, &mut forward, on_event),
        )
        .map_err(|error| response_error(error, auth))?;
        accumulator
            .finish()
            .map_err(|error| response_error(error, auth))?
    } else {
        // A server that ignores `stream: true` answers with one JSON body.
        let bytes = read_bounded(response.body_mut().as_reader(), MAX_RESPONSE_BYTES)
            .context("reading chat response")?;
        ensure!(
            !bytes.truncated,
            "chat response exceeds {MAX_RESPONSE_BYTES} bytes"
        );
        let reply = parse_reply(&bytes.data).map_err(|error| response_error(error, auth))?;
        if !reply.reasoning.is_empty()
            && !on_event(&StreamEvent::Reasoning(reply.reasoning.clone()))
        {
            return Err(Interrupted.into());
        }
        if !reply.text.is_empty() && !hider.push(&reply.text) {
            return Err(Interrupted.into());
        }
        reply
    };
    if !hider.finish() {
        return Err(Interrupted.into());
    }
    if !req.tools.is_empty() {
        hermes::apply(&mut reply);
    }
    Ok(reply)
}

fn request_body(
    model_label: &str,
    req: &Chat,
    timeout_secs: u64,
    streaming: bool,
) -> Result<Value> {
    ensure!(timeout_secs > 0, "chat timeout must be positive");
    ensure!(
        req.temperature.is_finite() && req.temperature >= 0.0,
        "temperature must be finite and non-negative; the server drops the connection otherwise"
    );
    let mut body = json!({
        "model": model_label,
        "messages": req.messages,
        "temperature": req.temperature,
        "max_tokens": req.max_tokens,
        "stream": streaming,
    });
    for message in body["messages"].as_array_mut().into_iter().flatten() {
        if let Some(object) = message.as_object_mut() {
            object.remove("dial_provider_content");
        }
    }
    if streaming {
        body["stream_options"] = json!({ "include_usage": true });
    }
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(req.tools.clone());
    }
    if let Some(seed) = req.seed {
        body["seed"] = json!(seed);
    }
    Ok(body)
}

/// POST `body` as JSON. The token is validated as a header value but never
/// appears in any error message.
fn send(
    url: &str,
    auth: Option<&str>,
    body: &Value,
    timeout_secs: u64,
    patience: Option<Patience>,
    interrupt: Option<Arc<AtomicBool>>,
) -> Result<ureq::http::Response<ureq::Body>> {
    send_with_headers(url, auth, body, timeout_secs, patience, interrupt, &[])
}

pub(super) fn send_with_headers(
    url: &str,
    auth: Option<&str>,
    body: &Value,
    timeout_secs: u64,
    patience: Option<Patience>,
    interrupt: Option<Arc<AtomicBool>>,
    headers: &[(&str, &str)],
) -> Result<ureq::http::Response<ureq::Body>> {
    if let Some(token) = auth {
        ensure!(
            !token.is_empty() && token.bytes().all(|b| b.is_ascii_graphic()),
            "api key is empty or contains characters that are not allowed in an HTTP header"
        );
    }
    for (name, value) in headers {
        ensure!(
            name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && value.bytes().all(|b| b.is_ascii_graphic()),
            "provider request header contains invalid characters"
        );
    }
    let local = host_of(url).is_some_and(|host| is_local_host(&host));
    let mut request = agent_with(timeout_secs, !local, interrupt, patience.clone()).post(url);
    if patience.is_some() {
        request = request.header("Accept", "text/event-stream");
    }
    if let Some(token) = auth {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request
        .send_json(body)
        .map_err(|error| transport_error(error, url, auth))
}

fn transport_error(error: ureq::Error, url: &str, auth: Option<&str>) -> anyhow::Error {
    let connection = match &error {
        // No response has arrived at this boundary. A fresh connection may recover
        // macOS EINVAL or a record-integrity alert, but certificate failures and
        // generic InvalidInput/InvalidData remain permanent. Body errors bypass this policy.
        ureq::Error::Io(io) => {
            let bad_record_mac = io
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<rustls::Error>())
                .is_some_and(|tls| {
                    matches!(
                        tls,
                        rustls::Error::AlertReceived(rustls::AlertDescription::BadRecordMac)
                    )
                });
            (cfg!(target_os = "macos") && io.raw_os_error() == Some(libc::EINVAL))
                || bad_record_mac
                || matches!(
                    io.kind(),
                    std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::NotConnected
                        | std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::UnexpectedEof
                )
        }
        ureq::Error::ConnectionFailed | ureq::Error::HostNotFound | ureq::Error::Timeout(_) => true,
        _ => false,
    };
    let message = redact(
        &format!("chat request to {} failed: {error}", display_url(url)),
        auth,
    );
    anyhow::Error::new(TransportError {
        message,
        connection,
    })
}

/// The URL without userinfo, query or fragment, safe for messages.
pub(crate) fn display_url(url: &str) -> String {
    match url.parse::<ureq::http::Uri>() {
        Ok(uri) => match (uri.scheme_str(), uri.authority()) {
            (Some(scheme), Some(authority)) => {
                let host = authority.as_str().rsplit('@').next().unwrap_or_default();
                format!("{scheme}://{host}{}", uri.path().trim_end_matches('/'))
            }
            _ => "<invalid url>".to_string(),
        },
        Err(_) => "<invalid url>".to_string(),
    }
}

fn redact(text: &str, auth: Option<&str>) -> String {
    match auth {
        Some(token) if !token.is_empty() => text.replace(token, "[redacted]"),
        _ => text.to_string(),
    }
}

/// Post-response failures are never transport failures, even if their source was I/O.
fn response_error(error: anyhow::Error, auth: Option<&str>) -> anyhow::Error {
    if error.downcast_ref::<Interrupted>().is_some() {
        error
    } else {
        anyhow::anyhow!("{}", redact(&format!("{error:#}"), auth))
    }
}

fn http_error(
    status: u16,
    body: &Bounded,
    auth: Option<&str>,
    retry_after: Option<Duration>,
) -> anyhow::Error {
    // Servers wrap the useful sentence in JSON; show that instead of a one-line blob.
    let message = if body.truncated {
        None
    } else {
        serde_json::from_slice::<serde_json::Value>(&body.data)
            .ok()
            .and_then(|value| {
                value
                    .pointer("/error/message")
                    .or_else(|| value.get("error"))
                    .or_else(|| value.get("message"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                    .map(str::to_owned)
            })
    };
    let error_type = if body.truncated {
        None
    } else {
        serde_json::from_slice::<serde_json::Value>(&body.data)
            .ok()
            .and_then(|value| {
                value
                    .pointer("/error/type")
                    .and_then(serde_json::Value::as_str)
                    .filter(|kind| {
                        !kind.is_empty()
                            && kind.len() <= 64
                            && kind.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    })
                    .map(str::to_owned)
            })
    };
    let mut excerpt = match &message {
        Some(message) => redact(&self::excerpt(message.as_bytes(), false), auth),
        None => redact(&self::excerpt(&body.data, body.truncated), auth),
    };
    // Some providers (Anthropic 429s) send only "Error"; the error type is then the only signal.
    let generic = message
        .as_deref()
        .is_none_or(|m| m.eq_ignore_ascii_case("error"));
    if let Some(kind) = error_type.filter(|_| generic) {
        excerpt = format!("{excerpt} (type: {kind})");
    }
    // LM Studio loads models on first use and refuses when it cannot (memory guardrail, unsupported
    // architecture); say what to do rather than leaving only its message.
    if status == 400
        && message
            .as_deref()
            .is_some_and(|m| m.contains("Failed to load model"))
    {
        excerpt.push_str(" — the server could not load this model. Load it in LM Studio first (a smaller context length helps), or pick another model.");
    }
    if matches!(status, 401 | 403) {
        excerpt.push_str(
            " — check this provider's API key/environment variable or sign in through /providers.",
        );
    } else if status == 404 {
        excerpt.push_str(" — refresh /models and select an available model; check the provider endpoint in /providers.");
    }
    // Validation errors often list every allowed content type, including "image". Only an
    // explicit capability rejection justifies suggesting another model, not malformed history,
    // oversized images, or unsupported image encodings.
    if status == 400
        && message.as_deref().is_some_and(|message| {
            let lower = message.to_ascii_lowercase();
            [
                "does not support image input",
                "does not support images",
                "model does not support image",
                "model doesn't support image",
                "image input is not supported",
                "image inputs are not supported",
                "image input not supported",
                "image inputs not supported",
                "unsupported image input",
                "images are not supported by this model",
                "image_url is only supported by certain models",
                "image_url is not supported by this model",
                "does not support vision",
                "vision is not supported",
                "not a vision model",
            ]
            .iter()
            .any(|reason| lower.contains(reason))
        })
    {
        excerpt.push_str(" — this model rejected image input; select a vision-capable model or remove the image attachment.");
    }
    anyhow::Error::new(HttpError {
        status,
        excerpt,
        retry_after,
    })
}

struct Bounded {
    data: Vec<u8>,
    truncated: bool,
}

/// Read at most `limit` bytes and report whether more were available.
fn read_bounded(reader: impl Read, limit: usize) -> Result<Bounded> {
    let mut data = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut data)?;
    let truncated = data.len() > limit;
    data.truncate(limit);
    Ok(Bounded { data, truncated })
}

fn excerpt(bytes: &[u8], truncated: bool) -> String {
    let text = String::from_utf8_lossy(bytes);
    let flattened: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flattened = flattened.trim();
    if flattened.is_empty() {
        return "<empty body>".to_string();
    }
    if truncated {
        format!("{flattened}…")
    } else {
        flattened.to_string()
    }
}

/// Parse an OpenAI chat completion body into a [`Reply`].
pub fn parse_reply(body: &[u8]) -> Result<Reply> {
    let root: Value = serde_json::from_slice(body).context("chat response is not valid JSON")?;
    let choice = root
        .pointer("/choices/0")
        .context("chat response has no choices")?;
    let message = choice
        .get("message")
        .context("chat response choice has no message")?;
    let text = match message.get("content") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(_) => bail!("chat response message content is not a string"),
    };
    let tool_calls = match message.get("tool_calls") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(calls)) => parse_tool_calls(calls)?,
        Some(_) => bail!("chat response tool_calls is not an array"),
    };
    Ok(Reply {
        text,
        tool_calls,
        finish_reason: choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        prompt_tokens: root.pointer("/usage/prompt_tokens").and_then(Value::as_u64),
        completion_tokens: root
            .pointer("/usage/completion_tokens")
            .and_then(Value::as_u64),
        reasoning: message
            .get("reasoning_content")
            .or_else(|| message.get("reasoning"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        reasoning_tokens: root
            .pointer("/usage/completion_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64),
        cached_prompt_tokens: root
            .pointer("/usage/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64),
        ..Default::default()
    })
}

fn parse_tool_calls(calls: &[Value]) -> Result<Vec<ToolCall>> {
    let mut seen = HashSet::new();
    let mut parsed = Vec::with_capacity(calls.len());
    for (index, call) in calls.iter().enumerate() {
        ensure!(
            call.is_object(),
            "chat response tool call {index} is not an object"
        );
        // Synthesized ids are positional so they are stable for a given reply;
        // a repeated server id would make tool results ambiguous.
        let id = match call.get("id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() && !seen.contains(id) => id.to_string(),
            _ => format!("call_{index}"),
        };
        ensure!(
            seen.insert(id.clone()),
            "chat response tool call ids collide ({id})"
        );
        let function = call.get("function");
        let name = function
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let (arguments, malformed_arguments) =
            normalize_arguments(function.and_then(|f| f.get("arguments")));
        parsed.push(ToolCall {
            id,
            name,
            arguments,
            malformed_arguments,
        });
    }
    Ok(parsed)
}

/// Tool-call arguments as a JSON object, plus whether they were unusable.
/// Servers send an object, a JSON string holding one, or nothing (no
/// parameters); anything else is malformed.
fn normalize_arguments(raw: Option<&Value>) -> (Value, bool) {
    match raw {
        None | Some(Value::Null) => (Value::Object(Map::new()), false),
        Some(Value::Object(object)) => (Value::Object(object.clone()), false),
        Some(Value::String(text)) if text.trim().is_empty() => (Value::Object(Map::new()), false),
        // Strict JSON first (the common case), then the repairs in `json_repair`. What still fails is cut-off or
        // not an object at all, and is reported as malformed rather than guessed.
        Some(Value::String(text)) => match json_repair::parse_arguments(text) {
            Some(value) => (value, false),
            None => (Value::Null, true),
        },
        Some(_) => (Value::Null, true),
    }
}

/// The assistant turn to append to the history before the tool results.
/// `mlx_lm.server` re-parses `arguments` with `json.loads`, so it must always
/// be a JSON string; malformed calls are replayed as `{}`.
pub fn assistant_message(reply: &Reply) -> Value {
    let mut message = json!({ "role": "assistant" });
    message["content"] = if reply.text.is_empty() && !reply.tool_calls.is_empty() {
        Value::Null
    } else {
        Value::String(reply.text.clone())
    };
    if !reply.provider_content.is_empty() {
        let mut content = reply.provider_content.clone();
        for block in &mut content {
            if block["type"] == "tool_use" && !block["input"].is_object() {
                block["input"] = json!({});
            }
        }
        message["dial_provider_content"] = json!(content);
    }
    if !reply.reasoning.is_empty() && reply.provider_content.is_empty() {
        message["reasoning_content"] = json!(reply.reasoning);
    }
    if !reply.tool_calls.is_empty() {
        message["tool_calls"] = reply
            .tool_calls
            .iter()
            .map(|call| {
                let arguments = if call.malformed_arguments {
                    "{}".to_string()
                } else {
                    call.arguments.to_string()
                };
                json!({
                    "id": call.id,
                    "type": "function",
                    "function": { "name": call.name, "arguments": arguments },
                })
            })
            .collect();
    }
    message
}

pub fn tool_message(id: &str, content: &str) -> Value {
    json!({ "role": "tool", "tool_call_id": id, "content": content })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::thread::JoinHandle;

    /// Serve exactly one HTTP request with a canned response; returns the base
    /// URL and a handle yielding the raw request bytes the client sent.
    fn serve_once(status: u16, body: Vec<u8>) -> (String, JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            let header_end = loop {
                let count = stream.read(&mut chunk).unwrap();
                assert!(count > 0, "client closed before sending a request");
                request.extend_from_slice(&chunk[..count]);
                if let Some(at) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    break at + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
            let length: usize = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .map(|value| value.trim().parse().unwrap())
                .unwrap_or(0);
            while request.len() < header_end + length {
                let count = stream.read(&mut chunk).unwrap();
                assert!(count > 0, "client closed mid-body");
                request.extend_from_slice(&chunk[..count]);
            }
            let head = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            // The client may hang up early (oversized responses); that is fine.
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
            let _ = stream.flush();
            request
        });
        (url, handle)
    }

    fn request(tools: Vec<Value>, seed: Option<u64>) -> Chat {
        Chat {
            messages: vec![json!({"role": "user", "content": "SECRET-PROMPT"})],
            tools,
            temperature: 0.0,
            max_tokens: 16,
            seed,
        }
    }

    fn body_of(raw: &[u8]) -> Value {
        let at = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        serde_json::from_slice(&raw[at..]).unwrap()
    }

    #[test]
    fn text_reply_and_request_shape() {
        let reply = json!({
            "choices": [{"finish_reason": "stop", "message": {"role": "assistant", "content": "hello"}}],
            "usage": {"prompt_tokens": 7, "completion_tokens": 2}
        });
        let (url, server) = serve_once(200, reply.to_string().into_bytes());
        let reply = chat(&url, SERVER_MODEL, &request(vec![], Some(9)), 5).unwrap();
        assert_eq!(reply.text, "hello");
        assert!(reply.tool_calls.is_empty());
        assert_eq!(reply.finish_reason, "stop");
        assert_eq!(
            (reply.prompt_tokens, reply.completion_tokens),
            (Some(7), Some(2))
        );
        let sent = body_of(&server.join().unwrap());
        assert_eq!(sent["model"], SERVER_MODEL);
        assert_eq!(sent["seed"], 9);
        assert_eq!(sent["stream"], false);
        assert_eq!(sent["max_tokens"], 16);
        assert!(
            sent.get("tools").is_none(),
            "empty tool list must be omitted"
        );
    }

    #[test]
    fn tool_call_with_string_arguments() {
        let reply = json!({
            "choices": [{"finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": null,
                "tool_calls": [{"id": "abc", "type": "function",
                    "function": {"name": "read_file", "arguments": "{\"path\": \"a.txt\"}"}}]
            }}],
        });
        let (url, server) = serve_once(200, reply.to_string().into_bytes());
        let tool = json!({"type": "function", "function": {"name": "read_file", "parameters": {"type": "object"}}});
        let reply = chat(&url, SERVER_MODEL, &request(vec![tool], None), 5).unwrap();
        assert_eq!(reply.text, "");
        assert_eq!(reply.finish_reason, "tool_calls");
        assert_eq!(
            reply.tool_calls,
            vec![ToolCall {
                id: "abc".into(),
                name: "read_file".into(),
                arguments: json!({"path": "a.txt"}),
                malformed_arguments: false,
            }]
        );
        assert_eq!(reply.prompt_tokens, None);
        let sent = body_of(&server.join().unwrap());
        assert_eq!(sent["tools"][0]["function"]["name"], "read_file");
        assert!(sent.get("seed").is_none());
    }

    #[test]
    fn malformed_and_missing_fields_are_tolerated() {
        let body = json!({
            "choices": [{"message": {"content": "", "tool_calls": [
                {"function": {"name": "a", "arguments": "{not json"}},
                {"function": {"name": "b", "arguments": "[1, 2]"}},
                {"id": "dup", "function": {"name": "c", "arguments": {"x": 1}}},
                {"id": "dup", "function": {"name": "d"}},
                {"function": {"name": "e", "arguments": ""}},
                {"function": {"arguments": 7}}
            ]}}]
        });
        let reply = parse_reply(body.to_string().as_bytes()).unwrap();
        let calls = &reply.tool_calls;
        assert_eq!(calls[0].id, "call_0");
        assert!(calls[0].malformed_arguments && calls[0].arguments.is_null());
        assert!(
            calls[1].malformed_arguments,
            "non-object arguments are malformed"
        );
        assert_eq!(
            (calls[2].id.as_str(), calls[2].arguments.clone()),
            ("dup", json!({"x": 1}))
        );
        assert_eq!(calls[3].id, "call_3", "repeated ids are replaced");
        assert!(!calls[3].malformed_arguments && calls[3].arguments == json!({}));
        assert!(!calls[4].malformed_arguments && calls[4].arguments == json!({}));
        assert_eq!(calls[5].name, "");
        assert!(calls[5].malformed_arguments);
        assert_eq!(reply.finish_reason, "");
    }

    #[test]
    fn malformed_arguments_round_trip_over_http() {
        let reply = json!({"choices": [{"finish_reason": "tool_calls", "message": {"content": null, "tool_calls": [
            {"id": "t1", "function": {"name": "bash", "arguments": "{\"cmd\": "}}]}}]});
        let (url, _server) = serve_once(200, reply.to_string().into_bytes());
        let reply = chat(&url, SERVER_MODEL, &request(vec![], None), 5).unwrap();
        assert!(reply.tool_calls[0].malformed_arguments);
        // History replay must stay valid for the server's json.loads.
        let message = assistant_message(&reply);
        assert_eq!(message["tool_calls"][0]["function"]["arguments"], "{}");
        assert_eq!(message["content"], Value::Null);
    }

    #[test]
    fn http_error_status_and_bounded_body() {
        let (url, server) = serve_once(500, "x".repeat(100_000).into_bytes());
        let error = chat(&url, SERVER_MODEL, &request(vec![], None), 5)
            .unwrap_err()
            .to_string();
        server.join().unwrap();
        assert!(error.contains("HTTP 500"), "{error}");
        assert!(
            error.len() < 700,
            "error not bounded: {} bytes",
            error.len()
        );
        assert!(error.ends_with('…'));
        assert!(!error.contains("SECRET-PROMPT"));
    }

    #[test]
    fn oversized_success_response_is_rejected() {
        let filler = "y".repeat(MAX_RESPONSE_BYTES + 10);
        let body = json!({"choices": [{"message": {"content": filler}}]}).to_string();
        let (url, _server) = serve_once(200, body.into_bytes());
        let error = chat(&url, SERVER_MODEL, &request(vec![], None), 10)
            .unwrap_err()
            .to_string();
        assert!(error.contains("exceeds"), "{error}");
    }

    #[test]
    fn unreachable_server_error_has_no_prompt() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let error = chat(
            &format!("http://127.0.0.1:{port}"),
            SERVER_MODEL,
            &request(vec![], None),
            2,
        )
        .unwrap_err()
        .to_string();
        assert!(!error.contains("SECRET-PROMPT"), "{error}");
    }

    #[test]
    fn server_dropped_tool_call_is_detectable() {
        let body = json!({"choices": [{"finish_reason": "tool_calls", "message": {"role": "assistant", "content": null}}]});
        let reply = parse_reply(body.to_string().as_bytes()).unwrap();
        assert!(reply.dropped_tool_call());
        let text = json!({"choices": [{"finish_reason": "stop", "message": {"content": "done"}}]});
        assert!(
            !parse_reply(text.to_string().as_bytes())
                .unwrap()
                .dropped_tool_call()
        );
    }

    #[test]
    fn history_helpers_match_openai_shape() {
        let reply = Reply {
            text: "thinking".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "ls".into(),
                arguments: json!({"dir": "."}),
                malformed_arguments: false,
            }],
            finish_reason: "tool_calls".into(),
            prompt_tokens: None,
            completion_tokens: None,
            ..Default::default()
        };
        let message = assistant_message(&reply);
        assert_eq!(message["content"], "thinking");
        assert_eq!(
            message["tool_calls"][0]["function"]["arguments"],
            "{\"dir\":\".\"}"
        );
        assert_eq!(message["tool_calls"][0]["type"], "function");
        assert_eq!(
            tool_message("c1", "out"),
            json!({"role": "tool", "tool_call_id": "c1", "content": "out"})
        );
        let plain = assistant_message(&Reply {
            tool_calls: vec![],
            text: String::new(),
            ..reply
        });
        assert_eq!(plain, json!({"role": "assistant", "content": ""}));
    }
}

#[cfg(test)]
mod net_tests {
    use super::fake::{self, Script, header, request_line};
    use super::*;
    use std::time::Instant;

    fn chat_request(tools: Vec<Value>) -> Chat {
        Chat {
            messages: vec![json!({"role": "user", "content": "SECRET-PROMPT"})],
            tools,
            temperature: 0.0,
            max_tokens: 32,
            seed: None,
        }
    }

    fn a_tool() -> Vec<Value> {
        vec![
            json!({"type": "function", "function": {"name": "read_file", "parameters": {"type": "object"}}}),
        ]
    }

    fn sse(events: &[Value]) -> String {
        let mut out: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
        out.push_str("data: [DONE]\n\n");
        out
    }

    fn delta(content: &str) -> Value {
        json!({"choices": [{"index": 0, "delta": {"content": content}, "finish_reason": null}]})
    }

    fn stop() -> Value {
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
    }

    fn collect(
        url: &str,
        auth: Option<&str>,
        req: &Chat,
        timeout: u64,
    ) -> (Result<Reply>, Vec<String>) {
        let mut pieces = Vec::new();
        let mut on_text = |piece: &str| {
            pieces.push(piece.to_string());
            true
        };
        let result = chat_stream(url, auth, SERVER_MODEL, req, timeout, &mut on_text);
        (result, pieces)
    }

    #[test]
    fn completions_url_rules() {
        for (base, want) in [
            (
                "http://127.0.0.1:8080",
                "http://127.0.0.1:8080/v1/chat/completions",
            ),
            (
                "http://127.0.0.1:8080/",
                "http://127.0.0.1:8080/v1/chat/completions",
            ),
            (
                "https://api.openai.com/v1",
                "https://api.openai.com/v1/chat/completions",
            ),
            (
                "https://api.openai.com/v1/",
                "https://api.openai.com/v1/chat/completions",
            ),
            (
                "https://h/openai/v1beta",
                "https://h/openai/v1beta/chat/completions",
            ),
            (
                "https://h/v1/chat/completions",
                "https://h/v1/chat/completions",
            ),
        ] {
            assert_eq!(completions_url(base), want, "{base}");
        }
    }

    #[test]
    fn streams_text_with_keepalives_and_usage() {
        let body = format!(
            ": keepalive\n\n{}",
            sse(&[
                json!({"choices": [{"delta": {"role": "assistant"}}]}),
                delta("Hel"),
                delta("lo"),
                stop(),
                json!({"choices": [], "usage": {"prompt_tokens": 12, "completion_tokens": 3}}),
            ])
        );
        let server = fake::serve(vec![Script::Sse(body)]);
        let (reply, pieces) = collect(&server.url, None, &chat_request(vec![]), 10);
        let reply = reply.unwrap();
        assert_eq!(pieces, ["Hel", "lo"]);
        assert_eq!(reply.text, "Hello");
        assert_eq!(reply.finish_reason, "stop");
        assert_eq!(
            (reply.prompt_tokens, reply.completion_tokens),
            (Some(12), Some(3))
        );
        let request = server.requests().remove(0);
        assert_eq!(request_line(&request), "POST /v1/chat/completions HTTP/1.1");
        assert_eq!(
            header(&request, "accept").as_deref(),
            Some("text/event-stream")
        );
        let sent = fake::body(&request);
        assert_eq!(sent["stream"], true);
        assert_eq!(sent["stream_options"]["include_usage"], true);
    }

    #[test]
    fn streams_tool_call_deltas() {
        let call = |index: u64, id: Option<&str>, name: Option<&str>, args: &str| {
            let mut function = json!({"arguments": args});
            if let Some(name) = name {
                function["name"] = json!(name);
            }
            let mut call = json!({"index": index, "function": function});
            if let Some(id) = id {
                call["id"] = json!(id);
            }
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [call]}}]})
        };
        let body = sse(&[
            call(0, Some("t0"), Some("read_file"), ""),
            call(0, None, None, "{\"path\":"),
            call(0, None, None, " \"a.txt\"}"),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        ]);
        let server = fake::serve(vec![Script::Sse(body)]);
        let (reply, pieces) = collect(&server.url, None, &chat_request(a_tool()), 10);
        let reply = reply.unwrap();
        assert!(pieces.is_empty());
        assert_eq!(reply.finish_reason, "tool_calls");
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].id, "t0");
        assert_eq!(reply.tool_calls[0].arguments, json!({"path": "a.txt"}));
    }

    #[test]
    fn truncated_stream_is_an_error() {
        let body = format!("data: {}\n\ndata: {}\n\n", delta("par"), delta("tial"));
        let server = fake::serve(vec![Script::Sse(body)]);
        let (reply, pieces) = collect(&server.url, None, &chat_request(vec![]), 10);
        let error = reply.unwrap_err().to_string();
        assert!(error.contains("ended before"), "{error}");
        assert_eq!(pieces.concat(), "partial");
    }

    #[test]
    fn finish_reason_without_done_is_complete() {
        let body = format!("data: {}\n\ndata: {}\n\n", delta("ok"), stop());
        let server = fake::serve(vec![Script::Sse(body)]);
        let (reply, _) = collect(&server.url, None, &chat_request(vec![]), 10);
        assert_eq!(reply.unwrap().text, "ok");
    }

    #[test]
    fn in_stream_error_event_is_reported() {
        let body = format!(
            "data: {}\n\n",
            json!({"error": {"message": "context length exceeded"}})
        );
        let server = fake::serve(vec![Script::Sse(body)]);
        let (reply, _) = collect(&server.url, None, &chat_request(vec![]), 10);
        assert!(
            reply
                .unwrap_err()
                .to_string()
                .contains("context length exceeded")
        );
    }

    #[test]
    fn oversized_stream_is_rejected() {
        let event = format!("data: {}\n\n", delta(&"z".repeat(60_000)));
        let body = event.repeat(MAX_RESPONSE_BYTES / event.len() + 2);
        let server = fake::serve(vec![Script::Sse(body)]);
        let (reply, _) = collect(&server.url, None, &chat_request(vec![]), 20);
        let error = reply.unwrap_err().to_string();
        assert!(error.contains("stream exceeds"), "{error}");
    }

    #[test]
    fn oversized_line_is_rejected() {
        let body = format!(
            "data: {}\n\n",
            delta(&"z".repeat(stream::MAX_LINE_BYTES + 10))
        );
        let server = fake::serve(vec![Script::Sse(body)]);
        let (reply, _) = collect(&server.url, None, &chat_request(vec![]), 20);
        let error = reply.unwrap_err().to_string();
        assert!(error.contains("line exceeds"), "{error}");
    }

    #[test]
    fn keepalive_comments_do_not_keep_a_started_reply_alive() {
        // Every read succeeds quickly; only the idle limit stops this. Comments are not progress once a reply began.
        let first = format!("data: {}\n\n", delta("Hel"));
        let server = fake::serve(vec![Script::DripAfter {
            first,
            pause: Duration::from_millis(40),
            count: 500,
        }]);
        let started = Instant::now();
        let (reply, _) = collect(&server.url, None, &chat_request(vec![]), 1);
        let error = reply.unwrap_err().to_string();
        assert!(error.contains("silent"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn silence_after_the_first_event_hits_the_idle_limit() {
        let first = format!("data: {}\n\n", delta("Hel"));
        let server = fake::serve(vec![Script::StallAfter {
            first,
            silence: Duration::from_secs(6),
        }]);
        let started = Instant::now();
        let (reply, _) = collect(&server.url, None, &chat_request(vec![]), 1);
        let error = reply.unwrap_err().to_string();
        assert!(error.contains("silent"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_local_cold_start_longer_than_the_idle_limit_still_completes() {
        // A model server reading a long prompt sends its headers, then nothing, for longer than the idle limit.
        // That is not a stall: the first-event wait is unbounded on a local server, and the reply that follows is
        // judged by silence, not by the total time the call took.
        let body = format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\n",
            delta("ok"),
            stop(),
            "[DONE]"
        );
        let server = fake::serve(vec![Script::ColdStart {
            delay: Duration::from_millis(2500),
            body,
        }]);
        let started = Instant::now();
        let (reply, _) = collect(&server.url, None, &chat_request(vec![]), 1);
        assert_eq!(reply.unwrap().text, "ok");
        assert!(
            started.elapsed() > Duration::from_millis(2400),
            "the idle limit was 1s, so this call outlived it"
        );
    }

    #[test]
    fn callback_false_interrupts_and_drops_the_connection() {
        let pieces: Vec<String> = (0..50)
            .map(|i| format!("data: {}\n\n", delta(&format!("w{i} "))))
            .collect();
        let server = fake::serve(vec![Script::SseSlow {
            pieces,
            pause: Duration::from_millis(100),
        }]);
        let mut seen = Vec::new();
        let started = Instant::now();
        let mut on_text = |piece: &str| {
            seen.push(piece.to_string());
            seen.len() < 2
        };
        let error = chat_stream(
            &server.url,
            None,
            SERVER_MODEL,
            &chat_request(vec![]),
            30,
            &mut on_text,
        )
        .unwrap_err();
        assert!(error.downcast_ref::<Interrupted>().is_some(), "{error:#}");
        assert_eq!(error.to_string(), "interrupted");
        assert_eq!(seen.len(), 2, "no deltas after the refusal");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn server_ignoring_stream_flag_still_works() {
        let body = json!({"choices": [{"finish_reason": "stop", "message": {"content": "whole"}}],
                          "usage": {"prompt_tokens": 1, "completion_tokens": 1}});
        let server = fake::serve(vec![fake::json(200, &body)]);
        let (reply, pieces) = collect(&server.url, None, &chat_request(vec![]), 10);
        assert_eq!(reply.unwrap().text, "whole");
        assert_eq!(pieces, ["whole"]);
    }

    #[test]
    fn tls_bad_record_mac_reconnects_only_before_a_response() {
        let error = transport_error(
            ureq::Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                rustls::Error::AlertReceived(rustls::AlertDescription::BadRecordMac),
            )),
            "https://chatgpt.com/backend-api/codex/responses?secret=hidden",
            None,
        );
        assert!(is_transient(&error), "{error:#}");
        assert!(!error.to_string().contains("secret"));
        assert!(!is_transient(&response_error(error, None)));

        for tls in [
            rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer),
            rustls::Error::AlertReceived(rustls::AlertDescription::HandshakeFailure),
            rustls::Error::General("received fatal alert: BadRecordMac".into()),
        ] {
            let error = transport_error(
                ureq::Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, tls)),
                "https://chatgpt.com/backend-api/codex/responses",
                None,
            );
            assert!(!is_transient(&error), "{error:#}");
        }
        let error = transport_error(
            ureq::Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "received fatal alert: BadRecordMac",
            )),
            "https://chatgpt.com/backend-api/codex/responses",
            None,
        );
        assert!(
            !is_transient(&error),
            "text alone must not classify a TLS error"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn socket_invalid_argument_retries_only_before_a_response() {
        let error = transport_error(
            ureq::Error::Io(std::io::Error::from_raw_os_error(libc::EINVAL)),
            "https://api.anthropic.com/v1/messages?secret=hidden",
            None,
        );
        assert!(is_transient(&error), "{error:#}");
        assert!(!error.to_string().contains("secret"));
        // Invalid headers/settings are client errors, even when their I/O kind is InvalidInput.
        let invalid = transport_error(
            ureq::Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid setting",
            )),
            "https://api.anthropic.com/v1/messages",
            None,
        );
        assert!(!is_transient(&invalid));
        // Once a response is being consumed, reconnecting could duplicate partial output.
        assert!(!is_transient(&response_error(error, None)));
    }

    #[test]
    fn http_errors_are_typed_and_classified() {
        let server = fake::serve(vec![
            fake::json(429, &json!({"error": "slow down"})),
            fake::json(503, &json!({"error": "busy"})),
        ]);
        let (limited, _) = collect(&server.url, None, &chat_request(vec![]), 10);
        let limited = limited.unwrap_err();
        assert_eq!(
            limited.downcast_ref::<HttpError>().map(|e| e.status),
            Some(429)
        );
        assert!(is_transient(&limited));
        let url = server.url.clone();
        let busy = chat(&url, SERVER_MODEL, &chat_request(vec![]), 10).unwrap_err();
        assert!(is_transient(&busy), "{busy:#}");
        assert!(busy.to_string().contains("HTTP 503"));

        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let refused = chat(
            &format!("http://127.0.0.1:{port}"),
            SERVER_MODEL,
            &chat_request(vec![]),
            5,
        )
        .unwrap_err();
        assert!(is_transient(&refused), "{refused:#}");
    }

    #[test]
    fn server_error_messages_replace_raw_json_and_load_failures_get_a_next_step() {
        let server = fake::serve(vec![
            fake::json(
                400,
                &json!({"error": {"message": "Failed to load model \"big-model\". Error: out of memory", "type": "invalid_request_error"}}),
            ),
            fake::json(500, &json!({"error": "plain string error"})),
            fake::json(
                400,
                &json!({"error": {"message": "Unrelated validation failure"}}),
            ),
        ]);
        let load = chat(&server.url, SERVER_MODEL, &chat_request(vec![]), 5)
            .unwrap_err()
            .to_string();
        assert!(
            load.contains("Failed to load model \"big-model\""),
            "{load}"
        );
        assert!(
            !load.contains("invalid_request_error") && !load.contains('{'),
            "no raw JSON in the message: {load}"
        );
        assert!(load.contains("Load it in LM Studio first"), "{load}");
        let plain = chat(&server.url, SERVER_MODEL, &chat_request(vec![]), 5)
            .unwrap_err()
            .to_string();
        assert!(
            plain.contains("HTTP 500")
                && plain.contains("plain string error")
                && !plain.contains("LM Studio"),
            "{plain}"
        );
        let other = chat(&server.url, SERVER_MODEL, &chat_request(vec![]), 5)
            .unwrap_err()
            .to_string();
        assert!(
            other.contains("Unrelated validation failure") && !other.contains("LM Studio"),
            "the hint is only for load failures: {other}"
        );
    }

    #[test]
    fn generic_error_message_reports_provider_error_type() {
        let server = fake::serve(vec![fake::json(
            400,
            &json!({"type":"error","error":{"type":"permission_error","message":"Error"}}),
        )]);
        let err = chat(&server.url, SERVER_MODEL, &chat_request(vec![]), 5)
            .unwrap_err()
            .to_string();
        assert!(err.contains("(type: permission_error)"), "{err}");
    }

    #[test]
    fn image_support_hint_requires_an_explicit_model_capability_error() {
        for (message, hint) in [
            (
                "messages.1.content.0: Input tag 'dial_response_item' found using 'type' does not match any expected tags: 'document', 'image', 'text', 'tool_use'",
                false,
            ),
            ("Image exceeds maximum dimensions", false),
            ("Invalid image format", false),
            ("This model does not support image input", true),
            (
                "Invalid content type. image_url is only supported by certain models.",
                true,
            ),
            ("Image inputs are not supported by this model", true),
        ] {
            let error = http_error(
                400,
                &Bounded {
                    data: serde_json::to_vec(
                        &json!({"error":{"type":"invalid_request_error","message":message}}),
                    )
                    .unwrap(),
                    truncated: false,
                },
                None,
                None,
            )
            .to_string();
            assert_eq!(
                error.contains("select a vision-capable model"),
                hint,
                "{error}"
            );
            assert!(
                error.contains(message),
                "keep the actual provider error: {error}"
            );
        }
    }

    #[test]
    fn bearer_token_is_sent_only_when_given_and_never_echoed() {
        let ok = json!({"choices": [{"finish_reason": "stop", "message": {"content": "hi"}}]});
        let server = fake::serve(vec![fake::json(200, &ok), fake::json(200, &ok)]);
        chat_with_auth(
            &server.url,
            Some("sk-secret-123"),
            SERVER_MODEL,
            &chat_request(vec![]),
            5,
        )
        .unwrap();
        chat_with_auth(&server.url, None, SERVER_MODEL, &chat_request(vec![]), 5).unwrap();
        let requests = server.requests();
        assert_eq!(
            header(&requests[0], "authorization").as_deref(),
            Some("Bearer sk-secret-123")
        );
        assert_eq!(header(&requests[1], "authorization"), None);

        // A server that echoes the credential back must not make it into the error.
        let echo = fake::serve(vec![fake::json(
            401,
            &json!({"error": "invalid key sk-secret-123"}),
        )]);
        let error = chat_with_auth(
            &echo.url,
            Some("sk-secret-123"),
            SERVER_MODEL,
            &chat_request(vec![]),
            5,
        )
        .unwrap_err();
        let text = format!("{error:#} {error:?}");
        assert!(!text.contains("sk-secret-123"), "{text}");
        assert!(
            text.contains("HTTP 401") && text.contains("[redacted]"),
            "{text}"
        );

        // Unusable tokens are refused without being quoted.
        for bad in ["bad token", "line\nbreak", ""] {
            let error = chat_with_auth(
                "http://127.0.0.1:9",
                Some(bad),
                SERVER_MODEL,
                &chat_request(vec![]),
                2,
            )
            .unwrap_err();
            let text = format!("{error:#}");
            assert!(text.contains("api key"), "{text}");
            assert!(bad.is_empty() || !text.contains(bad), "{text}");
        }

        // Connection failures do not carry the token either.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let error = chat_with_auth(
            &format!("http://127.0.0.1:{port}"),
            Some("sk-secret-123"),
            SERVER_MODEL,
            &chat_request(vec![]),
            2,
        )
        .unwrap_err();
        assert!(!format!("{error:#}").contains("sk-secret-123"));
    }

    #[test]
    fn hermes_text_calls_become_structured_when_tools_are_offered() {
        let text = "I'll read it.\n<tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"a.txt\"}}\n</tool_call>";
        let body = json!({"choices": [{"finish_reason": "stop", "message": {"content": text}}]});
        let server = fake::serve(vec![fake::json(200, &body), fake::json(200, &body)]);
        let reply = chat(&server.url, SERVER_MODEL, &chat_request(a_tool()), 5).unwrap();
        assert_eq!(reply.text, "I'll read it.");
        assert_eq!(reply.finish_reason, "tool_calls");
        assert_eq!(reply.tool_calls[0].name, "read_file");
        assert_eq!(reply.tool_calls[0].arguments, json!({"path": "a.txt"}));
        // Without tools the text is just text.
        let plain = chat(&server.url, SERVER_MODEL, &chat_request(vec![]), 5).unwrap();
        assert_eq!(plain.text, text);
        assert!(plain.tool_calls.is_empty());
    }

    #[test]
    fn streamed_hermes_calls_are_hidden_from_the_text_callback() {
        let body = sse(&[
            delta("Checking "),
            delta("<tool"),
            delta("_call>{\"name\": \"read_file\", "),
            delta("\"arguments\": {\"path\": \"x\"}}</tool_call>"),
            stop(),
        ]);
        let server = fake::serve(vec![Script::Sse(body)]);
        let (reply, pieces) = collect(&server.url, None, &chat_request(a_tool()), 10);
        let reply = reply.unwrap();
        assert_eq!(pieces.concat(), "Checking ");
        assert_eq!(reply.text, "Checking");
        assert_eq!(reply.tool_calls[0].arguments, json!({"path": "x"}));
        assert_eq!(reply.finish_reason, "tool_calls");
    }

    #[test]
    fn structured_calls_are_left_alone_by_the_text_fallback() {
        let body = json!({"choices": [{"finish_reason": "tool_calls", "message": {
            "content": "<tool_call>{\"name\":\"other\",\"arguments\":{}}</tool_call>",
            "tool_calls": [{"id": "s", "function": {"name": "read_file", "arguments": "{}"}}]}}]});
        let server = fake::serve(vec![fake::json(200, &body)]);
        let reply = chat(&server.url, SERVER_MODEL, &chat_request(a_tool()), 5).unwrap();
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].name, "read_file");
    }
}

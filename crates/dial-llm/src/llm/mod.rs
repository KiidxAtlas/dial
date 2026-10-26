//! Blocking OpenAI-compatible chat client for `mlx_lm.server` and remote
//! OpenAI-style endpoints.
//!
//! The client never retries (see `provider` for bounded endpoint retries) and
//! never echoes request content or credentials into errors: prompts may
//! contain private data, so error text carries only the HTTP status and a
//! short excerpt of the *server's* reply, with the bearer token redacted.
//!
//! Besides plain and streaming (SSE) completions this module lifts tool calls
//! that a server returned as plain text (`<tool_call>…</tool_call>`) into structured
//! [`ToolCall`]s when the request offered tools.

pub mod anthropic;
mod anthropic_cache;
pub mod anthropic_oauth;
pub mod apple;
pub mod auth;
mod aws_credentials;
mod azure;
mod bedrock;
mod cancel;
pub mod catalog;
pub mod chatgpt;
mod cloud_code_assist;
pub mod codex_ws;
mod connect;
mod copilot;
mod coreweave;
mod cursor;
mod devin;
pub mod download;
pub mod errors;
mod factory_droid;
pub mod fallback;
pub(crate) mod gemini;
mod gitlab_duo;
pub mod images;
pub mod json_repair;
mod key_pool;
pub mod local_evidence;
pub mod local_fit;
pub mod local_format;
pub mod local_hints;
pub mod local_runtime;
pub mod local_sampling;
pub mod local_server;
mod markup;
pub mod messages;
pub mod model_config;
pub mod model_sampling;
pub mod oauth;
pub mod ollama;
pub mod policy;
pub mod profiles;
pub mod prompt;
pub mod provider;
mod reasoning_memory;
pub mod reliability;
pub mod responses;
mod retry;
pub mod secrets;
pub mod server;
pub mod steering;
mod stream;
pub mod template_options;
mod text_tool_calls;
pub mod thinking_loop;
pub mod tool_repair;
mod transports;
pub mod usage;
pub mod usage_report;
mod vertex;
mod vertex_adc;
pub mod wire;
pub(crate) use cancel::Patience;
#[cfg(any(test, feature = "test-support"))]
pub mod fake;

/// Whether `url` is on `origin` (`scheme://host:port`), so `…:5000` does not match `…:50001`.
#[cfg(any(test, feature = "test-support"))]
fn same_origin(url: &str, origin: &str) -> bool {
    url.strip_prefix(origin)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

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

/// Largest accepted non-streaming response body.
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// Largest accepted raw streamed body (SSE/NDJSON framing included). Streaming wraps every few
/// tokens in JSON, so this is several times the longest decoded reply. Exceeding it ends the
/// attempt as a retryable incomplete stream, never as a partially executed reply.
pub const MAX_STREAM_BYTES: usize = 64 * 1024 * 1024;
/// Most error-body bytes read for classification (Google RPC details follow the message).
const ERROR_EXCERPT_BYTES: usize = 4096;
/// Longest server error excerpt placed in an error message.
const DISPLAY_EXCERPT_BYTES: usize = 512;

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
    /// Decided from the full body before any redaction replaces the excerpt.
    pub class: errors::ErrorClass,
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
        return http.class.is_retryable();
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

impl Chat {
    /// The request as the JSON object `before_provider_request` hooks receive and may rewrite.
    pub fn to_payload(&self) -> Value {
        serde_json::json!({
            "messages": self.messages,
            "tools": self.tools,
            "temperature": self.temperature,
            "max_tokens": self.max_tokens,
            "seed": self.seed,
        })
    }

    /// Rebuild the request from a hook-rewritten payload. A malformed payload is an error, so the
    /// provider never receives a body the hooks did not validly produce.
    pub fn from_payload(payload: Value) -> Result<Self> {
        #[derive(serde::Deserialize)]
        struct Wire {
            messages: Vec<Value>,
            tools: Vec<Value>,
            temperature: f64,
            max_tokens: u32,
            seed: Option<u64>,
        }
        let wire: Wire = serde_json::from_value(payload).map_err(|error| {
            anyhow::anyhow!("before_provider_request payload is not a valid request: {error}")
        })?;
        Ok(Self {
            messages: wire.messages,
            tools: wire.tools,
            temperature: wire.temperature,
            max_tokens: wire.max_tokens,
            seed: wire.seed,
        })
    }
}

/// Canonical serialized input envelope size, without allocating a second copy of its JSON.
pub fn input_bytes(request: &Chat) -> Result<usize> {
    #[derive(serde::Serialize)]
    struct Input<'a> {
        messages: &'a [Value],
        tools: &'a [Value],
    }
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.checked_add(bytes.len()).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "serialized input size overflow",
                )
            })?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(
        &mut counter,
        &Input {
            messages: &request.messages,
            tools: &request.tools,
        },
    )?;
    Ok(counter.0)
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
    /// Total prompt-cache writes (all TTL buckets). Anthropic: `usage.cache_creation_input_tokens`,
    /// or the sum of the reported buckets when only the split is present.
    pub cache_creation_tokens: Option<u64>,
    /// Anthropic 5-minute cache writes (`usage.cache_creation.ephemeral_5m_input_tokens`).
    /// `None` when the provider did not report the split; never invented as zero.
    pub cache_creation_5m_tokens: Option<u64>,
    /// Anthropic 1-hour cache writes (`usage.cache_creation.ephemeral_1h_input_tokens`).
    /// `None` when the provider did not report the split; never invented as zero.
    pub cache_creation_1h_tokens: Option<u64>,
    /// Native content blocks (including signed/redacted thinking) required for tool-turn replay.
    pub provider_content: Vec<Value>,
    /// Provider credit metering for the turn (Devin `creditCost`, `committedCreditCost`,
    /// `committedAcuCost`). `None` when the provider reported none; each field is set only when
    /// the provider reported it non-zero.
    pub credits: Option<ReplyCredits>,
    /// Fingerprint of the final outbound payload that produced this reply (cache diagnostics).
    pub wire: Option<wire::Observation>,
    /// Measured prompt-processing rate (uncached prompt tokens/s), when the server reported it.
    pub prefill_tokens_per_second: Option<f64>,
    /// Measured generation rate (output tokens/s), when the server reported it.
    pub decode_tokens_per_second: Option<f64>,
    /// Seconds the server spent loading the model for this request, when reported (Ollama).
    pub load_secs: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReplyCredits {
    pub cost: Option<i64>,
    pub committed_cost: Option<i64>,
    pub acu_cost: Option<f64>,
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
    /// Prompt tokens served from the server's prompt cache (llama.cpp `prompt_progress.cache`).
    #[serde(default)]
    pub cached_tokens: Option<u64>,
    /// Seconds remaining, computed by the agent from measured speed; never sent by a server.
    #[serde(default)]
    pub eta_secs: Option<f64>,
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
            cached_tokens: None,
            eta_secs: None,
        }
    }

    pub fn normalize(&mut self) {
        self.tokens_per_second = self.tokens_per_second.filter(|n| n.is_finite() && *n > 0.0);
        self.eta_secs = self.eta_secs.filter(|n| n.is_finite() && *n >= 0.0);
        if let (Some(done), Some(total)) = (self.processed_tokens, self.prompt_tokens) {
            self.processed_tokens = Some(done.min(total));
        }
    }
}

/// Incremental provider events, separate from visible answer text and retry notices.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// Backoff begins (`waiting`) or a fresh request starts. Consumers must
    /// reset request-local estimates and reasoning guards between attempts.
    Retry {
        waiting: bool,
    },
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
pub fn agent_with(
    timeout_secs: u64,
    env_proxy: bool,
    interrupt: Option<Arc<AtomicBool>>,
    patience: Option<Patience>,
) -> ureq::Agent {
    crate::install_tls_provider();
    let builder = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0);
    let builder = match patience {
        None => {
            builder.timeout_global((timeout_secs > 0).then(|| Duration::from_secs(timeout_secs)))
        }
        // `recv_*` ceilings are set (not left unset) so ureq reports the phase in each timeout; the transport reads
        // that to apply the first-event and idle limits.
        Some(_) => builder
            .timeout_global(None)
            .timeout_resolve(Some(Duration::from_secs(15)))
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_send_request(Some(Duration::from_secs(60)))
            .timeout_send_body(Some(Duration::from_secs(120)))
            .timeout_recv_response((timeout_secs > 0).then_some(STREAM_CEILING))
            .timeout_recv_body((timeout_secs > 0).then_some(STREAM_CEILING)),
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
pub fn cancellable_agent_with_resolver(
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

/// Whether `url` addresses a server reachable only from this machine or its network (see
/// [`is_local_host`]). A URL without a host is not local.
pub fn is_local_url(url: &str) -> bool {
    host_of(url).is_some_and(|host| is_local_host(&host))
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
/// content delta (text of `<tool_call>` blocks is withheld when
/// the request offers tools) and returns false to abandon the stream: the
/// connection is dropped and the call fails with [`Interrupted`].
///
/// Silence is bounded by `timeout_secs` after meaningful progress; local cold starts
/// may wait longer for the first event. A stream that ends before a `finish_reason`
/// or `[DONE]` is discarded (the caller already saw partial text through `on_text`).
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
    pub headers: &'a [(&'a str, &'a str)],
}

/// Retry-wide limits for one provider chat call: the per-request timeout and the cancellation flag.
pub struct Limits {
    pub timeout_secs: u64,
    pub interrupt: Arc<AtomicBool>,
}

/// Optional per-call controls for a chat request: cancellation, model profile, reasoning guard
/// and the event sink.
pub(super) struct Control<'a> {
    pub interrupt: Option<Arc<AtomicBool>>,
    pub profile: Option<&'a profiles::ModelProfile>,
    pub reasoning_guard: Option<&'a crate::config::types::ReasoningGuardConfig>,
    pub on_event: &'a mut dyn FnMut(&StreamEvent) -> bool,
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
            headers: &[],
            url,
            auth,
            model: model_label,
        },
        req,
        timeout_secs,
        Control {
            interrupt: None,
            profile: None,
            reasoning_guard: None,
            on_event: &mut |_| true,
        },
    )
}

fn chat_at_controlled(
    endpoint: Endpoint<'_>,
    req: &Chat,
    timeout_secs: u64,
    control: Control<'_>,
) -> Result<Reply> {
    let Control {
        interrupt,
        profile,
        reasoning_guard,
        on_event,
    } = control;
    let Endpoint {
        url,
        auth,
        model: model_label,
        headers,
    } = endpoint;
    let mut body = request_body(model_label, req, timeout_secs, false)?;
    apply_local_reasoning(&mut body, reasoning_guard);
    if let Some(profile) = profile {
        profile.validate_request(req, false)?;
        profile.apply_openai(&mut body);
    }
    let mut response = send_with_headers(url, auth, &body, timeout_secs, None, interrupt, headers)?;
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
    // The streamed path heals leaked `<think>` blocks the same way: they become reasoning, never
    // visible text that a later request would replay as the assistant's answer.
    let (visible_text, leaked_reasoning) = markup::rewrite(&reply.text);
    reply.text = visible_text;
    reply.reasoning.push_str(&leaked_reasoning);
    if !reply.reasoning.is_empty() && !on_event(&StreamEvent::Reasoning(reply.reasoning.clone())) {
        return Err(Interrupted.into());
    }
    if !req.tools.is_empty() {
        text_tool_calls::apply(&mut reply);
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
            headers: &[],
            url,
            auth,
            model: model_label,
        },
        req,
        timeout_secs,
        on_text,
        Control {
            interrupt: None,
            profile: None,
            reasoning_guard: None,
            on_event: &mut |_| true,
        },
    )
}

fn chat_stream_at_controlled(
    endpoint: Endpoint<'_>,
    req: &Chat,
    timeout_secs: u64,
    on_text: &mut dyn FnMut(&str) -> bool,
    control: Control<'_>,
) -> Result<Reply> {
    let Control {
        interrupt,
        profile,
        reasoning_guard,
        on_event,
    } = control;
    let Endpoint {
        url,
        auth,
        model: model_label,
        headers,
    } = endpoint;
    let mut body = request_body(model_label, req, timeout_secs, true)?;
    apply_local_reasoning(&mut body, reasoning_guard);
    let patience = Patience::for_stream(
        timeout_secs,
        host_of(url).is_some_and(|host| is_local_host(&host)),
    );
    if let Some(profile) = profile {
        profile.validate_request(req, false)?;
        profile.apply_openai(&mut body);
    }
    let mut response = send_with_headers(
        url,
        auth,
        &body,
        timeout_secs,
        Some(patience.clone()),
        interrupt,
        headers,
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
    let callback = std::cell::RefCell::new(on_event);
    let mut markup = markup::Healer::default();
    let mut hider = text_tool_calls::Hider::new(on_text, !req.tools.is_empty());
    let mut reply = if is_sse {
        let mut accumulator = stream::Accumulator::with_patience(patience.clone());
        let mut forward = |piece: &str| {
            markup.push(piece, &mut |text| hider.push(text), &mut |text| {
                (callback.borrow_mut())(&StreamEvent::Reasoning(text.to_owned()))
            })
        };
        let read = stream::read_events_with_progress(
            response.body_mut().as_reader(),
            MAX_STREAM_BYTES,
            stream::MAX_LINE_BYTES,
            patience,
            // The accumulator marks semantic progress while parsing once; role-only
            // preambles and empty data keepalives must not restart the watchdog.
            |_| false,
            |data| {
                accumulator.feed_with_events(data, &mut forward, &mut |event| {
                    (callback.borrow_mut())(event)
                })
            },
        );
        if let Err(error) = read {
            // A finish_reason commits this reply. Disconnection or expiration of
            // the trailing-usage grace window must not discard or regenerate it.
            if !accumulator.is_complete()
                || (error.downcast_ref::<stream::IncompleteStream>().is_none()
                    && error.downcast_ref::<stream::StalledStream>().is_none())
            {
                return Err(response_error(error, auth));
            }
        }
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
            && !(callback.borrow_mut())(&StreamEvent::Reasoning(reply.reasoning.clone()))
        {
            return Err(Interrupted.into());
        }
        let mut visible = |text: &str| hider.push(text);
        let mut reasoning =
            |text: &str| (callback.borrow_mut())(&StreamEvent::Reasoning(text.to_owned()));
        if !markup.push(&reply.text, &mut visible, &mut reasoning) {
            return Err(Interrupted.into());
        }
        reply
    };
    let mut visible = |text: &str| hider.push(text);
    let mut reasoning =
        |text: &str| (callback.borrow_mut())(&StreamEvent::Reasoning(text.to_owned()));
    if !markup.finish(&mut visible, &mut reasoning) {
        return Err(Interrupted.into());
    }
    let (visible_text, leaked_reasoning) = markup::rewrite(&reply.text);
    reply.text = visible_text;
    reply.reasoning.push_str(&leaked_reasoning);
    if !hider.finish() {
        return Err(Interrupted.into());
    }
    if !req.tools.is_empty() {
        text_tool_calls::apply(&mut reply);
    }
    Ok(reply)
}

fn apply_local_reasoning(
    body: &mut Value,
    guard: Option<&crate::config::types::ReasoningGuardConfig>,
) {
    if let Some(guard) = guard {
        // Leave room for the closing marker and transport latency before the client fallback.
        body["dial_reasoning"] = json!({
            "max_bytes": guard.max_reasoning_bytes.saturating_mul(3) / 4,
            "max_seconds": if guard.enabled { guard.max_pre_action_secs as f64 * 0.75 } else { 0.0 },
        });
    }
}

fn request_body(
    model_label: &str,
    req: &Chat,
    _timeout_secs: u64,
    streaming: bool,
) -> Result<Value> {
    ensure!(
        req.temperature.is_finite() && req.temperature >= 0.0,
        "temperature must be finite and non-negative; the server drops the connection otherwise"
    );
    let mut body = json!({
        "model": model_label,
        "messages": req.messages,
        // Some OpenAI-compatible servers validate even tool-free maintenance requests
        // against a required array field. Never send null or rely on their default.
        "tools": req.tools,
        "temperature": req.temperature,
        "max_tokens": req.max_tokens,
        "stream": streaming,
    });
    for message in body["messages"].as_array_mut().into_iter().flatten() {
        messages::canonicalize(message);
        if let Some(object) = message.as_object_mut() {
            object.remove("dial_provider_content");
        }
    }
    if streaming {
        body["stream_options"] = json!({ "include_usage": true });
    }
    if let Some(seed) = req.seed {
        body["seed"] = json!(seed);
    }
    Ok(body)
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
    let mut request = post_with_headers(url, auth, timeout_secs, patience, interrupt, headers)?;
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
    {
        request = request.header("Content-Type", "application/json");
    }
    // ureq's send_json pretty-prints (indented whitespace is billed as prompt tokens); send compact JSON.
    let bytes = serde_json::to_vec(body)?;
    usage::transport_attempt()?;
    wire::record(&bytes, "http");
    request
        .send(&bytes)
        .map_err(|error| transport_error(error, url, auth))
}

/// Send exactly the bytes a signing transport hashed; never reserialize a signed body.
pub(super) fn send_prepared_with_headers(
    url: &str,
    auth: Option<&str>,
    body: &[u8],
    timeout_secs: u64,
    patience: Option<Patience>,
    interrupt: Option<Arc<AtomicBool>>,
    headers: &[(&str, &str)],
) -> Result<ureq::http::Response<ureq::Body>> {
    let mut request = post_with_headers(url, auth, timeout_secs, patience, interrupt, headers)?;
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
    {
        request = request.header("Content-Type", "application/json");
    }
    usage::transport_attempt()?;
    wire::record(body, "http");
    request
        .send(body)
        .map_err(|error| transport_error(error, url, auth))
}

fn post_with_headers(
    url: &str,
    auth: Option<&str>,
    timeout_secs: u64,
    patience: Option<Patience>,
    interrupt: Option<Arc<AtomicBool>>,
    headers: &[(&str, &str)],
) -> Result<ureq::RequestBuilder<ureq::typestate::WithBody>> {
    if let Some(token) = auth {
        ensure!(
            !token.is_empty() && token.bytes().all(|b| b.is_ascii_graphic()),
            "api key is empty or contains characters that are not allowed in an HTTP header"
        );
    }
    for (name, value) in headers {
        ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
                && value.bytes().all(|b| (b' '..=b'~').contains(&b)),
            "provider request header contains invalid characters"
        );
    }
    let local = host_of(url).is_some_and(|host| is_local_host(&host));
    let mut request = agent_with(timeout_secs, !local, interrupt, patience.clone()).post(url);
    if patience.is_some()
        && !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("accept"))
    {
        request = request.header("Accept", "text/event-stream");
    }
    if let Some(token) = auth {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    for (name, value) in headers {
        request = request.header(*name, model_config::private_header_value(value)?);
    }
    Ok(request)
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
    let cause = match &error {
        // ureq names only the phase ("timeout: receive response"), which reads like a client fault.
        ureq::Error::Timeout(ureq::Timeout::RecvResponse) => {
            "no response before the silence limit (the server may be overloaded or the connection was lost)"
                .to_owned()
        }
        error => error.to_string(),
    };
    let message = redact(
        &format!("chat request to {} failed: {cause}", display_url(url)),
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

/// Only explicitly classified incomplete or stalled streams survive as retryable errors.
/// Other post-response failures are redacted and never become transport errors.
fn response_error(error: anyhow::Error, auth: Option<&str>) -> anyhow::Error {
    if error.downcast_ref::<Interrupted>().is_some()
        || error.downcast_ref::<stream::IncompleteStream>().is_some()
        || error.downcast_ref::<stream::StalledStream>().is_some()
        || error.downcast_ref::<responses::TerminalFailure>().is_some()
        || error
            .downcast_ref::<anthropic::IncompleteStream>()
            .is_some()
    {
        error
    } else if let Some(classified) = error.downcast_ref::<errors::Classified>() {
        // The class survives redaction so classification and retry stay unchanged.
        errors::Classified::new(
            classified.class.clone(),
            redact(&format!("{error:#}"), auth),
        )
        .into()
    } else {
        anyhow::anyhow!("{}", redact(&format!("{error:#}"), auth))
    }
}

fn http_error(
    status: u16,
    body: &Bounded,
    auth: Option<&str>,
    hints: impl Into<retry::ServerHints>,
) -> anyhow::Error {
    let mut hints = hints.into();
    if hints.delay.is_none() {
        hints.delay = retry::body_delay(&body.data);
    }
    let retry_after = hints.delay;
    // Parse the body once; servers wrap the useful sentence in JSON, which reads better than a
    // one-line blob. A truncated body is never trusted for the message or type.
    let parsed = serde_json::from_slice::<serde_json::Value>(&body.data).ok();
    let message = if body.truncated {
        None
    } else {
        parsed.as_ref().and_then(|value| {
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
        parsed.as_ref().and_then(|value| {
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
    // Redact the whole text first so truncation can never leave a partial credential behind.
    let mut excerpt = match &message {
        Some(message) => redact(&self::excerpt(message.as_bytes(), false), auth),
        None => redact(&self::excerpt(&body.data, body.truncated), auth),
    };
    if excerpt.len() > DISPLAY_EXCERPT_BYTES {
        let mut end = DISPLAY_EXCERPT_BYTES;
        while !excerpt.is_char_boundary(end) {
            end -= 1;
        }
        excerpt.truncate(end);
        excerpt.push('…');
    }
    // Some providers (Anthropic 429s) send only "Error"; the error type is then the only signal.
    let generic = message
        .as_deref()
        .is_none_or(|m| m.eq_ignore_ascii_case("error"));
    if let Some(kind) = error_type.as_deref().filter(|_| generic) {
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
    } else if matches!(status, 400 | 404 | 422)
        && message.as_deref().is_some_and(|m| {
            m.to_ascii_lowercase()
                .contains("system message must be at the beginning")
        })
    {
        excerpt.push_str(" — the model's chat template rejected the message layout, not the model selection; it requires a single system message at the beginning.");
    } else if status == 404 {
        excerpt.push_str(" — refresh /models and select an available model; check the provider endpoint in /providers.");
    }
    if message
        .as_deref()
        .is_some_and(|m| errors::LLAMA_NO_JINJA.is_match(m))
    {
        excerpt.push_str(" — this llama-server has Jinja chat templates disabled, so it cannot accept tool calls; restart it with --jinja.");
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
    // Classify from the whole (possibly truncated) body, including error codes the excerpt drops,
    // before any custom-header redaction can withhold it.
    let code = parsed
        .as_ref()
        .and_then(|value| {
            value
                .pointer("/error/code")
                .or_else(|| value.pointer("/error/status"))
                .or_else(|| value.get("code"))
                .map(|code| {
                    code.as_str()
                        .map_or_else(|| code.to_string(), str::to_owned)
                })
        })
        .unwrap_or_default();
    let class_text = format!(
        "{} {} {code} {}",
        message.as_deref().unwrap_or_default(),
        error_type.as_deref().unwrap_or_default(),
        String::from_utf8_lossy(&body.data)
    );
    let class = errors::classify_http(status, &class_text, retry_after, hints.reset_at);
    anyhow::Error::new(HttpError {
        status,
        excerpt,
        retry_after,
        class,
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
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect(),
        Some(_) => bail!("chat response message content is not a string or text parts"),
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
            .and_then(Value::as_str)
            .or_else(|| message.get("reasoning").and_then(Value::as_str))
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
    let mut seen: HashSet<String> = HashSet::new();
    let server_ids: HashSet<&str> = calls
        .iter()
        .filter_map(|call| call.get("id").and_then(Value::as_str))
        .filter(|id| !id.is_empty())
        .collect();
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
            _ => {
                let mut fallback = format!("call_{index}");
                let mut suffix = 0usize;
                while seen.contains(&fallback) || server_ids.contains(fallback.as_str()) {
                    suffix += 1;
                    fallback = format!("call_{index}_{suffix}");
                }
                fallback
            }
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
mod tests;

#[cfg(test)]
mod net_tests;

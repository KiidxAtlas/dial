//! Devin (Cascade) native transport: Connect RPC over HTTP with hand-encoded protobuf.
//!
//! `GetUserJwt` authenticates the session token,
//! `AssignModel` resolves the `adaptive` router, and `GetChatMessage` streams
//! gzip-compressed Connect frames. Only the protobuf fields Dial uses are encoded
//! and decoded; field numbers are pinned to the Devin wire schema.

use super::Patience;
use super::connect::{Enc, FLAG_COMPRESSED, FLAG_END_STREAM, Fields, Frames, frame, gunzip, gzip};
use super::errors::{Classified, ErrorClass};
use super::model_config::native_replay_scope;
use super::profiles::ModelProfile;
use super::transports::{OnText, Route};
use super::{Chat, Interrupted, Reply, StreamEvent, ToolCall};
use crate::hex;
use anyhow::{Context, Result, bail, ensure};
use regex::Regex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::io::Read;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};

const AUTH_PATH: &str = "/exa.auth_pb.AuthService/GetUserJwt";
const ASSIGN_PATH: &str = "/exa.api_server_pb.ApiServerService/AssignModel";
const CHAT_PATH: &str = "/exa.api_server_pb.ApiServerService/GetChatMessage";
/// Server-side router uid; the only model whose uid must be resolved by `AssignModel`.
const ROUTER_MODEL: &str = "adaptive";
const SESSION_TOKEN_PREFIX: &str = "devin-session-token$";
const CLI_VERSION: &str = "3000.11.3";
const USER_AGENT: &str = "connect-go/1.18.1 (go1.26.3)";
const STOP_PATTERNS: [&str; 5] = [
    "<|user|>",
    "<|bot|>",
    "<|context_request|>",
    "<|endoftext|>",
    "<|end_of_turn|>",
];
const TRAILER_EVIDENCE_CHARS: usize = 2000;
const LARGE_HISTORY_RECOVERY_BYTES: usize = 512 * 1024;
/// Trailer messages treated as an opaque backend failure (`\binternal error\b`).
static INTERNAL_ERROR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\binternal error\b").expect("static pattern"));

// Protobuf enum values (`devin-proto.ts`).
const SOURCE_USER: u64 = 1;
const SOURCE_SYSTEM: u64 = 2;
const SOURCE_TOOL: u64 = 4;
const REQUEST_TYPE_CASCADE: u64 = 5;
const PLANNER_MODE_DEFAULT: u64 = 1;
const CACHE_CONTROL_EPHEMERAL: u64 = 1;
const STOP_REASON_MAX_TOKENS: u64 = 3;

const UNARY_HEADERS: [(&str, &str); 3] = [
    ("content-type", "application/proto"),
    ("connect-protocol-version", "1"),
    ("accept", "*/*"),
];

/// Native replay carrier type stored in `dial_provider_content`.
const CARRIER_TYPE: &str = "dial_devin_message";

pub(super) fn exchange(
    route: &Route<'_>,
    req: &Chat,
    timeout: u64,
    on_text: OnText<'_, '_>,
    interrupt: Arc<AtomicBool>,
    _profile: Option<&ModelProfile>,
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
) -> Result<Reply> {
    let api_key = route
        .auth
        .filter(|token| !token.is_empty())
        .context("Devin requires a session token; sign in to the devin provider")?;
    let base = route.base_url.trim_end_matches('/');
    let session = authenticate(route, base, api_key, timeout, &interrupt)?;
    let cascade = route
        .session_id
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(random_uuid);
    let scope = native_replay_scope(
        "devin-agent",
        route.base_url,
        Some(route.model),
        route.account_id,
        route.auth,
        route.headers,
    );
    let (chat_model_uid, assignment_jwt) = if route.model == ROUTER_MODEL {
        let (uid, jwt) = assign_model(
            route,
            &session,
            &cascade,
            &req.messages,
            timeout,
            &interrupt,
        )?;
        (uid, Some(jwt))
    } else {
        (route.model.to_owned(), None)
    };
    let request = chat_request(
        route.model,
        req,
        &session,
        &cascade,
        &scope,
        &chat_model_uid,
        assignment_jwt.as_deref(),
    )?;
    let body = frame(FLAG_COMPRESSED, &gzip(&request.0)?)?;
    let url = format!("{}{CHAT_PATH}", session.base);
    let mut headers: Vec<(&str, &str)> = vec![
        ("content-type", "application/connect+proto"),
        ("connect-protocol-version", "1"),
        ("connect-content-encoding", "gzip"),
        ("accept-encoding", "identity"),
        ("user-agent", USER_AGENT),
        ("connect-accept-encoding", "gzip"),
        ("accept", "*/*"),
    ];
    headers.extend(route.headers.iter().copied());
    let local = super::host_of(route.base_url).is_some_and(|host| super::is_local_host(&host));
    let patience = Patience::for_stream(timeout, local);
    let stream_patience = patience.clone();
    let mut response = super::send_prepared_with_headers(
        &url,
        None,
        &body,
        timeout,
        Some(patience),
        Some(Arc::clone(&interrupt)),
        &headers,
    )?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let hints = super::retry::server_delay(response.headers());
        let bounded =
            super::read_bounded(response.body_mut().as_reader(), super::ERROR_EXCERPT_BYTES)
                .context("reading Devin response")?;
        return Err(super::http_error(status, &bounded, route.auth, hints));
    }

    let mut state = Stream::default();
    let mut frames = Frames::default();
    let mut chunk = vec![0u8; 16 * 1024];
    let mut total = 0usize;
    let mut reader = response.body_mut().as_reader();
    loop {
        if interrupt.load(Ordering::SeqCst) {
            return Err(Interrupted.into());
        }
        let read = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(super::stream::frame_read_error(
                    error,
                    &stream_patience,
                    route.auth,
                ));
            }
        };
        total += read;
        ensure!(
            total <= super::MAX_STREAM_BYTES,
            "Devin stream exceeds {} bytes",
            super::MAX_STREAM_BYTES
        );
        for (flags, payload) in frames.push(&chunk[..read])? {
            if interrupt.load(Ordering::SeqCst) {
                return Err(Interrupted.into());
            }
            // Silence is judged per decoded frame, as it is for SSE events.
            stream_patience.mark_event();
            let emits = match state.feed(flags, &payload) {
                Ok(emits) => emits,
                Err(error) if state.rejected_internal => {
                    return Err(large_history_overflow(
                        error,
                        &req.messages,
                        &cascade,
                        &scope,
                    ));
                }
                Err(error) => return Err(error),
            };
            for emit in emits {
                match emit {
                    Emit::Text(text) => {
                        if let Some(callback) = on_text.as_deref_mut()
                            && !callback(&text)
                        {
                            return Err(Interrupted.into());
                        }
                    }
                    Emit::Event(event) => {
                        if !on_event(&event) {
                            return Err(Interrupted.into());
                        }
                    }
                }
            }
        }
    }
    frames.finish()?;
    if !state.ended {
        return Err(super::stream::IncompleteStream(
            "Devin stream ended before its completion trailer",
        )
        .into());
    }
    Ok(state.finish(&scope))
}

/// Session credentials resolved by `GetUserJwt`.
struct Session {
    /// Wire API key echoed in every `Metadata`.
    api_key: String,
    user_jwt: String,
    /// Chat/assign base: the server may name a custom API server.
    base: String,
}

fn authenticate(
    route: &Route<'_>,
    base: &str,
    api_key: &str,
    timeout: u64,
    interrupt: &Arc<AtomicBool>,
) -> Result<Session> {
    let session_key = session_token(api_key);
    let mut wire_key = session_key.clone();
    let mut attempt = auth_call(route, base, &session_key, timeout, interrupt)?;
    if attempt.status == 401 && api_key != session_key {
        attempt = auth_call(route, base, api_key, timeout, interrupt)?;
        wire_key = api_key.to_owned();
    }
    if let Some(error) = attempt.error {
        return Err(error);
    }
    let payload = unary_payload(&attempt.body)?;
    let fields = Fields::parse(&payload)?;
    let user_jwt = fields.string(1);
    ensure!(
        !user_jwt.is_empty(),
        "Devin auth error: GetUserJwt returned an empty user JWT"
    );
    let custom = fields.string(2);
    let custom = custom.trim().trim_end_matches('/');
    let base = if custom.is_empty() {
        base.to_owned()
    } else {
        validated_api_base(base, custom)?
    };
    Ok(Session {
        api_key: wire_key,
        user_jwt,
        base,
    })
}

/// The server may name its API host, which then receives the session token and API key. Accept only
/// HTTPS hosts in the configured host or the Devin domain family.
fn validated_api_base(configured: &str, custom: &str) -> Result<String> {
    let uri = custom
        .parse::<ureq::http::Uri>()
        .context("Devin returned an invalid API server URL")?;
    ensure!(
        uri.scheme_str() == Some("https"),
        "Devin returned a non-HTTPS API server"
    );
    let host = uri
        .host()
        .context("Devin returned an API server without a host")?
        .to_ascii_lowercase();
    let configured_host = super::host_of(configured)
        .unwrap_or_default()
        .to_ascii_lowercase();
    ensure!(
        host == configured_host || host == "devin.ai" || host.ends_with(".devin.ai"),
        "Devin returned an API server outside its host family"
    );
    Ok(custom.to_owned())
}

fn auth_call(
    route: &Route<'_>,
    base: &str,
    api_key: &str,
    timeout: u64,
    interrupt: &Arc<AtomicBool>,
) -> Result<Unary> {
    let mut request = Enc::default();
    request.message(1, &metadata(api_key, ""));
    unary(
        route,
        &format!("{base}{AUTH_PATH}"),
        &request.0,
        &UNARY_HEADERS,
        timeout,
        interrupt,
    )
}

/// Resolve the `adaptive` router into a concrete model uid and its assignment JWT.
fn assign_model(
    route: &Route<'_>,
    session: &Session,
    cascade: &str,
    messages: &[Value],
    timeout: u64,
    interrupt: &Arc<AtomicBool>,
) -> Result<(String, String)> {
    let mut request = Enc::default();
    request.message(1, &metadata(&session.api_key, ""));
    request.string(2, route.model);
    request.string(3, cascade);
    if let Some(prompt) = router_prompt(messages)? {
        request.message(5, &prompt.encode());
    }
    let attempt = unary(
        route,
        &format!("{}{ASSIGN_PATH}", session.base),
        &request.0,
        &UNARY_HEADERS,
        timeout,
        interrupt,
    )?;
    if let Some(error) = attempt.error {
        return Err(error);
    }
    let payload = unary_payload(&attempt.body)?;
    let assignment = Fields::parse(&payload)?.message(1)?.unwrap_or_default();
    let jwt = assignment.string(1);
    let uid = assignment.string(2);
    ensure!(
        !jwt.is_empty() && !uid.is_empty(),
        "Devin AssignModel error: response carried no assignment JWT and model uid"
    );
    Ok((uid, jwt))
}

/// A unary Connect response. `error` carries the classified HTTP error for non-2xx statuses.
struct Unary {
    status: u16,
    body: Vec<u8>,
    error: Option<anyhow::Error>,
}

fn unary(
    route: &Route<'_>,
    url: &str,
    body: &[u8],
    headers: &[(&str, &str)],
    timeout: u64,
    interrupt: &Arc<AtomicBool>,
) -> Result<Unary> {
    let mut response = super::send_prepared_with_headers(
        url,
        None,
        body,
        timeout,
        None,
        Some(Arc::clone(interrupt)),
        headers,
    )?;
    let status = response.status().as_u16();
    let hints = super::retry::server_delay(response.headers());
    let bounded = super::read_bounded(response.body_mut().as_reader(), super::MAX_RESPONSE_BYTES)
        .context("reading Devin response")?;
    if (200..300).contains(&status) {
        ensure!(
            !bounded.truncated,
            "Devin response exceeds {} bytes",
            super::MAX_RESPONSE_BYTES
        );
        return Ok(Unary {
            status,
            body: bounded.data,
            error: None,
        });
    }
    let error = super::http_error(status, &bounded, route.auth, hints);
    Ok(Unary {
        status,
        body: bounded.data,
        error: Some(error),
    })
}

/// Unary bodies arrive as bare protobuf or a gzipped protobuf; decode whichever parses.
fn unary_payload(body: &[u8]) -> Result<Cow<'_, [u8]>> {
    if Fields::parse(body).is_ok() {
        return Ok(Cow::Borrowed(body));
    }
    Ok(Cow::Owned(
        super::connect::gunzip_limited(body, super::MAX_RESPONSE_BYTES)
            .context("Devin response is neither protobuf nor gzipped protobuf")?,
    ))
}

fn session_token(api_key: &str) -> String {
    if api_key.starts_with(SESSION_TOKEN_PREFIX) {
        api_key.to_owned()
    } else {
        format!("{SESSION_TOKEN_PREFIX}{api_key}")
    }
}

fn os() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(windows) {
        "windows"
    } else {
        "linux"
    }
}

/// `Metadata` for released-CLI calls (`devin-proto.ts` field numbers).
fn metadata(api_key: &str, user_jwt: &str) -> Enc {
    let mut meta = Enc::default();
    meta.string(1, "devin-cli"); // ideName
    meta.string(2, CLI_VERSION); // extensionVersion
    meta.string(3, api_key);
    meta.string(4, "en"); // locale
    meta.string(5, os());
    meta.string(7, CLI_VERSION); // ideVersion
    meta.string(12, "chisel"); // extensionName
    meta.string(21, user_jwt);
    meta.string(28, "chisel"); // ideType: unlocks router assignment
    meta
}

/// Request body for `GetChatMessage`.
fn chat_request(
    route_model: &str,
    req: &Chat,
    session: &Session,
    cascade: &str,
    scope: &str,
    chat_model_uid: &str,
    assignment_jwt: Option<&str>,
) -> Result<Enc> {
    let gemini = route_model.to_ascii_lowercase().contains("gemini")
        || chat_model_uid.starts_with("MODEL_GOOGLE_GEMINI_");
    let (system, prompts) = build_prompts(&req.messages, cascade, scope)?;
    let mut request = Enc::default();
    request.message(1, &metadata(&session.api_key, &session.user_jwt));
    request.string(2, &system);
    for prompt in &prompts {
        request.message(3, &prompt.encode());
    }
    request.uint(7, REQUEST_TYPE_CASCADE);
    let mut config = Enc::default();
    config.uint(1, 1); // numCompletions
    config.uint(2, u64::from(req.max_tokens));
    config.uint(3, 200); // maxNewlines
    config.double(5, req.temperature);
    config.double(6, req.temperature); // firstTemperature
    config.uint(7, 50); // topK
    config.double(8, 1.0); // topP
    for pattern in STOP_PATTERNS {
        config.string(9, pattern);
    }
    config.double(11, 1.0); // fimEotProbThreshold
    request.message(8, &config);
    for tool in &req.tools {
        request.message(10, &tool_definition(tool, gemini)?);
    }
    // Model parallel-tool support is not carried in Dial's request; the default is `false`.
    request.flag(11, true);
    let mut choice = Enc::default();
    choice.string(1, "auto");
    request.message(12, &choice);
    let mut cache = Enc::default();
    cache.uint(1, CACHE_CONTROL_EPHEMERAL);
    request.message(13, &cache);
    request.string(16, cascade);
    request.uint(20, PLANNER_MODE_DEFAULT);
    request.string(21, chat_model_uid);
    request.string(22, &random_uuid());
    request.optional_string(26, assignment_jwt);
    Ok(request)
}

fn tool_definition(tool: &Value, gemini: bool) -> Result<Enc> {
    let function = &tool["function"];
    let name = function["name"]
        .as_str()
        .context("Devin tool definition has no name")?;
    let parameters = function
        .get("parameters")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let schema = if gemini {
        gemini_schema(&parameters)
    } else {
        parameters
    };
    let mut definition = Enc::default();
    definition.string(1, name);
    definition.string(2, function["description"].as_str().unwrap_or(""));
    definition.string(3, &schema.to_string());
    definition.flag(12, function["strict"].as_bool().unwrap_or(false));
    Ok(definition)
}

/// JSON Schema keywords the Gemini backend rejects. They are
/// removed from the wire schema and their values are spilled into the description instead.
const GEMINI_UNSUPPORTED_KEYWORDS: &[&str] = &[
    "$schema",
    "$id",
    "$anchor",
    "$ref",
    "$defs",
    "$dynamicRef",
    "$dynamicAnchor",
    "examples",
    "prefixItems",
    "unevaluatedProperties",
    "unevaluatedItems",
    "patternProperties",
    "additionalProperties",
    "propertyNames",
    "minItems",
    "maxItems",
    "uniqueItems",
    "contains",
    "minContains",
    "maxContains",
    "additionalItems",
    "minLength",
    "maxLength",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "pattern",
    "format",
    "contentEncoding",
    "contentMediaType",
    "contentSchema",
    "dependencies",
    "dependentSchemas",
    "dependentRequired",
    "x-mcp-header",
    "deprecated",
    "readOnly",
    "writeOnly",
    "$comment",
];

/// Gemini tool-schema dialect. JSON Schema type arrays
/// (`["number", "null"]`) collapse to their first non-null type and mark the node nullable; `const`
/// becomes a one-entry `enum`; unsupported keywords are stripped and spilled into `description`.
/// Property names under `properties` are data and stay verbatim.
fn gemini_schema(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            let mut spilled: Vec<(&str, &Value)> = Vec::new();
            for (key, item) in map {
                if key == "properties"
                    && let Value::Object(properties) = item
                {
                    let kept = properties
                        .iter()
                        .map(|(name, schema)| (name.clone(), gemini_schema(schema)))
                        .collect();
                    out.insert(key.clone(), Value::Object(kept));
                    continue;
                }
                if GEMINI_UNSUPPORTED_KEYWORDS.contains(&key.as_str()) {
                    spilled.push((key.as_str(), item));
                    continue;
                }
                if key == "type"
                    && let Value::Array(types) = item
                {
                    let non_null: Vec<&Value> = types
                        .iter()
                        .filter(|kind| kind.as_str() != Some("null"))
                        .collect();
                    if let Some(first) = non_null.first() {
                        out.insert("type".to_owned(), (*first).clone());
                    }
                    if non_null.len() != types.len() {
                        out.insert("nullable".to_owned(), Value::Bool(true));
                    }
                    continue;
                }
                if key == "const" {
                    if !map.contains_key("enum") {
                        out.insert("enum".to_owned(), json!([item.clone()]));
                    }
                    continue;
                }
                out.insert(key.clone(), gemini_schema(item));
            }
            if !spilled.is_empty() {
                let existing = out
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let formatted = format!(
                    "{{{}}}",
                    spilled
                        .iter()
                        .map(|(key, item)| format!("{key}: {item}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                let description = if existing.is_empty() {
                    formatted
                } else {
                    format!("{existing}\n\n{formatted}")
                };
                out.insert("description".to_owned(), json!(description));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(gemini_schema).collect()),
        other => other.clone(),
    }
}

/// One `ChatMessagePrompt`.
#[derive(Default)]
struct Prompt {
    message_id: String,
    source: u64,
    prompt: String,
    thinking: String,
    signature: String,
    /// `(id, name, argumentsJson)`.
    tool_calls: Vec<(String, String, String)>,
    tool_call_id: String,
    tool_result_is_error: bool,
    /// `(mimeType, base64Data)`.
    images: Vec<(String, String)>,
}

impl Prompt {
    fn encode(&self) -> Enc {
        let mut out = Enc::default();
        out.string(1, &self.message_id);
        out.uint(2, self.source);
        out.string(3, &self.prompt);
        for (id, name, arguments) in &self.tool_calls {
            let mut call = Enc::default();
            call.string(1, id);
            call.string(2, name);
            call.string(3, arguments);
            out.message(6, &call);
        }
        out.string(7, &self.tool_call_id);
        out.flag(9, self.tool_result_is_error);
        for (mime, data) in &self.images {
            let mut image = Enc::default();
            image.string(1, data);
            image.string(2, mime);
            out.message(10, &image);
        }
        out.string(11, &self.thinking);
        out.string(12, &self.signature);
        out
    }
}

/// Map OpenAI-shaped history onto Cascade prompts. Returns the flattened system prompt and the
/// `chatMessagePrompts` sequence. Assistant signatures are replayed only from carriers of this
/// same scope, matching the native-message check.
fn build_prompts(messages: &[Value], cascade: &str, scope: &str) -> Result<(String, Vec<Prompt>)> {
    let mut system = Vec::new();
    let mut prompts = Vec::new();
    for (index, msg) in messages.iter().enumerate() {
        match msg["role"].as_str().unwrap_or("") {
            "system" => {
                let text = text_of(&msg["content"]);
                if !text.is_empty() {
                    system.push(text);
                }
            }
            role @ ("user" | "developer") => {
                let seed = format!("{cascade}\0{index}\0{role}");
                prompts.push(user_prompt(&msg["content"], deterministic_uuid(&seed))?);
            }
            "assistant" => {
                let native = carrier(msg, scope);
                let text = text_of(&msg["content"]);
                let thinking = native
                    .and_then(|item| item["thinking"].as_str())
                    .unwrap_or("")
                    .to_owned();
                let signature = native
                    .and_then(|item| item["signature"].as_str())
                    .unwrap_or("")
                    .to_owned();
                let mut tool_calls = Vec::new();
                for call in msg["tool_calls"].as_array().into_iter().flatten() {
                    let arguments = match &call["function"]["arguments"] {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    };
                    tool_calls.push((
                        call["id"].as_str().unwrap_or("").to_owned(),
                        call["function"]["name"].as_str().unwrap_or("").to_owned(),
                        arguments,
                    ));
                }
                if text.is_empty()
                    && thinking.is_empty()
                    && signature.is_empty()
                    && tool_calls.is_empty()
                {
                    continue;
                }
                let native_id = native
                    .and_then(|item| item["message_id"].as_str())
                    .filter(|id| !id.is_empty());
                let message_id = match native_id {
                    Some(id) => id.to_owned(),
                    None => format!(
                        "bot-{}",
                        deterministic_uuid(&format!("{cascade}\0{index}\0assistant"))
                    ),
                };
                prompts.push(Prompt {
                    message_id,
                    source: SOURCE_SYSTEM,
                    prompt: text,
                    thinking,
                    signature,
                    tool_calls,
                    ..Prompt::default()
                });
            }
            "tool" => {
                let tool_call_id = msg["tool_call_id"].as_str().unwrap_or("").to_owned();
                let seed = format!("{cascade}\0{index}\0tool\0{tool_call_id}");
                let mut prompt = user_prompt(&msg["content"], deterministic_uuid(&seed))?;
                prompt.source = SOURCE_TOOL;
                prompt.tool_call_id = tool_call_id;
                prompt.tool_result_is_error = false;
                prompts.push(prompt);
            }
            other => bail!("unsupported message role for Devin: {other:?}"),
        }
    }
    Ok((system.join("\n\n"), prompts))
}

/// The router scores only the latest user/developer turn, sent with an empty message id.
fn router_prompt(messages: &[Value]) -> Result<Option<Prompt>> {
    for msg in messages.iter().rev() {
        if matches!(msg["role"].as_str(), Some("user" | "developer")) {
            return user_prompt(&msg["content"], String::new()).map(Some);
        }
    }
    Ok(None)
}

fn user_prompt(content: &Value, message_id: String) -> Result<Prompt> {
    let mut prompt = Prompt {
        message_id,
        source: SOURCE_USER,
        ..Prompt::default()
    };
    match content {
        Value::String(text) => prompt.prompt.push_str(text),
        Value::Array(parts) => {
            for part in parts {
                match part["type"].as_str() {
                    Some("text") => prompt.prompt.push_str(part["text"].as_str().unwrap_or("")),
                    Some("image_url") => prompt.images.push(image(part)?),
                    _ => {}
                }
            }
        }
        Value::Null => {}
        _ => bail!("unsupported Devin message content"),
    }
    Ok(prompt)
}

fn image(part: &Value) -> Result<(String, String)> {
    let url = part["image_url"]["url"]
        .as_str()
        .context("Devin image has no URL")?;
    let rest = url
        .strip_prefix("data:")
        .context("Devin images must be base64 data URLs")?;
    let (mime, data) = rest
        .split_once(";base64,")
        .context("Devin image data URL must be base64")?;
    Ok((mime.to_owned(), data.to_owned()))
}

/// Concatenated text parts of a message's `content` (string or array form).
fn text_of(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|part| part["type"] == "text")
            .filter_map(|part| part["text"].as_str())
            .collect(),
        _ => String::new(),
    }
}

fn carrier<'a>(msg: &'a Value, scope: &str) -> Option<&'a Value> {
    msg["dial_provider_content"]
        .as_array()?
        .iter()
        .find(|item| item["type"] == CARRIER_TYPE && item["scope"].as_str() == Some(scope))
}

/// Stream decoder state for one `GetChatMessage` response.
#[derive(Default)]
struct Stream {
    message_id: String,
    ended: bool,
    decoded_bytes: usize,
    text: String,
    thinking: String,
    signature: String,
    tools: Vec<ToolAccumulator>,
    /// Id of the tool call that later deltas without an id belong to.
    active: Option<String>,
    stop_reason: u64,
    usage: Option<Usage>,
    /// Set when the trailer is an opaque `invalid_argument` "internal error" before any output.
    rejected_internal: bool,
    /// Credit metering from the frames (`creditCost`, `committedCreditCost`, `committedAcuCost`).
    credits: Option<super::ReplyCredits>,
}

#[derive(Default)]
struct Usage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

struct ToolAccumulator {
    id: String,
    name: String,
    arguments: String,
}

enum Emit {
    Text(String),
    Event(StreamEvent),
}

impl Stream {
    /// Whether any text, reasoning, or tool-call output has streamed yet.
    fn has_output(&self) -> bool {
        !(self.text.is_empty() && self.thinking.is_empty() && self.tools.is_empty())
    }

    fn feed(&mut self, flags: u8, payload: &[u8]) -> Result<Vec<Emit>> {
        ensure!(!self.ended, "Devin frame received after completion trailer");
        let raw: Cow<'_, [u8]> = if flags & FLAG_COMPRESSED != 0 {
            Cow::Owned(gunzip(payload)?)
        } else {
            Cow::Borrowed(payload)
        };
        super::connect::account_payload(&mut self.decoded_bytes, raw.len())?;
        if flags & FLAG_END_STREAM != 0 {
            let terminal: Value =
                serde_json::from_slice(&raw).context("invalid Devin completion trailer")?;
            ensure!(terminal.is_object(), "invalid Devin completion trailer");
            let trailer = String::from_utf8_lossy(&raw);
            if let Some(failure) = trailer_error(trailer.trim()) {
                // A pre-output `invalid_argument` "internal error" may be large-history overflow.
                self.rejected_internal = !self.has_output()
                    && failure.code.eq_ignore_ascii_case("invalid_argument")
                    && INTERNAL_ERROR.is_match(&failure.message);
                // Connect codes that mean "try again" must reach retry as typed transients.
                let class = match failure.code.as_str() {
                    "unavailable" | "deadline_exceeded" | "aborted" => Some(ErrorClass::Transient),
                    "resource_exhausted" => Some(ErrorClass::RateLimited { retry_after: None }),
                    _ => None,
                };
                if let Some(class) = class {
                    return Err(Classified::new(class, failure.formatted).into());
                }
                bail!("{}", failure.formatted);
            }
            ensure!(
                terminal.get("error").is_none(),
                "invalid Devin error trailer"
            );
            self.ended = true;
            return Ok(Vec::new());
        }
        let message = Fields::parse(&raw)?;
        let mut out = Vec::new();
        let id = message.string(1);
        if self.message_id.is_empty() && !id.is_empty() {
            self.message_id = id;
        }
        let thinking = message.string(9);
        if !thinking.is_empty() {
            self.thinking.push_str(&thinking);
            out.push(Emit::Event(StreamEvent::Reasoning(thinking)));
        }
        let signature = message.string(10);
        if !signature.is_empty() {
            self.signature = signature;
        }
        let delta = message.string(3);
        if !delta.is_empty() {
            self.text.push_str(&delta);
            out.push(Emit::Text(delta));
        }
        for call in message.messages(6)? {
            let call_id = call.string(1);
            let id = if call_id.is_empty() {
                let Some(active) = self.active.clone() else {
                    continue;
                };
                active
            } else {
                call_id
            };
            self.active = Some(id.clone());
            let index = match self.tools.iter().position(|tool| tool.id == id) {
                Some(index) => index,
                None => {
                    self.tools.push(ToolAccumulator {
                        id: id.clone(),
                        name: String::new(),
                        arguments: String::new(),
                    });
                    self.tools.len() - 1
                }
            };
            let name = call.string(2);
            let tool = &mut self.tools[index];
            if !name.is_empty() {
                tool.name = name;
            }
            let fragment = call.string(3);
            if fragment.is_empty() {
                continue;
            }
            let previous_len = tool.arguments.len();
            let next = if fragment.starts_with(tool.arguments.as_str()) {
                fragment
            } else {
                format!("{}{fragment}", tool.arguments)
            };
            let delta = next[previous_len..].to_owned();
            tool.arguments = next;
            if !delta.is_empty() {
                out.push(Emit::Event(StreamEvent::ToolArguments {
                    index: index as u64,
                    id: tool.id.clone(),
                    name: tool.name.clone(),
                    delta,
                }));
            }
        }
        let stop = message.uint(5);
        if stop != 0 {
            self.stop_reason = stop;
        }
        if let Some(usage) = message.message(7)? {
            self.usage = Some(Usage {
                input: usage.uint(2),
                output: usage.uint(3),
                cache_write: usage.uint(4),
                cache_read: usage.uint(5),
            });
            super::usage::capture(super::usage::Snapshot {
                input: usage.uint_opt(2),
                output: usage.uint_opt(3),
                cache_write: usage.uint_opt(4),
                cache_read: usage.uint_opt(5),
                ..Default::default()
            });
        }
        // Each non-zero credit field is recorded; `usage.credits` exists once any is set.
        let cost = message.uint(14) as u32 as i32;
        let committed = message.uint(18) as u32 as i32;
        let acu = message.fixed64_f64(22);
        if cost != 0 || committed != 0 || acu != 0.0 {
            let credits = self.credits.get_or_insert_with(Default::default);
            if cost != 0 {
                credits.cost = Some(i64::from(cost));
            }
            if committed != 0 {
                credits.committed_cost = Some(i64::from(committed));
            }
            if acu != 0.0 {
                credits.acu_cost = Some(acu);
            }
        }
        Ok(out)
    }

    fn finish(self, scope: &str) -> Reply {
        let mut tool_calls = Vec::with_capacity(self.tools.len());
        for tool in self.tools {
            let (arguments, malformed_arguments) = parse_arguments(&tool.arguments);
            tool_calls.push(ToolCall {
                id: tool.id,
                name: tool.name,
                arguments,
                malformed_arguments,
            });
        }
        let finish_reason = if !tool_calls.is_empty() {
            "tool_calls"
        } else if self.stop_reason == STOP_REASON_MAX_TOKENS {
            "length"
        } else {
            "stop"
        };
        let (prompt_tokens, completion_tokens, cached_prompt_tokens, cache_creation_tokens) =
            match self.usage {
                Some(usage) => (
                    Some(usage.input),
                    Some(usage.output),
                    Some(usage.cache_read),
                    Some(usage.cache_write),
                ),
                None => (None, None, None, None),
            };
        let provider_content = if self.thinking.is_empty() && self.signature.is_empty() {
            Vec::new()
        } else {
            vec![json!({
                "type": CARRIER_TYPE,
                "scope": scope,
                "message_id": self.message_id,
                "thinking": self.thinking,
                "signature": self.signature,
            })]
        };
        Reply {
            text: self.text,
            tool_calls,
            finish_reason: finish_reason.to_owned(),
            prompt_tokens,
            completion_tokens,
            reasoning: self.thinking,
            cached_prompt_tokens,
            cache_creation_tokens,
            provider_content,
            credits: self.credits,
            ..Reply::default()
        }
    }
}

/// Tool arguments must be a JSON object. Empty means no arguments; anything else is malformed.
fn parse_arguments(text: &str) -> (Value, bool) {
    if text.trim().is_empty() {
        return (json!({}), false);
    }
    match serde_json::from_str::<Value>(text) {
        Ok(value @ Value::Object(_)) => (value, false),
        _ => (Value::Null, true),
    }
}

/// Parsed Connect end-of-stream error: the formatted message plus the raw code and message.
struct TrailerError {
    formatted: String,
    code: String,
    message: String,
}

/// Connect end-of-stream trailer with `{ "error": { code, message, details } }`, if present.
fn trailer_error(text: &str) -> Option<TrailerError> {
    if text.is_empty() {
        return None;
    }
    let parsed: Value = serde_json::from_str(text).ok()?;
    let error = parsed.get("error")?.as_object()?;
    let code = error.get("code").and_then(Value::as_str).unwrap_or("");
    let message = error.get("message").and_then(Value::as_str).unwrap_or("");
    if code.is_empty() && message.is_empty() {
        return None;
    }
    let mut out = String::from("Devin stream error");
    if !code.is_empty() {
        out.push(' ');
        out.push_str(code);
    }
    out.push_str(": ");
    out.push_str(message);
    if let Some(details) = error.get("details").and_then(summarize_details) {
        out.push_str(" [details: ");
        out.push_str(&details);
        out.push(']');
    }
    Some(TrailerError {
        formatted: out,
        code: code.to_owned(),
        message: message.to_owned(),
    })
}

fn summarize_details(details: &Value) -> Option<String> {
    let mut parts = Vec::new();
    for entry in details.as_array()? {
        let Some(record) = entry.as_object() else {
            continue;
        };
        let kind = record
            .get("type")
            .and_then(Value::as_str)
            .filter(|kind| !kind.is_empty());
        let evidence = match record.get("debug") {
            Some(Value::String(debug)) => Some(debug.clone()),
            Some(other) => Some(other.to_string()),
            None => record
                .get("value")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
        };
        let part = match (kind, evidence) {
            (Some(kind), Some(evidence)) => format!("{kind}: {evidence}"),
            (Some(kind), None) => kind.to_owned(),
            (None, Some(evidence)) => evidence,
            (None, None) => continue,
        };
        parts.push(truncate(&part));
    }
    (!parts.is_empty()).then(|| truncate(&parts.join("; ")))
}

fn truncate(text: &str) -> String {
    text.chars().take(TRAILER_EVIDENCE_CHARS).collect()
}

fn random_uuid() -> String {
    let mut bytes: [u8; 16] = rand::random();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid_from_hex(&hex(&bytes))
}

/// Stable id from a seed: the first 128 bits of SHA-256, formatted.
fn deterministic_uuid(seed: &str) -> String {
    let digest = Sha256::digest(seed.as_bytes());
    uuid_from_hex(&hex(&digest[..16]))
}

fn uuid_from_hex(hex: &str) -> String {
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Trailing user/developer messages that form the active turn.
fn active_tail(messages: &[Value]) -> usize {
    let is_turn = |m: &&Value| matches!(m["role"].as_str(), Some("user" | "developer"));
    match messages.last().map(|m| m["role"].as_str()) {
        Some(Some("user")) => 1,
        Some(Some("developer")) => {
            1 + messages[..messages.len() - 1]
                .iter()
                .rev()
                .take_while(is_turn)
                .count()
        }
        _ => 0,
    }
}

/// An opaque `invalid_argument` "internal error" before any output, on repeated
/// history of at least `LARGE_HISTORY_RECOVERY_BYTES`, is reported as context overflow so
/// compaction runs. Only the history prompts count: the system prompt and tool schemas are not
/// compactable.
fn large_history_overflow(
    error: anyhow::Error,
    messages: &[Value],
    cascade: &str,
    scope: &str,
) -> anyhow::Error {
    let Ok((_, prompts)) = build_prompts(messages, cascade, scope) else {
        return error;
    };
    let tail = active_tail(messages).min(prompts.len());
    let mut history = Enc::default();
    for prompt in &prompts[..prompts.len() - tail] {
        history.message(3, &prompt.encode());
    }
    if history.0.len() < LARGE_HISTORY_RECOVERY_BYTES {
        return error;
    }
    anyhow::Error::new(Classified::new(
        ErrorClass::ContextOverflow,
        format!("{error:#}"),
    ))
}

#[cfg(test)]
mod tests;

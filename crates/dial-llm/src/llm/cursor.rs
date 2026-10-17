//! Cursor native transport: Connect framing with hand-encoded protobuf for Cursor's agent protocol.
//!
//! The server streams `AgentServerMessage` frames over `RunSSE`. Dial sends the run request and
//! every client reply as unary `BidiAppend` calls from a dedicated uplink thread, which also sends
//! heartbeats while the stream is open. MCP tool calls are handed to Dial's own tool loop: the
//! model's call becomes a `ToolCall`, and the server receives a handoff result so the turn can end.
//! Cursor-side shell, file, and search tools and hosted approvals are not served.

use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::{Map, Value, json};

use super::Patience;
use super::connect::{Enc, FLAG_COMPRESSED, FLAG_END_STREAM, Fields, Frames, frame, gunzip};
use super::errors::{Classified, ErrorClass};
use super::oauth::util::uuid_v4;
use super::profiles::ModelProfile;
use super::transports::{OnText, Route};
use super::{Chat, Interrupted, Reply, StreamEvent, ToolCall};

const RUN_SSE_PATH: &str = "/agent.v1.AgentService/RunSSE";
const BIDI_APPEND_PATH: &str = "/aiserver.v1.BidiService/BidiAppend";
const CLIENT_VERSION: &str = "cli-2026.09.02-c22c1a3";
const PROVIDER_IDENTIFIER: &str = "dial";
const APPEND_BASE_TIMEOUT_SECS: u64 = 60;
const APPEND_BYTES_PER_SECOND: usize = 128 * 1024;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const UPLINK_POLL: Duration = Duration::from_millis(200);
const HANDOFF_TEXT: &str = "Dial received this tool call and will run it on the client. Do not call this tool again in this turn. End the turn now and wait for the tool result.";
const UNSERVED_EXEC: &str =
    "Dial does not serve this Cursor-side tool; use Dial's own tools instead.";
const UNAPPROVED_REQUEST: &str =
    "Dial does not approve Cursor-hosted requests; continue without it.";
const RESERVED_HEADERS: [&str; 10] = [
    "authorization",
    "content-type",
    "connect-protocol-version",
    "x-request-id",
    "x-cursor-client-version",
    "x-cursor-client-type",
    "x-ghost-mode",
    "x-cursor-streaming",
    "host",
    "content-length",
];

/// `AgentClientMessage` field numbers.
const CLIENT_RUN_REQUEST: u32 = 1;
const CLIENT_EXEC: u32 = 2;
const CLIENT_KV: u32 = 3;
const CLIENT_CONTROL: u32 = 5;
const CLIENT_HEARTBEAT: u32 = 7;
const CLIENT_INTERACTION_RESPONSE: u32 = 6;

/// `AgentServerMessage` field numbers.
const SERVER_INTERACTION_UPDATE: u32 = 1;
const SERVER_EXEC: u32 = 2;
const SERVER_KV: u32 = 4;
const SERVER_INTERACTION_QUERY: u32 = 7;

/// `InteractionQuery` oneof field numbers that Dial answers with a rejection.
const QUERY_APPROVAL_FIELDS: [u32; 5] = [2, 4, 5, 6, 9];
const QUERY_ASK_QUESTION: u32 = 3;
const QUERY_CREATE_PLAN: u32 = 7;
const QUERY_SETUP_VM: u32 = 8;

/// Sends one `AgentClientMessage` (raw protobuf bytes) through the uplink.
type SendFn<'a> = dyn Fn(Vec<u8>) -> Result<()> + 'a;

pub(super) fn exchange(
    route: &Route<'_>,
    req: &Chat,
    timeout: u64,
    on_text: OnText<'_, '_>,
    interrupt: Arc<AtomicBool>,
    _profile: Option<&ModelProfile>,
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
) -> Result<Reply> {
    let token = route
        .auth
        .filter(|token| !token.is_empty())
        .context("Cursor requires sign-in; run /providers and sign in to Cursor")?;
    let request_id = uuid_v4();
    let conversation_id = route
        .session_id
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(uuid_v4);
    let run = run_request(req, route.model, &conversation_id)?;
    let tools = tool_definitions(&req.tools)?;
    let base = route.base_url.trim_end_matches('/');

    let mut headers = vec![
        ("authorization".to_owned(), format!("Bearer {token}")),
        ("x-cursor-client-type".to_owned(), "cli".to_owned()),
        (
            "x-cursor-client-version".to_owned(),
            CLIENT_VERSION.to_owned(),
        ),
        ("x-ghost-mode".to_owned(), "true".to_owned()),
        ("connect-protocol-version".to_owned(), "1".to_owned()),
        ("x-request-id".to_owned(), request_id.clone()),
        ("x-cursor-streaming".to_owned(), "true".to_owned()),
    ];
    headers.extend(
        route
            .headers
            .iter()
            .filter(|(name, _)| {
                !RESERVED_HEADERS
                    .iter()
                    .any(|r| name.eq_ignore_ascii_case(r))
            })
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
    );

    let uplink = Uplink::start(UplinkCtx {
        url: format!("{base}{BIDI_APPEND_PATH}"),
        token: token.to_owned(),
        headers: headers.clone(),
        request_id: request_id.clone(),
        interrupt: Arc::clone(&interrupt),
        stop: Arc::new(AtomicBool::new(false)),
        failure: Arc::new(Mutex::new(None)),
    })?;
    uplink.send(run)?;

    let mut id = Enc(Vec::new());
    id.string(1, &request_id);
    let body = frame(0, &id.0)?;
    let url = format!("{base}{RUN_SSE_PATH}");
    headers.push((
        "content-type".to_owned(),
        "application/connect+proto".to_owned(),
    ));
    let refs = as_refs(&headers);
    let local = super::host_of(route.base_url).is_some_and(|host| super::is_local_host(&host));
    let patience = Patience::for_stream(timeout, local);
    let mut response = super::send_prepared_with_headers(
        &url,
        None,
        &body,
        timeout,
        Some(patience),
        Some(Arc::clone(&interrupt)),
        &refs,
    )?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let hints = super::retry::server_delay(response.headers());
        let bounded =
            super::read_bounded(response.body_mut().as_reader(), super::ERROR_EXCERPT_BYTES)
                .context("reading Cursor error response")?;
        return Err(http_failure(status, &bounded, route.auth, hints));
    }

    let send = |bytes: Vec<u8>| uplink.send(bytes);
    let mut session = Session::new(&send, tools);
    let mut reader = response.body_mut().as_reader();
    let mut frames = Frames::default();
    let mut chunk = vec![0u8; 16 * 1024];
    let mut total = 0usize;
    'read: loop {
        if interrupt.load(Ordering::SeqCst) {
            return Err(Interrupted.into());
        }
        if let Some(error) = uplink.take_failure() {
            return Err(error);
        }
        let read = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(super::response_error(error.into(), route.auth)),
        };
        total += read;
        ensure!(
            total <= super::MAX_STREAM_BYTES,
            "Cursor stream exceeded the {} MiB response limit",
            super::MAX_STREAM_BYTES / (1024 * 1024)
        );
        for (flags, payload) in frames.push(&chunk[..read])? {
            if flags & FLAG_END_STREAM != 0 {
                trailer_error(&payload)?;
                break 'read;
            }
            let payload = if flags & FLAG_COMPRESSED != 0 {
                gunzip(&payload)?
            } else {
                payload
            };
            for emit in session.feed(&payload)? {
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
    if !session.turn_ended && session.tool_calls.is_empty() {
        return Err(Classified::new(
            ErrorClass::Transient,
            "Cursor stream ended before the turn finished",
        )
        .into());
    }
    Ok(session.finish())
}

fn as_refs(headers: &[(String, String)]) -> Vec<(&str, &str)> {
    headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect()
}

/// Wraps a single `AgentClientMessage` field around an encoded payload.
fn client_message(field: u32, inner: &Enc) -> Vec<u8> {
    let mut out = Enc(Vec::new());
    out.message(field, inner);
    out.0
}

fn heartbeat_message() -> Vec<u8> {
    client_message(CLIENT_HEARTBEAT, &Enc(Vec::new()))
}

fn bidi_append_body(request_id: &str, seqno: u64, data: &[u8]) -> Vec<u8> {
    let mut id = Enc(Vec::new());
    id.string(1, request_id);
    let mut body = Enc(Vec::new());
    body.message(2, &id);
    body.uint(3, seqno);
    body.bytes(4, data);
    body.0
}

struct UplinkCtx {
    url: String,
    token: String,
    headers: Vec<(String, String)>,
    request_id: String,
    interrupt: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<anyhow::Error>>>,
}

/// Owns the thread that posts client messages in order and keeps the stream alive.
struct Uplink {
    tx: mpsc::Sender<Vec<u8>>,
    ctx_failure: Arc<Mutex<Option<anyhow::Error>>>,
    stop: Arc<AtomicBool>,
}

impl Uplink {
    fn start(ctx: UplinkCtx) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let ctx_failure = Arc::clone(&ctx.failure);
        let stop = Arc::clone(&ctx.stop);
        thread::Builder::new()
            .name("dial-cursor-uplink".to_owned())
            .spawn(move || uplink_loop(&rx, &ctx))
            .context("starting Cursor uplink thread")?;
        Ok(Self {
            tx,
            ctx_failure,
            stop,
        })
    }

    fn send(&self, bytes: Vec<u8>) -> Result<()> {
        if self.tx.send(bytes).is_err() {
            return Err(self
                .take_failure()
                .unwrap_or_else(|| anyhow!("Cursor uplink closed unexpectedly")));
        }
        Ok(())
    }

    fn take_failure(&self) -> Option<anyhow::Error> {
        self.ctx_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

impl Drop for Uplink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn uplink_loop(rx: &mpsc::Receiver<Vec<u8>>, ctx: &UplinkCtx) {
    let halted = || ctx.interrupt.load(Ordering::SeqCst) || ctx.stop.load(Ordering::SeqCst);
    let mut seqno = 0u64;
    let mut last_heartbeat = Instant::now();
    while !halted() {
        match rx.recv_timeout(UPLINK_POLL) {
            Ok(message) => {
                if let Err(error) = append(ctx, seqno, &message) {
                    fail(ctx, error);
                    return;
                }
                seqno += 1;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            if let Err(error) = append(ctx, seqno, &heartbeat_message()) {
                fail(ctx, error);
                return;
            }
            seqno += 1;
            last_heartbeat = Instant::now();
        }
    }
}

fn fail(ctx: &UplinkCtx, error: anyhow::Error) {
    ctx.stop.store(true, Ordering::SeqCst);
    *ctx.failure
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error);
}

fn append(ctx: &UplinkCtx, seqno: u64, message: &[u8]) -> Result<()> {
    let body = bidi_append_body(&ctx.request_id, seqno, message);
    let timeout = APPEND_BASE_TIMEOUT_SECS + message.len().div_ceil(APPEND_BYTES_PER_SECOND) as u64;
    let mut headers = as_refs(&ctx.headers);
    headers.push(("content-type", "application/proto"));
    let mut response = super::send_prepared_with_headers(
        &ctx.url,
        None,
        &body,
        timeout,
        None,
        Some(Arc::clone(&ctx.interrupt)),
        &headers,
    )?;
    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(());
    }
    let hints = super::retry::server_delay(response.headers());
    let bounded = super::read_bounded(response.body_mut().as_reader(), super::ERROR_EXCERPT_BYTES)
        .context("reading Cursor BidiAppend response")?;
    Err(http_failure(status, &bounded, Some(&ctx.token), hints))
}

/// Maps a Connect error code to a Dial error class. `None` for codes Dial does not classify.
fn connect_failure(code: &str, message: &str) -> Option<Classified> {
    let class = match code {
        "unavailable" | "deadline_exceeded" | "aborted" => ErrorClass::Transient,
        "resource_exhausted" => ErrorClass::RateLimited { retry_after: None },
        "unauthenticated" | "permission_denied" => ErrorClass::Auth,
        _ => return None,
    };
    Some(Classified::new(class, format!("Cursor {code}: {message}")))
}

/// End-of-stream frame: empty on success, or `{"error":{"code","message"}}`.
fn trailer_error(payload: &[u8]) -> Result<()> {
    if payload.is_empty() {
        return Ok(());
    }
    let value: Value =
        serde_json::from_slice(payload).context("Cursor end-of-stream frame is not JSON")?;
    let Some(error) = value.get("error") else {
        return Ok(());
    };
    let code = error["code"].as_str().unwrap_or("unknown");
    let message = error["message"].as_str().unwrap_or("");
    match connect_failure(code, message) {
        Some(classified) => Err(classified.into()),
        None => bail!("Cursor {code}: {message}"),
    }
}

/// Non-2xx response: a Connect JSON error body is classified by its code; anything else is a
/// plain HTTP error.
fn http_failure(
    status: u16,
    bounded: &super::Bounded,
    auth: Option<&str>,
    hints: impl Into<super::retry::ServerHints>,
) -> anyhow::Error {
    if let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(&bounded.data)
        && let Some(code) = map.get("code").and_then(Value::as_str)
    {
        let message = map.get("message").and_then(Value::as_str).unwrap_or("");
        if let Some(classified) = connect_failure(code, message) {
            return classified.into();
        }
    }
    super::http_error(status, bounded, auth, hints)
}

fn text_of(content: &Value) -> Result<String> {
    match content {
        Value::Null => Ok(String::new()),
        Value::String(text) => Ok(text.clone()),
        Value::Array(parts) => {
            let mut text = String::new();
            for part in parts {
                match part["type"].as_str() {
                    Some("text") => {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(part["text"].as_str().unwrap_or(""));
                    }
                    other => bail!(
                        "Cursor accepts text input only; Dial received a {} part",
                        other.unwrap_or("non-text")
                    ),
                }
            }
            Ok(text)
        }
        _ => bail!("Cursor received message content in an unsupported shape"),
    }
}

/// Builds the `AgentClientMessage` that starts the turn.
///
/// Prior messages (including system and developer messages) travel as JSON root prompt messages.
/// A trailing user message becomes the turn's user action; a trailing tool result resumes the turn.
fn run_request(req: &Chat, model: &str, conversation_id: &str) -> Result<Vec<u8>> {
    let (last, history) = req
        .messages
        .split_last()
        .context("Cursor request has no messages")?;
    for message in &req.messages {
        text_of(&message["content"])?;
    }
    let role = last["role"]
        .as_str()
        .context("Cursor message has no role")?;
    let (action, root_messages) = match role {
        "user" => {
            let text = text_of(&last["content"])?;
            ensure!(
                !text.trim().is_empty(),
                "Cursor needs a non-empty user message"
            );
            let mut user = Enc(Vec::new());
            user.string(1, &text);
            user.string(2, &uuid_v4());
            let mut user_action = Enc(Vec::new());
            user_action.message(1, &user);
            let mut action = Enc(Vec::new());
            action.message(1, &user_action);
            (action, history)
        }
        "tool" => {
            let mut action = Enc(Vec::new());
            action.message(2, &Enc(Vec::new()));
            (action, req.messages.as_slice())
        }
        other => bail!(
            "Cursor cannot continue after a {other} message; send a user message or tool result"
        ),
    };
    let mut conversation = Enc(Vec::new());
    for message in root_messages {
        conversation.bytes(1, &serde_json::to_vec(message)?);
    }
    let mut details = Enc(Vec::new());
    details.string(1, model);
    details.string(3, model);
    details.string(4, model);
    let mut requested = Enc(Vec::new());
    requested.string(1, model);
    let mut run = Enc(Vec::new());
    run.message(1, &conversation);
    run.message(2, &action);
    run.message(3, &details);
    run.message(9, &requested);
    run.string(5, conversation_id);
    Ok(client_message(CLIENT_RUN_REQUEST, &run))
}

/// Encodes each declared tool as an MCP tool definition served through the handoff.
fn tool_definitions(tools: &[Value]) -> Result<Vec<Vec<u8>>> {
    let mut out = Vec::with_capacity(tools.len());
    for tool in tools {
        let function = &tool["function"];
        let name = function["name"].as_str().unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let schema = function
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"}));
        let mut def = Enc(Vec::new());
        def.string(1, name);
        def.string(2, function["description"].as_str().unwrap_or(""));
        def.bytes(3, &serde_json::to_vec(&schema)?);
        def.string(4, PROVIDER_IDENTIFIER);
        def.string(5, name);
        out.push(def.0);
    }
    Ok(out)
}

/// MCP argument values are JSON bytes; a string that itself holds a JSON object or array is parsed.
fn decode_arg(bytes: &[u8]) -> Value {
    let value = serde_json::from_slice::<Value>(bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(bytes).into_owned()));
    if let Value::String(text) = &value
        && matches!(text.trim_start().chars().next(), Some('{' | '['))
        && let Ok(nested) = serde_json::from_str::<Value>(text)
    {
        return nested;
    }
    value
}

fn mcp_call(args: &Fields) -> Result<ToolCall> {
    let name = args.string(1);
    let tool_name = args.string(5);
    let tool = if tool_name.is_empty() {
        name
    } else {
        tool_name
    };
    let id = args.string(3);
    let mut arguments = Map::new();
    for entry in args.messages(2)? {
        arguments.insert(entry.string(1), decode_arg(entry.bytes(2).unwrap_or(&[])));
    }
    Ok(ToolCall {
        id: if id.is_empty() { uuid_v4() } else { id },
        name: tool,
        arguments: Value::Object(arguments),
        malformed_arguments: false,
    })
}

enum Emit {
    Text(String),
    Event(StreamEvent),
}

#[derive(Default, Clone, Copy)]
struct Usage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    reasoning: u64,
}

/// Decodes server messages for one turn and produces the client replies.
struct Session<'s> {
    send: &'s SendFn<'s>,
    text: String,
    reasoning: String,
    tool_defs: Vec<Vec<u8>>,
    tool_calls: Vec<ToolCall>,
    blobs: HashMap<Vec<u8>, Vec<u8>>,
    usage: Option<Usage>,
    turn_ended: bool,
}

impl<'s> Session<'s> {
    fn new(send: &'s SendFn<'s>, tool_defs: Vec<Vec<u8>>) -> Self {
        Self {
            send,
            text: String::new(),
            reasoning: String::new(),
            tool_defs,
            tool_calls: Vec::new(),
            blobs: HashMap::new(),
            usage: None,
            turn_ended: false,
        }
    }

    fn feed(&mut self, payload: &[u8]) -> Result<Vec<Emit>> {
        let message = Fields::parse(payload)?;
        let mut out = Vec::new();
        if let Some(update) = message.message(SERVER_INTERACTION_UPDATE)? {
            self.interaction_update(&update, &mut out)?;
        } else if let Some(exec) = message.message(SERVER_EXEC)? {
            self.exec(&exec)?;
        } else if let Some(kv) = message.message(SERVER_KV)? {
            self.kv(&kv)?;
        } else if let Some(query) = message.message(SERVER_INTERACTION_QUERY)? {
            self.interaction_query(&query)?;
        }
        Ok(out)
    }

    fn interaction_update(&mut self, update: &Fields, out: &mut Vec<Emit>) -> Result<()> {
        if let Some(delta) = update.message(1)? {
            let text = delta.string(1);
            if !text.is_empty() {
                self.text.push_str(&text);
                out.push(Emit::Text(text));
            }
        }
        if let Some(delta) = update.message(4)? {
            let text = delta.string(1);
            if !text.is_empty() {
                self.reasoning.push_str(&text);
                out.push(Emit::Event(StreamEvent::Reasoning(text)));
            }
        }
        if let Some(ended) = update.message(14)? {
            self.turn_ended = true;
            self.usage = Some(Usage {
                input: ended.uint(1),
                output: ended.uint(2),
                cache_read: ended.uint(3),
                cache_write: ended.uint(4),
                reasoning: ended.uint(5),
            });
        }
        Ok(())
    }

    fn exec(&mut self, exec: &Fields) -> Result<()> {
        let id = exec.uint(1) as u32;
        let exec_id = exec.string(15);
        if exec.message(10)?.is_some() {
            let mut context = Enc(Vec::new());
            for tool in &self.tool_defs {
                context.bytes(7, tool);
            }
            let mut success = Enc(Vec::new());
            success.message(1, &context);
            let mut result = Enc(Vec::new());
            result.message(1, &success);
            return self.reply_exec(id, &exec_id, 10, &result);
        }
        if let Some(args) = exec.message(11)? {
            let call = mcp_call(&args)?;
            let mut text = Enc(Vec::new());
            text.string(1, HANDOFF_TEXT);
            let mut content = Enc(Vec::new());
            content.message(1, &text);
            let mut success = Enc(Vec::new());
            success.message(1, &content);
            let mut result = Enc(Vec::new());
            result.message(1, &success);
            self.tool_calls.push(call);
            return self.reply_exec(id, &exec_id, 11, &result);
        }
        self.reply_unserved(id)
    }

    fn reply_exec(&self, id: u32, exec_id: &str, field: u32, result: &Enc) -> Result<()> {
        let mut exec = Enc(Vec::new());
        exec.uint(1, u64::from(id));
        if !exec_id.is_empty() {
            exec.string(15, exec_id);
        }
        exec.message(field, result);
        (self.send)(client_message(CLIENT_EXEC, &exec))
    }

    /// Throws the exec back to the server with an explanation, then closes the stream.
    fn reply_unserved(&self, id: u32) -> Result<()> {
        let mut throw = Enc(Vec::new());
        throw.uint(1, u64::from(id));
        throw.string(2, UNSERVED_EXEC);
        throw.string(4, "exec_variant_unsupported");
        let mut control = Enc(Vec::new());
        control.message(2, &throw);
        (self.send)(client_message(CLIENT_CONTROL, &control))?;
        let mut close = Enc(Vec::new());
        close.uint(1, u64::from(id));
        let mut control = Enc(Vec::new());
        control.message(1, &close);
        (self.send)(client_message(CLIENT_CONTROL, &control))
    }

    fn kv(&mut self, kv: &Fields) -> Result<()> {
        let mut message = Enc(Vec::new());
        message.uint(1, kv.uint(1));
        if let Some(args) = kv.message(2)? {
            let key = args.bytes(1).unwrap_or(&[]).to_vec();
            let mut found = Enc(Vec::new());
            if let Some(data) = self.blobs.get(&key) {
                found.bytes(1, data);
            }
            message.message(2, &found);
        } else if let Some(args) = kv.message(3)? {
            let key = args.bytes(1).unwrap_or(&[]).to_vec();
            let data = args.bytes(2).unwrap_or(&[]).to_vec();
            self.blobs.insert(key, data);
            message.message(3, &Enc(Vec::new()));
        } else {
            return Ok(());
        }
        (self.send)(client_message(CLIENT_KV, &message))
    }

    /// Rejects Cursor-hosted prompts Dial does not approve. VM setup is left unanswered because
    /// Dial has no environment to offer and the protocol has no rejection shape for it.
    fn interaction_query(&self, query: &Fields) -> Result<()> {
        let id = query.uint(1);
        let mut field = None;
        for candidate in QUERY_APPROVAL_FIELDS.into_iter().chain([
            QUERY_ASK_QUESTION,
            QUERY_CREATE_PLAN,
            QUERY_SETUP_VM,
        ]) {
            if query.message(candidate)?.is_some() {
                field = Some(candidate);
                break;
            }
        }
        let body = match field {
            Some(QUERY_SETUP_VM) | None => return Ok(()),
            Some(QUERY_ASK_QUESTION) => {
                let mut rejected = Enc(Vec::new());
                rejected.string(1, UNAPPROVED_REQUEST);
                let mut result = Enc(Vec::new());
                result.message(3, &rejected);
                let mut body = Enc(Vec::new());
                body.message(1, &result);
                body
            }
            Some(QUERY_CREATE_PLAN) => {
                let mut error = Enc(Vec::new());
                error.string(1, UNAPPROVED_REQUEST);
                let mut result = Enc(Vec::new());
                result.message(2, &error);
                let mut body = Enc(Vec::new());
                body.message(1, &result);
                body
            }
            Some(_) => {
                let mut rejected = Enc(Vec::new());
                rejected.string(1, UNAPPROVED_REQUEST);
                let mut body = Enc(Vec::new());
                body.message(2, &rejected);
                body
            }
        };
        let Some(field) = field else { return Ok(()) };
        let mut response = Enc(Vec::new());
        response.uint(1, id);
        response.message(field, &body);
        (self.send)(client_message(CLIENT_INTERACTION_RESPONSE, &response))
    }

    fn finish(self) -> Reply {
        let finish_reason = if self.tool_calls.is_empty() {
            "stop"
        } else {
            "tool_calls"
        };
        let usage = self.usage;
        Reply {
            text: self.text,
            tool_calls: self.tool_calls,
            finish_reason: finish_reason.to_owned(),
            prompt_tokens: usage.map(|u| u.input),
            completion_tokens: usage.map(|u| u.output),
            reasoning: self.reasoning,
            reasoning_tokens: usage.map(|u| u.reasoning),
            cached_prompt_tokens: usage.map(|u| u.cache_read),
            cache_creation_tokens: usage.map(|u| u.cache_write),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests;

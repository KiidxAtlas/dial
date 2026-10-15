//! OpenAI Codex Responses over its WebSocket transport.
//!
//! One socket per session/account/model is reused across turns while it has been idle for at most
//! `MAX_IDLE_REUSE`; a reused socket that fails before any event is replaced and the request
//! replayed in full. A request whose input extends the
//! previous request plus that response's native output items chains with `previous_response_id`
//! and sends only the delta; any mismatch replays the full transcript. Live steering is submitted as
//! `response.steer` and governed by explicit receipts. Steering the server accepted for the final
//! response stays on the socket that accepted it; the next request either reads the server's
//! automatic successor (attach), sends only pending tool output (create), or drops the socket and
//! replays in full (discard).
use super::{Chat, Interrupted, Reply, StreamEvent};
use anyhow::{Context, Result, bail, ensure};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::ErrorKind,
    net::{TcpStream, ToSocketAddrs},
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, SyncSender, TrySendError},
    },
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket, client::IntoClientRequest, stream::MaybeTlsStream};

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;
/// Steering commands queued for one active response.
const STEER_QUEUE: usize = 8;
/// Longest quiet period a cached socket is trusted for. The backend evicts idle sockets without
/// a close frame; reusing one fails on first use (OMP: `CODEX_WEBSOCKET_MAX_IDLE_REUSE_MS`).
const MAX_IDLE_REUSE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteerOutcome {
    Accepted { id: String },
    Rejected,
    Uncertain,
}
/// Receipt for one submitted steering message. A dropped sender (socket gone) is `Uncertain`.
pub struct SteerTicket(Receiver<SteerOutcome>);
impl SteerTicket {
    pub fn wait_until(&self, deadline: Instant) -> SteerOutcome {
        self.0
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(SteerOutcome::Uncertain)
    }
    /// Non-blocking; `None` while the receipt is still outstanding.
    pub fn try_outcome(&self) -> Option<SteerOutcome> {
        match self.0.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(SteerOutcome::Uncertain),
        }
    }
}
struct SteerCommand {
    input: Value,
    outcome: SyncSender<SteerOutcome>,
}
static LIVE: LazyLock<Mutex<HashMap<String, SyncSender<SteerCommand>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Socket plus the append baseline the server holds for it.
struct Session {
    socket: Socket,
    /// Full (unchained) body of the last completed request, without `type`/`previous_response_id`.
    last_request: Option<Value>,
    last_response_id: Option<String>,
    last_response_items: Vec<Value>,
    can_append: bool,
    /// Steering accepted on this socket for `last_response_id`: (steer id, submitted items).
    accepted: Vec<(String, Vec<Value>)>,
    /// When the socket last finished a response (or connected).
    idle_since: Instant,
}
impl Session {
    fn new(socket: Socket) -> Self {
        Self {
            idle_since: Instant::now(),
            socket,
            last_request: None,
            last_response_id: None,
            last_response_items: Vec::new(),
            can_append: false,
            accepted: Vec::new(),
        }
    }
    fn reset_append(&mut self) {
        self.can_append = false;
        self.last_request = None;
        self.last_response_id = None;
        self.last_response_items.clear();
    }
}
static SESSIONS: LazyLock<Mutex<HashMap<String, Session>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Socket keys whose server refused the WebSocket upgrade (404, 405, 426) or ended a response
/// before completion. Like a session's `disableWebsocket`, they use SSE on later attempts.
/// Other failures (including interruption, fatal events and steering errors) do not disable WS.
static SSE_ONLY: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Drops the cached sockets for one bound session so the next request starts unchained.
/// Keys are `{session_key}\0{account}\0{model}`; the prefix match is exact on the NUL boundary.
pub fn forget(session_key: &str) {
    let prefix = format!("{session_key}\0");
    let removed: Vec<Session> = {
        let mut sessions = SESSIONS.lock();
        let keys: Vec<String> = sessions
            .keys()
            .filter(|key| key.starts_with(&prefix))
            .cloned()
            .collect();
        keys.iter().filter_map(|key| sessions.remove(key)).collect()
    };
    for mut session in removed {
        let _ = session.socket.close(None);
    }
}

/// Submits exact prepared input for an active response. `None` means local replay is required.
pub fn submit(session_key: &str, input: Value) -> Result<Option<SteerTicket>> {
    ensure!(
        steer_items(&input).is_some(),
        "only plain user messages can steer a Codex response"
    );
    let Some(tx) = LIVE.lock().get(session_key).cloned() else {
        return Ok(None);
    };
    let (outcome, receiver) = std::sync::mpsc::sync_channel(1);
    match tx.try_send(SteerCommand { input, outcome }) {
        Ok(()) => Ok(Some(SteerTicket(receiver))),
        // Full queue or finished response: the caller replays locally; nothing was submitted.
        Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => Ok(None),
    }
}

/// Only user message items may steer.
fn steer_items(input: &Value) -> Option<&Vec<Value>> {
    let items = input.as_array().filter(|items| !items.is_empty())?;
    items
        .iter()
        .all(|item| item["role"] == "user" && (item["type"].is_null() || item["type"] == "message"))
        .then_some(items)
}

/// Deep equality ignoring `exclude` keys at the top level of objects.
fn equal_without(a: &Value, b: &Value, exclude: &[&str]) -> bool {
    match (a.as_object(), b.as_object()) {
        (Some(a), Some(b)) => {
            let keys = |m: &Map<String, Value>| {
                m.keys()
                    .filter(|k| !exclude.contains(&k.as_str()))
                    .cloned()
                    .collect::<HashSet<_>>()
            };
            keys(a) == keys(b) && keys(a).iter().all(|k| a[k] == b[k])
        }
        _ => a == b,
    }
}

/// Items appended beyond previous input + response output, or
/// `None` when options differ or history mutated.
fn delta_input(previous: &Value, response_items: &[Value], current: &Value) -> Option<Vec<Value>> {
    const TOP: &[&str] = &[
        "input",
        "client_metadata",
        "service_tier",
        "previous_response_id",
        "type",
    ];
    let (prior, input) = (previous["input"].as_array()?, current["input"].as_array()?);
    if !equal_without(previous, current, TOP) {
        return None;
    }
    let baseline = prior.len() + response_items.len();
    if input.len() <= baseline {
        return None;
    }
    for (item, candidate) in prior.iter().chain(response_items).zip(input) {
        let omit: &[&str] = if matches!(
            item["type"].as_str(),
            Some("message" | "function_call" | "custom_tool_call")
        ) {
            &["status", "id"]
        } else {
            &["status"]
        };
        if !equal_without(item, candidate, omit) {
            return None;
        }
    }
    Some(input[baseline..].to_vec())
}

enum Plan {
    Attach,
    Create(Vec<Value>),
    Discard,
}
/// Identity of a steering item across submission and replay.
fn steer_key(item: &Value) -> String {
    if item["role"] != "user" {
        return format!(
            "\0{}:{}",
            item["type"].as_str().unwrap_or(""),
            item["call_id"]
                .as_str()
                .or(item["id"].as_str())
                .unwrap_or("")
        );
    }
    let content = match &item["content"] {
        Value::String(text) => json!([{"type":"input_text","text":text}]),
        Value::Array(parts) => Value::Array(
            parts
                .iter()
                .map(|part| {
                    let mut part = part.clone();
                    if let Some(map) = part.as_object_mut() {
                        map.remove("detail");
                    }
                    part
                })
                .collect(),
        ),
        other => other.clone(),
    };
    content.to_string()
}
/// Chooses how a steered request is sent.
fn plan_steered(delta: Option<&[Value]>, steering: &[Value]) -> Plan {
    let Some(delta) = delta else {
        return Plan::Discard;
    };
    let mut next = 0;
    let mut rest = Vec::new();
    for item in delta {
        if next < steering.len() && steer_key(item) == steer_key(&steering[next]) {
            next += 1;
        } else {
            rest.push(item.clone());
        }
    }
    if next < steering.len() {
        return Plan::Discard;
    }
    if rest.is_empty() {
        return Plan::Attach;
    }
    if rest.iter().any(|item| {
        item["type"]
            .as_str()
            .is_some_and(|t| t.ends_with("_output"))
    }) {
        Plan::Create(rest)
    } else {
        Plan::Discard
    }
}

/// `None` selects HTTP streaming before any response callbacks have run. An incomplete WS
/// response remains an error so the caller can discard partial callbacks before retrying via SSE.
#[allow(clippy::too_many_arguments)]
pub(super) fn exchange(
    endpoint: super::responses::Endpoint<'_>,
    chat: &Chat,
    timeout: u64,
    on_text: &mut Option<&mut dyn FnMut(&str) -> bool>,
    interrupt: Arc<AtomicBool>,
    profile: Option<&super::profiles::ModelProfile>,
    cache_key: Option<&str>,
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
    session_key: &str,
) -> Result<Option<Reply>> {
    let replay_scope = super::responses::validate_history(endpoint, chat)?;
    let mut body = super::responses::request_codex(endpoint.model, chat, profile)?;
    if super::responses::sends_prompt_cache_key(profile, true)
        && let Some(key) = cache_key
    {
        body["prompt_cache_key"] = json!(key);
    }
    let socket_key = format!(
        "{session_key}\0{}\0{}",
        endpoint.account_id.unwrap_or(""),
        endpoint.model
    );
    let cached = match SESSIONS.lock().remove(&socket_key) {
        Some(mut idle) if idle.idle_since.elapsed() > MAX_IDLE_REUSE => {
            // Replay in full on a fresh socket; any accepted steering is carried as history.
            let _ = idle.socket.close(None);
            None
        }
        cached => cached,
    };
    let reused = cached.is_some();
    let mut session = match cached {
        Some(session) => session,
        None if SSE_ONLY.lock().contains(&socket_key) => return Ok(None),
        // A failed handshake can select SSE immediately: no response data has been delivered.
        None => match connect(endpoint, session_key, timeout) {
            Ok(socket) => Session::new(socket),
            Err(error) => {
                if let Some(tungstenite::Error::Http(response)) =
                    error.downcast_ref::<tungstenite::Error>()
                    && matches!(response.status().as_u16(), 200 | 404 | 405 | 426)
                {
                    SSE_ONLY.lock().insert(socket_key);
                }
                return Ok(None);
            }
        },
    };
    // Chain over the reused socket when the transcript strictly extends the append baseline.
    let chained_delta = if session.can_append
        && session.last_request.as_ref().is_some_and(|last| {
            (last["service_tier"] == "ultrafast") == (body["service_tier"] == "ultrafast")
        }) {
        let delta = session
            .last_request
            .as_ref()
            .and_then(|last| delta_input(last, &session.last_response_items, &body))
            .filter(|d| !d.is_empty());
        if delta.is_none() {
            session.reset_append();
        }
        delta
    } else {
        None
    };
    let accepted = std::mem::take(&mut session.accepted);
    let mut outgoing = None::<Value>;
    let mut attach = HashSet::<String>::new();
    if !accepted.is_empty() {
        let steering: Vec<Value> = accepted
            .iter()
            .flat_map(|(_, items)| items.iter().cloned())
            .collect();
        match plan_steered(chained_delta.as_deref(), &steering) {
            Plan::Attach => attach = accepted.into_iter().map(|(id, _)| id).collect(),
            Plan::Create(input) => {
                outgoing = Some(chained(&body, session.last_response_id.as_deref(), input))
            }
            Plan::Discard => {
                // The server continues from input this request cannot line up with: a fresh
                // socket drops that queue and the full transcript carries the steering.
                let _ = session.socket.close(None);
                session = match connect(endpoint, session_key, timeout) {
                    Ok(socket) => Session::new(socket),
                    Err(_) => {
                        return Ok(None);
                    }
                };
                outgoing = Some(body.clone());
            }
        }
    } else {
        outgoing = Some(match (chained_delta, session.last_response_id.as_deref()) {
            (Some(delta), Some(id)) => chained(&body, Some(id), delta),
            _ => body.clone(),
        });
    }
    set_poll_timeout(&mut session.socket);
    let (tx, rx) = std::sync::mpsc::sync_channel(STEER_QUEUE);
    LIVE.lock().insert(session_key.to_owned(), tx);
    let mut run = Run {
        pending: VecDeque::new(),
        accepted: Vec::new(),
        tainted: false,
        response_id: None,
    };
    let mut result = stream(
        &mut session.socket,
        outgoing.as_ref(),
        &body,
        &attach,
        &rx,
        &mut run,
        timeout,
        &interrupt,
        on_text,
        on_event,
    );
    let replay = match &result {
        // Accepted steering never reached a successor: send this request explicitly, steering
        // included as ordinary history, on a fresh socket.
        Err(error) if error.downcast_ref::<SteerCommitFailed>().is_some() => true,
        // A reused socket the server already dropped fails before any event: nothing reached
        // the caller, so replay in full on a fresh socket rather than pin this key to SSE.
        Err(error) => reused && error.downcast_ref::<SocketGone>().is_some(),
        Ok(_) => false,
    };
    if replay {
        let _ = session.socket.close(None);
        for (_, _, ticket) in run.pending.drain(..) {
            let _ = ticket.send(SteerOutcome::Uncertain);
        }
        session = match connect(endpoint, session_key, timeout) {
            Ok(socket) => Session::new(socket),
            Err(_) => {
                LIVE.lock().remove(session_key);
                while let Ok(command) = rx.try_recv() {
                    let _ = command.outcome.send(SteerOutcome::Rejected);
                }
                return Ok(None);
            }
        };
        set_poll_timeout(&mut session.socket);
        run = Run {
            pending: VecDeque::new(),
            accepted: Vec::new(),
            tainted: false,
            response_id: None,
        };
        result = stream(
            &mut session.socket,
            Some(&body),
            &body,
            &HashSet::new(),
            &rx,
            &mut run,
            timeout,
            &interrupt,
            on_text,
            on_event,
        );
    }
    let mut result = result.map_err(|error| match error.downcast::<SocketGone>() {
        Ok(gone) => super::stream::IncompleteStream(gone.0).into(),
        Err(error) => error,
    });
    LIVE.lock().remove(session_key);
    // Commands queued after the response ended were never submitted.
    while let Ok(command) = rx.try_recv() {
        let _ = command.outcome.send(SteerOutcome::Rejected);
    }
    // Receipts still outstanding are unknowable once this response is over.
    let uncertain = !run.pending.is_empty();
    for (_, _, ticket) in run.pending.drain(..) {
        let _ = ticket.send(SteerOutcome::Uncertain);
    }
    let tainted = run.tainted || uncertain;
    if let Ok(reply) = &mut result {
        super::responses::stamp_history(reply, &replay_scope);
    }
    match &result {
        Ok(reply) => {
            let items: Vec<Value> = reply
                .provider_content
                .iter()
                .filter(|item| item["type"] == "dial_response_item")
                .map(|item| item["item"].clone())
                .collect();
            match run
                .response_id
                .clone()
                .filter(|_| items.len() == reply.provider_content.len())
            {
                Some(id) => {
                    session.last_request = Some(body);
                    session.last_response_id = Some(id);
                    session.last_response_items = items;
                    session.can_append = true;
                }
                None => session.reset_append(),
            }
            if !run.accepted.is_empty() || tainted {
                if !tainted && session.can_append {
                    session.accepted = std::mem::take(&mut run.accepted);
                } else {
                    // No stray successor may stream into a later request.
                    let _ = session.socket.close(None);
                    return result.map(Some);
                }
            }
            session.idle_since = Instant::now();
            SESSIONS.lock().insert(socket_key, session);
        }
        Err(error) => {
            if error
                .downcast_ref::<super::stream::IncompleteStream>()
                .is_some()
            {
                // Never retry inline after callbacks: partial text/tool data belongs to the
                // failed attempt. Only a subsequent exchange may select HTTP streaming.
                SSE_ONLY.lock().insert(socket_key);
            }
            let _ = session.socket.close(None);
        }
    }
    result.map(Some)
}

fn chained(body: &Value, previous: Option<&str>, input: Vec<Value>) -> Value {
    let mut request = body.clone();
    if let Some(id) = previous {
        request["previous_response_id"] = json!(id);
    }
    request["input"] = Value::Array(input);
    request
}

fn set_poll_timeout(socket: &mut Socket) {
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => {
            stream
                .set_read_timeout(Some(Duration::from_millis(100)))
                .ok();
        }
        MaybeTlsStream::Rustls(stream) => {
            stream
                .sock
                .set_read_timeout(Some(Duration::from_millis(100)))
                .ok();
        }
        _ => {}
    }
}

/// Per-response steering bookkeeping.
struct Run {
    /// Submitted, unacknowledged: (previous_response_id, items, receipt).
    pending: VecDeque<(String, Vec<Value>, SyncSender<SteerOutcome>)>,
    accepted: Vec<(String, Vec<Value>)>,
    tainted: bool,
    response_id: Option<String>,
}

#[derive(Debug)]
struct SteerCommitFailed;
impl std::fmt::Display for SteerCommitFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("accepted steering was not committed to a successor response")
    }
}
impl std::error::Error for SteerCommitFailed {}

/// The socket failed before the server sent any event for this request; leaves `exchange` as
/// `IncompleteStream`.
#[derive(Debug)]
struct SocketGone(&'static str);
impl std::fmt::Display for SocketGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for SocketGone {}

/// Writes can discover dead cached sockets just as reads can. Mid-response writes recover via an
/// incomplete stream so retries use SSE; never replay inline after callbacks. The request write
/// itself reports `SocketGone`, which `exchange` may replay on a fresh socket.
fn write_error(error: tungstenite::Error) -> anyhow::Error {
    match error {
        tungstenite::Error::Io(_)
        | tungstenite::Error::ConnectionClosed
        | tungstenite::Error::AlreadyClosed
        | tungstenite::Error::Protocol(
            tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
        ) => super::stream::IncompleteStream(
            "Codex WebSocket write failed before response.completed; no tool calls were executed",
        )
        .into(),
        error => error.into(),
    }
}

#[allow(clippy::too_many_arguments)]
fn stream(
    socket: &mut Socket,
    outgoing: Option<&Value>,
    full: &Value,
    attach: &HashSet<String>,
    rx: &Receiver<SteerCommand>,
    run: &mut Run,
    timeout: u64,
    interrupt: &AtomicBool,
    on_text: &mut Option<&mut dyn FnMut(&str) -> bool>,
    on_event: &mut dyn FnMut(&StreamEvent) -> bool,
) -> Result<Reply> {
    if let Some(body) = outgoing {
        let mut request = body
            .as_object()
            .cloned()
            .context("invalid Codex request body")?;
        request.insert("type".into(), json!("response.create"));
        let wire = Value::Object(request).to_string();
        let prompt = body.get("previous_response_id").is_some().then_some(full);
        super::wire::record_with_prompt(wire.as_bytes(), prompt, "websocket");
        socket.send(Message::Text(wire.into())).map_err(|error| {
            match write_error(error).downcast::<super::stream::IncompleteStream>() {
                Ok(gone) => SocketGone(gone.0).into(),
                Err(error) => error,
            }
        })?;
    }
    let mut received = false;
    let patience = super::Patience::for_stream(timeout, false);
    let mut acc = super::responses::Accumulator::default();
    let mut bytes = 0usize;
    let mut content_seen = false;
    loop {
        if interrupt.load(Ordering::Relaxed) {
            return Err(Interrupted.into());
        }
        while let Ok(command) = rx.try_recv() {
            let (Some(id), Some(items)) = (
                run.response_id.clone(),
                steer_items(&command.input).cloned(),
            ) else {
                let _ = command.outcome.send(SteerOutcome::Rejected);
                continue;
            };
            let frame =
                json!({"type":"response.steer","previous_response_id":id,"input":command.input});
            if let Err(error) = socket.send(Message::Text(frame.to_string().into())) {
                let _ = command.outcome.send(SteerOutcome::Uncertain);
                run.tainted = true;
                return Err(
                    write_error(error).context("Codex WebSocket steering submission failed")
                );
            }
            run.pending.push_back((id, items, command.outcome));
        }
        match socket.read() {
            Ok(Message::Text(text)) => {
                received = true;
                bytes = bytes.saturating_add(text.len());
                ensure!(
                    bytes <= super::MAX_STREAM_BYTES,
                    "Codex WebSocket response exceeded size limit"
                );
                let event: Value =
                    serde_json::from_str(text.as_ref()).context("invalid Codex WebSocket event")?;
                let kind = event["type"].as_str().unwrap_or("");
                if kind == "response.steer.accepted" || kind == "response.steer.failed" {
                    let steer = &event["steer"];
                    let previous = steer["previous_response_id"].as_str().unwrap_or("");
                    let id = steer["id"].as_str().unwrap_or("");
                    if kind == "response.steer.failed"
                        && !id.is_empty()
                        && run.accepted.iter().any(|(accepted, _)| accepted == id)
                    {
                        // Accepted input the server later failed to commit to a successor.
                        run.accepted.retain(|(accepted, _)| accepted != id);
                        run.tainted = true;
                    }
                    if kind == "response.steer.failed" && attach.contains(id) && !content_seen {
                        return Err(SteerCommitFailed.into());
                    }
                    if let Some(index) = run
                        .pending
                        .iter()
                        .position(|(response, _, _)| response == previous)
                    {
                        let (_, items, ticket) = run.pending.remove(index).expect("index exists");
                        if kind == "response.steer.accepted" && !id.is_empty() {
                            run.accepted.push((id.to_owned(), items));
                            let _ = ticket.send(SteerOutcome::Accepted { id: id.to_owned() });
                        } else {
                            let _ = ticket.send(SteerOutcome::Rejected);
                        }
                    }
                    continue;
                }
                if kind == "response.created" {
                    run.response_id = event
                        .pointer("/response/id")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
                content_seen = true;
                if super::responses::advances_reply(&event.to_string()) {
                    patience.mark_event();
                }
                if acc.feed(
                    &event.to_string(),
                    &mut |s| on_text.as_deref_mut().is_none_or(|cb| cb(s)),
                    on_event,
                )? {
                    return acc.finish();
                }
            }
            Ok(Message::Ping(payload)) => {
                socket.send(Message::Pong(payload)).map_err(write_error)?
            }
            Ok(Message::Close(_)) => {
                const CLOSED: &str =
                    "Codex WebSocket closed before response.completed; no tool calls were executed";
                return Err(if received {
                    super::stream::IncompleteStream(CLOSED).into()
                } else {
                    SocketGone(CLOSED).into()
                });
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                if patience
                    .remaining()
                    .is_some_and(|remaining| remaining.is_zero())
                {
                    return Err(super::stream::StalledStream.into());
                }
            }
            Err(
                tungstenite::Error::Io(_)
                | tungstenite::Error::ConnectionClosed
                | tungstenite::Error::AlreadyClosed
                | tungstenite::Error::Protocol(
                    tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
                ),
            ) => {
                const DROPPED: &str = "Codex WebSocket dropped before response.completed; no tool calls were executed";
                return Err(if received {
                    super::stream::IncompleteStream(DROPPED).into()
                } else {
                    SocketGone(DROPPED).into()
                });
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn connect(
    endpoint: super::responses::Endpoint<'_>,
    session_id: &str,
    timeout: u64,
) -> Result<Socket> {
    let mut url = url::Url::parse(endpoint.url)?;
    url.set_scheme(if url.scheme() == "https" { "wss" } else { "ws" })
        .map_err(|_| anyhow::anyhow!("invalid Codex WebSocket scheme"))?;
    let mut request = url.as_str().into_client_request()?;
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {}", endpoint.token).parse()?,
    );
    if let Some(account) = endpoint.account_id {
        request
            .headers_mut()
            .insert("chatgpt-account-id", account.parse()?);
    }
    request
        .headers_mut()
        .insert("openai-beta", "responses_websockets=2026-02-06".parse()?);
    request.headers_mut().insert("originator", "dial".parse()?);
    request.headers_mut().insert("version", "0.159.0".parse()?);
    request
        .headers_mut()
        .insert("x-session-id", session_id.parse()?);
    request
        .headers_mut()
        .insert("session_id", session_id.parse()?);
    request
        .headers_mut()
        .insert("conversation_id", session_id.parse()?);
    request
        .headers_mut()
        .insert("x-client-request-id", session_id.parse()?);
    // Connect and handshake must not block past the request timeout; tungstenite's own
    // `connect` has neither a TCP connect nor a handshake deadline.
    let limit = (timeout > 0).then(|| Duration::from_secs(timeout));
    let host = url.host_str().context("Codex WebSocket URL has no host")?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let port = url
        .port_or_known_default()
        .context("Codex WebSocket URL has no port")?;
    let mut tcp = None;
    let mut last_error = None;
    for addr in (host, port).to_socket_addrs()? {
        let attempt = match limit {
            Some(limit) => TcpStream::connect_timeout(&addr, limit),
            None => TcpStream::connect(addr),
        };
        match attempt {
            Ok(stream) => {
                tcp = Some(stream);
                break;
            }
            Err(error) => last_error = Some(error),
        }
    }
    let tcp = match (tcp, last_error) {
        (Some(tcp), _) => tcp,
        (None, Some(error)) => return Err(error.into()),
        (None, None) => bail!("Codex WebSocket host resolved to no addresses"),
    };
    tcp.set_nodelay(true)?;
    tcp.set_read_timeout(limit)?;
    tcp.set_write_timeout(limit)?;
    let (socket, _) = tungstenite::client_tls_with_config(request, tcp, None, None).map_err(
        |error| match error {
            tungstenite::handshake::HandshakeError::Failure(error) => anyhow::Error::from(error),
            tungstenite::handshake::HandshakeError::Interrupted(_) => {
                anyhow::anyhow!("Codex WebSocket handshake interrupted")
            }
        },
    )?;
    Ok(socket)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Value {
        json!({"role":"user","content":[{"type":"input_text","text":text}]})
    }

    #[test]
    fn delta_requires_strict_prefix_and_equal_options() {
        let previous = json!({"model":"m","input":[user("a")]});
        let output = vec![
            json!({"type":"message","id":"msg_1","status":"completed","role":"assistant","content":[]}),
        ];
        let replayed = json!({"type":"message","role":"assistant","content":[]});
        let current = json!({"model":"m","input":[user("a"), replayed, user("b")]});
        assert_eq!(
            delta_input(&previous, &output, &current),
            Some(vec![user("b")])
        );
        let mutated = json!({"model":"m","input":[user("x"), json!({"type":"message","role":"assistant","content":[]}), user("b")]});
        assert_eq!(delta_input(&previous, &output, &mutated), None);
        let other_model = json!({"model":"n","input":[user("a"), json!({"type":"message","role":"assistant","content":[]}), user("b")]});
        assert_eq!(delta_input(&previous, &output, &other_model), None);
    }

    #[test]
    fn steered_plan_attaches_creates_or_discards() {
        let steer = vec![user("stop and do b")];
        let output = json!({"type":"function_call_output","call_id":"c1","output":"ok"});
        assert!(matches!(
            plan_steered(Some(&[user("stop and do b")]), &steer),
            Plan::Attach
        ));
        assert!(
            matches!(plan_steered(Some(&[output.clone(), user("stop and do b")]), &steer), Plan::Create(rest) if rest == vec![output])
        );
        assert!(matches!(
            plan_steered(Some(&[user("stop and do b"), user("unrelated")]), &steer),
            Plan::Discard
        ));
        assert!(matches!(
            plan_steered(Some(&[user("other")]), &steer),
            Plan::Discard
        ));
        assert!(matches!(plan_steered(None, &steer), Plan::Discard));
    }

    #[test]
    fn broken_pipe_write_is_retryable_but_protocol_errors_are_not() {
        let error = write_error(tungstenite::Error::Io(std::io::Error::from(
            ErrorKind::BrokenPipe,
        )));
        assert!(
            error
                .downcast_ref::<super::super::stream::IncompleteStream>()
                .is_some()
        );
        assert!(super::super::errors::classify(&error).is_retryable());
        let error = write_error(tungstenite::Error::Protocol(
            tungstenite::error::ProtocolError::InvalidOpcode(15),
        ));
        assert!(!super::super::errors::classify(&error).is_retryable());
    }

    #[test]
    fn only_plain_user_messages_can_steer() {
        assert!(steer_items(&json!([user("a")])).is_some());
        assert!(
            steer_items(&json!([{"type":"function_call_output","call_id":"c","output":""}]))
                .is_none()
        );
        assert!(steer_items(&json!([])).is_none());
    }
}

#[cfg(test)]
mod loopback {
    use super::*;
    use std::net::TcpListener;

    fn text(socket: &mut WebSocket<TcpStream>) -> Value {
        loop {
            if let Message::Text(text) = socket.read().unwrap() {
                return serde_json::from_str(text.as_ref()).unwrap();
            }
        }
    }
    fn send(socket: &mut WebSocket<TcpStream>, event: Value) {
        socket
            .send(Message::Text(event.to_string().into()))
            .unwrap();
    }
    fn completed(id: &str, text: &str) -> Value {
        json!({"type":"response.completed","response":{"id":id,"status":"completed","usage":{"input_tokens":1,"output_tokens":1},
            "output":[{"type":"message","id":format!("msg_{id}"),"status":"completed","role":"assistant","content":[{"type":"output_text","text":text,"annotations":[]}]}]}})
    }

    fn chat() -> Chat {
        Chat {
            messages: vec![json!({"role":"user","content":"hello"})],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 64,
            seed: None,
        }
    }

    /// Serves one stale connection (which must receive no request) and then one fresh connection
    /// that answers the full replay. Returns the replayed request.
    fn stale_then_fresh(listener: TcpListener) -> std::thread::JoinHandle<Value> {
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut stale = tungstenite::accept(stream).unwrap();
            loop {
                match stale.read() {
                    Ok(Message::Text(text)) => panic!("stale socket received a request: {text}"),
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            let (stream, _) = listener.accept().unwrap();
            let mut fresh = tungstenite::accept(stream).unwrap();
            let request = text(&mut fresh);
            send(&mut fresh, completed("r1", "ok"));
            request
        })
    }

    fn run_exchange(endpoint: super::super::responses::Endpoint<'_>, key: &str) -> Option<Reply> {
        exchange(
            endpoint,
            &chat(),
            5,
            &mut None,
            Arc::new(AtomicBool::new(false)),
            None,
            None,
            &mut |_| true,
            key,
        )
        .unwrap()
    }

    #[test]
    fn cached_socket_broken_pipe_replays_on_a_fresh_socket_without_pinning_sse() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/codex/responses");
        let key = format!("loopback-write-{addr}");
        let socket_key = format!("{key}\0acct\0gpt-test");
        let server = stale_then_fresh(listener);
        let endpoint = super::super::responses::Endpoint {
            url: &url,
            token: "test-token",
            account_id: Some("acct"),
            model: "gpt-test",
            headers: &[],
        };
        let socket = connect(endpoint, &key, 5).unwrap();
        let MaybeTlsStream::Plain(tcp) = socket.get_ref() else {
            panic!("loopback connection should not use TLS");
        };
        tcp.shutdown(std::net::Shutdown::Write).unwrap();
        SESSIONS
            .lock()
            .insert(socket_key.clone(), Session::new(socket));
        let reply = run_exchange(endpoint, &key).expect("replayed over WebSocket, not SSE");
        assert_eq!(reply.text, "ok");
        let request = server.join().unwrap();
        assert!(request["previous_response_id"].is_null());
        assert_eq!(request["input"][0]["role"], "user");
        assert!(!SSE_ONLY.lock().contains(&socket_key));
        assert!(SESSIONS.lock().remove(&socket_key).is_some());
    }

    #[test]
    fn idle_cached_socket_is_replaced_before_sending() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/codex/responses");
        let key = format!("loopback-idle-{addr}");
        let socket_key = format!("{key}\0acct\0gpt-test");
        let server = stale_then_fresh(listener);
        let endpoint = super::super::responses::Endpoint {
            url: &url,
            token: "test-token",
            account_id: Some("acct"),
            model: "gpt-test",
            headers: &[],
        };
        let mut idle = Session::new(connect(endpoint, &key, 5).unwrap());
        idle.idle_since = Instant::now() - MAX_IDLE_REUSE - Duration::from_secs(1);
        SESSIONS.lock().insert(socket_key.clone(), idle);
        assert_eq!(run_exchange(endpoint, &key).unwrap().text, "ok");
        assert!(server.join().unwrap()["previous_response_id"].is_null());
        assert!(!SSE_ONLY.lock().contains(&socket_key));
        assert!(SESSIONS.lock().remove(&socket_key).is_some());
    }

    #[test]
    fn websocket_history_survives_http_fallback_and_token_refresh() {
        use std::io::{BufRead, BufReader, Read, Write};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/codex/responses");
        let key = format!("loopback-history-{addr}");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            assert_eq!(text(&mut socket)["type"], "response.create");
            send(&mut socket, completed("ws", "first"));
            drop(socket);

            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(line.starts_with("POST /codex/responses "));
            let mut length = None;
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = Some(value.trim().parse::<usize>().unwrap());
                }
            }
            let mut body = vec![0; length.unwrap()];
            reader.read_exact(&mut body).unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert!(body["input"].as_array().unwrap().iter().any(|item| {
                item["role"] == "assistant" && item["content"][0]["text"] == "first"
            }));
            let events = format!("data: {}\n\n", completed("http", "second"));
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                events.len(),
                events
            )
            .unwrap();
        });
        let endpoint = super::super::responses::Endpoint {
            url: &url,
            token: "initial-token",
            account_id: Some("acct"),
            model: "gpt-test",
            headers: &[],
        };
        let mut chat = chat();
        let reply = exchange(
            endpoint,
            &chat,
            5,
            &mut None,
            Arc::new(AtomicBool::new(false)),
            None,
            None,
            &mut |_| true,
            &key,
        )
        .unwrap()
        .unwrap();
        assert_eq!(reply.text, "first");
        chat.messages
            .push(json!({"role":"assistant","content":reply.text,
            "dial_provider_content":reply.provider_content}));
        chat.messages
            .push(json!({"role":"user","content":"continue"}));

        // Ownership failures must be caught before connecting, on either transport.
        let other_url = format!("{url}/other");
        let mut unverified = chat.clone();
        unverified.messages[1]["dial_provider_content"][0]
            .as_object_mut()
            .unwrap()
            .remove("scope");
        for (route, history) in [
            (
                super::super::responses::Endpoint {
                    account_id: Some("other"),
                    ..endpoint
                },
                &chat,
            ),
            (
                super::super::responses::Endpoint {
                    url: &other_url,
                    ..endpoint
                },
                &chat,
            ),
            (endpoint, &unverified),
        ] {
            let error = exchange(
                route,
                history,
                5,
                &mut None,
                Arc::new(AtomicBool::new(false)),
                None,
                None,
                &mut |_| panic!("invalid history must not emit callbacks"),
                &key,
            )
            .unwrap_err();
            assert!(error.to_string().contains("native history belongs"));
            let error = super::super::responses::exchange_codex(
                route,
                history,
                5,
                &mut None,
                Arc::new(AtomicBool::new(false)),
                None,
                None,
                &mut |_| panic!("invalid history must not emit callbacks"),
                None,
            )
            .unwrap_err();
            assert!(error.to_string().contains("native history belongs"));
        }
        forget(&key);
        let reply = super::super::responses::exchange_codex(
            super::super::responses::Endpoint {
                token: "refreshed-token",
                ..endpoint
            },
            &chat,
            5,
            &mut None,
            Arc::new(AtomicBool::new(false)),
            None,
            None,
            &mut |_| true,
            None,
        )
        .unwrap();
        assert_eq!(reply.text, "second");
        assert_eq!(
            reply.provider_content[0]["scope"],
            chat.messages[1]["dial_provider_content"][0]["scope"]
        );
        server.join().unwrap();
    }

    fn premature_close_uses_sse_on_next_attempt(graceful: bool) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/codex/responses");
        let key = format!("loopback-close-{addr}");
        let socket_key = format!("{key}\0acct\0gpt-test");
        let (closed_tx, closed_rx) = std::sync::mpsc::channel();
        let (check_tx, check_rx) = std::sync::mpsc::channel();
        let (checked_tx, checked_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            assert_eq!(text(&mut socket)["type"], "response.create");
            send(
                &mut socket,
                json!({"type":"response.created","response":{"id":"partial"}}),
            );
            send(
                &mut socket,
                json!({"type":"response.output_text.delta","delta":"unfinished"}),
            );
            send(
                &mut socket,
                json!({"type":"response.output_item.added","output_index":0,
                "item":{"type":"function_call","id":"tool","call_id":"call","name":"read","arguments":""}}),
            );
            send(
                &mut socket,
                json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"path\":"}),
            );
            if graceful {
                socket.close(None).unwrap();
            }
            drop(socket);
            closed_tx.send(()).unwrap();
            // The failed key must select SSE without opening another TCP connection.
            check_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            listener.set_nonblocking(true).unwrap();
            assert!(
                matches!(listener.accept(), Err(error) if error.kind() == ErrorKind::WouldBlock)
            );
            listener.set_nonblocking(false).unwrap();
            checked_tx.send(()).unwrap();
            // Changing just the session, account, or model must still permit WebSockets.
            for _ in 0..3 {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut socket = tungstenite::accept(stream).unwrap();
                assert_eq!(text(&mut socket)["type"], "response.create");
                send(&mut socket, completed("ok", "complete"));
            }
        });
        let endpoint = super::super::responses::Endpoint {
            url: &url,
            token: "t",
            account_id: Some("acct"),
            model: "gpt-test",
            headers: &[],
        };
        let mut seen = String::new();
        let mut on_text = |text: &str| {
            seen.push_str(text);
            true
        };
        let result = exchange(
            endpoint,
            &chat(),
            5,
            &mut Some(&mut on_text),
            Arc::new(AtomicBool::new(false)),
            None,
            None,
            &mut |_| true,
            &key,
        );
        let error = result
            .err()
            .expect("partial text and tool data must not become a Reply");
        assert!(
            error
                .downcast_ref::<super::super::stream::IncompleteStream>()
                .is_some()
        );
        assert!(error.to_string().contains(if graceful {
            "closed before"
        } else {
            "dropped before"
        }));
        assert_eq!(seen, "unfinished");
        assert!(SSE_ONLY.lock().contains(&socket_key));
        assert!(!SESSIONS.lock().contains_key(&socket_key));
        assert!(!LIVE.lock().contains_key(&key));
        closed_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            exchange(
                endpoint,
                &chat(),
                5,
                &mut None,
                Arc::new(AtomicBool::new(false)),
                None,
                None,
                &mut |_| panic!("SSE selection must not emit callbacks"),
                &key
            )
            .unwrap()
            .is_none()
        );
        check_tx.send(()).unwrap();
        checked_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let other_session = format!("{key}-other");
        for (session, account, model) in [
            (other_session.as_str(), "acct", "gpt-test"),
            (key.as_str(), "other-acct", "gpt-test"),
            (key.as_str(), "acct", "other-model"),
        ] {
            let isolated = super::super::responses::Endpoint {
                account_id: Some(account),
                model,
                ..endpoint
            };
            assert_eq!(
                exchange(
                    isolated,
                    &chat(),
                    5,
                    &mut None,
                    Arc::new(AtomicBool::new(false)),
                    None,
                    None,
                    &mut |_| true,
                    session
                )
                .unwrap()
                .unwrap()
                .text,
                "complete"
            );
            assert!(
                !SSE_ONLY
                    .lock()
                    .contains(&format!("{session}\0{account}\0{model}"))
            );
            forget(session);
        }
        server.join().unwrap();
        SSE_ONLY.lock().remove(&socket_key);
    }

    #[test]
    fn graceful_early_close_discards_partial_reply_and_disables_only_its_key() {
        premature_close_uses_sse_on_next_attempt(true);
    }

    #[test]
    fn abrupt_drop_discards_partial_reply_and_disables_only_its_key() {
        premature_close_uses_sse_on_next_attempt(false);
    }

    #[test]
    fn interrupt_and_fatal_event_do_not_disable_websockets() {
        for interrupted in [true, false] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let url = format!("http://{addr}/codex/responses");
            let key = format!("loopback-nontransport-{addr}");
            let server = std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut socket = tungstenite::accept(stream).unwrap();
                assert_eq!(text(&mut socket)["type"], "response.create");
                if !interrupted {
                    send(
                        &mut socket,
                        json!({"type":"response.failed","response":{"error":{"code":"model_not_found"}}}),
                    );
                }
            });
            let endpoint = super::super::responses::Endpoint {
                url: &url,
                token: "t",
                account_id: Some("acct"),
                model: "gpt-test",
                headers: &[],
            };
            let error = exchange(
                endpoint,
                &chat(),
                5,
                &mut None,
                Arc::new(AtomicBool::new(interrupted)),
                None,
                None,
                &mut |_| true,
                &key,
            )
            .err()
            .unwrap();
            assert_eq!(error.downcast_ref::<Interrupted>().is_some(), interrupted);
            assert!(
                error
                    .downcast_ref::<super::super::stream::IncompleteStream>()
                    .is_none()
            );
            assert!(!SSE_ONLY.lock().contains(&format!("{key}\0acct\0gpt-test")));
            assert!(!LIVE.lock().contains_key(&key));
            server.join().unwrap();
        }
    }

    #[test]
    fn failed_steering_commit_replays_on_fresh_websocket_without_disabling_it() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/codex/responses");
        let key = format!("loopback-steer-failed-{addr}");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            // The seeded session attaches to an accepted automatic successor, but the
            // server reports that this steering was never committed to that successor.
            send(
                &mut socket,
                json!({"type":"response.steer.failed","steer":{"id":"s1","previous_response_id":"r1"}}),
            );
            drop(socket);
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let replay = text(&mut socket);
            assert_eq!(replay["type"], "response.create");
            assert!(replay["previous_response_id"].is_null());
            assert_eq!(replay["input"].as_array().unwrap().len(), 2);
            send(&mut socket, completed("replayed", "complete"));
        });
        let endpoint = super::super::responses::Endpoint {
            url: &url,
            token: "t",
            account_id: Some("acct"),
            model: "gpt-test",
            headers: &[],
        };
        let mut chat = chat();
        let mut session = Session::new(connect(endpoint, &key, 5).unwrap());
        session.last_request =
            Some(super::super::responses::request_codex(endpoint.model, &chat, None).unwrap());
        session.last_response_id = Some("r1".into());
        session.can_append = true;
        let steering = json!({"role":"user","content":"redirect"});
        session.accepted = vec![(
            "s1".into(),
            super::super::responses::input_items(&[steering.clone()])
                .unwrap()
                .as_array()
                .unwrap()
                .clone(),
        )];
        chat.messages.push(steering);
        let socket_key = format!("{key}\0acct\0gpt-test");
        SESSIONS.lock().insert(socket_key.clone(), session);
        let reply = exchange(
            endpoint,
            &chat,
            5,
            &mut None,
            Arc::new(AtomicBool::new(false)),
            None,
            None,
            &mut |_| true,
            &key,
        )
        .unwrap()
        .unwrap();
        assert_eq!(reply.text, "complete");
        assert!(!SSE_ONLY.lock().contains(&socket_key));
        server.join().unwrap();
        forget(&key);
    }

    /// Accepted steering governs the next request: the client reads the server's automatic
    /// successor without sending, then chains the following turn on the same socket.
    #[test]
    fn accepted_steering_attaches_to_successor_then_chains() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/codex/responses", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let first = text(&mut socket);
            assert_eq!(first["type"], "response.create");
            assert!(first["previous_response_id"].is_null());
            send(
                &mut socket,
                json!({"type":"response.created","response":{"id":"r1"}}),
            );
            send(
                &mut socket,
                json!({"type":"response.output_text.delta","delta":"o"}),
            );
            let steer = text(&mut socket);
            assert_eq!(steer["type"], "response.steer");
            assert_eq!(steer["previous_response_id"], "r1");
            send(
                &mut socket,
                json!({"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"r1"}}),
            );
            send(&mut socket, completed("r1", "one"));
            // Automatic successor carrying the accepted steering; the client must not send for it.
            send(
                &mut socket,
                json!({"type":"response.created","response":{"id":"r2"}}),
            );
            send(&mut socket, completed("r2", "two"));
            let third = text(&mut socket);
            assert_eq!(third["type"], "response.create");
            assert_eq!(third["previous_response_id"], "r2");
            assert_eq!(third["input"], json!([{"role":"user","content":"c"}]));
            send(
                &mut socket,
                json!({"type":"response.created","response":{"id":"r3"}}),
            );
            send(&mut socket, completed("r3", "three"));
        });
        let key = format!("loopback-{}", std::process::id());
        let endpoint = super::super::responses::Endpoint {
            url: &url,
            token: "t",
            account_id: Some("acct"),
            model: "gpt-test",
            headers: &[],
        };
        let mut on_event = |_: &StreamEvent| true;
        let run = |chat: &Chat,
                   on_text: &mut Option<&mut dyn FnMut(&str) -> bool>,
                   on_event: &mut dyn FnMut(&StreamEvent) -> bool| {
            exchange(
                endpoint,
                chat,
                10,
                on_text,
                Arc::new(AtomicBool::new(false)),
                None,
                None,
                on_event,
                &key,
            )
        };
        let user = |text: &str| json!({"role":"user","content":text});
        let mut chat = Chat {
            messages: vec![user("a")],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 64,
            seed: None,
        };
        let mut ticket = None;
        let mut steer = |_: &str| {
            if ticket.is_none() {
                ticket = submit(
                    &key,
                    super::super::responses::input_items(&[user("b")]).unwrap(),
                )
                .unwrap();
            }
            true
        };
        let first = run(&chat, &mut Some(&mut steer), &mut on_event)
            .unwrap()
            .unwrap();
        assert_eq!(first.text, "one");
        assert_eq!(
            ticket.unwrap().try_outcome(),
            Some(SteerOutcome::Accepted { id: "s1".into() })
        );
        chat.messages.push(json!({"role":"assistant","content":"one","dial_provider_content":first.provider_content}));
        chat.messages.push(user("b"));
        let second = run(&chat, &mut None, &mut on_event).unwrap().unwrap();
        assert_eq!(second.text, "two");
        chat.messages.push(json!({"role":"assistant","content":"two","dial_provider_content":second.provider_content}));
        chat.messages.push(user("c"));
        assert_eq!(
            run(&chat, &mut None, &mut on_event).unwrap().unwrap().text,
            "three"
        );
        server.join().unwrap();
        assert!(!SSE_ONLY.lock().contains(&format!("{key}\0acct\0gpt-test")));
        forget(&key);
    }
}

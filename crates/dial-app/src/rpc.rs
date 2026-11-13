//! Versioned newline-delimited JSON control protocol. Stdin is control, stdout is JSON only.
use crate::agent::workflow::todo::TodoList;
use crate::agent::{Agent, Decision, Ui, make_backend};
use crate::config::paths::Layout;
use crate::config::types::Config;
use crate::runtime::tool_output::ToolOutput;
use crate::storage::session::{Session, SessionMeta};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    io::BufRead,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock, TryLockError,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

mod actor;
mod auth;
mod checkpoints;
mod client_fs;
mod extensions;
mod host_tools;
mod host_uris;
mod jobs;
mod models;
mod parity;
mod sessions;
mod settings;
/// Wire vocabulary shared with clients: versions, the method table and error codes.
pub use dial_protocol::PROTOCOL_VERSION;
use dial_protocol::{DEFAULT_PROTOCOL_VERSION, ErrorCode, METHODS, SUPPORTED_PROTOCOL_VERSIONS};
const MAX_REQUEST: usize = 1024 * 1024;
/// ACP clients may send larger frames (embedded resources and images).
const MAX_ACP_REQUEST: usize = 8 * MAX_REQUEST;
/// Queued `steer` messages per active turn.
const MAX_STEERING: usize = 8;
const MAX_SEED_MESSAGES: usize = 200;
const MAX_SEED_ENTRY_BYTES: usize = 1024 * 1024;
const MAX_MEMORY_COMMAND_BYTES: usize = 8192;
const MESSAGES_PAGE_DEFAULT: u64 = 50;
const MESSAGES_PAGE_MAX: u64 = 500;
/// `tool_result` events clip their text to this many bytes (the model-visible result is unchanged).
const TOOL_RESULT_EVENT_BYTES: usize = 24 * 1024;

/// Largest request line the reader accepts on this transport.
fn request_limit(acp: bool) -> usize {
    if acp { MAX_ACP_REQUEST } else { MAX_REQUEST }
}

/// Capabilities a transport lacks: ACP has no custom question input, no live voice, no host tool or
/// host URI bridge, no provider sign-in, and no session administration, settings, checkpoint,
/// workspace-directory, background-job, session-control or schema methods (these are native
/// `dial rpc` capabilities).
fn capability_available(capability: &str, acp: bool) -> bool {
    !(acp
        && matches!(
            capability,
            "questions"
                | "live_voice"
                | "host_tools"
                | "host_uris"
                | "auth"
                | "session_admin"
                | "settings"
                | "checkpoints"
                | "extensions"
                | "workspace"
                | "jobs"
                | "session_control"
                | "schema"
        ))
}

/// The first event of every connection: identity, negotiation data and the limits the reader and
/// handlers actually enforce.
pub(crate) fn ready_frame(
    session_id: &str,
    model: &str,
    tools: Value,
    image_input: bool,
    acp: bool,
) -> Value {
    // `dial_protocol::METHODS` is the one method table: `ready` derives its method and
    // capability lists from it, `docs/rpc-schema.json` is tested against it, and a source scan
    // test keeps it equal to the methods the dispatchers match.
    let methods: Vec<&str> = METHODS
        .iter()
        .filter(|method| capability_available(method.capability, acp))
        .map(|method| method.name)
        .collect();
    let mut capabilities: Vec<&str> = Vec::new();
    for method in METHODS {
        if capability_available(method.capability, acp)
            && !capabilities.contains(&method.capability)
        {
            capabilities.push(method.capability);
        }
    }
    let question_input = if acp {
        json!({"supported":false,"reason":"ACP custom input capability was not negotiated"})
    } else {
        json!({"supported":true,"protocol":1,"presentation_ack_required":true,"answer_method":"question_answer","presentation_method":"question_presented","images":"explicit_user_paths","preview":"text"})
    };
    json!({
        "event":"ready",
        "engine_version":crate::build_info::VERSION,
        "protocol_version":DEFAULT_PROTOCOL_VERSION,
        "supported_protocol_versions":SUPPORTED_PROTOCOL_VERSIONS,
        "session_id":session_id,
        "model":model,
        "tools":tools,
        "image_input":image_input,
        "methods":methods,
        "capabilities":capabilities,
        "limits":{
            "max_request_bytes":request_limit(acp),
            "max_pending_steering":MAX_STEERING,
            "max_seed_messages":MAX_SEED_MESSAGES,
            "max_seed_message_bytes":MAX_SEED_ENTRY_BYTES,
            "max_memory_command_bytes":MAX_MEMORY_COMMAND_BYTES,
            "messages_page_default":MESSAGES_PAGE_DEFAULT,
            "messages_page_max":MESSAGES_PAGE_MAX,
            "tool_result_event_bytes":TOOL_RESULT_EVENT_BYTES,
        },
        "question_input":question_input,
    })
}
struct Output {
    acp: Option<crate::acp::Adapter>,
    /// Negotiated protocol version; `DEFAULT_PROTOCOL_VERSION` until the client sends `hello`.
    protocol: u32,
    /// Set by `hello {interactive:true}`: a person answers approvals and questions, so the engine
    /// may ask (directory grants, recovery choices, draft review). Off by default for scripts.
    interactive: bool,
    /// Envelope fields stamped on every plain-RPC event (ACP frames are mapped, not stamped).
    envelope: Envelope,
}
/// Attribution for plain-RPC events. `seq` is assigned under the same lock that reserves the
/// output position, so it increases in stdout order with no gaps; a gap means a lost frame.
#[derive(Default)]
struct Envelope {
    seq: u64,
    /// Session the actor was serving when the event was produced.
    session_id: Option<String>,
    /// Id of the actor request in flight, or null for background work between requests.
    request_id: Value,
}
impl Envelope {
    fn stamp(&mut self, value: &mut Value) {
        let Some(event) = value.as_object_mut().filter(|v| v.contains_key("event")) else {
            return;
        };
        self.seq += 1;
        event.insert("seq".into(), json!(self.seq));
        if !event.contains_key("session_id")
            && let Some(session) = &self.session_id
        {
            event.insert("session_id".into(), json!(session));
        }
        event.insert("request_id".into(), self.request_id.clone());
    }
}
/// A request failure with a stable machine code. Protocol 2 clients receive the code next to the
/// message; protocol 1 clients receive the message alone.
#[derive(Debug)]
pub(super) struct RequestError {
    pub(super) code: ErrorCode,
    pub(super) message: String,
}
impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for RequestError {}
/// A request failure carrying a stable protocol-2 code; [`ErrorCode`] documents each one.
/// Uncoded failures are reported as `failed`.
pub(super) fn coded(code: ErrorCode, error: impl std::fmt::Display) -> anyhow::Error {
    anyhow::Error::new(RequestError {
        code,
        message: format!("{error:#}"),
    })
}
/// A plain RPC method listed in [`METHODS`].
fn known_method(method: &str) -> bool {
    dial_protocol::method(method).is_some()
}
/// The error for a method this endpoint does not serve; the actor returns the same one.
pub(super) fn unknown_method() -> anyhow::Error {
    anyhow::Error::new(RequestError {
        code: ErrorCode::UnknownMethod,
        message: "unknown RPC method".to_owned(),
    })
}
/// Highest served protocol version the client can speak. A client that asks for a version older
/// than every served one gets a coded error listing what this build serves.
fn negotiated_version(requested: u64) -> Result<u32> {
    SUPPORTED_PROTOCOL_VERSIONS
        .iter()
        .copied()
        .filter(|version| u64::from(*version) <= requested)
        .max()
        .ok_or_else(|| {
            anyhow::Error::new(RequestError {
                code: ErrorCode::UnsupportedProtocolVersion,
                message: format!(
                    "protocol_version {requested} is not supported; supported: {SUPPORTED_PROTOCOL_VERSIONS:?}"
                ),
            })
        })
}
enum Batch {
    Single(Value),
    Mapped(Vec<Value>),
}
#[derive(Clone)]
struct Sender {
    output: Arc<Mutex<Output>>,
    writer: crate::output::StdoutWriter,
    failed: Arc<AtomicBool>,
    failure: Arc<OnceLock<String>>,
}
impl Sender {
    fn unavailable(&self) -> bool {
        self.failed.load(Ordering::SeqCst) || self.writer.is_disconnected() || self.writer.failed()
    }
    fn mapping(&self) -> Result<MutexGuard<'_, Output>> {
        let started = Instant::now();
        loop {
            anyhow::ensure!(!self.unavailable(), "control transport disconnected");
            match self.output.try_lock() {
                Ok(output) => return Ok(output),
                Err(TryLockError::Poisoned(_)) => {
                    anyhow::bail!("control protocol ownership poisoned")
                }
                Err(TryLockError::WouldBlock) => {
                    anyhow::ensure!(
                        started.elapsed() < CLEANUP_TIMEOUT,
                        "control protocol owner made no progress for 10 seconds"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
    fn fail(&self, error: &anyhow::Error) {
        let _ = self.failure.set(format!("{error:#}"));
        self.failed.store(true, Ordering::SeqCst);
        self.writer.disconnect();
    }
    fn publish(&self, ticket: u64, batch: Batch) -> Result<()> {
        match batch {
            Batch::Single(value) => self.writer.json_batch(ticket, std::iter::once(value))?,
            Batch::Mapped(values) => self.writer.json_batch(ticket, values)?,
        }
        Ok(())
    }
    /// Returns false when the event was not published (ACP dropped it or the transport failed).
    fn emit(&self, mut value: Value) -> bool {
        let result = (|| -> Result<bool> {
            let (ticket, batch) = {
                let mut guard = self.mapping()?;
                let output = &mut *guard;
                let batch = match &mut output.acp {
                    Some(acp) => {
                        let values = acp.emit(value);
                        if values.is_empty() {
                            return Ok(false);
                        }
                        Batch::Mapped(values)
                    }
                    None => {
                        output.envelope.stamp(&mut value);
                        Batch::Single(value)
                    }
                };
                (self.writer.reserve(), batch)
            };
            self.publish(ticket, batch)?;
            Ok(true)
        })();
        match result {
            Ok(published) => published,
            Err(error) => {
                self.fail(&error);
                false
            }
        }
    }
    fn receive(&self, value: Value) -> Result<Option<Value>> {
        let result = (|| -> Result<Option<Value>> {
            let (request, batch) = {
                let mut output = self.mapping()?;
                let Some(acp) = &mut output.acp else {
                    return Ok(Some(value));
                };
                let (request, replies) = acp.receive(value);
                let batch = if replies.is_empty() {
                    None
                } else {
                    Some((self.writer.reserve(), Batch::Mapped(replies)))
                };
                (request, batch)
            };
            if let Some((ticket, batch)) = batch {
                self.publish(ticket, batch)?;
            }
            Ok(request)
        })();
        if let Err(error) = &result {
            self.fail(error);
        }
        result
    }
    /// Negotiated protocol version of this connection.
    fn protocol(&self) -> u32 {
        self.mapping()
            .map_or(DEFAULT_PROTOCOL_VERSION, |output| output.protocol)
    }
    fn negotiate(&self, version: u32, interactive: bool) -> Result<()> {
        let mut output = self.mapping()?;
        output.protocol = version;
        output.interactive = interactive;
        Ok(())
    }
    /// Attribute later events to `session_id` and to the request in flight (null when idle).
    fn attribute(&self, session_id: &str, request_id: Value) {
        if let Ok(mut output) = self.mapping() {
            output.envelope.session_id = Some(session_id.to_owned());
            output.envelope.request_id = request_id;
        }
    }
    fn result(&self, id: &Value, result: Result<Value>) {
        let v2 = self.protocol() >= 2;
        self.emit(match result {
            Ok(v) => json!({"id":id,"result":v}),
            Err(e) if v2 => {
                let code = e
                    .downcast_ref::<RequestError>()
                    .map_or(ErrorCode::Failed, |error| error.code)
                    .as_str();
                json!({"id":id,"error":{"code":code,"message":format!("{e:#}")}})
            }
            Err(e) => json!({"id":id,"error":{"message":format!("{e:#}")}}),
        });
    }
    fn protocol_error(&self, message: &str, parse: bool) {
        let result = (|| -> Result<()> {
            let (ticket, value) = {
                let output = self.mapping()?;
                let value = if output.acp.is_some() {
                    json!({"jsonrpc":"2.0","id":null,"error":{"code":if parse {-32700} else {-32600},"message":message}})
                } else {
                    json!({"id":null,"error":{"message":message}})
                };
                (self.writer.reserve(), value)
            };
            self.publish(ticket, Batch::Single(value))
        })();
        if let Err(error) = result {
            self.fail(&error);
        }
    }
}
#[derive(Clone)]
struct PendingQuestion {
    request: crate::agent::workflow::questions::QuestionRequest,
    presented: bool,
    deadline: Option<Instant>,
}
struct RpcUi {
    out: Sender,
    approvals: mpsc::Receiver<(String, Decision)>,
    pending_approval: Arc<Mutex<Option<String>>>,
    question_answers: mpsc::Receiver<(String, crate::agent::workflow::questions::QuestionOutcome)>,
    pending_question: Arc<Mutex<Option<PendingQuestion>>>,
    interrupt: Arc<AtomicBool>,
    input_disconnected: Arc<AtomicBool>,
    steering: Arc<Mutex<Vec<String>>>,
    prompt_queued: Arc<AtomicBool>,
    next: u64,
    node_cursors: std::collections::BTreeMap<String, usize>,
    node_revisions: std::collections::BTreeMap<String, u64>,
    /// Last emitted metadata per node. Comparing it across revisions tells a text append from a metadata edit.
    node_meta: std::collections::BTreeMap<String, Value>,
    /// Set by runtime changes; the idle worker republishes the query projection when this is set.
    projection_dirty: bool,
    tree: Option<crate::runtime::Tree>,
}
impl RpcUi {
    fn request_approval(
        &mut self,
        tool: &str,
        summary: &str,
        call_id: Option<&str>,
        cancel: Option<&AtomicBool>,
    ) -> Decision {
        self.next += 1;
        let approval_id = format!("approval-{}", self.next);
        if let Ok(mut pending) = self.pending_approval.lock() {
            *pending = Some(approval_id.clone());
        } else {
            return Decision::Deny;
        }
        let mut event =
            json!({"event":"approval","approval_id":approval_id,"tool":tool,"summary":summary});
        // The call card's ID lets clients attach the request to that exact call, even among same-name calls.
        if let Some(call_id) = call_id {
            event["call_id"] = json!(call_id);
        }
        let delivered = self.out.emit(event);
        // ACP without an active session drops the event: no client can answer it, so deny.
        let decision = if !delivered {
            Decision::Deny
        } else {
            loop {
                if self.interrupt.load(Ordering::SeqCst)
                    || self.out.unavailable()
                    || cancel.is_some_and(|flag| flag.load(Ordering::SeqCst))
                {
                    break Decision::Deny;
                }
                match self.approvals.recv_timeout(Duration::from_millis(100)) {
                    Ok((id, d)) if id == approval_id => break d,
                    Ok(_) => {}
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(_) => break Decision::Deny,
                }
            }
        };
        if delivered {
            // The request is no longer pending for any client once decided, so clients can retire it.
            self.out
                .emit(json!({"event":"approval_resolved","approval_id":approval_id}));
        }
        if let Ok(mut pending) = self.pending_approval.lock()
            && pending.as_deref() == Some(&approval_id)
        {
            *pending = None;
        }
        decision
    }
}
/// Everything a client renders about a node except its text. Comparing it across revisions tells a
/// text append apart from a metadata edit without copying the text.
fn node_metadata(node: &crate::runtime::Node) -> Value {
    json!({
        "kind": node.kind,
        "label": node.label,
        "parent": node.parent,
        "call_id": node.call_id,
        "presentation": node.presentation,
        "phase": node.result.phase,
        "execution": node.result.execution,
        "data": node.result.data,
    })
}
impl Ui for RpcUi {
    /// Plain RPC clients opt in through `hello`; ACP never collects free input this way.
    fn interactive_input(&self) -> bool {
        self.out
            .mapping()
            .is_ok_and(|output| output.acp.is_none() && output.interactive)
    }
    fn runtime(&mut self, tree: &crate::runtime::Tree) {
        if self.tree.as_ref().is_none_or(|old| !old.same(tree)) {
            self.node_cursors.clear();
            self.node_revisions.clear();
            self.node_meta.clear();
            self.tree = Some(tree.clone());
            self.projection_dirty = true;
            self.out.emit(json!({"event":"state_reset"}));
        }
    }
    fn heartbeat(&mut self) {
        if let Some(tree) = self.tree.clone() {
            for body in tree.bodies("worker") {
                self.result_node(&body);
            }
        }
    }
    fn maintenance_ready(&mut self) -> bool {
        !self.prompt_queued.load(Ordering::SeqCst) && !self.out.unavailable()
    }
    fn result_node(&mut self, body: &crate::runtime::Body) {
        body.read(|node| {
            if self.node_revisions.insert(node.id.clone(), node.result.revision) == Some(node.result.revision) { return; }
            self.projection_dirty = true;
            let metadata = node_metadata(node);
            let metadata_changed = self.node_meta.insert(node.id.clone(), metadata.clone()).as_ref() != Some(&metadata);
            let len = node.result.text.len();
            let previous = self.node_cursors.insert(node.id.clone(), len);
            // Only growth of a running, non-worker node's text is a delta; anything else sends the node.
            let text_only = !metadata_changed && node.kind != "worker" && node.result.phase.is_running();
            match previous {
                Some(bytes) if text_only && bytes <= len && node.result.text.is_char_boundary(bytes) => {
                    if bytes < len {
                        self.out.emit(json!({"event":"state_delta","id":node.id,"revision":node.result.revision,"append":&node.result.text[bytes..]}));
                    }
                }
                _ => {
                    self.out.emit(json!({"event":"state_node","node":node}));
                }
            }
        });
    }
    fn text_delta(&mut self, text: &str) {
        self.out.emit(json!({"event":"text","delta":text}));
    }
    fn tool_call_with_id(&mut self, call_id: &str, name: &str, summary: &str) {
        self.out
            .emit(json!({"event":"tool_call","call_id":call_id,"name":name,"summary":summary}));
    }
    fn tool_result_with_id(&mut self, call_id: &str, name: &str, output: &ToolOutput) {
        self.out.emit(json!({"event":"tool_result","call_id":call_id,"name":name,"text":crate::agent::safety::guard::clip_middle(&output.text,TOOL_RESULT_EVENT_BYTES),"is_error":output.is_error,"data":output.data,"status":crate::tools::ToolStatus::from_output(output),"effects":output.execution.effects,"changed_paths":output.execution.changed_paths}));
    }
    /// The terminal's card for this call, as fields a GUI renders without parsing summaries.
    fn tool_presentation(
        &mut self,
        call_id: &str,
        view: &crate::tools::surface::presentation::Presentation,
    ) {
        let mut card = json!({"event":"tool_card","call_id":call_id});
        if let (Some(card), Ok(Value::Object(fields))) =
            (card.as_object_mut(), serde_json::to_value(view))
        {
            card.extend(fields);
        }
        self.out.emit(card);
    }
    fn notice(&mut self, message: &str) {
        self.out.emit(json!({"event":"notice","message":message}));
    }
    /// Same `notice` event clients already read, plus the typed kind and lane for richer clients.
    fn event(&mut self, event: &crate::agent::ui::ui_event::UiEvent) {
        self.out.emit(typed_notice(event));
    }
    fn model_progress(&mut self, progress: &crate::llm::ModelProgress) {
        self.out
            .emit(json!({"event":"model_progress","progress":progress}));
    }
    fn context_usage(&mut self, tokens: u64, estimated: bool) {
        self.out
            .emit(json!({"event":"context_usage","tokens":tokens,"estimated":estimated}));
    }
    fn usage(&mut self, prompt: Option<u64>, output: Option<u64>, seconds: f64) {
        self.out.emit(json!({"event":"usage","prompt_tokens":prompt,"output_tokens":output,"seconds":seconds}));
    }
    fn efficiency(&mut self, report: &Value) {
        self.out.emit(json!({"event":"efficiency","report":report}));
    }
    fn tool_timing(&mut self, call_id: &str, name: &str, seconds: f64) {
        self.out
            .emit(json!({"event":"tool_timing","call_id":call_id,"name":name,"seconds":seconds}));
    }
    fn tool_output(&mut self, name: &str, text: &str) {
        self.out
            .emit(json!({"event":"tool_output","name":name,"delta":text}));
    }
    fn background_notice(&mut self, message: &str) {
        self.out
            .emit(json!({"event":"background","message":message}));
    }
    fn background_output(&mut self, name: &str, text: &str, replace: bool) {
        self.out
            .emit(json!({"event":"background","name":name,"message":text,"replace":replace}));
    }
    fn reasoning_delta(&mut self, text: &str) {
        self.out.emit(json!({"event":"reasoning","delta":text}));
    }
    fn tool_arguments_delta(&mut self, index: u64, id: &str, name: &str, delta: &str) {
        self.out.emit(
            json!({"event":"tool_arguments","index":index,"id":id,"name":name,"delta":delta}),
        );
    }
    fn cached_usage(&mut self, read: Option<u64>, write: Option<u64>) {
        self.out
            .emit(json!({"event":"cached_usage","read_tokens":read,"write_tokens":write}));
    }
    fn extension_panel(&mut self, title: &str, markdown: &str) {
        self.out
            .emit(json!({"event":"panel","title":title,"markdown":markdown}));
    }
    fn todo(&mut self, todo: &TodoList) {
        self.out
            .emit(json!({"event":"todo","tasks":todo.snapshot()}));
    }
    fn take_steering(&mut self) -> Vec<String> {
        self.steering
            .lock()
            .map(|mut q| std::mem::take(&mut *q))
            .unwrap_or_default()
    }
    fn pending_input(&mut self) -> bool {
        self.steering.lock().is_ok_and(|queue| !queue.is_empty())
    }
    fn policy_explanation(
        &mut self,
        call_id: &str,
        tool: &str,
        explanation: &crate::agent::safety::approval::PolicyExplanation,
    ) {
        self.out.emit(json!({"event":"policy_explanation", "call_id":call_id, "tool":tool, "explanation":explanation, "message":explanation.to_string()}));
    }
    fn approve(&mut self, tool: &str, summary: &str) -> Decision {
        self.request_approval(tool, summary, None, None)
    }
    fn approve_with_cancel(&mut self, tool: &str, summary: &str, cancel: &AtomicBool) -> Decision {
        self.request_approval(tool, summary, None, Some(cancel))
    }
    fn approve_call(
        &mut self,
        call_id: &str,
        tool: &str,
        summary: &str,
        cancel: Option<&AtomicBool>,
    ) -> Decision {
        let call_id = (!call_id.is_empty()).then_some(call_id);
        self.request_approval(tool, summary, call_id, cancel)
    }
    fn ask_questions(
        &mut self,
        request: &crate::agent::workflow::questions::QuestionRequest,
        cancel: &AtomicBool,
    ) -> crate::agent::workflow::questions::QuestionOutcome {
        use crate::agent::workflow::questions::{self, QuestionOutcome};
        if !self.out.mapping().is_ok_and(|output| output.acp.is_none()) {
            return if self.out.unavailable() {
                QuestionOutcome::Disconnected
            } else {
                QuestionOutcome::Unsupported("ACP custom question input was not negotiated; permission choices are not user answers. Ask is unsupported on this ACP connection.".into())
            };
        }
        if self.input_disconnected.load(Ordering::SeqCst) || self.out.unavailable() {
            return QuestionOutcome::Disconnected;
        }
        if cancel.load(Ordering::SeqCst) || self.interrupt.load(Ordering::SeqCst) {
            return QuestionOutcome::Cancelled;
        }
        if let Ok(mut pending) = self.pending_question.lock() {
            if pending.is_some() {
                return QuestionOutcome::Unsupported(
                    "another owner question is already pending".into(),
                );
            }
            *pending = Some(PendingQuestion {
                request: request.clone(),
                presented: false,
                deadline: None,
            });
        } else {
            return QuestionOutcome::Disconnected;
        }
        self.out.emit(json!({"event":"question","request":request,"presentation_ack_required":true,
            "capabilities":{"protocol":1,"multiple":true,"custom_input":true,"notes":true,"chat":true,"images":"explicit_user_paths","preview":"text"}}));
        let outcome = loop {
            if self.input_disconnected.load(Ordering::SeqCst) || self.out.unavailable() {
                break QuestionOutcome::Disconnected;
            }
            if cancel.load(Ordering::SeqCst) || self.interrupt.load(Ordering::SeqCst) {
                break QuestionOutcome::Cancelled;
            }
            let expired = match self.pending_question.lock() {
                Ok(mut pending) => {
                    let expired = pending.as_ref().is_some_and(|value| {
                        value.request.question_id == request.question_id
                            && value
                                .deadline
                                .is_some_and(|deadline| Instant::now() >= deadline)
                    });
                    if expired {
                        *pending = None;
                    }
                    expired
                }
                Err(_) => break QuestionOutcome::Disconnected,
            };
            if expired {
                break questions::expire(request, questions::empty_answers(request));
            }
            match self
                .question_answers
                .recv_timeout(Duration::from_millis(50))
            {
                Ok((id, outcome)) if id == request.question_id => break outcome,
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break QuestionOutcome::Disconnected,
            }
        };
        if let Ok(mut pending) = self.pending_question.lock()
            && pending
                .as_ref()
                .is_some_and(|value| value.request.question_id == request.question_id)
        {
            *pending = None;
        }
        self.out.emit(json!({"event":"question_closed","question_id":request.question_id,"generation":request.generation,"session_id":request.session_id,
            "outcome":match &outcome { QuestionOutcome::Answered(_) => "answered", QuestionOutcome::Chat => "chat", QuestionOutcome::Cancelled => "cancelled", QuestionOutcome::Disconnected => "disconnected", QuestionOutcome::Unsupported(_) => "unsupported" }}));
        outcome
    }
}

#[cfg(any(unix, test))]
fn read_request(
    reader: &mut impl BufRead,
) -> std::io::Result<Option<Result<Vec<u8>, &'static str>>> {
    read_request_limit(reader, MAX_REQUEST)
}
#[cfg(any(unix, test))]
fn read_request_limit(
    reader: &mut impl BufRead,
    limit: usize,
) -> std::io::Result<Option<Result<Vec<u8>, &'static str>>> {
    read_request_limit_with(reader, limit, |_| {
        #[cfg(unix)]
        {
            let mut poll = libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll points to one initialized descriptor and waits only 25 ms.
            if unsafe { libc::poll(&mut poll, 1, 25) } < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
        #[cfg(windows)]
        std::thread::sleep(Duration::from_millis(25));
        Ok(())
    })
}

fn read_request_limit_with<R: BufRead>(
    reader: &mut R,
    limit: usize,
    mut wait_readable: impl FnMut(&mut R) -> std::io::Result<()>,
) -> std::io::Result<Option<Result<Vec<u8>, &'static str>>> {
    let mut bytes = Vec::new();
    let mut oversized = false;
    loop {
        let buffer = match reader.fill_buf() {
            Ok(buffer) => buffer,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                // A terminal can share stdout's open-file description; preserve partial input
                // while its status flags are nonblocking rather than treating EAGAIN as EOF.
                wait_readable(reader)?;
                continue;
            }
            Err(error) => return Err(error),
        };
        if buffer.is_empty() {
            return Ok(if bytes.is_empty() && !oversized {
                None
            } else if oversized {
                Some(Err(if limit == MAX_REQUEST {
                    "RPC request exceeds 1 MiB"
                } else {
                    "ACP request exceeds 8 MiB"
                }))
            } else {
                Some(Ok(bytes))
            });
        }
        let used = buffer
            .iter()
            .position(|b| *b == b'\n')
            .map_or(buffer.len(), |at| at + 1);
        let ended = buffer[used - 1] == b'\n';
        if !oversized {
            // Only a real line terminator is exempt from the payload limit. Reserving an
            // unconditional extra byte also accepts oversized requests ending at EOF.
            if bytes.len().saturating_add(used - usize::from(ended)) > limit {
                oversized = true;
                bytes.clear();
            } else {
                bytes.extend_from_slice(&buffer[..used]);
            }
        }
        reader.consume(used);
        if ended {
            if oversized {
                return Ok(Some(Err(if limit == MAX_REQUEST {
                    "RPC request exceeds 1 MiB"
                } else {
                    "ACP request exceeds 8 MiB"
                })));
            }
            bytes.pop();
            return Ok(Some(Ok(bytes)));
        }
    }
}

/// Healthy work has no artificial deadline. Only cancellation/connection teardown has this bound.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);
fn stop_owned_tasks(agent: &mut Agent<'_>, ui: &mut RpcUi) {
    let control = agent.live_task_control();
    for (id, running) in control.list() {
        if running {
            let _ = control.cancel(&id);
        }
    }
    let started = Instant::now();
    while control.list().iter().any(|(_, running)| *running) {
        agent.pump_subagents(ui);
        if started.elapsed() >= CLEANUP_TIMEOUT {
            eprintln!(
                "control cancellation did not settle within 10 seconds; terminating owned actor"
            );
            let _ = crate::flush_stdout_on_exit();
            std::process::exit(1);
        }
        thread::sleep(Duration::from_millis(20));
    }
    agent.pump_subagents(ui);
}

/// Worker-owned query projection. `state` and `messages` are kept apart so a `state` query is
/// answered without reading or cloning the message history.
struct QueryProjection {
    state: Value,
    messages: Value,
    cursor: crate::storage::session::MessageCursor,
}

impl Default for QueryProjection {
    fn default() -> Self {
        Self {
            state: Value::Null,
            messages: json!([]),
            cursor: Default::default(),
        }
    }
}

/// The single `state` projection, used by the worker's `state` arm and by the cached reader reply.
fn state_projection(agent: &Agent<'_>) -> Value {
    let (usage_total, usage_turn) = agent.usage();
    let workspace = agent.workspace();
    json!({"session_id":agent.session().id(),"title":agent.session().title(),"tools":agent.tool_names(),
        "plan_mode":agent.plan_mode(),"approval":agent.approval(),"cost":agent.session_cost(),
        "cost_note":"observed priced session spend, not an invoice total; usage has explicit unknown-cost coverage",
        "observed_cost":usage_total.all.observed_cost_usd,"usage":{"total":usage_total,"turn":usage_turn},
        "metrics":agent.metrics(),"efficiency":agent.efficiency_report(),"speculation":agent.speculation_stats(),"tree":agent.session().execution_tree(),
        "workspace":{"root":workspace.root(),"dirs":workspace.additional_roots()},
        "current_model":models::current(agent.effective_config())})
}

/// The schema clients generate bindings from: `docs/rpc-schema.json` as built into this engine.
const SCHEMA_JSON: &str = include_str!("../../../docs/rpc-schema.json");

fn schema_document() -> Result<Value> {
    serde_json::from_str(SCHEMA_JSON).context("embedded RPC schema is invalid")
}

/// Native controller methods that never touch the agent: session administration, the offline
/// model listing, settings and the protocol schema. Reads run off the controller thread; writes
/// run on it, so a later request observes an earlier write. `current` is the active session and
/// model from the worker's cached projection. `config` is replaced when `apply_settings` rebuilds.
struct Admin {
    layout: Layout,
    config: Arc<std::sync::RwLock<Config>>,
    /// Command-line settings, kept when a method reads the saved configuration again.
    overlay: toml::Table,
}

impl Admin {
    fn current(
        projection: &QueryProjection,
    ) -> (Option<String>, crate::llm::model_config::ModelSelection) {
        let state = &projection.state;
        let model = &state["current_model"];
        (
            state["session_id"].as_str().map(str::to_owned),
            crate::llm::model_config::ModelSelection {
                provider: model["provider"].as_str().unwrap_or_default().to_owned(),
                model: model["model_id"].as_str().unwrap_or_default().to_owned(),
                ..Default::default()
            },
        )
    }

    /// The configuration as saved now (with this launch's command-line settings), so a change
    /// saved by an earlier request is visible before `apply_settings`.
    fn saved_config(&self) -> Result<Config> {
        crate::config::settings_store::load_config(&self.layout, &self.overlay)
            .map_err(|error| coded(ErrorCode::InvalidParams, error))
    }

    /// Whether `method` only reads, so it may run on its own thread.
    fn reads(method: &str) -> bool {
        matches!(
            method,
            "list_sessions"
                | "list_models"
                | "settings_schema"
                | "get_settings"
                | "list_extensions"
                | "get_schema"
        )
    }

    fn handle(
        &self,
        method: &str,
        params: &Value,
        current: (Option<String>, crate::llm::model_config::ModelSelection),
    ) -> Result<Value> {
        let (session, model) = current;
        let config = self.config.read().unwrap_or_else(|p| p.into_inner());
        match method {
            "list_sessions" => sessions::list(&self.layout, session.as_deref(), params),
            "delete_session" => sessions::delete(&self.layout, session.as_deref(), params),
            "pin_session" => sessions::pin(&self.layout, params),
            "list_models" => {
                let provider = match params.get("provider").filter(|value| !value.is_null()) {
                    Some(value) => Some(
                        value
                            .as_str()
                            .filter(|provider| !provider.is_empty())
                            .context("provider must be a non-empty string")?,
                    ),
                    None => None,
                };
                let models = if model.provider.is_empty() {
                    models::list(
                        &config,
                        &config.selection(),
                        &self.layout.user_dir,
                        provider,
                    )
                } else {
                    models::list(&config, &model, &self.layout.user_dir, provider)
                };
                Ok(json!({"models":models}))
            }
            "settings_schema" => Ok(settings::schema()),
            "get_settings" => settings::get(&self.layout, params),
            "set_setting" => settings::set(&self.layout, params),
            "reset_setting" => settings::reset(&self.layout, params),
            "list_extensions" => {
                drop(config);
                Ok(extensions::list(&self.layout, &self.saved_config()?))
            }
            "get_schema" => schema_document(),
            _ => Err(unknown_method()),
        }
    }
}

fn query_snapshot(agent: &Agent<'_>, projection: &mut QueryProjection) {
    projection.state = state_projection(agent);
    refresh_messages(agent.session(), projection);
}

fn refresh_messages(session: &Session, projection: &mut QueryProjection) {
    let delta = session.message_delta(&mut projection.cursor, false);
    let messages = projection
        .messages
        .as_array_mut()
        .expect("message projection is an array");
    if delta.reset {
        messages.clear();
    }
    messages.extend(
        delta
            .messages
            .into_iter()
            .map(|message| message.into_owned()),
    );
}

/// Reply to a `state` query: the cached projection plus the live busy flag. Never includes messages.
fn state_reply(projection: &QueryProjection, busy: bool) -> Result<Value> {
    anyhow::ensure!(
        projection.state.is_object(),
        "RPC state snapshot is unavailable"
    );
    let mut state = projection.state.clone();
    state["busy"] = json!(busy);
    Ok(state)
}

/// Page the cached history without copying unrelated messages. Invalid cursors must not silently
/// restart pagination: clients otherwise cannot distinguish a malformed request from the first page.
fn messages_page(messages: &[Value], params: &Value) -> Result<Value> {
    anyhow::ensure!(
        params.is_null() || params.is_object(),
        "params must be an object"
    );
    let offset = match params.get("offset").filter(|value| !value.is_null()) {
        Some(value) => usize::try_from(
            value
                .as_u64()
                .context("offset must be an unsigned integer")?,
        )
        .context("message offset cannot be represented on this platform")?,
        None => match params.get("cursor").filter(|value| !value.is_null()) {
            Some(value) => value
                .as_str()
                .context("cursor must be a string")?
                .parse::<usize>()
                .context("invalid message cursor")?,
            None => 0,
        },
    };
    let limit = match params.get("limit").filter(|value| !value.is_null()) {
        Some(value) => value
            .as_u64()
            .context("limit must be an unsigned integer")?,
        None => MESSAGES_PAGE_DEFAULT,
    }
    .clamp(1, MESSAGES_PAGE_MAX) as usize;
    anyhow::ensure!(offset <= messages.len(), "message offset is past the end");
    let end = offset + limit.min(messages.len() - offset);
    Ok(
        json!({"messages":&messages[offset..end],"offset":offset,"total":messages.len(),
        "next_offset":(end < messages.len()).then_some(end)}),
    )
}

/// Typed `notice` event. `text` stays the authoritative plain notice; `kind`, `lane` and `tokens` are additive.
pub(crate) fn typed_notice(event: &crate::agent::ui::ui_event::UiEvent) -> Value {
    json!({"event":"notice","message":event.text,"kind":event.kind,"lane":event.kind.lane(),"tokens":event.tokens})
}
/// Live voice state of one control connection. `speech` cancels the reply being read aloud.
#[derive(Default)]
struct LiveState {
    session: Option<crate::voice::live::LiveSession>,
    speech: Option<Arc<AtomicBool>>,
}

fn live_state(live: &Mutex<LiveState>) -> std::sync::MutexGuard<'_, LiveState> {
    live.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Cancels the reply being read aloud, if any.
fn stop_speech(state: &mut LiveState) {
    if let Some(cancel) = state.speech.take() {
        cancel.store(true, Ordering::SeqCst);
    }
}

/// Reads a finished reply aloud on a detached thread while a live session is active and unmuted.
/// Playback failures are reported as `live_error` events.
fn speak_reply(live: &Mutex<LiveState>, out: &Sender, text: &str) {
    let mut state = live_state(live);
    stop_speech(&mut state);
    let Some(session) = state.session.clone() else {
        return;
    };
    if session.is_muted() {
        return;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    state.speech = Some(cancel.clone());
    drop(state);
    let text = text.to_owned();
    let out = out.clone();
    thread::spawn(move || {
        if let Err(error) = session.speak(&text, &cancel) {
            out.emit(json!({"event":"live_error","message":format!("{error:#}")}));
        }
    });
}

/// Run a persistent agent. Control requests received during inference can abort, steer or approve.
pub fn run(layout: Layout, config: Config) -> Result<()> {
    run_with_launch(layout, config, crate::agent::modes::Launch::default())
}
/// Run with process-scoped CLI restrictions, validated after provider registration.
pub fn run_with_launch(
    layout: Layout,
    config: Config,
    launch: crate::agent::modes::Launch,
) -> Result<()> {
    run_with_overlay(layout, config, launch, toml::Table::new())
}
/// [`run_with_launch`] keeping the command-line settings `overlay`, so `apply_settings` reloads
/// the saved settings with the same flags the process was launched with.
pub fn run_with_overlay(
    layout: Layout,
    config: Config,
    launch: crate::agent::modes::Launch,
    overlay: toml::Table,
) -> Result<()> {
    run_control(layout, config, false, launch, overlay)
}
pub(crate) fn run_acp(
    layout: Layout,
    config: Config,
    launch: crate::agent::modes::Launch,
) -> Result<()> {
    run_control(layout, config, true, launch, toml::Table::new())
}
fn run_control(
    layout: Layout,
    config: Config,
    acp: bool,
    launch: crate::agent::modes::Launch,
    overlay: toml::Table,
) -> Result<()> {
    config.require_model()?;
    layout.ensure_project_outside_storage()?;
    let interrupt = Arc::new(AtomicBool::new(false));
    let mut writer =
        crate::output::StdoutWriter::new().context("opening nonblocking control stdout")?;
    writer.set_interrupt(interrupt.clone());
    let out = Sender {
        output: Arc::new(Mutex::new(Output {
            acp: acp.then(|| crate::acp::Adapter::new(layout.clone())),
            protocol: DEFAULT_PROTOCOL_VERSION,
            interactive: false,
            envelope: Envelope::default(),
        })),
        writer,
        failed: Arc::new(AtomicBool::new(false)),
        failure: Arc::new(OnceLock::new()),
    };
    let busy = Arc::new(AtomicBool::new(false));
    let prompt_queued = Arc::new(AtomicBool::new(false));
    let closing = Arc::new(AtomicBool::new(false));
    let steering = Arc::new(Mutex::new(Vec::new()));
    let (commands, rx) = mpsc::sync_channel::<Value>(8);
    let (approval_tx, approval_rx) = mpsc::channel();
    let pending_approval = Arc::new(Mutex::new(None::<String>));
    let worker_pending = pending_approval.clone();
    let host_bridge = Arc::new(host_tools::HostBridge::default());
    let worker_host = host_bridge.clone();
    let uris = Arc::new(host_uris::HostUris::default());
    let worker_uris = uris.clone();
    let fs_calls = Arc::new(client_fs::ClientFsCalls::default());
    let worker_fs = fs_calls.clone();
    let auth_state = Arc::new(auth::AuthState::default());
    let auth_dir = layout.user_dir.clone();
    let (question_tx, question_rx) = mpsc::channel();
    let pending_question = Arc::new(Mutex::new(None::<PendingQuestion>));
    let worker_questions = pending_question.clone();
    let question_root = layout.project_root.clone();
    let input_disconnected = Arc::new(AtomicBool::new(false));
    let query_cache = Arc::new(Mutex::new(QueryProjection::default()));
    let worker_query_cache = query_cache.clone();
    let worker_disconnected = input_disconnected.clone();
    let (control_tx, control_rx) = mpsc::sync_channel(1);
    let live: Arc<Mutex<LiveState>> = Arc::new(Mutex::new(LiveState::default()));
    let worker_live = live.clone();
    let voice_env = crate::voice::VoiceEnv::from_config(&config);
    let worker_out = out.clone();
    let flag = interrupt.clone();
    let active = busy.clone();
    let worker_closing = closing.clone();
    let worker_prompt_queued = prompt_queued.clone();
    let admin_config = Arc::new(std::sync::RwLock::new(config.clone()));
    let admin = Arc::new(Admin {
        layout: layout.clone(),
        config: admin_config.clone(),
        overlay: overlay.clone(),
    });
    let extension_jobs = Arc::new(extensions::ExtensionJobs::default());
    let queue = steering.clone();
    let actor = actor::Actor {
        layout,
        config,
        overlay,
        acp,
        launch,
        rx,
        approval_rx,
        question_rx,
        control_tx,
        worker_out,
        flag,
        active,
        worker_closing,
        worker_prompt_queued,
        worker_pending,
        worker_host,
        worker_uris,
        worker_questions,
        worker_disconnected,
        worker_query_cache,
        worker_live,
        queue,
        admin_config,
        worker_fs,
    };
    let worker = thread::spawn(move || actor.run());
    let (mut task_control, mut maintenance_control, mut background) = match control_rx.recv() {
        Ok(controls) => controls,
        Err(_) => {
            // The actor may fail before publishing its controls. Preserve that cause instead
            // of replacing it with a closed-channel error (and dropping its fatal event).
            return match worker.join() {
                Ok(Err(error)) => Err(error.context("RPC initialization failed")),
                Ok(Ok(())) => Err(anyhow::anyhow!("RPC actor ended before initialization")),
                Err(_) => Err(anyhow::anyhow!("RPC worker panicked during initialization")),
            };
        }
    };
    // Stdin can remain open after stdout breaks. A dedicated reader lets the controller observe
    // failed output/cancellation without waiting forever for another client line.
    #[cfg(windows)]
    let stdin_reader = crate::platform::io::duplicate_stdin_reader()?;
    let (input_tx, input_rx) = mpsc::sync_channel(8);
    thread::spawn(move || {
        #[cfg(unix)]
        let stdin = std::io::stdin();
        #[cfg(unix)]
        let mut input = stdin.lock();
        #[cfg(windows)]
        let mut input = std::io::BufReader::new(stdin_reader);
        loop {
            #[cfg(unix)]
            let line = if acp {
                read_request_limit(&mut input, MAX_ACP_REQUEST)
            } else {
                read_request(&mut input)
            };
            #[cfg(windows)]
            let line = read_request_limit_with(&mut input, request_limit(acp), |input| {
                input
                    .get_mut()
                    .wait_readable(Duration::from_millis(25))
                    .map(|_| ())
            });
            let finished = !matches!(&line, Ok(Some(_)));
            // EOF is a FIFO marker, not a broken output transport. The controller must
            // consume every preceding line before beginning cancellation and cleanup.
            if input_tx.send(line).is_err() || finished {
                break;
            }
        }
    });
    let input_result = (|| -> Result<()> {
        let mut cancelling = None::<Instant>;
        loop {
            if out.unavailable() || worker.is_finished() {
                break;
            }
            // `apply_settings` rebuilds the agent and publishes its controls before it replies.
            while let Ok((tasks, maintenance, jobs)) = control_rx.try_recv() {
                (task_control, maintenance_control, background) = (tasks, maintenance, jobs);
            }
            if let Some(started) = cancelling {
                if !busy.load(Ordering::SeqCst)
                    && !task_control.list().iter().any(|(_, running)| *running)
                {
                    cancelling = None;
                } else if started.elapsed() >= CLEANUP_TIMEOUT {
                    anyhow::bail!("control cancellation did not settle within 10 seconds");
                }
            }
            let line = match input_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Ok(Some(line))) => line,
                Ok(Ok(None)) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Ok(Err(e)) => return Err(e.into()),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
            };
            if out.unavailable() {
                break;
            }
            let line = match line {
                Ok(line) => line,
                Err(message) => {
                    out.protocol_error(message, false);
                    continue;
                }
            };
            if line.is_empty() {
                continue;
            }
            let wire: Value = match serde_json::from_slice(&line) {
                Ok(v) => v,
                Err(e) => {
                    out.protocol_error(&e.to_string(), true);
                    continue;
                }
            };
            let Some(request) = out.receive(wire)? else {
                continue;
            };
            let id = request.get("id").cloned().unwrap_or(Value::Null);
            match request["method"].as_str().unwrap_or("") {
                "state" | "messages" | "messages_page" | "list_dirs" => {
                    let result = (|| -> Result<Value> {
                        let projection = query_cache.lock().unwrap_or_else(|p| p.into_inner());
                        match request["method"].as_str().unwrap_or("") {
                            "state" => state_reply(&projection, busy.load(Ordering::SeqCst)),
                            "messages" => Ok(json!({"messages":projection.messages.clone()})),
                            "list_dirs" => Ok(projection.state["workspace"].clone()),
                            _ => messages_page(
                                projection
                                    .messages
                                    .as_array()
                                    .context("RPC message snapshot is unavailable")?,
                                &request["params"],
                            )
                            .map_err(|error| coded(ErrorCode::InvalidParams, error)),
                        }
                    })();
                    out.result(&id, result);
                }
                "abort" => {
                    maintenance_control.cancel();
                    interrupt.store(true, Ordering::SeqCst);
                    for (job, running) in task_control.list() {
                        if running {
                            let _ = task_control.cancel(&job);
                        }
                    }
                    if let Ok(mut pending) = pending_approval.lock() {
                        *pending = None;
                    }
                    if let Ok(mut pending) = pending_question.lock() {
                        *pending = None;
                    }
                    if acp
                        && busy
                            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok()
                        && commands
                            .send(json!({"id":null,"method":"_cancel","params":{"_acp":true}}))
                            .is_err()
                    {
                        break;
                    }
                    cancelling = Some(Instant::now());
                    out.result(&id, Ok(json!({"requested":true})));
                }
                "steer" => {
                    let result = (|| -> Result<Value> {
                        if !busy.load(Ordering::SeqCst) {
                            return Err(coded(ErrorCode::NotPending, "no active turn"));
                        }
                        let text = request["params"]["text"].as_str().ok_or_else(|| {
                            coded(ErrorCode::InvalidParams, "steer requires params.text")
                        })?;
                        let mut q = steering
                            .lock()
                            .map_err(|_| anyhow::anyhow!("steering unavailable"))?;
                        if q.len() >= MAX_STEERING {
                            return Err(coded(ErrorCode::Busy, "steering queue full"));
                        }
                        q.push(text.to_owned());
                        maintenance_control.note_input();
                        Ok(json!({"queued":true}))
                    })();
                    out.result(&id, result);
                }
                "question_presented" => {
                    let result = (|| -> Result<Value> {
                        anyhow::ensure!(!acp, "ACP custom question input is not negotiated");
                        let correlation: crate::agent::workflow::questions::Correlation =
                            serde_json::from_value(request["params"].clone())
                                .map_err(|error| coded(ErrorCode::InvalidParams, error))?;
                        let mut pending = pending_question
                            .lock()
                            .map_err(|_| anyhow::anyhow!("question owner unavailable"))?;
                        let question = pending.as_mut().ok_or_else(|| {
                            coded(ErrorCode::NotPending, "no question is currently pending")
                        })?;
                        if !correlation.matches(&question.request) {
                            return Err(coded(
                                ErrorCode::NotPending,
                                "question id, generation or session does not match the pending question",
                            ));
                        }
                        if question.presented {
                            return Err(coded(
                                ErrorCode::NotPending,
                                "question presentation was already acknowledged",
                            ));
                        }
                        question.deadline = if question.request.timeout_seconds == 0 {
                            None
                        } else {
                            Some(Instant::now().checked_add(Duration::from_secs(question.request.timeout_seconds)).context("configured ask timeout cannot be represented by the presentation clock")?)
                        };
                        question.presented = true;
                        Ok(json!({"presented":true}))
                    })();
                    out.result(&id, result);
                }
                "question_answer" => {
                    let result = (|| -> Result<Value> {
                        anyhow::ensure!(!acp, "ACP custom question input is not negotiated");
                        let submission: crate::agent::workflow::questions::Submission =
                            serde_json::from_value(request["params"].clone())
                                .map_err(|error| coded(ErrorCode::InvalidParams, error))?;
                        let correlation = submission.correlation();
                        let question = {
                            let pending = pending_question
                                .lock()
                                .map_err(|_| anyhow::anyhow!("question owner unavailable"))?;
                            let question = pending.as_ref().ok_or_else(|| {
                                coded(ErrorCode::NotPending, "no question is currently pending")
                            })?;
                            if !correlation.matches(&question.request) {
                                return Err(coded(
                                    ErrorCode::NotPending,
                                    "question id, generation or session does not match the pending question",
                                ));
                            }
                            if !question.presented {
                                return Err(coded(
                                    ErrorCode::NotPending,
                                    "acknowledge actual question presentation before answering",
                                ));
                            }
                            if question
                                .deadline
                                .is_some_and(|deadline| Instant::now() >= deadline)
                            {
                                return Err(coded(
                                    ErrorCode::NotPending,
                                    "question presentation deadline expired",
                                ));
                            }
                            question.request.clone()
                        };
                        let outcome = submission
                            .outcome(&question, &question_root)
                            .map_err(|error| coded(ErrorCode::InvalidParams, error))?;
                        let mut pending = pending_question
                            .lock()
                            .map_err(|_| anyhow::anyhow!("question owner unavailable"))?;
                        let current = pending.as_ref().ok_or_else(|| {
                            coded(ErrorCode::NotPending, "question is no longer pending")
                        })?;
                        if !correlation.matches(&current.request) {
                            return Err(coded(
                                ErrorCode::NotPending,
                                "stale or replayed question answer",
                            ));
                        }
                        if current
                            .deadline
                            .is_some_and(|deadline| Instant::now() >= deadline)
                        {
                            return Err(coded(
                                ErrorCode::NotPending,
                                "question presentation deadline expired",
                            ));
                        }
                        question_tx.send((question.question_id, outcome))?;
                        *pending = None;
                        maintenance_control.note_input();
                        Ok(json!({"received":true}))
                    })();
                    out.result(&id, result);
                }
                "approve" => {
                    let result = (|| -> Result<Value> {
                        let approval_id =
                            request["params"]["approval_id"].as_str().ok_or_else(|| {
                                coded(ErrorCode::InvalidParams, "approval_id required")
                            })?;
                        let decision = match request["params"]["decision"].as_str() {
                            Some("allow") => Decision::Allow,
                            Some("always") => Decision::AllowAlways,
                            Some("project") => Decision::AllowProject,
                            Some("deny") => Decision::Deny,
                            _ => {
                                return Err(coded(
                                    ErrorCode::InvalidParams,
                                    "decision must be allow, always, project or deny",
                                ));
                            }
                        };
                        let mut pending = pending_approval
                            .lock()
                            .map_err(|_| anyhow::anyhow!("approval unavailable"))?;
                        if pending.as_deref() != Some(approval_id) {
                            return Err(coded(
                                ErrorCode::NotPending,
                                "approval id is not currently pending",
                            ));
                        }
                        *pending = None;
                        approval_tx.send((approval_id.to_owned(), decision))?;
                        Ok(json!({"received":true}))
                    })();
                    out.result(&id, result);
                }
                "auth_providers" => {
                    let result = (|| -> Result<Value> {
                        auth::ensure_v2(&out)?;
                        Ok(auth::providers(&auth_dir))
                    })();
                    out.result(&id, result);
                }
                "login" => {
                    let result = auth::ensure_v2(&out).and_then(|()| {
                        auth::start_login(&auth_state, &out, &auth_dir, &id, &request["params"])
                    });
                    if let Err(error) = result {
                        out.result(&id, Err(error));
                    }
                }
                "auth_code" => {
                    let result = (|| -> Result<Value> {
                        auth::ensure_v2(&out)?;
                        auth::submit_code(&auth_state, &request["params"])
                    })();
                    out.result(&id, result);
                }
                "auth_cancel" => {
                    let result = (|| -> Result<Value> {
                        auth::ensure_v2(&out)?;
                        auth::cancel_login(&auth_state, &request["params"])
                    })();
                    out.result(&id, result);
                }
                "logout" => {
                    let result = (|| -> Result<Value> {
                        auth::ensure_v2(&out)?;
                        auth::logout(&auth_state, &auth_dir, &request["params"])
                    })();
                    out.result(&id, result);
                }
                "_fs_result" if acp => {
                    // Relayed by the ACP adapter from the client's response to an `fs/*` request;
                    // a stale or unknown call is dropped, since it has no response slot.
                    let params = &request["params"];
                    if let (Some(call_id), Some(output)) =
                        (params["call_id"].as_str(), params["output"].as_str())
                    {
                        let is_error = params["is_error"].as_bool().unwrap_or(true);
                        let _ = fs_calls.complete(call_id, output.to_owned(), is_error);
                    }
                }
                "host_uri_result" => {
                    let result = (|| -> Result<Value> {
                        let call_id = request["params"]["call_id"]
                            .as_str()
                            .context("call_id required")?;
                        let output = request["params"]["output"]
                            .as_str()
                            .context("output must be a string")?;
                        let is_error = request["params"]["is_error"].as_bool().unwrap_or(false);
                        uris.complete(call_id, output.to_owned(), is_error)?;
                        Ok(json!({"received":true}))
                    })();
                    out.result(&id, result);
                }
                "host_tool_result" => {
                    let result = (|| -> Result<Value> {
                        let call_id = request["params"]["call_id"]
                            .as_str()
                            .context("call_id required")?;
                        let output = request["params"]["output"]
                            .as_str()
                            .context("output must be a string")?;
                        let is_error = request["params"]["is_error"].as_bool().unwrap_or(false);
                        host_bridge.complete(call_id, output.to_owned(), is_error)?;
                        Ok(json!({"received":true}))
                    })();
                    out.result(&id, result);
                }
                "hello" => {
                    let result = (|| -> Result<Value> {
                        let requested =
                            request["params"]["protocol_version"]
                                .as_u64()
                                .ok_or_else(|| {
                                    coded(ErrorCode::InvalidParams, "protocol_version required")
                                })?;
                        let interactive = match request["params"].get("interactive") {
                            None | Some(Value::Null) => false,
                            Some(Value::Bool(flag)) => *flag,
                            Some(_) => {
                                return Err(coded(
                                    ErrorCode::InvalidParams,
                                    "interactive must be a boolean",
                                ));
                            }
                        };
                        let version = negotiated_version(requested)?;
                        out.negotiate(version, interactive && !acp)?;
                        Ok(
                            json!({"protocol_version":version,"supported_protocol_versions":SUPPORTED_PROTOCOL_VERSIONS,"interactive":interactive && !acp}),
                        )
                    })();
                    out.result(&id, result);
                }
                "shutdown" => {
                    maintenance_control.cancel();
                    interrupt.store(true, Ordering::SeqCst);
                    if let Ok(mut pending) = pending_question.lock() {
                        *pending = None;
                    }
                    out.result(&id, Ok(json!({"closing":true})));
                    break;
                }
                "task_cancel" => {
                    let result = (|| -> Result<Value> {
                        let task_id = request["params"]["id"]
                            .as_str()
                            .context("task id required")?;
                        let result = task_control.cancel(task_id)?;
                        if let Ok(mut pending) = pending_question.lock()
                            && pending.as_ref().is_some_and(|question| {
                                question.request.owner_task_id.as_deref() == Some(task_id)
                            })
                        {
                            *pending = None;
                        }
                        Ok(result)
                    })();
                    out.result(&id, result);
                }
                "task_message" => {
                    let result = (|| -> Result<Value> {
                        let job = request["params"]["id"]
                            .as_str()
                            .context("task id required")?;
                        let text = request["params"]["text"]
                            .as_str()
                            .context("text required")?;
                        task_control.send(job, text)
                    })();
                    out.result(&id, result);
                }
                "live_start" => {
                    let result = (|| -> Result<Value> {
                        anyhow::ensure!(!acp, "live voice is not available over ACP");
                        let session = crate::voice::live::start(
                            &voice_env,
                            request["params"]["voice"].as_str(),
                            request["params"]["instructions"].as_str(),
                        )?;
                        let info =
                            json!({"live":true,"voice":session.voice(),"muted":session.is_muted()});
                        let mut state = live_state(&live);
                        stop_speech(&mut state);
                        state.session = Some(session);
                        Ok(info)
                    })();
                    out.result(&id, result);
                }
                "live_mute" => {
                    let result = (|| -> Result<Value> {
                        anyhow::ensure!(!acp, "live voice is not available over ACP");
                        let muted = request["params"]["muted"]
                            .as_bool()
                            .context("live_mute requires params.muted")?;
                        let mut state = live_state(&live);
                        state
                            .session
                            .as_ref()
                            .context("no live voice session is active")?
                            .mute(muted);
                        if muted {
                            stop_speech(&mut state);
                        }
                        Ok(json!({"muted":muted}))
                    })();
                    out.result(&id, result);
                }
                "live_stop" => {
                    let mut state = live_state(&live);
                    stop_speech(&mut state);
                    let stopped = state.session.take().is_some();
                    drop(state);
                    out.result(&id, Ok(json!({"live":false,"stopped":stopped})));
                }
                "extension_action" if !acp => {
                    let result = admin.saved_config().and_then(|config| {
                        extensions::start(
                            &extension_jobs,
                            &out,
                            &admin.layout,
                            config,
                            &id,
                            &request["params"],
                        )
                    });
                    if let Err(error) = result {
                        out.result(&id, Err(error));
                    }
                }
                "mcp_login" if !acp => {
                    let result = auth::ensure_v2(&out)
                        .and_then(|()| admin.saved_config())
                        .and_then(|config| {
                            auth::start_mcp_login(
                                &auth_state,
                                &out,
                                &config,
                                &id,
                                &request["params"],
                            )
                        });
                    if let Err(error) = result {
                        out.result(&id, Err(error));
                    }
                }
                "mcp_logout" if !acp => {
                    let result = auth::ensure_v2(&out).and_then(|()| {
                        auth::mcp_logout(&auth_state, &auth_dir, &request["params"])
                    });
                    out.result(&id, result);
                }
                "list_jobs" | "stop_job" | "list_sessions" | "delete_session" | "pin_session"
                | "list_models" | "settings_schema" | "get_settings" | "set_setting"
                | "reset_setting" | "list_extensions" | "get_schema"
                    if !acp =>
                {
                    let method = request["method"].as_str().unwrap_or_default().to_owned();
                    if matches!(method.as_str(), "list_jobs" | "stop_job") {
                        out.result(
                            &id,
                            jobs::handle(background.as_ref(), &method, &request["params"]),
                        );
                        continue;
                    }
                    let current =
                        Admin::current(&query_cache.lock().unwrap_or_else(|p| p.into_inner()));
                    if Admin::reads(&method) {
                        let (admin, out, params) =
                            (admin.clone(), out.clone(), request["params"].clone());
                        thread::spawn(move || {
                            out.result(&id, admin.handle(&method, &params, current));
                        });
                    } else {
                        out.result(&id, admin.handle(&method, &request["params"], current));
                    }
                }
                _ => {
                    // An unknown method is refused here, before it takes the busy slot: queued
                    // behind the agent it would make `state` report busy and refuse the next
                    // request. ACP methods are translated and checked in `acp.rs` first.
                    if !acp && !known_method(request["method"].as_str().unwrap_or("")) {
                        out.result(&id, Err(unknown_method()));
                        continue;
                    }
                    if acp
                        && request["method"] == "close_session"
                        && request["params"]["_active"] == true
                        && busy.load(Ordering::SeqCst)
                    {
                        if closing.swap(true, Ordering::SeqCst) {
                            out.result(
                                &id,
                                Err(coded(ErrorCode::Busy, "session is already closing")),
                            );
                            continue;
                        }
                        interrupt.store(true, Ordering::SeqCst);
                        maintenance_control.cancel();
                        for (job, running) in task_control.list() {
                            if running {
                                let _ = task_control.cancel(&job);
                            }
                        }
                        if let Ok(mut pending) = pending_approval.lock() {
                            *pending = None;
                        }
                        if let Ok(mut pending) = pending_question.lock() {
                            *pending = None;
                        }
                        cancelling = Some(Instant::now());
                        if commands.send(request).is_err() {
                            break;
                        }
                        continue;
                    }
                    if busy
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                        .is_err()
                    {
                        out.result(
                            &id,
                            Err(coded(
                                ErrorCode::Busy,
                                "agent busy; use steer, abort or approve",
                            )),
                        );
                        continue;
                    }
                    if matches!(request["method"].as_str(), Some("send" | "retry")) {
                        prompt_queued.store(true, Ordering::SeqCst);
                        maintenance_control.note_input();
                    } else if matches!(
                        request["method"].as_str(),
                        Some(
                            "compact"
                                | "memory"
                                | "new_session"
                                | "resume"
                                | "fork_session"
                                | "close_session"
                        )
                    ) {
                        maintenance_control.cancel();
                    }
                    interrupt.store(false, Ordering::SeqCst);
                    if out.unavailable() {
                        interrupt.store(true, Ordering::SeqCst);
                        prompt_queued.store(false, Ordering::SeqCst);
                        busy.store(false, Ordering::SeqCst);
                        break;
                    }
                    if commands.send(request).is_err() {
                        prompt_queued.store(false, Ordering::SeqCst);
                        busy.store(false, Ordering::SeqCst);
                        break;
                    }
                }
            }
        }
        Ok(())
    })();
    input_disconnected.store(true, Ordering::SeqCst);
    interrupt.store(true, Ordering::SeqCst);
    maintenance_control.cancel();
    for (job, running) in task_control.list() {
        if running {
            let _ = task_control.cancel(&job);
        }
    }
    if let Ok(mut pending) = pending_approval.lock() {
        *pending = None;
    }
    if let Ok(mut pending) = pending_question.lock() {
        *pending = None;
    }
    drop(commands);
    drop(approval_tx);
    drop(question_tx);
    let cleanup = Instant::now();
    while !worker.is_finished() {
        if cleanup.elapsed() >= CLEANUP_TIMEOUT {
            eprintln!(
                "control disconnect cleanup did not settle within 10 seconds; terminating owned actor"
            );
            let _ = crate::flush_stdout_on_exit();
            std::process::exit(1);
        }
        thread::sleep(Duration::from_millis(20));
    }
    let result = worker
        .join()
        .map_err(|_| anyhow::anyhow!("RPC worker panicked"))?;
    stop_speech(&mut live_state(&live));
    input_result?;
    if out.failed.load(Ordering::SeqCst) || out.writer.failed() {
        if let Some(error) = out.writer.failure() {
            return Err(error).context("control stdout failed");
        }
        anyhow::bail!(
            "{}",
            out.failure
                .get()
                .map_or("control transport disconnected", String::as_str)
        );
    }
    result?;
    Ok(())
}

#[cfg(test)]
mod tests;

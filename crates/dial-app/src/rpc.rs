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
pub const PROTOCOL_VERSION: u32 = 1;
/// Protocol versions this build serves, announced in `ready` so clients can negotiate.
const SUPPORTED_PROTOCOL_VERSIONS: &[u32] = &[PROTOCOL_VERSION];
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

/// Every request method of the plain RPC endpoint with the capability it belongs to. The `ready`
/// frame derives its method and capability lists from this table, `docs/rpc-schema.json` is
/// tested against it, and a source scan test keeps it equal to the methods the dispatchers match.
const METHODS: &[(&str, &str)] = &[
    ("send", "prompt"),
    ("state", "query"),
    ("messages", "query"),
    ("messages_page", "query"),
    ("plan", "plan_mode"),
    ("new_session", "sessions"),
    ("resume", "sessions"),
    ("rename", "sessions"),
    ("compact", "maintenance"),
    ("memory", "maintenance"),
    ("seed_context", "context_seed"),
    ("task_start", "tasks"),
    ("task_status", "tasks"),
    ("task_cancel", "tasks"),
    ("task_message", "tasks"),
    ("steer", "steering"),
    ("abort", "abort"),
    ("approve", "approvals"),
    ("question_presented", "questions"),
    ("question_answer", "questions"),
    ("live_start", "live_voice"),
    ("live_mute", "live_voice"),
    ("live_stop", "live_voice"),
    ("live_listen", "live_voice"),
    ("shutdown", "shutdown"),
];

/// Largest request line the reader accepts on this transport.
fn request_limit(acp: bool) -> usize {
    if acp { MAX_ACP_REQUEST } else { MAX_REQUEST }
}

/// Capabilities a transport lacks: ACP has no custom question input and no live voice.
fn capability_available(capability: &str, acp: bool) -> bool {
    !(acp && matches!(capability, "questions" | "live_voice"))
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
    let methods: Vec<&str> = METHODS
        .iter()
        .filter(|(_, capability)| capability_available(capability, acp))
        .map(|(method, _)| *method)
        .collect();
    let mut capabilities: Vec<&str> = Vec::new();
    for (_, capability) in METHODS {
        if capability_available(capability, acp) && !capabilities.contains(capability) {
            capabilities.push(capability);
        }
    }
    let question_input = if acp {
        json!({"supported":false,"reason":"ACP custom input capability was not negotiated"})
    } else {
        json!({"supported":true,"protocol":1,"presentation_ack_required":true,"answer_method":"question_answer","presentation_method":"question_presented","images":"explicit_user_paths","preview":"text"})
    };
    json!({
        "event":"ready",
        "protocol_version":PROTOCOL_VERSION,
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
    fn emit(&self, value: Value) -> bool {
        let result = (|| -> Result<bool> {
            let (ticket, batch) = {
                let mut output = self.mapping()?;
                let batch = match &mut output.acp {
                    Some(acp) => {
                        let values = acp.emit(value);
                        if values.is_empty() {
                            return Ok(false);
                        }
                        Batch::Mapped(values)
                    }
                    None => Batch::Single(value),
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
    fn result(&self, id: &Value, result: Result<Value>) {
        self.emit(match result {
            Ok(v) => json!({"id":id,"result":v}),
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
        self.out.emit(json!({"event":"tool_result","call_id":call_id,"name":name,"text":crate::agent::safety::guard::clip_middle(&output.text,TOOL_RESULT_EVENT_BYTES),"is_error":output.is_error,"data":output.data}));
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
    fn tool_timing(&mut self, name: &str, seconds: f64) {
        self.out
            .emit(json!({"event":"tool_timing","name":name,"seconds":seconds}));
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
    json!({"session_id":agent.session().id(),"title":agent.session().title(),"tools":agent.tool_names(),
        "plan_mode":agent.plan_mode(),"cost":agent.session_cost(),
        "cost_note":"observed priced session spend, not an invoice total; usage has explicit unknown-cost coverage",
        "observed_cost":usage_total.all.observed_cost_usd,"usage":{"total":usage_total,"turn":usage_turn},
        "metrics":agent.metrics(),"efficiency":agent.efficiency_report(),"speculation":agent.speculation_stats(),"tree":agent.session().execution_tree()})
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
    run_control(layout, config, false, launch)
}
pub(crate) fn run_acp(
    layout: Layout,
    config: Config,
    launch: crate::agent::modes::Launch,
) -> Result<()> {
    run_control(layout, config, true, launch)
}
fn run_control(
    layout: Layout,
    config: Config,
    acp: bool,
    launch: crate::agent::modes::Launch,
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
    let queue = steering.clone();
    let actor = actor::Actor {
        layout,
        config,
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
        worker_questions,
        worker_disconnected,
        worker_query_cache,
        worker_live,
        queue,
    };
    let worker = thread::spawn(move || actor.run());
    let (task_control, maintenance_control) = match control_rx.recv() {
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
                "state" | "messages" | "messages_page" => {
                    let result = (|| -> Result<Value> {
                        let projection = query_cache.lock().unwrap_or_else(|p| p.into_inner());
                        match request["method"].as_str().unwrap_or("") {
                            "state" => state_reply(&projection, busy.load(Ordering::SeqCst)),
                            "messages" => Ok(json!({"messages":projection.messages.clone()})),
                            _ => messages_page(
                                projection
                                    .messages
                                    .as_array()
                                    .context("RPC message snapshot is unavailable")?,
                                &request["params"],
                            ),
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
                        anyhow::ensure!(busy.load(Ordering::SeqCst), "no active turn");
                        let text = request["params"]["text"]
                            .as_str()
                            .context("steer requires params.text")?;
                        let mut q = steering
                            .lock()
                            .map_err(|_| anyhow::anyhow!("steering unavailable"))?;
                        anyhow::ensure!(q.len() < MAX_STEERING, "steering queue full");
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
                            serde_json::from_value(request["params"].clone())?;
                        let mut pending = pending_question
                            .lock()
                            .map_err(|_| anyhow::anyhow!("question owner unavailable"))?;
                        let question = pending
                            .as_mut()
                            .context("no question is currently pending")?;
                        anyhow::ensure!(
                            correlation.matches(&question.request),
                            "question id, generation or session does not match the pending question"
                        );
                        anyhow::ensure!(
                            !question.presented,
                            "question presentation was already acknowledged"
                        );
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
                            serde_json::from_value(request["params"].clone())?;
                        let correlation = submission.correlation();
                        let question = {
                            let pending = pending_question
                                .lock()
                                .map_err(|_| anyhow::anyhow!("question owner unavailable"))?;
                            let question = pending
                                .as_ref()
                                .context("no question is currently pending")?;
                            anyhow::ensure!(
                                correlation.matches(&question.request),
                                "question id, generation or session does not match the pending question"
                            );
                            anyhow::ensure!(
                                question.presented,
                                "acknowledge actual question presentation before answering"
                            );
                            anyhow::ensure!(
                                !question
                                    .deadline
                                    .is_some_and(|deadline| Instant::now() >= deadline),
                                "question presentation deadline expired"
                            );
                            question.request.clone()
                        };
                        let outcome = submission
                            .outcome(&question, &question_root)
                            .map_err(anyhow::Error::msg)?;
                        let mut pending = pending_question
                            .lock()
                            .map_err(|_| anyhow::anyhow!("question owner unavailable"))?;
                        let current = pending.as_ref().context("question is no longer pending")?;
                        anyhow::ensure!(
                            correlation.matches(&current.request),
                            "stale or replayed question answer"
                        );
                        anyhow::ensure!(
                            !current
                                .deadline
                                .is_some_and(|deadline| Instant::now() >= deadline),
                            "question presentation deadline expired"
                        );
                        question_tx.send((question.question_id, outcome))?;
                        *pending = None;
                        maintenance_control.note_input();
                        Ok(json!({"received":true}))
                    })();
                    out.result(&id, result);
                }
                "approve" => {
                    let result = (|| -> Result<Value> {
                        let approval_id = request["params"]["approval_id"]
                            .as_str()
                            .context("approval_id required")?;
                        let decision = match request["params"]["decision"].as_str() {
                            Some("allow") => Decision::Allow,
                            Some("always") => Decision::AllowAlways,
                            Some("project") => Decision::AllowProject,
                            Some("deny") => Decision::Deny,
                            _ => anyhow::bail!("decision must be allow, always or deny"),
                        };
                        let mut pending = pending_approval
                            .lock()
                            .map_err(|_| anyhow::anyhow!("approval unavailable"))?;
                        anyhow::ensure!(
                            pending.as_deref() == Some(approval_id),
                            "approval id is not currently pending"
                        );
                        *pending = None;
                        approval_tx.send((approval_id.to_owned(), decision))?;
                        Ok(json!({"received":true}))
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
                _ => {
                    if acp
                        && request["method"] == "close_session"
                        && request["params"]["_active"] == true
                        && busy.load(Ordering::SeqCst)
                    {
                        if closing.swap(true, Ordering::SeqCst) {
                            out.result(&id, Err(anyhow::anyhow!("session is already closing")));
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
                            Err(anyhow::anyhow!("agent busy; use steer, abort or approve")),
                        );
                        continue;
                    }
                    if request["method"] == "send" {
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

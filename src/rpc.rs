//! Versioned newline-delimited JSON control protocol. Stdin is control, stdout is JSON only.
use crate::{
    agent::{Agent, Decision, Ui, make_backend, todo::TodoList},
    config::types::Config,
    storage::{
        paths::Layout,
        session::{Session, SessionMeta},
    },
    tools::ToolOutput,
};
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

pub const PROTOCOL_VERSION: u32 = 1;
const MAX_REQUEST: usize = 1024 * 1024;
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
    request: crate::agent::questions::QuestionRequest,
    presented: bool,
    deadline: Option<Instant>,
}
struct RpcUi {
    out: Sender,
    approvals: mpsc::Receiver<(String, Decision)>,
    pending_approval: Arc<Mutex<Option<String>>>,
    question_answers: mpsc::Receiver<(String, crate::agent::questions::QuestionOutcome)>,
    pending_question: Arc<Mutex<Option<PendingQuestion>>>,
    interrupt: Arc<AtomicBool>,
    input_disconnected: Arc<AtomicBool>,
    steering: Arc<Mutex<Vec<String>>>,
    prompt_queued: Arc<AtomicBool>,
    next: u64,
    node_cursors: std::collections::BTreeMap<String, usize>,
    node_revisions: std::collections::BTreeMap<String, u64>,
    tree: Option<crate::runtime::Tree>,
}
impl RpcUi {
    fn request_approval(
        &mut self,
        tool: &str,
        summary: &str,
        cancel: Option<&AtomicBool>,
    ) -> Decision {
        self.next += 1;
        let approval_id = format!("approval-{}", self.next);
        if let Ok(mut pending) = self.pending_approval.lock() {
            *pending = Some(approval_id.clone());
        } else {
            return Decision::Deny;
        }
        let delivered = self.out.emit(
            json!({"event":"approval","approval_id":approval_id,"tool":tool,"summary":summary}),
        );
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
        if let Ok(mut pending) = self.pending_approval.lock()
            && pending.as_deref() == Some(&approval_id)
        {
            *pending = None;
        }
        decision
    }
}
impl Ui for RpcUi {
    fn runtime(&mut self, tree: &crate::runtime::Tree) {
        if self.tree.as_ref().is_none_or(|old| !old.same(tree)) {
            self.node_cursors.clear();
            self.node_revisions.clear();
            self.tree = Some(tree.clone());
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
            let bytes=self.node_cursors.insert(node.id.clone(),node.result.text.len());
            if node.kind != "worker" && node.result.phase==crate::runtime::Phase::Running && let Some(bytes)=bytes
                && bytes<=node.result.text.len() && node.result.text.is_char_boundary(bytes) {
                self.out.emit(json!({"event":"state_delta","id":node.id,"revision":node.result.revision,"append":&node.result.text[bytes..]}));
            } else {self.out.emit(json!({"event":"state_node","node":node}));}
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
        self.out.emit(json!({"event":"tool_result","call_id":call_id,"name":name,"text":crate::agent::guard::clip_middle(&output.text,24*1024),"is_error":output.is_error,"data":output.data}));
    }
    fn notice(&mut self, message: &str) {
        self.out.emit(json!({"event":"notice","message":message}));
    }
    /// Same `notice` event clients already read, plus the typed kind and lane for richer clients.
    fn event(&mut self, event: &crate::agent::ui_event::UiEvent) {
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
    fn approve(&mut self, tool: &str, summary: &str) -> Decision {
        self.request_approval(tool, summary, None)
    }
    fn approve_with_cancel(&mut self, tool: &str, summary: &str, cancel: &AtomicBool) -> Decision {
        self.request_approval(tool, summary, Some(cancel))
    }
    fn ask_questions(
        &mut self,
        request: &crate::agent::questions::QuestionRequest,
        cancel: &AtomicBool,
    ) -> crate::agent::questions::QuestionOutcome {
        use crate::agent::questions::{self, QuestionOutcome};
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
            if bytes.len().saturating_add(used) > limit + 1 {
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

fn park_session(
    agent: &mut Agent<'_>,
    session: Session,
    parked: &mut std::collections::HashMap<String, Session>,
    parked_tools: &mut std::collections::HashMap<String, crate::tools::ext::Registry>,
    modes: &mut std::collections::HashMap<String, bool>,
    current: bool,
) {
    let previous_id = agent.session().id().to_owned();
    let plan = agent.plan_mode();
    let next_plan = modes.get(session.id()).copied().unwrap_or(false);
    let tools = parked_tools
        .remove(session.id())
        .unwrap_or_else(|| agent.fresh_session_tools());
    let previous_tools = agent.replace_session_tools(tools);
    let previous = agent.swap_session(session);
    agent.set_plan_mode(next_plan);
    if current {
        modes.insert(previous_id.clone(), plan);
        parked_tools.insert(previous_id.clone(), previous_tools);
        parked.insert(previous_id, previous);
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
}

impl Default for QueryProjection {
    fn default() -> Self {
        Self {
            state: Value::Null,
            messages: json!([]),
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

fn query_snapshot(agent: &Agent<'_>) -> QueryProjection {
    QueryProjection {
        state: state_projection(agent),
        messages: json!(agent.session().messages()),
    }
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
        None => 50,
    }
    .clamp(1, 500) as usize;
    anyhow::ensure!(offset <= messages.len(), "message offset is past the end");
    let end = offset + limit.min(messages.len() - offset);
    Ok(
        json!({"messages":&messages[offset..end],"offset":offset,"total":messages.len(),
        "next_offset":(end < messages.len()).then_some(end)}),
    )
}

/// Typed `notice` event. `text` stays the authoritative plain notice; `kind`, `lane` and `tokens` are additive.
pub(crate) fn typed_notice(event: &crate::agent::ui_event::UiEvent) -> Value {
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
    run_control(layout, config, false)
}
pub(crate) fn run_acp(layout: Layout, config: Config) -> Result<()> {
    run_control(layout, config, true)
}
fn run_control(layout: Layout, config: Config, acp: bool) -> Result<()> {
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
    let worker = thread::spawn(move || -> Result<()> {
        let outcome = (|| -> Result<()> {
            let mut compute = None;
            let backend = make_backend(&config)?;
            let image_input = backend.accepts_image_input();
            let session = if acp {
                Session::ephemeral()
            } else {
                Session::create(
                    &layout,
                    SessionMeta {
                        model: config.model.clone(),
                    },
                )?
            };
            let mut agent = Agent::interactive(
                &config,
                backend,
                layout.clone(),
                session,
                None,
                None,
                flag.clone(),
            )?;
            agent.begin_session_lifecycle();
            if let Some(path) = std::env::var_os("DIAL_BENCHMARK_ADAPTER") {
                agent.set_benchmark_adapter(std::path::Path::new(&path))?;
            }
            *worker_query_cache.lock().unwrap_or_else(|p| p.into_inner()) = query_snapshot(&agent);
            let mut ui = RpcUi {
                out: worker_out.clone(),
                approvals: approval_rx,
                pending_approval: worker_pending,
                question_answers: question_rx,
                pending_question: worker_questions,
                interrupt: flag.clone(),
                input_disconnected: worker_disconnected,
                steering: queue,
                prompt_queued: worker_prompt_queued.clone(),
                next: 0,
                node_cursors: Default::default(),
                node_revisions: Default::default(),
                tree: None,
            };
            worker_out.emit(json!({"event":"ready","protocol_version":PROTOCOL_VERSION,"session_id":agent.session().id(),"model":config.model,"tools":agent.tool_names(),"image_input":image_input,
                "question_input":if acp { json!({"supported":false,"reason":"ACP custom input capability was not negotiated"}) } else { json!({"supported":true,"protocol":1,"presentation_ack_required":true,"answer_method":"question_answer","presentation_method":"question_presented","images":"explicit_user_paths","preview":"text"}) }}));
            let _ = control_tx.send((
                agent.live_task_control(),
                agent.background_compaction_control(),
            ));
            let mut parked = std::collections::HashMap::<String, Session>::new();
            let mut parked_tools =
                std::collections::HashMap::<String, crate::tools::ext::Registry>::new();
            let mut modes = std::collections::HashMap::<String, bool>::new();
            let mut current = !acp;
            loop {
                if worker_out.unavailable() {
                    break;
                }
                let request = match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(request) => request,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        agent.pump_subagents(&mut ui);
                        if current {
                            let queued = worker_prompt_queued.load(Ordering::SeqCst);
                            let foreground =
                                config.compute_locks && agent.foreground_idle_pending(queued)?;
                            let mut ready = !foreground || compute.is_some();
                            if foreground && compute.is_none() {
                                let lock = crate::learn::auto::lock_file(
                                    &layout.user_dir.join("compute.lock"),
                                )?;
                                match fs2::FileExt::try_lock_exclusive(&lock) {
                                    Ok(()) => {
                                        crate::learn::compute::register(
                                            &layout.user_dir,
                                            "session",
                                            &layout.project_root,
                                            &config.state_dir,
                                            &config.model,
                                        )?;
                                        compute = Some(lock);
                                        ready = true;
                                    }
                                    Err(error)
                                        if error.kind() == std::io::ErrorKind::WouldBlock => {}
                                    Err(error) => return Err(error.into()),
                                }
                            }
                            if ready && let Err(error) = agent.maintenance_tick(&mut ui, queued) {
                                ui.notice(&format!("Idle maintenance did not commit: {error:#}"));
                            }
                        }
                        if compute.is_some()
                            && (crate::learn::compute::background_pending(&layout.user_dir)?
                                || crate::learn::compute::foreground_pending(&layout.user_dir)?)
                            && agent.release_idle_models()
                            && let Some(lock) = compute.take()
                        {
                            crate::learn::compute::clear_owner(&layout.user_dir);
                            fs2::FileExt::unlock(&lock)?;
                        }
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                if worker_out.unavailable() {
                    break;
                }
                if matches!(request["method"].as_str(), Some("send" | "live_listen")) {
                    worker_prompt_queued.store(false, Ordering::SeqCst);
                    agent.note_compaction_input();
                }
                let id = request.get("id").cloned().unwrap_or(Value::Null);
                let params = &request["params"];
                let memory_compute = request["method"] == "memory"
                    && config.memory_uses_local_compute()
                    && !matches!(
                        params["command"]
                            .as_str()
                            .unwrap_or("status")
                            .split_whitespace()
                            .next(),
                        None | Some("status" | "queue")
                    );
                if compute.is_none()
                    && config.compute_locks
                    && (memory_compute
                        || (config.requires_compute_lock()
                            && matches!(
                                request["method"].as_str(),
                                Some("send" | "live_listen" | "compact" | "task_start")
                            )))
                {
                    compute = Some(crate::learn::auto::compute_lock(&layout)?);
                    crate::learn::compute::register(
                        &layout.user_dir,
                        "session",
                        &layout.project_root,
                        &config.state_dir,
                        &config.model,
                    )?;
                }
                let result = (|| -> Result<Value> {
                    let method = request["method"].as_str().unwrap_or("");
                    if acp
                        && !matches!(
                            method,
                            "new_session"
                                | "resume"
                                | "fork_session"
                                | "list_sessions"
                                | "close_session"
                                | "_cancel"
                        )
                    {
                        let wanted = params["session_id"]
                            .as_str()
                            .context("session_id required")?;
                        anyhow::ensure!(
                            current || parked.contains_key(wanted),
                            "session is not open"
                        );
                        if !current || agent.session().id() != wanted {
                            stop_owned_tasks(&mut agent, &mut ui);
                            let session = parked.remove(wanted).context("session is not open")?;
                            park_session(
                                &mut agent,
                                session,
                                &mut parked,
                                &mut parked_tools,
                                &mut modes,
                                current,
                            );
                            current = true;
                        }
                        worker_out.emit(
                            json!({"event":"active_session","session_id":agent.session().id()}),
                        );
                    }
                    match request["method"].as_str().unwrap_or("") {
                        "send" | "live_listen" => {
                            let (spoken, transcript) = if request["method"] == "live_listen" {
                                let session = live_state(&worker_live)
                                    .session
                                    .clone()
                                    .context("no live voice session is active")?;
                                let heard = session.transcribe(&flag)?;
                                (heard.clone(), Some(heard))
                            } else {
                                let text = params["text"]
                                    .as_str()
                                    .context("send requires params.text")?;
                                (text.to_owned(), None)
                            };
                            let started = Instant::now();
                            let prompt = {
                                let mut state = live_state(&worker_live);
                                stop_speech(&mut state);
                                state
                                    .session
                                    .as_ref()
                                    .map(|session| session.prompt_for(&spoken))
                            };
                            let text = prompt.unwrap_or(spoken);
                            let turn = if acp {
                                agent.turn_prepared_preserving_interrupt(
                                    &text,
                                    params["_message"].clone(),
                                    &mut ui,
                                )
                            } else {
                                agent.turn_preserving_interrupt(&text, &mut ui)
                            };
                            let report = match turn {
                                Ok(report) => report,
                                Err(_) if acp && flag.load(Ordering::SeqCst) => {
                                    return Ok(
                                        json!({"interrupted":true,"plan_mode":agent.plan_mode()}),
                                    );
                                }
                                Err(error) => return Err(error),
                            };
                            if !report.interrupted {
                                speak_reply(&worker_live, &worker_out, &report.text);
                            }
                            let mut result = json!({"text":report.text,"outcome":report.outcome,"steps":report.steps,"interrupted":report.interrupted,"duration_ms":started.elapsed().as_millis(),"session_id":agent.session().id(),"plan_mode":agent.plan_mode(),"metrics":agent.metrics(),"efficiency":agent.efficiency_report(),"speculation":agent.speculation_stats()});
                            if let Some(transcript) = transcript {
                                result["transcript"] = json!(transcript);
                            }
                            Ok(result)
                        }
                        "state" => Ok(state_projection(&agent)),
                        "plan" => {
                            agent.set_plan_mode(
                                params["enabled"]
                                    .as_bool()
                                    .context("plan requires params.enabled")?,
                            );
                            Ok(json!({"enabled":agent.plan_mode()}))
                        }
                        "new_session" => {
                            let session = Session::create(
                                &layout,
                                SessionMeta {
                                    model: config.model.clone(),
                                },
                            )?;
                            if acp {
                                stop_owned_tasks(&mut agent, &mut ui);
                                park_session(
                                    &mut agent,
                                    session,
                                    &mut parked,
                                    &mut parked_tools,
                                    &mut modes,
                                    current,
                                );
                                agent.set_plan_mode(false);
                                current = true;
                            } else {
                                agent.swap_session(session);
                                agent.set_plan_mode(false);
                            }
                            Ok(
                                json!({"session_id":agent.session().id(),"plan_mode":agent.plan_mode()}),
                            )
                        }
                        "compact" => {
                            let mut options: crate::agent::compaction::CompactionOptions =
                                if params.is_null() {
                                    Default::default()
                                } else {
                                    serde_json::from_value(params.clone())
                                        .context("invalid compact options")?
                                };
                            anyhow::ensure!(
                                options.reason
                                    == crate::agent::compaction::CompactionReason::Manual,
                                "RPC compact accepts only manual maintenance; automatic reasons are owned by the actor"
                            );
                            options.suppress_continuation = true;
                            let outcome = agent.compact_with_options(&mut ui, options)?;
                            Ok(json!({
                                "compacted":outcome.kind == crate::agent::compaction::CompactionOutcomeKind::Compaction,
                                "context_reset":outcome.kind == crate::agent::compaction::CompactionOutcomeKind::NewContext,
                                "outcome":outcome
                            }))
                        }
                        "memory" => {
                            let command = params["command"]
                                .as_str()
                                .context("memory requires params.command")?;
                            anyhow::ensure!(
                                command.len() <= 8192,
                                "memory command exceeds 8192 bytes"
                            );
                            agent.selected_memory_command(command, &mut ui)
                        }
                        "seed_context" => {
                            let messages = params["messages"]
                                .as_array()
                                .context("seed_context requires params.messages")?;
                            anyhow::ensure!(messages.len() <= 200, "at most 200 seed messages");
                            for text in messages {
                                let text =
                                    text.as_str().context("context entries must be strings")?;
                                anyhow::ensure!(
                                    text.len() <= 1024 * 1024,
                                    "context entry exceeds 1 MiB"
                                );
                                agent
                                    .session_mut()
                                    .append(json!({"role":"user","content":text}))?;
                            }
                            Ok(json!({"seeded":messages.len()}))
                        }
                        "resume" => {
                            let wanted = params["session_id"]
                                .as_str()
                                .context("resume requires params.session_id")?;
                            if acp {
                                if !current || agent.session().id() != wanted {
                                    let session = if let Some(session) = parked.remove(wanted) {
                                        session
                                    } else {
                                        let info = Session::find_prefix(&layout, wanted)?
                                            .into_iter()
                                            .find(|s| s.id == wanted)
                                            .context("session id not found in launched project")?;
                                        Session::open(&info.path)?
                                    };
                                    stop_owned_tasks(&mut agent, &mut ui);
                                    park_session(
                                        &mut agent,
                                        session,
                                        &mut parked,
                                        &mut parked_tools,
                                        &mut modes,
                                        current,
                                    );
                                    current = true;
                                }
                                if params["_replay"] == true {
                                    worker_out.emit(json!({"event":"active_session","session_id":agent.session().id()}));
                                    for message in agent.session().messages() {
                                        if flag.load(Ordering::SeqCst)
                                            || worker_out.failed.load(Ordering::SeqCst)
                                        {
                                            break;
                                        }
                                        worker_out.emit(
                                            json!({"event":"replay_message","message":message}),
                                        );
                                    }
                                }
                                Ok(
                                    json!({"session_id":agent.session().id(),"plan_mode":agent.plan_mode()}),
                                )
                            } else {
                                let matching = Session::find_prefix(&layout, wanted)?;
                                anyhow::ensure!(
                                    matching.len() == 1,
                                    "session id missing or ambiguous"
                                );
                                if agent.session().id() != matching[0].id {
                                    agent.swap_session(Session::open(&matching[0].path)?);
                                    agent.set_plan_mode(false);
                                }
                                Ok(json!({"session_id":agent.session().id()}))
                            }
                        }
                        "fork_session" if acp => {
                            let wanted = params["session_id"]
                                .as_str()
                                .context("session_id required")?;
                            let session = if current && agent.session().id() == wanted {
                                stop_owned_tasks(&mut agent, &mut ui);
                                agent.session().fork(&layout)?
                            } else if let Some(session) = parked.get(wanted) {
                                session.fork(&layout)?
                            } else {
                                let info = Session::find_prefix(&layout, wanted)?
                                    .into_iter()
                                    .find(|s| s.id == wanted)
                                    .context("session id not found in launched project")?;
                                Session::read(&info.path)?.fork(&layout)?
                            };
                            if !current || agent.session().id() != wanted {
                                stop_owned_tasks(&mut agent, &mut ui);
                            }
                            park_session(
                                &mut agent,
                                session,
                                &mut parked,
                                &mut parked_tools,
                                &mut modes,
                                current,
                            );
                            agent.set_plan_mode(false);
                            current = true;
                            Ok(
                                json!({"session_id":agent.session().id(),"plan_mode":agent.plan_mode()}),
                            )
                        }
                        "close_session" if acp => {
                            let wanted = params["session_id"]
                                .as_str()
                                .context("session_id required")?;
                            if current && agent.session().id() == wanted {
                                stop_owned_tasks(&mut agent, &mut ui);
                                let tools = agent.fresh_session_tools();
                                agent.replace_session_tools(tools);
                                agent.swap_session(Session::ephemeral());
                                agent.set_plan_mode(false);
                                current = false;
                            } else {
                                parked.remove(wanted).context("session is not open")?;
                            }
                            parked_tools.remove(wanted);
                            modes.remove(wanted);
                            Ok(json!({}))
                        }
                        "list_sessions" if acp => {
                            let offset = match params["cursor"].as_str() {
                                Some(cursor) => {
                                    cursor.parse::<usize>().context("invalid session cursor")?
                                }
                                None => 0,
                            };
                            // One lookahead row tells whether a next page exists without loading the whole history.
                            let sessions = Session::list(&layout, offset.saturating_add(101))?;
                            anyhow::ensure!(
                                offset <= sessions.len(),
                                "session cursor is past the end"
                            );
                            let page:Vec<_> = sessions.iter().skip(offset).take(100).map(|s|
                                json!({"sessionId":s.id,"cwd":layout.project_root,"title":s.title})).collect();
                            let next = offset + page.len();
                            let mut result = json!({"sessions":page});
                            if next < sessions.len() {
                                result["nextCursor"] = json!(next.to_string());
                            }
                            Ok(result)
                        }
                        "messages" => Ok(json!({"messages":agent.session().messages()})),
                        "rename" => {
                            agent.session_mut().rename(
                                params["title"]
                                    .as_str()
                                    .context("rename requires params.title")?,
                            )?;
                            Ok(json!({"title":agent.session().title()}))
                        }
                        "task_start" => agent.start_subagents(params, &mut ui),
                        "_cancel" if acp => Ok(json!({})),
                        "task_status" => agent.subagent_status(params["id"].as_str(), &mut ui),
                        _ => anyhow::bail!("unknown RPC method"),
                    }
                })();
                if acp && current && flag.load(Ordering::SeqCst) {
                    stop_owned_tasks(&mut agent, &mut ui);
                    let tools = agent.fresh_session_tools();
                    agent.replace_session_tools(tools);
                }
                if request["method"] == "close_session" {
                    worker_closing.store(false, Ordering::SeqCst);
                }
                *worker_query_cache.lock().unwrap_or_else(|p| p.into_inner()) =
                    query_snapshot(&agent);
                if !worker_closing.load(Ordering::SeqCst) {
                    active.store(false, Ordering::SeqCst);
                }
                worker_out.result(&id, result);
                if worker_out.failed.load(Ordering::SeqCst) {
                    break;
                }
            }
            Ok(())
        })();
        if let Err(error) = &outcome {
            worker_out.emit(json!({"event":"fatal","message":format!("{error:#}")}));
        }
        outcome
    });
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
    let input_cancel = out.writer.cancellation();
    let input_failed = out.failed.clone();
    let input_failure = out.failure.clone();
    let reader_maintenance = maintenance_control.clone();
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
                read_request_limit(&mut input, 8 * MAX_REQUEST)
            } else {
                read_request(&mut input)
            };
            #[cfg(windows)]
            let line = read_request_limit_with(
                &mut input,
                if acp { 8 * MAX_REQUEST } else { MAX_REQUEST },
                |input| {
                    input
                        .get_mut()
                        .wait_readable(Duration::from_millis(25))
                        .map(|_| ())
                },
            );
            let finished = !matches!(&line, Ok(Some(_)));
            if finished {
                reader_maintenance.cancel();
                if let Err(error) = &line {
                    let _ = input_failure.set(format!("control stdin failed: {error}"));
                    input_failed.store(true, Ordering::SeqCst);
                }
                input_cancel.disconnect();
            }
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
                    {
                        if commands
                            .send(json!({"id":null,"method":"_cancel","params":{"_acp":true}}))
                            .is_err()
                        {
                            break;
                        }
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
                        anyhow::ensure!(q.len() < 8, "steering queue full");
                        q.push(text.to_owned());
                        maintenance_control.note_input();
                        Ok(json!({"queued":true}))
                    })();
                    out.result(&id, result);
                }
                "question_presented" => {
                    let result = (|| -> Result<Value> {
                        anyhow::ensure!(!acp, "ACP custom question input is not negotiated");
                        let correlation: crate::agent::questions::Correlation =
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
                        let submission: crate::agent::questions::Submission =
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
mod tests {
    use super::*;
    #[test]
    fn oversized_request_is_drained_without_consuming_the_next_request() {
        let mut bytes = vec![b'x'; MAX_REQUEST + 100];
        bytes.extend_from_slice(b"\n{\"id\":1,\"method\":\"state\"}\n");
        let mut input = std::io::BufReader::with_capacity(32, std::io::Cursor::new(bytes));
        assert!(read_request(&mut input).unwrap().unwrap().is_err());
        let next = read_request(&mut input).unwrap().unwrap().unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&next).unwrap()["method"],
            "state"
        );
        assert!(read_request(&mut input).unwrap().is_none());
    }
    #[test]
    fn complete_request_without_final_newline_is_accepted() {
        let mut input = std::io::Cursor::new(b"{\"id\":2,\"method\":\"state\"}");
        let next = read_request(&mut input).unwrap().unwrap().unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&next).unwrap()["id"], 2);
    }
    #[test]
    fn state_reply_reads_only_the_state_projection() {
        let projection = QueryProjection {
            state: json!({"session_id":"s1","tree":[]}),
            messages: json!([{"role":"user","content":"large history"}]),
        };
        let reply = state_reply(&projection, true).unwrap();
        assert_eq!(reply, json!({"session_id":"s1","tree":[],"busy":true}));
        assert!(reply.get("messages").is_none());
        assert!(state_reply(&QueryProjection::default(), false).is_err());
        assert_eq!(QueryProjection::default().messages, json!([]));
    }
    #[test]
    fn history_pages_preserve_boundaries_and_reject_invalid_cursors() {
        let messages: Vec<_> = (0..600).map(|n| json!({"content":n})).collect();
        assert_eq!(
            messages_page(&messages, &Value::Null).unwrap()["next_offset"],
            50
        );
        let page = messages_page(&messages, &json!({"cursor":"599","limit":u64::MAX})).unwrap();
        assert_eq!(
            page,
            json!({"messages":[{"content":599}],"offset":599,"total":600,"next_offset":null})
        );
        assert_eq!(
            messages_page(&messages, &json!({"offset":600})).unwrap()["messages"],
            json!([])
        );
        assert_eq!(
            messages_page(&messages, &json!({"limit":0})).unwrap()["next_offset"],
            1
        );
        assert_eq!(
            messages_page(&messages, &json!({"limit":u64::MAX})).unwrap()["next_offset"],
            500
        );
        assert_eq!(
            messages_page(&messages, &json!({"offset":2,"cursor":"4"})).unwrap()["offset"],
            2
        );
        for params in [
            json!({"cursor":"bad"}),
            json!({"cursor":-1}),
            json!({"offset":-1}),
            json!({"offset":"2"}),
            json!({"offset":1.5}),
            json!({"offset":u64::MAX}),
            json!({"cursor":"18446744073709551616"}),
            json!({"limit":-1}),
            json!({"limit":"2"}),
            json!([]),
        ] {
            assert!(messages_page(&messages, &params).is_err(), "{params}");
        }
        assert_eq!(
            messages_page(&[], &json!({})).unwrap(),
            json!({"messages":[],"offset":0,"total":0,"next_offset":null})
        );
    }

    #[test]
    fn typed_notice_adds_lane_and_keeps_plain_text_and_tokens() {
        use crate::agent::ui_event::{EventKind, UiEvent};
        let value = typed_notice(
            &UiEvent::new(EventKind::Compaction, "History compacted").with_tokens(900, 120),
        );
        assert_eq!(value["event"], "notice");
        assert_eq!(value["message"], "History compacted");
        assert_eq!(value["kind"], "compaction");
        assert_eq!(value["lane"], "divider");
        assert_eq!(value["tokens"], json!([900, 120]));
        let alert = typed_notice(&UiEvent::new(EventKind::Recovery, "retrying"));
        assert_eq!(alert["lane"], "alert");
        assert!(alert["tokens"].is_null());
    }
}

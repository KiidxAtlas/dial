use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::computer::desktop::DesktopSession;
use crate::computer::desktop::types::DesktopSessionOptions;
use crate::computer::rpc::{ComputerError, ComputerResult, desktop_error, dispatch};

const START_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_TIMEOUT: Duration = Duration::from_millis(1500);
const GRACE: Duration = Duration::from_millis(750);
const CAPABILITIES_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(20);
const STDERR_TAIL_BYTES: usize = 4000;
const RESTART_MESSAGE: &str = "computer worker restarted; captures and ax refs were reset";
const WORKER_EXITED: &str = "Computer worker exited";
const NOT_RUNNING: &str = "Computer worker is not running";
const NO_SESSION: &str = "No active computer session";
const CLOSED: &str = "Computer session is closed";
const SESSION_CLOSED: &str = "Computer session closed";
const ABORTED: &str = "Computer run was aborted";
const CONTROL_TITLE: &str = "Allow foreground computer control?";
const WORKER_SOURCE: &str = include_str!("js/worker.mjs");

#[derive(Debug, Clone)]
pub struct RunRequest {
    pub code: String,
    pub timeout_ms: u64,
    pub session: Value,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct AbortSignal(Arc<AtomicBool>);

impl AbortSignal {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    #[cfg(test)]
    pub fn from_flag(flag: Arc<AtomicBool>) -> Self {
        Self(flag)
    }

    pub fn abort(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_aborted(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

pub trait ComputerHost: Send + Sync {
    fn confirm_control(
        &self,
        title: &str,
        body: &str,
        signal: &AbortSignal,
    ) -> ComputerResult<bool>;

    fn call_tool(&self, name: &str, args: Value, signal: &AbortSignal) -> ComputerResult<Value>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Idle,
    Ready,
    Failed,
    Closed,
}

struct Worker {
    generation: u64,
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: HashMap<u64, flume::Sender<ComputerResult<Value>>>,
    ready: Option<flume::Sender<ComputerResult<Value>>>,
    closed: Option<flume::Sender<ComputerResult<Value>>>,
}

struct Run {
    reply: flume::Sender<ComputerResult<Value>>,
    host: Arc<dyn ComputerHost>,
    tool_signals: HashMap<String, AbortSignal>,
}

struct State {
    status: Status,
    worker: Option<Worker>,
    session: Option<Arc<DesktopSession>>,
    runs: HashMap<u64, Run>,
    next_id: u64,
    next_gen: u64,
}

struct Inner {
    state: Mutex<State>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(worker) = self.state.get_mut().worker.take() {
            kill_child(worker.child);
        }
    }
}

pub struct ComputerSupervisor {
    inner: Arc<Inner>,
}

impl Default for ComputerSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl ComputerSupervisor {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    status: Status::Idle,
                    worker: None,
                    session: None,
                    runs: HashMap::new(),
                    next_id: 0,
                    next_gen: 0,
                }),
            }),
        }
    }

    pub fn run(
        &self,
        request: &RunRequest,
        host: Arc<dyn ComputerHost>,
        signal: &AbortSignal,
    ) -> ComputerResult<Value> {
        let inner = &self.inner;
        if signal.is_aborted() {
            return Err(ComputerError::abort(ABORTED));
        }
        ensure_worker(inner, &request.cwd)?;
        let (reply, rx) = flume::bounded(1);
        let id = {
            let mut guard = inner.state.lock();
            let st = &mut *guard;
            if st.status == Status::Closed {
                return Err(closed_error());
            }
            if st.worker.is_none() {
                return Err(not_running());
            }
            ensure_session_locked(st, &request.session)?;
            st.next_id += 1;
            let id = st.next_id;
            st.runs.insert(
                id,
                Run {
                    reply,
                    host,
                    tool_signals: HashMap::new(),
                },
            );
            id
        };

        let message = json!({
            "type": "run",
            "id": id.to_string(),
            "code": request.code,
            "timeoutMs": request.timeout_ms,
            "session": request.session,
        });
        let start = Instant::now();
        if let Err(error) = send(inner, &message) {
            inner.state.lock().runs.remove(&id);
            return Err(error);
        }

        let deadline = start + Duration::from_millis(request.timeout_ms) + GRACE;
        let mut aborted = false;
        let mut abort_deadline: Option<Instant> = None;
        let result = loop {
            match rx.recv_timeout(POLL) {
                Ok(result) => break result,
                Err(flume::RecvTimeoutError::Disconnected) => {
                    break Err(ComputerError::tool(WORKER_EXITED));
                }
                Err(flume::RecvTimeoutError::Timeout) => {}
            }
            let now = Instant::now();
            if !aborted && signal.is_aborted() {
                aborted = true;
                abort_deadline = Some(now + GRACE);
                cancel_run(inner, id);
                let _ = send(inner, &json!({"type": "abort", "id": id.to_string()}));
            }
            if now >= deadline || abort_deadline.is_some_and(|d| now >= d) {
                let error = if aborted {
                    ComputerError::abort(ABORTED)
                } else {
                    ComputerError::tool(RESTART_MESSAGE)
                };
                terminate(inner, error.clone());
                break Err(error);
            }
        };

        let session = {
            let mut guard = inner.state.lock();
            guard.runs.remove(&id);
            guard.session.clone()
        };
        if let Some(session) = session {
            if result.is_ok() && !aborted {
                session.retire();
            } else {
                session.cancel();
            }
        }
        result
    }

    pub fn capabilities(
        &self,
        session: &Value,
        cwd: &Path,
        signal: &AbortSignal,
    ) -> ComputerResult<Value> {
        let inner = &self.inner;
        if signal.is_aborted() {
            return Err(ComputerError::abort(ABORTED));
        }
        ensure_worker(inner, cwd)?;
        {
            let mut guard = inner.state.lock();
            if guard.status == Status::Closed {
                return Err(closed_error());
            }
            ensure_session_locked(&mut *guard, session)?;
        }
        let (id, rx) = register_pending(inner)?;
        if let Err(error) = send(
            inner,
            &json!({"type": "capabilities", "id": id.to_string(), "session": session}),
        ) {
            forget_pending(inner, id);
            return Err(error);
        }
        await_reply(
            inner,
            Some(id),
            rx,
            Instant::now() + CAPABILITIES_TIMEOUT,
            Some(signal),
            "Timed out fetching computer capabilities",
        )
    }

    pub fn close(&self) -> ComputerResult<()> {
        let inner = &self.inner;
        let (closed_tx, closed_rx) = flume::bounded(1);
        let (runs, session, worker_running) = {
            let mut guard = inner.state.lock();
            let st = &mut *guard;
            if st.status == Status::Closed {
                return Ok(());
            }
            st.status = Status::Closed;
            let runs: Vec<Run> = st.runs.drain().map(|(_, run)| run).collect();
            let session = st.session.take();
            let worker_running = match st.worker.as_mut() {
                Some(worker) => {
                    worker.closed = Some(closed_tx);
                    true
                }
                None => false,
            };
            (runs, session, worker_running)
        };
        for run in runs {
            for signal in run.tool_signals.values() {
                signal.abort();
            }
            let _ = run.reply.send(Err(ComputerError::abort(SESSION_CLOSED)));
        }

        let mut first_error: Option<ComputerError> = None;
        if worker_running && send(inner, &json!({"type": "close"})).is_ok() {
            if let Err(error) = await_reply(
                inner,
                None,
                closed_rx,
                Instant::now() + CLOSE_TIMEOUT,
                None,
                "Timed out closing computer worker",
            ) {
                if error.message != WORKER_EXITED {
                    first_error = Some(error);
                }
            }
        }

        if let Some(session) = session {
            session.cancel();
            let (tx, rx) = flume::bounded(1);
            let closing = Arc::clone(&session);
            std::thread::spawn(move || {
                let _ = tx.send(closing.close());
            });
            match rx.recv_timeout(CLOSE_TIMEOUT) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    if first_error.is_none() {
                        first_error = Some(desktop_error(error));
                    }
                }
                Err(flume::RecvTimeoutError::Timeout) => {
                    if first_error.is_none() {
                        first_error =
                            Some(ComputerError::tool("Timed out closing computer worker"));
                    }
                }
                Err(flume::RecvTimeoutError::Disconnected) => {}
            }
        }

        let worker = inner.state.lock().worker.take();
        if let Some(worker) = worker {
            kill_child(worker.child);
        }
        first_error.map_or(Ok(()), Err)
    }
}

fn closed_error() -> ComputerError {
    ComputerError::tool(CLOSED)
}

fn not_running() -> ComputerError {
    ComputerError::tool(NOT_RUNNING)
}

fn ensure_session_locked(st: &mut State, session: &Value) -> ComputerResult<()> {
    if st.session.is_some() {
        return Ok(());
    }
    let options: DesktopSessionOptions = serde_json::from_value(json!({
        "display": session.get("display").cloned().unwrap_or(Value::Null),
    }))
    .map_err(|e| ComputerError::internal(e.to_string()))?;
    let desktop = DesktopSession::new(Some(options)).map_err(desktop_error)?;
    st.session = Some(Arc::new(desktop));
    Ok(())
}

fn ensure_worker(inner: &Arc<Inner>, cwd: &Path) -> ComputerResult<()> {
    {
        let st = inner.state.lock();
        if st.status == Status::Closed {
            return Err(closed_error());
        }
        if st.worker.is_some() {
            return Ok(());
        }
    }

    let mut child = Command::new("node")
        .args(["--input-type=module", "-e", WORKER_SOURCE])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ComputerError::tool(format!("Failed to start computer worker: {e}")))?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        kill_child(child);
        return Err(ComputerError::internal("Computer worker pipes unavailable"));
    };

    let tail = Arc::new(Mutex::new(String::new()));
    spawn_stderr(stderr, Arc::clone(&tail));
    let (ready_tx, ready_rx) = flume::bounded(1);
    let generation = {
        let mut st = inner.state.lock();
        if st.status == Status::Closed {
            drop(st);
            kill_child(child);
            return Err(closed_error());
        }
        if st.worker.is_some() {
            drop(st);
            kill_child(child);
            return Ok(());
        }
        st.next_gen += 1;
        let generation = st.next_gen;
        st.worker = Some(Worker {
            generation,
            child,
            stdin: Arc::new(Mutex::new(stdin)),
            pending: HashMap::new(),
            ready: Some(ready_tx),
            closed: None,
        });
        generation
    };
    spawn_reader(Arc::downgrade(inner), generation, stdout);

    match await_reply(
        inner,
        None,
        ready_rx,
        Instant::now() + START_TIMEOUT,
        None,
        "Timed out starting computer worker",
    ) {
        Ok(_) => {
            let mut st = inner.state.lock();
            if st.status != Status::Closed {
                st.status = Status::Ready;
            }
            Ok(())
        }
        Err(error) if error.message == WORKER_EXITED => Err(ComputerError::tool(format!(
            "{WORKER_EXITED} before ready: {}",
            tail.lock().trim()
        ))),
        Err(error) => Err(error),
    }
}

fn spawn_stderr(stderr: ChildStderr, tail: Arc<Mutex<String>>) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let mut text = tail.lock();
                    text.push_str(&String::from_utf8_lossy(&line));
                    if text.len() > STDERR_TAIL_BYTES {
                        let mut cut = text.len() - STDERR_TAIL_BYTES;
                        while !text.is_char_boundary(cut) {
                            cut += 1;
                        }
                        text.replace_range(..cut, "");
                    }
                }
            }
        }
    });
}

fn spawn_reader(weak: Weak<Inner>, generation: u64, stdout: ChildStdout) {
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(inner) = weak.upgrade() else {
                return;
            };
            handle_message(&inner, generation, message);
        }
        if let Some(inner) = weak.upgrade() {
            terminate_inner(&inner, ComputerError::tool(WORKER_EXITED), Some(generation));
        }
    });
}

fn handle_message(inner: &Arc<Inner>, generation: u64, message: Value) {
    let ok = message.get("ok").and_then(Value::as_bool) == Some(true);
    match message.get("type").and_then(Value::as_str) {
        Some("ready") => {
            let sender = {
                let mut st = inner.state.lock();
                st.worker
                    .as_mut()
                    .filter(|w| w.generation == generation)
                    .and_then(|w| w.ready.take())
            };
            if let Some(tx) = sender {
                let _ = tx.send(Ok(Value::Null));
            }
        }
        Some("closed") => {
            let sender = {
                let mut st = inner.state.lock();
                st.worker
                    .as_mut()
                    .filter(|w| w.generation == generation)
                    .and_then(|w| w.closed.take())
            };
            if let Some(tx) = sender {
                let _ = tx.send(Ok(Value::Null));
            }
        }
        Some("capabilities") => {
            let result = if ok {
                Ok(message.get("capabilities").cloned().unwrap_or(Value::Null))
            } else {
                Err(payload_error(&message))
            };
            resolve_pending(inner, generation, &message, result);
        }
        Some("result") => resolve_run(inner, &message, ok),
        Some("tool-call") => spawn_tool_call(inner, &message),
        Some("control-request") => spawn_control_request(inner, &message),
        Some("native") => spawn_native(inner, &message),
        _ => {}
    }
}

fn payload_error(message: &Value) -> ComputerError {
    ComputerError::from_payload(message.get("error").unwrap_or(&Value::Null))
}

fn id_of(value: &Value) -> Option<u64> {
    match value {
        Value::String(text) => text.parse().ok(),
        other => other.as_u64(),
    }
}

fn resolve_pending(
    inner: &Arc<Inner>,
    generation: u64,
    message: &Value,
    result: ComputerResult<Value>,
) {
    let Some(id) = message.get("id").and_then(id_of) else {
        return;
    };
    let sender = {
        let mut st = inner.state.lock();
        st.worker
            .as_mut()
            .filter(|w| w.generation == generation)
            .and_then(|w| w.pending.remove(&id))
    };
    if let Some(tx) = sender {
        let _ = tx.send(result);
    }
}

fn resolve_run(inner: &Arc<Inner>, message: &Value, ok: bool) {
    let Some(id) = message.get("id").and_then(id_of) else {
        return;
    };
    let run = inner.state.lock().runs.remove(&id);
    let Some(run) = run else {
        return;
    };
    let result = if ok {
        Ok(message.get("payload").cloned().unwrap_or(Value::Null))
    } else {
        Err(payload_error(message))
    };
    let _ = run.reply.send(result);
}

struct Claim {
    host: Arc<dyn ComputerHost>,
    signal: AbortSignal,
}

fn claim_signal(inner: &Arc<Inner>, run_id: Option<u64>, key: &str) -> Option<Claim> {
    let mut guard = inner.state.lock();
    let run = run_id.and_then(|rid| guard.runs.get_mut(&rid))?;
    let signal = AbortSignal::new();
    run.tool_signals.insert(key.to_owned(), signal.clone());
    Some(Claim {
        host: Arc::clone(&run.host),
        signal,
    })
}

fn release_signal(inner: &Arc<Inner>, run_id: Option<u64>, key: &str) {
    let mut guard = inner.state.lock();
    if let Some(run) = run_id.and_then(|rid| guard.runs.get_mut(&rid)) {
        run.tool_signals.remove(key);
    }
}

fn reply_envelope(result: ComputerResult<Value>) -> Value {
    match result {
        Ok(value) => json!({"ok": true, "value": value}),
        Err(error) => json!({"ok": false, "error": error.to_payload()}),
    }
}

fn spawn_tool_call(inner: &Arc<Inner>, message: &Value) {
    let Some(id) = message.get("id").cloned() else {
        return;
    };
    let run_id = message.get("runId").and_then(id_of);
    let name = message
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let args = message.get("args").cloned().unwrap_or(Value::Null);
    let inner = Arc::clone(inner);
    std::thread::spawn(move || {
        let key = id.to_string();
        let Some(claim) = claim_signal(&inner, run_id, &key) else {
            let reply = reply_envelope(Err(ComputerError::tool("No active run for tool call")));
            let _ = send(
                &inner,
                &json!({"type": "tool-reply", "id": id, "reply": reply}),
            );
            return;
        };
        let result = claim.host.call_tool(&name, args, &claim.signal);
        release_signal(&inner, run_id, &key);
        let _ = send(
            &inner,
            &json!({"type": "tool-reply", "id": id, "reply": reply_envelope(result)}),
        );
    });
}

fn spawn_control_request(inner: &Arc<Inner>, message: &Value) {
    let Some(id) = message.get("id").cloned() else {
        return;
    };
    let run_id = message.get("runId").and_then(id_of);
    let reason = message
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let inner = Arc::clone(inner);
    std::thread::spawn(move || {
        let key = id.to_string();
        let result = match claim_signal(&inner, run_id, &key) {
            None => Err(ComputerError::tool("No active run for control request")),
            Some(claim) => {
                let body = format!(
                    "{reason}\n\nFor this task only. Use the host interrupt to stop and revoke control. This does not authorize external side effects."
                );
                let granted = claim
                    .host
                    .confirm_control(CONTROL_TITLE, &body, &claim.signal);
                release_signal(&inner, run_id, &key);
                granted.map(|granted| Value::Bool(granted && !claim.signal.is_aborted()))
            }
        };
        let _ = send(
            &inner,
            &json!({"type": "tool-reply", "id": id, "reply": reply_envelope(result)}),
        );
    });
}

fn spawn_native(inner: &Arc<Inner>, message: &Value) {
    let Some(id) = message.get("id").cloned() else {
        return;
    };
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let args = message.get("args").cloned().unwrap_or(Value::Null);
    let inner = Arc::clone(inner);
    std::thread::spawn(move || {
        let session = inner.state.lock().session.clone();
        let result = match session {
            Some(session) => dispatch(&session, &method, args),
            None => Err(ComputerError::tool(NO_SESSION)),
        };
        let reply = match result {
            Ok(value) => json!({"type": "native-result", "id": id, "ok": true, "value": value}),
            Err(error) => json!({
                "type": "native-result",
                "id": id,
                "ok": false,
                "error": error.to_payload(),
            }),
        };
        let _ = send(&inner, &reply);
    });
}

fn send(inner: &Arc<Inner>, message: &Value) -> ComputerResult<()> {
    let stdin = inner
        .state
        .lock()
        .worker
        .as_ref()
        .map(|w| Arc::clone(&w.stdin))
        .ok_or_else(not_running)?;
    let mut line =
        serde_json::to_string(message).map_err(|e| ComputerError::internal(e.to_string()))?;
    line.push('\n');
    let mut pipe = stdin.lock();
    pipe.write_all(line.as_bytes())
        .map_err(|e| ComputerError::tool(format!("Failed to write to computer worker: {e}")))?;
    pipe.flush()
        .map_err(|e| ComputerError::tool(format!("Failed to write to computer worker: {e}")))
}

fn register_pending(
    inner: &Arc<Inner>,
) -> ComputerResult<(u64, flume::Receiver<ComputerResult<Value>>)> {
    let (tx, rx) = flume::bounded(1);
    let mut guard = inner.state.lock();
    let st = &mut *guard;
    st.next_id += 1;
    let id = st.next_id;
    let worker = st.worker.as_mut().ok_or_else(not_running)?;
    worker.pending.insert(id, tx);
    Ok((id, rx))
}

fn forget_pending(inner: &Arc<Inner>, id: u64) {
    if let Some(worker) = inner.state.lock().worker.as_mut() {
        worker.pending.remove(&id);
    }
}

fn await_reply(
    inner: &Arc<Inner>,
    key: Option<u64>,
    rx: flume::Receiver<ComputerResult<Value>>,
    deadline: Instant,
    signal: Option<&AbortSignal>,
    timeout_message: &str,
) -> ComputerResult<Value> {
    loop {
        match rx.recv_timeout(POLL) {
            Ok(result) => return result,
            Err(flume::RecvTimeoutError::Disconnected) => {
                return Err(ComputerError::tool(WORKER_EXITED));
            }
            Err(flume::RecvTimeoutError::Timeout) => {}
        }
        if signal.is_some_and(AbortSignal::is_aborted) {
            if let Some(id) = key {
                forget_pending(inner, id);
            }
            return Err(ComputerError::abort(ABORTED));
        }
        if Instant::now() >= deadline {
            if let Some(id) = key {
                forget_pending(inner, id);
            }
            let error = ComputerError::tool(timeout_message);
            terminate(inner, error.clone());
            return Err(error);
        }
    }
}

fn cancel_run(inner: &Arc<Inner>, id: u64) {
    let session = {
        let guard = inner.state.lock();
        if let Some(run) = guard.runs.get(&id) {
            for signal in run.tool_signals.values() {
                signal.abort();
            }
        }
        guard.session.clone()
    };
    if let Some(session) = session {
        session.cancel();
    }
}

fn terminate(inner: &Arc<Inner>, error: ComputerError) {
    terminate_inner(inner, error, None);
}

fn terminate_inner(inner: &Arc<Inner>, error: ComputerError, only_gen: Option<u64>) {
    let (worker, runs, session) = {
        let mut guard = inner.state.lock();
        let st = &mut *guard;
        if let Some(generation) = only_gen {
            if !st
                .worker
                .as_ref()
                .is_some_and(|w| w.generation == generation)
            {
                return;
            }
        }
        if st.status != Status::Closed {
            st.status = Status::Failed;
        }
        let runs: Vec<Run> = st.runs.drain().map(|(_, run)| run).collect();
        (st.worker.take(), runs, st.session.take())
    };
    for run in runs {
        for signal in run.tool_signals.values() {
            signal.abort();
        }
        let _ = run.reply.send(Err(error.clone()));
    }
    if let Some(session) = session {
        session.cancel();
    }
    if let Some(worker) = worker {
        kill_child(worker.child);
    }
}

fn kill_child(mut child: Child) {
    #[cfg(unix)]
    {
        if let Ok(pid) = libc::pid_t::try_from(child.id()) {
            // SAFETY: pid belongs to the child process this supervisor spawned.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
        let deadline = Instant::now() + GRACE;
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(POLL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod computer_supervisor_tests {
    use super::*;

    #[test]
    fn computer_abort_signal_shares_flag() {
        let flag = Arc::new(AtomicBool::new(false));
        let signal = AbortSignal::from_flag(Arc::clone(&flag));
        assert!(!signal.is_aborted());
        flag.store(true, Ordering::SeqCst);
        assert!(signal.is_aborted());
    }

    #[test]
    fn computer_ids_accept_strings_and_numbers() {
        assert_eq!(id_of(&json!("42")), Some(42));
        assert_eq!(id_of(&json!(7)), Some(7));
        assert_eq!(id_of(&json!("native-1")), None);
    }
}

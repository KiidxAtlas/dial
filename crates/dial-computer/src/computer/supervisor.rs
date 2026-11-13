use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use serde_json::{Value, json};

use crate::computer::desktop::types::DesktopSessionOptions;
use crate::computer::desktop::{CancelReason, DesktopSession};
use crate::computer::rpc::{ComputerError, ComputerResult, desktop_error, dispatch};
use crate::runtime::execution::NativeCapture;

/// Worker payload and host-observed native capture facts travel separately.
pub struct RunOutput {
    pub value: Value,
    pub captures: Vec<NativeCapture>,
}

impl RunOutput {
    pub(super) fn plain(value: Value) -> Self {
        Self {
            value,
            captures: Vec::new(),
        }
    }
}

const MAX_FRAME: usize = crate::runtime::json_transport::MAX_FRAME;
#[cfg(unix)]
type WorkerInput = ChildStdin;
#[cfg(windows)]
type WorkerInput = crate::platform::io::QueuedWriter<ChildStdin>;

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

/// The worker source is written to a private `.mjs` file rather than passed with `node -e`: at
/// ~76 KB it exceeds the 32,767-character Windows command line, where `CreateProcess` refused the
/// launch and every computer action failed. The path is removed when the worker is dropped.
pub(super) fn worker_script() -> std::io::Result<tempfile::TempPath> {
    let mut file = tempfile::Builder::new()
        .prefix("dial-computer-worker-")
        .suffix(".mjs")
        .tempfile()?;
    file.write_all(WORKER_SOURCE.as_bytes())?;
    file.flush()?;
    Ok(file.into_temp_path())
}

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

    /// The shared flag, for model requests that take a cancel flag instead of a signal.
    pub fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.0)
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
    stdin: Arc<Mutex<WorkerInput>>,
    stopped: AbortSignal,
    pending: HashMap<u64, flume::Sender<ComputerResult<Value>>>,
    ready: Option<flume::Sender<ComputerResult<Value>>>,
    closed: Option<flume::Sender<ComputerResult<Value>>>,
    /// Kept until the worker is stopped; dropping it removes the script file.
    _script: tempfile::TempPath,
}

impl Worker {
    fn stop(self) {
        self.stopped.abort();
        kill_child(self.child);
    }
}

struct Run {
    reply: flume::Sender<ComputerResult<RunOutput>>,
    captures: NativeCaptures,
    host: Arc<dyn ComputerHost>,
    tool_signals: HashMap<String, AbortSignal>,
    /// Generation of the worker that admitted this run; replies bind to it.
    generation: u64,
}

/// Native RPCs are threaded. Bind captures to a quiet effect epoch, not reply arrival order.
#[derive(Default)]
struct NativeCaptures {
    epoch: u64,
    mutations: usize,
    receipts: Vec<NativeCapture>,
}

impl NativeCaptures {
    fn begin(&mut self, mutation: bool) -> Option<u64> {
        if mutation {
            self.epoch += 1;
            self.mutations += 1;
            self.receipts.clear();
        }
        (self.mutations == 0).then_some(self.epoch)
    }

    fn complete(&mut self, mutation: bool, epoch: Option<u64>, capture: Option<NativeCapture>) {
        if mutation {
            self.mutations -= 1;
            self.epoch += 1;
            self.receipts.clear();
        } else if self.mutations == 0
            && epoch == Some(self.epoch)
            && let Some(capture) = capture
        {
            self.receipts.push(capture);
        }
    }
}

/// An evidence invalidation policy, not an approval grant. Unknown native methods fail closed.
fn native_may_mutate(method: &str) -> bool {
    !matches!(
        method,
        "capture" | "captureRegion" | "observe" | "capabilities" | "listDisplays"
        | "listWindows" | "getWindow" | "listApplications" | "menuItems" | "controlState"
        | "axSnapshot" | "axQuery" | "axElementAt" | "axFocused" | "axNode"
        | "axAttributes" | "axChildren" | "axParent" | "clipboardRead"
        // Successful run cleanup retires outstanding requests; it does not send desktop input.
        | "retire"
    )
}

struct State {
    status: Status,
    worker: Option<Worker>,
    session: Option<Arc<DesktopSession>>,
    runs: HashMap<u64, Run>,
    next_id: u64,
    next_gen: u64,
    /// Exit status of the last worker that exited on its own, for start-failure messages.
    last_exit: Option<i32>,
}

struct Inner {
    state: Mutex<State>,
    /// One run at a time owns the shared worker and native session. Cleanup of
    /// run N (retire/cancel) must finish before run N+1 is admitted, or it would
    /// invalidate the next run's native work.
    gate: Mutex<bool>,
    gate_wake: Condvar,
}

/// Held for the whole run, including its native-session cleanup.
struct RunGate(Arc<Inner>);

impl Drop for RunGate {
    fn drop(&mut self) {
        *self.0.gate.lock() = false;
        self.0.gate_wake.notify_one();
    }
}

/// Waits for the previous run while staying responsive to abort, close and the
/// request's own timeout. A run that never holds the gate never touches the
/// shared session.
fn acquire_gate(
    inner: &Arc<Inner>,
    signal: &AbortSignal,
    deadline: Instant,
) -> ComputerResult<RunGate> {
    let mut busy = inner.gate.lock();
    loop {
        if signal.is_aborted() {
            return Err(ComputerError::abort(ABORTED));
        }
        if inner.state.lock().status == Status::Closed {
            return Err(closed_error());
        }
        if Instant::now() >= deadline {
            return Err(ComputerError::tool(
                "Computer is busy with another run; timed out waiting for it to finish",
            ));
        }
        if !*busy {
            *busy = true;
            return Ok(RunGate(Arc::clone(inner)));
        }
        inner.gate_wake.wait_for(&mut busy, POLL);
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(worker) = self.state.get_mut().worker.take() {
            worker.stop();
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
                    last_exit: None,
                }),
                gate: Mutex::new(false),
                gate_wake: Condvar::new(),
            }),
        }
    }

    pub fn run(
        &self,
        request: &RunRequest,
        host: Arc<dyn ComputerHost>,
        signal: &AbortSignal,
    ) -> ComputerResult<RunOutput> {
        let inner = &self.inner;
        if signal.is_aborted() {
            return Err(ComputerError::abort(ABORTED));
        }
        let _gate = acquire_gate(
            inner,
            signal,
            Instant::now() + Duration::from_millis(request.timeout_ms),
        )?;
        if signal.is_aborted() {
            return Err(ComputerError::abort(ABORTED));
        }
        ensure_worker(inner, &request.cwd, signal)?;
        let (reply, rx) = flume::bounded(1);
        let id = {
            let mut guard = inner.state.lock();
            let st = &mut *guard;
            if st.status == Status::Closed {
                return Err(closed_error());
            }
            let Some(generation) = st.worker.as_ref().map(|w| w.generation) else {
                return Err(not_running());
            };
            ensure_session_locked(st, &request.session)?;
            st.next_id += 1;
            let id = st.next_id;
            st.runs.insert(
                id,
                Run {
                    reply,
                    host,
                    tool_signals: HashMap::new(),
                    generation,
                    captures: NativeCaptures::default(),
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
        if let Err(error) = send_guarded(
            inner,
            &message,
            start + Duration::from_millis(request.timeout_ms),
            Some(signal),
        ) {
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
                let _ = send_guarded(
                    inner,
                    &json!({"type": "abort", "id": id.to_string()}),
                    now + GRACE,
                    None,
                );
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
            } else if aborted {
                session.cancel_for(CancelReason::RunAborted);
            } else {
                session.cancel_for(CancelReason::RunFailed);
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
        ensure_worker(inner, cwd, signal)?;
        {
            let mut guard = inner.state.lock();
            if guard.status == Status::Closed {
                return Err(closed_error());
            }
            ensure_session_locked(&mut guard, session)?;
        }
        let (id, rx) = register_pending(inner)?;
        if let Err(error) = send_guarded(
            inner,
            &json!({"type": "capabilities", "id": id.to_string(), "session": session}),
            Instant::now() + CAPABILITIES_TIMEOUT,
            Some(signal),
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
        if worker_running
            && send_guarded(
                inner,
                &json!({"type": "close"}),
                Instant::now() + CLOSE_TIMEOUT,
                None,
            )
            .is_ok()
            && let Err(error) = await_reply(
                inner,
                None,
                closed_rx,
                Instant::now() + CLOSE_TIMEOUT,
                None,
                "Timed out closing computer worker",
            )
            && error.message != WORKER_EXITED
        {
            first_error = Some(error);
        }

        if let Some(session) = session {
            session.cancel_for(CancelReason::SessionClosed);
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
            worker.stop();
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

fn ensure_worker(inner: &Arc<Inner>, cwd: &Path, signal: &AbortSignal) -> ComputerResult<()> {
    {
        let st = inner.state.lock();
        if st.status == Status::Closed {
            return Err(closed_error());
        }
        if st.worker.is_some() {
            return Ok(());
        }
    }

    let script = worker_script()
        .map_err(|e| ComputerError::tool(format!("Failed to write computer worker script: {e}")))?;
    // A Node install that cannot load (a broken or wrong-architecture node.exe or DLL) would
    // otherwise stop at a Windows popup; this turns it into an exit status reported below.
    crate::platform::process::quiet_error_dialogs();
    let node = dial_tools::setup::deps::bundled_node::resolve(cwd, "node", "computer.node")
        .map_err(|reason| {
            if reason.starts_with("cannot find computer.node") {
                ComputerError::tool(format!("{reason}. {NODE_HINT}"))
            } else {
                ComputerError::tool(reason)
            }
        })?;
    let mut command = Command::new(&node);
    command
        .arg(&*script)
        .env_clear()
        .envs(
            std::env::vars().filter(|(name, value)| {
                crate::tools::exec::sandbox::child_env_allowed(name, value)
            }),
        )
        // The same Windows system variables the browser launch keeps; empty elsewhere.
        .envs(crate::tools::exec::sandbox::windows_system_env())
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = crate::platform::process::spawn_group(&mut command)
        .map_err(|e| ComputerError::tool(launch_failure(&e)))?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        kill_child(child);
        return Err(ComputerError::internal("Computer worker pipes unavailable"));
    };

    for result in [
        crate::platform::io::set_nonblocking(&stdin),
        crate::platform::io::set_nonblocking(&stdout),
        crate::platform::io::set_nonblocking(&stderr),
    ] {
        if let Err(error) = result {
            kill_child(child);
            return Err(ComputerError::tool(format!(
                "Computer pipe setup failed: {error}"
            )));
        }
    }
    #[cfg(windows)]
    let stdin = crate::platform::io::QueuedWriter::new(stdin, 64 * 1024);
    let stopped = AbortSignal::new();
    let tail = Arc::new(Mutex::new(String::new()));
    spawn_stderr(stderr, Arc::clone(&tail), stopped.clone());
    let (ready_tx, ready_rx) = flume::bounded(1);
    let generation = {
        let mut st = inner.state.lock();
        if st.status == Status::Closed {
            drop(st);
            stopped.abort();
            kill_child(child);
            return Err(closed_error());
        }
        if st.worker.is_some() {
            drop(st);
            stopped.abort();
            kill_child(child);
            return Ok(());
        }
        st.next_gen += 1;
        let generation = st.next_gen;
        st.last_exit = None;
        st.worker = Some(Worker {
            generation,
            child,
            stdin: Arc::new(Mutex::new(stdin)),
            stopped: stopped.clone(),
            pending: HashMap::new(),
            ready: Some(ready_tx),
            closed: None,
            _script: script,
        });
        generation
    };
    spawn_reader(Arc::downgrade(inner), generation, stdout, stopped);

    match await_reply(
        inner,
        None,
        ready_rx,
        Instant::now() + START_TIMEOUT,
        Some(signal),
        "Timed out starting computer worker",
    ) {
        Ok(_) => {
            let mut st = inner.state.lock();
            if st.status != Status::Closed {
                st.status = Status::Ready;
            }
            Ok(())
        }
        Err(error) if error.message == WORKER_EXITED => {
            let code = inner.state.lock().last_exit.take();
            Err(ComputerError::tool(start_failure(code, tail.lock().trim())))
        }
        Err(error) => Err(error),
    }
}

const NODE_HINT: &str = "Computer use runs on Node.js: check that `node --version` works in a new terminal, and if it does not, install the current Node.js LTS for this computer's CPU from https://nodejs.org, then restart Dial.";

fn node_hint() -> &'static str {
    if dial_tools::setup::deps::bundled_node::available() {
        "Computer use uses the Node.js runtime bundled with Dial; repair or reinstall Dial if the bundled runtime cannot start. Installing another Node on PATH does not replace this runtime."
    } else {
        NODE_HINT
    }
}

/// Why `node` could not be launched at all.
fn launch_failure(error: &std::io::Error) -> String {
    let hint = node_hint();
    if error.kind() == std::io::ErrorKind::NotFound
        && !dial_tools::setup::deps::bundled_node::available()
    {
        format!("Failed to start computer worker: `node` was not found on PATH. {hint}")
    } else {
        format!("Failed to start computer worker: {error}. {hint}")
    }
}

/// Why the worker exited before it was ready. `code` is its exit status when known; Windows
/// loader failures (NTSTATUS values) are named, since they otherwise show only as a number.
fn start_failure(code: Option<i32>, stderr: &str) -> String {
    let status = code.map(|code| {
        let reason = match code as u32 {
            0xC000_0142 => Some("a DLL failed to initialize while Node started"),
            0xC000_007B => Some(
                "node.exe or a DLL it loads is damaged or built for a different CPU architecture",
            ),
            0xC000_0135 => Some("a DLL that node.exe needs is missing"),
            0xC000_0139 => Some("node.exe and one of its DLLs do not match"),
            _ => None,
        };
        match reason {
            Some(reason) => format!(" (exit {:#010X}: {reason})", code as u32),
            None if code as u32 >= 0xC000_0000 => format!(" (exit {:#010X})", code as u32),
            None => format!(" (exit {code})"),
        }
    });
    let mut message = format!(
        "{WORKER_EXITED} before ready{}. {}",
        status.unwrap_or_default(),
        node_hint()
    );
    if !stderr.is_empty() {
        message.push_str(" Worker output: ");
        message.push_str(stderr);
    }
    message
}

/// The exit code of a worker whose output already closed. It is exiting, so wait briefly for
/// the status rather than report none.
fn exited_code(child: &mut Child) -> Option<i32> {
    for _ in 0..25 {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => return None,
        }
    }
    None
}

fn spawn_stderr(mut stderr: ChildStderr, tail: Arc<Mutex<String>>, stopped: AbortSignal) {
    std::thread::spawn(move || {
        let mut bytes = [0u8; STDERR_TAIL_BYTES];
        while !stopped.is_aborted() {
            match crate::platform::io::read_nonblocking(&mut stderr, &mut bytes) {
                Ok(0) => break,
                Ok(n) => {
                    let mut text = tail.lock();
                    text.push_str(&String::from_utf8_lossy(&bytes[..n]));
                    if text.len() > STDERR_TAIL_BYTES {
                        let mut cut = text.len() - STDERR_TAIL_BYTES;
                        while !text.is_char_boundary(cut) {
                            cut += 1;
                        }
                        text.replace_range(..cut, "");
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    let _ = crate::platform::io::wait_readable(&stderr, POLL);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    });
}

fn spawn_reader(weak: Weak<Inner>, generation: u64, mut stdout: ChildStdout, stopped: AbortSignal) {
    std::thread::spawn(move || {
        let mut pending = Vec::new();
        let mut scanned = 0;
        let mut bytes = [0u8; 16384];
        let mut failure = ComputerError::tool(WORKER_EXITED);
        while !stopped.is_aborted() {
            match crate::platform::io::read_nonblocking(&mut stdout, &mut bytes) {
                Ok(0) => break,
                Ok(n) => {
                    pending.extend_from_slice(&bytes[..n]);
                    // Parse bounded frames, never allocate an unbounded line on a reader thread.
                    while let Some(end) = pending[scanned..]
                        .iter()
                        .position(|byte| *byte == b'\n')
                        .map(|index| scanned + index)
                    {
                        if end > MAX_FRAME {
                            break;
                        }
                        let line: Vec<_> = pending.drain(..=end).collect();
                        scanned = 0;
                        let message = match serde_json::from_slice(&line) {
                            Ok(message) => message,
                            Err(error) => {
                                if let Some(inner) = weak.upgrade() {
                                    terminate_inner(
                                        &inner,
                                        ComputerError::tool(format!(
                                            "Invalid computer frame: {error}"
                                        )),
                                        Some(generation),
                                    );
                                }
                                return;
                            }
                        };
                        let Some(inner) = weak.upgrade() else {
                            return;
                        };
                        handle_message(&inner, generation, message);
                    }
                    scanned = pending.len();
                    if pending.len() > MAX_FRAME {
                        failure = ComputerError::tool("Computer response exceeded frame limit");
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    let _ = crate::platform::io::wait_readable(&stdout, POLL);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        if let Some(inner) = weak.upgrade() {
            terminate_inner(&inner, failure, Some(generation));
        }
    });
}

fn handle_message(inner: &Arc<Inner>, generation: u64, message: Value) {
    if !inner
        .state
        .lock()
        .worker
        .as_ref()
        .is_some_and(|worker| worker.generation == generation)
    {
        return;
    }
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
        Some("pong") => resolve_pending(inner, generation, &message, Ok(Value::Null)),
        Some("result") => resolve_run(inner, &message, ok),
        Some("tool-call") => spawn_tool_call(inner, generation, &message),
        Some("control-request") => spawn_control_request(inner, generation, &message),
        Some("native") => spawn_native(inner, generation, &message),
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
        Ok(RunOutput {
            value: message.get("payload").cloned().unwrap_or(Value::Null),
            captures: run.captures.receipts,
        })
    } else {
        Err(payload_error(message))
    };
    let _ = run.reply.send(result);
}

struct Claim {
    host: Arc<dyn ComputerHost>,
    signal: AbortSignal,
}

fn claim_signal(
    inner: &Arc<Inner>,
    generation: u64,
    run_id: Option<u64>,
    key: &str,
) -> Option<Claim> {
    let mut guard = inner.state.lock();
    let run = run_id
        .and_then(|rid| guard.runs.get_mut(&rid))
        .filter(|run| run.generation == generation)?;
    let signal = AbortSignal::new();
    run.tool_signals.insert(key.to_owned(), signal.clone());
    Some(Claim {
        host: Arc::clone(&run.host),
        signal,
    })
}

fn release_signal(inner: &Arc<Inner>, generation: u64, run_id: Option<u64>, key: &str) {
    let mut guard = inner.state.lock();
    if let Some(run) = run_id
        .and_then(|rid| guard.runs.get_mut(&rid))
        .filter(|run| run.generation == generation)
    {
        run.tool_signals.remove(key);
    }
}

fn reply_envelope(result: ComputerResult<Value>) -> Value {
    match result {
        Ok(value) => json!({"ok": true, "value": value}),
        Err(error) => json!({"ok": false, "error": error.to_payload()}),
    }
}

/// Tool and control requests are answered only by the worker generation that issued them.
fn spawn_tool_call(inner: &Arc<Inner>, generation: u64, message: &Value) {
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
        let Some(claim) = claim_signal(&inner, generation, run_id, &key) else {
            let reply = reply_envelope(Err(ComputerError::tool("No active run for tool call")));
            let _ = send_reply(
                &inner,
                generation,
                &json!({"type": "tool-reply", "id": id, "reply": reply}),
            );
            return;
        };
        let result = claim.host.call_tool(&name, args, &claim.signal);
        release_signal(&inner, generation, run_id, &key);
        let _ = send_reply(
            &inner,
            generation,
            &json!({"type": "tool-reply", "id": id, "reply": reply_envelope(result)}),
        );
    });
}

fn spawn_control_request(inner: &Arc<Inner>, generation: u64, message: &Value) {
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
        let result = match claim_signal(&inner, generation, run_id, &key) {
            None => Err(ComputerError::tool("No active run for control request")),
            Some(claim) => {
                let body = format!(
                    "{reason}\n\nFor this task only. Use the host interrupt to stop and revoke control. This does not authorize external side effects."
                );
                let granted = claim
                    .host
                    .confirm_control(CONTROL_TITLE, &body, &claim.signal);
                release_signal(&inner, generation, run_id, &key);
                granted.map(|granted| Value::Bool(granted && !claim.signal.is_aborted()))
            }
        };
        let _ = send_reply(
            &inner,
            generation,
            &json!({"type": "tool-reply", "id": id, "reply": reply_envelope(result)}),
        );
    });
}

/// Native requests bind to the session and worker generation at admission. A replacement
/// worker never serves them, and a reply produced after restart is discarded.
fn spawn_native(inner: &Arc<Inner>, generation: u64, message: &Value) {
    let Some(id) = message.get("id").cloned() else {
        return;
    };
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let args = message.get("args").cloned().unwrap_or(Value::Null);
    let mutation = native_may_mutate(&method);
    let (session, stopped, capture_scope) = {
        let mut st = inner.state.lock();
        let Some(stopped) = st
            .worker
            .as_ref()
            .filter(|w| w.generation == generation)
            .map(|worker| worker.stopped.clone())
        else {
            return;
        };
        let session = st.session.as_ref().map(|session| session.admit());
        let capture_scope = st
            .runs
            .iter_mut()
            .find(|(_, run)| run.generation == generation)
            .map(|(id, run)| (*id, run.captures.begin(mutation)));
        (session, stopped, capture_scope)
    };
    // These boundaries only invalidate tokens/revoke control; they never wait
    // for the native worker. Apply them in protocol order before admitting the
    // next request, while slow native operations remain independently threaded.
    let boundary = matches!(method.as_str(), "retire" | "cancel" | "releaseControl");
    let boundary_result = boundary.then(|| match session.as_ref() {
        Some(session) => dispatch(session, &method, args.clone()),
        None => Err(ComputerError::tool(NO_SESSION)),
    });
    let inner = Arc::clone(inner);
    std::thread::spawn(move || {
        if stopped.is_aborted() {
            return;
        }
        let result = boundary_result.unwrap_or_else(|| match session {
            Some(session) => dispatch(&session, &method, args),
            None => Err(ComputerError::tool(NO_SESSION)),
        });
        if let Some((run_id, epoch)) = capture_scope
            && let Some(run) = inner.state.lock().runs.get_mut(&run_id)
            && run.generation == generation
        {
            let capture = result
                .as_ref()
                .ok()
                .and_then(|value| native_capture(&method, value));
            run.captures.complete(mutation, epoch, capture);
        }
        let reply = match result {
            Ok(value) => json!({"type": "native-result", "id": id, "ok": true, "value": value}),
            Err(error) => json!({
                "type": "native-result",
                "id": id,
                "ok": false,
                "error": error.to_payload(),
            }),
        };
        let _ = send_reply(&inner, generation, &reply);
    });
}

/// Called only on the Rust desktop RPC result, never on worker-authored displays/return values.
fn native_capture(method: &str, value: &Value) -> Option<NativeCapture> {
    use base64::Engine;
    let value = match method {
        "capture" | "captureRegion" => value,
        "observe" => &value["capture"],
        _ => return None,
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value["data"].as_str()?)
        .ok()?;
    if bytes.is_empty() {
        return None;
    }
    let target = value["target"]
        .as_str()
        .filter(|target| !target.is_empty())
        .and_then(|target| {
            if target == "desktop" || target.starts_with("display:") {
                // Resolve selectors only from the actual capture's single-display metadata.
                let displays = value["displays"].as_array()?;
                if displays.len() != 1 {
                    return None;
                }
                Some(format!("display:{}", displays[0]["id"].as_str()?))
            } else {
                Some(format!("window:{target}"))
            }
        });
    Some(NativeCapture::new(&bytes, "image/png", target))
}

/// Writes a reply to the issuing generation only; fails once that worker is no longer current.
fn send_reply(inner: &Arc<Inner>, generation: u64, message: &Value) -> ComputerResult<()> {
    send_scoped(
        inner,
        Some(generation),
        message,
        Instant::now() + START_TIMEOUT,
        None,
    )
}

fn send_guarded(
    inner: &Arc<Inner>,
    message: &Value,
    deadline: Instant,
    signal: Option<&AbortSignal>,
) -> ComputerResult<()> {
    send_scoped(inner, None, message, deadline, signal)
}

fn send_scoped(
    inner: &Arc<Inner>,
    only: Option<u64>,
    message: &Value,
    deadline: Instant,
    signal: Option<&AbortSignal>,
) -> ComputerResult<()> {
    let (stdin, stopped, generation) = inner
        .state
        .lock()
        .worker
        .as_ref()
        .filter(|w| only.is_none_or(|generation| generation == w.generation))
        .map(|w| (Arc::clone(&w.stdin), w.stopped.clone(), w.generation))
        .ok_or_else(not_running)?;
    let mut line =
        serde_json::to_vec(message).map_err(|e| ComputerError::internal(e.to_string()))?;
    if line.len() > MAX_FRAME {
        return Err(ComputerError::tool("Computer request exceeded frame limit"));
    }
    line.push(b'\n');
    let check = || {
        if stopped.is_aborted() || signal.is_some_and(AbortSignal::is_aborted) {
            Err(ComputerError::abort(ABORTED))
        } else if Instant::now() >= deadline {
            Err(ComputerError::tool("Timed out writing to computer worker"))
        } else {
            Ok(())
        }
    };
    let result = (|| {
        let pipe = loop {
            check()?;
            if let Some(pipe) = stdin.try_lock_for(POLL) {
                break pipe;
            }
        };
        #[cfg(unix)]
        let mut pipe = pipe;
        let mut offset = 0;
        while offset < line.len() {
            check()?;
            #[cfg(unix)]
            let written = pipe.write(&line[offset..]);
            #[cfg(windows)]
            let written = pipe.try_write(&line[offset..]);
            match written {
                Ok(0) => return Err(ComputerError::tool("Computer worker stdin closed")),
                Ok(n) => offset += n,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    #[cfg(unix)]
                    let _ = crate::platform::io::wait_writable(&*pipe, POLL);
                    #[cfg(windows)]
                    let _ = pipe.wait_until_drained(POLL);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => {
                    return Err(ComputerError::tool(format!(
                        "Failed to write to computer worker: {error}"
                    )));
                }
            }
        }
        #[cfg(windows)]
        loop {
            check()?;
            if pipe
                .wait_until_drained(POLL)
                .map_err(|e| ComputerError::tool(e.to_string()))?
            {
                break;
            }
        }
        check()
    })();
    if let Err(error) = &result {
        // Partial JSON must never be reused by a later operation.
        terminate_inner(inner, error.clone(), Some(generation));
    }
    result
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
            let error = ComputerError::abort(ABORTED);
            terminate(inner, error.clone());
            return Err(error);
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
        session.cancel_for(CancelReason::RunAborted);
    }
}

fn terminate(inner: &Arc<Inner>, error: ComputerError) {
    terminate_inner(inner, error, None);
}

fn terminate_inner(inner: &Arc<Inner>, error: ComputerError, only_gen: Option<u64>) {
    let (worker, runs, session) = {
        let mut guard = inner.state.lock();
        let st = &mut *guard;
        if let Some(generation) = only_gen
            && !st
                .worker
                .as_ref()
                .is_some_and(|w| w.generation == generation)
        {
            return;
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
        session.cancel_for(CancelReason::WorkerTerminated);
    }
    if let Some(mut worker) = worker {
        if error.message == WORKER_EXITED
            && let Some(code) = exited_code(&mut worker.child)
        {
            inner.state.lock().last_exit = Some(code);
        }
        worker.stop();
    }
}

fn kill_child(child: Child) {
    drop(crate::runtime::process::ProcessGroup::new(child));
}

const SMOKE_PING_TIMEOUT: Duration = Duration::from_secs(10);

/// Installed-feature probe for the real worker transport: start it, round-trip `ping`/`pong`,
/// and require its `closed` acknowledgement. It never builds a DesktopSession or sends native
/// work, so it needs no display or desktop permissions. The worker is reaped on every path.
pub fn smoke_test(cwd: &Path) -> ComputerResult<()> {
    let supervisor = ComputerSupervisor::new();
    let signal = AbortSignal::new();
    let inner = &supervisor.inner;
    let result = ensure_worker(inner, cwd, &signal)
        .and_then(|()| ping_worker(inner, SMOKE_PING_TIMEOUT))
        .and_then(|()| close_worker_strictly(inner));
    let worker = {
        let mut st = inner.state.lock();
        st.status = Status::Closed;
        st.worker.take()
    };
    if let Some(worker) = worker {
        worker.stop();
    }
    result
}

/// Sends one `ping` and waits for its matching `pong`. A timeout terminates the worker.
fn ping_worker(inner: &Arc<Inner>, timeout: Duration) -> ComputerResult<()> {
    let (id, rx) = register_pending(inner)?;
    let deadline = Instant::now() + timeout;
    if let Err(error) = send_guarded(inner, &json!({"type": "ping", "id": id}), deadline, None) {
        forget_pending(inner, id);
        return Err(error);
    }
    await_reply(
        inner,
        Some(id),
        rx,
        deadline,
        None,
        "Timed out waiting for computer worker ping",
    )
    .map(|_| ())
}

/// Unlike ordinary `close`, which tolerates a failed or lost handshake, the probe requires the
/// worker's `closed` acknowledgement before it reports success.
fn close_worker_strictly(inner: &Arc<Inner>) -> ComputerResult<()> {
    let (closed_tx, closed_rx) = flume::bounded(1);
    {
        let mut st = inner.state.lock();
        let worker = st.worker.as_mut().ok_or_else(not_running)?;
        worker.closed = Some(closed_tx);
    }
    let deadline = Instant::now() + CLOSE_TIMEOUT;
    send_guarded(inner, &json!({"type": "close"}), deadline, None)?;
    await_reply(
        inner,
        None,
        closed_rx,
        deadline,
        None,
        "Timed out closing computer worker",
    )
    .map(|_| ())
}

#[cfg(test)]
mod computer_supervisor_tests;

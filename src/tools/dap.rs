//! Approved, event-aware DAP sessions. Launch is a two-phase configuration handshake.
use super::shell::ProcessGroup;
use crate::tools::ext::{Tool, ToolContext, ToolEvent};
use crate::tools::{ToolOutput, Workspace, tool_schema};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::{
    cell::Cell,
    collections::{BTreeMap, VecDeque},
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

mod breakpoints;
mod select;
pub use select::catalogue;

const MAX_FRAME: usize = 8 * 1024 * 1024;
const MAX_HEADER: usize = 8 * 1024;
const MAX_OUTPUT: usize = 64 * 1024;
const MAX_EVENTS: usize = 256;
const EVENT_BYTES: usize = 256 * 1024;
const POLL: Duration = Duration::from_millis(25);
const REVERSE_REPLY_TIMEOUT: Duration = Duration::from_secs(10);
const DESCRIPTION: &str = "Approved event-aware DAP control for an operator-installed adapter. launch/attach return when initialized, possibly before their response: setBreakpoints then configurationDone completes configuration. setBreakpoints and removeBreakpoint (source + line) merge into the per-source list; setFunctionBreakpoints and removeFunctionBreakpoint (name) merge into the function list; an empty list clears its scope. Each sends the full merged list and records adapter verification in status.breakpoints. Execution actions also complete configuration. status exposes capabilities/state; output drains bounded events, debuggee output and adapter stderr with loss counters. Positive timeout_secs covers writes and waits; zero is unlimited until cancellation/adapter exit. Reverse runInTerminal/startDebugging requests are refused, never host-launched. Each call requires approval; launch program/cwd remain inside the workspace. Disconnect/terminate/cancellation destroy the owned adapter process group.";

/// Operator-controlled command used to start one DAP adapter.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DapTransport {
    #[default]
    Stdio,
    Tcp,
    Socket,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DapAdapterConfig {
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub transport: DapTransport,
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub file_types: Vec<String>,
    #[serde(default)]
    pub root_markers: Vec<String>,
    #[serde(default)]
    pub launch_defaults: serde_json::Map<String, Value>,
    #[serde(default)]
    pub attach_defaults: serde_json::Map<String, Value>,
}

fn default_timeout_secs() -> u64 {
    0
}

/// Creates one lazy, single-session tool per catalogued adapter, plus a `dap` tool that selects an
/// adapter from its `adapter` argument, the launch program's extension, or project root markers.
pub fn tools(adapters: &BTreeMap<String, DapAdapterConfig>) -> Vec<Arc<dyn Tool>> {
    let all = Arc::new(catalogue(adapters));
    let mut tools: Vec<Arc<dyn Tool>> = all
        .keys()
        .map(|id| {
            Arc::new(DapTool::new(
                format!("dap_{id}"),
                Some(id.clone()),
                Arc::clone(&all),
            )) as Arc<dyn Tool>
        })
        .collect();
    tools.push(Arc::new(DapTool::new("dap".to_owned(), None, all)));
    tools
}

struct DapTool {
    name: String,
    /// Adapter pinned by a `dap_<id>` tool; `None` for the `dap` tool, which selects per session.
    requested: Option<String>,
    catalogue: Arc<BTreeMap<String, DapAdapterConfig>>,
    session: Mutex<Option<Session>>,
    active: Mutex<Option<Arc<AdapterProcess>>>,
    cancellation: AtomicU64,
    interrupted: AtomicBool,
}

impl Tool for DapTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn schema(&self) -> Value {
        let mut properties = json!({
            "action": {"type":"string","enum":["initialize","launch","attach","setBreakpoints","removeBreakpoint","setFunctionBreakpoints","removeFunctionBreakpoint","configurationDone","threads","pause","status","output","continue","next","stepIn","stepOut","stackTrace","scopes","variables","evaluate","disconnect","terminate"]},
            "program": {"type":"string","description":"launch: program path inside the workspace"},
            "cwd": {"type":"string","description":"launch: working directory inside the workspace (defaults to workspace root)"},
            "args": {"type":"array","items":{"type":"string"},"description":"launch: program arguments"},
            "arguments": {"type":"object","description":"Action-specific DAP arguments (attach, breakpoints, threads, frames, variables, or expression)"},
            "source": {"type":"object","description":"setBreakpoints/removeBreakpoint: DAP source descriptor (path or name keys the session list)"},
            "line": {"type":"integer","description":"removeBreakpoint: source line to remove"},
            "name": {"type":"string","description":"removeFunctionBreakpoint: function name to remove"},
            "breakpoints": {"type":"array","items":{"type":"object"},"description":"setBreakpoints: requested breakpoints (merged into the session list per line); setFunctionBreakpoints: [{name}] entries (merged by name)"},
            "threadId": {"type":"integer"}, "frameId": {"type":"integer"},
            "variablesReference": {"type":"integer"}, "expression": {"type":"string"},
            "terminateDebuggee": {"type":"boolean"}
        });
        if self.requested.is_none() {
            properties["adapter"] = json!({"type":"string","description":"Adapter id from the catalogue (gdb, lldb-dap, debugpy, dlv, ...); omitted selects by launch program extension and project root markers"});
        }
        tool_schema(&self.name, DESCRIPTION, properties, &["action"])
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }

    fn fork_session(&self) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self::new(
            self.name.clone(),
            self.requested.clone(),
            Arc::clone(&self.catalogue),
        )))
    }

    fn cancel_operation(&self) {
        self.interrupted.store(true, Ordering::Release);
        self.cancellation.fetch_add(1, Ordering::AcqRel);
        if let Some(process) = self
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            process.terminate();
        }
    }

    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        self.execute(ctx, args, &AtomicBool::new(false))
    }

    fn call_streaming(
        &self,
        ctx: &ToolContext,
        args: &Value,
        _on_event: &(dyn Fn(ToolEvent) + Sync),
        cancel: &AtomicBool,
    ) -> ToolOutput {
        self.execute(ctx, args, cancel)
    }
}

impl DapTool {
    fn new(
        name: String,
        requested: Option<String>,
        catalogue: Arc<BTreeMap<String, DapAdapterConfig>>,
    ) -> Self {
        Self {
            name,
            requested,
            catalogue,
            session: Mutex::new(None),
            active: Mutex::new(None),
            cancellation: AtomicU64::new(0),
            interrupted: AtomicBool::new(false),
        }
    }

    /// A pinned `dap_<id>` tool keeps its adapter; the `dap` tool honors `adapter`, else selects
    /// by launch program extension and root markers.
    fn select_adapter(
        &self,
        workspace: &Workspace,
        args: &Value,
        arguments: &Value,
    ) -> DapResult<(String, DapAdapterConfig)> {
        let adapter = args.get("adapter").filter(|value| !value.is_null());
        let requested = match (&self.requested, adapter) {
            (Some(id), _) => Some(id.as_str()),
            (None, None) => None,
            (None, Some(value)) => Some(
                value
                    .as_str()
                    .ok_or_else(|| DapError::user("adapter must be a string"))?,
            ),
        };
        let program = arguments
            .get("program")
            .and_then(Value::as_str)
            .map(Path::new);
        select::select(&self.catalogue, requested, program, workspace.root())
            .map_err(DapError::user)
    }

    /// The `dap` tool's deadline follows the selected adapter; pinned tools set theirs at entry.
    fn bind_deadline(&self, op: &Operation<'_>, timeout_secs: u64) {
        if self.requested.is_none() {
            op.end
                .set(super::kernel::execution_deadline(Duration::from_secs(
                    timeout_secs,
                )));
        }
    }

    fn execute(&self, ctx: &ToolContext, args: &Value, cancel: &AtomicBool) -> ToolOutput {
        // Do not erase a cancellation delivered just before this call entered.
        if self.interrupted.swap(false, Ordering::AcqRel) {
            return ToolOutput::err("DAP operation cancelled before entry; no adapter started");
        }
        let timeout = self
            .requested
            .as_ref()
            .and_then(|id| self.catalogue.get(id))
            .map_or(0, |config| config.timeout_secs);
        let operation = Operation {
            end: Cell::new(super::kernel::execution_deadline(Duration::from_secs(
                timeout,
            ))),
            generation: self.cancellation.load(Ordering::Acquire),
            cancellation: &self.cancellation,
            interrupted: &self.interrupted,
            cancel,
        };
        let result = self.call_inner(ctx.workspace, args, &operation);
        let (full, failed) = match result {
            Ok(value) => (value.to_string(), false),
            Err(error) => (error.message, true),
        };
        let mut text = bounded(&full);
        if full.len() > MAX_OUTPUT
            && let Some(store) = ctx.workspace.artifacts()
        {
            match store.retain(&full) {
                Ok(uri) => text.push_str(&format!("\n[Complete debugger evidence: {uri}]")),
                Err(error) => {
                    return ToolOutput::err(format!("retaining debugger evidence: {error:#}"));
                }
            }
        }
        if failed {
            ToolOutput::err(text)
        } else {
            ToolOutput::ok(text)
        }
    }

    fn call_inner(
        &self,
        workspace: &Workspace,
        args: &Value,
        op: &Operation<'_>,
    ) -> DapResult<Value> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| DapError::user("action must be a string"))?;
        // Validate before acquiring or modifying a healthy session.
        let arguments = match action {
            "launch" => launch_arguments(workspace, args).map_err(DapError::user)?,
            "setBreakpoints" => breakpoint_arguments(args).map_err(DapError::user)?,
            "removeBreakpoint" => remove_breakpoint_arguments(args).map_err(DapError::user)?,
            "setFunctionBreakpoints" => {
                function_breakpoint_arguments(args).map_err(DapError::user)?
            }
            "removeFunctionBreakpoint" => {
                remove_function_breakpoint_arguments(args).map_err(DapError::user)?
            }
            "initialize" | "configurationDone" | "status" | "output" | "terminate" => json!({}),
            "attach" | "threads" | "pause" | "continue" | "next" | "stepIn" | "stepOut"
            | "stackTrace" | "scopes" | "variables" | "evaluate" => {
                action_arguments(args).map_err(DapError::user)?
            }
            "disconnect" => {
                let mut value = action_arguments(args).map_err(DapError::user)?;
                if let Some(flag) = args.get("terminateDebuggee").filter(|flag| !flag.is_null()) {
                    if !flag.is_boolean() {
                        return Err(DapError::user("terminateDebuggee must be boolean"));
                    }
                    value
                        .as_object_mut()
                        .unwrap()
                        .insert("terminateDebuggee".into(), flag.clone());
                }
                value
            }
            _ => return Err(DapError::user(format!("unsupported DAP action {action:?}"))),
        };
        let mut slot = loop {
            op.check()?;
            match self.session.try_lock() {
                Ok(slot) => break slot,
                Err(std::sync::TryLockError::Poisoned(error)) => break error.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => thread::sleep(POLL),
            }
        };
        if slot.is_none() && matches!(action, "status" | "output") {
            return Ok(json!({"active":false,"events":[]}));
        }
        if slot.is_none() && matches!(action, "disconnect" | "terminate") {
            return Err(DapError::user("no active DAP session"));
        }
        if slot.is_none() {
            let (adapter, config) = self.select_adapter(workspace, args, &arguments)?;
            let session = Session::start(&config, workspace.root(), &self.active, op)?;
            *self.active.lock().unwrap_or_else(|e| e.into_inner()) = Some(session.process.clone());
            *slot = Some(session);
            self.bind_deadline(op, config.timeout_secs);
            let result = slot.as_mut().unwrap().initialize(&adapter, op);
            if let Err(mut error) = result {
                error
                    .message
                    .push_str(&slot.as_ref().unwrap().failure_evidence());
                slot.take();
                self.active.lock().unwrap_or_else(|e| e.into_inner()).take();
                return Err(error);
            }
        }
        let session = slot.as_mut().unwrap();
        self.bind_deadline(op, session.timeout_secs);
        let mut result = session.perform(action, arguments, op);
        if let Err(error) = &mut result
            && error.fatal
        {
            error.message.push_str(&session.failure_evidence());
        }
        // User/capability/adapter operation errors do not poison the transport.
        let close = matches!(action, "disconnect" | "terminate") && result.is_ok()
            || result.as_ref().err().is_some_and(|error| error.fatal);
        if close {
            slot.take();
            self.active.lock().unwrap_or_else(|e| e.into_inner()).take();
        }
        result
    }
}

fn launch_arguments(workspace: &Workspace, args: &Value) -> Result<Value, String> {
    let program = args
        .get("program")
        .and_then(Value::as_str)
        .ok_or("launch requires program")?;
    let program = workspace.resolve(program)?;
    let program = canonical_inside(workspace, &program, "program")?;
    let cwd = match args.get("cwd").and_then(Value::as_str) {
        Some(path) => canonical_inside(workspace, &workspace.resolve(path)?, "cwd")?,
        None => workspace.root().to_path_buf(),
    };
    let mut result = action_arguments(args)?.as_object().unwrap().clone();
    result.insert("program".into(), json!(program));
    result.insert("cwd".into(), json!(cwd));
    if let Some(values) = args.get("args").filter(|values| !values.is_null()) {
        if !values
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_string))
        {
            return Err("launch args must be an array of strings".into());
        }
        result.insert("args".into(), values.clone());
    }
    Ok(Value::Object(result))
}

fn canonical_inside(workspace: &Workspace, path: &Path, label: &str) -> Result<PathBuf, String> {
    let resolved = path
        .canonicalize()
        .map_err(|error| format!("cannot resolve launch {label}: {error}"))?;
    if !resolved.starts_with(workspace.root()) {
        return Err(format!("launch {label} resolves outside the workspace"));
    }
    if label == "cwd" && !resolved.is_dir() {
        return Err("launch cwd must be a directory".into());
    }
    if label == "program" && !resolved.is_file() {
        return Err("launch program must be a file".into());
    }
    Ok(resolved)
}

fn action_arguments(args: &Value) -> Result<Value, String> {
    let mut value = args
        .get("arguments")
        .filter(|value| !value.is_null())
        .cloned()
        .unwrap_or_else(|| {
            let mut object = serde_json::Map::new();
            for key in ["threadId", "frameId", "variablesReference", "expression"] {
                if let Some(value) = args.get(key).filter(|value| !value.is_null()) {
                    object.insert(key.into(), value.clone());
                }
            }
            Value::Object(object)
        });
    if !value.is_object() {
        return Err("arguments must be an object".into());
    }
    // Null optional fields are absent: presence checks downstream must not see them.
    if let Some(object) = value.as_object_mut() {
        object.retain(|_, field| !field.is_null());
    }
    for key in ["threadId", "frameId", "variablesReference"] {
        if let Some(id) = value.get(key)
            && !id.as_i64().is_some_and(|id| id >= 0)
        {
            return Err(format!("{key} must be a nonnegative integer"));
        }
    }
    if value
        .get("expression")
        .is_some_and(|value| !value.is_string())
    {
        return Err("expression must be a string".into());
    }
    Ok(value)
}

/// Drops null-valued fields from an object, or from each object in an array.
fn without_null_fields(value: &Value) -> Value {
    fn strip(object: &mut Value) {
        if let Some(fields) = object.as_object_mut() {
            fields.retain(|_, field| !field.is_null());
        }
    }
    let mut value = value.clone();
    match value.as_array_mut() {
        Some(items) => items.iter_mut().for_each(strip),
        None => strip(&mut value),
    }
    value
}

fn breakpoint_arguments(args: &Value) -> Result<Value, String> {
    let mut value = action_arguments(args)?;
    let object = value.as_object_mut().expect("validated object");
    if let Some(source) = args.get("source").filter(|source| !source.is_null()) {
        object.insert("source".into(), without_null_fields(source));
    }
    if let Some(breakpoints) = args.get("breakpoints").filter(|value| !value.is_null()) {
        object.insert("breakpoints".into(), without_null_fields(breakpoints));
    }
    if !object.get("source").is_some_and(Value::is_object)
        || !object
            .get("breakpoints")
            .and_then(Value::as_array)
            .is_some_and(|values| values.iter().all(Value::is_object))
    {
        return Err(
            "setBreakpoints requires a source object and an array of breakpoint objects".into(),
        );
    }
    Ok(value)
}

fn remove_breakpoint_arguments(args: &Value) -> Result<Value, String> {
    let mut value = action_arguments(args)?;
    let source = args
        .get("source")
        .filter(|source| source.is_object())
        .ok_or("removeBreakpoint requires a source object")?;
    let line = args
        .get("line")
        .and_then(Value::as_i64)
        .ok_or("removeBreakpoint requires an integer line")?;
    let object = value.as_object_mut().expect("validated object");
    object.insert("source".into(), source.clone());
    object.insert("line".into(), json!(line));
    Ok(value)
}

fn function_breakpoint_arguments(args: &Value) -> Result<Value, String> {
    let mut value = action_arguments(args)?;
    let breakpoints = args
        .get("breakpoints")
        .and_then(Value::as_array)
        .filter(|values| {
            values
                .iter()
                .all(|entry| entry.get("name").is_some_and(Value::is_string))
        })
        .ok_or("setFunctionBreakpoints requires an array of objects with a string name")?;
    let object = value.as_object_mut().expect("validated object");
    object.insert(
        "breakpoints".into(),
        without_null_fields(&json!(breakpoints)),
    );
    Ok(value)
}

fn remove_function_breakpoint_arguments(args: &Value) -> Result<Value, String> {
    let mut value = action_arguments(args)?;
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .ok_or("removeFunctionBreakpoint requires a string name")?;
    let object = value.as_object_mut().expect("validated object");
    object.insert("name".into(), json!(name));
    Ok(value)
}

fn is_breakpoint_action(action: &str) -> bool {
    matches!(
        action,
        "setBreakpoints"
            | "removeBreakpoint"
            | "setFunctionBreakpoints"
            | "removeFunctionBreakpoint"
    )
}

#[derive(Debug)]
struct DapError {
    message: String,
    fatal: bool,
}
type DapResult<T> = Result<T, DapError>;
impl DapError {
    fn user(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            fatal: false,
        }
    }
    fn transport(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            fatal: true,
        }
    }
}

struct Operation<'a> {
    end: Cell<Option<Instant>>,
    generation: u64,
    cancellation: &'a AtomicU64,
    cancel: &'a AtomicBool,
    interrupted: &'a AtomicBool,
}
impl Operation<'_> {
    fn check(&self) -> DapResult<()> {
        if self.cancel.load(Ordering::Acquire)
            || self.interrupted.load(Ordering::Acquire)
            || self.cancellation.load(Ordering::Acquire) != self.generation
        {
            return Err(DapError::transport(
                "DAP operation cancelled; session destroyed",
            ));
        }
        if self.end.get().is_some_and(|end| Instant::now() >= end) {
            return Err(DapError::transport(
                "DAP operation timed out; session destroyed",
            ));
        }
        Ok(())
    }
}

// Reserve the leader PID until killpg, and never join a reader or block a caller in wait().
struct AdapterProcess {
    group: Mutex<Option<ProcessGroup>>,
    stopping: AtomicBool,
}
impl AdapterProcess {
    fn terminate(&self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(group) = self
            .group
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            group.terminate();
        }
    }
    fn ended(&self) -> bool {
        if self.stopping.load(Ordering::Acquire) {
            return true;
        }
        let exited = self
            .group
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_none_or(|group| group.exited().unwrap_or(false));
        if exited {
            self.terminate();
        }
        exited
    }
}
impl Drop for AdapterProcess {
    fn drop(&mut self) {
        if let Some(mut group) = self
            .group
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            group.terminate();
            thread::spawn(move || {
                // Drop's wait() runs only after WNOWAIT reports exit; never blocking reap.
                loop {
                    match group.exited() {
                        Ok(true) => break,
                        Ok(false) | Err(_) => thread::sleep(POLL),
                    }
                }
                drop(group);
            });
        }
    }
}

struct AdapterPipe<R> {
    reader: R,
    process: Arc<AdapterProcess>,
    drain_end: Option<Instant>,
}
impl<R: Read> Read for AdapterPipe<R> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.process.ended() && self.drain_end.is_none() {
                self.drain_end = Some(Instant::now() + Duration::from_millis(250));
            }
            if self.drain_end.is_some_and(|end| Instant::now() >= end) {
                return Ok(0);
            }
            match self.reader.read(bytes) {
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(POLL),
                result => return result,
            }
        }
    }
}

fn nonblocking(pipe: &impl crate::platform::io::Pipe) -> DapResult<()> {
    crate::platform::io::set_nonblocking(pipe)
        .map_err(|error| DapError::transport(format!("making DAP pipe nonblocking: {error}")))
}

struct Pending {
    command: String,
    response: Option<Value>,
}

#[derive(Default)]
struct ProtocolState {
    pending: BTreeMap<i64, Pending>,
    events: VecDeque<(Value, usize)>,
    event_bytes: usize,
    dropped_events: u64,
    dropped_bytes: u64,
    failure: Option<String>,
    initialized_seen: bool,
    status: String,
    stopped: Option<Value>,
    threads: BTreeMap<i64, Value>,
    capabilities: Value,
}
impl ProtocolState {
    fn event(&mut self, mut event: Value) {
        let serialized = event.to_string();
        let original = serialized.len();
        // A single huge output event must not defeat the evidence ring bound.
        let bytes = if original > MAX_OUTPUT {
            event = json!({"type":"event","event":event.get("event"),"body":{"truncated":true,"evidence":bounded(&serialized)}});
            let retained = event.to_string().len();
            self.dropped_bytes = self
                .dropped_bytes
                .saturating_add(original.saturating_sub(MAX_OUTPUT) as u64);
            retained
        } else {
            original
        };
        while self.events.len() >= MAX_EVENTS || self.event_bytes + bytes > EVENT_BYTES {
            if let Some((_, removed)) = self.events.pop_front() {
                self.event_bytes -= removed;
                self.dropped_events = self.dropped_events.saturating_add(1);
                self.dropped_bytes = self.dropped_bytes.saturating_add(removed as u64);
            } else {
                break;
            }
        }
        self.event_bytes += bytes;
        self.events.push_back((event, bytes));
    }
    fn summary(&self, configured: bool, start: Option<i64>, launched: bool) -> Value {
        json!({
            "active":self.failure.is_none() && self.status != "terminated","state":self.status,"initialized":self.initialized_seen,
            "configurationDone":configured,"needsConfigurationDone":!configured && launched,
            "startRequestPending":start.is_some(),"stopped":self.stopped,
            "threads":self.threads.values().collect::<Vec<_>>(),"capabilities":self.capabilities,
            "failure":self.failure,"retainedEvents":self.events.len(),
            "droppedEvents":self.dropped_events,"droppedBytes":self.dropped_bytes,
        })
    }
}

struct Wire {
    input: Mutex<Box<dyn Write + Send>>,
    state: Mutex<ProtocolState>,
    changed: Condvar,
    next_seq: AtomicI64,
    process: Arc<AdapterProcess>,
    timeout: Duration,
}
impl Wire {
    fn fail(&self, message: impl Into<String>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.failure.is_none() {
            state.failure = Some(message.into());
        }
        if state.status != "terminated" {
            state.status = "failed".into();
        }
        self.changed.notify_all();
        drop(state);
        self.process.terminate();
    }
    fn seq(&self) -> DapResult<i64> {
        self.next_seq
            .try_update(Ordering::AcqRel, Ordering::Acquire, |seq| {
                seq.checked_add(1)
            })
            .map_err(|_| DapError::transport("DAP sequence exhausted"))
    }
    fn write(
        &self,
        body: &str,
        end: Option<Instant>,
        check: impl Fn() -> DapResult<()>,
    ) -> DapResult<()> {
        if body.len() > MAX_FRAME {
            return Err(DapError::user("DAP request exceeds frame limit"));
        }
        let mut input = loop {
            check()?;
            if end.is_some_and(|end| Instant::now() >= end) {
                return Err(DapError::transport("DAP stdin write timed out"));
            }
            if self.process.ended() {
                return Err(DapError::transport("DAP adapter exited during write"));
            }
            match self.input.try_lock() {
                Ok(input) => break input,
                Err(std::sync::TryLockError::Poisoned(error)) => break error.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => thread::sleep(POLL),
            }
        };
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        for bytes in [header.as_bytes(), body.as_bytes()] {
            let mut offset = 0;
            while offset < bytes.len() {
                check()?;
                if end.is_some_and(|end| Instant::now() >= end) {
                    return Err(DapError::transport("DAP stdin write timed out"));
                }
                if self.process.ended() {
                    return Err(DapError::transport("DAP adapter exited during write"));
                }
                match input.write(&bytes[offset..]) {
                    Ok(0) => return Err(DapError::transport("DAP stdin closed during write")),
                    Ok(count) => offset += count,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(POLL)
                    }
                    Err(error) => {
                        return Err(DapError::transport(format!("writing DAP request: {error}")));
                    }
                }
            }
        }
        loop {
            check()?;
            if end.is_some_and(|end| Instant::now() >= end) {
                return Err(DapError::transport("DAP stdin delivery timed out"));
            }
            if self.process.ended() {
                return Err(DapError::transport("DAP adapter exited during delivery"));
            }
            match input.flush() {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(POLL),
                Err(error) => {
                    return Err(DapError::transport(format!(
                        "delivering DAP request: {error}"
                    )));
                }
            }
        }
    }
}

struct SocketCleanup(Option<tempfile::TempDir>);

fn startup_timeout(config: &DapAdapterConfig) -> Duration {
    Duration::from_secs(if config.timeout_secs == 0 {
        10
    } else {
        config.timeout_secs.min(30)
    })
}

fn wait_tcp(
    process: &AdapterProcess,
    address: std::net::SocketAddr,
    timeout: Duration,
    op: &Operation<'_>,
) -> DapResult<TcpStream> {
    let end = Instant::now() + timeout;
    loop {
        op.check()?;
        if process.ended() {
            return Err(DapError::transport(
                "DAP adapter exited before opening its TCP endpoint",
            ));
        }
        match TcpStream::connect_timeout(&address, Duration::from_millis(100)) {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() < end => {
                if !matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::AddrNotAvailable
                ) {
                    return Err(DapError::transport(format!(
                        "connecting to DAP adapter at {address}: {error}"
                    )));
                }
                thread::sleep(POLL);
            }
            Err(_) => {
                return Err(DapError::transport(format!(
                    "DAP adapter did not listen at {address} before startup timed out"
                )));
            }
        }
    }
}

#[cfg(unix)]
fn wait_unix(
    process: &AdapterProcess,
    path: &Path,
    timeout: Duration,
    op: &Operation<'_>,
) -> DapResult<UnixStream> {
    let end = Instant::now() + timeout;
    loop {
        op.check()?;
        if process.ended() {
            return Err(DapError::transport(
                "DAP adapter exited before opening its Unix socket",
            ));
        }
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() < end => {
                if error.kind() != std::io::ErrorKind::NotFound
                    && error.kind() != std::io::ErrorKind::ConnectionRefused
                {
                    return Err(DapError::transport(format!(
                        "connecting to DAP Unix socket: {error}"
                    )));
                }
                thread::sleep(POLL);
            }
            Err(_) => {
                return Err(DapError::transport(
                    "DAP adapter did not open its Unix socket before startup timed out",
                ));
            }
        }
    }
}

#[cfg(not(unix))]
fn wait_accept(
    process: &AdapterProcess,
    listener: TcpListener,
    timeout: Duration,
    op: &Operation<'_>,
) -> DapResult<TcpStream> {
    let end = Instant::now() + timeout;
    loop {
        op.check()?;
        if process.ended() {
            return Err(DapError::transport(
                "DAP adapter exited before connecting to its callback endpoint",
            ));
        }
        match listener.accept() {
            Ok((stream, _)) => return Ok(stream),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < end =>
            {
                thread::sleep(POLL)
            }
            Err(error) => {
                return Err(DapError::transport(format!(
                    "waiting for DAP adapter callback: {error}"
                )));
            }
        }
    }
}

struct Session {
    process: Arc<AdapterProcess>,
    wire: Arc<Wire>,
    initialized: Value,
    start_request: Option<i64>,
    configured: bool,
    launched: bool,
    timeout_secs: u64,
    book: breakpoints::Book,
    _socket_dir: Option<tempfile::TempDir>,
}
impl Drop for Session {
    fn drop(&mut self) {
        self.process.terminate();
    }
}
struct ActiveGuard<'a> {
    slot: &'a Mutex<Option<Arc<AdapterProcess>>>,
    process: Arc<AdapterProcess>,
    keep: bool,
}
impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        if !self.keep {
            self.process.terminate();
            let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
            if slot
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, &self.process))
            {
                slot.take();
            }
        }
    }
}
impl Session {
    fn start(
        config: &DapAdapterConfig,
        root: &Path,
        active: &Mutex<Option<Arc<AdapterProcess>>>,
        op: &Operation<'_>,
    ) -> DapResult<Self> {
        #[cfg(not(unix))]
        let mut tcp_listener: Option<TcpListener> = None;
        let mut args = config.args.clone();
        // Assigned only by the Unix socket transport below.
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut cleanup_dir: Option<tempfile::TempDir> = None;
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut socket_path: Option<PathBuf> = None;
        let mut tcp_address = None;
        match config.transport {
            DapTransport::Stdio => {}
            DapTransport::Tcp => {
                let address = if config.address.is_empty() {
                    let reservation = TcpListener::bind(("127.0.0.1", 0)).map_err(|error| {
                        DapError::user(format!("reserving DAP TCP endpoint: {error}"))
                    })?;
                    reservation
                        .local_addr()
                        .map_err(|error| DapError::user(error.to_string()))?
                } else {
                    config.address.parse().map_err(|error| {
                        DapError::user(format!("invalid DAP TCP address: {error}"))
                    })?
                };
                for arg in &mut args {
                    *arg = arg.replace("${port}", &address.port().to_string());
                }
                tcp_address = Some(address);
            }
            DapTransport::Socket => {
                #[cfg(unix)]
                {
                    let path = if config.address.is_empty() {
                        let dir = tempfile::Builder::new()
                            .prefix("dial-dap")
                            .tempdir()
                            .map_err(|error| {
                                DapError::user(format!(
                                    "creating private DAP socket directory: {error}"
                                ))
                            })?;
                        let path = dir.path().join("adapter.sock");
                        cleanup_dir = Some(dir);
                        path
                    } else {
                        PathBuf::from(&config.address)
                    };
                    args.push(format!("--listen=unix:{}", path.display()));
                    socket_path = Some(path);
                }
                #[cfg(not(unix))]
                {
                    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|error| {
                        DapError::user(format!("binding DAP callback endpoint: {error}"))
                    })?;
                    listener
                        .set_nonblocking(true)
                        .map_err(|error| DapError::user(error.to_string()))?;
                    let address = listener
                        .local_addr()
                        .map_err(|error| DapError::user(error.to_string()))?;
                    tcp_address = Some(address);
                    args.push(format!("--client-addr={address}"));
                    tcp_listener = Some(listener);
                }
            }
        }
        let mut env = config.env.clone();
        if let Some(address) = tcp_address {
            for value in env.values_mut() {
                *value = value.replace("${port}", &address.port().to_string());
            }
        }
        if let Some(path) = &socket_path {
            for value in env.values_mut() {
                *value = value.replace("${socket}", &path.to_string_lossy());
            }
        }
        let mut cleanup = SocketCleanup(cleanup_dir);
        let mut command = Command::new(&config.program);
        let piped = config.transport == DapTransport::Stdio;
        command
            .args(&args)
            .envs(&env)
            .current_dir(root)
            .stdin(if piped { Stdio::piped() } else { Stdio::null() })
            .stdout(if piped { Stdio::piped() } else { Stdio::null() })
            .stderr(Stdio::piped());
        let child = crate::platform::process::spawn_group(&mut command).map_err(|error| {
            DapError::user(format!(
                "failed to start DAP adapter (install/configure its executable): {error}"
            ))
        })?;
        let process = Arc::new(AdapterProcess {
            group: Mutex::new(Some(ProcessGroup::new(child))),
            stopping: AtomicBool::new(false),
        });
        *active.lock().unwrap_or_else(|e| e.into_inner()) = Some(process.clone());
        let mut active_guard = ActiveGuard {
            slot: active,
            process: process.clone(),
            keep: false,
        };
        let stderr = process
            .group
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
            .unwrap()
            .child
            .stderr
            .take()
            .ok_or_else(|| DapError::transport("DAP stderr unavailable"))?;
        let (input, output, stderr): (
            Box<dyn Write + Send>,
            Box<dyn Read + Send>,
            Box<dyn Read + Send>,
        ) = match config.transport {
            DapTransport::Stdio => {
                let mut guard = process.group.lock().unwrap_or_else(|e| e.into_inner());
                let child = &mut guard.as_mut().unwrap().child;
                let input = child
                    .stdin
                    .take()
                    .ok_or_else(|| DapError::transport("DAP stdin unavailable"))?;
                let output = child
                    .stdout
                    .take()
                    .ok_or_else(|| DapError::transport("DAP stdout unavailable"))?;
                nonblocking(&input)?;
                nonblocking(&output)?;
                nonblocking(&stderr)?;
                #[cfg(windows)]
                let input = crate::platform::io::QueuedWriter::new(input, 256 * 1024);
                (Box::new(input), Box::new(output), Box::new(stderr))
            }
            DapTransport::Tcp => {
                let address = tcp_address.unwrap();
                let stream = wait_tcp(&process, address, startup_timeout(config), op)?;
                let reader = stream
                    .try_clone()
                    .map_err(|error| DapError::transport(error.to_string()))?;
                stream
                    .set_nonblocking(true)
                    .map_err(|error| DapError::transport(error.to_string()))?;
                reader
                    .set_nonblocking(true)
                    .map_err(|error| DapError::transport(error.to_string()))?;
                nonblocking(&stderr)?;
                (Box::new(stream), Box::new(reader), Box::new(stderr))
            }
            DapTransport::Socket => {
                #[cfg(unix)]
                {
                    let path = socket_path.as_ref().unwrap();
                    let stream = wait_unix(&process, path, startup_timeout(config), op)?;
                    let reader = stream
                        .try_clone()
                        .map_err(|error| DapError::transport(error.to_string()))?;
                    stream
                        .set_nonblocking(true)
                        .map_err(|error| DapError::transport(error.to_string()))?;
                    reader
                        .set_nonblocking(true)
                        .map_err(|error| DapError::transport(error.to_string()))?;
                    nonblocking(&stderr)?;
                    (Box::new(stream), Box::new(reader), Box::new(stderr))
                }
                #[cfg(not(unix))]
                {
                    let stream = wait_accept(
                        &process,
                        tcp_listener.take().unwrap(),
                        startup_timeout(config),
                        op,
                    )?;
                    let reader = stream
                        .try_clone()
                        .map_err(|error| DapError::transport(error.to_string()))?;
                    stream
                        .set_nonblocking(true)
                        .map_err(|error| DapError::transport(error.to_string()))?;
                    reader
                        .set_nonblocking(true)
                        .map_err(|error| DapError::transport(error.to_string()))?;
                    nonblocking(&stderr)?;
                    (Box::new(stream), Box::new(reader), Box::new(stderr))
                }
            }
        };
        let output = AdapterPipe {
            reader: output,
            process: process.clone(),
            drain_end: None,
        };
        let stderr = AdapterPipe {
            reader: stderr,
            process: process.clone(),
            drain_end: None,
        };
        let wire = Arc::new(Wire {
            input: Mutex::new(input),
            state: Mutex::new(ProtocolState {
                status: "initializing".into(),
                ..ProtocolState::default()
            }),
            changed: Condvar::new(),
            next_seq: AtomicI64::new(1),
            process: process.clone(),
            timeout: Duration::from_secs(config.timeout_secs),
        });
        let reader = wire.clone();
        if let Err(error) = thread::Builder::new()
            .name("dap-protocol-reader".into())
            .spawn(move || read_messages(output, reader))
        {
            wire.fail(format!("starting DAP reader: {error}"));
            return Err(DapError::transport(format!("starting DAP reader: {error}")));
        }
        let evidence = wire.clone();
        if let Err(error) = thread::Builder::new().name("dap-stderr-reader".into()).spawn(move || {
            let mut stderr = stderr;
            let mut buffer = [0; 4096];
            let mut pending: Vec<u8> = Vec::new();
            let emit = |text: String| {
                if text.is_empty() {
                    return;
                }
                let mut state = evidence.state.lock().unwrap_or_else(|e| e.into_inner());
                state.event(json!({"type":"event","event":"output","body":{"category":"adapter-stderr","output":text}}));
                evidence.changed.notify_all();
            };
            loop {
                match stderr.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        pending.extend_from_slice(&buffer[..count]);
                        // Keep an incomplete trailing UTF-8 sequence for the next read.
                        let cut = match std::str::from_utf8(&pending) {
                            Err(error) if error.error_len().is_none() => error.valid_up_to(),
                            _ => pending.len(),
                        };
                        let text = String::from_utf8_lossy(&pending[..cut]).into_owned();
                        pending.drain(..cut);
                        emit(text);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            emit(String::from_utf8_lossy(&pending).into_owned());
        }) {
            wire.fail(format!("starting DAP stderr reader: {error}"));
            return Err(DapError::transport(format!("starting DAP stderr reader: {error}")));
        }
        active_guard.keep = true;
        Ok(Self {
            process,
            wire,
            initialized: json!({}),
            start_request: None,
            configured: false,
            launched: false,
            timeout_secs: config.timeout_secs,
            book: breakpoints::Book::default(),
            // The path is only needed while connecting; `_socket_dir` owns its cleanup.
            _socket_dir: cleanup.0.take(),
        })
    }
    fn initialize(&mut self, id: &str, op: &Operation<'_>) -> DapResult<()> {
        self.initialized = self.request(
            "initialize",
            json!({
                "clientID":"dial","clientName":"dial","adapterID":id,"pathFormat":"path",
                "linesStartAt1":true,"columnsStartAt1":true,"supportsVariableType":true,
                "supportsRunInTerminalRequest":false,"supportsStartDebuggingRequest":false,
            }),
            op,
        )?;
        if !self.initialized.is_object() {
            return Err(DapError::transport(
                "DAP initialize capabilities must be an object",
            ));
        }
        self.wire
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .capabilities = self.initialized.clone();
        Ok(())
    }
    fn supports(&self, name: &str) -> bool {
        self.wire
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .capabilities
            .get(name)
            .and_then(Value::as_bool)
            == Some(true)
    }
    fn send(&self, command: &str, arguments: Value, op: &Operation<'_>) -> DapResult<i64> {
        op.check()?;
        let seq = self.wire.seq()?;
        let body =
            json!({"seq":seq,"type":"request","command":command,"arguments":arguments}).to_string();
        if body.len() > MAX_FRAME {
            return Err(DapError::user("DAP request exceeds frame limit"));
        }
        {
            let mut state = self.wire.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(error) = &state.failure {
                return Err(DapError::transport(error.clone()));
            }
            state.pending.insert(
                seq,
                Pending {
                    command: command.into(),
                    response: None,
                },
            );
        }
        self.wire.write(&body, op.end.get(), || op.check())?;
        Ok(seq)
    }
    fn wait(&self, op: &Operation<'_>, ready: impl Fn(&ProtocolState) -> bool) -> DapResult<()> {
        let mut state = self.wire.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            op.check()?;
            // A final response is usable even if EOF immediately follows it.
            if ready(&state) {
                return Ok(());
            }
            if let Some(error) = &state.failure {
                return Err(DapError::transport(error.clone()));
            }
            state = self
                .wire
                .changed
                .wait_timeout(state, POLL)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
    fn response(&self, seq: i64, op: &Operation<'_>) -> DapResult<Value> {
        self.wait(op, |state| {
            state
                .pending
                .get(&seq)
                .is_some_and(|pending| pending.response.is_some())
        })?;
        let pending = self
            .wire
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .remove(&seq)
            .unwrap();
        let mut value = pending.response.unwrap();
        if value.get("success").and_then(Value::as_bool) != Some(true) {
            return Err(DapError::user(format!(
                "DAP {} failed: {}",
                pending.command,
                value
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("adapter reported failure")
            )));
        }
        Ok(value
            .as_object_mut()
            .and_then(|object| object.remove("body"))
            .unwrap_or_else(|| json!({})))
    }
    fn request(&self, command: &str, arguments: Value, op: &Operation<'_>) -> DapResult<Value> {
        let seq = self.send(command, arguments, op)?;
        self.response(seq, op)
    }
    /// Breakpoint actions merge into the session book, then resend the full list that DAP requires.
    /// The book changes only after the adapter accepts the request.
    fn breakpoint_action(
        &mut self,
        action: &str,
        arguments: Value,
        op: &Operation<'_>,
    ) -> DapResult<Value> {
        match action {
            "setBreakpoints" | "removeBreakpoint" => {
                let source = arguments["source"].clone();
                let key = breakpoints::source_key(&source)
                    .ok_or_else(|| DapError::user("source needs a path or name"))?;
                let current = self.book.sources.get(&key).cloned().unwrap_or_default();
                let requested = arguments["breakpoints"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let next = if action == "removeBreakpoint" {
                    breakpoints::remove_source(
                        &current,
                        arguments["line"].as_i64().unwrap_or_default(),
                    )
                } else if requested.is_empty() {
                    Vec::new()
                } else {
                    requested
                        .into_iter()
                        .fold(current, |list, entry| breakpoints::add_source(&list, entry))
                };
                let body = self.request(
                    "setBreakpoints",
                    json!({"source": source, "breakpoints": breakpoints::request_entries(&next)}),
                    op,
                )?;
                let annotated = breakpoints::annotate(&next, &body);
                if annotated.is_empty() {
                    self.book.sources.remove(&key);
                } else {
                    self.book.sources.insert(key, annotated);
                }
                Ok(body)
            }
            "setFunctionBreakpoints" | "removeFunctionBreakpoint" => {
                let current = self.book.functions.clone();
                let requested = arguments["breakpoints"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let next = if action == "removeFunctionBreakpoint" {
                    breakpoints::remove_function(
                        &current,
                        arguments["name"].as_str().unwrap_or_default(),
                    )
                } else if requested.is_empty() {
                    Vec::new()
                } else {
                    requested.into_iter().fold(current, |list, entry| {
                        breakpoints::add_function(&list, entry)
                    })
                };
                let body = self.request(
                    "setFunctionBreakpoints",
                    json!({"breakpoints": breakpoints::request_entries(&next)}),
                    op,
                )?;
                self.book.functions = breakpoints::annotate(&next, &body);
                Ok(body)
            }
            _ => Err(DapError::user(format!(
                "unsupported breakpoint action {action:?}"
            ))),
        }
    }
    fn summary(&self) -> Value {
        let mut summary = self
            .wire
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .summary(self.configured, self.start_request, self.launched);
        if let Some(object) = summary.as_object_mut() {
            object.insert("breakpoints".into(), self.book.snapshot());
        }
        summary
    }
    fn failure_evidence(&self) -> String {
        let state = self.wire.state.lock().unwrap_or_else(|e| e.into_inner());
        let events: Vec<&Value> = state
            .events
            .iter()
            .rev()
            .take(8)
            .map(|(event, _)| event)
            .collect();
        if events.is_empty() {
            return String::new();
        }
        format!("\n[Last adapter events, newest first: {}]", bounded(&json!({
            "events":events,"droppedEvents":state.dropped_events,"droppedBytes":state.dropped_bytes,
        }).to_string()))
    }
    fn finish_configuration(&mut self, op: &Operation<'_>) -> DapResult<Value> {
        if !self.launched {
            return Err(DapError::user("launch/attach before configurationDone"));
        }
        if !self.configured {
            // Adapters without configurationDone may complete launch without initialized.
            if self.supports("supportsConfigurationDoneRequest") {
                self.wait(op, |state| {
                    state.initialized_seen
                        || self.start_request.is_some_and(|seq| {
                            state
                                .pending
                                .get(&seq)
                                .is_some_and(|pending| pending.response.is_some())
                        })
                })?;
            }
            if let Some(seq) = self.start_request {
                let failed = self
                    .wire
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .pending
                    .get(&seq)
                    .and_then(|pending| pending.response.as_ref())
                    .is_some_and(|response| response.get("success") == Some(&Value::Bool(false)));
                if failed {
                    self.start_request = None;
                    self.launched = false;
                    return self.response(seq, op);
                }
            }
            if self.supports("supportsConfigurationDoneRequest") {
                self.wait(op, |state| state.initialized_seen)?;
                self.request("configurationDone", json!({}), op)?;
            }
            self.configured = true;
            let mut state = self.wire.state.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(state.status.as_str(), "launching" | "configuring") {
                state.status = "running".into();
            }
        }
        let start = match self.start_request.take() {
            Some(seq) => match self.response(seq, op) {
                Ok(body) => body,
                Err(error) => {
                    self.launched = false;
                    self.configured = false;
                    return Err(error);
                }
            },
            None => json!({}),
        };
        Ok(json!({"response":start,"session":self.summary()}))
    }
    fn perform(
        &mut self,
        action: &str,
        mut arguments: Value,
        op: &Operation<'_>,
    ) -> DapResult<Value> {
        op.check()?;
        if action == "status" {
            return Ok(self.summary());
        }
        if action == "output" {
            let mut state = self.wire.state.lock().unwrap_or_else(|e| e.into_inner());
            let summary = state.summary(self.configured, self.start_request, self.launched);
            let events: Vec<Value> = state.events.drain(..).map(|(event, _)| event).collect();
            state.event_bytes = 0;
            return Ok(json!({"session":summary,"events":events}));
        }
        if action == "initialize" {
            return Ok(self.initialized.clone());
        }
        if action == "terminate" && !self.supports("supportsTerminateRequest") {
            return Err(DapError::user(
                "adapter does not support terminate; use disconnect",
            ));
        }
        if action == "disconnect"
            && super::present(&arguments, "terminateDebuggee")
            && !self.supports("supportTerminateDebuggee")
        {
            return Err(DapError::user(
                "adapter does not advertise supportTerminateDebuggee",
            ));
        }
        if super::present(&arguments, "granularity")
            && !self.supports("supportsSteppingGranularity")
        {
            return Err(DapError::user(
                "adapter does not support stepping granularity",
            ));
        }
        if action == "evaluate"
            && arguments.get("context").and_then(Value::as_str) == Some("hover")
            && !self.supports("supportsEvaluateForHovers")
        {
            return Err(DapError::user("adapter does not support hover evaluation"));
        }
        if action == "setBreakpoints" {
            for (field, capability) in [
                ("condition", "supportsConditionalBreakpoints"),
                ("hitCondition", "supportsHitConditionalBreakpoints"),
                ("logMessage", "supportsLogPoints"),
            ] {
                if arguments["breakpoints"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|bp| super::present(bp, field))
                    && !self.supports(capability)
                {
                    return Err(DapError::user(format!(
                        "adapter does not support breakpoint {field}"
                    )));
                }
            }
        }
        if matches!(action, "launch" | "attach") {
            if self.launched {
                return Err(DapError::user(
                    "already launched/attached; disconnect before starting another debuggee",
                ));
            }
            let seq = self.send(action, arguments, op)?;
            self.start_request = Some(seq);
            self.launched = true;
            {
                let mut state = self.wire.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.stopped.is_none() {
                    state.status = if state.initialized_seen {
                        "configuring"
                    } else {
                        "launching"
                    }
                    .into();
                }
            }
            self.wait(op, |state| {
                state.initialized_seen
                    || state
                        .pending
                        .get(&seq)
                        .is_some_and(|pending| pending.response.is_some())
            })?;
            let ready = self
                .wire
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pending
                .get(&seq)
                .is_some_and(|pending| pending.response.is_some());
            let body = if ready {
                self.start_request = None;
                match self.response(seq, op) {
                    Ok(body) => body,
                    Err(error) => {
                        self.launched = false;
                        return Err(error);
                    }
                }
            } else {
                json!({})
            };
            return Ok(
                json!({"response":body,"session":self.summary(),"next":"setBreakpoints, then configurationDone"}),
            );
        }
        if action == "configurationDone" {
            return self.finish_configuration(op);
        }
        if is_breakpoint_action(action) && self.launched && !self.configured {
            self.wait(op, |state| {
                state.initialized_seen
                    || self.start_request.is_some_and(|seq| {
                        state.pending.get(&seq).is_some_and(|pending| {
                            pending.response.as_ref().is_some_and(|response| {
                                response.get("success") == Some(&Value::Bool(false))
                            })
                        })
                    })
            })?;
            let initialized = self
                .wire
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .initialized_seen;
            if !initialized {
                return self.finish_configuration(op);
            }
        } else if !is_breakpoint_action(action)
            && !matches!(action, "disconnect" | "terminate")
            && self.launched
        {
            self.finish_configuration(op)?;
        }
        if matches!(
            action,
            "pause" | "continue" | "next" | "stepIn" | "stepOut" | "stackTrace"
        ) && arguments.get("threadId").is_none()
        {
            let stopped = self
                .wire
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .stopped
                .as_ref()
                .and_then(|body| body.get("threadId"))
                .and_then(Value::as_i64);
            let thread = match stopped {
                Some(id) => id,
                None => self
                    .request("threads", json!({}), op)?
                    .get("threads")
                    .and_then(Value::as_array)
                    .and_then(|threads| threads.first())
                    .and_then(|thread| thread.get("id"))
                    .and_then(Value::as_i64)
                    .ok_or_else(|| {
                        DapError::user("adapter reported no threads; supply threadId")
                    })?,
            };
            arguments
                .as_object_mut()
                .unwrap()
                .insert("threadId".into(), json!(thread));
        }
        if is_breakpoint_action(action) {
            return self.breakpoint_action(action, arguments, op);
        }
        let body = self.request(action, arguments, op)?;
        if action == "threads"
            && let Some(threads) = body.get("threads").and_then(Value::as_array)
        {
            let mut state = self.wire.state.lock().unwrap_or_else(|e| e.into_inner());
            state.threads.clear();
            for thread in threads.iter().take(MAX_EVENTS) {
                if let Some(id) = thread.get("id").and_then(Value::as_i64) {
                    state.threads.insert(id, thread.clone());
                }
            }
        }
        Ok(body)
    }
}

fn read_messages(output: impl Read, wire: Arc<Wire>) {
    let mut reader = BufReader::new(output);
    loop {
        let value = match read_frame(&mut reader) {
            Ok(Some(value)) => value,
            Ok(None) => {
                wire.fail("DAP adapter closed its output");
                break;
            }
            Err(error) => {
                wire.fail(error);
                break;
            }
        };
        let result = route_message(&wire, value);
        if let Err(error) = result {
            wire.fail(error.message);
            break;
        }
    }
}

fn route_message(wire: &Wire, value: Value) -> DapResult<()> {
    match value.get("type").and_then(Value::as_str) {
        Some("response") => {
            let seq = value
                .get("request_seq")
                .and_then(Value::as_i64)
                .ok_or_else(|| DapError::transport("DAP response lacks request_seq"))?;
            if !value.get("success").is_some_and(Value::is_boolean) {
                return Err(DapError::transport("DAP response lacks success"));
            }
            let mut state = wire.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(pending) = state.pending.get_mut(&seq) {
                if value.get("command").and_then(Value::as_str) != Some(pending.command.as_str()) {
                    return Err(DapError::transport(
                        "DAP response command does not match request",
                    ));
                }
                if pending.response.is_some() {
                    return Err(DapError::transport("duplicate DAP response"));
                }
                pending.response = Some(value);
            } else {
                return Err(DapError::transport(
                    "DAP response references unknown request",
                ));
            }
            wire.changed.notify_all();
        }
        Some("event") => {
            let event = value
                .get("event")
                .and_then(Value::as_str)
                .ok_or_else(|| DapError::transport("DAP event lacks name"))?;
            let empty = json!({});
            let body = value.get("body").unwrap_or(&empty);
            let mut state = wire.state.lock().unwrap_or_else(|e| e.into_inner());
            match event {
                "initialized" => {
                    state.initialized_seen = true;
                    if matches!(state.status.as_str(), "launching" | "initializing") {
                        state.status = "configuring".into();
                    }
                }
                "stopped" => {
                    state.status = "stopped".into();
                    state.stopped = Some(body.clone());
                }
                "continued" => {
                    state.status = "running".into();
                    state.stopped = None;
                }
                "terminated" | "exited" => {
                    state.status = "terminated".into();
                    state.stopped = None;
                }
                "thread" => {
                    if let Some(id) = body.get("threadId").and_then(Value::as_i64) {
                        if body.get("reason").and_then(Value::as_str) == Some("exited") {
                            state.threads.remove(&id);
                        } else if state.threads.len() < MAX_EVENTS {
                            state
                                .threads
                                .insert(id, json!({"id":id,"reason":body.get("reason")}));
                        }
                    }
                }
                "capabilities" => {
                    if let Some(updates) = body.get("capabilities").and_then(Value::as_object)
                        && let Some(capabilities) = state.capabilities.as_object_mut()
                    {
                        // Capability switches are booleans. Bound arbitrary adapter extensions.
                        for (key, value) in updates {
                            if value.is_boolean()
                                && (capabilities.contains_key(key)
                                    || capabilities.len() < MAX_EVENTS)
                            {
                                capabilities.insert(key.clone(), value.clone());
                            }
                        }
                    }
                }
                _ => {}
            }
            state.event(value);
            wire.changed.notify_all();
        }
        Some("request") => {
            let request_seq = value
                .get("seq")
                .and_then(Value::as_i64)
                .ok_or_else(|| DapError::transport("DAP reverse request lacks seq"))?;
            let command = value
                .get("command")
                .and_then(Value::as_str)
                .ok_or_else(|| DapError::transport("DAP reverse request lacks command"))?;
            let seq = wire.seq()?;
            // The reader thread must not wait without bound: zero (unlimited) falls back to a fixed reply bound.
            let limit = if wire.timeout.is_zero() {
                REVERSE_REPLY_TIMEOUT
            } else {
                wire.timeout
            };
            let end = Some(Instant::now() + limit);
            wire.write(&json!({"seq":seq,"type":"response","request_seq":request_seq,"command":command,"success":false,
                "message":"Dial refuses reverse requests; configure an internal console and no child-debug-session spawning"}).to_string(), end, || {
                if wire.state.lock().unwrap_or_else(|e| e.into_inner()).failure.is_some() { Err(DapError::transport("DAP session closed")) } else { Ok(()) }
            })?;
            wire.state.lock().unwrap_or_else(|e| e.into_inner()).event(json!({"type":"event","event":"reverseRequestRefused","body":{"command":command,"request_seq":request_seq}}));
        }
        _ => return Err(DapError::transport("invalid DAP message type")),
    }
    Ok(())
}

fn read_frame(reader: &mut impl BufRead) -> Result<Option<Value>, String> {
    let mut length = None;
    let mut total = 0usize;
    let mut line = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        let read = reader
            .read(&mut byte)
            .map_err(|error| format!("reading DAP header: {error}"))?;
        if read == 0 {
            if total == 0 {
                return Ok(None);
            }
            return Err("truncated DAP header".into());
        }
        total += 1;
        if total > MAX_HEADER {
            return Err("DAP header exceeds limit".into());
        }
        if byte[0] == b'\n' {
            if line == b"\r" {
                break;
            }
            let text = std::str::from_utf8(&line)
                .map_err(|_| "DAP header is not UTF-8")?
                .trim();
            if let Some((key, value)) = text.split_once(':')
                && key.eq_ignore_ascii_case("content-length")
            {
                if length.is_some() {
                    return Err("duplicate DAP Content-Length".into());
                }
                length = Some(
                    value
                        .trim()
                        .parse::<usize>()
                        .map_err(|_| "invalid DAP Content-Length")?,
                );
            }
            line.clear();
        } else {
            line.push(byte[0]);
        }
    }
    let length = length.ok_or("DAP frame has no Content-Length")?;
    if length > MAX_FRAME {
        return Err("DAP frame exceeds limit".into());
    }
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .map_err(|error| format!("reading DAP body: {error}"))?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| format!("invalid DAP JSON: {error}"))
}

fn bounded(text: &str) -> String {
    if text.len() <= MAX_OUTPUT {
        return text.to_owned();
    }
    let mut end = MAX_OUTPUT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [DAP output truncated]", &text[..end])
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod transport_tests;

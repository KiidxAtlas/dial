//! Desktop computer use: the `computer` tool and its eval prelude.

pub mod call;
pub mod desktop;
pub mod protocol;
pub mod rpc;
pub mod supervisor;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::types::Config;
use crate::tools::ToolOutput;
use crate::tools::ext::{Concurrency, Tool, ToolContext, ToolEvent};
use crate::tools::preludes::{PreludeCall, PreludeDef, PreludeHandler, register_prelude};

use crate::llm::model_config::ApiProtocol;
use call::{
    ComputerCallStep, is_read_only_computer_call, render_computer_call, render_function_run,
};
use protocol::{ComputerContent, ComputerRunOk, ComputerSessionSnapshot};
use rpc::{ComputerError, ComputerResult};
use supervisor::{AbortSignal, ComputerHost, ComputerSupervisor, RunRequest};

pub const NAME: &str = "computer";
pub const MAX_WIDTH: u32 = 3840;
pub const MAX_HEIGHT: u32 = 2400;
const COORDINATE_SAFE_MAX_WIDTH: u32 = 1280;
const COORDINATE_SAFE_MAX_HEIGHT: u32 = 896;
const DEFAULT_TIMEOUT_SECONDS: f64 = 120.0;
const MIN_TIMEOUT_SECONDS: f64 = 1.0;
const MAX_TIMEOUT_SECONDS: f64 = 300.0;
const INLINE_BYTE_CAP: usize = 50 * 1024;
const FN_SCOPE: &[&str] = &["desktop", "wait", "assert"];
const CANCEL_POLL: Duration = Duration::from_millis(50);

pub const TOOL_PROMPT: &str = include_str!("prompts/computer.md");
pub const USE_PROMPT: &str = include_str!("prompts/computer-use.md");
const PRELUDE_JS: &str = include_str!("js/prelude.js");
const PRELUDE_PY: &str = include_str!("js/prelude.py");

/// The `computer` tool and its eval prelude when `integrations` enables it; empty otherwise.
pub fn tools(config: &Config) -> Vec<Arc<dyn Tool>> {
    if !config.integrations.iter().any(|name| name == NAME) {
        return Vec::new();
    }
    let session = Arc::new(ComputerSession::new(current_cwd()));
    register_prelude(
        PreludeDef {
            name: NAME,
            js: PRELUDE_JS,
            py: PRELUDE_PY,
        },
        prelude_handler(),
    );
    let tool: Arc<dyn Tool> = Arc::new(Computer { session });
    vec![tool]
}

/// Probe the native desktop backend. Used by `/computer` status and enable checks.
pub fn capabilities() -> Result<Value, String> {
    let cwd = current_cwd();
    let session = ComputerSession::new(cwd.clone());
    let result = session.execute(
        ComputerAction::Capabilities,
        &cwd,
        false,
        None,
        &AbortSignal::new(),
    );
    let _ = session.supervisor.close();
    result.map_err(|error| error.message)
}

fn current_cwd() -> String {
    std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_default()
}

fn new_session_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!("computer-{}-{nanos:x}", std::process::id())
}

/// The action surface shared by the direct tool and the eval prelude.
#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase", deny_unknown_fields)]
enum ComputerAction {
    Run {
        code: Option<String>,
        #[serde(rename = "fn")]
        function: Option<String>,
        args: Option<Vec<Value>>,
        read_only: Option<bool>,
        timeout: Option<f64>,
    },
    Call {
        chain: Vec<ComputerCallStep>,
        timeout: Option<f64>,
    },
    Capabilities,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Policy {
    Read,
    Exec,
}

fn action_policy(action: &ComputerAction) -> Policy {
    match action {
        ComputerAction::Capabilities => Policy::Read,
        ComputerAction::Call { chain, .. } => match is_read_only_computer_call(chain) {
            Ok(true) => Policy::Read,
            _ => Policy::Exec,
        },
        ComputerAction::Run { .. } | ComputerAction::Close => Policy::Exec,
    }
}

fn parse_action(args: &Value) -> Result<ComputerAction, String> {
    serde_json::from_value(args.clone())
        .map_err(|error| format!("computer received invalid arguments: {error}"))
}

fn computer_policy(args: &Value) -> Policy {
    match parse_action(args) {
        Ok(action) => action_policy(&action),
        Err(_) => Policy::Exec,
    }
}

fn clamp_timeout(raw: Option<f64>) -> Duration {
    Duration::from_secs_f64(
        raw.unwrap_or(DEFAULT_TIMEOUT_SECONDS)
            .clamp(MIN_TIMEOUT_SECONDS, MAX_TIMEOUT_SECONDS),
    )
}

fn resolve_run_source(
    code: Option<String>,
    function: Option<String>,
    args: Option<Vec<Value>>,
) -> Result<String, String> {
    let code = code
        .map(|code| code.trim().to_owned())
        .filter(|code| !code.is_empty());
    let function = function
        .map(|function| function.trim().to_owned())
        .filter(|function| !function.is_empty());
    match (code, function) {
        (Some(code), None) => Ok(code),
        (None, Some(function)) => Ok(render_function_run(
            &function,
            FN_SCOPE,
            &args.unwrap_or_default(),
        )),
        _ => Err("Action 'run' requires exactly one of 'code' or 'fn'.".into()),
    }
}

/// Confirmation request forwarded from a worker control thread to the tool thread.
struct ConfirmRequest {
    summary: String,
    reply: flume::Sender<bool>,
}

/// Host for worker callbacks. Control grants are forwarded to the tool thread, which asks the
/// tool context's confirm callback; with no callback, grants are denied.
struct ToolHost {
    requests: flume::Sender<ConfirmRequest>,
}

impl ComputerHost for ToolHost {
    fn confirm_control(
        &self,
        title: &str,
        body: &str,
        _signal: &AbortSignal,
    ) -> ComputerResult<bool> {
        let (reply, answer) = flume::bounded(1);
        let request = ConfirmRequest {
            summary: format!("{title}\n{body}"),
            reply,
        };
        if self.requests.send(request).is_err() {
            return Ok(false);
        }
        Ok(answer.recv().unwrap_or(false))
    }

    fn call_tool(&self, name: &str, _args: Value, _signal: &AbortSignal) -> ComputerResult<Value> {
        Err(ComputerError::tool(format!(
            "Unknown computer host tool \"{name}\"."
        )))
    }
}

struct ComputerSession {
    supervisor: ComputerSupervisor,
    session_id: String,
    cwd: Mutex<String>,
    closed: AtomicBool,
}

impl ComputerSession {
    fn new(cwd: String) -> Self {
        Self {
            supervisor: ComputerSupervisor::new(),
            session_id: new_session_id(),
            cwd: Mutex::new(cwd),
            closed: AtomicBool::new(false),
        }
    }

    fn snapshot(&self, read_only: bool, coordinate_safe: bool) -> ComputerSessionSnapshot {
        let (capture_max_width, capture_max_height) = capture_frame(coordinate_safe);
        ComputerSessionSnapshot {
            cwd: self.cwd.lock().clone(),
            session_id: self.session_id.clone(),
            capture_max_width,
            capture_max_height,
            display: "active".into(),
            read_only,
        }
    }

    fn execute(
        &self,
        action: ComputerAction,
        cwd: &str,
        coordinate_safe: bool,
        confirm: Option<&dyn Fn(&str) -> bool>,
        signal: &AbortSignal,
    ) -> ComputerResult<Value> {
        // Per run, the session's cwd follows the caller's workspace root.
        *self.cwd.lock() = cwd.to_owned();
        let closed = self.closed.load(Ordering::Acquire);
        match action {
            ComputerAction::Close => {
                self.closed.store(true, Ordering::Release);
                self.supervisor.close()?;
                Ok(json!({ "closed": true }))
            }
            ComputerAction::Capabilities => {
                if closed {
                    return Err(ComputerError::tool("Computer capabilities unavailable"));
                }
                let read_only = action_policy(&ComputerAction::Capabilities) == Policy::Read;
                let snapshot = snapshot_value(&self.snapshot(read_only, coordinate_safe))?;
                self.supervisor
                    .capabilities(&snapshot, std::path::Path::new(cwd), signal)
            }
            ComputerAction::Run {
                code,
                function,
                args,
                read_only,
                timeout,
            } => {
                if closed {
                    return Err(ComputerError::tool("Computer session is closed"));
                }
                let source =
                    resolve_run_source(code, function, args).map_err(ComputerError::tool)?;
                let read_only = read_only == Some(true);
                self.run_source(
                    source,
                    cwd,
                    coordinate_safe,
                    confirm,
                    read_only,
                    timeout,
                    signal,
                )
            }
            ComputerAction::Call { chain, timeout } => {
                if closed {
                    return Err(ComputerError::tool("Computer session is closed"));
                }
                let source = render_computer_call(&chain).map_err(ComputerError::tool)?;
                let read_only = is_read_only_computer_call(&chain).map_err(ComputerError::tool)?;
                self.run_source(
                    source,
                    cwd,
                    coordinate_safe,
                    confirm,
                    read_only,
                    timeout,
                    signal,
                )
            }
        }
    }

    /// Runs the supervisor on a scoped thread so this thread can answer control confirmations
    /// with the tool context's confirm callback while the run is in flight.
    fn run_source(
        &self,
        code: String,
        cwd: &str,
        coordinate_safe: bool,
        confirm: Option<&dyn Fn(&str) -> bool>,
        read_only: bool,
        timeout: Option<f64>,
        signal: &AbortSignal,
    ) -> ComputerResult<Value> {
        let timeout = clamp_timeout(timeout);
        let request = RunRequest {
            code,
            timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            session: snapshot_value(&self.snapshot(read_only, coordinate_safe))?,
            cwd: std::path::PathBuf::from(cwd),
        };
        let (requests, confirm_requests) = flume::unbounded::<ConfirmRequest>();
        let host: Arc<dyn ComputerHost> = Arc::new(ToolHost { requests });
        std::thread::scope(|scope| {
            let run = scope.spawn(move || self.supervisor.run(&request, host, signal));
            loop {
                match confirm_requests.recv_timeout(Duration::from_millis(50)) {
                    Ok(ConfirmRequest { summary, reply }) => {
                        let approved = confirm.is_some_and(|confirm| confirm(&summary));
                        let _ = reply.send(approved);
                    }
                    Err(flume::RecvTimeoutError::Timeout) => {
                        if run.is_finished() {
                            break;
                        }
                    }
                    Err(flume::RecvTimeoutError::Disconnected) => break,
                }
            }
            run.join()
                .unwrap_or_else(|_| Err(ComputerError::internal("computer run thread panicked")))
        })
    }
}

impl Drop for ComputerSession {
    fn drop(&mut self) {
        // Native callback threads may still retain the supervisor's inner Arc. Close explicitly
        // at the execution-owner boundary rather than waiting for the last callback to release it.
        let _ = self.supervisor.close();
    }
}

/// Coordinate-safe image sizing applies to Anthropic-class models, which get frames capped at 1280x896.
fn coordinate_safe_frames(config: &Config) -> bool {
    let selection = config.selection();
    let protocol = selection.protocol.or_else(|| {
        config
            .providers
            .get(&selection.provider)
            .and_then(|spec| spec.protocol)
    });
    protocol == Some(ApiProtocol::Anthropic)
}

fn capture_frame(coordinate_safe: bool) -> (u32, u32) {
    if coordinate_safe {
        (
            MAX_WIDTH.min(COORDINATE_SAFE_MAX_WIDTH),
            MAX_HEIGHT.min(COORDINATE_SAFE_MAX_HEIGHT),
        )
    } else {
        (MAX_WIDTH, MAX_HEIGHT)
    }
}

fn snapshot_value(snapshot: &ComputerSessionSnapshot) -> ComputerResult<Value> {
    serde_json::to_value(snapshot).map_err(|error| ComputerError::internal(error.to_string()))
}

/// Run `f` with an abort signal that trips when `cancel` is set. The watcher thread ends with `f`,
/// including when `f` unwinds: a panic must not leave the scope join waiting on a watcher that
/// never observes `cancel`.
fn with_cancel<T>(cancel: &AtomicBool, f: impl FnOnce(&AbortSignal) -> T) -> T {
    struct Done<'a>(&'a AtomicBool);
    impl Drop for Done<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let signal = AbortSignal::new();
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !done.load(Ordering::Acquire) {
                if cancel.load(Ordering::Acquire) {
                    signal.abort();
                    break;
                }
                std::thread::sleep(CANCEL_POLL);
            }
        });
        let _done = Done(&done);
        f(&signal)
    })
}

/// Cap inline output: keep the head and tail, elide the middle, and point at the full
/// text when an artifact store can retain it.
fn cap_inline(text: String, save: impl FnOnce(&str) -> Option<String>) -> String {
    if text.len() <= INLINE_BYTE_CAP {
        return text;
    }
    let head_budget = INLINE_BYTE_CAP * 60 / 100;
    let tail_budget = INLINE_BYTE_CAP / 4;
    let mut head_end = head_budget;
    while !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let head_full = &text[..head_end];
    let head = match head_full.rfind('\n') {
        Some(index) => &head_full[..index],
        None => head_full,
    };
    let mut tail_start = text.len() - tail_budget;
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let tail_full = &text[tail_start..];
    let tail = match tail_full.find('\n') {
        Some(index) => &tail_full[index + 1..],
        None => tail_full,
    };
    let elided = text.len() - head.len() - tail.len();
    let mut out = format!("{head}\n[…{elided}B elided…]\n{tail}");
    if let Some(uri) = save(&text) {
        out.push_str(&format!("\n[raw output: {uri}]"));
    }
    out
}

fn run_text(displays: &[ComputerContent]) -> String {
    displays
        .iter()
        .filter_map(|content| match content {
            ComputerContent::Text { text } => Some(text.as_str()),
            ComputerContent::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct Computer {
    session: Arc<ComputerSession>,
}

impl Computer {
    fn output(&self, ctx: &ToolContext, args: &Value, cancel: &AtomicBool) -> ToolOutput {
        let action = match parse_action(args) {
            Ok(action) => action,
            Err(error) => return ToolOutput::err(error),
        };
        let read_only = action_policy(&action) == Policy::Read;
        let is_run = matches!(
            action,
            ComputerAction::Run { .. } | ComputerAction::Call { .. }
        );
        let is_capabilities = matches!(action, ComputerAction::Capabilities);
        let cwd = ctx.workspace.root().display().to_string();
        let coordinate_safe = coordinate_safe_frames(ctx.config);
        let result = with_cancel(cancel, |signal| {
            self.session
                .execute(action, &cwd, coordinate_safe, ctx.confirm, signal)
        });
        let value = match result {
            Ok(value) => value,
            Err(error) if error.is_abort => {
                return ToolOutput::err(format!(
                    "Computer action was aborted before it finished ({}). Input already sent may have taken effect; observe the current state before acting again.",
                    error.message
                ));
            }
            Err(error) => return ToolOutput::err(error.message),
        };
        if is_capabilities {
            return match serde_json::to_string_pretty(&value) {
                Ok(text) => ToolOutput {
                    execution: Default::default(),
                    text,
                    is_error: false,
                    data: Some(value),
                },
                Err(error) => ToolOutput::err(error.to_string()),
            };
        }
        if !is_run {
            return ToolOutput {
                execution: Default::default(),
                text: "Closed computer session".into(),
                is_error: false,
                data: Some(value),
            };
        }
        let run: ComputerRunOk = match serde_json::from_value(value) {
            Ok(run) => run,
            Err(error) => {
                return ToolOutput::err(format!("computer returned a malformed result: {error}"));
            }
        };
        let text = cap_inline(run_text(&run.displays), |full| {
            ctx.workspace
                .artifacts()
                .and_then(|store| store.retain(full).ok())
        });
        let image = run.displays.iter().rev().find_map(|content| match content {
            ComputerContent::Image { data, mime_type } => Some((data, mime_type)),
            ComputerContent::Text { .. } => None,
        });
        let mut data = json!({
            "text": text,
            "readOnly": read_only,
            "screenshots": run.screenshots,
            "value": run.return_value,
            "capabilities": run.capabilities,
        });
        if let Some((image, mime_type)) = image {
            data["artifact"] =
                json!({"data": image, "encoding": "base64", "media_type": mime_type});
        }
        ToolOutput {
            execution: Default::default(),
            text,
            is_error: false,
            data: Some(data),
        }
    }
}

impl Tool for Computer {
    fn fork_session(&self) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self {
            session: Arc::new(ComputerSession::new(self.session.cwd.lock().clone())),
        }))
    }

    fn name(&self) -> &str {
        NAME
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": NAME,
                "description": TOOL_PROMPT,
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["run", "call", "capabilities", "close"]
                        },
                        "code": {"type": "string"},
                        "fn": {"type": "string"},
                        "args": {"type": "array", "items": {}},
                        "read_only": {"type": "boolean"},
                        "chain": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": {
                                    "method": {"type": "string"},
                                    "args": {"type": "array", "items": {}}
                                },
                                "required": ["method"]
                            }
                        },
                        "timeout": {"type": "number"}
                    },
                    "required": ["action"]
                }
            }
        })
    }

    fn needs_approval(&self, args: &Value) -> bool {
        computer_policy(args) == Policy::Exec
    }

    fn concurrency(&self, args: &Value) -> Concurrency {
        match computer_policy(args) {
            Policy::Read => Concurrency::Shared,
            Policy::Exec => Concurrency::Exclusive,
        }
    }

    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        self.output(ctx, args, &AtomicBool::new(false))
    }

    fn call_streaming(
        &self,
        ctx: &ToolContext,
        args: &Value,
        _on_event: &(dyn Fn(ToolEvent) + Sync),
        cancel: &AtomicBool,
    ) -> ToolOutput {
        self.output(ctx, args, cancel)
    }
}

fn prelude_handler() -> PreludeHandler {
    Arc::new(|params: Value, call: &mut PreludeCall<'_>| {
        parse_action(&params).map_err(anyhow::Error::msg)?;
        call.tool(NAME, params).map_err(anyhow::Error::msg)
    })
}

#[cfg(test)]
mod computer_tests;

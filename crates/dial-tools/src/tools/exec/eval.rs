//! Owner-local persistent JavaScript/Python execution. The OS sandbox is the security boundary.
use crate::config::tools::eval::EvalConfig;

use super::background::AdoptedJob;
use super::kernel::{
    ActiveGuard, BackgroundPolicy, CellOutcome, CellScope, Deadline, Handoff, MAX_CODE,
    NativeProcess, PythonRuntime, busy_job, cell_label, check_operation, code_output, deadline,
    execution_deadline, guarded_operation, lock_session, native_cell, require_fork_containment,
    resolve_operator_executable, strip_runtime_injection,
};
#[cfg(not(windows))]
use super::kernel::{fenced_command, fork_fence_prefix, shell_quote};
use crate::config::tools::kernel::KernelConfig;
use crate::config::types::Config;
use crate::tools::integrations::ext::{Tool, ToolCallback, ToolContext, ToolEvent};
use crate::tools::{ToolOutput, Workspace, tool_schema};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering},
};

const DRIVER: &str = include_str!("eval/driver.cjs");

/// Reserved callback name for `tool.batch([...])`; never a valid provider tool name. The
/// execution controller runs the entries together when all are read-only, else one at a time.
/// Controllers without batch support refuse it, and the driver then calls each entry serially.
pub const BATCH_CALLBACK: &str = "dial.batch";

pub(super) mod cells;
#[cfg(feature = "embedded-js")]
mod embedded;
#[cfg(feature = "embedded-js")]
pub use embedded::{WORKER_ARG as EMBEDDED_JS_WORKER_ARG, worker_main as embedded_js_worker_main};

/// Whether JavaScript cells can run without an operator-installed Node: the embedded QuickJS engine
/// is compiled in and re-executes `dial` itself.
pub const fn embedded_js_available() -> bool {
    cfg!(feature = "embedded-js")
}

/// The JavaScript kernel command: the operator's Node with the trusted driver, or (`None`) the
/// embedded engine, under the process-fork fence. The shell preserves only the trusted protocol
/// on fd 3, so native stdout/stderr cannot be interpreted as controller callbacks or result frames.
#[cfg(not(windows))]
fn js_kernel_command(workspace: &Workspace, node: Option<&Path>) -> Result<Command, String> {
    let fence = fork_fence_prefix(workspace);
    let script = match node {
        Some(node) => format!(
            "{fence} {} --no-addons -e {} 3>&1 1>&2",
            shell_quote(node.to_str().ok_or("Node executable path is not UTF-8")?),
            shell_quote(DRIVER),
        ),
        None => embedded_js_script(&fence)?,
    };
    fenced_command(workspace, &script)
}

/// Windows has no fd 3 for the protocol: it travels on stdout. The Node driver routes its own
/// stream writes to stderr; the embedded engine exposes no I/O to cells at all.
#[cfg(windows)]
fn js_kernel_command(workspace: &Workspace, node: Option<&Path>) -> Result<Command, String> {
    let (program, args) = match node {
        Some(node) => (
            node.to_str()
                .ok_or("Node executable path is not UTF-8")?
                .to_owned(),
            vec!["--no-addons".to_owned(), "-e".to_owned(), DRIVER.to_owned()],
        ),
        None => embedded_js_invocation()?,
    };
    super::kernel::windows_kernel_command(workspace, &program, &args)
}

#[cfg(all(windows, feature = "embedded-js"))]
fn embedded_js_invocation() -> Result<(String, Vec<String>), String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("locating dial for the embedded JavaScript engine: {error}"))?;
    Ok((
        exe.to_str()
            .ok_or("dial executable path is not UTF-8")?
            .to_owned(),
        vec![EMBEDDED_JS_WORKER_ARG.to_owned()],
    ))
}

#[cfg(all(windows, not(feature = "embedded-js")))]
fn embedded_js_invocation() -> Result<(String, Vec<String>), String> {
    Err("the embedded JavaScript engine is not built into this binary".into())
}

/// Shell line that serves JavaScript cells with the embedded engine: this `dial` binary re-executed
/// under the same fork fence, with the protocol pipe on fd 3.
#[cfg(all(feature = "embedded-js", not(windows)))]
fn embedded_js_script(fence: &str) -> Result<String, String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("locating dial for the embedded JavaScript engine: {error}"))?;
    let exe = exe.to_str().ok_or("dial executable path is not UTF-8")?;
    Ok(format!(
        "{fence} {} {EMBEDDED_JS_WORKER_ARG} 3>&1 1>&2",
        shell_quote(exe)
    ))
}

#[cfg(all(not(feature = "embedded-js"), not(windows)))]
fn embedded_js_script(_fence: &str) -> Result<String, String> {
    Err("the embedded JavaScript engine is not built into this binary".into())
}

/// Explicitly prepare mandatory containment for an automatic offer, without starting an interpreter.
/// Catalog/readiness queries only inspect the cached observation; they never call this hook.
pub fn prepare_containment() -> Result<(), String> {
    require_fork_containment("eval")
}

/// Languages backed by a resolved operator launcher or the compiled JavaScript engine. Only
/// paths/build facts are inspected; version and containment preflight runs at cell launch.
pub fn installed_languages(workspace: &std::path::Path, config: &Config) -> Vec<&'static str> {
    let mut languages = Vec::new();
    if resolve_operator_executable(workspace, &config.eval.node, "eval.node").is_ok()
        || embedded_js_available()
    {
        languages.push("js");
    }
    if super::kernel::python_launcher_resolves(workspace, &config.kernel) {
        languages.push("py");
    }
    languages
}

/// Interpreter prerequisites observed without starting a process. Containment and interpreter
/// versions are still checked by execution; a resolved launcher is not proof of readiness.
pub fn runtime_readiness(
    workspace: &std::path::Path,
    config: &Config,
) -> crate::tools::surface::catalog::Readiness {
    use crate::tools::surface::catalog::Readiness;
    if installed_languages(workspace, config).is_empty() {
        Readiness::MissingPrerequisite {
            reason: "No eval interpreter resolves: JavaScript needs eval.node (Node 22+) or the embedded engine, which this build does not include; Python needs kernel.python (Python 3.8+). Loading eval does not install either interpreter. Status/reset/cancel remain available.".into(),
        }
    } else {
        match super::kernel::cached_fork_containment() {
            Some(Err(reason)) => Readiness::MissingPrerequisite { reason },
            Some(Ok(())) | None => Readiness::Unknown,
        }
    }
}

/// Bounded environment facts for the session prompt, using the same resolution as eval itself.
pub fn runtime_guidance(workspace: &std::path::Path, config: &Config) -> String {
    let installed = installed_languages(workspace, config);
    let state = |language| {
        if installed.contains(&language) {
            "resolved"
        } else {
            "not found"
        }
    };
    format!(
        "- Eval launchers: JavaScript {} (configured Node 22+ when it resolves, otherwise the embedded engine), Python {} (path resolution only; versions and containment are checked on execution). Loading a tool installs no runtime. Before generating a helper, check its needed interpreter/compiler in the execution environment; a configured eval launcher need not be on the shell PATH.\n",
        state("js"),
        state("py")
    )
}

pub fn tools(config: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(EvalTool::new(
        config.eval.clone(),
        config.kernel.clone(),
    ))]
}

/// The single `eval` tool. Each cell runs in its language's persistent runtime; the namespaces are
/// independent and never share state.
struct EvalTool {
    js: JsRuntime,
    python: PythonRuntime,
}
impl EvalTool {
    fn new(js: EvalConfig, python: KernelConfig) -> Self {
        Self {
            js: JsRuntime::new(js),
            python: PythonRuntime::new(python),
        }
    }
}

/// Persistent Node.js namespace owned by one execution owner.
struct JsRuntime {
    config: EvalConfig,
    session: Arc<Mutex<Option<NativeProcess>>>,
    busy: Arc<Mutex<Option<String>>>,
    pid: Arc<AtomicI32>,
    interrupt: AtomicBool,
    active: AtomicUsize,
}
impl JsRuntime {
    fn new(config: EvalConfig) -> Self {
        Self {
            config,
            session: Arc::new(Mutex::new(None)),
            busy: Arc::new(Mutex::new(None)),
            pid: Arc::new(AtomicI32::new(0)),
            interrupt: AtomicBool::new(false),
            active: AtomicUsize::new(0),
        }
    }
    /// Cancels the in-flight cell and reports whether one was running. A flag left behind would
    /// reject the next cell, so nothing is recorded when idle.
    fn cancel(&self) -> bool {
        if self.active.load(Ordering::Acquire) == 0 {
            return false;
        }
        self.interrupt.store(true, Ordering::Release);
        let pid = self.pid.load(Ordering::Acquire);
        if pid > 0 {
            let _ = crate::platform::process::kill_group(pid as u32);
        }
        true
    }
    #[allow(clippy::too_many_arguments)]
    fn run(
        &self,
        ctx: &ToolContext,
        action: &str,
        args: &Value,
        cancel: &AtomicBool,
        adopt: &dyn Fn(AdoptedJob) -> Result<String, String>,
        dispatch: &mut dyn FnMut(ToolCallback) -> ToolOutput,
        effect_started: &AtomicBool,
    ) -> Result<CellOutcome, String> {
        // Counts the whole operation, including lock wait, so cancel reaches it before startup.
        let _active = ActiveGuard::new(&self.active);
        let cell = Arc::new(Deadline::new(execution_deadline(deadline(
            args,
            self.config.timeout_secs,
        )?)));
        let end = &cell;
        let pending_cancel = self.interrupt.swap(false, Ordering::AcqRel);
        if cancel.load(Ordering::Acquire) || (pending_cancel && action == "eval") {
            return Err("JavaScript execution cancelled before startup".into());
        }
        let policy = BackgroundPolicy::from_args(args, ctx.config.shell.auto_background_secs)?;
        let handoff = Handoff {
            label: cell_label(args, "js")?,
            policy: &policy,
            adopt,
            session: self.session.clone(),
            busy: self.busy.clone(),
            pid: self.pid.clone(),
        };
        let callback_wait = AtomicBool::new(false);
        let result = guarded_operation(
            end,
            &self.pid,
            &self.interrupt,
            cancel,
            &callback_wait,
            || {
                let mut slot = lock_session(&self.session, end, &self.interrupt, cancel)?;
                if let Some(job) = busy_job(&self.busy) {
                    if action == "status" {
                        return Ok(CellOutcome::Value(
                            json!({"running":true,"background":job,"language":"js",
                                "platform":crate::tools::surface::catalog::execution_platform("eval")}),
                        ));
                    }
                    return Err(format!("runtime busy in background job {job}"));
                }
                if action == "reset" {
                    slot.take();
                    return Ok(CellOutcome::Value(json!({"reset":true})));
                }
                if action == "status" {
                    return Ok(CellOutcome::Value(
                        json!({"running":slot.as_mut().is_some_and(NativeProcess::alive),"language":"js",
                            "platform":crate::tools::surface::catalog::execution_platform("eval")}),
                    ));
                }
                let code = args
                    .get("code")
                    .and_then(Value::as_str)
                    .ok_or("eval requires code")?;
                if code.len() > MAX_CODE {
                    return Err(format!("code exceeds {MAX_CODE} bytes"));
                }
                let scope = CellScope {
                    workspace: ctx.workspace,
                    prelude_mode: if ctx.config.eval.portable_helpers {
                        super::preludes::PreludeMode::Portable
                    } else {
                        super::preludes::PreludeMode::ExtensionsOnly
                    },
                    end,
                    interrupt: &self.interrupt,
                    cancel,
                    callback_wait: &callback_wait,
                };
                let result = (|| {
                    if slot.as_mut().is_some_and(|process| !process.alive()) {
                        slot.take();
                    }
                    if slot.is_none() {
                        require_fork_containment("eval")?;
                        let (node, kernel) = match resolve_operator_executable(
                            ctx.workspace.root(),
                            &self.config.node,
                            "eval.node",
                        ) {
                            Ok(node) => (Some(node), "Node kernel (install/configure eval.node)"),
                            Err(node_error) => {
                                if !embedded_js_available() {
                                    return Err(node_error);
                                }
                                (None, "embedded JavaScript engine")
                            }
                        };
                        check_operation(end, &self.interrupt, cancel)?;
                        let mut command = js_kernel_command(ctx.workspace, node.as_deref())?;
                        // Even the explicit filesystem-sandbox opt-out cannot inject code before the fence.
                        strip_runtime_injection(&mut command);
                        check_operation(end, &self.interrupt, cancel)?;
                        *slot = Some(
                            NativeProcess::start(command, self.pid.clone())
                                .map_err(|error| format!("starting {kernel}: {error}"))?,
                        );
                    }
                    check_operation(end, &self.interrupt, cancel)?;
                    slot.as_mut().unwrap().prelude_language =
                        Some(crate::tools::exec::preludes::PreludeLanguage::Js);
                    // Re-running an edited cell must not fail on its own earlier declarations.
                    let code = cells::redeclarable(code);
                    native_cell(
                        slot.as_mut().unwrap(),
                        &scope,
                        &code,
                        dispatch,
                        &handoff,
                        effect_started,
                    )
                })();
                if result.is_err() {
                    slot.take();
                }
                result
            },
        );
        // An interrupt raised during this operation must not reject the next cell.
        self.interrupt.store(false, Ordering::Release);
        result
    }
}

impl Tool for EvalTool {
    fn name(&self) -> &str {
        "eval"
    }
    fn schema(&self) -> Value {
        let mut schema = tool_schema(
            "eval",
            &crate::tools::surface::catalog::execution_description(
                "eval",
                "Persistent JavaScript (default) or Python 3.8+ cells with separate namespaces; top-level bindings persist (prefer var in JS). Embedded JS has no Node modules; OS containment is required and subprocesses/native addons are denied. Portable dial helpers, methods of `dial` rather than globals (JS await, Python synchronous): read/readText/readJson/readBytes, readLines(path, offset, limit)/lines(path) (page text files of any size by line), writeText/writeJson/writeBytes (creates only), run, batch (raises with error.results unless collect:true), check (local checks, not task-wide verification), snapshot/fresh/restore (individual source bytes, not an atomic live-database backup), hex/pack/unpack; whole-file exact I/O is capped at 1 MiB per call. Call tool.<name>(args) or tool.call(name,args) (JS await; Python returns dictionaries synchronously, also await-compatible without replay). tool.batch([{name,args},...]) returns ordered {is_error,text,data} and runs eligible read-only entries together: filter/aggregate many lookups in one cell. Normal approval applies; denied/malformed callbacks raise ToolError. For generated data use one foreground cell (background:false): read inputs programmatically, compute, serialize, write, then assert saved bytes, unchanged inputs, shape and independently expected results; prove a tiny known-answer case first and never transcribe source rows or printed values. Keep large values in the namespace and print short summaries; stdout alone is not check evidence. Failed cells can leave effects: inspect receipts before retrying.",
            ),
            json!({
                "language":{"type":"string","enum":["js","py"],"description":"Runtime for this cell; defaults to js. Windows Python requires explicit unsandboxed opt-in."},
                "action":{"type":"string","enum":["eval","reset","cancel","status"],"description":"reset/status/cancel are owner-scoped; cancel without language affects both runtimes."},
                "code":{"type":"string","description":"Cell source, at most 64 KiB UTF-8."},
                "timeout":{"type":"integer","minimum":0,"maximum":3600,"description":"Seconds; timeout or cancel discards the namespace. Callback time is excluded."},
                "background":{"type":"boolean","description":"Omit to follow shell.auto_background_secs (default 0: the cell stays in the foreground); false keeps it in the foreground; true moves it immediately."},
                "title":{"type":"string","description":"Label for a backgrounded cell in the background job list."}
            }),
            &[],
        );
        if !self.js.config.portable_helpers
            && let Some(description) = schema["function"]["description"].as_str()
            && let Some((start, remaining)) = description.split_once("Portable dial helpers")
            && let Some((_, rest)) = remaining.split_once("Call tool.<name>")
        {
            schema["function"]["description"] = json!(format!(
                "{start}Portable helpers are disabled. Call tool.<name>{rest}"
            ));
        }
        schema
    }
    fn readiness(&self, _: &Config) -> crate::tools::surface::catalog::Readiness {
        use crate::tools::surface::catalog::Readiness;
        match super::kernel::cached_fork_containment() {
            Some(Err(reason)) => Readiness::MissingPrerequisite { reason },
            // Containment alone does not verify an operator's interpreter installation.
            Some(Ok(())) | None => Readiness::Unknown,
        }
    }
    fn needs_approval(&self, _: &Value) -> bool {
        true
    }
    fn supports_tool_dispatch(&self) -> bool {
        true
    }
    fn fork_session(&self) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self {
            js: JsRuntime::new(self.js.config.clone()),
            python: self.python.fork(),
        }))
    }
    fn cancel_operation(&self) {
        self.js.cancel();
        self.python.cancel();
    }
    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        self.call_with_dispatch(
            ctx,
            args,
            &|_| {},
            &AtomicBool::new(false),
            &no_background,
            &mut |_| ToolOutput::err("tool callbacks require an execution controller"),
        )
    }
    fn call_streaming(
        &self,
        ctx: &ToolContext,
        args: &Value,
        on_event: &(dyn Fn(ToolEvent) + Sync),
        cancel: &AtomicBool,
    ) -> ToolOutput {
        self.call_with_dispatch(ctx, args, on_event, cancel, &no_background, &mut |_| {
            ToolOutput::err("tool callbacks require an execution controller")
        })
    }
    fn call_with_dispatch(
        &self,
        ctx: &ToolContext,
        args: &Value,
        _on_event: &(dyn Fn(ToolEvent) + Sync),
        cancel: &AtomicBool,
        adopt: &dyn Fn(AdoptedJob) -> Result<String, String>,
        dispatch: &mut dyn FnMut(ToolCallback) -> ToolOutput,
    ) -> ToolOutput {
        let started = AtomicBool::new(false);
        // Collect native facts before callback presentation/prelude conversion discards metadata.
        // Neither a cell's return value nor printed JSON can create a capture receipt.
        let mut captures = Vec::new();
        let mut images = Vec::new();
        let result = self.run(
            ctx,
            args,
            cancel,
            adopt,
            &mut |callback| {
                // Callback effect defaults do not certify read-only behavior. Keep only captures
                // from the final tool callback, rather than relabeling older pixels with the
                // compound eval's final revision. Schema inspection cannot mutate the target.
                if matches!(&callback, ToolCallback::Call { .. }) {
                    captures.clear();
                    images.clear();
                }
                let output = dispatch(callback);
                if !output.is_error && !output.execution.captures.is_empty() {
                    for image in output
                        .data
                        .as_ref()
                        .into_iter()
                        .flat_map(|data| data["images"].as_array().into_iter().flatten())
                    {
                        if let Some(reference) = image.get("reference") {
                            images.push(reference.clone());
                        }
                    }
                }
                captures.extend(output.execution.captures.iter().cloned());
                output
            },
            &started,
        );
        let uncertain = result.is_err() && started.load(Ordering::Acquire);
        let mut output = match result {
            Ok(CellOutcome::Value(value)) => code_output(Ok(value)),
            // Like a `bash` hand-off, a move is a launch receipt: `bg` owns the job from here and
            // reports its settlement, so the hand-off itself must not stop the turn as uncertain.
            Ok(CellOutcome::Moved(job)) => {
                let mut output = ToolOutput::ok(format!(
                    "eval cell moved to the background as job {job}; this runtime stays busy until it finishes. Use the bg tool to read its output, wait for it, or kill it."
                ));
                output.data = Some(json!({"background_job": job}));
                output
            }
            Err(error) => {
                let output = code_output(Err(error));
                if uncertain {
                    output.with_effects(crate::tools::surface::outcome::Effects::Unknown)
                } else {
                    output
                }
            }
        }.with_captures(captures);
        if let Some(data) = output.data.as_mut().and_then(Value::as_object_mut) {
            // Always overwrite: cell-produced fields are not host attachment provenance.
            data.remove("dial_images");
            if !images.is_empty() {
                data.insert("dial_images".into(), json!(images));
            }
        }
        if !output.is_error
            && args["action"] == "reset"
            && output
                .data
                .as_ref()
                .is_some_and(|data| data["reset"] == true)
        {
            output.execution.namespace_reset = match args["language"].as_str().unwrap_or("js") {
                "js" => Some(crate::runtime::execution::EvalNamespace::JavaScript),
                "py" => Some(crate::runtime::execution::EvalNamespace::Python),
                _ => None,
            };
        }
        output
    }
}

/// Adopter for callers without a background owner; the cell then stays in the foreground.
fn no_background(_job: AdoptedJob) -> Result<String, String> {
    Err("eval cells cannot move to the background without an execution controller".into())
}
impl EvalTool {
    fn run(
        &self,
        ctx: &ToolContext,
        args: &Value,
        cancel: &AtomicBool,
        adopt: &dyn Fn(AdoptedJob) -> Result<String, String>,
        dispatch: &mut dyn FnMut(ToolCallback) -> ToolOutput,
        effect_started: &AtomicBool,
    ) -> Result<CellOutcome, String> {
        let action = match args.get("action") {
            None | Some(Value::Null) => "eval",
            Some(value) => value.as_str().ok_or("action must be a string")?,
        };
        if !matches!(action, "eval" | "reset" | "status" | "cancel") {
            return Err(format!("unsupported eval action {action:?}"));
        }
        let language = match args.get("language") {
            None | Some(Value::Null) => None,
            Some(value) => Some(value.as_str().ok_or("language must be a string")?),
        };
        if action == "cancel" {
            // Without a language, cancel reaches both namespaces.
            let (js, python) = match language {
                None => (true, true),
                Some("js") => (true, false),
                Some("py") => (false, true),
                Some(other) => {
                    return Err(format!("unsupported eval language {other:?}; use js or py"));
                }
            };
            let mut cancelled = false;
            if js {
                cancelled |= self.js.cancel();
            }
            if python {
                cancelled |= self.python.cancel();
            }
            return Ok(CellOutcome::Value(
                json!({"cancelled":cancelled,"namespace_discarded":cancelled}),
            ));
        }
        match language.unwrap_or("js") {
            "js" => self
                .js
                .run(ctx, action, args, cancel, adopt, dispatch, effect_started),
            "py" => self
                .python
                .run(ctx, action, args, cancel, adopt, dispatch, effect_started)
                .map_err(|error| python_unavailable_hint(error, ctx)),
            other => Err(format!("unsupported eval language {other:?}; use js or py")),
        }
    }
}

/// A Python interpreter that cannot be found or started is not the end of eval: when JavaScript
/// resolves, the error says so. Other Python failures are returned unchanged.
fn python_unavailable_hint(error: String, ctx: &ToolContext) -> String {
    let launcher_failed = error.contains("kernel.python");
    if launcher_failed && installed_languages(ctx.workspace.root(), ctx.config).contains(&"js") {
        format!(
            "{error}\nJavaScript eval is available in this session: rerun the cell with language \"js\" (or omit language) instead of installing Python."
        )
    } else {
        error
    }
}

#[cfg(test)]
mod tests;

#[cfg(all(test, target_os = "linux"))]
mod linux_tests;

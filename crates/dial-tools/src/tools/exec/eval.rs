//! Owner-local persistent JavaScript/Python execution. The OS sandbox is the security boundary.
use crate::config::tools::eval::EvalConfig;

use super::background::AdoptedJob;
use super::kernel::{
    CancelDelivery, CellAction, CellOutcome, CellRuntime, CellSpec, NativeProcess, PythonRuntime,
    code_output, require_fork_containment, resolve_operator_executable, strip_runtime_injection,
};
#[cfg(not(windows))]
use super::kernel::{fenced_command, fork_fence_prefix, shell_quote};
use crate::config::tools::kernel::KernelConfig;
use crate::config::types::Config;
use crate::tools::integrations::ext::{Tool, ToolCallback, ToolContext, ToolEvent};
use crate::tools::{ToolOutput, Workspace, tool_schema};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

const DRIVER: &str = include_str!("eval/driver.cjs");
const MAX_CODE: usize = 64 * 1024;

/// Reserved callback name for `tool.batch([...])`; never a valid provider tool name. The
/// execution controller runs the entries together when all are read-only, else one at a time.
/// Controllers without batch support refuse it, and the driver then calls each entry serially.
pub const BATCH_CALLBACK: &str = "dial.batch";

pub(super) mod cells;
pub(super) mod checks;
pub use checks::{check_source_digest, is_check_source_path};
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

fn embedded_fallback_available(node: &str) -> bool {
    embedded_js_available() && (node != "node" || !crate::setup::deps::bundled_node::available())
}

/// `None` selects QuickJS; an unusable release payload must never silently switch engines.
fn js_launcher(workspace: &Path, node: &str) -> Result<Option<std::path::PathBuf>, String> {
    match resolve_operator_executable(workspace, node, "eval.node") {
        Ok(node) => Ok(Some(node)),
        Err(_) if embedded_fallback_available(node) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Languages backed by a resolved operator launcher or the compiled JavaScript engine. Only
/// paths/build facts are inspected; version and containment preflight runs at cell launch.
pub fn installed_languages(workspace: &std::path::Path, config: &Config) -> Vec<&'static str> {
    let mut languages = Vec::new();
    if js_launcher(workspace, &config.eval.node).is_ok() {
        languages.push("js");
    }
    if super::kernel::python_launcher_resolves(workspace, &config.kernel) {
        languages.push("py");
    }
    languages
}

/// Containment is the only eval prerequisite observable without starting a cell. It does not
/// verify an operator's interpreter installation.
fn containment_readiness() -> crate::tools::surface::catalog::Readiness {
    use crate::tools::surface::catalog::Readiness;
    match super::kernel::cached_fork_containment() {
        Some(Err(reason)) => Readiness::MissingPrerequisite { reason },
        Some(Ok(())) | None => Readiness::Unknown,
    }
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
            reason: "No eval interpreter resolves. JavaScript needs a usable Node 22+ launcher or the embedded engine; Python needs a bundled or configured Python 3.8+ launcher. Loading eval does not install an interpreter. Status/reset/cancel remain available.".into(),
        }
    } else {
        containment_readiness()
    }
}

/// Bounded environment facts for the session prompt, using the same resolution as eval itself.
pub fn runtime_guidance(workspace: &std::path::Path, config: &Config) -> String {
    let js = match js_launcher(workspace, &config.eval.node) {
        Ok(Some(_)) => "resolved: Node (use await import('node:…'), not require)",
        Ok(None) => {
            "resolved: embedded QuickJS (no Node modules except require('fs') and require('path') shims, no Buffer; use tool callbacks and Uint8Array)"
        }
        Err(_) => "not found",
    };
    let python = if super::kernel::python_launcher_resolves(workspace, &config.kernel) {
        "resolved"
    } else {
        "not found"
    };
    format!(
        "- Eval launchers: JavaScript {js}; Python {python}. Version and containment are checked at execution. Loading a tool installs no runtime. Check the shell environment before generating a helper: eval launchers need not be on PATH.\n"
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

/// Persistent JavaScript namespace owned by one execution owner. The shared cell lifecycle lives in
/// [`CellRuntime`]; this type supplies only the Node or embedded-engine launch.
struct JsRuntime {
    config: EvalConfig,
    cell: CellRuntime,
}
impl JsRuntime {
    fn new(config: EvalConfig) -> Self {
        Self {
            config,
            cell: CellRuntime::new(),
        }
    }
    /// Cancels the in-flight cell and reports whether one was running. JavaScript cells are killed
    /// outright rather than interrupted.
    fn cancel(&self) -> bool {
        self.cell.cancel(CancelDelivery::KillGroup)
    }
    #[allow(clippy::too_many_arguments)]
    fn run(
        &self,
        ctx: &ToolContext,
        action: CellAction<'_>,
        args: &Value,
        cancel: &AtomicBool,
        adopt: &dyn Fn(AdoptedJob) -> Result<String, String>,
        dispatch: &mut dyn FnMut(ToolCallback) -> ToolOutput,
        effect_started: &AtomicBool,
    ) -> Result<CellOutcome, String> {
        // Redeclaration rewriting is a pure source transform, applied before admission so that a
        // busy runtime and the namespace see the same code.
        let redeclared;
        let action = match action {
            CellAction::Eval(code) => {
                redeclared = cells::redeclarable(code);
                CellAction::Eval(&redeclared)
            }
            other => other,
        };
        let spec = CellSpec {
            code: "js",
            name: "JavaScript",
            timeout_secs: self.config.timeout_secs,
        };
        self.cell.run(
            ctx,
            spec,
            action,
            args,
            cancel,
            adopt,
            dispatch,
            effect_started,
            |slot, attempt| {
                let node = js_launcher(ctx.workspace.root(), &self.config.node)?;
                let kernel = if node.is_some() {
                    "Node kernel (install/configure eval.node)"
                } else {
                    "embedded JavaScript engine"
                };
                attempt.check()?;
                let mut command = js_kernel_command(ctx.workspace, node.as_deref())?;
                // Even the explicit filesystem-sandbox opt-out cannot inject code before the fence.
                strip_runtime_injection(&mut command);
                attempt.check()?;
                let mut process = NativeProcess::start(command, attempt.pid.clone())
                    .map_err(|error| format!("starting {kernel}: {error}"))?;
                process.backend = Some(if node.is_some() {
                    super::preludes::EvalBackend::Node
                } else {
                    super::preludes::EvalBackend::QuickJs
                });
                *slot = Some(process);
                Ok(())
            },
        )
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
                "Persistent JavaScript (default) or Python 3.8+; separate namespaces keep top-level bindings (var in JS): patch earlier definitions, don't resend whole scripts. Embedded JS has no Node modules except require('fs') and require('path') shims; OS containment required; subprocesses/native addons denied. Portable dial helpers are dial.* methods (JS await, Python sync): read/readText/readJson/readBytes(path), readLines/lines, writeText/writeJson (overwrite only with expectedSha256, Python expected_sha256=, from a whole-file read receipt; never read implicitly), writeBytes (create only), run, batch (raises unless collect:true), check(name, bool, optional dial.read receipt; local only), snapshot/fresh/restore, hex/pack/unpack; 1 MiB/whole-file call; relative paths resolve in the project directory. tool.<name>({...}) and tool.batch([{name,args},...]) take objects; denials raise ToolError. Generated files: in one cell read inputs, compute, write, reopen and check saved bytes; prove tricky logic on a tiny known-answer case; never print then transcribe. Print short summaries; stdout isn't check evidence. Failed cells can leave effects.",
            ),
            json!({
                "language":{"type":"string","enum":["js","py"],"description":"Defaults to js. Windows Python requires explicit unsandboxed opt-in."},
                "action":{"type":"string","enum":["eval","reset","cancel","status"],"description":"Default eval; reset/status/cancel need no code. Cancel without language affects both."},
                "code":{"type":"string","description":"Required nonblank source for eval, max 64 KiB."},
                "timeout":{"type":"integer","minimum":0,"maximum":3600,"description":"Seconds; timeout or cancel discards the namespace. Callback time is excluded."},
                "background":{"type":"boolean","description":"Omit to follow shell.auto_background_secs (default 0: the cell stays in the foreground); false keeps it in the foreground; true moves it immediately."},
                "title":{"type":"string","description":"Label for a backgrounded cell in the background job list."}
            }),
            &[],
        );
        if !self.js.config.portable_helpers
            && let Some(description) = schema["function"]["description"].as_str()
            && let Some((start, remaining)) = description.split_once("Portable dial helpers")
            && let Some((_, rest)) = remaining.split_once("tool.<name>")
        {
            schema["function"]["description"] = json!(format!(
                "{start}Portable helpers are disabled. tool.<name>{rest}"
            ));
        }
        schema
    }
    fn readiness(&self, _: &Config) -> crate::tools::surface::catalog::Readiness {
        containment_readiness()
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
        self.call_streaming(ctx, args, &|_| {}, &AtomicBool::new(false))
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
            Err(EvalError::Invalid(error)) => ToolOutput::err(error)
                .with_effects(crate::runtime::execution::Effects::None),
            Err(EvalError::Runtime(error)) => {
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

#[derive(Debug, thiserror::Error)]
enum EvalError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Runtime(String),
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
    ) -> Result<CellOutcome, EvalError> {
        let invalid = |message: &str| EvalError::Invalid(message.to_owned());
        let action = match args.get("action") {
            None | Some(Value::Null) => "eval",
            Some(value) => value
                .as_str()
                .ok_or_else(|| invalid("action must be a string"))?,
        };

        let language = match args.get("language") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_str()
                    .ok_or_else(|| invalid("language must be a string"))?,
            ),
        };
        if let Some(other) = language
            && !matches!(other, "js" | "py")
        {
            return Err(EvalError::Invalid(format!(
                "unsupported eval language {other:?}; use js or py"
            )));
        }
        if action == "cancel" {
            // Without a language, cancel reaches both namespaces.
            let mut cancelled = false;
            if language != Some("py") {
                cancelled |= self.js.cancel();
            }
            if language != Some("js") {
                cancelled |= self.python.cancel();
            }
            return Ok(CellOutcome::Value(
                json!({"cancelled":cancelled,"namespace_discarded":cancelled}),
            ));
        }
        let language = language.unwrap_or("js");
        let action = match action {
            "status" => CellAction::Status,
            "reset" => CellAction::Reset,
            "eval" => {
                let code = args.get("code").and_then(Value::as_str);
                if code.is_some_and(|s| s.len() > MAX_CODE) {
                    return Err(EvalError::Invalid(format!(
                        "code exceeds {MAX_CODE} bytes; split the cell"
                    )));
                }
                let code = code.filter(|s| !s.trim().is_empty()).ok_or_else(|| EvalError::Invalid(format!(
                    r#"No cell ran. Send a nonblank string in the top-level code field, e.g. {{"language":"{language}","code":"1 + 1"}}. Only status, reset and cancel omit code."#
                )))?;
                CellAction::Eval(code)
            }
            other => {
                return Err(EvalError::Invalid(format!(
                    "unsupported eval action {other:?}; use eval, status, reset or cancel"
                )));
            }
        };
        if language == "py" {
            self.python
                .run(ctx, action, args, cancel, adopt, dispatch, effect_started)
                .map_err(|error| python_unavailable_hint(error, ctx))
        } else {
            self.js
                .run(ctx, action, args, cancel, adopt, dispatch, effect_started)
        }
        .map_err(EvalError::Runtime)
    }
}

/// A Python interpreter that cannot be found or started is not the end of eval: when JavaScript
/// resolves, the error says so. Other Python failures are returned unchanged.
fn python_unavailable_hint(error: String, ctx: &ToolContext) -> String {
    let launcher_failed = error.contains("kernel.python");
    if launcher_failed && installed_languages(ctx.workspace.root(), ctx.config).contains(&"js") {
        format!(
            "{error}\nJavaScript eval is available for language-independent work. It cannot verify Python-specific behavior."
        )
    } else {
        error
    }
}

#[cfg(test)]
mod tests;

#[cfg(all(test, target_os = "linux"))]
mod linux_tests;

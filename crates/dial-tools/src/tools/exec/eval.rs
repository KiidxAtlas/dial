//! Owner-local persistent Node code execution. The OS sandbox is the security boundary.
use crate::config::tools::eval::EvalConfig;

use super::background::AdoptedJob;
use super::kernel::{
    ActiveGuard, BackgroundPolicy, CellOutcome, CellScope, Deadline, Handoff, MAX_CODE,
    NativeProcess, PythonRuntime, busy_job, cell_label, check_operation, code_output, deadline,
    execution_deadline, fenced_command, guarded_operation, lock_session, native_cell,
    require_fork_containment, resolve_operator_executable, shell_quote, strip_runtime_injection,
};
use crate::config::tools::kernel::KernelConfig;
use crate::config::types::Config;
use crate::tools::integrations::ext::{Tool, ToolCallback, ToolContext, ToolEvent};
use crate::tools::{ToolOutput, tool_schema};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering},
};

const DRIVER: &str = include_str!("eval/driver.cjs");

use super::kernel::fork_fence_prefix;

/// Explicitly prepare mandatory containment for an automatic offer, without starting an interpreter.
/// Catalog/readiness queries only inspect the cached observation; they never call this hook.
pub fn prepare_containment() -> Result<(), String> {
    require_fork_containment("eval")
}

/// Cell languages whose configured launcher resolves to an operator-installed executable, in
/// schema order. Only paths are inspected; no interpreter starts, so a resolved launcher can still
/// fail its preflight when a cell runs.
pub fn installed_languages(workspace: &std::path::Path, config: &Config) -> Vec<&'static str> {
    [
        ("js", config.eval.node.as_str(), "eval.node"),
        ("py", config.kernel.python.as_str(), "kernel.python"),
    ]
    .into_iter()
    .filter(|(_, program, label)| resolve_operator_executable(workspace, program, label).is_ok())
    .map(|(language, _, _)| language)
    .collect()
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
                        let node = resolve_operator_executable(
                            ctx.workspace.root(),
                            &self.config.node,
                            "eval.node",
                        )?;
                        check_operation(end, &self.interrupt, cancel)?;
                        // The shell preserves only the trusted protocol on fd 3. Native stdout/stderr
                        // cannot be interpreted as controller callbacks or result frames.
                        let script = format!(
                            "{} {} --no-addons -e {} 3>&1 1>&2",
                            fork_fence_prefix(ctx.workspace),
                            shell_quote(node.to_str().ok_or("Node executable path is not UTF-8")?),
                            shell_quote(DRIVER),
                        );
                        let mut command = fenced_command(ctx.workspace, &script)?;
                        // Even the explicit filesystem-sandbox opt-out cannot inject code before the fence.
                        strip_runtime_injection(&mut command);
                        check_operation(end, &self.interrupt, cancel)?;
                        *slot = Some(NativeProcess::start(command, self.pid.clone()).map_err(
                            |error| {
                                format!(
                                    "starting Node kernel (install/configure eval.node): {error}"
                                )
                            },
                        )?);
                    }
                    check_operation(end, &self.interrupt, cancel)?;
                    slot.as_mut().unwrap().prelude_language =
                        Some(crate::tools::exec::preludes::PreludeLanguage::Js);
                    native_cell(
                        slot.as_mut().unwrap(),
                        &scope,
                        code,
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
        tool_schema(
            "eval",
            &crate::tools::surface::catalog::execution_description(
                "eval",
                "Persistent JavaScript (Node 22+) or Python 3.8+ cells, with separate namespaces. Call tool.<name>(args) or tool.call(name,args) under normal approval: JS requires await; Python callbacks are synchronous dictionaries (also await-compatible, without replay). Denied or malformed callbacks raise ToolError. For generated data use one foreground cell (background:false): read original inputs programmatically, compute, serialize, write through tool.write, then assert saved bytes, unchanged inputs, expected shape and independently expected results. Do not transcribe source rows or printed values. Reuse namespace variables until inputs change; then reread and recompute. Print a short validation summary. Assertions cover the saved artifact; stdout alone is not check evidence, and equality alone does not prove the computation. A failed assertion does not undo or authorize replaying a write. Source/config changes use write/edit. reset/status/cancel are owner-scoped; cancel without language affects both. Timeout/cancel destroys the namespace and pending callbacks; callback time is excluded. Requires approval and OS containment; subprocesses/native JS addons are denied, and Windows Python requires explicit unsandboxed opt-in.",
            ),
            json!({
                "language":{"type":"string","enum":["js","py"],"description":"Runtime for this cell; defaults to js."},
                "action":{"type":"string","enum":["eval","reset","cancel","status"]},
                "code":{"type":"string","description":"Cell source, at most 64 KiB UTF-8."},
                "timeout":{"type":"integer","minimum":0,"maximum":3600},
                "background":{"type":"boolean","description":"Omit for automatic handoff of a long-running cell to the background; false keeps it in the foreground; true moves it immediately."},
                "title":{"type":"string","description":"Label for a backgrounded cell in the background job list."}
            }),
            &[],
        )
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
                .run(ctx, action, args, cancel, adopt, dispatch, effect_started),
            other => Err(format!("unsupported eval language {other:?}; use js or py")),
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(all(test, target_os = "linux"))]
mod linux_tests;

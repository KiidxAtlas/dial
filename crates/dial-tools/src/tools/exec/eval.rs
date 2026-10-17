//! Owner-local persistent Node code execution. The OS sandbox is the security boundary.
use crate::config::tools::eval::EvalConfig;

use super::kernel::{
    ActiveGuard, FORK_FENCE, MAX_CODE, NativeProcess, check_operation, code_output, deadline,
    execution_deadline, guarded_operation, lock_session, native_cell, require_fork_containment,
    resolve_operator_executable, shell_quote, strip_runtime_injection,
};
use super::shell::shell_command;
use crate::config::types::Config;
use crate::tools::integrations::ext::{Tool, ToolCallback, ToolContext, ToolEvent};
use crate::tools::{ToolOutput, tool_schema};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering},
};

const DRIVER: &str = include_str!("../eval_driver.cjs");

pub fn tools(config: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(EvalTool::new(config.eval.clone()))]
}

struct EvalTool {
    config: EvalConfig,
    session: Mutex<Option<NativeProcess>>,
    pid: Arc<AtomicI32>,
    interrupt: AtomicBool,
    active: AtomicUsize,
}
impl EvalTool {
    fn new(config: EvalConfig) -> Self {
        Self {
            config,
            session: Mutex::new(None),
            pid: Arc::new(AtomicI32::new(0)),
            interrupt: AtomicBool::new(false),
            active: AtomicUsize::new(0),
        }
    }
    fn run(
        &self,
        ctx: &ToolContext,
        args: &Value,
        cancel: &AtomicBool,
        dispatch: &mut dyn FnMut(ToolCallback) -> ToolOutput,
        effect_started: &AtomicBool,
    ) -> Result<Value, String> {
        if args
            .get("language")
            .is_some_and(|language| !language.is_null() && language.as_str() != Some("js"))
        {
            return Err("eval language must be js; use kernel for Python".into());
        }
        let action = match args.get("action") {
            None | Some(Value::Null) => "eval",
            Some(value) => value.as_str().ok_or("action must be a string")?,
        };
        if action == "cancel" {
            // Only an in-flight operation has anything to interrupt; a flag left behind would reject the next cell.
            let cancelled = self.active.load(Ordering::Acquire) > 0;
            if cancelled {
                self.cancel_operation();
            }
            return Ok(json!({"cancelled":cancelled,"namespace_discarded":true}));
        }
        if !matches!(action, "eval" | "reset" | "status") {
            return Err(format!("unsupported eval action {action:?}"));
        }
        // Counts the whole operation, including lock wait, so cancel reaches it before startup.
        let _active = ActiveGuard::new(&self.active);
        let end = execution_deadline(deadline(args, self.config.timeout_secs)?);
        let pending_cancel = self.interrupt.swap(false, Ordering::AcqRel);
        if cancel.load(Ordering::Acquire) || (pending_cancel && action == "eval") {
            return Err("JavaScript execution cancelled before startup".into());
        }
        let callback_wait = AtomicBool::new(false);
        let result = guarded_operation(
            end,
            &self.pid,
            &self.interrupt,
            cancel,
            &callback_wait,
            || {
                let mut slot = lock_session(&self.session, end, &self.interrupt, cancel)?;
                if action == "reset" {
                    slot.take();
                    return Ok(json!({"reset":true}));
                }
                if action == "status" {
                    return Ok(
                        json!({"running":slot.as_mut().is_some_and(NativeProcess::alive),"language":"js",
                            "platform":crate::tools::surface::catalog::execution_platform("eval")}),
                    );
                }
                let code = args
                    .get("code")
                    .and_then(Value::as_str)
                    .ok_or("eval requires code")?;
                if code.len() > MAX_CODE {
                    return Err(format!("code exceeds {MAX_CODE} bytes"));
                }
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
                            "exec /usr/bin/sandbox-exec -p {} {} --no-addons -e {} 3>&1 1>&2",
                            shell_quote(FORK_FENCE),
                            shell_quote(node.to_str().ok_or("Node executable path is not UTF-8")?),
                            shell_quote(DRIVER),
                        );
                        let mut command =
                            shell_command(ctx.workspace, &script, ctx.workspace.root())?;
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
                    effect_started.store(true, Ordering::Release);
                    native_cell(
                        slot.as_mut().unwrap(),
                        ctx.workspace,
                        code,
                        end,
                        &self.interrupt,
                        cancel,
                        &callback_wait,
                        dispatch,
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
                "Persistent operator-installed Node 22+ JavaScript namespace (not browser page evaluation). action defaults to eval; reset/cancel/status manage only this execution owner's session. Real top-level await and lexical declarations persist. tool.<name>({...}), tool.call(name,args), and tool.schemas use the current controller's direct-tool authorization, approval, validation and write observers; callbacks are synchronous, and denied/malformed calls throw ToolError retaining result metadata. Returns bounded stdout/stderr/value/error; callback results retain text/data/is_error. Requires approval, macOS OS sandbox and mandatory process-fork containment, including when filesystem sandboxing is disabled. Node runtime threads are permitted; subprocess creation and native addons are denied. Timeout/cancel destroys this owner's namespace; if a controller callback is pending its approval/nested operation is also cancelled. Use kernel for Python.",
            ),
            json!({
                "language":{"type":"string","enum":["js"]},
                "action":{"type":"string","enum":["eval","reset","cancel","status"]},
                "code":{"type":"string","description":"JavaScript cell, at most 64 KiB UTF-8."},
                "timeout":{"type":"integer","minimum":0,"maximum":3600}
            }),
            &[],
        )
    }
    fn needs_approval(&self, _: &Value) -> bool {
        true
    }
    fn supports_tool_dispatch(&self) -> bool {
        true
    }
    fn fork_session(&self) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self::new(self.config.clone())))
    }
    fn cancel_operation(&self) {
        if self.active.load(Ordering::Acquire) == 0 {
            return;
        }
        self.interrupt.store(true, Ordering::Release);
        let pid = self.pid.load(Ordering::Acquire);
        if pid > 0 {
            let _ = crate::platform::process::kill_group(pid as u32);
        }
    }
    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        self.call_with_dispatch(ctx, args, &|_| {}, &AtomicBool::new(false), &mut |_| {
            ToolOutput::err("tool callbacks require an execution controller")
        })
    }
    fn call_streaming(
        &self,
        ctx: &ToolContext,
        args: &Value,
        on_event: &(dyn Fn(ToolEvent) + Sync),
        cancel: &AtomicBool,
    ) -> ToolOutput {
        self.call_with_dispatch(ctx, args, on_event, cancel, &mut |_| {
            ToolOutput::err("tool callbacks require an execution controller")
        })
    }
    fn call_with_dispatch(
        &self,
        ctx: &ToolContext,
        args: &Value,
        _on_event: &(dyn Fn(ToolEvent) + Sync),
        cancel: &AtomicBool,
        dispatch: &mut dyn FnMut(ToolCallback) -> ToolOutput,
    ) -> ToolOutput {
        let started = AtomicBool::new(false);
        let result = self.run(ctx, args, cancel, dispatch, &started);
        let uncertain = result.is_err() && started.load(Ordering::Acquire);
        let output = code_output(result);
        if uncertain {
            output.with_effects(crate::tools::surface::outcome::Effects::Unknown)
        } else {
            output
        }
    }
}

//! Persistent Python execution with controller-owned tool callbacks and OS process containment.
//!
//! Cancellation and timeouts first deliver SIGINT, which the driver turns into `KeyboardInterrupt`
//! inside the running cell while keeping the namespace; only a cell that ignores it for
//! [`INTERRUPT_GRACE`] is killed together with its session.
use super::ext::{Tool, ToolCallback, ToolContext, ToolEvent};
#[cfg(windows)]
use super::sandbox::Sandbox;
use super::shell::ProcessGroup;
#[cfg(not(windows))]
use super::shell::shell_command;
use super::{ToolOutput, Workspace, preludes, tool_schema};
use crate::config::types::Config;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(unix)]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

mod env;

pub(crate) const PYTHON_DRIVER: &str = include_str!("kernel_driver.py");
pub(crate) const MAX_CODE: usize = 64 * 1024;
pub(crate) const MAX_FRAME: usize = 12 * 1024 * 1024;
/// How long an interrupted cell may take to unwind before the session is killed.
pub(crate) const INTERRUPT_GRACE: Duration = Duration::from_secs(5);
const DEFAULT_PYTHON: &str = "python3";
// No descendant can detach: even with the operator's filesystem sandbox opt-out, fork/spawn is denied.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) const FORK_FENCE: &str = "(version 1)(allow default)(deny process-fork)";
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const BWRAP: &str = "/usr/bin/bwrap";
const PYTHON_PREFLIGHT: &str = "import sys,json\nr=json.loads(sys.stdin.readline())\nprint(json.dumps({'id':r['id'],'result':{'executable':sys.executable,'version':list(sys.version_info[:2])}}),flush=True)";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct KernelConfig {
    pub python: String,
    pub timeout_secs: u64,
    /// Project environment: `"auto"`, `"off"`, or an explicit virtualenv/conda prefix.
    pub venv: String,
}
impl Default for KernelConfig {
    fn default() -> Self {
        Self {
            python: DEFAULT_PYTHON.into(),
            timeout_secs: 0,
            venv: "auto".into(),
        }
    }
}

pub fn tools(config: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(KernelTool {
        config: config.kernel.clone(),
        session: Mutex::new(None),
        pid: Arc::new(AtomicI32::new(0)),
        interrupt: AtomicBool::new(false),
        active: AtomicUsize::new(0),
    })]
}
pub(crate) struct ActiveGuard<'a>(&'a AtomicUsize);
impl<'a> ActiveGuard<'a> {
    pub(crate) fn new(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, Ordering::AcqRel);
        Self(count)
    }
}
impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
struct KernelTool {
    config: KernelConfig,
    session: Mutex<Option<JsonProcess>>,
    pid: Arc<AtomicI32>,
    interrupt: AtomicBool,
    active: AtomicUsize,
}
impl Tool for KernelTool {
    fn name(&self) -> &str {
        "kernel"
    }
    fn schema(&self) -> Value {
        tool_schema(
            "kernel",
            "Persistent Python 3.8+ namespace with real top-level await. tool.<name>(**kwargs), tool.call(name,args), and tool.schemas return to the current controller for the same permissions, approvals, validation and observers as direct tools. Results retain text/data/is_error; denied or malformed calls raise ToolError. Each cell requires approval and runs under the workspace OS sandbox plus a mandatory process-creation fence, even when filesystem sandboxing is disabled. Windows requires explicit unsandboxed opt-in because filesystem/network isolation is unavailable; its trusted driver installs a native no-child-process mitigation before user code. stdout/stderr/final expression/tracebacks are bounded. Timeout/cancel destroys this owner's namespace; if a controller callback is pending its approval/nested operation is also cancelled. Reset starts fresh lazily. macOS containment or the Windows native fence and operator-installed Python are required.",
            json!({
                "action":{"type":"string","enum":["eval","reset","cancel","status"]},
                "code":{"type":"string","description":"Python cell, at most 64 KiB UTF-8."},
                "timeout":{"type":"integer","minimum":0,"maximum":3600}
            }),
            &["action"],
        )
    }
    fn needs_approval(&self, _: &Value) -> bool {
        true
    }
    fn fork_session(&self) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self {
            config: self.config.clone(),
            session: Mutex::new(None),
            pid: Arc::new(AtomicI32::new(0)),
            interrupt: AtomicBool::new(false),
            active: AtomicUsize::new(0),
        }))
    }
    fn cancel_operation(&self) {
        // The running operation's watcher delivers SIGINT and escalates to a kill after INTERRUPT_GRACE.
        if self.active.load(Ordering::Acquire) > 0 {
            self.interrupt.store(true, Ordering::Release);
        }
    }
    fn supports_tool_dispatch(&self) -> bool {
        true
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
            output.with_effects(super::outcome::Effects::Unknown)
        } else {
            output
        }
    }
}
impl KernelTool {
    fn run(
        &self,
        ctx: &ToolContext,
        args: &Value,
        cancel: &AtomicBool,
        dispatch: &mut dyn FnMut(ToolCallback) -> ToolOutput,
        effect_started: &AtomicBool,
    ) -> Result<Value, String> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .ok_or("action is required")?;
        if action == "cancel" {
            // Only an in-flight operation has anything to interrupt; a flag left behind would reject the next cell.
            let requested = self.active.load(Ordering::Acquire) > 0;
            if requested {
                self.cancel_operation();
            }
            return Ok(json!({"cancel_requested":requested}));
        }
        if !matches!(action, "eval" | "reset" | "status") {
            return Err(format!("unsupported kernel action {action:?}"));
        }
        // Counts the whole operation, including lock wait, so cancel reaches it before startup.
        let _active = ActiveGuard::new(&self.active);
        let end = execution_deadline(deadline(args, self.config.timeout_secs)?);
        // A pending execution cancellation must never admit another cell. Explicit control
        // operations may still inspect/reset the now-dead namespace.
        let pending_cancel = self.interrupt.swap(false, Ordering::AcqRel);
        if cancel.load(Ordering::Acquire) || (pending_cancel && action == "eval") {
            return Err("kernel execution cancelled before startup".into());
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
                    return Ok(json!({"running":slot.as_mut().is_some_and(JsonProcess::alive)}));
                }
                let code = args
                    .get("code")
                    .and_then(Value::as_str)
                    .ok_or("eval requires code")?;
                if code.len() > MAX_CODE {
                    return Err(format!("code exceeds {MAX_CODE} bytes"));
                }
                #[cfg(windows)]
                if !ctx.workspace.unsandboxed {
                    let isolation = ctx
                        .workspace
                        .sandbox
                        .clone()
                        .unwrap_or_else(Sandbox::detect)
                        .available()
                        .err()
                        .unwrap_or_else(|| {
                            "Windows filesystem/network isolation is unavailable".into()
                        });
                    return Err(format!(
                        "Windows Python kernels require the explicit unsandboxed setting because filesystem/network isolation is unavailable: {isolation}"
                    ));
                }
                let result = (|| {
                    if slot.as_mut().is_some_and(|s| !s.alive()) {
                        slot.take();
                    }
                    if slot.is_none() {
                        let root = ctx.workspace.root();
                        let timeout = remaining_timeout(end)?;
                        let environment =
                            env::detect(root, &self.config.venv, &self.config.python, &|key| {
                                std::env::var(key).ok()
                            })?;
                        let launch = match environment {
                            None => {
                                let (python, _) = resolve_python(
                                    root,
                                    &self.config.python,
                                    timeout,
                                    self.pid.clone(),
                                    &self.interrupt,
                                )?;
                                Launch {
                                    python,
                                    site: None,
                                    isolate_site: false,
                                }
                            }
                            Some(environment) => resolve_environment(
                                root,
                                &environment,
                                timeout,
                                &self.pid,
                                &self.interrupt,
                            )?,
                        };
                        check_operation(end, &self.interrupt, cancel)?;
                        #[cfg(windows)]
                        let mut command = {
                            let mut command = Command::new(&launch.python);
                            command
                                .args(["-I", "-S", "-u", "-c"])
                                .arg(PYTHON_DRIVER)
                                .args(launch.site.iter())
                                .env_clear()
                                .current_dir(ctx.workspace.root());
                            for (key, value) in std::env::vars_os() {
                                if !super::sandbox::is_secret_env(
                                    &key.to_string_lossy(),
                                    &value.to_string_lossy(),
                                ) {
                                    command.env(key, value);
                                }
                            }
                            command
                        };
                        #[cfg(not(windows))]
                        let mut command = {
                            let flags = if launch.isolate_site {
                                "-I -S -u -c"
                            } else {
                                "-I -u -c"
                            };
                            let sites = match &launch.site {
                                Some(site) => format!(
                                    " {}",
                                    shell_quote(
                                        site.to_str()
                                            .ok_or("kernel site-packages path is not UTF-8")?
                                    )
                                ),
                                None => String::new(),
                            };
                            let script = format!(
                                "exec /usr/bin/sandbox-exec -p {} {} {flags} {} {sites}",
                                shell_quote(FORK_FENCE),
                                shell_quote(
                                    launch
                                        .python
                                        .to_str()
                                        .ok_or("Python executable path is not UTF-8")?
                                ),
                                shell_quote(PYTHON_DRIVER),
                            );
                            shell_command(ctx.workspace, &script, ctx.workspace.root())?
                        };
                        strip_runtime_injection(&mut command);
                        require_fork_containment("kernel")?;
                        check_operation(end, &self.interrupt, cancel)?;
                        *slot =
                            Some(JsonProcess::start(command, self.pid.clone()).map_err(|e| {
                                format!(
                                    "starting Python kernel (install/configure kernel.python): {e}"
                                )
                            })?);
                    }
                    check_operation(end, &self.interrupt, cancel)?;
                    slot.as_mut().unwrap().prelude_language =
                        Some(preludes::PreludeLanguage::Python);
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
                    // Only this owner's namespace is discarded; a pending host callback additionally
                    // propagates cancellation to its controller so approval/nested calls can unblock.
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
pub(crate) fn resolve_operator_executable(
    workspace: &Path,
    program: &str,
    label: &str,
) -> Result<PathBuf, String> {
    let path = Path::new(program);
    let candidate = if path.is_absolute() || path.components().count() > 1 {
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            workspace.join(path)
        };
        resolve_executable_candidate(&candidate)
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join(program))
            .find_map(|candidate| resolve_executable_candidate(&candidate))
    }
    .ok_or_else(|| {
        format!("cannot find {label} {program:?}; install its operator-owned executable")
    })?;
    let resolved = candidate
        .canonicalize()
        .map_err(|e| format!("{label} {program:?}: {e}"))?;
    if resolved.starts_with(workspace) {
        return Err(format!(
            "{label} must be an operator-installed executable outside the editable workspace"
        ));
    }
    Ok(resolved)
}

fn executable_file(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && crate::platform::fs::is_executable(path))
}

fn resolve_executable_candidate(path: &Path) -> Option<PathBuf> {
    if executable_file(path) {
        return Some(path.to_path_buf());
    }
    #[cfg(windows)]
    if path.extension().is_none() {
        // CreateProcess needs an image path. Search executable PATHEXT suffixes explicitly;
        // `.bat`/`.cmd` entries require a shell and are not valid trusted native programs here.
        let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        for extension in extensions.split(';').filter(|extension| {
            matches!(
                extension
                    .trim()
                    .trim_start_matches('.')
                    .to_ascii_lowercase()
                    .as_str(),
                "com" | "exe"
            )
        }) {
            let candidate = path.with_extension(extension.trim().trim_start_matches('.'));
            if executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn resolve_python(
    workspace: &Path,
    program: &str,
    timeout: Duration,
    pid: Arc<AtomicI32>,
    interrupt: &AtomicBool,
) -> Result<(PathBuf, (u64, u64)), String> {
    let launcher = resolve_operator_executable(workspace, program, "kernel.python")?;
    let scratch = tempfile::Builder::new()
        .prefix("dial-kernel-resolve-")
        .tempdir()
        .map_err(|e| e.to_string())?;
    let mut command = Command::new(launcher);
    // Resolve operator shims without importing workspace/site code.
    command
        .args(["-I", "-S", "-u", "-c"])
        .arg(PYTHON_PREFLIGHT)
        .env_clear()
        .current_dir(scratch.path());
    for (key, value) in std::env::vars_os() {
        if !super::sandbox::is_secret_env(&key.to_string_lossy(), &value.to_string_lossy()) {
            command.env(key, value);
        }
    }
    strip_runtime_injection(&mut command);
    command
        .env("PWD", scratch.path())
        .env("PYENV_DIR", scratch.path());
    if interrupt.load(Ordering::Acquire) {
        return Err("kernel preflight cancelled".into());
    }
    let mut process = JsonProcess::start(command, pid)?;
    // Cancellation before PID publication cannot kill the new process itself.
    // Recheck immediately after registration so startup never enters an uncancellable wait.
    if interrupt.load(Ordering::Acquire) {
        return Err("kernel preflight cancelled".into());
    }
    let result = process.request(json!({}), timeout, |_| {
        Err("unexpected Python resolution callback".into())
    })?;
    let executable = result["executable"]
        .as_str()
        .ok_or("Python preflight did not report sys.executable")?;
    let version = result["version"]
        .as_array()
        .and_then(|v| Some((v.first()?.as_u64()?, v.get(1)?.as_u64()?)))
        .ok_or("Python preflight did not report sys.version_info")?;
    Ok((
        resolve_operator_executable(workspace, executable, "resolved kernel.python")?,
        version,
    ))
}

/// What the driver is launched with: the base interpreter, the venv `site-packages` the driver adds
/// through `site.addsitedir`, and whether the base's own site packages must stay hidden.
struct Launch {
    python: PathBuf,
    site: Option<PathBuf>,
    /// Unix launches choose `-S` from this; Windows always passes `-I -S`.
    #[cfg_attr(windows, allow(dead_code))]
    isolate_site: bool,
}

/// Resolves the base interpreter a project environment runs on. Only a virtualenv contributes a
/// site directory; a conda prefix's interpreter already owns its site.
fn resolve_environment(
    workspace: &Path,
    environment: &env::PythonEnv,
    timeout: Duration,
    pid: &Arc<AtomicI32>,
    interrupt: &AtomicBool,
) -> Result<Launch, String> {
    let mut last = String::from("no interpreter candidates");
    for candidate in &environment.launchers {
        let program = candidate
            .to_str()
            .ok_or("kernel environment interpreter path is not UTF-8")?;
        match resolve_python(workspace, program, timeout, pid.clone(), interrupt) {
            Ok((python, version)) => {
                let (site, isolate_site) = match environment.kind {
                    env::EnvKind::Venv => (
                        Some(env::site_packages(&environment.prefix, version)?),
                        !environment.include_system_site,
                    ),
                    env::EnvKind::Conda => (None, false),
                };
                return Ok(Launch {
                    python,
                    site,
                    isolate_site,
                });
            }
            Err(error) => last = error,
        }
    }
    Err(format!(
        "kernel {} from {} is unusable: {last}",
        environment.kind.label(),
        environment.source
    ))
}

/// Zero disables a limit; a request cannot override a finite operator cap.
pub(crate) fn deadline(args: &Value, configured: u64) -> Result<Duration, String> {
    if configured > 3600 {
        return Err("configured timeout_secs must be in 0..=3600".into());
    }
    let requested = match args.get("timeout") {
        None | Some(Value::Null) => configured,
        Some(v) => v
            .as_u64()
            .filter(|n| *n <= 3600)
            .ok_or("timeout must be in 0..=3600")?,
    };
    Ok(Duration::from_secs(match (requested, configured) {
        (0, cap) => cap,
        (request, 0) => request,
        (request, cap) => request.min(cap),
    }))
}
pub(crate) fn execution_deadline(timeout: Duration) -> Option<Instant> {
    (!timeout.is_zero()).then(|| Instant::now() + timeout)
}
pub(crate) fn remaining_timeout(end: Option<Instant>) -> Result<Duration, String> {
    match end {
        None => Ok(Duration::ZERO),
        Some(end) => {
            let remaining = end.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                Err("execution deadline exceeded".into())
            } else {
                Ok(remaining)
            }
        }
    }
}
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

pub(crate) fn strip_runtime_injection(command: &mut Command) {
    for name in [
        "NODE_OPTIONS",
        "NODE_PATH",
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
        "DYLD_FRAMEWORK_PATH",
        "BASH_ENV",
        "ENV",
    ] {
        command.env_remove(name);
    }
}

pub(crate) fn code_output(result: Result<Value, String>) -> ToolOutput {
    match result {
        Ok(data) => ToolOutput {
            execution: Default::default(),
            is_error: data.get("error").is_some_and(|error| !error.is_null()),
            text: data.to_string(),
            data: Some(data),
        },
        Err(error) => ToolOutput::err(error),
    }
}

pub(crate) fn require_fork_containment(label: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        // The trusted kernel bootstrap applies ProcessChildProcessPolicy before user code.
        let _ = label;
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        super::sandbox::Sandbox::detect().available().map_err(|error| {
            format!("{label} requires the OS process-fork containment fence even when filesystem sandboxing is disabled: {error}")
        })
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Err(format!(
            "{label} requires OS process-fork containment; no unsandboxed or Linux fallback is provided"
        ))
    }
}

pub(crate) fn check_operation(
    end: Option<Instant>,
    interrupt: &AtomicBool,
    cancel: &AtomicBool,
) -> Result<(), String> {
    if end.is_some_and(|end| Instant::now() >= end) {
        return Err("execution deadline exceeded; namespace discarded".into());
    }
    if cancel.load(Ordering::Acquire) || interrupt.load(Ordering::Acquire) {
        return Err("execution cancelled; namespace discarded".into());
    }
    Ok(())
}

/// Includes lock waits, startup, pipe I/O and controller callback waits in one budget.
/// The controller observes the same flag and must propagate it to its nested operation.
pub(crate) fn guarded_operation<T>(
    end: Option<Instant>,
    pid: &AtomicI32,
    interrupt: &AtomicBool,
    cancel: &AtomicBool,
    callback_wait: &AtomicBool,
    run: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let done = AtomicBool::new(false);
    struct Finished<'a>(&'a AtomicBool);
    impl Drop for Finished<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut interrupted_at: Option<Instant> = None;
            while !done.load(Ordering::Acquire) {
                let expired = end.is_some_and(|end| Instant::now() >= end);
                if expired || cancel.load(Ordering::Acquire) || interrupt.load(Ordering::Acquire) {
                    if callback_wait.load(Ordering::Acquire) {
                        cancel.store(true, Ordering::Release);
                    }
                    interrupt.store(true, Ordering::Release);
                    let active = pid.load(Ordering::Acquire);
                    if active > 0 {
                        match interrupted_at {
                            None => {
                                if deliver_interrupt(active as u32) {
                                    interrupted_at = Some(Instant::now());
                                } else {
                                    let _ = crate::platform::process::kill_group(active as u32);
                                }
                            }
                            Some(at) if at.elapsed() >= INTERRUPT_GRACE => {
                                let _ = crate::platform::process::kill_group(active as u32);
                            }
                            Some(_) => {}
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let finished = Finished(&done);
        let result = run();
        drop(finished);
        result
    })
}

/// Delivers SIGINT to the kernel's process group so the driver raises `KeyboardInterrupt` in the
/// running cell. Returns false when no graceful signal exists (Windows kernels have no console to
/// receive one), so the caller falls back to an immediate group kill.
fn deliver_interrupt(group: u32) -> bool {
    #[cfg(unix)]
    {
        libc::pid_t::try_from(group).is_ok_and(|pid| {
            // SAFETY: killpg only signals the group this operation spawned.
            unsafe { libc::killpg(pid, libc::SIGINT) == 0 }
        })
    }
    #[cfg(not(unix))]
    {
        let _ = group;
        false
    }
}

pub(crate) fn lock_session<'a>(
    session: &'a Mutex<Option<JsonProcess>>,
    end: Option<Instant>,
    interrupt: &AtomicBool,
    cancel: &AtomicBool,
) -> Result<parking_lot::MutexGuard<'a, Option<JsonProcess>>, String> {
    loop {
        check_operation(end, interrupt, cancel)?;
        if let Some(slot) = session.try_lock_for(Duration::from_millis(10)) {
            check_operation(end, interrupt, cancel)?;
            return Ok(slot);
        }
    }
}

fn serial_operation<'a>(
    workspace: &'a Workspace,
    end: Option<Instant>,
    interrupt: &AtomicBool,
    cancel: &AtomicBool,
) -> Result<std::sync::MutexGuard<'a, ()>, String> {
    loop {
        check_operation(end, interrupt, cancel)?;
        if let Some(guard) = workspace.try_serial_operation() {
            check_operation(end, interrupt, cancel)?;
            return Ok(guard);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Runs one controller callback with the workspace serial guard released; the guard is reacquired
/// before returning. The driver stays paused until the reply is sent.
fn dispatch_guarded<'a>(
    serial: &mut Option<std::sync::MutexGuard<'a, ()>>,
    workspace: &'a Workspace,
    end: Option<Instant>,
    interrupt: &AtomicBool,
    cancel: &AtomicBool,
    callback_wait: &AtomicBool,
    dispatch: &mut dyn FnMut(ToolCallback) -> ToolOutput,
    callback: ToolCallback,
) -> Result<ToolOutput, String> {
    struct CallbackWait<'a>(&'a AtomicBool);
    impl Drop for CallbackWait<'_> {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }
    callback_wait.store(true, Ordering::Release);
    let waiting = CallbackWait(callback_wait);
    check_operation(end, interrupt, cancel)?;
    drop(serial.take());
    let output = dispatch(callback);
    drop(waiting);
    if super::outcome::effects(&output) == super::outcome::Effects::Unknown {
        return Err("native callback ended with uncertain effects; namespace discarded".into());
    }
    check_operation(end, interrupt, cancel)?;
    *serial = Some(serial_operation(workspace, end, interrupt, cancel)?);
    Ok(output)
}

fn native_cell_once(
    process: &mut JsonProcess,
    workspace: &Workspace,
    code: &str,
    end: Option<Instant>,
    interrupt: &AtomicBool,
    cancel: &AtomicBool,
    callback_wait: &AtomicBool,
    dispatch: &mut dyn FnMut(ToolCallback) -> ToolOutput,
) -> Result<Value, String> {
    // Native code may write directly. Never run it concurrently with another workspace transaction.
    // The outer agent MUST NOT hold its own serial guard for supports_tool_dispatch() tools.
    let mut serial = Some(serial_operation(workspace, end, interrupt, cancel)?);
    process.request_guarded(
        json!({"code":code}),
        end,
        || check_operation(end, interrupt, cancel),
        128,
        |request| {
            check_operation(end, interrupt, cancel)?;
            let mut call = |callback: ToolCallback| {
                dispatch_guarded(
                    &mut serial,
                    workspace,
                    end,
                    interrupt,
                    cancel,
                    callback_wait,
                    dispatch,
                    callback,
                )
            };
            if let Some(reply) = preludes::bridge_reply(&request, &mut call) {
                return reply;
            }
            let callback = match request.get("callback").and_then(Value::as_str) {
                Some("schemas") => ToolCallback::Schemas,
                Some("tool") => {
                    let name = request
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty() && name.len() <= 256)
                        .ok_or("malformed tool callback name")?;
                    let arguments = request
                        .get("arguments")
                        .filter(|arguments| arguments.is_object())
                        .ok_or("tool callback arguments must be an object")?;
                    ToolCallback::Call {
                        name: name.to_owned(),
                        arguments: arguments.clone(),
                    }
                }
                _ => return Err("unsupported code-mode callback".into()),
            };
            // Controller callbacks execute their own Workspace transaction, approvals and observers.
            let output = dispatch_guarded(
                &mut serial,
                workspace,
                end,
                interrupt,
                cancel,
                callback_wait,
                dispatch,
                callback,
            )?;
            Ok(json!({"text":output.text,"data":output.data,"is_error":output.is_error}))
        },
    )
}

/// Runs one cell after installing eval preludes when the registry changed; `__dial_display__` text
/// is prepended to the cell's stdout.
pub(crate) fn native_cell(
    process: &mut JsonProcess,
    workspace: &Workspace,
    code: &str,
    end: Option<Instant>,
    interrupt: &AtomicBool,
    cancel: &AtomicBool,
    callback_wait: &AtomicBool,
    dispatch: &mut dyn FnMut(ToolCallback) -> ToolOutput,
) -> Result<Value, String> {
    if let Some(language) = process.prelude_language {
        let generation = preludes::generation();
        if process.prelude_generation != generation {
            let source = preludes::prelude_source_for(language);
            if !source.is_empty() {
                let installed = native_cell_once(
                    process,
                    workspace,
                    &source,
                    end,
                    interrupt,
                    cancel,
                    callback_wait,
                    dispatch,
                )?;
                if installed.get("is_error").and_then(Value::as_bool) == Some(true) {
                    return Err(format!("installing eval preludes failed: {installed}"));
                }
            }
            process.prelude_generation = generation;
        }
    }
    preludes::take_display();
    let mut value = native_cell_once(
        process,
        workspace,
        code,
        end,
        interrupt,
        cancel,
        callback_wait,
        dispatch,
    )?;
    let displayed = preludes::take_display();
    if !displayed.is_empty() {
        let stdout = value
            .get("stdout")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let combined = format!("{displayed}{stdout}");
        if let Some(object) = value.as_object_mut() {
            object.insert("stdout".into(), Value::String(combined));
        }
    }
    Ok(value)
}

#[cfg(windows)]
type KernelStdin = crate::platform::io::QueuedWriter<ChildStdin>;
#[cfg(unix)]
type KernelStdin = ChildStdin;
/// Bounded JSON-lines protocol. Pipes are nonblocking so neither output nor input can defeat deadlines.
pub(crate) struct JsonProcess {
    group: ProcessGroup,
    input: KernelStdin,
    output: ChildStdout,
    errors: ChildStderr,
    pending: Vec<u8>,
    stderr: Vec<u8>,
    stderr_dropped: usize,
    pid: Arc<AtomicI32>,
    sequence: u64,
    callback_sequence: u64,
    /// A request frame was fully written and its reply has not been read yet.
    in_flight: bool,
    /// Language whose eval preludes are installed into this process; `None` disables installation.
    pub(crate) prelude_language: Option<preludes::PreludeLanguage>,
    /// Registry generation last installed into this process.
    prelude_generation: u64,
}
impl JsonProcess {
    pub(crate) fn start(mut command: Command, pid: Arc<AtomicI32>) -> Result<Self, String> {
        let child = crate::platform::process::spawn_group(
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        )
        .map_err(|e| e.to_string())?;
        // Owned from spawn onward: any later error terminates and reaps the group.
        let mut group = ProcessGroup::new(child);
        let input = group.child.stdin.take().ok_or("child stdin unavailable")?;
        let output = group
            .child
            .stdout
            .take()
            .ok_or("child stdout unavailable")?;
        let errors = group
            .child
            .stderr
            .take()
            .ok_or("child stderr unavailable")?;
        crate::platform::io::set_nonblocking(&input).map_err(|e| e.to_string())?;
        crate::platform::io::set_nonblocking(&output).map_err(|e| e.to_string())?;
        crate::platform::io::set_nonblocking(&errors).map_err(|e| e.to_string())?;
        #[cfg(windows)]
        let input = crate::platform::io::QueuedWriter::new(input, 64 * 1024);
        pid.store(group.child.id() as i32, Ordering::Release);
        Ok(Self {
            group,
            input,
            output,
            errors,
            pending: Vec::new(),
            stderr: Vec::new(),
            stderr_dropped: 0,
            pid,
            sequence: 0,
            callback_sequence: 0,
            in_flight: false,
            prelude_language: None,
            prelude_generation: 0,
        })
    }
    pub(crate) fn alive(&mut self) -> bool {
        self.group.exited().is_ok_and(|exited| !exited)
    }
    fn write_frame(
        &mut self,
        value: &Value,
        end: Option<Instant>,
        check: &impl Fn() -> Result<(), String>,
    ) -> Result<(), String> {
        let mut bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
        if bytes.len() > MAX_FRAME {
            return Err("request frame too large".into());
        }
        bytes.push(b'\n');
        let mut offset = 0;
        while offset < bytes.len() {
            check()?;
            self.drain_stderr();
            if end.is_some_and(|end| Instant::now() >= end) {
                return Err("execution deadline exceeded; namespace/session discarded".into());
            }
            match self.write_input(&bytes[offset..]) {
                Ok(0) => return Err("child stdin closed".into()),
                Ok(n) => offset += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => self.poll_input(),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        loop {
            check()?;
            self.drain_stderr();
            if end.is_some_and(|end| Instant::now() >= end) {
                return Err("execution deadline exceeded; namespace/session discarded".into());
            }
            if self.input_drained().map_err(|error| error.to_string())? {
                break;
            }
        }
        Ok(())
    }
    fn write_input(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        #[cfg(windows)]
        {
            self.input.try_write(bytes)
        }
        #[cfg(unix)]
        {
            self.input.write(bytes)
        }
    }
    fn input_drained(&self) -> std::io::Result<bool> {
        #[cfg(windows)]
        {
            self.input.wait_until_drained(Duration::from_millis(10))
        }
        #[cfg(unix)]
        {
            Ok(true)
        }
    }
    fn poll_input(&self) {
        #[cfg(windows)]
        {
            let _ = self.input.wait_until_drained(Duration::from_millis(10));
        }
        #[cfg(unix)]
        {
            let _ = crate::platform::io::wait_writable(&self.input, Duration::from_millis(10));
        }
    }
    fn poll_output(&self) {
        let _ = crate::platform::io::wait_readable(&self.output, Duration::from_millis(10));
    }
    fn drain_stderr(&mut self) {
        let mut buffer = [0u8; 16384];
        for _ in 0..8 {
            match crate::platform::io::read_nonblocking(&mut self.errors, &mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let remaining = (16 * 1024usize).saturating_sub(self.stderr.len());
                    self.stderr.extend_from_slice(&buffer[..n.min(remaining)]);
                    self.stderr_dropped = self
                        .stderr_dropped
                        .saturating_add(n.saturating_sub(remaining));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    }
    pub(crate) fn request(
        &mut self,
        value: Value,
        timeout: Duration,
        callback: impl FnMut(&Value) -> Result<Value, String>,
    ) -> Result<Value, String> {
        self.request_guarded(
            value,
            execution_deadline(timeout),
            || Ok(()),
            usize::MAX,
            callback,
        )
    }
    /// Interrupt-aware request: a tripped check (cancel, deadline, interrupt) first gives the driver
    /// [`INTERRUPT_GRACE`] to unwind the cell after SIGINT, then the caller discards the session.
    pub(crate) fn request_guarded(
        &mut self,
        value: Value,
        end: Option<Instant>,
        check: impl Fn() -> Result<(), String>,
        max_callbacks: usize,
        callback: impl FnMut(&Value) -> Result<Value, String>,
    ) -> Result<Value, String> {
        let tripped = std::cell::Cell::new(false);
        let watched = || check().inspect_err(|_| tripped.set(true));
        let result = self.exchange(value, end, &watched, max_callbacks, callback);
        if tripped.get() && self.in_flight {
            self.settle_interrupted();
        }
        self.in_flight = false;
        result
    }
    fn settle_interrupted(&mut self) {
        let deadline = Instant::now() + INTERRUPT_GRACE;
        let mut buffer = [0u8; 16384];
        while !self.pending.contains(&b'\n') && Instant::now() < deadline && self.alive() {
            self.drain_stderr();
            match crate::platform::io::read_nonblocking(&mut self.output, &mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if self.pending.len() + n > MAX_FRAME {
                        break;
                    }
                    self.pending.extend_from_slice(&buffer[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => self.poll_output(),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    }
    fn exchange(
        &mut self,
        mut value: Value,
        end: Option<Instant>,
        check: &impl Fn() -> Result<(), String>,
        max_callbacks: usize,
        mut callback: impl FnMut(&Value) -> Result<Value, String>,
    ) -> Result<Value, String> {
        check()?;
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or("operation sequence exhausted")?;
        value["id"] = json!(self.sequence);
        self.write_frame(&value, end, &check)?;
        self.in_flight = true;
        let mut callbacks = 0usize;
        loop {
            check()?;
            self.drain_stderr();
            let mut buffer = [0u8; 16384];
            if end.is_some_and(|end| Instant::now() >= end) {
                return Err("execution deadline exceeded; namespace/session discarded".into());
            }
            if let Some(index) = self.pending.iter().position(|b| *b == b'\n') {
                let line = self.pending.drain(..=index).collect::<Vec<_>>();
                let reply: Value = serde_json::from_slice(&line)
                    .map_err(|e| format!("invalid adapter response: {e}"))?;
                if reply.get("callback").is_some() {
                    callbacks += 1;
                    if callbacks > max_callbacks {
                        return Err("operation callback limit exceeded".into());
                    }
                    let operation = reply.get("operation_id").and_then(Value::as_u64);
                    let identity = reply.get("callback_id").and_then(Value::as_u64);
                    let outcome = if operation != Some(self.sequence) {
                        Err("callback belongs to an expired operation".into())
                    } else if identity.is_none_or(|id| id <= self.callback_sequence) {
                        Err("callback identity is malformed or repeated".into())
                    } else {
                        self.callback_sequence = identity.unwrap();
                        check()?;
                        callback(&reply)
                    };
                    // A deadline/cancel while approval or a nested tool was pending must never resume code.
                    check()?;
                    if end.is_some_and(|end| Instant::now() >= end) {
                        return Err(
                            "execution deadline exceeded; namespace/session discarded".into()
                        );
                    }
                    let response = match outcome {
                        Ok(value) => {
                            json!({"callback_id":reply["callback_id"],"operation_id":self.sequence,"result":value})
                        }
                        Err(error) => {
                            json!({"callback_id":reply["callback_id"],"operation_id":self.sequence,"error":error})
                        }
                    };
                    self.write_frame(&response, end, &check)?;
                    continue;
                }
                if reply.get("id").and_then(Value::as_u64) != Some(self.sequence) {
                    return Err("adapter response sequence mismatch".into());
                }
                if let Some(error) = reply.get("error").and_then(Value::as_str) {
                    return Err(error.into());
                }
                let mut result = reply.get("result").cloned().unwrap_or(Value::Null);
                if !self.stderr.is_empty() {
                    if let Some(object) = result.as_object_mut() {
                        object.insert(
                            "process_output".into(),
                            json!(String::from_utf8_lossy(&self.stderr)),
                        );
                        if self.stderr_dropped > 0 {
                            object.insert(
                                "process_output_truncated_bytes".into(),
                                json!(self.stderr_dropped),
                            );
                        }
                    }
                    self.stderr.clear();
                    self.stderr_dropped = 0;
                }
                check()?;
                if end.is_some_and(|end| Instant::now() >= end) {
                    return Err("execution deadline exceeded; namespace/session discarded".into());
                }
                return Ok(result);
            }
            match crate::platform::io::read_nonblocking(&mut self.output, &mut buffer) {
                Ok(0) => {
                    return Err(format!(
                        "execution process exited: {}",
                        String::from_utf8_lossy(&self.stderr)
                    ));
                }
                Ok(n) => {
                    if self.pending.len() + n > MAX_FRAME {
                        return Err("adapter output exceeded frame limit".into());
                    }
                    self.pending.extend_from_slice(&buffer[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => self.poll_output(),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.to_string()),
            }
        }
    }
}
impl Drop for JsonProcess {
    fn drop(&mut self) {
        self.group.terminate();
        self.pid.store(0, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quote_preserves_shell_metacharacters() {
        assert_eq!(shell_quote("a'b;$(x)"), "'a'\\''b;$(x)'");
    }
    #[test]
    fn deadline_is_operator_bounded() {
        assert_eq!(
            deadline(&json!({"timeout":99}), 5).unwrap(),
            Duration::from_secs(5)
        );
        assert_eq!(
            deadline(&json!({"timeout":0}), 5).unwrap(),
            Duration::from_secs(5)
        );
        assert_eq!(deadline(&json!({"timeout":0}), 0).unwrap(), Duration::ZERO);
        assert_eq!(
            deadline(&json!({"timeout":2}), 0).unwrap(),
            Duration::from_secs(2)
        );
    }
    #[test]
    fn zero_timeout_waits_for_a_delayed_response() {
        let mut p = process();
        let result = p
            .request(
                json!({"code":"import time; time.sleep(0.1); print('done')"}),
                Duration::ZERO,
                |_| unreachable!(),
            )
            .unwrap();
        assert_eq!(result["stdout"], "done\n");
    }
    fn process() -> JsonProcess {
        let mut command = Command::new("python3");
        command.args(["-u", "-c", PYTHON_DRIVER]);
        JsonProcess::start(command, Arc::new(AtomicI32::new(0))).unwrap()
    }
    #[test]
    fn python_state_stdout_tracebacks_and_reset() {
        let mut p = process();
        let call = |p: &mut JsonProcess, code: &str| {
            p.request(
                json!({"code":code}),
                Duration::from_secs(5),
                |_| unreachable!(),
            )
            .unwrap()
        };
        assert_eq!(call(&mut p, "x = 40\nprint('hello')")["stdout"], "hello\n");
        assert_eq!(call(&mut p, "x + 2")["value"], "42");
        assert!(
            call(&mut p, "1 / 0")["error"]
                .as_str()
                .unwrap()
                .contains("ZeroDivisionError")
        );
        assert_eq!(call(&mut p, "x")["value"], "40");
        drop(p);
        let mut fresh = process();
        assert!(
            call(&mut fresh, "x")["error"]
                .as_str()
                .unwrap()
                .contains("NameError")
        );
    }
    #[test]
    fn endless_python_has_a_deadline() {
        let mut p = process();
        let result = p.request(
            json!({"code":"while True: pass"}),
            Duration::from_millis(100),
            |_| unreachable!(),
        );
        assert!(result.unwrap_err().contains("deadline"));
    }
    #[test]
    fn venv_site_packages_passed_as_argv_are_importable() {
        let site = std::env::temp_dir().join(format!("dial-kernel-site-{}", std::process::id()));
        std::fs::create_dir_all(site.join("dialfixture")).unwrap();
        std::fs::write(site.join("dialfixture").join("__init__.py"), "VALUE = 41\n").unwrap();
        let mut command = Command::new("python3");
        command.args(["-I", "-u", "-c", PYTHON_DRIVER]).arg(&site);
        let mut p = JsonProcess::start(command, Arc::new(AtomicI32::new(0))).unwrap();
        let result = p
            .request(
                json!({"code":"import dialfixture\ndialfixture.VALUE + 1"}),
                Duration::from_secs(5),
                |_| unreachable!(),
            )
            .unwrap();
        drop(p);
        let _ = std::fs::remove_dir_all(&site);
        assert_eq!(result["value"], "42");
    }
    #[cfg(unix)]
    #[test]
    fn idle_sigint_keeps_the_namespace_and_the_session() {
        let pid = Arc::new(AtomicI32::new(0));
        let mut p = JsonProcess::start(
            {
                let mut command = Command::new("python3");
                command.args(["-u", "-c", PYTHON_DRIVER]);
                command
            },
            pid.clone(),
        )
        .unwrap();
        let call = |p: &mut JsonProcess, code: &str| {
            p.request(
                json!({"code":code}),
                Duration::from_secs(5),
                |_| unreachable!(),
            )
            .unwrap()
        };
        call(&mut p, "kept = 7");
        std::thread::sleep(Duration::from_millis(100));
        // SAFETY: the pid belongs to the child this test started and has not been reaped.
        unsafe { libc::kill(pid.load(Ordering::Acquire), libc::SIGINT) };
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(call(&mut p, "kept")["value"], "7");
    }
    #[test]
    fn noisy_output_is_bounded() {
        let mut p = process();
        let v = p
            .request(
                json!({"code":"print('x' * 100000)"}),
                Duration::from_secs(5),
                |_| unreachable!(),
            )
            .unwrap();
        assert!(v["stdout"].as_str().unwrap().len() < 25000);
        assert!(v["stdout"].as_str().unwrap().contains("truncated"));
    }
    #[test]
    fn native_output_is_captured_without_protocol_corruption() {
        let mut p = process();
        let v = p
            .request(
                json!({"code":"import os\nos.write(1, b'native-output')"}),
                Duration::from_secs(5),
                |_| unreachable!(),
            )
            .unwrap();
        assert!(
            v["process_output"]
                .as_str()
                .unwrap()
                .contains("native-output")
        );
    }
    #[cfg(unix)]
    #[test]
    fn drop_kills_and_reaps_the_process_group_leader() {
        let p = process();
        let pid = p.group.child.id() as libc::pid_t;
        drop(p);
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    #[test]
    fn unavailable_sandbox_never_runs_model_code() {
        use super::super::{Workspace, sandbox::Sandbox};
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path())
            .unwrap()
            .with_sandbox(Sandbox::Unavailable("test-denied".into()));
        let config = Config::default();
        let ctx = ToolContext {
            workspace: &workspace,
            config: &config,
            mutation_authorizer: None,
            confirm: None,
        };
        let tool = tools(&config).pop().unwrap();
        let out = tool.call(
            &ctx,
            &json!({"action":"eval","code":"open('escaped', 'w').write('no')"}),
        );
        assert!(
            out.is_error && out.text.contains("test-denied"),
            "{}",
            out.text
        );
        assert!(!dir.path().join("escaped").exists());
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn cancellation_interrupts_an_inflight_kernel_without_session_lock() {
        let dir = tempfile::tempdir().unwrap();
        // The transport/control test opts out explicitly; confinement has a separate fail-closed test.
        let workspace = super::super::Workspace::new(dir.path())
            .unwrap()
            .without_sandbox();
        let config = Config::default();
        let tool = Arc::new(KernelTool {
            config: KernelConfig::default(),
            session: Mutex::new(None),
            pid: Arc::new(AtomicI32::new(0)),
            interrupt: AtomicBool::new(false),
            active: AtomicUsize::new(0),
        });
        let running = tool.clone();
        let thread = std::thread::spawn(move || {
            running.call(
                &ToolContext {
                    workspace: &workspace,
                    config: &config,
                    mutation_authorizer: None,
                    confirm: None,
                },
                &json!({"action":"eval","code":"while True: pass"}),
            )
        });
        let end = Instant::now() + Duration::from_secs(5);
        while tool.pid.load(Ordering::Acquire) == 0 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(tool.pid.load(Ordering::Acquire) > 0);
        tool.cancel_operation();
        assert!(thread.join().unwrap().is_error);
        assert_eq!(tool.pid.load(Ordering::Acquire), 0);
        assert!(tool.session.lock().is_none());
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn kernel_denies_fork_spawn_and_detached_descendants_even_without_filesystem_sandbox() {
        if !crate::tools::sandbox::require_for_test() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let workspace = super::super::Workspace::new(dir.path())
            .unwrap()
            .without_sandbox();
        let config = Config::default();
        let tool = tools(&config).pop().unwrap();
        let ctx = ToolContext {
            workspace: &workspace,
            config: &config,
            mutation_authorizer: None,
            confirm: None,
        };
        for code in [
            "import subprocess, sys\np = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'], start_new_session=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)\np.pid",
            "import os, sys\nos.posix_spawn(sys.executable, [sys.executable, '-c', 'import time; time.sleep(60)'], os.environ)",
            "import os\nos.fork()",
        ] {
            let output = tool.call(&ctx, &json!({"action":"eval","code":code}));
            // Clean up a spawned PID even if containment regresses, before failing the test.
            if !output.is_error {
                let value: Value = serde_json::from_str(&output.text).unwrap();
                if let Some(pid) = value["value"]
                    .as_str()
                    .and_then(|s| s.parse::<i32>().ok())
                    .filter(|pid| *pid > 0)
                {
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
            }
            assert!(
                output.is_error && output.text.contains("PermissionError"),
                "fork containment failed: {}",
                output.text
            );
        }
        assert!(!tool.call(&ctx, &json!({"action":"reset"})).is_error);
    }
    #[test]
    fn expired_callback_operation_never_invokes_the_network_handler() {
        let driver = "import sys,json\nr=json.loads(sys.stdin.readline())\nprint(json.dumps({'callback':'http','callback_id':1,'operation_id':0}),flush=True)\nreply=json.loads(sys.stdin.readline())\nprint(json.dumps({'id':r['id'],'result':reply}),flush=True)";
        let mut command = Command::new("python3");
        command.args(["-u", "-c", driver]);
        let mut process = JsonProcess::start(command, Arc::new(AtomicI32::new(0))).unwrap();
        let value = process
            .request(json!({}), Duration::from_secs(5), |_| {
                panic!("expired callback performed networking")
            })
            .unwrap();
        assert!(
            value["error"]
                .as_str()
                .unwrap()
                .contains("expired operation")
        );
    }
    #[cfg(unix)]
    #[test]
    fn workspace_python_launcher_is_rejected_before_execution() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let marker = outside.path().join("escaped");
        let launcher = dir.path().join("python3");
        std::fs::write(
            &launcher,
            format!(
                "#!/bin/sh\nprintf escaped > {}\n",
                shell_quote(marker.to_str().unwrap())
            ),
        )
        .unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
        let root = dir.path().canonicalize().unwrap();
        let error = resolve_python(
            &root,
            launcher.to_str().unwrap(),
            Duration::from_secs(5),
            Arc::new(AtomicI32::new(0)),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(error.contains("outside the editable workspace"));
        assert!(!marker.exists());
    }
}

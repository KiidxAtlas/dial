//! Persistent Python execution with controller-owned tool callbacks and OS process containment.
//!
//! Cancellation and timeouts first deliver SIGINT, which the driver turns into `KeyboardInterrupt`
//! inside the running cell. The interrupted namespace is always discarded; a driver that does
//! not unwind within [`INTERRUPT_GRACE`] is killed together with its session.
use crate::config::tools::kernel::KernelConfig;

use super::background::{AdoptedJob, CellJob};
#[cfg(windows)]
use super::sandbox::Sandbox;
#[cfg(not(windows))]
use super::shell::shell_command;
use crate::tools::integrations::ext::{ToolCallback, ToolContext};
use crate::tools::{ToolOutput, Workspace, exec::preludes};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

mod env;

pub(crate) const PYTHON_DRIVER: &str = include_str!("kernel/driver.py");
pub(crate) const MAX_CODE: usize = 64 * 1024;
pub(crate) use crate::runtime::json_transport::{INTERRUPT_GRACE, execution_deadline};
// No descendant can detach: even with the operator's filesystem sandbox opt-out, fork/spawn is denied.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) const FORK_FENCE: &str = "(version 1)(allow default)(deny process-fork)";
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const BWRAP: &str = "/usr/bin/bwrap";
const PYTHON_PREFLIGHT: &str = "import sys,json\nr=json.loads(sys.stdin.readline())\nprint(json.dumps({'id':r['id'],'result':{'executable':sys.executable,'version':list(sys.version_info[:2])}}),flush=True)";

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
/// Owner-local Python runtime: one persistent namespace session plus the cancellation and deadline
/// state shared with the in-flight cell.
pub(crate) struct PythonRuntime {
    config: KernelConfig,
    session: Arc<Mutex<Option<NativeProcess>>>,
    busy: Arc<Mutex<Option<String>>>,
    pid: Arc<AtomicI32>,
    interrupt: AtomicBool,
    active: AtomicUsize,
}
impl PythonRuntime {
    pub(super) fn new(config: KernelConfig) -> Self {
        Self {
            config,
            session: Arc::new(Mutex::new(None)),
            busy: Arc::new(Mutex::new(None)),
            pid: Arc::new(AtomicI32::new(0)),
            interrupt: AtomicBool::new(false),
            active: AtomicUsize::new(0),
        }
    }
    pub(super) fn fork(&self) -> Self {
        Self::new(self.config.clone())
    }
    /// Requests cancellation of the in-flight cell; a flag left behind would reject the next cell.
    pub(super) fn cancel(&self) -> bool {
        let requested = self.active.load(Ordering::Acquire) > 0;
        if requested {
            self.interrupt.store(true, Ordering::Release);
        }
        requested
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run(
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
        // A pending execution cancellation must never admit another cell. Explicit control
        // operations may still inspect/reset the now-dead namespace.
        let pending_cancel = self.interrupt.swap(false, Ordering::AcqRel);
        if cancel.load(Ordering::Acquire) || (pending_cancel && action == "eval") {
            return Err("python execution cancelled before startup".into());
        }
        let policy = BackgroundPolicy::from_args(args, ctx.config.shell.auto_background_secs)?;
        let handoff = Handoff {
            label: cell_label(args, "py")?,
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
                        return Ok(CellOutcome::Value(json!({"running":true,"background":job,
                            "language":"py",
                            "platform":crate::tools::surface::catalog::execution_platform("eval")})));
                    }
                    return Err(format!("runtime busy in background job {job}"));
                }
                if action == "reset" {
                    slot.take();
                    return Ok(CellOutcome::Value(json!({"reset":true})));
                }
                if action == "status" {
                    return Ok(CellOutcome::Value(json!({
                        "running":slot.as_mut().is_some_and(NativeProcess::alive),
                        "language":"py",
                        "platform":crate::tools::surface::catalog::execution_platform("eval")})));
                }
                require_fork_containment("eval")?;
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
                        let timeout = remaining_timeout(end.at())?;
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
                        require_fork_containment("eval")?;
                        check_operation(end, &self.interrupt, cancel)?;
                        *slot = Some(NativeProcess::start(command, self.pid.clone()).map_err(
                            |e| {
                                format!(
                                    "starting Python kernel (install/configure kernel.python): {e}"
                                )
                            },
                        )?);
                    }
                    check_operation(end, &self.interrupt, cancel)?;
                    slot.as_mut().unwrap().prelude_language =
                        Some(preludes::PreludeLanguage::Python);
                    effect_started.store(true, Ordering::Release);
                    let scope = CellScope {
                        workspace: ctx.workspace,
                        end,
                        interrupt: &self.interrupt,
                        cancel,
                        callback_wait: &callback_wait,
                    };
                    native_cell(slot.as_mut().unwrap(), &scope, code, dispatch, &handoff)
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
    let mut process = NativeProcess::start(command, pid)?;
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
    use crate::runtime::execution::{Completeness, ObservationKind, OutputIntegrity};
    match result {
        Ok(data) => {
            let original = data["output_integrity"]["original_bytes"].as_u64();
            let retained = data["output_integrity"]["retained_bytes"].as_u64();
            let omitted = original
                .zip(retained)
                .map(|(original, retained)| original.saturating_sub(retained));
            // A protocol reply completes observation, not the persistent interpreter's lifetime.
            // Native writes and bounded value/traceback formatting prevent a lossless capture claim.
            let integrity = OutputIntegrity {
                capture: Completeness::Unknown,
                retention: if omitted.is_some_and(|n| n > 0) {
                    Completeness::Partial
                } else {
                    Completeness::Unknown
                },
                presentation: Completeness::Unknown,
                original_bytes: original,
                retained_bytes: retained,
                omitted_bytes: omitted,
                ..Default::default()
            };
            ToolOutput {
                execution: Default::default(),
                is_error: data.get("error").is_some_and(|error| !error.is_null()),
                text: data.to_string(),
                data: Some(data),
            }
            .with_output_integrity(integrity)
            .with_observation(ObservationKind::Wait, true)
        }
        Err(error) => ToolOutput::err(error)
            .with_output_integrity(OutputIntegrity::default())
            .with_observation(ObservationKind::Wait, false),
    }
}

pub(crate) fn require_fork_containment(label: &str) -> Result<(), String> {
    match crate::tools::surface::catalog::execution_platform(label).reason {
        Some(reason) => Err(reason),
        None => Ok(()),
    }
}

/// Cell deadline with host-callback time excluded. A controller callback runs on the caller's
/// clock, so its duration is credited back rather than charged to the cell.
pub(crate) struct Deadline {
    end: Option<Instant>,
    paused: AtomicU64,
}

impl Deadline {
    pub(crate) fn new(end: Option<Instant>) -> Self {
        Self {
            end,
            paused: AtomicU64::new(0),
        }
    }
    /// The effective instant, extended by every completed callback.
    pub(crate) fn at(&self) -> Option<Instant> {
        let paused = Duration::from_nanos(self.paused.load(Ordering::Acquire));
        self.end.map(|end| end + paused)
    }
    pub(crate) fn expired(&self) -> bool {
        self.at().is_some_and(|end| Instant::now() >= end)
    }
    pub(crate) fn pause(&self, elapsed: Duration) {
        let nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        self.paused.fetch_add(nanos, Ordering::AcqRel);
    }
}

pub(crate) fn check_operation(
    end: &Deadline,
    interrupt: &AtomicBool,
    cancel: &AtomicBool,
) -> Result<(), String> {
    if end.expired() {
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
    end: &Deadline,
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
                let expired = !callback_wait.load(Ordering::Acquire) && end.expired();
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
    session: &'a Mutex<Option<NativeProcess>>,
    end: &Deadline,
    interrupt: &AtomicBool,
    cancel: &AtomicBool,
) -> Result<parking_lot::MutexGuard<'a, Option<NativeProcess>>, String> {
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
    end: &Deadline,
    interrupt: &AtomicBool,
    cancel: &AtomicBool,
) -> Result<std::sync::MutexGuard<'a, ()>, String> {
    workspace.serial_operation_checked(|| check_operation(end, interrupt, cancel))
}

/// Runs one controller callback with the workspace serial guard released; the guard is reacquired
/// before returning. The driver stays paused until the reply is sent.
fn dispatch_guarded<'a>(
    serial: &mut Option<std::sync::MutexGuard<'a, ()>>,
    scope: &CellScope<'a>,
    dispatch: &mut dyn FnMut(ToolCallback) -> ToolOutput,
    callback: ToolCallback,
) -> Result<ToolOutput, String> {
    let &CellScope {
        workspace,
        end,
        interrupt,
        cancel,
        callback_wait,
    } = scope;
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
    let started = Instant::now();
    let output = dispatch(callback);
    end.pause(started.elapsed());
    drop(waiting);
    if crate::tools::surface::outcome::effects(&output)
        == crate::tools::surface::outcome::Effects::Unknown
    {
        return Err("native callback ended with uncertain effects; namespace discarded".into());
    }
    check_operation(end, interrupt, cancel)?;
    *serial = Some(serial_operation(workspace, end, interrupt, cancel)?);
    Ok(output)
}

/// Background mode from the eval `background` argument: absent = auto, `false` = never, `true` = immediate.
pub(crate) struct BackgroundPolicy {
    mode: Option<bool>,
    threshold: Duration,
    started: Instant,
}

impl BackgroundPolicy {
    pub(crate) fn from_args(args: &Value, threshold_secs: u64) -> Result<Self, String> {
        let mode = match args.get("background") {
            None | Some(Value::Null) => None,
            Some(Value::Bool(flag)) => Some(*flag),
            Some(_) => return Err("background must be a boolean".into()),
        };
        Ok(Self {
            mode,
            threshold: Duration::from_secs(threshold_secs),
            started: Instant::now(),
        })
    }

    /// Whether a cell that is waiting for output should move to the background now.
    pub(crate) fn should_yield(&self) -> bool {
        match self.mode {
            Some(false) => false,
            Some(true) => true,
            None => self.started.elapsed() >= self.threshold,
        }
    }
}

/// Background owner for a cell: the job label, the runtime state the job keeps, and the adopter.
pub(crate) struct Handoff<'a> {
    pub label: String,
    pub policy: &'a BackgroundPolicy,
    pub adopt: &'a dyn Fn(AdoptedJob) -> Result<String, String>,
    pub session: Arc<Mutex<Option<NativeProcess>>>,
    pub busy: Arc<Mutex<Option<String>>>,
    pub pid: Arc<AtomicI32>,
}

impl Handoff<'_> {
    /// Adopts the yielded cell as a managed background job and marks the runtime busy under its id.
    /// The caller still holds the session slot, so the job cannot take the kernel before this returns.
    fn move_to_background(
        &self,
        end: &Arc<Deadline>,
        display: Arc<preludes::DisplayBuffer>,
    ) -> Result<String, String> {
        let (end, session, busy, pid) = (
            end.clone(),
            self.session.clone(),
            self.busy.clone(),
            self.pid.clone(),
        );
        let job = CellJob {
            command: self.label.clone(),
            network: false,
            work: Box::new(move |cancel: Arc<AtomicBool>| {
                detached_cell(&session, &pid, &busy, &end, &cancel, &display)
            }),
        };
        let name = (self.adopt)(AdoptedJob::Cell(job))?;
        *self.busy.lock() = Some(name.clone());
        Ok(name)
    }
}

/// Borrowed state a foreground cell step needs.
pub(crate) struct CellScope<'a> {
    pub workspace: &'a Workspace,
    pub end: &'a Arc<Deadline>,
    pub interrupt: &'a AtomicBool,
    pub cancel: &'a AtomicBool,
    pub callback_wait: &'a AtomicBool,
}

/// Result of one foreground cell: its value, or the job id after the cell moved to the background.
pub(crate) enum CellOutcome {
    Value(Value),
    Moved(String),
}

/// Host-tool refusal for callbacks issued by a cell that already runs in the background.
pub(crate) const MOVED_CALLBACK_REFUSAL: &str = "this cell was moved to the background; host tool calls are unavailable there — rerun with background:false to keep it in the foreground";

/// Job id of the background owner, if the runtime is busy.
pub(crate) fn busy_job(busy: &Mutex<Option<String>>) -> Option<String> {
    busy.lock().clone()
}

/// Label of a backgrounded cell: the optional `title` argument, else the runtime name.
pub(crate) fn cell_label(args: &Value, language: &str) -> Result<String, String> {
    match args.get("title") {
        None | Some(Value::Null) => Ok(format!("eval cell ({language})")),
        Some(Value::String(title)) if !title.trim().is_empty() && title.len() <= 256 => {
            Ok(title.clone())
        }
        Some(_) => Err("title must be a non-empty string of at most 256 bytes".into()),
    }
}

/// One protocol step: sends `code` (or resumes the in-flight cell when `None`). `Ok(None)` means the cell
/// yielded to `yield_now` and is still in flight.
fn native_cell_once(
    process: &mut NativeProcess,
    scope: &CellScope<'_>,
    code: Option<&str>,
    display: &preludes::DisplayBuffer,
    dispatch: &mut dyn FnMut(ToolCallback) -> ToolOutput,
    yield_now: &dyn Fn() -> bool,
) -> Result<Option<Value>, String> {
    let end: &Deadline = scope.end;
    let (workspace, interrupt, cancel) = (scope.workspace, scope.interrupt, scope.cancel);
    // Native code may write directly. Never run it concurrently with another workspace transaction.
    // The outer agent MUST NOT hold its own serial guard for supports_tool_dispatch() tools.
    let mut serial = Some(serial_operation(workspace, end, interrupt, cancel)?);
    let check = || check_operation(end, interrupt, cancel);
    let callback = |request: &Value| {
        check_operation(end, interrupt, cancel)?;
        let mut call =
            |callback: ToolCallback| dispatch_guarded(&mut serial, scope, dispatch, callback);
        if let Some(reply) = preludes::bridge_reply(request, display, &mut call) {
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
        let output = dispatch_guarded(&mut serial, scope, dispatch, callback)?;
        Ok(json!({"text":output.text,"data":output.data,"is_error":output.is_error}))
    };
    match code {
        Some(code) => {
            process.request_yieldable(json!({"code":code}), None, check, yield_now, 128, callback)
        }
        None => process.resume_yieldable(None, check, yield_now, 128, callback),
    }
}

/// Runs one cell in the foreground after installing eval preludes when the registry changed; `__dial_display__`
/// text is prepended to the cell's stdout. A cell that `handoff.policy` asks to move is adopted as a background
/// job; if adoption is refused, the cell resumes in the foreground and finishes there.
pub(crate) fn native_cell(
    process: &mut NativeProcess,
    scope: &CellScope<'_>,
    code: &str,
    dispatch: &mut dyn FnMut(ToolCallback) -> ToolOutput,
    handoff: &Handoff<'_>,
) -> Result<CellOutcome, String> {
    let never = || false;
    if let Some(language) = process.prelude_language {
        let generation = preludes::generation();
        if process.prelude_generation != generation {
            let source = preludes::prelude_source_for(language);
            if !source.is_empty() {
                let installed = native_cell_once(
                    process,
                    scope,
                    Some(&source),
                    &preludes::DisplayBuffer::default(),
                    dispatch,
                    &never,
                )?
                .ok_or("installing eval preludes yielded")?;
                if installed.get("is_error").and_then(Value::as_bool) == Some(true) {
                    return Err(format!("installing eval preludes failed: {installed}"));
                }
            }
            process.prelude_generation = generation;
        }
    }
    let display = Arc::new(preludes::DisplayBuffer::default());
    let yield_now = || handoff.policy.should_yield();
    let mut notice = None;
    let mut value =
        match native_cell_once(process, scope, Some(code), &display, dispatch, &yield_now)? {
            Some(value) => value,
            None => match handoff.move_to_background(scope.end, display.clone()) {
                Ok(name) => return Ok(CellOutcome::Moved(name)),
                Err(error) => {
                    notice = Some(format!(
                        "backgrounding unavailable: {error}; the cell stayed in the foreground"
                    ));
                    native_cell_once(process, scope, None, &display, dispatch, &never)?
                        .ok_or("eval cell yielded without a handoff")?
                }
            },
        };
    let mut shown = notice
        .map(|notice| format!("[{notice}]\n"))
        .unwrap_or_default();
    shown.push_str(&display.take());
    if !shown.is_empty() {
        let stdout = value
            .get("stdout")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let combined = format!("{shown}{stdout}");
        if let Some(object) = value.as_object_mut() {
            object.insert("stdout".into(), Value::String(combined));
        }
    }
    Ok(CellOutcome::Value(value))
}

/// Background body of a moved cell. Waits until the foreground releases the kernel, resumes the in-flight cell
/// without host callbacks, and returns the kernel to the runtime while it stays usable.
fn detached_cell(
    session: &Mutex<Option<NativeProcess>>,
    pid: &AtomicI32,
    busy: &Mutex<Option<String>>,
    end: &Deadline,
    cancel: &AtomicBool,
    display: &preludes::DisplayBuffer,
) -> String {
    let interrupt = AtomicBool::new(false);
    let callback_wait = AtomicBool::new(false);
    let Some(mut process) = session.lock().take() else {
        *busy.lock() = None;
        return "eval cell lost its kernel before it moved to the background".into();
    };
    let result = guarded_operation(end, pid, &interrupt, cancel, &callback_wait, || {
        process
            .resume_yieldable(
                None,
                || check_operation(end, &interrupt, cancel),
                || false,
                128,
                |request: &Value| {
                    // Display text stays with the job; every other host callback is refused.
                    preludes::bridge_reply(request, display, &mut |_: ToolCallback| {
                        Err(MOVED_CALLBACK_REFUSAL.to_owned())
                    })
                    .unwrap_or_else(|| Err(MOVED_CALLBACK_REFUSAL.to_owned()))
                },
            )?
            .ok_or_else(|| "background cell yielded again".to_owned())
    });
    let (text, keep) = match result {
        Ok(value) => (code_output(Ok(value)).text, process.alive()),
        Err(error) => (code_output(Err(error)).text, false),
    };
    if keep {
        *session.lock() = Some(process);
    }
    *busy.lock() = None;
    format!("{}{text}", display.take())
}

/// Tool-owned prelude state wraps the neutral transport; browser processes do not install preludes.
pub(crate) struct NativeProcess {
    transport: crate::runtime::json_transport::JsonProcess,
    pub(crate) prelude_language: Option<preludes::PreludeLanguage>,
    prelude_generation: u64,
}
impl NativeProcess {
    pub(crate) fn start(command: Command, pid: Arc<AtomicI32>) -> Result<Self, String> {
        Ok(Self {
            transport: crate::runtime::json_transport::JsonProcess::start(command, pid)?,
            prelude_language: None,
            prelude_generation: 0,
        })
    }
    pub(crate) fn alive(&mut self) -> bool {
        self.transport.alive()
    }
}
impl std::ops::Deref for NativeProcess {
    type Target = crate::runtime::json_transport::JsonProcess;
    fn deref(&self) -> &Self::Target {
        &self.transport
    }
}
impl std::ops::DerefMut for NativeProcess {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.transport
    }
}

#[cfg(test)]
mod tests;

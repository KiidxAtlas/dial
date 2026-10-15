//! The `bash` tool: platform-native shell execution, process ownership, live output and artifacts.

use super::sandbox::is_secret_env;
#[cfg(not(windows))]
use super::sandbox::{Policy, Sandbox};
use super::{SCRATCH_DIR, ToolOutput, Workspace, opt_bool, opt_u64, req_str};
use crate::config::types::ShellBackend;
use anyhow::Result;
use serde_json::Value;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const DEFAULT_BASH_TIMEOUT_SECS: u64 = 300;

fn working_directory(
    ws: &Workspace,
    args: &Value,
    command: &str,
) -> Result<(std::path::PathBuf, String), String> {
    let mut cwd = ws.root().to_path_buf();
    if let Some(value) = args.get("cwd") {
        let value = value.as_str().ok_or("cwd must be a string")?;
        let path = Path::new(value);
        cwd = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        cwd = cwd
            .canonicalize()
            .map_err(|e| format!("resolving cwd: {e}"))?;
        if !cwd.starts_with(ws.root()) || !cwd.is_dir() {
            return Err("cwd must be an existing directory inside the workspace".into());
        }
    }
    #[cfg(not(windows))]
    {
        // Retain the bash convenience of accepting an initial `cd path && ...`; PowerShell
        // commands stay untouched and use the explicit `cwd` argument/current directory.
        let trimmed = command.trim_start();
        if let Some(rest) = trimmed.strip_prefix("cd ") {
            if let Some((target, script)) = rest.split_once(" && ") {
                let target = target.trim();
                if !target.is_empty() && !target.contains(['\'', '"', '`', '$', ';', '|']) {
                    let path = Path::new(target);
                    let candidate = if path.is_absolute() {
                        path.to_path_buf()
                    } else {
                        cwd.join(path)
                    };
                    if let Ok(path) = candidate.canonicalize()
                        && path.is_dir()
                        && path.starts_with(ws.root())
                    {
                        return Ok((path, script.to_owned()));
                    }
                }
            }
        }
    }
    Ok((cwd, command.to_owned()))
}
const MAX_COMMAND: usize = 64 * 1024;
const BASH_HEAD: usize = 6 * 1024;
const BASH_TAIL: usize = 14 * 1024;
/// Diagnostic lines kept from the omitted middle (a compiler's first error is often there).
const BASH_DIAGNOSTICS: usize = 3 * 1024;
#[cfg(not(windows))]
const CLT_DIR: &str = "/Library/Developer/CommandLineTools";

/// Transfers the process leader out of the foreground owner's drop guard.
pub(crate) fn release_process(mut group: ProcessGroup) -> Child {
    group.terminated = true;
    let group = std::mem::ManuallyDrop::new(group);
    unsafe { std::ptr::read(&group.child) }
}
/// A foreground process and its output stream, transferred intact to the managed background owner.
#[doc(hidden)]
pub struct ForegroundJob {
    pub(crate) workspace: Workspace,
    pub(crate) command: String,
    pub(crate) cwd: std::path::PathBuf,
    pub(crate) child: Child,
    pub(crate) reader: io::PipeReader,
    /// Output already read before adoption, so the adopted job keeps it.
    pub(crate) captured: Vec<u8>,
    /// Complete-output artifact already being written before adoption.
    pub(crate) spool: Option<super::artifact::ArtifactSpool>,
}

/// The last lines of output captured before adoption, bounded so the adoption notice stays small.
fn output_tail(bytes: &[u8]) -> String {
    const MAX_BYTES: usize = 2000;
    const MAX_LINES: usize = 20;
    let text = String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(MAX_BYTES)..]);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(MAX_LINES)..].join("\n")
}

impl ForegroundJob {
    pub(crate) fn into_parts(
        self,
    ) -> (
        Workspace,
        String,
        std::path::PathBuf,
        Child,
        io::PipeReader,
        Vec<u8>,
        Option<super::artifact::ArtifactSpool>,
    ) {
        let job = std::mem::ManuallyDrop::new(self);
        unsafe {
            (
                std::ptr::read(&job.workspace),
                std::ptr::read(&job.command),
                std::ptr::read(&job.cwd),
                std::ptr::read(&job.child),
                std::ptr::read(&job.reader),
                std::ptr::read(&job.captured),
                std::ptr::read(&job.spool),
            )
        }
    }
}

impl Drop for ForegroundJob {
    fn drop(&mut self) {
        let _ = crate::platform::process::kill_group(self.child.id());
        let _ = self.child.wait();
    }
}

/// Kills the whole process group on drop so nothing outlives its owning operation.
pub(crate) struct ProcessGroup {
    pub(crate) child: Child,
    terminated: bool,
}

impl ProcessGroup {
    pub(crate) fn new(child: Child) -> Self {
        ProcessGroup {
            child,
            terminated: false,
        }
    }

    pub(crate) fn terminate(&mut self) {
        if !self.terminated {
            let _ = crate::platform::process::terminate_group(self.child.id());
            // The leader's exit wakes this wait directly; the group is killed only if it outlives the grace period.
            let _ = crate::platform::process::wait_exited(&self.child, Duration::from_millis(100));
            let _ = crate::platform::process::kill_group(self.child.id());
            self.terminated = true;
        }
    }

    /// True once the leader has exited. It is not reaped, so its PID stays reserved.
    pub(crate) fn exited(&self) -> io::Result<bool> {
        crate::platform::process::exited(&self.child)
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.terminate();
        let _ = self.child.wait();
        crate::platform::process::forget_group(self.child.id());
    }
}

/// Keeps the first and last bytes of a stream, counting what was dropped, plus the diagnostic lines
/// (errors, panics, failed tests) found in the dropped middle.
struct Capture {
    head: Vec<u8>,
    tail: Vec<u8>,
    total: usize,
    diagnostics: super::diagnostics::Collector,
}

impl Capture {
    fn new() -> Self {
        Capture {
            head: Vec::new(),
            tail: Vec::new(),
            total: 0,
            diagnostics: super::diagnostics::Collector::new(BASH_DIAGNOSTICS),
        }
    }

    /// The head and tail bytes kept so far, in order; the dropped middle is not retained.
    fn retained(&self) -> Vec<u8> {
        let mut bytes = self.head.clone();
        bytes.extend_from_slice(&self.tail);
        bytes
    }

    fn push(&mut self, mut chunk: &[u8]) {
        self.total += chunk.len();
        if self.head.len() < BASH_HEAD {
            let take = (BASH_HEAD - self.head.len()).min(chunk.len());
            self.head.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
        }
        self.tail.extend_from_slice(chunk);
        if self.tail.len() > 2 * BASH_TAIL {
            self.drop_front(self.tail.len() - BASH_TAIL);
        }
    }

    fn drop_front(&mut self, excess: usize) {
        self.diagnostics.feed(&self.tail[..excess]);
        self.tail.drain(..excess);
    }

    fn finish(mut self) -> String {
        if self.tail.len() > BASH_TAIL {
            self.drop_front(self.tail.len() - BASH_TAIL);
        }
        self.diagnostics.finish();
        let dropped = self.total - self.head.len() - self.tail.len();
        let mut text = String::from_utf8_lossy(&self.head).into_owned();
        let (kept, lines, overflow) = self.diagnostics.kept();
        if dropped > 0 && lines > 0 {
            let more = if overflow > 0 {
                format!(" ({overflow} more not shown)")
            } else {
                String::new()
            };
            text.push_str(&format!("\n[... {dropped} bytes of output omitted; {lines} diagnostic line(s) from that part follow{more} ...]\n{kept}[... end of omitted-part diagnostics ...]\n"));
        } else if dropped > 0 {
            text.push_str(&format!("\n[... {dropped} bytes of output omitted ...]\n"));
        }
        text.push_str(&String::from_utf8_lossy(&self.tail));
        text
    }
}

/// Read what is available without blocking. Returns true at EOF.
fn drain_pipe(
    reader: &mut (impl Read + crate::platform::io::Pipe),
    capture: &mut Capture,
    spool: &mut Option<super::artifact::ArtifactSpool>,
    artifact_error: &mut Option<String>,
    on_output: &(dyn Fn(&[u8]) + Sync),
) -> io::Result<bool> {
    let mut buffer = [0u8; 16 * 1024];
    // Bounded work per pass so a noisy child cannot starve the deadline check.
    for _ in 0..16 {
        match crate::platform::io::read_nonblocking(reader, &mut buffer) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                if let Some(writer) = spool
                    && let Err(error) = writer.write_all(&buffer[..count])
                {
                    *artifact_error = Some(format!("capturing complete output: {error}"));
                    *spool = None;
                }
                capture.push(&buffer[..count]);
                on_output(&buffer[..count]);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}

/// The sandboxed bash's PATH on Unix: Homebrew or /usr/local, the Command Line Tools, then the system bins.
#[cfg(not(windows))]
fn bash_path() -> String {
    let mut paths = Vec::new();
    if Path::new("/opt/homebrew/bin").is_dir() {
        paths.push("/opt/homebrew/bin");
    } else if Path::new("/usr/local/bin").is_dir() {
        paths.push("/usr/local/bin");
    }
    if Path::new(CLT_DIR).is_dir() {
        paths.push("/Library/Developer/CommandLineTools/usr/bin");
    }
    paths.extend(["/usr/bin", "/bin", "/usr/sbin", "/sbin"]);
    paths.join(":")
}

/// Recognize only simple, conventional preview commands. This is a routing diagnostic, never an
/// approval exemption; compound shell scripts and one-shot flags retain normal bash handling.
fn preview_server(command: &str) -> bool {
    if command.chars().any(|c| ";&|`$\n\r".contains(c)) {
        return false;
    }
    let words: Vec<_> = command.split_whitespace().collect();
    let Some(program) = words.first() else {
        return false;
    };
    if words
        .iter()
        .any(|w| matches!(*w, "--help" | "-h" | "--version" | "-v" | "--once"))
    {
        return false;
    }
    let args = &words[1..];
    let program = Path::new(program)
        .file_name()
        .and_then(|p| p.to_str())
        .unwrap_or(program);
    match program {
        "pnpm" | "yarn" | "bun" | "npm" => {
            let args = args.strip_prefix(&["run"]).unwrap_or(args);
            args.first()
                .is_some_and(|arg| matches!(*arg, "dev" | "serve" | "preview" | "watch"))
        }
        "nuxt" | "nuxi" | "next" | "astro" => args.first() == Some(&"dev"),
        "vite" => {
            args.is_empty()
                || args.first().is_some_and(|arg| {
                    arg.starts_with('-') || matches!(*arg, "dev" | "serve" | "preview")
                })
        }
        "python" | "python3" => args.starts_with(&["-m", "http.server"]),
        _ => false,
    }
}

pub(super) fn bash_tool(
    ws: &Workspace,
    args: &Value,
    timeout_secs: u64,
) -> Result<ToolOutput, String> {
    bash_tool_streaming(ws, args, timeout_secs, &|_| {}, &AtomicBool::new(false))
}

/// Executes the platform-native shell with bounded live chunks, artifact capture, and cooperative cancellation.
/// The callback runs before the final result, on the calling thread, with at most 16 KiB.
/// Cancellation is checked at least every pipe-drain pass (maximum 256 KiB / 20 ms poll).
/// Artifact failures retain inline output and exit status, but make the tool result an error.
/// Cleanup waits up to 300ms for pipe EOF; a saved stream without EOF is labeled partial.
pub fn bash_tool_streaming(
    ws: &Workspace,
    args: &Value,
    timeout_secs: u64,
    on_output: &(dyn Fn(&[u8]) + Sync),
    cancel: &AtomicBool,
) -> Result<ToolOutput, String> {
    if opt_bool(args, "async")? || opt_bool(args, "background")? {
        return Err("managed background adoption is unavailable".into());
    }
    // Without an adoption owner, cooperative steering must not kill foreground effects.
    // Hard cancellation still travels through `cancel`.
    super::signals::scope(&super::signals::CallSignals::new(), || {
        bash_tool_streaming_with_adopter(
            ws,
            args,
            timeout_secs,
            0,
            on_output,
            cancel,
            &|mut job| {
                let _ = crate::platform::process::kill_group(job.child.id());
                let _ = job.child.wait();
                Err("managed background adoption is unavailable".into())
            },
        )
    })
}

pub fn bash_tool_streaming_with_adopter(
    ws: &Workspace,
    args: &Value,
    timeout_secs: u64,
    auto_background_secs: u64,
    on_output: &(dyn Fn(&[u8]) + Sync),
    cancel: &AtomicBool,
    on_adopt: &(dyn Fn(ForegroundJob) -> Result<String, String> + Sync),
) -> Result<ToolOutput, String> {
    let command = req_str(args, "command")?;
    if command.trim().is_empty() {
        return Err("command must not be empty".to_string());
    }
    if command.len() > MAX_COMMAND {
        return Err(format!("command is longer than {MAX_COMMAND} bytes"));
    }
    if let Some((config, offered)) = ws.bash_interceptor() {
        let offered: Vec<&str> = offered.iter().map(String::as_str).collect();
        if let Some(message) = crate::tools::bash_interceptor::check(config, &offered, command) {
            return Ok(ToolOutput::err(message));
        }
    }
    let session = super::opt_str(args, "session")?;
    let asynchronous = opt_bool(args, "async")? || opt_bool(args, "background")?;
    if let Some(name) = session {
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(
                "shell session name needs 1..64 ASCII letters, digits, underscores or hyphens"
                    .into(),
            );
        }
        if asynchronous
            || resolve_runner(
                ws.shell_backend,
                find_system_bash(),
                cfg!(feature = "embedded-shell"),
                cfg!(windows),
            )? != Runner::Embedded
        {
            return Err(
                "named shell state requires the embedded backend and foreground execution".into(),
            );
        }
    }
    let mut sessions = session
        .map(|_| {
            ws.shell_sessions
                .try_lock()
                .map_err(|_| "another named shell command is active in this workspace".to_string())
        })
        .transpose()?;
    let seed = session.and_then(|name| sessions.as_ref()?.get(name).cloned());
    if sessions.as_ref().is_some_and(|sessions| {
        sessions.len() >= 128 && session.is_some_and(|name| !sessions.contains_key(name))
    }) {
        return Err("workspace shell state limit is 128 named sessions".into());
    }
    let mut effective = args.clone();
    if effective.get("cwd").is_none()
        && let Some(seed) = &seed
    {
        effective["cwd"] = serde_json::json!(seed.cwd);
    }
    let (cwd, command) = working_directory(ws, &effective, command)?;
    let exchange = if session.is_some() {
        let mut seed = seed.unwrap_or_else(|| crate::embedded_shell::state::State::new(&cwd));
        seed.cwd = cwd.clone();
        Some(crate::embedded_shell::state::Exchange::new(
            &ws.root.join(SCRATCH_DIR),
            &seed,
        )?)
    } else {
        None
    };
    if preview_server(&command) {
        let start = serde_json::json!({"action":"start","name":"preview","command":command});
        return Ok(ToolOutput::err(format!(
            "Command not started: preview servers must use the managed background tool `bg`: {start}"
        )));
    }
    let timeout_secs = match opt_u64(args, "timeout")? {
        None => DEFAULT_BASH_TIMEOUT_SECS.min(timeout_secs),
        Some(0) => timeout_secs,
        Some(requested) if timeout_secs == 0 => requested,
        Some(requested) => requested.min(timeout_secs),
    };

    if cancel.load(Ordering::Acquire) {
        return Ok(ToolOutput::err("[cancelled before command started]\n"));
    }
    let mut spool = ws
        .artifacts()
        .map(|store| store.spool())
        .transpose()
        .map_err(|e| format!("creating command output artifact: {e}"))?;
    let mut spawn = shell_command_with_state(
        ws,
        &command,
        &cwd,
        None,
        exchange.as_ref().map(|exchange| exchange.path()),
    )?;
    let (mut reader, writer) = io::pipe().map_err(|e| format!("creating output pipe: {e}"))?;
    let writer_err = writer
        .try_clone()
        .map_err(|e| format!("creating output pipe: {e}"))?;
    spawn.stdin(Stdio::null()).stdout(writer).stderr(writer_err);
    let child = crate::platform::process::spawn_group(&mut spawn)
        .map_err(|e| format!("starting sandboxed shell: {e}"))?;
    // The Command owns the pipe's write ends; release them so EOF is observable.
    drop(spawn);

    let mut group = ProcessGroup::new(child);
    crate::platform::io::set_nonblocking(&reader)
        .map_err(|e| format!("configuring output pipe: {e}"))?;

    let timeout = Duration::from_secs(timeout_secs);
    let started = Instant::now();
    let mut capture = Capture::new();
    let mut artifact_error = None;
    let mut capture_error = None;
    let mut eof = false;
    let mut timed_out = false;
    let mut cancelled = false;
    let auto_background = Duration::from_secs(auto_background_secs);
    let waiter = crate::platform::wake::Waiter::new()
        .map_err(|e| format!("creating command wait channel: {e}"))?;
    loop {
        if session.is_none()
            && (asynchronous
                || crate::tools::signals::steering_requested()
                || (!auto_background.is_zero() && started.elapsed() >= auto_background))
        {
            crate::platform::io::set_blocking(&reader)
                .map_err(|e| format!("restoring adopted output pipe mode: {e}"))?;
            let captured = capture.retained();
            let so_far = output_tail(&captured);
            let job = ForegroundJob {
                workspace: ws.clone(),
                command: command.clone(),
                cwd: cwd.clone(),
                child: release_process(group),
                reader,
                captured,
                spool: spool.take(),
            };
            return on_adopt(job).map(|id| {
                let so_far = if so_far.is_empty() {
                    String::new()
                } else {
                    format!("Output so far:\n{so_far}\n")
                };
                ToolOutput::ok(format!(
                    "command is running as background job {id} (after {}s): {command}\n{so_far}Keep working; you are notified when it finishes. Inspect with bg output/wait, stop with bg kill.",
                    started.elapsed().as_secs()
                ))
            });
        }
        if cancel.load(Ordering::Acquire) {
            cancelled = true;
            break;
        }
        if !eof {
            match drain_pipe(
                &mut reader,
                &mut capture,
                &mut spool,
                &mut artifact_error,
                on_output,
            ) {
                Ok(closed) => eof = closed,
                Err(error) => {
                    capture_error = Some(format!("reading command output: {error}"));
                    break;
                }
            }
        }
        if group
            .exited()
            .map_err(|e| format!("observing command exit: {e}"))?
        {
            break;
        }
        if !timeout.is_zero() && started.elapsed() >= timeout {
            timed_out = true;
            break;
        }
        // Block until output arrives, the leader exits, a cancel is signalled, or a deadline
        // passes; the loop re-checks each condition on every return.
        // Cap each slice so a cancel flag stored without a wake signal is still seen promptly.
        let slice = Duration::from_millis(100);
        let mut limit = (!timeout.is_zero()).then(|| timeout.saturating_sub(started.elapsed()));
        if !auto_background.is_zero() {
            let until = auto_background.saturating_sub(started.elapsed());
            limit = Some(limit.map_or(until, |l| l.min(until)));
        }
        let limit = Some(limit.map_or(slice, |l| l.min(slice)));
        waiter
            .wait((!eof).then_some(&reader), Some(&group.child), limit)
            .map_err(|e| format!("waiting for command output: {e}"))?;
    }
    group.terminate();
    let status = group
        .child
        .wait()
        .map_err(|e| format!("reaping command: {e}"))?;
    // Everything in the group is dead; collect what it wrote. A process that
    // escaped the group may still hold the pipe, so the wait is bounded.
    let grace = Instant::now();
    while !eof && capture_error.is_none() && grace.elapsed() < Duration::from_millis(300) {
        match drain_pipe(
            &mut reader,
            &mut capture,
            &mut spool,
            &mut artifact_error,
            on_output,
        ) {
            Ok(closed) => eof = closed,
            Err(error) => {
                capture_error = Some(format!("reading command output: {error}"));
                break;
            }
        }
        if !eof {
            waiter
                .wait(
                    Some(&reader),
                    None::<&std::process::Child>,
                    Some(Duration::from_millis(300).saturating_sub(grace.elapsed())),
                )
                .map_err(|e| format!("waiting for command output: {e}"))?;
        }
    }
    if !eof && capture_error.is_none() {
        capture_error = Some(
            "pipe did not reach EOF within the 300ms cleanup grace; output may be incomplete"
                .to_string(),
        );
    }

    let mut text = capture.finish();
    if text.is_empty() {
        text.push_str("(no output)");
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    let success = status.success() && !timed_out && !cancelled;
    if cancelled {
        text.push_str("[cancelled; process group killed]\n");
    } else if timed_out {
        text.push_str(&format!(
            "[timed out after {timeout_secs}s; process group killed]\n"
        ));
        text.push_str("[foreground commands do not survive this timeout; for a dev server/watcher discover/load bg, reuse an existing process or start it with bg, then wait for readiness before browser navigation]\n");
    } else if let Some(code) = status.code() {
        text.push_str(&format!("[exit code: {code}]\n"));
    } else if let Some(signal) = crate::platform::process::exit_signal(&status) {
        text.push_str(&format!("[terminated by signal {signal}]\n"));
    }
    if !success && text.contains("Operation not permitted") && text.contains("/tmp/") {
        text.push_str("[sandbox: /tmp is outside the project and not writable; use $TMPDIR, a scratch directory inside the project]\n");
    }
    if let Some(spool) = spool {
        match spool.finish() {
            Ok(uri) => {
                let completeness = if capture_error.is_some() {
                    "partial"
                } else {
                    "complete"
                };
                text.push_str(&format!("[{completeness} output: {uri}]\n"));
            }
            Err(error) => {
                artifact_error = Some(format!("saving complete command output artifact: {error}"));
            }
        }
    }
    if let Some(error) = &capture_error {
        text.push_str(&format!("[output capture error: {error}]\n"));
    }
    if let Some(error) = &artifact_error {
        text.push_str(&format!(
            "[output artifact error: {error}; inline output retained]\n"
        ));
    }
    let mut state_error = None;
    if let (Some(name), Some(sessions), Some(exchange)) =
        (session, sessions.as_mut(), exchange.as_ref())
    {
        sessions.remove(name);
        if !cancelled && !timed_out && capture_error.is_none() {
            match exchange.read().and_then(|state| {
                if !state.completed {
                    return Err("shell did not publish a completed state exchange".into());
                }
                let cwd = state
                    .cwd
                    .canonicalize()
                    .map_err(|error| error.to_string())?;
                if !cwd.starts_with(ws.root()) || !cwd.is_dir() {
                    return Err("shell state cwd is outside this workspace".into());
                }
                Ok(state)
            }) {
                Ok(state) => {
                    sessions.insert(name.to_owned(), state);
                }
                Err(error) => {
                    text.push_str(&format!("[shell state invalidated: {error}]\n"));
                    state_error = Some(error);
                }
            }
        } else {
            text.push_str("[shell state invalidated by incomplete execution]\n");
        }
    }
    let output = ToolOutput {
        execution: Default::default(),
        data: None,
        text,
        is_error: !success
            || capture_error.is_some()
            || artifact_error.is_some()
            || state_error.is_some(),
    };
    let output = output.with_process(crate::runtime::execution::ProcessOutcome {
        started: true,
        exit_code: status.code(),
        signal: crate::platform::process::exit_signal(&status),
        timed_out,
        cancelled,
        output_complete: capture_error.is_none() && artifact_error.is_none() && eof,
    });
    Ok(if cancelled {
        output
            .with_status(super::ToolStatus::Aborted)
            .with_effects(super::outcome::Effects::Unknown)
    } else if timed_out {
        output
            .with_status(super::ToolStatus::Timeout)
            .with_effects(super::outcome::Effects::Unknown)
    } else {
        output
    })
}

/// Build the platform shell command in `cwd`. POSIX commands use the workspace sandbox policy or
/// its explicit unsandboxed opt-out; Windows PowerShell requires that opt-out because native
/// filesystem/network isolation is unavailable. Stdio and process ownership belong to the caller.
pub(super) fn shell_command(
    ws: &Workspace,
    script: &str,
    cwd: &Path,
) -> Result<std::process::Command, String> {
    shell_command_mode(ws, script, cwd, None)
}

/// Which program runs a `bash` command.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Runner {
    /// bash with `-c script`: `/bin/bash`, or Git for Windows' `bash.exe` on Windows.
    Bash(PathBuf),
    /// PowerShell (Windows only).
    PowerShell,
    /// Dial's built-in shell, hosted by a child Dial process.
    Embedded,
}

/// Maps `shell.backend` to a runner. `auto` prefers bash, then the built-in shell, then PowerShell
/// on Windows when the built-in shell is not in this build.
fn resolve_runner(
    backend: ShellBackend,
    system_bash: Option<PathBuf>,
    embedded_built: bool,
    windows: bool,
) -> Result<Runner, String> {
    match backend {
        ShellBackend::Auto => match system_bash {
            Some(bash) => Ok(Runner::Bash(bash)),
            None if embedded_built => Ok(Runner::Embedded),
            None if windows => Ok(Runner::PowerShell),
            None => Err("no bash was found and the built-in shell is not in this build".into()),
        },
        ShellBackend::System => system_bash.map(Runner::Bash).ok_or_else(|| {
            if windows {
                "shell.backend is \"system\" but no bash was found; install Git for Windows, or set shell.backend to \"embedded\" or \"powershell\"".into()
            } else {
                "shell.backend is \"system\" but no bash was found (/bin/bash is missing); set shell.backend to \"embedded\"".into()
            }
        }),
        ShellBackend::PowerShell if windows => Ok(Runner::PowerShell),
        ShellBackend::PowerShell => Err(
            "shell.backend is \"powershell\" but PowerShell is only available on Windows; set shell.backend to \"system\" or \"embedded\""
                .into(),
        ),
        ShellBackend::Embedded if embedded_built => Ok(Runner::Embedded),
        ShellBackend::Embedded => Err(
            "the built-in shell is not in this build (the embedded-shell feature is disabled); set shell.backend to \"system\""
                .into(),
        ),
    }
}

/// The bash `auto` and `system` use: `/bin/bash` on Unix; on Windows Git for Windows' `bash.exe`
/// from its standard install folders or PATH. WSL's `bash.exe` launcher is not Git Bash and is
/// never returned.
fn find_system_bash() -> Option<PathBuf> {
    if !cfg!(windows) {
        let bash = Path::new("/bin/bash");
        return bash.is_file().then(|| bash.to_path_buf());
    }
    let mut bin_dirs = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(dir) = std::env::var_os(var) {
            bin_dirs.push(PathBuf::from(dir).join("Git").join("bin"));
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        bin_dirs.push(
            PathBuf::from(local)
                .join("Programs")
                .join("Git")
                .join("bin"),
        );
    }
    if let Some(path) = std::env::var_os("PATH") {
        bin_dirs.extend(std::env::split_paths(&path));
    }
    git_for_windows_bash(bin_dirs)
}

/// The first `bin` folder holding `bash.exe` whose Git installation (its parent) also has
/// `cmd\git.exe`. That pair identifies Git for Windows and rejects WSL's launcher.
fn git_for_windows_bash(bin_dirs: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    bin_dirs.into_iter().find_map(|bin| {
        let bash = bin.join("bash.exe");
        let git = bin.parent()?.join("cmd").join("git.exe");
        (bash.is_file() && git.is_file()).then_some(bash)
    })
}

/// One-line description of the shell `bash` commands run in, for the start-up banner.
pub fn shell_label(backend: ShellBackend) -> String {
    let runner = resolve_runner(
        backend,
        find_system_bash(),
        cfg!(feature = "embedded-shell"),
        cfg!(windows),
    );
    match runner {
        Ok(Runner::Bash(path)) => format!("bash ({})", path.display()),
        Ok(Runner::PowerShell) => "PowerShell".into(),
        Ok(Runner::Embedded) => "built-in shell".into(),
        Err(error) => format!("unavailable ({error})"),
    }
}

/// Writes `script` for the built-in shell and returns the host program and its arguments.
fn embedded_invocation(
    scratch: &Path,
    script: &str,
    state: Option<&Path>,
) -> Result<(String, Vec<String>), String> {
    let (program, mut args) = crate::embedded_shell::invocation(scratch, script)
        .map_err(|e| format!("preparing the built-in shell: {e}"))?;
    if let Some(state) = state {
        args.push(
            state
                .to_str()
                .ok_or("shell state path is not UTF-8")?
                .to_owned(),
        );
    }
    let program = program
        .to_str()
        .ok_or("Dial executable path is not UTF-8")?
        .to_owned();
    Ok((program, args))
}

/// Background PTYs alone retain a new private terminal session through the sandbox.
pub(super) fn shell_command_mode(
    ws: &Workspace,
    script: &str,
    cwd: &Path,
    owned_terminal: Option<i32>,
) -> Result<std::process::Command, String> {
    shell_command_with_state(ws, script, cwd, owned_terminal, None)
}
fn shell_command_with_state(
    ws: &Workspace,
    script: &str,
    cwd: &Path,
    owned_terminal: Option<i32>,
    state: Option<&Path>,
) -> Result<std::process::Command, String> {
    let runner = resolve_runner(
        ws.shell_backend,
        find_system_bash(),
        cfg!(feature = "embedded-shell"),
        cfg!(windows),
    )?;
    #[cfg(windows)]
    {
        if !ws.unsandboxed {
            return Err("Windows command isolation is unavailable; enable the explicit unsandboxed command setting to run bash, PowerShell or the built-in shell".into());
        }
        if owned_terminal.is_some() {
            return Err("Windows PTY commands require the ConPTY process interface".into());
        }
        let scratch = ws.root.join(SCRATCH_DIR);
        fs::create_dir_all(&scratch).map_err(|e| format!("creating scratch directory: {e}"))?;
        let mut spawn = match runner {
            Runner::Bash(bash) => {
                let mut spawn = std::process::Command::new(bash);
                spawn.args(["-c", script]);
                spawn
            }
            Runner::Embedded => {
                let (program, args) = embedded_invocation(&scratch, script, state)?;
                let mut spawn = std::process::Command::new(program);
                spawn.args(args);
                spawn
            }
            Runner::PowerShell => {
                let system_root =
                    std::env::var_os("SystemRoot").ok_or("SystemRoot is unavailable")?;
                let powershell = Path::new(&system_root)
                    .join("System32")
                    .join("WindowsPowerShell")
                    .join("v1.0")
                    .join("powershell.exe");
                let command = format!(
                    "$global:LASTEXITCODE = 0; {script}; if ($LASTEXITCODE -ne 0) {{ exit $LASTEXITCODE }}"
                );
                let mut spawn = std::process::Command::new(powershell);
                spawn
                    .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
                    .arg(command);
                spawn
            }
        };
        spawn.env_clear();
        for (key, value) in std::env::vars_os() {
            if !is_secret_env(&key.to_string_lossy(), &value.to_string_lossy()) {
                spawn.env(key, value);
            }
        }
        spawn
            .current_dir(cwd)
            .env("TEMP", &scratch)
            .env("TMP", &scratch)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never");
        return Ok(spawn);
    }
    #[cfg(not(windows))]
    {
        let root = ws.root.to_str().ok_or("workspace path is not UTF-8")?;
        let cwd_str = cwd.to_str().ok_or("working directory is not UTF-8")?;
        let scratch = ws.root.join(SCRATCH_DIR);
        fs::create_dir_all(&scratch).map_err(|e| format!("creating scratch directory: {e}"))?;
        let scratch_str = scratch
            .to_str()
            .ok_or("scratch path is not UTF-8")?
            .to_owned();
        let strict = matches!(ws.policy, None | Some(Policy::Strict { .. }));
        // bubblewrap starts every sandboxed program in the workspace root (`--chdir`), so the
        // process working directory set below does not reach the shell; enter `cwd` explicitly.
        #[cfg(target_os = "linux")]
        let entered;
        #[cfg(target_os = "linux")]
        let script = if !ws.unsandboxed && cwd != ws.root.as_path() {
            entered = format!(
                "cd {} || exit 1\n{script}",
                crate::tools::kernel::shell_quote(cwd_str)
            );
            entered.as_str()
        } else {
            script
        };
        let (program, args) = match runner {
            Runner::Bash(bash) => (
                bash.to_str().ok_or("bash path is not UTF-8")?.to_owned(),
                vec!["-c".to_owned(), script.to_owned()],
            ),
            Runner::Embedded => embedded_invocation(&scratch, script, state)?,
            Runner::PowerShell => return Err("PowerShell is only available on Windows".into()),
        };
        let mut spawn;
        if ws.unsandboxed {
            spawn = std::process::Command::new(&program);
            spawn.args(&args).env_clear();
            for (key, value) in std::env::vars_os() {
                if !is_secret_env(&key.to_string_lossy(), &value.to_string_lossy()) {
                    spawn.env(key, value);
                }
            }
            spawn
                .env("TMPDIR", &scratch_str)
                .env("PAGER", "cat")
                .env("GIT_PAGER", "cat");
        } else {
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let sandbox = ws.sandbox.clone().unwrap_or_else(Sandbox::detect);
            let policy = ws.sandbox_policy();
            spawn = if let Some(fd) = owned_terminal {
                sandbox.command_with_owned_terminal(&ws.root, &policy, &program, &arg_refs, fd)?
            } else {
                sandbox.command_with_policy(&ws.root, &policy, &program, &arg_refs)?
            };
            if strict {
                spawn
                    .env_clear()
                    .env("PATH", bash_path())
                    .env("HOME", root)
                    .env("TMPDIR", &scratch_str)
                    .env("PWD", cwd_str)
                    .env("SHELL", "/bin/bash")
                    .env("LANG", "en_US.UTF-8");
                if Path::new(CLT_DIR).is_dir() {
                    spawn.env("DEVELOPER_DIR", CLT_DIR);
                }
            } else {
                spawn.env("PAGER", "cat").env("GIT_PAGER", "cat");
            }
        }
        spawn
            .current_dir(cwd)
            .env("TERM", "dumb")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never")
            .env("GIT_ASKPASS", "/usr/bin/false")
            .env("SSH_ASKPASS", "/usr/bin/false");
        Ok(spawn)
    }
}

#[cfg(test)]
mod backend_tests {
    use super::*;

    fn bash() -> PathBuf {
        PathBuf::from("/bin/bash")
    }

    #[test]
    fn explicit_system_uses_bash_or_fails_clearly() {
        assert_eq!(
            resolve_runner(ShellBackend::System, Some(bash()), true, false),
            Ok(Runner::Bash(bash()))
        );
        let err = resolve_runner(ShellBackend::System, None, true, false).unwrap_err();
        assert!(err.contains("no bash"), "{err}");
        assert!(err.contains("\"embedded\""), "{err}");
    }

    #[test]
    fn explicit_embedded_ignores_an_installed_bash() {
        assert_eq!(
            resolve_runner(ShellBackend::Embedded, Some(bash()), true, false),
            Ok(Runner::Embedded)
        );
    }

    #[test]
    fn embedded_without_the_feature_is_a_clear_error() {
        let err = resolve_runner(ShellBackend::Embedded, Some(bash()), false, false).unwrap_err();
        assert!(err.contains("embedded-shell"), "{err}");
        assert!(err.contains("\"system\""), "{err}");
    }

    #[test]
    fn powershell_is_windows_only() {
        assert_eq!(
            resolve_runner(ShellBackend::PowerShell, None, true, true),
            Ok(Runner::PowerShell)
        );
        let err = resolve_runner(ShellBackend::PowerShell, Some(bash()), true, false).unwrap_err();
        assert!(err.contains("only available on Windows"), "{err}");
    }

    #[test]
    fn auto_prefers_bash_then_embedded() {
        assert_eq!(
            resolve_runner(ShellBackend::Auto, Some(bash()), true, false),
            Ok(Runner::Bash(bash()))
        );
        assert_eq!(
            resolve_runner(ShellBackend::Auto, None, true, false),
            Ok(Runner::Embedded)
        );
    }

    #[test]
    fn auto_on_windows_prefers_git_bash_then_embedded_then_powershell() {
        let git = PathBuf::from(r"C:\Program Files\Git\bin\bash.exe");
        assert_eq!(
            resolve_runner(ShellBackend::Auto, Some(git.clone()), true, true),
            Ok(Runner::Bash(git))
        );
        assert_eq!(
            resolve_runner(ShellBackend::Auto, None, true, true),
            Ok(Runner::Embedded)
        );
        assert_eq!(
            resolve_runner(ShellBackend::Auto, None, false, true),
            Ok(Runner::PowerShell)
        );
    }

    #[test]
    fn auto_without_any_shell_is_a_clear_error() {
        let err = resolve_runner(ShellBackend::Auto, None, false, false).unwrap_err();
        assert!(err.contains("no bash was found"), "{err}");
    }

    /// Creates `path` with empty contents, along with its parent folders.
    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"").unwrap();
    }

    #[test]
    fn git_bash_needs_bash_and_git_from_the_same_installation() {
        let root = std::env::temp_dir().join(format!("dial-git-bash-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let installed = root.join("Git");
        touch(&installed.join("bin").join("bash.exe"));
        touch(&installed.join("cmd").join("git.exe"));
        // WSL's launcher sits in System32 with no Git installation beside it.
        touch(&root.join("System32").join("bash.exe"));
        // A bin folder whose installation lacks git.exe is not Git for Windows.
        touch(&root.join("Other").join("bin").join("bash.exe"));

        let found = git_for_windows_bash([
            root.join("System32"),
            root.join("Other").join("bin"),
            installed.join("bin"),
        ]);
        assert_eq!(found, Some(installed.join("bin").join("bash.exe")));
        assert_eq!(
            git_for_windows_bash([root.join("System32"), root.join("Other").join("bin")]),
            None
        );
        let _ = fs::remove_dir_all(&root);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    use crate::tools::execute;
    use crate::tools::testkit::*;
    use serde_json::json;
    #[cfg(target_os = "macos")]
    use std::path::PathBuf;
    fn sandboxed() -> bool {
        super::super::sandbox::require_for_test()
    }

    fn bash(ws: &Workspace, command: &str) -> ToolOutput {
        run(ws, "bash", json!({"command": command}))
    }

    #[test]
    fn cwd_argument_and_leading_cd_are_confined_to_workspace() {
        let (dir, ws) = workspace();
        fs::create_dir(dir.path().join("src")).unwrap();
        let (cwd, command) = working_directory(&ws, &json!({"cwd": "src"}), "pwd").unwrap();
        assert_eq!(cwd, ws.root().join("src"));
        assert_eq!(command, "pwd");
        let (cwd, command) = working_directory(&ws, &json!({}), "cd src && pwd").unwrap();
        assert_eq!(cwd, ws.root().join("src"));
        assert_eq!(command, "pwd");
        assert!(working_directory(&ws, &json!({"cwd": "../"}), "pwd").is_err());
    }
    #[cfg(unix)]
    #[test]
    fn steering_hands_off_running_work_without_killing_its_effects() {
        let (dir, ws) = workspace();
        let ws = ws.without_sandbox();
        let signals = super::super::signals::CallSignals::new();
        signals.request_steering();
        let adopted = parking_lot::Mutex::new(None);
        let output = super::super::signals::scope(&signals, || {
            bash_tool_streaming_with_adopter(
                &ws,
                &json!({"command":"sleep 0.1; printf survived > result.txt"}),
                5,
                0,
                &|_| {},
                &AtomicBool::new(false),
                &|job| {
                    *adopted.lock() = Some(job);
                    Ok("bg-test".into())
                },
            )
        })
        .unwrap();
        assert!(output.text.contains("bg-test"));
        let mut job = adopted.lock().take().unwrap();
        assert!(job.child.wait().unwrap().success());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("result.txt")).unwrap(),
            "survived"
        );
    }

    #[cfg(unix)]
    #[test]
    fn explicit_background_and_threshold_both_adopt_with_a_notice() {
        let (_dir, ws) = workspace();
        let ws = ws.without_sandbox();
        let cancel = AtomicBool::new(false);
        let adopted = parking_lot::Mutex::new(0usize);
        let on_adopt = |_job: ForegroundJob| {
            *adopted.lock() += 1;
            Ok("bg7".to_owned())
        };
        let explicit = bash_tool_streaming_with_adopter(
            &ws,
            &json!({"command": "sleep 5", "background": true}),
            0,
            0,
            &|_: &[u8]| {},
            &cancel,
            &on_adopt,
        )
        .unwrap();
        assert!(
            explicit.text.contains("background job bg7"),
            "{}",
            explicit.text
        );
        let threshold = bash_tool_streaming_with_adopter(
            &ws,
            &json!({"command": "sleep 5"}),
            0,
            1,
            &|_: &[u8]| {},
            &cancel,
            &on_adopt,
        )
        .unwrap();
        assert!(
            threshold.text.contains("background job bg7"),
            "{}",
            threshold.text
        );
        assert_eq!(*adopted.lock(), 2);
    }
    #[cfg(unix)]
    #[test]
    fn preview_servers_are_redirected_before_spawn_but_finite_commands_still_run() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, ws) = workspace();
        let ws = ws.without_sandbox();
        fs::write(
            dir.path().join("pnpm"),
            "#!/bin/sh\necho launched > launched.txt\n",
        )
        .unwrap();
        fs::set_permissions(dir.path().join("pnpm"), fs::Permissions::from_mode(0o755)).unwrap();
        let refused = bash(&ws, "./pnpm dev --host 127.0.0.1 --port 3000");
        assert!(
            refused.is_error
                && refused.text.contains("Command not started")
                && refused.text.contains("bg"),
            "{}",
            refused.text
        );
        assert!(
            !dir.path().join("launched.txt").exists(),
            "no unmanaged process may start"
        );
        let build = bash(&ws, "./pnpm run build");
        assert!(
            !build.is_error && build.text.contains("[exit code: 0]"),
            "{}",
            build.text
        );
        assert!(dir.path().join("launched.txt").exists());
        for command in [
            "npm run dev",
            "pnpm dev",
            "yarn dev",
            "bun run dev",
            "nuxt dev",
            "nuxi dev",
            "next dev",
            "astro dev",
            "vite --host 127.0.0.1",
            "python3 -m http.server 3000",
        ] {
            assert!(preview_server(command), "{command}");
        }
        for command in [
            "pnpm dev --help",
            "pnpm watch --once",
            "vite --version",
            "vite build",
            "pnpm run build",
            "npm test",
            "cargo test",
            "printf 'pnpm dev'",
            "echo ok; pnpm dev",
        ] {
            assert!(!preview_server(command), "{command}");
        }
    }

    #[test]
    fn bash_refuses_without_sandbox() {
        let (dir, ws) = workspace();
        let ws = ws.with_sandbox(Sandbox::Unavailable("test says no".to_string()));
        let out = bash(&ws, "touch ran.txt");
        assert!(
            out.is_error && out.text.contains("test says no"),
            "{}",
            out.text
        );
        assert!(!dir.path().join("ran.txt").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bash_reads_and_writes_inside_workspace_only() {
        if !sandboxed() {
            return;
        }
        let (dir, ws) = workspace();
        let outside = tempfile::tempdir().unwrap();
        let canary = outside.path().join("canary");
        fs::write(&canary, "OUTSIDE-SECRET").unwrap();
        let out = bash(
            &ws,
            "echo inside > made.txt && cat made.txt && mkdir -p d/e && echo x > d/e/f && pwd",
        );
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("inside"));
        assert!(
            out.text
                .contains(dir.path().canonicalize().unwrap().to_str().unwrap())
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("made.txt")).unwrap(),
            "inside\n"
        );

        let read = bash(&ws, &format!("cat '{}'", canary.display()));
        assert!(
            read.is_error && !read.text.contains("OUTSIDE-SECRET"),
            "{}",
            read.text
        );
        let write = bash(
            &ws,
            &format!("echo pwn > '{}/new'", outside.path().display()),
        );
        assert!(write.is_error && !outside.path().join("new").exists());
        let stat = bash(&ws, &format!("ls '{}'", outside.path().display()));
        assert!(stat.is_error);
        // Redirections to /dev/null and heredocs (temp files) work.
        let misc = bash(&ws, "echo hidden >/dev/null 2>&1; cat <<EOF\nhere\nEOF");
        assert!(
            !misc.is_error && misc.text.contains("here"),
            "{}",
            misc.text
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bash_cannot_read_home() {
        if !sandboxed() {
            return;
        }
        let (_dir, ws) = workspace();
        let home = PathBuf::from(std::env::var("HOME").expect("HOME"));
        let mut canary = tempfile::Builder::new()
            .prefix(".dial-sandbox-canary-")
            .tempfile_in(&home)
            .unwrap();
        io::Write::write_all(&mut canary, b"HOME-SECRET").unwrap();
        assert_eq!(fs::read_to_string(canary.path()).unwrap(), "HOME-SECRET");
        let out = bash(&ws, &format!("cat '{}'", canary.path().display()));
        assert!(
            out.is_error && !out.text.contains("HOME-SECRET"),
            "{}",
            out.text
        );
        let listing = bash(&ws, &format!("ls -a '{}'", home.display()));
        assert!(listing.is_error, "{}", listing.text);
        // The scrubbed HOME is the workspace, not the real home.
        let env = bash(&ws, "echo $HOME");
        assert!(
            env.text.starts_with(ws.root().to_str().unwrap()),
            "{}",
            env.text
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bash_environment_is_scrubbed() {
        if !sandboxed() {
            return;
        }
        let (_dir, ws) = workspace();
        // cargo exports CARGO_* variables into this test process.
        assert!(std::env::vars().any(|(k, _)| k.starts_with("CARGO")));
        let out = bash(&ws, "env | sort");
        assert!(!out.is_error);
        for line in out.text.lines() {
            let key = line.split('=').next().unwrap_or("");
            assert!(
                [
                    "PATH",
                    "HOME",
                    "TMPDIR",
                    "PWD",
                    "SHELL",
                    "LANG",
                    "TERM",
                    "DEVELOPER_DIR",
                    "SHLVL",
                    "_",
                    "OLDPWD",
                    // Non-interactive git/credential defaults set by `shell_command`.
                    "GIT_TERMINAL_PROMPT",
                    "GCM_INTERACTIVE",
                    "GIT_ASKPASS",
                    "SSH_ASKPASS"
                ]
                .contains(&key)
                    || line.starts_with('['),
                "unexpected environment entry {line:?}"
            );
        }
        assert!(out.text.contains("TMPDIR="));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bash_network_is_blocked_unless_allowed() {
        if !sandboxed() {
            return;
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let probe = format!("exec 3<>/dev/tcp/127.0.0.1/{port} && echo CONNECTED");
        // Control: the same command outside the sandbox does connect.
        let control = std::process::Command::new("/bin/bash")
            .args(["-c", &probe])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&control.stdout).contains("CONNECTED"),
            "control failed"
        );

        let (_dir, ws) = workspace();
        let blocked = bash(&ws, &probe);
        assert!(
            blocked.is_error && !blocked.text.contains("CONNECTED"),
            "{}",
            blocked.text
        );
        let curl = bash(&ws, "/usr/bin/curl --max-time 3 -sS https://example.com");
        assert!(
            curl.is_error && !curl.text.contains("Example Domain"),
            "{}",
            curl.text
        );

        let (_dir2, open) = workspace();
        let open = open.with_network(true);
        let allowed = bash(&open, &probe);
        assert!(allowed.text.contains("CONNECTED"), "{}", allowed.text);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bash_symlink_escape_is_blocked() {
        if !sandboxed() {
            return;
        }
        let (dir, ws) = workspace();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("canary"), "LINKED-SECRET").unwrap();
        let out = bash(
            &ws,
            &format!(
                "ln -s '{0}/canary' flink && ln -s '{0}' dlink; cat flink; cat dlink/canary; echo pwn > dlink/new",
                outside.path().display()
            ),
        );
        assert!(
            out.is_error && !out.text.contains("LINKED-SECRET"),
            "{}",
            out.text
        );
        assert!(!outside.path().join("new").exists());
        // The symlinks now exist in the workspace; the fs tools refuse them too.
        assert!(dir.path().join("flink").symlink_metadata().is_ok());
        assert!(run(&ws, "read", json!({"path": "flink"})).is_error);
        assert!(run(&ws, "read", json!({"path": "dlink/canary"})).is_error);
        assert!(run(&ws, "write", json!({"path": "dlink/new", "content": "x"})).is_error);
        assert!(
            !run(&ws, "grep", json!({"pattern": "LINKED"}))
                .text
                .contains("LINKED-SECRET")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bash_output_is_bounded_and_keeps_head_and_tail() {
        if !sandboxed() {
            return;
        }
        let (_dir, ws) = workspace();
        let out = bash(&ws, "echo START; seq 1 300000; echo END");
        assert!(!out.is_error);
        assert!(
            out.text.len() < BASH_HEAD + BASH_TAIL + 200,
            "{}",
            out.text.len()
        );
        assert!(out.text.starts_with("START\n") && out.text.contains("bytes of output omitted"));
        assert!(out.text.contains("\nEND\n[exit code: 0]"));
        let flood = bash(&ws, "yes | head -c 20000000");
        assert!(flood.text.len() < BASH_HEAD + BASH_TAIL + 200);
        let failing = bash(&ws, "echo oops >&2; exit 3");
        assert!(
            failing.is_error
                && failing.text.contains("oops")
                && failing.text.contains("[exit code: 3]")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bash_keeps_diagnostics_from_the_omitted_middle_within_bounds() {
        if !sandboxed() {
            return;
        }
        let (_dir, ws) = workspace();
        let out = bash(
            &ws,
            "seq 1 50000; echo 'error[E0308]: mismatched types'; echo '  --> src/lib.rs:7:5'; seq 1 50000; exit 101",
        );
        assert!(out.is_error);
        let omitted = out
            .text
            .find("diagnostic line(s) from that part follow")
            .expect("marker");
        let error = out
            .text
            .find("error[E0308]: mismatched types\n  --> src/lib.rs:7:5\n")
            .expect("the middle error survives");
        assert!(
            omitted < error && out.text.contains("[exit code: 101]"),
            "{}",
            &out.text[omitted..omitted + 300]
        );
        assert!(
            out.text.len() < BASH_HEAD + BASH_TAIL + BASH_DIAGNOSTICS + 300,
            "{}",
            out.text.len()
        );
    }

    #[cfg(target_os = "macos")]
    fn alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[cfg(target_os = "macos")]
    fn read_pid(dir: &Path, name: &str) -> i32 {
        fs::read_to_string(dir.join(name))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bash_timeout_kills_process_group_without_orphans() {
        if !sandboxed() {
            return;
        }
        let (dir, ws) = workspace();
        let started = Instant::now();
        let out = run(
            &ws,
            "bash",
            json!({"command": "sleep 30 & echo $! > bg.pid; echo $$ > sh.pid; echo partial; sleep 30", "timeout": 1}),
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "took {:?}",
            started.elapsed()
        );
        assert!(
            out.is_error && out.text.contains("timed out after 1s") && out.text.contains("partial"),
            "{}",
            out.text
        );
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !alive(read_pid(dir.path(), "bg.pid")),
            "backgrounded child survived"
        );
        assert!(!alive(read_pid(dir.path(), "sh.pid")), "shell survived");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bash_kills_background_jobs_when_command_returns() {
        if !sandboxed() {
            return;
        }
        let (dir, ws) = workspace();
        let started = Instant::now();
        let out = bash(&ws, "sleep 30 & echo $! > bg.pid; echo done");
        assert!(!out.is_error && out.text.contains("done"), "{}", out.text);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !alive(read_pid(dir.path(), "bg.pid")),
            "background job outlived the call"
        );
        // The harness cap wins over a larger requested timeout.
        let capped = execute(
            &ws,
            "bash",
            &json!({"command": "sleep 30", "timeout": 600}),
            1,
        );
        assert!(
            capped.text.contains("timed out after 1s"),
            "{}",
            capped.text
        );
        assert!(bash(&ws, "   ").is_error);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bash_runs_python_and_git_when_homebrew_is_present() {
        if !sandboxed() {
            return;
        }
        let (_dir, ws) = workspace();
        let out = bash(
            &ws,
            "python3 -c 'import json, sys; print(json.dumps([6*7]))'",
        );
        assert!(
            !out.is_error && out.text.contains("[42]"),
            "python3: {}",
            out.text
        );
        let git = bash(
            &ws,
            "git init -q repo && cd repo && echo a > a && git add a && \
             git -c user.name=t -c user.email=t@example.com commit -q -m first && git log --oneline | wc -l",
        );
        assert!(
            !git.is_error && git.text.trim_start().starts_with('1'),
            "git: {}",
            git.text
        );
    }

    #[test]
    fn streaming_output_precedes_result_and_has_bounded_chunks() {
        let (dir, ws) = workspace();
        let ws = ws.without_sandbox();
        let events = parking_lot::Mutex::new(Vec::new());
        let on_output = |bytes: &[u8]| {
            assert!(bytes.len() <= 16 * 1024);
            assert!(
                !dir.path().join("finished").exists(),
                "output arrived only after command finished"
            );
            events
                .lock()
                .push(String::from_utf8_lossy(bytes).into_owned());
        };
        let output = bash_tool_streaming(
            &ws,
            &json!({
                "command": "printf 'early\\n'; sleep 0.3; touch finished"
            }),
            5,
            &on_output,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(!output.is_error, "{}", output.text);
        let mut events = events.into_inner();
        assert!(events.join("").contains("early\n"));
        events.push("RESULT".into());
        assert!(events.first().unwrap().contains("early"));
        assert_eq!(events.last().unwrap(), "RESULT");
        assert!(dir.path().join("finished").exists());
    }

    #[test]
    fn complete_artifact_keeps_middle_of_large_binary_output() {
        let (_dir, ws) = workspace();
        let artifacts = tempfile::tempdir().unwrap();
        let store = super::super::artifact::ArtifactStore::new(artifacts.path()).unwrap();
        let ws = ws.without_sandbox().with_artifacts(store);
        let output = bash_tool_streaming(&ws, &json!({
            "command": "printf 'BEGIN\\n'; head -c 1048576 /dev/zero; printf '\\nMIDDLE\\n'; head -c 1048576 /dev/zero; printf '\\nEND\\n'"
        }), 10, &|_| {}, &AtomicBool::new(false)).unwrap();
        assert!(!output.is_error, "{}", output.text);
        assert!(!output.text.contains("MIDDLE"));
        let uri = output
            .text
            .lines()
            .find_map(|line| line.strip_prefix("[complete output: "))
            .unwrap()
            .trim_end_matches(']');
        let bytes = fs::read(
            artifacts
                .path()
                .join(uri.strip_prefix("artifact://").unwrap()),
        )
        .unwrap();
        assert_eq!(bytes.len(), 2 * 1048576 + b"BEGIN\n\nMIDDLE\n\nEND\n".len());
        assert!(bytes.starts_with(b"BEGIN\n") && bytes.ends_with(b"\nEND\n"));
        assert_eq!(&bytes[1048576 + 6..1048576 + 14], b"\nMIDDLE\n");
        assert!(output.text.len() < BASH_HEAD + BASH_TAIL + 400);
    }

    static SIGNAL_CANCEL: AtomicBool = AtomicBool::new(false);

    extern "C" fn cancel_on_sigint(_: libc::c_int) {
        SIGNAL_CANCEL.store(true, Ordering::Release);
        // Lock-free and async-signal-safe: wakes the waiting shell loop immediately.
        crate::platform::wake::signal();
    }

    // Run in its own test process: installing SIGINT must not affect concurrent tests.
    #[cfg(unix)]
    #[test]
    #[ignore]
    fn sigint_shell_helper() {
        let root = std::env::var("DIAL_TEST_SHELL_SIGINT_ROOT")
            .expect("launched by SIGINT regression test");
        let ws = Workspace::new(Path::new(&root)).unwrap().without_sandbox();
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = cancel_on_sigint as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
        }
        let output = bash_tool_streaming(
            &ws,
            &json!({
                "command": "sleep 30 & echo $! > bg.pid; echo $$ > sh.pid; echo READY; wait"
            }),
            30,
            &|bytes| {
                if !bytes.is_empty() {
                    fs::write(ws.root().join("ready"), b"ready").unwrap();
                }
            },
            &SIGNAL_CANCEL,
        )
        .unwrap();
        assert!(
            output.is_error && output.text.contains("cancelled"),
            "{}",
            output.text
        );
    }

    #[cfg(unix)]
    #[test]
    fn sigint_cancellation_kills_shell_and_process_group_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::shell::tests::sigint_shell_helper",
                "--ignored",
            ])
            .env("DIAL_TEST_SHELL_SIGINT_ROOT", dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !dir.path().join("ready").exists() {
            if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
                let _ = child.kill();
                let _ = child.wait();
                panic!("SIGINT helper never produced live output");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let started = Instant::now();
        assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if started.elapsed() > Duration::from_secs(3) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("SIGINT did not promptly cancel the shell");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success(), "SIGINT helper failed: {status}");
        for name in ["sh.pid", "bg.pid"] {
            let pid: i32 = fs::read_to_string(dir.path().join(name))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_ne!(
                unsafe { libc::kill(pid, 0) },
                0,
                "{name} survived cancellation"
            );
        }
    }
}

//! The `bash` tool: platform-native shell execution, process ownership, live output and artifacts.

use super::sandbox::is_secret_env;
#[cfg(not(windows))]
use super::sandbox::{Policy, Sandbox};
use crate::config::types::ShellBackend;
use crate::tools::{ToolOutput, Workspace, opt_bool, opt_u64, req_str};
use crate::workspace_copy::SCRATCH_DIR;
use anyhow::Result;
use serde_json::Value;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
pub mod session;
use session::{SessionClaim, admit, read_published};

const DEFAULT_BASH_TIMEOUT_SECS: u64 = 300;

/// The command's working directory: the primary root by default, or `cwd` / a leading simple
/// `cd path && ...` resolved through [`Workspace::resolve`], so explicitly added roots work and
/// symlink escapes, protected Dial storage and undeclared directories stay refused.
fn working_directory(
    ws: &Workspace,
    args: &Value,
    command: &str,
) -> Result<(std::path::PathBuf, String), String> {
    let mut cwd = ws.root().to_path_buf();
    if let Some(value) = args.get("cwd") {
        let value = value.as_str().ok_or("cwd must be a string")?;
        cwd = ws.resolve(value)?;
        if !cwd.is_dir() {
            return Err("cwd must be an existing directory inside a workspace root".into());
        }
    }
    #[cfg(not(windows))]
    {
        // Retain the bash convenience of accepting an initial `cd path && ...`; PowerShell
        // commands stay untouched and use the explicit `cwd` argument/current directory.
        let trimmed = command.trim_start();
        if let Some(rest) = trimmed.strip_prefix("cd ")
            && let Some((target, script)) = rest.split_once(" && ")
        {
            let target = target.trim();
            if !target.is_empty() && !target.contains(['\'', '"', '`', '$', ';', '|']) {
                // Relative targets are relative to the effective cwd, not the primary root.
                let candidate = cwd.join(target);
                if let Some(path) = candidate
                    .to_str()
                    .and_then(|candidate| ws.resolve(candidate).ok())
                    && path.is_dir()
                {
                    return Ok((path, script.to_owned()));
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
pub(crate) fn release_process(group: ProcessGroup) -> Child {
    group.release()
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
    pub(crate) captured_total: u64,
    /// Complete-output artifact already being written before adoption.
    pub(crate) spool: Option<crate::storage::artifact::ArtifactSpool>,
    /// Named session this command runs in; the claim keeps it busy until an adopted job settles.
    pub(crate) session: Option<SessionClaim>,
}

/// The last lines of output captured before adoption, bounded so the adoption notice stays small.
fn output_tail(bytes: &[u8]) -> String {
    const MAX_BYTES: usize = 2000;
    const MAX_LINES: usize = 20;
    let text = String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(MAX_BYTES)..]);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(MAX_LINES)..].join("\n")
}

/// The moment a foreground command is handed to the background owner, or `None` when it may not be.
/// A zero threshold disables automatic backgrounding, so it gets no deadline. Otherwise the deadline
/// is the configured threshold, whatever the `timeout`: a command still running after it moves to
/// `bg`, so an explicit `timeout` never keeps a command in the foreground past the threshold.
fn adoption_deadline(auto_background: Duration) -> Option<Duration> {
    (!auto_background.is_zero()).then_some(auto_background)
}

/// Whether the timeout kills the command on this pass. A timeout at or after the adoption deadline
/// never kills, because the command is handed to `bg` at the deadline first. Only a timeout shorter
/// than the threshold kills, or any timeout when backgrounding is disabled.
fn timeout_kills(timeout: Duration, elapsed: Duration, adopt_after: Option<Duration>) -> bool {
    !timeout.is_zero() && elapsed >= timeout && adopt_after.is_none_or(|adopt| adopt > timeout)
}

/// Whether this pass should adopt: an explicit background request, steering, or the deadline.
fn adoption_due(
    asynchronous: bool,
    steering: bool,
    elapsed: Duration,
    deadline: Option<Duration>,
) -> bool {
    asynchronous || steering || deadline.is_some_and(|deadline| elapsed >= deadline)
}

/// Hard cancellation outranks adoption on the same pass: a cancelled command is never transferred.
fn may_adopt(cancelled: bool, due: bool) -> bool {
    !cancelled && due
}

/// The parts of a [`ForegroundJob`] that an adopter takes over; the job itself is not dropped.
pub(crate) struct ForegroundParts {
    pub(crate) workspace: Workspace,
    pub(crate) command: String,
    pub(crate) child: Child,
    pub(crate) reader: io::PipeReader,
    pub(crate) captured: Vec<u8>,
    pub(crate) captured_total: u64,
    pub(crate) spool: Option<crate::storage::artifact::ArtifactSpool>,
    pub(crate) session: Option<SessionClaim>,
}

impl ForegroundJob {
    pub(crate) fn into_parts(self) -> ForegroundParts {
        let job = std::mem::ManuallyDrop::new(self);
        // The working directory is not handed over; release it here.
        drop(unsafe { std::ptr::read(&job.cwd) });
        unsafe {
            ForegroundParts {
                workspace: std::ptr::read(&job.workspace),
                command: std::ptr::read(&job.command),
                child: std::ptr::read(&job.child),
                reader: std::ptr::read(&job.reader),
                captured: std::ptr::read(&job.captured),
                captured_total: job.captured_total,
                spool: std::ptr::read(&job.spool),
                session: std::ptr::read(&job.session),
            }
        }
    }
}

impl Drop for ForegroundJob {
    fn drop(&mut self) {
        let _ = crate::platform::process::kill_group(self.child.id());
        let _ = self.child.wait();
    }
}

// Compatibility path for existing process-owning adapters.
pub use crate::runtime::process::ProcessGroup;

/// Keeps the first and last bytes of a stream, counting what was dropped, plus the diagnostic lines
/// (errors, panics, failed tests) found in the dropped middle.
struct Capture {
    head: Vec<u8>,
    tail: Vec<u8>,
    total: usize,
    diagnostics: crate::tools::code::diagnostics::Collector,
}

impl Capture {
    fn new() -> Self {
        Capture {
            head: Vec::new(),
            tail: Vec::new(),
            total: 0,
            diagnostics: crate::tools::code::diagnostics::Collector::new(BASH_DIAGNOSTICS),
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
    spool: &mut Option<crate::storage::artifact::ArtifactSpool>,
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

/// After the process (group) is dead, collects what it wrote. A process that escaped may still hold
/// the pipe, so the wait is bounded by a 300ms grace; missing EOF is reported as a capture error.
#[allow(clippy::too_many_arguments)]
fn drain_after_exit<R: Read + crate::platform::io::Pipe>(
    reader: &mut R,
    capture: &mut Capture,
    spool: &mut Option<crate::storage::artifact::ArtifactSpool>,
    artifact_error: &mut Option<String>,
    capture_error: &mut Option<String>,
    eof: &mut bool,
    waiter: &crate::platform::wake::Waiter,
    on_output: &(dyn Fn(&[u8]) + Sync),
) -> Result<(), String> {
    const GRACE: Duration = Duration::from_millis(300);
    let grace = Instant::now();
    while !*eof && capture_error.is_none() && grace.elapsed() < GRACE {
        match drain_pipe(reader, capture, spool, artifact_error, on_output) {
            Ok(closed) => *eof = closed,
            Err(error) => {
                *capture_error = Some(format!("reading command output: {error}"));
                break;
            }
        }
        if !*eof {
            waiter
                .wait(
                    Some(&*reader),
                    None::<&Child>,
                    Some(GRACE.saturating_sub(grace.elapsed())),
                )
                .map_err(|e| format!("waiting for command output: {e}"))?;
        }
    }
    if !*eof && capture_error.is_none() {
        *capture_error = Some(
            "pipe did not reach EOF within the 300ms cleanup grace; output may be incomplete"
                .to_string(),
        );
    }
    Ok(())
}

/// The captured stream as result text: `(no output)` when empty, always newline-terminated.
fn captured_text(capture: Capture) -> String {
    let mut text = capture.finish();
    if text.is_empty() {
        text.push_str("(no output)");
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

fn output_integrity(
    original: u64,
    inline: u64,
    elided: Option<u64>,
    artifact: Option<String>,
    complete: bool,
    minimized: bool,
) -> crate::runtime::execution::OutputIntegrity {
    use crate::runtime::execution::{Completeness, OutputIntegrity};
    let retained = if artifact.is_some() {
        original.saturating_sub(elided.unwrap_or(0))
    } else {
        inline
    };
    OutputIntegrity {
        capture: if complete {
            Completeness::Complete
        } else {
            Completeness::Partial
        },
        retention: if retained == original {
            Completeness::Complete
        } else {
            Completeness::Partial
        },
        presentation: if original <= inline && !minimized {
            Completeness::Complete
        } else {
            Completeness::Partial
        },
        original_bytes: Some(original),
        retained_bytes: Some(retained),
        omitted_bytes: Some(original.saturating_sub(retained)),
        presented_bytes: (!minimized).then_some(inline),
        artifact,
    }
}

/// Finishes the artifact spool and appends the `[complete|partial output: uri]` block plus any
/// capture/artifact error lines. A failed spool finish becomes the artifact error.
fn append_output_notes(
    text: &mut String,
    spool: Option<crate::storage::artifact::ArtifactSpool>,
    capture_error: &Option<String>,
    artifact_error: &mut Option<String>,
) -> Option<String> {
    let mut artifact = None;
    if let Some(spool) = spool {
        let elided = spool.elided_bytes();
        match spool.finish() {
            Ok(uri) => {
                let completeness = if capture_error.is_some() || elided > 0 {
                    "partial"
                } else {
                    "complete"
                };
                text.push_str(&format!("[{completeness} output: {uri}]\n"));
                artifact = Some(uri);
            }
            Err(error) => {
                *artifact_error = Some(format!("saving complete command output artifact: {error}"));
            }
        }
    }
    if let Some(error) = capture_error {
        text.push_str(&format!("[output capture error: {error}]\n"));
    }
    if let Some(error) = artifact_error {
        text.push_str(&format!(
            "[output artifact error: {error}; inline output retained]\n"
        ));
    }
    artifact
}

/// Replaces the first `body_len` bytes of `text` (the captured output) with their minimized form
/// when `shell.minimize_output` is on and the complete output is saved as an artifact, which the
/// `[complete output: ...]` line that follows the note names. Without that line the raw output
/// would be unrecoverable, so the text is left alone.
fn minimize_body(
    ws: &Workspace,
    command: &str,
    body_len: usize,
    text: &mut String,
    capture_complete: bool,
) -> bool {
    if !capture_complete
        || !ws.minimizing_output()
        || !text[body_len..].contains("[complete output: artifact://")
        || text.contains("[output capture error:")
        || text.contains("[output artifact error:")
    {
        return false;
    }
    let Some(minimized) = super::minimizer::minimize(command, &text[..body_len]) else {
        return false;
    };
    let mut body = minimized.text;
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(&format!(
        "[output filtered ({}): {} -> {} bytes; the complete output is at the artifact below]\n",
        minimized.filter, minimized.before, minimized.after
    ));
    text.replace_range(..body_len, &body);
    true
}

/// Deletes the one-shot script an embedded-shell command line refers to. The host removes it when
/// it runs; a command that is never spawned would otherwise leak it into the scratch directory.
pub(in crate::tools) fn discard_unspawned_script(spawn: &std::process::Command) {
    crate::embedded_shell::discard_script(spawn.get_args());
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

pub(in crate::tools) fn bash_tool(
    ws: &Workspace,
    args: &Value,
    timeout_secs: u64,
) -> Result<ToolOutput, String> {
    bash_tool_streaming(ws, args, timeout_secs, &|_| {}, &AtomicBool::new(false))
}

/// The deadline for one command: `requested` is the model's `timeout` argument, `cap` the
/// configured maximum (0 = no cap and, per the documented contract, no deadline for an omitted
/// timeout). An explicit 0 asks for the cap itself; 0 as result means no deadline.
fn effective_timeout(requested: Option<u64>, cap: u64) -> u64 {
    match requested {
        None => DEFAULT_BASH_TIMEOUT_SECS.min(cap),
        Some(0) => cap,
        Some(requested) if cap == 0 => requested,
        Some(requested) => requested.min(cap),
    }
}

/// Executes the platform-native shell with bounded live chunks, artifact capture, and cooperative cancellation.
/// The callback runs before the final result, on the calling thread, with at most 16 KiB.
/// Cancellation is checked at least once per wait slice (100 ms) and after every pipe-drain pass (at most 256 KiB).
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
    let Some((config, offered)) = ws.bash_interceptor() else {
        return run_bash(
            ws,
            args,
            command,
            timeout_secs,
            auto_background_secs,
            on_output,
            cancel,
            on_adopt,
        );
    };
    use crate::tools::exec::bash_interceptor::{self, Verdict};
    let offered: Vec<&str> = offered.iter().map(String::as_str).collect();
    let hint = match bash_interceptor::check(config, &offered, command) {
        Some(Verdict::Block(message)) => return Ok(ToolOutput::err(message)),
        Some(Verdict::Hint(line)) => Some(line),
        None => None,
    };
    let mut output = run_bash(
        ws,
        args,
        command,
        timeout_secs,
        auto_background_secs,
        on_output,
        cancel,
        on_adopt,
    )?;
    if let Some(line) = hint
        .or_else(|| bash_interceptor::ci_poll_hint(config, &offered, command, &output.text))
        .or_else(|| bash_interceptor::trim_hint(config, command))
    {
        if !output.text.ends_with('\n') {
            output.text.push('\n');
        }
        output.text.push_str(&line);
    }
    Ok(output)
}

#[allow(clippy::too_many_arguments)]
fn run_bash(
    ws: &Workspace,
    args: &Value,
    command: &str,
    timeout_secs: u64,
    auto_background_secs: u64,
    on_output: &(dyn Fn(&[u8]) + Sync),
    cancel: &AtomicBool,
    on_adopt: &(dyn Fn(ForegroundJob) -> Result<String, String> + Sync),
) -> Result<ToolOutput, String> {
    let session = crate::tools::opt_str(args, "session")?;
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
        // A named session may move to the background (explicitly or at the threshold); its state is
        // written back when the job settles. Only the embedded runner keeps that state.
        if runner_for(ws)? != Runner::Embedded {
            return Err("named shell state requires the embedded backend".into());
        }
    }
    let mut sessions = session
        .map(|_| {
            ws.shell_sessions
                .try_lock()
                .map_err(|_| "another named shell command is active in this workspace".to_string())
        })
        .transpose()?;
    if let (Some(name), Some(sessions)) = (session, sessions.as_mut()) {
        admit(ws, sessions, name)?;
    }
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
    if let Some(refusal) = super::git_guard::guard(ws, &command, &cwd) {
        return Ok(refusal);
    }
    if preview_server(&command) {
        let start = serde_json::json!({"action":"start","name":"preview","command":command});
        return Ok(ToolOutput::err(format!(
            "Command not started: preview servers must use the managed background tool `bg`: {start}"
        )));
    }
    let requested_timeout = opt_u64(args, "timeout")?;
    let timeout_secs = effective_timeout(requested_timeout, timeout_secs);

    if cancel.load(Ordering::Acquire) {
        return Ok(ToolOutput::err("[cancelled before command started]\n"));
    }
    // Background requests need a child process, so they take the child-host path below.
    if !asynchronous && session.is_none() && runner_for(ws)? == Runner::InProcess {
        return inprocess::run(ws, &command, &cwd, timeout_secs, on_output, cancel);
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
    let spawned = (|| {
        let (reader, writer) = io::pipe().map_err(|e| format!("creating output pipe: {e}"))?;
        let writer_err = writer
            .try_clone()
            .map_err(|e| format!("creating output pipe: {e}"))?;
        spawn.stdin(Stdio::null()).stdout(writer).stderr(writer_err);
        let child = crate::platform::process::spawn_group(&mut spawn)
            .map_err(|e| format!("starting sandboxed shell: {e}"))?;
        Ok::<_, String>((reader, child))
    })();
    let (mut reader, child) = match spawned {
        Ok(started) => started,
        Err(error) => {
            discard_unspawned_script(&spawn);
            return Err(error);
        }
    };
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
    let adopt_after = adoption_deadline(auto_background);
    let waiter = crate::platform::wake::Waiter::new()
        .map_err(|e| format!("creating command wait channel: {e}"))?;
    loop {
        // Hard cancellation is read once per pass and outranks adoption: a cancelled command stays
        // with this owner, and the group kill below reaps it.
        let cancel_now = cancel.load(Ordering::Acquire);
        let adoption_wanted = adoption_due(
            asynchronous,
            crate::tools::exec::signals::steering_requested(),
            started.elapsed(),
            adopt_after,
        );
        if may_adopt(cancel_now, adoption_wanted) {
            crate::platform::io::set_blocking(&reader)
                .map_err(|e| format!("restoring adopted output pipe mode: {e}"))?;
            let mut captured = capture.retained();
            if captured.len() < capture.total {
                // Only a contiguous suffix can be placed in bg's offset-addressed ring.
                captured = capture.tail.clone();
            }
            let so_far = output_tail(&captured);
            let job = ForegroundJob {
                workspace: ws.clone(),
                command: command.clone(),
                cwd: cwd.clone(),
                child: release_process(group),
                reader,
                captured,
                captured_total: capture.total as u64,
                spool: spool.take(),
                session: session
                    .zip(exchange)
                    .map(|(name, exchange)| SessionClaim::new(ws, name, exchange)),
            };
            return on_adopt(job).map(|id| {
                let so_far = if so_far.is_empty() {
                    String::new()
                } else {
                    format!("Output so far:\n{so_far}\n")
                };
                let mut output = ToolOutput::ok(format!(
                    "command is running as background job {id} (after {}s): {command}\n{so_far}Keep working; you are notified when it finishes. bg wait returns its output when it exits; stop it with bg kill. Do not poll it; continue independent work or use a bounded bg wait.",
                    started.elapsed().as_secs()
                ));
                // Structured so the controller can follow the job without parsing this prose.
                output.data = Some(serde_json::json!({"background_job": id}));
                output
            });
        }
        if cancel_now {
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
        if timeout_kills(timeout, started.elapsed(), adopt_after) {
            timed_out = true;
            break;
        }
        // Block until output arrives, the leader exits, a cancel is signalled, or a deadline
        // passes; the loop re-checks each condition on every return.
        // Cap each slice so a cancel flag stored without a wake signal is still seen promptly.
        let slice = Duration::from_millis(100);
        let mut limit = (!timeout.is_zero()).then(|| timeout.saturating_sub(started.elapsed()));
        if let Some(adopt_after) = adopt_after {
            let until = adopt_after.saturating_sub(started.elapsed());
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
    // Everything in the group is dead; collect what it wrote.
    drain_after_exit(
        &mut reader,
        &mut capture,
        &mut spool,
        &mut artifact_error,
        &mut capture_error,
        &mut eof,
        &waiter,
        on_output,
    )?;

    use crate::runtime::execution::Completeness;
    let original = capture.total as u64;
    let inline_retained = (capture.head.len() + capture.tail.len().min(BASH_TAIL)) as u64;
    let elided = spool.as_ref().map(|spool| spool.elided_bytes());
    let mut text = captured_text(capture);
    let body_len = text.len();
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
    let artifact = append_output_notes(&mut text, spool, &capture_error, &mut artifact_error);
    let minimized = minimize_body(
        ws,
        &command,
        body_len,
        &mut text,
        eof && capture_error.is_none() && original == inline_retained,
    );
    let integrity = output_integrity(
        original,
        inline_retained,
        elided,
        artifact,
        eof && capture_error.is_none(),
        minimized,
    );
    let mut state_error = None;
    if let (Some(name), Some(sessions), Some(exchange)) =
        (session, sessions.as_mut(), exchange.as_ref())
    {
        sessions.remove(name);
        if !cancelled && !timed_out && capture_error.is_none() {
            match read_published(ws, exchange) {
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
        output_complete: capture_error.is_none()
            && artifact_error.is_none()
            && eof
            && integrity.retention == Completeness::Complete,
    });
    let output = output.with_output_integrity(integrity);
    // The group was killed and its leader reaped above, so a timeout is a known (failed)
    // outcome the model can inspect and retry; only a user cancel stays uncertain.
    Ok(if cancelled {
        output
            .with_status(crate::tools::ToolStatus::Aborted)
            .with_effects(crate::tools::surface::outcome::Effects::Unknown)
    } else if timed_out {
        output.with_status(crate::tools::ToolStatus::Timeout)
    } else {
        output
    })
}

/// Build the platform shell command in `cwd`. POSIX commands use the workspace sandbox policy or
/// its explicit unsandboxed opt-out; Windows PowerShell requires that opt-out because native
/// filesystem/network isolation is unavailable. Stdio and process ownership belong to the caller.
pub(in crate::tools) fn shell_command(
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
    /// Dial's built-in shell run inside this process; allowed only with `sandbox.mode = "off"`.
    InProcess,
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
        ShellBackend::InProcess if embedded_built => Ok(Runner::InProcess),
        ShellBackend::InProcess => Err(
            "the in-process shell is not in this build (the embedded-shell feature is disabled); set shell.backend to \"system\""
                .into(),
        ),
        ShellBackend::Embedded if embedded_built => Ok(Runner::Embedded),
        ShellBackend::Embedded => Err(
            "the built-in shell is not in this build (the embedded-shell feature is disabled); set shell.backend to \"system\""
                .into(),
        ),
    }
}

/// The runner for this workspace. The in-process shell shares Dial's address space, credentials
/// and file access, so it is refused unless the operator turned the OS sandbox off; it never
/// falls back to another backend or runs beside a configured sandbox.
fn runner_for(ws: &Workspace) -> Result<Runner, String> {
    let runner = resolve_runner(
        ws.shell_backend,
        find_system_bash(),
        cfg!(feature = "embedded-shell"),
        cfg!(windows),
    )?;
    if runner == Runner::InProcess && !ws.unsandboxed {
        return Err(
            "shell.backend is \"inprocess\", which runs commands inside the Dial process and is allowed only when sandbox.mode is \"off\"; nothing was run. Set sandbox.mode to \"off\", or set shell.backend to \"auto\", \"system\" or \"embedded\""
                .into(),
        );
    }
    Ok(runner)
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

/// Writes `script` for the built-in shell and returns the host program and its arguments.
fn embedded_invocation(
    scratch: &Path,
    script: &str,
    state: Option<&Path>,
) -> Result<(String, Vec<String>), String> {
    let (program, mut args) = crate::embedded_shell::invocation(scratch, script)
        .map_err(|e| format!("preparing the built-in shell: {e}"))?;
    let prepared = (|| {
        if let Some(state) = state {
            args.push(
                state
                    .to_str()
                    .ok_or("shell state path is not UTF-8")?
                    .to_owned(),
            );
        }
        Ok::<_, String>(
            program
                .to_str()
                .ok_or("Dial executable path is not UTF-8")?
                .to_owned(),
        )
    })();
    match prepared {
        Ok(program) => Ok((program, args)),
        Err(error) => {
            crate::embedded_shell::discard_script(&args);
            Err(error)
        }
    }
}

/// Background PTYs alone retain a new private terminal session through the sandbox.
pub(in crate::tools) fn shell_command_mode(
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
    let runner = runner_for(ws)?;
    // Background jobs and tool helpers need a child process; they run the same shell in a child
    // host (only reachable with the sandbox off, which `runner_for` verified).
    let runner = if runner == Runner::InProcess {
        Runner::Embedded
    } else {
        runner
    };
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
            // `InProcess` was mapped to `Embedded` above; both run the child shell host here.
            Runner::Embedded | Runner::InProcess => {
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
        crate::tools::exec::cargo_target::apply(&mut spawn, ws.cargo_target_dir());
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
                crate::tools::exec::kernel::shell_quote(cwd_str)
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
            Runner::Embedded | Runner::InProcess => embedded_invocation(&scratch, script, state)?,
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
            let built = if let Some(fd) = owned_terminal {
                sandbox.command_with_owned_terminal(&ws.root, &policy, &program, &arg_refs, fd)
            } else {
                sandbox.command_with_policy(&ws.root, &policy, &program, &arg_refs)
            };
            spawn = match built {
                Ok(spawn) => spawn,
                Err(error) => {
                    crate::embedded_shell::discard_script(&args);
                    return Err(error);
                }
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
        crate::tools::exec::cargo_target::apply(&mut spawn, ws.cargo_target_dir());
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

mod inprocess;

#[cfg(test)]
mod backend_tests;

#[cfg(all(test, unix))]
mod tests;

#[cfg(all(test, unix))]
mod inprocess_tests;

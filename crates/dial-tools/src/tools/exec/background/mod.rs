//! The `bg` tool: long-running processes (dev servers, watchers) that outlive a single call.
//!
//! Commands use `shell_command_mode`: POSIX commands use bash's sandbox policy and scrubbed
//! environment; Windows commands use PowerShell only with explicit unsandboxed opt-in. Owned groups
//! (or private PTY sessions) are retained across turns without global shell state.
//! Stdout/stderr are spooled to artifact storage before the bounded display ring. Artifact failures
//! are reported without interrupting a live process; pipe read failures stop the process group.
//! `output`/`wait` and heartbeat notifications use independent cursors, so UI updates do not
//! consume model-visible output. Limits: eight running processes and 256 KiB retained for
//! immediate display per process. `kill`, tool/session drop and observed leader exit kill the
//! owned process group or Windows Job Object. POSIX descendants that make a new group/session can
//! escape group cleanup; Linux's sandbox PID namespace additionally contains them. Abrupt host
//! termination may prevent cleanup. Kill and drop stop each reader, which drains for at most 300ms
//! and then closes Dial's read side. An open pipe/PTY is therefore left behind only as an
//! explicitly incomplete saved artifact: an escaped descendant keeps running, but it does not keep
//! Dial's reader, spool or ring alive.
//! Input is capped at 64 KiB per call, cancellable and optionally deadline-bound; partial bytes
//! cannot be rolled back. Pipes close explicitly for EOF. Canonical PTYs receive VEOF while
//! keeping output open; raw PTYs have no half-close. Port readiness is network-approved and
//! requires lsof-proven owned listeners; Linux sandbox namespace ownership fails closed.

pub mod durable;
mod input;
mod monitor;
mod readiness;
mod table;
mod wake;
pub use wake::notify_cancel;
#[cfg(all(test, unix))]
mod tests;

use crate::config::types::Config;
use crate::runtime::execution::{Completeness, ObservationKind, OutputIntegrity, ProcessOutcome};
use crate::tools::integrations::ext::{JobEntry, JobsView, Tool, ToolContext, ToolEvent};
use crate::tools::{
    ToolOutput, Workspace, opt_bool, opt_str, opt_u64, present, req_str, tool_schema,
};
use readiness::Ready;
use regex::{Regex, RegexBuilder};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
pub use table::CellJob;
use table::{Attachment, Ring, Snapshot, State, Table, kill_all_entries, kill_entry, lock};

/// A foreground job transferred to the managed background owner: a shell command that outlived its
/// wait, or a native eval cell moved off its controller.
pub enum AdoptedJob {
    Shell(Box<crate::tools::exec::shell::ForegroundJob>),
    Cell(CellJob),
}

const MAX_COMMAND: usize = 64 * 1024;
const MAX_PATTERN: usize = 512;
const MAX_INPUT: usize = 64 * 1024;
/// Most bytes one `output` call returns; the rest of a larger burst is reported as omitted.
const MAX_OUTPUT: usize = 32 * 1024;
const MAX_MATCH_LINE: usize = 300;
const MAX_LISTED_COMMAND: usize = 80;
const DEFAULT_WAIT_SECS: u64 = 5;
const MAX_WAIT_SECS: u64 = 300;

const DESCRIPTION: &str = "Session-owned background jobs without persistent shell state. POSIX \
commands use bash's sandbox, cwd and network policy; Windows uses PowerShell only with explicit \
unsandboxed opt-in. Output is kept in artifacts plus a 256 KiB ring (PTY: raw bytes, rendered screen \
snapshot); at most 32 KiB inline. `start`: command, optional unique name/cwd, pty=true (rows/cols) or \
stdin=true for a pipe, else stdin is null. `input`: name, text (exact UTF-8, no automatic newline, \
max 64 KiB); `resize`: name, rows/cols; `close_stdin`: pipe EOF or PTY VEOF (raw PTYs cannot \
half-close); their timeout defaults 0 (unlimited, cancellable) and partial writes are reported. \
`list`; `output` (pipe: delta; PTY: screen; optional tail); `wait` (until regex, or exit); `kill` \
(name or all=true). `ready` on named start/wait: log regex and/or numeric host/port, both required if \
given; port readiness needs network permission and approval and lsof-proven owned listeners (POSIX \
only). Waits default 5s; 0 is unlimited, positive max 300s. At most 8 live and 8 finished entries. \
persist=true: Unix-only named finite-lifetime pipe jobs that survive restarts (no PTY/stdin/ready); \
pass it on every durable call. Session exit kills owned process groups or Windows Job Objects; POSIX \
descendants that escape with setsid/job control may survive.";

/// Owns every background process; dropping it kills them all (each entry kills its process
/// group when dropped).
pub struct Background {
    table: Mutex<Table>,
    interrupt: AtomicBool,
    /// False under `--no-pty`: `start` rejects `pty=true`.
    pty_allowed: bool,
}

/// The tools this module contributes, for `Registry::standard`.
pub fn tools(config: &Config) -> Vec<Arc<dyn Tool>> {
    let background = Arc::new(Background {
        table: Mutex::new(Table::default()),
        interrupt: AtomicBool::new(false),
        pty_allowed: !config.shell.no_pty,
    });
    vec![
        background.clone(),
        Arc::new(monitor::Monitor::new(Some(background))),
    ]
}

impl Background {
    /// Write keyboard bytes to a running PTY job from the interactive UI.
    pub fn attach_user_pty(&self, name: &str, bytes: &[u8]) -> Result<usize, String> {
        static NO_CANCEL: AtomicBool = AtomicBool::new(false);
        if bytes.len() > MAX_INPUT {
            return Err(format!("interactive PTY input exceeds {MAX_INPUT} bytes"));
        }
        let input = {
            let mut table = self.table();
            table.refresh();
            let index = table
                .procs
                .iter()
                .position(|proc| proc.name == name)
                .ok_or_else(|| format!("no background process named {name:?}"))?;
            let proc = &table.procs[index];
            if !proc.is_running() || !proc.terminal {
                return Err(format!("{name:?} is not a running PTY job"));
            }
            proc.input
                .clone()
                .ok_or_else(|| format!("{name:?} has no PTY input"))?
        };
        input.write(bytes, None, &NO_CANCEL, &NO_CANCEL)
    }

    /// Job table and the attached job's output. Pipe views never consume the tool-output cursor.
    pub fn jobs_view(&self, attached: Option<&str>) -> JobsView {
        let mut table = self.table();
        table.refresh();
        let jobs = table
            .procs
            .iter()
            .map(|proc| JobEntry {
                name: proc.name.clone(),
                command: proc.command.clone(),
                status: proc.state.label(),
                running: proc.is_running(),
                terminal: proc.terminal,
            })
            .collect();
        let (screen, cursor) = attached
            .and_then(|name| table.procs.iter().find(|proc| proc.name == name))
            .map(|proc| {
                let ring = lock(&proc.ring);
                let rows = ring
                    .screen_rows()
                    .unwrap_or_else(|| ring.text().lines().map(str::to_owned).collect());
                (rows, ring.screen_cursor())
            })
            .unwrap_or_default();
        JobsView {
            jobs,
            screen,
            cursor,
        }
    }

    fn table(&self) -> MutexGuard<'_, Table> {
        lock(&self.table)
    }

    fn dispatch(
        &self,
        ws: &Workspace,
        args: &Value,
        on_event: Option<&(dyn Fn(ToolEvent) + Sync)>,
        cancel: &AtomicBool,
    ) -> Result<ToolOutput, String> {
        if opt_bool(args, "persist")? {
            return durable::dispatch(ws, args, cancel, &self.interrupt);
        }
        if present(args, "lifetime") {
            return Err("lifetime requires explicit persist=true".into());
        }
        match req_str(args, "action")? {
            "start" => self.start(ws, args, on_event, cancel),
            "list" => Ok(self.list()),
            "output" => self.output(ws, args),
            "wait" => self.wait(ws, args, on_event, cancel, Ready::parse(args)?),
            "input" | "close_stdin" | "resize" => self.input(args, cancel),
            "kill" => self.kill(args),
            other => Err(format!(
                "unknown action {other:?}; use start, list, output, wait, input, close_stdin, resize or kill"
            )),
        }
    }

    fn start(
        &self,
        ws: &Workspace,
        args: &Value,
        on_event: Option<&(dyn Fn(ToolEvent) + Sync)>,
        cancel: &AtomicBool,
    ) -> Result<ToolOutput, String> {
        let command = req_str(args, "command")?;
        if command.trim().is_empty() {
            return Err("command must not be empty".to_string());
        }
        if command.len() > MAX_COMMAND {
            return Err(format!("command is longer than {MAX_COMMAND} bytes"));
        }
        let ready = Ready::parse(args)?;
        if ready.is_some() && opt_str(args, "name")?.is_none() {
            return Err("named service readiness requires an explicit name".into());
        }
        if ready.is_some() {
            if present(args, "until") {
                return Err("use either until or ready.log, not both".into());
            }
            opt_u64(args, "timeout")?;
        }
        if ready.as_ref().is_some_and(|ready| ready.port.is_some()) && !network_allowed(ws) {
            return Err("port readiness is denied by the workspace's network policy".into());
        }
        #[cfg(target_os = "linux")]
        if ready.as_ref().is_some_and(|ready| ready.port.is_some()) && !ws.unsandboxed {
            return Err("port readiness cannot prove listener ownership across the Linux sandbox PID namespace; use ready.log instead".into());
        }
        let pty = opt_bool(args, "pty")?;
        if pty && !self.pty_allowed {
            return Err("pty is disabled by --no-pty; start without pty=true".into());
        }
        let stdin = opt_bool(args, "stdin")?;
        let rows = dimension(args, "rows", 24)?;
        let cols = dimension(args, "cols", 80)?;
        let cwd = match opt_str(args, "cwd")? {
            Some(dir) => ws.resolve(dir)?,
            None => ws.root.clone(),
        };
        if !cwd.is_dir() {
            return Err(format!("cwd {} is not a directory", ws.rel(&cwd)));
        }
        if let Some(refusal) = crate::tools::exec::git_guard::guard(ws, command, &cwd) {
            return Ok(refusal);
        }
        let (name, pid) = self.table().start(
            ws,
            opt_str(args, "name")?,
            command,
            &cwd,
            Attachment {
                terminal: pty,
                stdin,
                rows,
                cols,
            },
        )?;
        let started = format!("started {name} (pid {pid}) in {}: {command}", ws.rel(&cwd));
        if ready.is_some() {
            let mut wait_args = args.clone();
            wait_args["name"] = Value::String(name);
            let mut output = self
                .wait(ws, &wait_args, on_event, cancel, ready)
                .map_err(|error| format!("{error}\n{started}; process remains managed by bg"))?;
            output.text.push_str(&format!("\n{started}"));
            Ok(output)
        } else {
            Ok(ToolOutput::ok(started))
        }
    }

    fn input(&self, args: &Value, cancel: &AtomicBool) -> Result<ToolOutput, String> {
        let name = req_str(args, "name")?;
        let input = self.table().input(name)?;
        let timeout = super::kernel::deadline(args, 0)?;
        let end = super::kernel::execution_deadline(timeout);
        let action = req_str(args, "action")?;
        let result = match action {
            "input" => {
                let text = req_str(args, "text")?;
                if text.len() > MAX_INPUT {
                    return Err(format!("input exceeds {MAX_INPUT} bytes"));
                }
                let written = input.write(text.as_bytes(), end, cancel, &self.interrupt)?;
                format!("wrote {written} bytes to {name}")
            }
            "close_stdin" => format!("{name}: {}", input.close(end, cancel, &self.interrupt)?),
            "resize" => {
                let rows = dimension(args, "rows", 24)?;
                let cols = dimension(args, "cols", 80)?;
                let pending = self.table().resize(name, rows, cols)?;
                if let Some(pending) = pending {
                    pending.finish(end, cancel, &self.interrupt)?;
                }
                format!("resized {name}")
            }
            _ => unreachable!(),
        };
        Ok(ToolOutput::ok(result))
    }

    fn list(&self) -> ToolOutput {
        let mut table = self.table();
        table.refresh();
        if table.procs.is_empty() {
            return ToolOutput::ok("no background processes".to_string());
        }
        let mut text = format!(
            "{} running, {} finished\n",
            table.running(),
            table.procs.len() - table.running()
        );
        for proc in &table.procs {
            let bytes = lock(&proc.ring).total;
            let command: String = proc.command.chars().take(MAX_LISTED_COMMAND).collect();
            let ellipsis = if command.len() < proc.command.len() {
                "…"
            } else {
                ""
            };
            text.push_str(&format!(
                "{}  pid {}  {}{}  up {}s  {bytes} bytes of output  $ {command}{ellipsis}\n",
                proc.name,
                proc.pid,
                proc.state.label(),
                if proc.terminal { " PTY" } else { "" },
                proc.uptime(),
            ));
        }
        ToolOutput::ok(text)
    }

    fn output(&self, ws: &Workspace, args: &Value) -> Result<ToolOutput, String> {
        let name = req_str(args, "name")?;
        let tail = match opt_u64(args, "tail")? {
            Some(0) => return Err("tail must be at least 1 line".to_string()),
            Some(lines) => Some(lines as usize),
            None => None,
        };
        let snap = self.table().snapshot(name)?;
        snap.settle();
        let (body, lost, integrity) = take_presented(ws, &snap, tail);
        let is_error = lock(&snap.ring).error.is_some();
        Ok(ToolOutput {
            execution: Default::default(),
            data: None,
            text: render_output(&snap, body, lost, tail),
            is_error,
        }
        .with_observation(ObservationKind::Snapshot, !is_error)
        .with_process(process_outcome(&snap))
        .with_output_integrity(integrity))
    }

    fn wait(
        &self,
        ws: &Workspace,
        args: &Value,
        on_event: Option<&(dyn Fn(ToolEvent) + Sync)>,
        cancel: &AtomicBool,
        ready: Option<Ready>,
    ) -> Result<ToolOutput, String> {
        let name = req_str(args, "name")?;
        let secs = match opt_u64(args, "timeout")? {
            Some(0) => 0,
            Some(secs) => secs.min(MAX_WAIT_SECS),
            None => DEFAULT_WAIT_SECS,
        };
        if ready.is_some() && present(args, "until") {
            return Err("use either until or ready.log, not both".into());
        }
        if ready.as_ref().is_some_and(|ready| ready.port.is_some()) && !network_allowed(ws) {
            return Err("port readiness is denied by the workspace's network policy".into());
        }
        #[cfg(target_os = "linux")]
        if ready.as_ref().is_some_and(|ready| ready.port.is_some()) && !ws.unsandboxed {
            return Err("port readiness cannot prove listener ownership across the Linux sandbox PID namespace; use ready.log instead".into());
        }
        let service = ready.is_some();
        let (until, port, ready_timeout) = match ready {
            Some(ready) => (ready.log, ready.port, ready.timeout),
            None => (
                opt_str(args, "until")?.map(compile).transpose()?,
                None,
                None,
            ),
        };
        let secs = ready_timeout.unwrap_or(secs);
        let deadline = super::kernel::execution_deadline(Duration::from_secs(secs));
        let mut scanned = None;
        let mut matched = None;
        let mut next_probe = Instant::now();
        let initial_pid = self.table().snapshot(name)?.pid;
        loop {
            let seen = wake::generation();
            if let Some(on_event) = on_event {
                let events = self.table().drain_notifications();
                for event in events {
                    on_event(event);
                }
            }
            let hard_cancel = cancel.load(Ordering::Acquire);
            if hard_cancel
                || self.interrupt.load(Ordering::Acquire)
                || super::signals::wait_interrupted()
            {
                self.interrupt.store(false, Ordering::Release);
                // Waiting only observes: the job keeps running under bg either way, so neither
                // ending has effects to inspect. Unknown effects would stop the turn with a
                // misleading "uncertain effects" report and refuse further calls.
                if !hard_cancel && super::signals::steering_requested() {
                    return Ok(ToolOutput::ok(format!(
                        "stopped waiting for {name} because a new user message arrived; {name} is still running under bg (use output or wait to check it again)"
                    ))
                    .with_effects(crate::tools::surface::outcome::Effects::None));
                }
                return Ok(ToolOutput::err(format!(
                    "cancelled waiting for {name}; background process remains managed by bg"
                ))
                .with_status(crate::tools::ToolStatus::Aborted)
                .with_effects(crate::tools::surface::outcome::Effects::None));
            }
            let snap = self.table().snapshot(name)?;
            if snap.pid != initial_pid {
                return Err(format!("not ready: {name} was replaced while waiting"));
            }
            if port.is_some() && !snap.network {
                return Err(
                    "port readiness denied: this job was launched without network permission"
                        .into(),
                );
            }
            let exited = !matches!(snap.state, State::Running);
            if exited {
                snap.settle();
            }
            if matched.is_none()
                && let Some(pattern) = &until
            {
                matched = find_match(&snap.ring, snap.terminal, pattern, &mut scanned);
            }
            let mut port_ok = false;
            if let Some(address) = port
                && !exited
                && Instant::now() >= next_probe
            {
                port_ok =
                    readiness::port_ready(snap.pid, address, deadline, cancel, &self.interrupt)?;
                next_probe = Instant::now() + Duration::from_millis(250);
                let current = self.table().snapshot(name)?;
                if current.pid != initial_pid || !matches!(current.state, State::Running) {
                    return Err(format!(
                        "not ready: {name} exited or was replaced during its port probe"
                    ));
                }
            }
            let conditions = until.is_some() || port.is_some();
            if conditions
                && (!service || !exited)
                && until.as_ref().is_none_or(|_| matched.is_some())
                && port.is_none_or(|_| port_ok && !exited)
            {
                let detail = match (&until, &matched, port) {
                    (Some(pattern), Some(line), None) => {
                        format!("output matched /{pattern}/\n> {line}")
                    }
                    (Some(pattern), Some(line), Some(address)) => format!(
                        "owned listener accepting at {address} and output matched /{pattern}/\n> {line}"
                    ),
                    (_, _, Some(address)) => format!("owned listener accepting at {address}"),
                    _ => unreachable!(),
                };
                return Ok(ToolOutput::ok(format!(
                    "ready: {name} {detail}\n[{}]",
                    snap.state.label()
                ))
                .with_observation(ObservationKind::Readiness, true)
                .with_process(process_outcome(&snap)));
            }
            if exited {
                let state = snap.state.label();
                // The exit result carries the unread output, so a finished build or test needs no
                // separate `output` round trip. Shares the model-visible cursor with `output`.
                let (body, lost, integrity) = take_presented(ws, &snap, None);
                let output = render_body(&snap, body, lost, None);
                let observation_satisfied = !conditions && lock(&snap.ring).error.is_none();
                return Ok(if conditions {
                    ToolOutput::err(format!(
                        "not ready: {name} {state} before {} was established\n{output}",
                        until.as_ref().map_or_else(
                            || "port readiness".into(),
                            |pattern| format!(
                                "/{pattern}/{}",
                                if port.is_some() {
                                    " and port readiness"
                                } else {
                                    ""
                                }
                            )
                        )
                    ))
                } else {
                    let text = format!("ready: {name} {state}\n{output}");
                    if lock(&snap.ring).error.is_some() {
                        ToolOutput::err(text)
                    } else {
                        ToolOutput::ok(text)
                    }
                }
                .with_observation(
                    if conditions {
                        ObservationKind::Readiness
                    } else {
                        ObservationKind::Wait
                    },
                    observation_satisfied,
                )
                .with_process(process_outcome(&snap))
                .with_output_integrity(integrity));
            }
            if deadline.is_some_and(|end| Instant::now() >= end) {
                return Ok(ToolOutput::ok(format!(
                    "not ready: timed out after {secs}s; {name} is still running"
                ))
                .with_observation(
                    if conditions {
                        ObservationKind::Readiness
                    } else {
                        ObservationKind::Wait
                    },
                    false,
                )
                .with_process(process_outcome(&snap)));
            }
            let mut wake_at = deadline;
            if port.is_some() {
                wake_at = Some(wake_at.map_or(next_probe, |end| end.min(next_probe)));
            }
            wake::wait_past(seen, wake_at);
        }
    }

    fn kill(&self, args: &Value) -> Result<ToolOutput, String> {
        let all = opt_bool(args, "all")?;
        match (opt_str(args, "name")?, all) {
            (Some(name), false) => {
                let state = kill_entry(&self.table, name)?;
                Ok(ToolOutput::ok(format!(
                    "killed {name} (was {})",
                    state.label()
                )))
            }
            (None, true) => {
                let running = kill_all_entries(&self.table);
                Ok(ToolOutput::ok(format!(
                    "killed {running} running process(es); table cleared"
                )))
            }
            _ => Err("kill needs either `name` or `all: true`".to_string()),
        }
    }
}

fn network_allowed(ws: &Workspace) -> bool {
    match &ws.policy {
        Some(super::sandbox::Policy::Strict { allow_network, .. })
        | Some(super::sandbox::Policy::Dev { allow_network, .. }) => *allow_network,
        None => ws.allow_network,
    }
}

fn dimension(args: &Value, field: &str, default: u16) -> Result<u16, String> {
    let value = opt_u64(args, field)?.unwrap_or(default as u64);
    u16::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("{field} must be in 1..=65535"))
}

fn compile(pattern: &str) -> Result<Regex, String> {
    if pattern.len() > MAX_PATTERN {
        return Err(format!("until pattern is longer than {MAX_PATTERN} bytes"));
    }
    RegexBuilder::new(pattern)
        .size_limit(1 << 20)
        .multi_line(true)
        .build()
        .map_err(|e| format!("until is not a valid regex: {e}"))
}

/// Plain text of a terminal byte stream: escape sequences and control characters are removed,
/// CRLF becomes LF and a bare CR (a redraw) is dropped, so patterns match what a reader sees.
fn plain_terminal(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    // CSI parameters end at a final byte in 0x40..=0x7e.
                    for n in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&n) {
                            break;
                        }
                    }
                }
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    chars.next();
                    // OSC, DCS, SOS, PM and APC end at BEL or ST (ESC \).
                    while let Some(n) = chars.next() {
                        if n == '\x07' {
                            break;
                        }
                        if n == '\x1b' {
                            chars.next_if_eq(&'\\');
                            break;
                        }
                    }
                }
                // Charset designators (ESC ( B, ESC # 8, ...) carry one more byte.
                Some('(' | ')' | '#' | '%') => {
                    chars.next();
                    chars.next();
                }
                // Any other escape is two bytes, e.g. ESC 7 or ESC M.
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// The line holding the first match in the retained output, scanning only if it grew since
/// `scanned`. Terminal output is matched as plain text (see [`plain_terminal`]).
fn find_match(
    ring: &Mutex<Ring>,
    terminal: bool,
    pattern: &Regex,
    scanned: &mut Option<u64>,
) -> Option<String> {
    let text = {
        let ring = lock(ring);
        if *scanned == Some(ring.total) {
            return None;
        }
        *scanned = Some(ring.total);
        let raw = ring.text();
        if terminal { plain_terminal(&raw) } else { raw }
    };
    let found = pattern.find(&text)?;
    let start = text[..found.start()].rfind('\n').map_or(0, |i| i + 1);
    let end = text[found.end()..]
        .find('\n')
        .map_or(text.len(), |i| found.end() + i);
    Some(text[start..end].chars().take(MAX_MATCH_LINE).collect())
}

/// The last `n` lines of `text`, trailing newline included.
fn last_lines(text: &str, n: usize) -> &str {
    let trimmed = text.strip_suffix('\n').unwrap_or(text);
    let mut start = trimmed.len();
    for _ in 0..n {
        match trimmed[..start].rfind('\n') {
            Some(i) => start = i,
            None => return text,
        }
    }
    &text[start + 1..]
}

/// Filter only a complete, previously unread exited pipe result, including failed commands.
/// A delta must never be interpreted by a filter as the whole build/test output. Stay within the
/// bounded ring; larger captures still use the existing capped delta plus artifact reference.
fn take_presented(
    ws: &Workspace,
    snap: &Snapshot,
    tail: Option<usize>,
) -> (String, u64, OutputIntegrity) {
    let minimized = if ws.minimizing_output()
        && !snap.terminal
        && tail.is_none()
        && matches!(
            snap.state,
            State::Exited {
                status: Some(_),
                ..
            }
        ) {
        let mut ring = lock(&snap.ring);
        let minimized = if ring.cursor == 0 {
            minimize_complete(&snap.command, &ring)
        } else {
            None
        };
        if minimized.is_some() {
            ring.cursor = ring.total;
        }
        minimized
    } else {
        None
    };
    if let Some(minimized) = minimized {
        let mut integrity = output_integrity(snap, &minimized.text, 0, None);
        integrity.presentation = Completeness::Partial;
        let mut body = minimized.text;
        if !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&format!(
            "[output filtered ({}): {} -> {} bytes; the complete output is at the artifact below]\n",
            minimized.filter, minimized.before, minimized.after
        ));
        return (body, 0, integrity);
    }
    let (body, lost) = take_unread(snap);
    let integrity = output_integrity(snap, &body, lost, tail);
    (body, lost, integrity)
}

/// Shared by explicit reads and completion notices. Never classify a delta or reread an artifact;
/// the bounded ring must still contain the complete, valid UTF-8 capture backed by an artifact.
fn minimize_complete(command: &str, ring: &Ring) -> Option<super::minimizer::Minimized> {
    if !ring.closed
        || !ring.reached_eof
        || ring.error.is_some()
        || ring.elided_bytes != 0
        || ring.artifact.is_none()
        || ring.retained_bytes() != ring.total
    {
        return None;
    }
    let (bytes, _) = ring.since(0);
    let body = String::from_utf8(bytes).ok()?;
    super::minimizer::minimize(command, &body)
}

/// Model-visible output not yet returned by `output` or `wait`, advancing their shared cursor.
/// A terminal's model-facing output is its rendered screen, a snapshot not a delta.
fn take_unread(snap: &Snapshot) -> (String, u64) {
    let mut ring = lock(&snap.ring);
    let fresh = match ring.screen_text() {
        Some(screen) => (screen, 0),
        None => {
            // Preserve a UTF-8 lookbehind before render_body applies its character-safe cap.
            let (bytes, lost) = ring.recent_since(ring.cursor, MAX_OUTPUT + 3);
            let text = String::from_utf8(bytes)
                .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned());
            (text, lost)
        }
    };
    ring.cursor = ring.total;
    fresh
}

fn process_outcome(snap: &Snapshot) -> ProcessOutcome {
    let status = match &snap.state {
        State::Exited { status, .. } => *status,
        State::Running => None,
    };
    let ring = lock(&snap.ring);
    ProcessOutcome {
        started: true,
        exit_code: status.and_then(|status| status.code()),
        signal: status
            .as_ref()
            .and_then(crate::platform::process::exit_signal),
        timed_out: false,
        cancelled: false,
        output_complete: ring.closed
            && ring.reached_eof
            && ring.error.is_none()
            && ring.elided_bytes == 0,
    }
}

fn output_integrity(
    snap: &Snapshot,
    body: &str,
    lost: u64,
    tail: Option<usize>,
) -> OutputIntegrity {
    let body = tail.map_or(body, |lines| last_lines(body, lines));
    let mut cut = body.len().saturating_sub(MAX_OUTPUT);
    while !body.is_char_boundary(cut) {
        cut += 1;
    }
    let presented = (body.len() - cut) as u64;
    let ring = lock(&snap.ring);
    let retained = if ring.artifact.is_some() {
        ring.total.saturating_sub(ring.elided_bytes)
    } else {
        ring.retained_bytes()
    };
    OutputIntegrity {
        capture: if !ring.closed {
            Completeness::Unknown
        } else if ring.reached_eof {
            Completeness::Complete
        } else {
            Completeness::Partial
        },
        retention: if retained == ring.total {
            Completeness::Complete
        } else {
            Completeness::Partial
        },
        presentation: if !snap.terminal && presented == ring.total && lost == 0 {
            Completeness::Complete
        } else {
            Completeness::Partial
        },
        original_bytes: Some(ring.total),
        retained_bytes: Some(retained),
        omitted_bytes: Some(ring.total.saturating_sub(retained)),
        presented_bytes: (!snap.terminal).then_some(presented),
        artifact: ring.artifact.clone(),
    }
}

fn render_output(snap: &Snapshot, body: String, lost: u64, tail: Option<usize>) -> String {
    format!(
        "[{}: {}, pid {}]\n{}",
        snap.name,
        snap.state.label(),
        snap.pid,
        render_body(snap, body, lost, tail)
    )
}

fn render_body(snap: &Snapshot, mut body: String, lost: u64, tail: Option<usize>) -> String {
    if let Some(lines) = tail {
        let skip = body.len() - last_lines(&body, lines).len();
        body.drain(..skip);
    }
    let mut omitted = lost;
    if body.len() > MAX_OUTPUT {
        let mut cut = body.len() - MAX_OUTPUT;
        while !body.is_char_boundary(cut) {
            cut += 1;
        }
        omitted += cut as u64;
        body.drain(..cut);
    }
    let mut text = String::new();
    if body.is_empty() {
        text.push_str(if snap.terminal {
            "(blank screen)\n"
        } else {
            "(no new output)\n"
        });
    } else {
        text.push_str(&body);
        if !body.ends_with('\n') {
            text.push('\n');
        }
    }
    if omitted > 0 {
        text.push_str(&format!(
            "[{omitted} bytes of earlier output were discarded]\n"
        ));
    }
    let ring = lock(&snap.ring);
    if let Some(uri) = &ring.artifact {
        let completeness = if ring.error.is_some() || ring.elided_bytes > 0 || !ring.reached_eof {
            "partial"
        } else {
            "complete"
        };
        text.push_str(&format!("[{completeness} output: {uri}]\n"));
    }
    if let Some(error) = &ring.error {
        text.push_str(&format!("[output capture error: {error}]\n"));
    }
    text
}

impl Tool for Background {
    fn name(&self) -> &str {
        "bg"
    }
    fn attach_user_pty(&self, name: &str, bytes: &[u8]) -> Result<usize, String> {
        Background::attach_user_pty(self, name, bytes)
    }
    fn jobs_view(&self, attached: Option<&str>) -> Option<JobsView> {
        Some(Background::jobs_view(self, attached))
    }
    fn kill_job(&self, name: &str) -> Result<String, String> {
        let state = kill_entry(&self.table, name)?;
        Ok(format!("killed {name} (was {})", state.label()))
    }

    fn schema(&self) -> Value {
        tool_schema(
            "bg",
            DESCRIPTION,
            json!({
                "action": {"type": "string", "enum": ["start", "list", "output", "wait", "input", "close_stdin", "resize", "kill"]},
                "command": {"type": "string", "description": "start: shell command to run"},
                "name": {"type": "string", "description": "start: optional unique name (letters, digits, - _ .); output/wait/kill: which process"},
                "cwd": {"type": "string", "description": "start: working directory relative to the primary workspace root, or absolute inside it or an added workspace root (default: primary root)"},
                "persist": {"type": "boolean", "description": "Explicit opt-in: Unix pipe-only job owned beyond controller exit; use true on every start/list/output/wait/kill call. Named start required; no PTY/stdin/ready. Default false, ordinary session lifecycle unchanged."},
                "lifetime": {"type": "integer", "minimum": 1, "maximum": 86400, "description": "persist start only: maximum lifetime in seconds (default 3600); owner stops process at deadline"},
                "pty": {"type": "boolean", "description": "start: allocate a private controlling terminal (default false); also enables input"},
                "stdin": {"type": "boolean", "description": "start without PTY: keep a stdin pipe open (default false)"},
                "rows": {"type": "integer", "minimum": 1, "maximum": 65535, "description": "PTY start/resize rows (default 24)"},
                "cols": {"type": "integer", "minimum": 1, "maximum": 65535, "description": "PTY start/resize columns (default 80)"},
                "text": {"type": "string", "description": "input: exact UTF-8 bytes (max 64 KiB), no automatic newline; can contain terminal control characters"},
                "ready": {"type": "object", "additionalProperties": false, "description": "named start/wait: log and/or port; both conditions if both supplied", "properties": {
                    "log": {"type": "string", "description": "regex in this process's retained output"},
                    "host": {"type": "string", "description": "numeric IP address (default 127.0.0.1), requires port"},
                    "port": {"type": "integer", "minimum": 1, "maximum": 65535, "description": "owned TCP listener readiness; network permission/approval and lsof required"},
                    "timeout": {"type": "integer", "minimum": 0, "maximum": 300, "description": "seconds, explicit 0 unlimited (default outer timeout or 5s)"}
                }},
                "tail": {"type": "integer", "minimum": 1, "description": "output: return only the last N lines of the new output"},
                "timeout": {"type": "integer", "minimum": 0, "maximum": 3600, "description": "wait: seconds (default 5, explicit 0 unlimited, positive max 300); input/close_stdin/resize: 0 unlimited cancellable, positive max 3600"},
                "until": {"type": "string", "description": "wait: regex to wait for in the output; ^ and $ match at line boundaries (default: wait for exit)"},
                "all": {"type": "boolean", "description": "kill: kill every background process"},
            }),
            &["action"],
        )
    }

    fn needs_approval(&self, args: &Value) -> bool {
        match args.get("action").and_then(Value::as_str) {
            Some("list" | "output") => false,
            Some("wait") => args
                .get("ready")
                .is_some_and(|ready| present(ready, "port")),
            _ => true,
        }
    }
    fn concurrency(&self, args: &Value) -> crate::tools::integrations::ext::Concurrency {
        if matches!(args["action"].as_str(), Some("wait" | "list" | "output")) {
            // The job table synchronizes these reads; a wait must not lock out independent edits.
            crate::tools::integrations::ext::Concurrency::Shared
        } else {
            crate::tools::integrations::ext::Concurrency::Exclusive
        }
    }

    fn fork_session(&self) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self {
            table: Mutex::new(Table::default()),
            interrupt: AtomicBool::new(false),
            pty_allowed: self.pty_allowed,
        }))
    }

    fn cancel_operation(&self) {
        self.interrupt.store(true, Ordering::Release);
        wake::notify_cancel();
    }

    fn interruptible(&self, args: &Value) -> bool {
        args["action"] == "wait" || (args["action"] == "start" && args.get("ready").is_some())
    }

    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        if self.interrupt.swap(false, Ordering::AcqRel) {
            return ToolOutput::err("cancelled before background action");
        }
        let output = self
            .dispatch(ctx.workspace, args, None, &AtomicBool::new(false))
            .unwrap_or_else(ToolOutput::err);
        self.interrupt.store(false, Ordering::Release);
        output
    }
    fn adopt_job(&self, job: AdoptedJob) -> Result<String, String> {
        match job {
            AdoptedJob::Shell(job) => self.table().adopt(*job),
            AdoptedJob::Cell(job) => self.table().adopt_cell(job),
        }
    }

    fn call_streaming(
        &self,
        ctx: &ToolContext,
        args: &Value,
        on_event: &(dyn Fn(ToolEvent) + Sync),
        cancel: &AtomicBool,
    ) -> ToolOutput {
        let interrupted = self.interrupt.swap(false, Ordering::AcqRel);
        if cancel.load(Ordering::Acquire) || interrupted {
            return ToolOutput::err("cancelled before background action");
        }
        let output = self
            .dispatch(ctx.workspace, args, Some(on_event), cancel)
            .unwrap_or_else(ToolOutput::err);
        // A cancel that arrived after this call began belongs to it, not to the next call.
        self.interrupt.store(false, Ordering::Release);
        let events = self.table().drain_notifications();
        for event in events {
            on_event(event);
        }
        output
    }

    fn drain_notifications(&self) -> Vec<ToolEvent> {
        self.table().drain_notifications()
    }

    fn has_live_work(&self) -> bool {
        let mut table = self.table();
        table.refresh();
        table.running() != 0
    }

    fn awaited_work(&self) -> Vec<String> {
        let mut table = self.table();
        table.refresh();
        table.awaited()
    }
}

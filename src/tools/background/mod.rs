//! The `bg` tool: long-running processes (dev servers, watchers) that outlive a single call.
//!
//! Commands run exactly as `bash` runs them (same sandbox policy, scrubbed environment and
//! process-group handling, via `shell_command`), but stay running while the agent keeps working.
//! Complete stdout/stderr are spooled to artifact storage before the bounded display ring.
//! `output`/`wait` and heartbeat notifications use independent cursors, so UI updates do not
//! consume model-visible output. Limits: eight running processes and 256 KiB retained for
//! immediate display per process. Every process is killed with its whole group by `kill`, when the
//! tool (and so the registry) is dropped, or when its leader exits. A SIGKILL of dial itself
//! cannot run destructors; the orphan's output pipe then has no reader, so a process that keeps
//! writing dies of SIGPIPE, but a silent one survives.

mod table;
#[cfg(test)]
mod tests;

use super::ext::{Tool, ToolContext, ToolEvent};
use super::{ToolOutput, Workspace, opt_bool, opt_str, opt_u64, req_str, tool_schema};
use crate::config::types::Config;
use regex::{Regex, RegexBuilder};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, MutexGuard};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use table::{Ring, Snapshot, State, Table, lock};

const MAX_COMMAND: usize = 64 * 1024;
const MAX_PATTERN: usize = 512;
/// Most bytes one `output` call returns; the rest of a larger burst is reported as omitted.
const MAX_OUTPUT: usize = 32 * 1024;
const MAX_MATCH_LINE: usize = 300;
const MAX_LISTED_COMMAND: usize = 80;
const DEFAULT_WAIT_SECS: u64 = 30;
const MAX_WAIT_SECS: u64 = 300;
const POLL: Duration = Duration::from_millis(25);

const DESCRIPTION: &str = "Run long-lived processes (dev servers, watchers, tail -f) in the background and \
check on them later. Commands run exactly like `bash` (same sandbox, workspace-only writes, network only if \
bash has it) in their own process group; stdout+stderr are captured. Actions: `start` (command, optional name, \
optional cwd inside the workspace), `list`, `output` (name; returns only output new since the last `output` \
call, optionally just its last `tail` lines; at most 32 KiB inline, complete logs saved as artifacts), \
`wait` (name, optional timeout seconds and `until` regex; returns ready when the regex shows up in the output, \
or, without `until`, when the process exits), `kill` (name, or all=true). At most 8 run at once. They are all \
killed when dial exits.";

/// Owns every background process; dropping it kills them all (each entry kills its process
/// group when dropped).
pub struct Background {
    table: Mutex<Table>,
}

/// The tools this module contributes, for `Registry::standard`.
pub fn tools(_config: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(Background { table: Mutex::new(Table::default()) })]
}

impl Background {
    fn table(&self) -> MutexGuard<'_, Table> {
        lock(&self.table)
    }

    fn dispatch(&self, ws: &Workspace, args: &Value) -> Result<ToolOutput, String> {
        match req_str(args, "action")? {
            "start" => self.start(ws, args),
            "list" => Ok(self.list()),
            "output" => self.output(args),
            "wait" => self.wait(args, None, &AtomicBool::new(false)),
            "kill" => self.kill(args),
            other => Err(format!("unknown action {other:?}; use start, list, output, wait or kill")),
        }
    }

    fn start(&self, ws: &Workspace, args: &Value) -> Result<ToolOutput, String> {
        let command = req_str(args, "command")?;
        if command.trim().is_empty() {
            return Err("command must not be empty".to_string());
        }
        if command.len() > MAX_COMMAND {
            return Err(format!("command is longer than {MAX_COMMAND} bytes"));
        }
        let cwd = match opt_str(args, "cwd")? {
            Some(dir) => ws.resolve(dir)?,
            None => ws.root.clone(),
        };
        if !cwd.is_dir() {
            return Err(format!("cwd {} is not a directory", ws.rel(&cwd)));
        }
        let (name, pid) = self.table().start(ws, opt_str(args, "name")?, command, &cwd)?;
        Ok(ToolOutput::ok(format!("started {name} (pid {pid}) in {}: {command}", ws.rel(&cwd))))
    }

    fn list(&self) -> ToolOutput {
        let mut table = self.table();
        table.refresh();
        if table.procs.is_empty() {
            return ToolOutput::ok("no background processes".to_string());
        }
        let mut text = format!("{} running, {} finished\n", table.running(), table.procs.len() - table.running());
        for proc in &table.procs {
            let bytes = lock(&proc.ring).total;
            let command: String = proc.command.chars().take(MAX_LISTED_COMMAND).collect();
            let ellipsis = if command.len() < proc.command.len() { "…" } else { "" };
            text.push_str(&format!(
                "{}  pid {}  {}  up {}s  {bytes} bytes of output  $ {command}{ellipsis}\n",
                proc.name,
                proc.pid,
                proc.state.label(),
                proc.uptime(),
            ));
        }
        ToolOutput::ok(text)
    }

    fn output(&self, args: &Value) -> Result<ToolOutput, String> {
        let name = req_str(args, "name")?;
        let tail = match opt_u64(args, "tail")? {
            Some(0) => return Err("tail must be at least 1 line".to_string()),
            Some(lines) => Some(lines as usize),
            None => None,
        };
        let snap = self.table().snapshot(name)?;
        snap.settle();
        let (bytes, lost) = {
            let mut ring = lock(&snap.ring);
            let fresh = ring.since(ring.cursor);
            ring.cursor = ring.total;
            fresh
        };
        let is_error = lock(&snap.ring).error.is_some();
        Ok(ToolOutput { text: render_output(&snap, &bytes, lost, tail), is_error })
    }

    fn wait(
        &self,
        args: &Value,
        on_event: Option<&(dyn Fn(ToolEvent) + Sync)>,
        cancel: &AtomicBool,
    ) -> Result<ToolOutput, String> {
        let name = req_str(args, "name")?;
        let secs = match opt_u64(args, "timeout")? {
            Some(0) => return Err("timeout must be at least 1 second".to_string()),
            Some(secs) => secs.min(MAX_WAIT_SECS),
            None => DEFAULT_WAIT_SECS,
        };
        let until = opt_str(args, "until")?.map(compile).transpose()?;
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut scanned = None;
        loop {
            if let Some(on_event) = on_event {
                for event in self.table().drain_notifications() { on_event(event); }
            }
            if cancel.load(Ordering::Acquire) {
                return Ok(ToolOutput::err(format!("cancelled waiting for {name}; background process remains managed by bg")));
            }
            let snap = self.table().snapshot(name)?;
            let exited = !matches!(snap.state, State::Running);
            if exited {
                snap.settle();
            }
            if let Some(pattern) = &until {
                if let Some(line) = find_match(&snap.ring, pattern, &mut scanned) {
                    return Ok(ToolOutput::ok(format!(
                        "ready: {name} output matched /{pattern}/\n> {line}\n[{}]",
                        snap.state.label()
                    )));
                }
            }
            if exited {
                let state = snap.state.label();
                return Ok(match until {
                    None => ToolOutput::ok(format!("ready: {name} {state}")),
                    Some(pattern) => {
                        ToolOutput::err(format!("not ready: {name} {state} before /{pattern}/ appeared in its output"))
                    }
                });
            }
            if Instant::now() >= deadline {
                return Ok(ToolOutput::ok(format!("not ready: timed out after {secs}s; {name} is still running")));
            }
            std::thread::sleep(POLL);
        }
    }

    fn kill(&self, args: &Value) -> Result<ToolOutput, String> {
        let all = opt_bool(args, "all")?;
        match (opt_str(args, "name")?, all) {
            (Some(name), false) => {
                let state = self.table().kill(name)?;
                Ok(ToolOutput::ok(format!("killed {name} (was {})", state.label())))
            }
            (None, true) => {
                let running = self.table().kill_all();
                Ok(ToolOutput::ok(format!("killed {running} running process(es); table cleared")))
            }
            _ => Err("kill needs either `name` or `all: true`".to_string()),
        }
    }
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

/// The line holding the first match in the retained output, scanning only if it grew since
/// `scanned`.
fn find_match(ring: &Mutex<Ring>, pattern: &Regex, scanned: &mut Option<u64>) -> Option<String> {
    let text = {
        let ring = lock(ring);
        if *scanned == Some(ring.total) {
            return None;
        }
        *scanned = Some(ring.total);
        ring.text()
    };
    let found = pattern.find(&text)?;
    let start = text[..found.start()].rfind('\n').map_or(0, |i| i + 1);
    let end = text[found.end()..].find('\n').map_or(text.len(), |i| found.end() + i);
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

fn render_output(snap: &Snapshot, bytes: &[u8], lost: u64, tail: Option<usize>) -> String {
    let mut body = String::from_utf8_lossy(bytes).into_owned();
    if let Some(lines) = tail {
        body = last_lines(&body, lines).to_owned();
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
    let mut text = format!("[{}: {}, pid {}]\n", snap.name, snap.state.label(), snap.pid);
    if body.is_empty() {
        text.push_str("(no new output)\n");
    } else {
        text.push_str(&body);
        if !body.ends_with('\n') {
            text.push('\n');
        }
    }
    if omitted > 0 {
        text.push_str(&format!("[{omitted} bytes of earlier output were discarded]\n"));
    }
    let ring = lock(&snap.ring);
    if let Some(uri) = &ring.artifact {
        text.push_str(&format!("[complete output: {uri}]\n"));
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

    fn schema(&self) -> Value {
        tool_schema(
            "bg",
            DESCRIPTION,
            json!({
                "action": {"type": "string", "enum": ["start", "list", "output", "wait", "kill"]},
                "command": {"type": "string", "description": "start: shell command to run"},
                "name": {"type": "string", "description": "start: optional unique name (letters, digits, - _ .); output/wait/kill: which process"},
                "cwd": {"type": "string", "description": "start: working directory inside the workspace (default: its root)"},
                "tail": {"type": "integer", "minimum": 1, "description": "output: return only the last N lines of the new output"},
                "timeout": {"type": "integer", "minimum": 1, "description": "wait: seconds to wait (default 30, max 300)"},
                "until": {"type": "string", "description": "wait: regex to wait for in the output; ^ and $ match at line boundaries (default: wait for exit)"},
                "all": {"type": "boolean", "description": "kill: kill every background process"},
            }),
            &["action"],
        )
    }

    fn needs_approval(&self, args: &Value) -> bool {
        !matches!(args.get("action").and_then(Value::as_str), Some("list" | "output" | "wait"))
    }

    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        self.dispatch(ctx.workspace, args).unwrap_or_else(ToolOutput::err)
    }

    fn call_streaming(
        &self,
        ctx: &ToolContext,
        args: &Value,
        on_event: &(dyn Fn(ToolEvent) + Sync),
        cancel: &AtomicBool,
    ) -> ToolOutput {
        if cancel.load(Ordering::Acquire) {
            return ToolOutput::err("cancelled before background action");
        }
        let output = if args.get("action").and_then(Value::as_str) == Some("wait") {
            self.wait(args, Some(on_event), cancel)
        } else {
            self.dispatch(ctx.workspace, args)
        }.unwrap_or_else(ToolOutput::err);
        for event in self.table().drain_notifications() { on_event(event); }
        output
    }

    fn drain_notifications(&self) -> Vec<ToolEvent> {
        self.table().drain_notifications()
    }
}

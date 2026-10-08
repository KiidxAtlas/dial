//! The process table behind `bg`: full artifact capture, bounded display rings and delta events.
//!
//! Every process runs in its own process group and is killed with the whole group when its
//! [`Proc`] is dropped (see `ProcessGroup`), so removing an entry, dropping the table and
//! dropping the tool all reap everything they started.

use crate::platform::process::{CommandExt, ExitStatusExt};
use crate::tools::Workspace;
use crate::tools::artifact::ArtifactSpool;
use crate::tools::ext::ToolEvent;
use crate::tools::shell::{ProcessGroup, shell_command};
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Running processes allowed at once.
pub(super) const MAX_PROCESSES: usize = 8;
/// Exited processes kept (for their output) before the oldest is forgotten.
const MAX_FINISHED: usize = 8;
/// Output retained per process; older bytes are discarded.
pub(super) const MAX_BUFFER: usize = 256 * 1024;
const MAX_NAME: usize = 32;
/// How long after exit the reader may still be draining the pipe.
const SETTLE: Duration = Duration::from_millis(300);

pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The last `cap` bytes a process wrote, plus how much the caller has already been shown.
pub(super) struct Ring {
    data: VecDeque<u8>,
    cap: usize,
    /// Bytes ever written (absolute offset one past the newest byte).
    pub(super) total: u64,
    /// Absolute offset the next `output` call starts from.
    pub(super) cursor: u64,
    /// The writer side hit EOF: nothing more will arrive.
    pub(super) closed: bool,
    pub(super) artifact: Option<String>,
    pub(super) error: Option<String>,
}

impl Ring {
    pub(super) fn new(cap: usize) -> Self {
        Ring {
            data: VecDeque::new(),
            cap,
            total: 0,
            cursor: 0,
            closed: false,
            artifact: None,
            error: None,
        }
    }

    pub(super) fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len() as u64;
        if chunk.len() >= self.cap {
            self.data.clear();
            self.data.extend(&chunk[chunk.len() - self.cap..]);
        } else {
            self.data.extend(chunk);
            let excess = self.data.len().saturating_sub(self.cap);
            self.data.drain(..excess);
        }
    }

    /// Absolute offset of the oldest retained byte.
    fn start(&self) -> u64 {
        self.total - self.data.len() as u64
    }

    /// Retained bytes from absolute offset `from` on, and how many bytes after `from` were
    /// already discarded.
    pub(super) fn since(&self, from: u64) -> (Vec<u8>, u64) {
        let start = self.start();
        let lost = start.saturating_sub(from);
        let skip = from.max(start) - start;
        (self.data.range(skip as usize..).copied().collect(), lost)
    }

    /// Everything retained, as text.
    pub(super) fn text(&self) -> String {
        let (bytes, _) = self.since(0);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// Spool every byte before clipping. Nonblocking reads let drop stop escaped pipe holders.
fn spawn_reader(
    mut pipe: io::PipeReader,
    ring: Arc<Mutex<Ring>>,
    stop: Arc<AtomicBool>,
    mut spool: Option<ArtifactSpool>,
) -> io::Result<JoinHandle<()>> {
    let mut pipe = crate::platform::pipe::Reader::new(pipe)?;
    std::thread::Builder::new()
        .name("dial-bg-reader".into())
        .spawn(move || {
            let mut buffer = [0u8; 16 * 1024];
            let mut error = None;
            let mut stopping = None;
            loop {
                if stop.load(Ordering::Acquire) {
                    let since = stopping.get_or_insert_with(Instant::now);
                    if since.elapsed() >= SETTLE {
                        break;
                    }
                }
                match pipe.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        if let Some(spool) = &mut spool
                            && let Err(e) = spool.write_all(&buffer[..count])
                        {
                            error = Some(format!("capturing complete output: {e}"));
                            break;
                        }
                        lock(&ring).push(&buffer[..count]);
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        if stop.load(Ordering::Acquire) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => {
                        error = Some(format!("reading background output: {e}"));
                        break;
                    }
                }
            }
            let artifact = spool.map(ArtifactSpool::finish).transpose();
            let mut ring = lock(&ring);
            match artifact {
                Ok(uri) => ring.artifact = uri,
                Err(e) => error = Some(format!("saving complete output artifact: {e}")),
            }
            ring.error = error;
            ring.closed = true;
        })
}

#[derive(Clone)]
pub(super) enum State {
    Running,
    Exited { at: Instant, detail: String },
}

impl State {
    pub(super) fn label(&self) -> String {
        match self {
            State::Running => "running".to_string(),
            State::Exited { detail, .. } => format!("exited ({detail})"),
        }
    }
}

fn describe(status: ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit code {code}"),
        (None, Some(signal)) => format!("killed by signal {signal}"),
        (None, None) => "exited".to_string(),
    }
}

pub(super) struct Proc {
    pub(super) name: String,
    pub(super) command: String,
    pub(super) pid: u32,
    group: ProcessGroup,
    pub(super) ring: Arc<Mutex<Ring>>,
    started: Instant,
    pub(super) state: State,
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    notice_cursor: u64,
    completion_notified: bool,
}

impl Proc {
    fn spawn(ws: &Workspace, name: String, command: &str, cwd: &Path) -> Result<Proc, String> {
        let spool = ws
            .artifacts()
            .map(|store| store.spool())
            .transpose()
            .map_err(|e| format!("creating background output artifact: {e}"))?;
        let mut spawn = shell_command(ws, command, cwd)?;
        let (reader, writer) = io::pipe().map_err(|e| format!("creating output pipe: {e}"))?;
        let writer_err = writer
            .try_clone()
            .map_err(|e| format!("creating output pipe: {e}"))?;
        spawn
            .stdin(Stdio::null())
            .stdout(writer)
            .stderr(writer_err)
            .process_group(0);
        let child = crate::platform::process::spawn(spawn)
            .map_err(|e| format!("starting sandboxed bash: {e}"))?;
        // The Command owns the pipe's write ends; release them so EOF is observable.

        let pid = child.id();
        let group = ProcessGroup::new(child);
        let ring = Arc::new(Mutex::new(Ring::new(MAX_BUFFER)));
        // On failure `group` drops here and kills what was just started.
        let stop = Arc::new(AtomicBool::new(false));
        let reader = spawn_reader(reader, ring.clone(), stop.clone(), spool)
            .map_err(|e| format!("starting output reader: {e}"))?;
        Ok(Proc {
            name,
            command: command.to_owned(),
            pid,
            group,
            ring,
            started: Instant::now(),
            state: State::Running,
            stop,
            reader: Some(reader),
            notice_cursor: 0,
            completion_notified: false,
        })
    }

    pub(super) fn is_running(&self) -> bool {
        matches!(self.state, State::Running)
    }

    /// Notice that the leader exited. The rest of its group is killed with it (the same rule
    /// `bash` applies), so a finished entry never leaves stray members behind.
    fn refresh(&mut self) {
        if !self.is_running() {
            return;
        }
        let capture_error = lock(&self.ring).error.clone();
        let failure = match (capture_error, self.group.exited()) {
            (Some(error), _) => Some(error),
            (None, Ok(false)) => return,
            (None, Ok(true)) => None,
            (None, Err(e)) => Some(format!("status unavailable: {e}")),
        };
        self.group.terminate();
        self.stop.store(true, Ordering::Release);
        let detail = match (failure, self.group.child.wait()) {
            (Some(detail), _) => detail,
            (None, Ok(status)) => describe(status),
            (None, Err(e)) => format!("status unavailable: {e}"),
        };
        self.state = State::Exited {
            at: Instant::now(),
            detail,
        };
    }

    /// Seconds the process has been (or was) up.
    pub(super) fn uptime(&self) -> u64 {
        let end = match self.state {
            State::Running => Instant::now(),
            State::Exited { at, .. } => at,
        };
        end.duration_since(self.started).as_secs()
    }

    fn drain_notifications(&mut self) -> Vec<ToolEvent> {
        let proc = self;
        let mut events = Vec::new();
        let ring = lock(&proc.ring);
        if ring.total > proc.notice_cursor {
            let from = proc.notice_cursor.max(ring.total.saturating_sub(16 * 1024));
            let (bytes, lost) = ring.since(from);
            let omitted = from.saturating_sub(proc.notice_cursor) + lost;
            let mut text = String::from_utf8_lossy(&bytes).into_owned();
            if omitted > 0 {
                text.insert_str(
                    0,
                    &format!("[{omitted} earlier live bytes omitted; complete log in artifact]\n"),
                );
            }
            proc.notice_cursor = ring.total;
            events.push(ToolEvent {
                kind: "output".into(),
                name: proc.name.clone(),
                text,
                artifact: ring.artifact.clone(),
            });
        }
        if !proc.is_running() && ring.closed && !proc.completion_notified {
            proc.completion_notified = true;
            let mut text = proc.state.label();
            if let Some(error) = &ring.error {
                text.push_str(&format!("; {error}"));
            }
            events.push(ToolEvent {
                kind: "complete".into(),
                name: proc.name.clone(),
                text,
                artifact: ring.artifact.clone(),
            });
        }
        events
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.group.terminate();
        self.stop.store(true, Ordering::Release);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// What a caller needs from an entry without holding the table lock.
pub(super) struct Snapshot {
    pub(super) name: String,
    pub(super) pid: u32,
    pub(super) ring: Arc<Mutex<Ring>>,
    pub(super) state: State,
}

impl Snapshot {
    /// After an exit, give the reader a moment to drain what the process wrote last.
    pub(super) fn settle(&self) {
        let State::Exited { at, .. } = self.state else {
            return;
        };
        while !lock(&self.ring).closed && at.elapsed() < SETTLE {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn validate_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if valid {
        Ok(())
    } else {
        Err(format!(
            "name must be 1-{MAX_NAME} characters of letters, digits, '-', '_' or '.'"
        ))
    }
}

#[derive(Default)]
pub(super) struct Table {
    pub(super) procs: Vec<Proc>,
    started: u64,
    notices: VecDeque<ToolEvent>,
}

impl Table {
    pub(super) fn refresh(&mut self) {
        self.procs.iter_mut().for_each(Proc::refresh);
    }

    fn notice(&mut self, event: ToolEvent) {
        // Bounded metadata queue; output is generated only when the heartbeat drains.
        if self.notices.len() >= 64 {
            self.notices.pop_front();
        }
        self.notices.push_back(event);
    }

    pub(super) fn drain_notifications(&mut self) -> Vec<ToolEvent> {
        self.refresh();
        let mut events: Vec<_> = self.notices.drain(..).collect();
        for proc in &mut self.procs {
            events.extend(proc.drain_notifications());
        }
        events
    }

    fn position(&self, name: &str) -> Option<usize> {
        self.procs.iter().position(|p| p.name == name)
    }

    pub(super) fn running(&self) -> usize {
        self.procs.iter().filter(|p| p.is_running()).count()
    }

    fn fresh_name(&mut self) -> String {
        loop {
            self.started += 1;
            let name = format!("bg{}", self.started);
            if self.position(&name).is_none() {
                return name;
            }
        }
    }

    /// Forget the oldest exited entries so at most `MAX_FINISHED - 1` remain.
    fn prune_finished(&mut self) {
        while self.procs.iter().filter(|p| !p.is_running()).count() >= MAX_FINISHED {
            if let Some(i) = self.procs.iter().position(|p| !p.is_running()) {
                let name = self.procs[i].name.clone();
                let _ = self.kill(&name);
            }
        }
    }

    /// Start `command` as `name` (or a generated name). Returns the name and pid.
    pub(super) fn start(
        &mut self,
        ws: &Workspace,
        name: Option<&str>,
        command: &str,
        cwd: &Path,
    ) -> Result<(String, u32), String> {
        self.refresh();
        let name = match name {
            Some(name) => {
                validate_name(name)?;
                name.to_owned()
            }
            None => self.fresh_name(),
        };
        if self
            .position(&name)
            .is_some_and(|i| self.procs[i].is_running())
        {
            return Err(format!(
                "a process named {name:?} is already running; kill it or pick another name"
            ));
        }
        if self.running() >= MAX_PROCESSES {
            return Err(format!(
                "{MAX_PROCESSES} background processes are already running; kill one first"
            ));
        }
        let proc = Proc::spawn(ws, name.clone(), command, cwd)?;
        // Only now that the spawn worked is a finished entry of the same name replaced.
        if self.position(&name).is_some() {
            let _ = self.kill(&name);
        }
        self.prune_finished();
        let pid = proc.pid;
        self.procs.push(proc);
        self.notice(ToolEvent {
            kind: "start".into(),
            name: name.clone(),
            text: format!("started (pid {pid})"),
            artifact: None,
        });
        Ok((name, pid))
    }

    /// Current state of `name` for use outside the lock.
    pub(super) fn snapshot(&mut self, name: &str) -> Result<Snapshot, String> {
        self.refresh();
        let Some(i) = self.position(name) else {
            let known: Vec<&str> = self.procs.iter().map(|p| p.name.as_str()).collect();
            return Err(if known.is_empty() {
                format!("no background process named {name:?}; none exist")
            } else {
                format!(
                    "no background process named {name:?}; known: {}",
                    known.join(", ")
                )
            });
        };
        let proc = &self.procs[i];
        Ok(Snapshot {
            name: proc.name.clone(),
            pid: proc.pid,
            ring: proc.ring.clone(),
            state: proc.state.clone(),
        })
    }

    /// Kill `name` (group and all) and forget it. Returns what state it was in.
    pub(super) fn kill(&mut self, name: &str) -> Result<State, String> {
        self.snapshot(name)?;
        let i = self
            .position(name)
            .ok_or_else(|| format!("no background process named {name:?}"))?;
        let mut proc = self.procs.remove(i);
        let state = proc.state.clone();
        proc.group.terminate();
        proc.stop.store(true, Ordering::Release);
        if let Some(reader) = proc.reader.take() {
            let _ = reader.join();
        }
        if proc.is_running() {
            proc.state = State::Exited {
                at: Instant::now(),
                detail: "killed; process group stopped".into(),
            };
        }
        for event in proc.drain_notifications() {
            self.notice(event);
        }
        Ok(state)
    }

    /// Kill everything; returns how many entries were running.
    pub(super) fn kill_all(&mut self) -> usize {
        self.refresh();
        let running = self.running();
        let names: Vec<_> = self.procs.iter().map(|p| p.name.clone()).collect();
        for name in names {
            let _ = self.kill(&name);
        }
        running
    }
}

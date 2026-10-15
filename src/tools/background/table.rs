//! The process table behind `bg`: artifact capture, bounded display rings and delta events.
//!
//! Every process runs in its own process group and is killed with the whole group when its
//! [`Proc`] is dropped (see `ProcessGroup`), so removing an entry, dropping the table and
//! dropping the tool all reap everything they started.

use super::input::{self, Handle, Input};
use super::wake;
use crate::tools::Workspace;
use crate::tools::artifact::ArtifactSpool;
use crate::tools::ext::ToolEvent;
use crate::tools::shell::{ProcessGroup, shell_command_mode};
use std::collections::VecDeque;
use std::io::{self, Read, Write};
#[cfg(not(windows))]
use std::os::fd::AsRawFd;
#[cfg(not(windows))]
use std::os::unix::process::CommandExt;
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
///
/// Terminal (PTY) processes also feed every byte through a terminal emulator, so the model can
/// be shown the rendered screen rather than raw escape sequences.
pub(super) struct Ring {
    data: VecDeque<u8>,
    cap: usize,
    /// Bytes ever written (absolute offset one past the newest byte).
    pub(super) total: u64,
    /// Absolute offset the next `output` call starts from.
    pub(super) cursor: u64,
    /// The output reader has finished, including artifact finalization.
    pub(super) closed: bool,
    pub(super) artifact: Option<String>,
    pub(super) error: Option<String>,
    /// A pipe read failure requires stopping the process; an artifact failure does not.
    read_failed: bool,
    /// Terminal emulator for PTY output; `None` for pipes and adopted processes.
    screen: Option<vt100::Parser>,
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
            read_failed: false,
            screen: None,
        }
    }

    /// A ring whose bytes are also rendered by a `rows` x `cols` terminal emulator.
    pub(super) fn terminal(cap: usize, rows: u16, cols: u16) -> Self {
        Ring {
            screen: Some(vt100::Parser::new(rows.max(1), cols.max(1), 0)),
            ..Ring::new(cap)
        }
    }

    pub(super) fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len() as u64;
        if let Some(screen) = self.screen.as_mut() {
            screen.process(chunk);
        }
        if chunk.len() >= self.cap {
            self.data.clear();
            self.data.extend(&chunk[chunk.len() - self.cap..]);
        } else {
            let excess = self
                .data
                .len()
                .saturating_add(chunk.len())
                .saturating_sub(self.cap);
            self.data.drain(..excess);
            self.data.extend(chunk);
        }
    }

    /// Resize the emulated screen after the PTY was resized.
    pub(super) fn resize_screen(&mut self, rows: u16, cols: u16) {
        if let Some(screen) = self.screen.as_mut() {
            screen.set_size(rows.max(1), cols.max(1));
        }
    }

    /// The rendered current screen as plain text, trailing blank rows removed. `None` when this
    /// ring is not a terminal.
    pub(super) fn screen_text(&self) -> Option<String> {
        let screen = self.screen.as_ref()?.screen();
        let text = screen.contents();
        Some(text.trim_end_matches([' ', '\n']).to_owned())
    }

    /// Cursor `(row, col)` on the rendered screen; `None` when this ring is not a terminal.
    pub(super) fn screen_cursor(&self) -> Option<(u16, u16)> {
        Some(self.screen.as_ref()?.screen().cursor_position())
    }

    /// Every row of the rendered screen, untrimmed, so the panel can place the cursor on blank rows.
    pub(super) fn screen_rows(&self) -> Option<Vec<String>> {
        let screen = self.screen.as_ref()?.screen();
        let (_, cols) = screen.size();
        Some(screen.rows(0, cols).collect())
    }

    /// Absolute offset of the oldest retained byte.
    fn start(&self) -> u64 {
        self.total - self.data.len() as u64
    }

    /// Retained bytes from absolute offset `from` on, and how many bytes after `from` were
    /// already discarded.
    pub(super) fn since(&self, from: u64) -> (Vec<u8>, u64) {
        self.recent_since(from, usize::MAX)
    }

    /// Return only the newest `cap` bytes without copying the rest of the display ring.
    pub(super) fn recent_since(&self, from: u64, cap: usize) -> (Vec<u8>, u64) {
        let retained = self.start();
        let start = retained.max(self.total.saturating_sub(cap as u64));
        let lost = start.saturating_sub(from);
        let skip = from.max(start) - retained;
        (self.data.range(skip as usize..).copied().collect(), lost)
    }

    /// Everything retained, as text.
    pub(super) fn text(&self) -> String {
        let (bytes, _) = self.since(0);
        String::from_utf8(bytes)
            .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
    }
}

/// Spool every byte before clipping. Pipes block; nonblocking PTYs retry after a short wait.
fn spawn_reader<R: Read + Send + 'static>(
    mut pipe: R,
    terminal: bool,
    ring: Arc<Mutex<Ring>>,
    mut spool: Option<ArtifactSpool>,
) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("dial-bg-reader".into())
        .spawn(move || {
            let mut buffer = [0u8; 16 * 1024];
            let mut error = None;
            let mut read_failed = false;
            loop {
                match pipe.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        if let Some(writer) = &mut spool
                            && let Err(e) = writer.write_all(&buffer[..count])
                        {
                            error = Some(format!("capturing complete output: {e}"));
                            // Do not publish a partially written artifact, or lose live output.
                            spool = None;
                            lock(&ring).error = error.clone();
                        }
                        lock(&ring).push(&buffer[..count]);
                        wake::notify();
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    // PTY input and output clones share O_NONBLOCK. No bytes available yet is
                    // not a capture failure (and must not cause the process to be killed).
                    Err(e) if terminal && e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    // Linux PTY masters report EIO when the last slave is closed.
                    #[cfg(unix)]
                    Err(e) if terminal && e.raw_os_error() == Some(libc::EIO) => break,
                    #[cfg(windows)]
                    Err(e) if terminal && e.kind() == io::ErrorKind::BrokenPipe => break,
                    Err(e) => {
                        error = Some(format!("reading background output: {e}"));
                        read_failed = true;
                        break;
                    }
                }
            }
            drop(pipe);
            if read_failed {
                let mut ring = lock(&ring);
                ring.read_failed = true;
                ring.error = error.clone();
            }
            let artifact = spool.map(ArtifactSpool::finish).transpose();
            {
                let mut ring = lock(&ring);
                match artifact {
                    Ok(uri) => ring.artifact = uri,
                    Err(e) => error = Some(format!("saving complete output artifact: {e}")),
                }
                ring.error = error;
                ring.closed = true;
            }
            wake::notify();
        })
}

/// Waits briefly for the reader to reach EOF. A descendant that escaped the process group can keep
/// the pipe open forever; detach the reader then instead of blocking the table on it.
fn finish_reader(ring: &Mutex<Ring>, reader: JoinHandle<()>) {
    let end = Instant::now() + SETTLE;
    loop {
        let seen = wake::generation();
        if lock(ring).closed || Instant::now() >= end {
            break;
        }
        wake::wait_past(seen, Some(end));
    }
    if lock(ring).closed {
        let _ = reader.join();
    }
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
    match (
        status.code(),
        crate::platform::process::exit_signal(&status),
    ) {
        (Some(code), _) => format!("exit code {code}"),
        (None, Some(signal)) => format!("killed by signal {signal}"),
        (None, None) => "exited".to_string(),
    }
}

enum ManagedProcess {
    Group(ProcessGroup),
    #[cfg(windows)]
    Pty {
        child: crate::platform::NativePtyChild,
        terminated: bool,
    },
}
impl ManagedProcess {
    fn id(&self) -> u32 {
        match self {
            Self::Group(group) => group.child.id(),
            #[cfg(windows)]
            Self::Pty { child, .. } => child.id(),
        }
    }
    fn terminate(&mut self) {
        match self {
            Self::Group(group) => group.terminate(),
            #[cfg(windows)]
            Self::Pty { child, terminated } => {
                if !*terminated {
                    let _ = child.kill();
                    *terminated = true;
                }
            }
        }
    }
    fn exited(&mut self) -> io::Result<bool> {
        match self {
            Self::Group(group) => group.exited(),
            #[cfg(windows)]
            Self::Pty { child, .. } => child.try_wait().map(|status| status.is_some()),
        }
    }
    fn wait(&mut self) -> io::Result<String> {
        match self {
            Self::Group(group) => group.child.wait().map(describe),
            #[cfg(windows)]
            Self::Pty { child, .. } => child.wait().map(describe),
        }
    }
    fn close_output(&mut self) {
        #[cfg(windows)]
        if let Self::Pty { child, .. } = self {
            child.close_output();
        }
    }
}
pub(super) struct Proc {
    pub(super) name: String,
    pub(super) command: String,
    pub(super) pid: u32,
    group: ManagedProcess,
    pub(super) ring: Arc<Mutex<Ring>>,
    started: Instant,
    pub(super) state: State,
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    pub(super) input: Option<Arc<Input>>,
    pub(super) terminal: bool,
    network: bool,
    notice_cursor: u64,
    completion_notified: bool,
    /// Started by a `bash` call that moved to the background (not an explicit `bg start`).
    /// Only these are awaited at turn end and block `finish`.
    adopted: bool,
}

impl Proc {
    fn spawn(
        ws: &Workspace,
        name: String,
        command: &str,
        cwd: &Path,
        terminal: bool,
        stdin: bool,
        rows: u16,
        cols: u16,
    ) -> Result<Proc, String> {
        let spool = ws
            .artifacts()
            .map(|store| store.spool())
            .transpose()
            .map_err(|e| format!("creating background output artifact: {e}"))?;
        #[cfg(not(windows))]
        let terminal_pair = if terminal {
            Some(input::terminal(rows, cols)?)
        } else {
            None
        };
        #[cfg(not(windows))]
        let mut spawn = shell_command_mode(
            ws,
            command,
            cwd,
            terminal_pair.as_ref().map(|(_, slave)| slave.as_raw_fd()),
        )?;
        #[cfg(windows)]
        let mut spawn = shell_command_mode(ws, command, cwd, None)?;
        let stop = Arc::new(AtomicBool::new(false));
        let ring = Arc::new(Mutex::new(if terminal {
            Ring::terminal(MAX_BUFFER, rows, cols)
        } else {
            Ring::new(MAX_BUFFER)
        }));
        #[cfg(not(windows))]
        let (group, reader, input) = if let Some((master, slave)) = terminal_pair {
            let input = Input::new(
                Handle::terminal_input(
                    master
                        .try_clone()
                        .map_err(|e| format!("cloning PTY: {e}"))?,
                ),
                stop.clone(),
            )?;
            spawn
                .stdin(slave.try_clone().map_err(|e| format!("cloning PTY: {e}"))?)
                .stdout(slave.try_clone().map_err(|e| format!("cloning PTY: {e}"))?)
                .stderr(slave)
                .env("TERM", "xterm-256color");
            unsafe {
                spawn.pre_exec(|| {
                    if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let child = spawn
                .spawn()
                .map_err(|e| format!("starting sandboxed PTY shell: {e}"))?;
            drop(spawn);
            let group = ManagedProcess::Group(ProcessGroup::new(child));
            let reader = spawn_reader(master, true, ring.clone(), spool)
                .map_err(|e| format!("starting PTY output reader: {e}"))?;
            (group, reader, Some(Arc::new(input)))
        } else {
            let (reader, writer) = io::pipe().map_err(|e| format!("creating output pipe: {e}"))?;
            let writer_err = writer
                .try_clone()
                .map_err(|e| format!("creating output pipe: {e}"))?;
            let input = if stdin {
                let (read, write) = io::pipe().map_err(|e| format!("creating stdin pipe: {e}"))?;
                spawn.stdin(read);
                Some(Arc::new(Input::new(Handle::pipe(write), stop.clone())?))
            } else {
                spawn.stdin(Stdio::null());
                None
            };
            spawn.stdout(writer).stderr(writer_err);
            let child = crate::platform::process::spawn_group(&mut spawn)
                .map_err(|e| format!("starting sandboxed shell: {e}"))?;
            drop(spawn);
            let group = ManagedProcess::Group(ProcessGroup::new(child));
            let reader = spawn_reader(reader, false, ring.clone(), spool)
                .map_err(|e| format!("starting output reader: {e}"))?;
            (group, reader, input)
        };
        #[cfg(windows)]
        let (group, reader, input) = if terminal {
            use std::ffi::OsString;
            let args: Vec<OsString> = spawn.get_args().map(OsString::from).collect();
            let env: Vec<(OsString, OsString)> = spawn
                .get_envs()
                .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value.to_owned())))
                .collect();
            let mut child = crate::platform::spawn_conpty(
                spawn.get_program(),
                &args,
                spawn.get_current_dir(),
                &env,
                cols,
                rows,
            )
            .map_err(|e| format!("starting ConPTY shell: {e}"))?;
            let output = child
                .output()
                .try_clone()
                .map_err(|e| format!("cloning ConPTY output: {e}"))?;
            let input = if stdin {
                let input = child
                    .input()
                    .try_clone()
                    .map_err(|e| format!("cloning ConPTY input: {e}"))?;
                Some(Arc::new(Input::new(
                    Handle::terminal_input(input),
                    stop.clone(),
                )?))
            } else {
                None
            };
            let group = ManagedProcess::Pty {
                child,
                terminated: false,
            };
            let reader = spawn_reader(output, true, ring.clone(), spool)
                .map_err(|e| format!("starting ConPTY output reader: {e}"))?;
            (group, reader, input)
        } else {
            let (reader, writer) = io::pipe().map_err(|e| format!("creating output pipe: {e}"))?;
            let writer_err = writer
                .try_clone()
                .map_err(|e| format!("creating output pipe: {e}"))?;
            let input = if stdin {
                let (read, write) = io::pipe().map_err(|e| format!("creating stdin pipe: {e}"))?;
                spawn.stdin(read);
                Some(Arc::new(Input::new(Handle::pipe(write), stop.clone())?))
            } else {
                spawn.stdin(Stdio::null());
                None
            };
            spawn.stdout(writer).stderr(writer_err);
            let child = crate::platform::process::spawn_group(&mut spawn)
                .map_err(|e| format!("starting shell: {e}"))?;
            drop(spawn);
            let group = ManagedProcess::Group(ProcessGroup::new(child));
            let reader = spawn_reader(reader, false, ring.clone(), spool)
                .map_err(|e| format!("starting output reader: {e}"))?;
            (group, reader, input)
        };
        let pid = group.id();
        wake::watch_exit(pid);
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
            input,
            terminal,
            network: super::network_allowed(ws),
            notice_cursor: 0,
            completion_notified: false,
            adopted: false,
        })
    }

    pub(super) fn is_running(&self) -> bool {
        matches!(self.state, State::Running)
    }

    /// Terminate the group, close its input side and stop the reader's pipe handling.
    fn signal_stop(&mut self) {
        self.group.terminate();
        self.group.close_output();
        self.stop.store(true, Ordering::Release);
    }

    /// Notice that the leader exited. The rest of its group is killed with it (the same rule
    /// `bash` applies), so a finished entry never leaves stray members behind.
    fn refresh(&mut self) {
        if !self.is_running() {
            return;
        }
        let capture_error = {
            let ring = lock(&self.ring);
            if ring.read_failed {
                ring.error.clone()
            } else {
                None
            }
        };
        let failure = match (capture_error, self.group.exited()) {
            (Some(error), _) => Some(error),
            (None, Ok(false)) => return,
            (None, Ok(true)) => None,
            (None, Err(e)) => Some(format!("status unavailable: {e}")),
        };
        self.group.terminate();
        self.group.close_output();
        self.stop.store(true, Ordering::Release);
        self.input.take();
        let detail = match failure {
            Some(detail) => detail,
            None => self
                .group
                .wait()
                .unwrap_or_else(|e| format!("status unavailable: {e}")),
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
            // A terminal's rendered screen is a snapshot, so it replaces rather than appends.
            if let Some(screen) = ring.screen_text() {
                proc.notice_cursor = ring.total;
                events.push(ToolEvent {
                    kind: "progress".into(),
                    name: proc.name.clone(),
                    text: screen,
                    artifact: None,
                });
            } else {
                let (bytes, omitted) = ring.recent_since(proc.notice_cursor, 16 * 1024);
                let mut text = String::from_utf8(bytes)
                    .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned());
                if omitted > 0 {
                    text.insert_str(
                        0,
                        &format!("[{omitted} earlier live bytes omitted; use bg output or the saved artifact]\n"),
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
    /// Native ConPTY resizes in place. A managed PTY returns its input endpoint so the caller can
    /// wait for the input slot after the table lock is released.
    fn resize(&mut self, rows: u16, cols: u16) -> Result<Option<PendingResize>, String> {
        match &mut self.group {
            #[cfg(windows)]
            ManagedProcess::Pty { child, .. } => {
                child
                    .resize(cols, rows)
                    .map_err(|e| format!("resizing ConPTY: {e}"))?;
                lock(&self.ring).resize_screen(rows, cols);
                Ok(None)
            }
            ManagedProcess::Group(_) => Ok(Some(PendingResize {
                input: self.input.clone().ok_or("resize requires a PTY process")?,
                ring: self.ring.clone(),
                rows,
                cols,
            })),
        }
    }
}

/// A resize waiting on a managed PTY's input slot. Finish it with the table lock released, so a
/// stalled input write cannot block every other background operation.
pub(super) struct PendingResize {
    input: Arc<Input>,
    ring: Arc<Mutex<Ring>>,
    rows: u16,
    cols: u16,
}

impl PendingResize {
    pub(super) fn finish(
        self,
        end: Option<Instant>,
        cancel: &AtomicBool,
        interrupt: &AtomicBool,
    ) -> Result<(), String> {
        self.input
            .resize(self.rows, self.cols, end, cancel, interrupt)?;
        lock(&self.ring).resize_screen(self.rows, self.cols);
        Ok(())
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.group.terminate();
        self.group.close_output();
        self.stop.store(true, Ordering::Release);
        if let Some(reader) = self.reader.take() {
            finish_reader(&self.ring, reader);
        }
    }
}

/// An entry removed by a kill. Its group is already signalled; [`Killed::finish`] waits for the
/// output reader with the table lock released.
pub(super) struct Killed {
    previous: State,
    proc: Proc,
}

impl Killed {
    /// Waits (bounded by [`SETTLE`]) for the reader to drain, then returns the state the entry had
    /// before the kill and its final notifications. Must run without the table lock.
    fn finish(mut self) -> (State, Vec<ToolEvent>) {
        if let Some(reader) = self.proc.reader.take() {
            finish_reader(&self.proc.ring, reader);
        }
        if self.proc.is_running() {
            self.proc.state = State::Exited {
                at: Instant::now(),
                detail: "killed; process group stopped".into(),
            };
        }
        let events = self.proc.drain_notifications();
        (self.previous, events)
    }
}

/// What a caller needs from an entry without holding the table lock.
pub(super) struct Snapshot {
    pub(super) name: String,
    pub(super) pid: u32,
    pub(super) ring: Arc<Mutex<Ring>>,
    pub(super) state: State,
    pub(super) network: bool,
    /// Whether `ring` is a terminal screen (PTY) rather than a byte stream.
    pub(super) terminal: bool,
}

impl Snapshot {
    /// After an exit, give the reader a moment to drain what the process wrote last.
    pub(super) fn settle(&self) {
        let State::Exited { at, .. } = self.state else {
            return;
        };
        let end = at + SETTLE;
        loop {
            let seen = wake::generation();
            if lock(&self.ring).closed || Instant::now() >= end {
                return;
            }
            wake::wait_past(seen, Some(end));
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

    /// Running `bash`-adopted jobs, as `name (command)`, that a turn must wait for before it finishes.
    pub(super) fn awaited(&self) -> Vec<String> {
        self.procs
            .iter()
            .filter(|p| p.is_running() && p.adopted)
            .map(|p| format!("{} ({})", p.name, p.command))
            .collect()
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
                self.discard(i);
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
        terminal: bool,
        stdin: bool,
        rows: u16,
        cols: u16,
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
        let proc = Proc::spawn(ws, name.clone(), command, cwd, terminal, stdin, rows, cols)?;
        // Only now that the spawn worked is a finished entry of the same name replaced.
        if let Some(i) = self.position(&name) {
            self.discard(i);
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
    /// Take ownership of a foreground process and its already-open output reader.
    pub(super) fn adopt(
        &mut self,
        job: crate::tools::shell::ForegroundJob,
    ) -> Result<String, String> {
        let (workspace, command, _, child, reader, captured, spool) = job.into_parts();
        let group = ManagedProcess::Group(ProcessGroup::new(child));
        self.refresh();
        if self.running() >= MAX_PROCESSES {
            return Err(format!(
                "{MAX_PROCESSES} background processes are already running; cannot adopt foreground process, which was stopped"
            ));
        }
        let name = self.fresh_name();
        let mut ring = Ring::new(MAX_BUFFER);
        // Output read before adoption is part of this process's output and artifact.
        ring.push(&captured);
        let ring = Arc::new(Mutex::new(ring));
        let reader = spawn_reader(reader, false, ring.clone(), spool)
            .map_err(|error| format!("starting adopted output reader: {error}"))?;
        let pid = group.id();
        wake::watch_exit(pid);
        let proc = Proc {
            name: name.clone(),
            command,
            pid,
            group,
            ring,
            started: Instant::now(),
            state: State::Running,
            stop: Arc::new(AtomicBool::new(false)),
            reader: Some(reader),
            input: None,
            terminal: false,
            network: super::network_allowed(&workspace),
            notice_cursor: 0,
            completion_notified: false,
            adopted: true,
        };
        self.prune_finished();
        self.procs.push(proc);
        self.notice(ToolEvent {
            kind: "start".into(),
            name: name.clone(),
            text: format!("adopted foreground process (pid {pid})"),
            artifact: None,
        });
        Ok(name)
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
            network: proc.network,
            terminal: proc.terminal,
        })
    }

    /// Resolves `name` for a resize. A managed PTY's input slot is waited on by the caller after
    /// the table lock is released (see [`PendingResize::finish`]).
    pub(super) fn resize(
        &mut self,
        name: &str,
        rows: u16,
        cols: u16,
    ) -> Result<Option<PendingResize>, String> {
        self.refresh();
        let index = self
            .position(name)
            .ok_or_else(|| format!("no background process named {name:?}"))?;
        self.procs[index].resize(rows, cols)
    }
    /// Clone the input endpoint, never hold the process table across pending writes.
    pub(super) fn input(&mut self, name: &str) -> Result<Arc<Input>, String> {
        self.snapshot(name)?;
        let proc = &self.procs[self.position(name).ok_or("process no longer exists")?];
        if !proc.is_running() {
            return Err(format!("{name} {}", proc.state.label()));
        }
        proc.input
            .clone()
            .ok_or_else(|| "stdin is disabled; start with stdin=true or pty=true".into())
    }

    /// Remove `name` and signal its group. The caller finishes the returned entry without the
    /// table lock (see [`kill_entry`]).
    fn detach(&mut self, name: &str) -> Result<Killed, String> {
        self.snapshot(name)?;
        let i = self
            .position(name)
            .ok_or_else(|| format!("no background process named {name:?}"))?;
        let mut proc = self.procs.remove(i);
        let previous = proc.state.clone();
        proc.signal_stop();
        Ok(Killed { previous, proc })
    }

    /// Remove every entry and signal every group; returns how many were running.
    fn detach_all(&mut self) -> (usize, Vec<Killed>) {
        self.refresh();
        let running = self.running();
        let killed = self
            .procs
            .drain(..)
            .map(|mut proc| {
                let previous = proc.state.clone();
                proc.signal_stop();
                Killed { previous, proc }
            })
            .collect();
        (running, killed)
    }

    fn record(&mut self, events: Vec<ToolEvent>) {
        for event in events {
            self.notice(event);
        }
    }

    /// Forget the finished entry at `index`. Its group was already terminated when it exited, so
    /// an output reader still open (a descendant holding the pipe) is detached rather than waited
    /// on under the table lock.
    fn discard(&mut self, index: usize) {
        let mut proc = self.procs.remove(index);
        let events = proc.drain_notifications();
        self.record(events);
        drop(proc.reader.take());
    }
}

/// Kill `name` and forget it; returns what state it was in. The table lock is held only to
/// detach the entry and to record its notices, never while the reader drains.
pub(super) fn kill_entry(table: &Mutex<Table>, name: &str) -> Result<State, String> {
    let killed = lock(table).detach(name)?;
    let (previous, events) = killed.finish();
    lock(table).record(events);
    Ok(previous)
}

/// Kill everything and clear the table; returns how many entries were running. As with
/// [`kill_entry`], readers drain with the table lock released.
pub(super) fn kill_all_entries(table: &Mutex<Table>) -> usize {
    let (running, killed) = lock(table).detach_all();
    let mut events = Vec::new();
    for entry in killed {
        events.extend(entry.finish().1);
    }
    lock(table).record(events);
    running
}

#[cfg(test)]
mod reader_tests {
    use super::*;

    struct DelayedOutput {
        pending: bool,
        data: io::Cursor<&'static [u8]>,
    }

    impl DelayedOutput {
        fn new() -> Self {
            Self {
                pending: true,
                data: io::Cursor::new(b"ready\n"),
            }
        }
    }

    impl Read for DelayedOutput {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if std::mem::take(&mut self.pending) {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            self.data.read(buffer)
        }
    }

    #[test]
    fn terminal_reader_waits_for_nonblocking_output() {
        let ring = Arc::new(Mutex::new(Ring::terminal(1024, 24, 80)));
        spawn_reader(DelayedOutput::new(), true, ring.clone(), None)
            .unwrap()
            .join()
            .unwrap();
        let ring = lock(&ring);
        assert_eq!(ring.text(), "ready\n");
        assert!(ring.closed);
        assert!(!ring.read_failed);
        assert!(ring.error.is_none());
    }

    #[test]
    fn pipe_reader_still_reports_unexpected_read_failures() {
        let ring = Arc::new(Mutex::new(Ring::new(1024)));
        spawn_reader(DelayedOutput::new(), false, ring.clone(), None)
            .unwrap()
            .join()
            .unwrap();
        let ring = lock(&ring);
        assert!(ring.closed);
        assert!(ring.read_failed);
        assert!(
            ring.error
                .as_ref()
                .unwrap()
                .contains("reading background output")
        );
    }
}

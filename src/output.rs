//! Process stdout ownership for headless protocols: ordered frames, one dedicated writer thread
//! doing ordinary blocking writes, and a private disk spool for backpressure.
//!
//! Producers never block on the reader and never change descriptor status flags: the writer owns
//! a `dup` of fd 1, so `O_NONBLOCK` is never installed on an open-file description the terminal,
//! stderr or a parent process share, and no exit path has flags to restore. When the reader
//! stalls (an IDE paused in a debugger, Ctrl-S in a terminal) frames queue in memory up to a
//! bound, then append to an unlinked 0600 temp file that drains in order once the reader resumes.
//! Only a write error (EPIPE, EIO, …) or an exhausted spool budget is a failure; slowness is not.
#[cfg(windows)]
use crate::platform;
#[cfg(windows)]
use std::os::windows::fs::FileExt;
#[cfg(unix)]
use std::os::{
    fd::{AsRawFd, FromRawFd, OwnedFd},
    unix::fs::FileExt,
};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::File,
    io::{self, Write},
    sync::{
        Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
fn spool_write(file: &File, bytes: &[u8], offset: u64) -> io::Result<()> {
    file.write_all_at(bytes, offset)
}
#[cfg(windows)]
fn spool_write(file: &File, mut bytes: &[u8], mut offset: u64) -> io::Result<()> {
    while !bytes.is_empty() {
        let written = file.seek_write(bytes, offset)?;
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "spool write made no progress",
            ));
        }
        bytes = &bytes[written..];
        offset += written as u64;
    }
    Ok(())
}
#[cfg(unix)]
fn spool_read(file: &File, bytes: &mut [u8], offset: u64) -> io::Result<()> {
    file.read_exact_at(bytes, offset)
}
#[cfg(windows)]
fn spool_read(file: &File, mut bytes: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !bytes.is_empty() {
        let read = file.seek_read(bytes, offset)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "spool ended early",
            ));
        }
        offset += read as u64;
        bytes = &mut bytes[read..];
    }
    Ok(())
}

/// Queued bytes held in memory before new frames spill to disk.
const MEMORY_LIMIT: usize = 4 * 1024 * 1024;
/// Disk the spool may use before the stalled reader is treated as gone.
const SPOOL_LIMIT: u64 = 1024 * 1024 * 1024;
const READ_CHUNK: u64 = 64 * 1024;

#[derive(Clone, Debug)]
struct Failure {
    kind: io::ErrorKind,
    message: String,
}
impl Failure {
    fn from_error(error: &io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
    fn error(&self) -> io::Error {
        io::Error::new(self.kind, self.message.clone())
    }
}

struct Spool {
    file: File,
    read: u64,
    written: u64,
}

#[derive(Default)]
struct State {
    memory: VecDeque<Vec<u8>>,
    memory_bytes: usize,
    spool: Option<Spool>,
    /// The writer holds bytes it has not finished writing.
    busy: bool,
    failure: Option<Failure>,
    /// Total bytes ever spilled; observable for diagnostics and tests.
    spilled: u64,
    interrupts: Vec<Weak<AtomicBool>>,
    next_ticket: u64,
    next_publish: u64,
    pending: BTreeMap<u64, Vec<u8>>,
}

/// One ordered byte stream to one descriptor.
pub(crate) struct Pipe {
    state: Mutex<State>,
    /// Serializes publication after tickets have been reserved.
    publication: Mutex<()>,
    wake: Condvar,
    idle: Condvar,
    memory_limit: usize,
    spool_limit: u64,
}

enum Chunk {
    Memory(Vec<u8>),
    Spool { file: File, start: u64, end: u64 },
}

impl Pipe {
    /// Take ownership of the writer handle and start its thread.
    pub(crate) fn spawn(out: File, memory_limit: usize, spool_limit: u64) -> io::Result<Arc<Self>> {
        let pipe = Arc::new(Self {
            publication: Mutex::new(()),
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            idle: Condvar::new(),
            memory_limit,
            spool_limit,
        });
        let writer = pipe.clone();
        thread::Builder::new()
            .name("dial-stdout".into())
            .spawn(move || writer.run(out))?;
        Ok(pipe)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic while holding the lock cannot leave a half-applied byte queue: every mutation
        // completes before unlocking, so recovering the guard is sound.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn fail(&self, state: &mut State, error: &io::Error) {
        if state.failure.is_none() {
            state.failure = Some(Failure::from_error(error));
        }
        state.memory.clear();
        state.memory_bytes = 0;
        state.spool = None;
        state.busy = false;
        for interrupt in state.interrupts.iter().filter_map(Weak::upgrade) {
            interrupt.store(true, Ordering::SeqCst);
        }
        self.idle.notify_all();
        self.wake.notify_all();
    }

    fn failure(&self) -> Option<io::Error> {
        self.lock().failure.as_ref().map(Failure::error)
    }

    fn register(&self, interrupt: &Arc<AtomicBool>) {
        let mut state = self.lock();
        state.interrupts.retain(|weak| weak.strong_count() > 0);
        state.interrupts.push(Arc::downgrade(interrupt));
        if state.failure.is_some() {
            interrupt.store(true, Ordering::SeqCst);
        }
    }

    /// Queue one complete frame group; never waits for the reader.
    fn enqueue(&self, bytes: Vec<u8>) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut state = self.lock();
        if let Some(failure) = &state.failure {
            return Err(failure.error());
        }
        let spill = state.spool.is_some()
            || (state.memory_bytes > 0 && state.memory_bytes + bytes.len() > self.memory_limit);
        if !spill {
            state.memory_bytes += bytes.len();
            state.memory.push_back(bytes);
            self.wake.notify_one();
            return Ok(());
        }
        let result = (|| -> io::Result<()> {
            if state.spool.is_none() {
                // Unlinked at creation: no exit path (even process::exit) can leave it behind.
                let file = tempfile::tempfile()?;
                state.spool = Some(Spool {
                    file,
                    read: 0,
                    written: 0,
                });
            }
            let limit = self.spool_limit;
            let spool = state.spool.as_mut().expect("spool exists");
            if spool.written.saturating_add(bytes.len() as u64) > limit {
                return Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    format!(
                        "stdout reader made no progress while {} MiB of output was spooled",
                        limit / (1024 * 1024)
                    ),
                ));
            }
            spool_write(&spool.file, &bytes, spool.written)?;
            spool.written += bytes.len() as u64;
            Ok(())
        })();
        match result {
            Ok(()) => {
                state.spilled += bytes.len() as u64;
                self.wake.notify_one();
                Ok(())
            }
            Err(error) => {
                self.fail(&mut state, &error);
                Err(error)
            }
        }
    }

    fn next_chunk(&self) -> Option<Chunk> {
        let mut state = self.lock();
        loop {
            if state.failure.is_some() {
                return None;
            }
            if let Some(bytes) = state.memory.pop_front() {
                state.memory_bytes -= bytes.len();
                state.busy = true;
                return Some(Chunk::Memory(bytes));
            }
            if let Some(spool) = &state.spool {
                if spool.read < spool.written {
                    let start = spool.read;
                    let end = (start + READ_CHUNK).min(spool.written);
                    match spool.file.try_clone() {
                        Ok(file) => {
                            state.busy = true;
                            return Some(Chunk::Spool { file, start, end });
                        }
                        Err(error) => {
                            self.fail(&mut state, &error);
                            return None;
                        }
                    }
                }
                // Fully delivered: later frames may use memory again.
                state.spool = None;
                continue;
            }
            state.busy = false;
            self.idle.notify_all();
            state = self
                .wake
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn run(&self, mut out: File) {
        while let Some(chunk) = self.next_chunk() {
            let result = match &chunk {
                Chunk::Memory(bytes) => write_blocking(&mut out, bytes),
                Chunk::Spool { file, start, end } => {
                    let mut bytes = vec![0; (end - start) as usize];
                    spool_read(file, &mut bytes, *start)
                        .and_then(|()| write_blocking(&mut out, &bytes))
                }
            };
            let mut state = self.lock();
            match result {
                Ok(()) => {
                    if let (Chunk::Spool { end, .. }, Some(spool)) = (&chunk, state.spool.as_mut())
                    {
                        spool.read = *end;
                    }
                }
                Err(error) => {
                    self.fail(&mut state, &error);
                    return;
                }
            }
        }
    }

    /// Wait until everything queued so far has been written (or the pipe failed).
    fn drain(&self, timeout: Option<Duration>) -> io::Result<()> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let mut state = self.lock();
        loop {
            if let Some(failure) = &state.failure {
                return Err(failure.error());
            }
            if state.memory.is_empty() && state.spool.is_none() && !state.busy {
                return Ok(());
            }
            let wait = match deadline {
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "stdout reader did not drain queued output in time",
                        ));
                    }
                    left.min(Duration::from_millis(100))
                }
                None => Duration::from_millis(100),
            };
            state = self
                .idle
                .wait_timeout(state, wait)
                .map(|(guard, _)| guard)
                .unwrap_or_else(|poisoned| poisoned.into_inner().0);
        }
    }

    fn spooling(&self) -> bool {
        self.lock().spool.is_some()
    }
}

/// Ordinary blocking write. An inherited nonblocking description is waited on, never changed.
fn write_blocking(out: &mut File, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        match out.write(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "stdout write made no progress",
                ));
            }
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                #[cfg(unix)]
                {
                    let mut poll = libc::pollfd {
                        fd: out.as_raw_fd(),
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    // SAFETY: one initialized pollfd and a finite wait; only readiness is observed.
                    if unsafe { libc::poll(&mut poll, 1, 1000) } < 0 {
                        let error = io::Error::last_os_error();
                        if error.kind() != io::ErrorKind::Interrupted {
                            return Err(error);
                        }
                    } else if poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "stdout reader disconnected",
                        ));
                    }
                }
                #[cfg(windows)]
                platform::io::wait_writable(out, Duration::from_secs(1))?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn duplicate_stdout() -> io::Result<File> {
    // SAFETY: duplicate fd 1 into an owned close-on-exec descriptor; stdout itself is untouched.
    let fd = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fcntl returned a new uniquely owned descriptor.
    Ok(unsafe { File::from(OwnedFd::from_raw_fd(fd)) })
}

#[cfg(windows)]
fn duplicate_stdout() -> io::Result<File> {
    platform::io::duplicate_stdout()
}
static STDOUT: OnceLock<Result<Arc<Pipe>, Failure>> = OnceLock::new();

fn stdout_pipe() -> io::Result<Arc<Pipe>> {
    STDOUT
        .get_or_init(|| {
            let stdout = duplicate_stdout().map_err(|error| Failure::from_error(&error))?;
            Pipe::spawn(stdout, MEMORY_LIMIT, SPOOL_LIMIT)
                .map_err(|error| Failure::from_error(&error))
        })
        .clone()
        .map_err(|failure| failure.error())
}

/// Wait for queued stdout to reach the reader. Exit paths call this so the final frames are not
/// lost; `timeout` bounds teardown when the reader may be gone. No-op when nothing used stdout.
pub(crate) fn drain_stdout(timeout: Option<Duration>) -> io::Result<()> {
    match STDOUT.get() {
        Some(Ok(pipe)) => pipe.drain(timeout),
        Some(Err(failure)) => Err(failure.error()),
        None => Ok(()),
    }
}

#[derive(Clone)]
pub(crate) struct StdoutWriter {
    pipe: Arc<Pipe>,
    interrupt: Arc<AtomicBool>,
    disconnected: Arc<AtomicBool>,
}
/// Lets the stdin reader end a connection without holding the writer.
#[derive(Clone)]
pub(crate) struct Cancellation {
    interrupt: Arc<AtomicBool>,
    disconnected: Arc<AtomicBool>,
}
impl Cancellation {
    pub(crate) fn disconnect(&self) {
        self.disconnected.store(true, Ordering::SeqCst);
        self.interrupt.store(true, Ordering::SeqCst);
    }
}
impl StdoutWriter {
    pub(crate) fn new() -> io::Result<Self> {
        Ok(Self::over(stdout_pipe()?))
    }
    pub(crate) fn over(pipe: Arc<Pipe>) -> Self {
        let interrupt = Arc::new(AtomicBool::new(false));
        pipe.register(&interrupt);
        Self {
            pipe,
            interrupt,
            disconnected: Arc::new(AtomicBool::new(false)),
        }
    }
    /// A write failure raises this flag so the owning turn stops producing output.
    pub(crate) fn set_interrupt(&mut self, interrupt: Arc<AtomicBool>) {
        self.pipe.register(&interrupt);
        self.interrupt = interrupt;
    }
    /// Called by the stdin reader on EOF: this connection is over even if stdout still works.
    pub(crate) fn disconnect(&self) {
        self.disconnected.store(true, Ordering::SeqCst);
        self.interrupt.store(true, Ordering::SeqCst);
    }
    pub(crate) fn cancellation(&self) -> Cancellation {
        Cancellation {
            interrupt: self.interrupt.clone(),
            disconnected: self.disconnected.clone(),
        }
    }
    pub(crate) fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::SeqCst)
    }
    pub(crate) fn failed(&self) -> bool {
        self.pipe.failure().is_some()
    }
    pub(crate) fn failure(&self) -> Option<io::Error> {
        self.pipe.failure()
    }
    pub(crate) fn spooling(&self) -> bool {
        self.pipe.spooling()
    }
    fn check(&self) -> io::Result<()> {
        if let Some(error) = self.failure() {
            return Err(error);
        }
        if self.is_disconnected() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stdout client disconnected",
            ));
        }
        Ok(())
    }
    /// Reserve an output position before work that may race with another producer.
    pub(crate) fn reserve(&self) -> u64 {
        let mut state = self.pipe.lock();
        let ticket = state.next_ticket;
        state.next_ticket = state.next_ticket.wrapping_add(1);
        ticket
    }
    /// Publish a complete batch at its reserved position. Later tickets wait until all earlier
    /// reservations arrive, then enter the bounded queue/spool in the same order.
    pub(crate) fn json_batch(
        &self,
        ticket: u64,
        values: impl IntoIterator<Item = serde_json::Value>,
    ) -> io::Result<()> {
        // A refused or unserializable batch still occupies its ticket position with no bytes;
        // otherwise every later ticket on this pipe waits forever in `pending`.
        let (bytes, status) = match self.check().and_then(|()| {
            let mut bytes = Vec::new();
            for value in values {
                serde_json::to_writer(&mut bytes, &value).map_err(io::Error::other)?;
                bytes.push(b'\n');
            }
            Ok(bytes)
        }) {
            Ok(bytes) => (bytes, Ok(())),
            Err(error) => (Vec::new(), Err(error)),
        };
        let _publication = self
            .pipe
            .publication
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let ready = {
            let mut state = self.pipe.lock();
            if ticket < state.next_publish || ticket >= state.next_ticket {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid or duplicate stdout ticket",
                ));
            }
            state.pending.insert(ticket, bytes);
            let mut ready = Vec::new();
            let mut next_publish = state.next_publish;
            while let Some(frame) = state.pending.remove(&next_publish) {
                next_publish = next_publish.wrapping_add(1);
                ready.push(frame);
            }
            state.next_publish = next_publish;
            ready
        };
        for frame in ready {
            self.pipe.enqueue(frame)?;
        }
        status
    }
    /// Queue preassembled bytes (complete lines) as one ordered unit.
    pub(crate) fn bytes(&self, bytes: Vec<u8>) -> io::Result<()> {
        self.check()?;
        self.pipe.enqueue(bytes)
    }
    pub(crate) fn json(&self, value: &serde_json::Value) -> io::Result<()> {
        self.json_lines(std::iter::once(value))
    }
    /// Several JSON lines that must stay adjacent and ordered.
    pub(crate) fn json_lines<'a>(
        &self,
        values: impl IntoIterator<Item = &'a serde_json::Value>,
    ) -> io::Result<()> {
        let mut bytes = Vec::new();
        for value in values {
            serde_json::to_writer(&mut bytes, value).map_err(io::Error::other)?;
            bytes.push(b'\n');
        }
        self.bytes(bytes)
    }
    pub(crate) fn text(&self, text: &[u8]) -> io::Result<()> {
        self.bytes(text.to_vec())
    }
    /// Wait until queued output has been written; see `drain_stdout`. A timeout while output sits
    /// in the disk spool says so: spooled frames live in an unlinked file and do not outlive the process.
    pub(crate) fn drain(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.pipe.drain(timeout).map_err(|error| {
            if error.kind() == io::ErrorKind::TimedOut && self.spooling() {
                io::Error::new(
                    error.kind(),
                    format!("{error}; queued output is spooled to disk and will be lost on exit"),
                )
            } else {
                error
            }
        })
    }
}

#[cfg(all(test, unix))]
mod tests;

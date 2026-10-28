//! Output sink of the inline terminal.
//!
//! Fake writers (tests) are written synchronously. A real TTY is written by one dedicated thread
//! that drains a queue in push order. Admission never waits: a write the queue cannot take right
//! now is refused with `WouldBlock` (or [`Sink::try_write`] returns `false`), so the paint path
//! keeps its state and retries later instead of stalling input. Queued bytes are accounted:
//! [`Sink::backlogged`] tells the renderer to coalesce frames once the reader falls behind. Past
//! [`HARD_LIMIT`] new chunks are refused; a single chunk larger than the limit is admitted only
//! into an empty queue, and a chunk over [`MAX_FRAME`] is an error. Queued memory therefore grows by
//! at most one frame past the limit. A write error is sticky and
//! reported by every later write or drain.
//!
//! Ordering: every byte, history rows and control sequences alike, goes through the same queue,
//! so they reach the terminal in the order they were written. Anything that hands the terminal to
//! another owner (raw-mode changes, external programs, teardown) calls [`Sink::drain`] first.
use std::{
    collections::VecDeque,
    io::{self, Write},
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// Queued bytes (including the chunk being written) past which new chunks are refused, not waited for.
const HARD_LIMIT: usize = 8 * 1024 * 1024;
/// Largest single write the queue accepts. A bigger frame is an error, never a wait.
const MAX_FRAME: usize = 16 * 1024 * 1024;
/// Queued bytes past which the renderer should coalesce frames instead of writing each one.
const BACKLOG_LIMIT: usize = 256 * 1024;

/// Where the terminal's bytes go.
pub(super) enum Sink {
    /// Synchronous writer for fake terminals (tests and `test-support` only).
    #[cfg(any(test, feature = "test-support"))]
    Direct(Box<dyn Write>),
    /// Real TTY: a dedicated writer thread drains the bounded queue in order.
    Queued(Queued),
}

impl Sink {
    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn direct(out: Box<dyn Write>) -> Self {
        Self::Direct(out)
    }

    /// Spawn the writer thread for `out`. The thread ends when the queue is closed and empty.
    pub(super) fn queued(out: Box<dyn Write + Send>) -> io::Result<Self> {
        let queue = Arc::new(Queue::default());
        let worker = Arc::clone(&queue);
        thread::Builder::new()
            .name("dial-terminal-output".into())
            .spawn(move || run(worker, out))?;
        *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&queue));
        Ok(Self::Queued(Queued { queue }))
    }

    /// Whether the reader has fallen far enough behind that intermediate frames should be skipped.
    pub(super) fn backlogged(&self) -> bool {
        match self {
            #[cfg(any(test, feature = "test-support"))]
            Self::Direct(_) => false,
            Self::Queued(queued) => queued.queue.backlog.load(Ordering::SeqCst) > BACKLOG_LIMIT,
        }
    }

    /// Wait until every byte queued so far has reached the terminal. Errors on a write failure
    /// and when the reader stalls past `timeout`.
    pub(super) fn drain(&mut self, timeout: Duration) -> io::Result<()> {
        match self {
            #[cfg(any(test, feature = "test-support"))]
            Self::Direct(out) => out.flush(),
            Self::Queued(queued) => queued.queue.drain(timeout),
        }
    }

    /// Admit `data` without waiting. Borrowed bytes are copied only once admitted. `Ok(false)`
    /// means the queue is full and nothing was queued.
    pub(super) fn try_write(&mut self, data: &[u8]) -> io::Result<bool> {
        match self {
            #[cfg(any(test, feature = "test-support"))]
            Self::Direct(out) => out.write_all(data).map(|()| true),
            Self::Queued(queued) => queued.queue.try_push_with(data.len(), || data.to_vec()),
        }
    }

    /// Admit an already-rendered frame without waiting or copying it: the buffer moves into the
    /// queue. `Ok(false)` means the queue is full; the frame is dropped and must be rendered again.
    pub(super) fn try_write_frame(&mut self, frame: Vec<u8>) -> io::Result<bool> {
        match self {
            #[cfg(any(test, feature = "test-support"))]
            Self::Direct(out) => out.write_all(&frame).map(|()| true),
            Self::Queued(queued) => {
                let len = frame.len();
                queued.queue.try_push_with(len, move || frame)
            }
        }
    }
}

impl Write for Sink {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        match self {
            #[cfg(any(test, feature = "test-support"))]
            Self::Direct(out) => out.write(data),
            Self::Queued(_) => {
                if self.try_write(data)? {
                    Ok(data.len())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "terminal output is backlogged",
                    ))
                }
            }
        }
    }

    /// The queue is ordered by itself; a real barrier is [`Sink::drain`].
    fn flush(&mut self) -> io::Result<()> {
        match self {
            #[cfg(any(test, feature = "test-support"))]
            Self::Direct(out) => out.flush(),
            Self::Queued(_) => Ok(()),
        }
    }
}

pub(super) struct Queued {
    queue: Arc<Queue>,
}

impl Drop for Queued {
    fn drop(&mut self) {
        self.queue.close();
        let mut current = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
        if current
            .as_ref()
            .is_some_and(|held| Arc::ptr_eq(held, &self.queue))
        {
            *current = None;
        }
    }
}

/// The live queue, for the panic hook (which cannot reach the `InlineTerminal`).
static CURRENT: Mutex<Option<Arc<Queue>>> = Mutex::new(None);

pub(super) fn write_control(bytes: Vec<u8>) -> io::Result<()> {
    let queue = CURRENT
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    if let Some(queue) = queue {
        let length = bytes.len();
        if !queue.try_push_with(length, || bytes)? {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "terminal output is backlogged",
            ));
        }
        return Ok(());
    }
    let mut out = io::stdout().lock();
    out.write_all(&bytes)?;
    out.flush()
}

/// Best-effort drain of the live queue from the panic hook. Bounded: a stalled reader must not
/// hang the panic report.
pub(super) fn drain_current(timeout: Duration) {
    let queue = match CURRENT.try_lock() {
        Ok(current) => current.clone(),
        Err(_) => None,
    };
    if let Some(queue) = queue {
        let _ = queue.drain(timeout);
    }
}

#[derive(Default)]
struct Queue {
    state: Mutex<State>,
    changed: Condvar,
    /// Mirror of `State::bytes`, readable without the lock.
    backlog: AtomicUsize,
}

#[derive(Default)]
struct State {
    chunks: VecDeque<Vec<u8>>,
    /// Bytes queued plus the chunk in flight; zero means everything reached the terminal.
    bytes: usize,
    failure: Option<Failure>,
    closed: bool,
}

#[derive(Clone)]
struct Failure {
    kind: io::ErrorKind,
    message: String,
}

impl Failure {
    fn error(&self) -> io::Error {
        io::Error::new(self.kind, self.message.clone())
    }
}

impl Queue {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Admit a chunk of `len` bytes without waiting. `chunk` runs only once the chunk is accepted,
    /// so a refused write is never copied. `Ok(false)`: the queue is full and nothing was queued.
    fn try_push_with(&self, len: usize, chunk: impl FnOnce() -> Vec<u8>) -> io::Result<bool> {
        if len == 0 {
            return Ok(true);
        }
        let mut state = self.state();
        if let Some(failure) = &state.failure {
            return Err(failure.error());
        }
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "terminal output is closed",
            ));
        }
        if len > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("terminal frame of {len} bytes exceeds the {MAX_FRAME} byte limit"),
            ));
        }
        if state.bytes != 0 && state.bytes + len > HARD_LIMIT {
            return Ok(false);
        }
        state.bytes += len;
        self.backlog.store(state.bytes, Ordering::SeqCst);
        state.chunks.push_back(chunk());
        self.changed.notify_all();
        Ok(true)
    }

    fn drain(&self, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        let mut state = self.state();
        loop {
            if let Some(failure) = &state.failure {
                return Err(failure.error());
            }
            if state.bytes == 0 {
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("terminal output stalled with {} bytes pending", state.bytes),
                ));
            }
            state = match self.changed.wait_timeout(state, deadline - now) {
                Ok((state, _)) => state,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    fn close(&self) {
        self.state().closed = true;
        self.changed.notify_all();
    }
}

/// Writer thread body: one chunk at a time, in push order. Stops on a write error (sticky) or
/// once the queue is closed and empty.
fn run(queue: Arc<Queue>, mut out: Box<dyn Write + Send>) {
    loop {
        let chunk = {
            let mut state = queue.state();
            loop {
                if let Some(chunk) = state.chunks.pop_front() {
                    break chunk;
                }
                if state.closed {
                    return;
                }
                state = queue.changed.wait(state).unwrap_or_else(|e| e.into_inner());
            }
        };
        let written = out.write_all(&chunk).and_then(|()| out.flush());
        let mut state = queue.state();
        let failed = match written {
            Ok(()) => {
                state.bytes -= chunk.len();
                false
            }
            Err(error) => {
                state.failure = Some(Failure {
                    kind: error.kind(),
                    message: error.to_string(),
                });
                state.chunks.clear();
                state.bytes = 0;
                true
            }
        };
        queue.backlog.store(state.bytes, Ordering::SeqCst);
        queue.changed.notify_all();
        if failed {
            return;
        }
    }
}

#[cfg(test)]
mod tests;

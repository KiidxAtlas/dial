//! Central terminal input: one reader owns stdin while the real terminal is active.
//!
//! While the pump runs, a single thread reads crossterm events into a bounded queue, so keys typed
//! during a long-running turn are kept and delivered in order when the controller polls again.
//! [`Pump::stop`] joins that thread before the terminal is handed to an external program (or torn
//! down), so the program is the only reader of stdin then. Queued events survive a restart.
//! When the pump is not running (tests, non-TTY, between sessions), `poll` and `read` read the
//! terminal directly; there is never more than one reader at a time.
use std::{
    collections::VecDeque,
    io,
    sync::{
        Arc, Condvar, LazyLock, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crossterm::event::{self, Event};

/// Events held before the reader stops; further input stays in the kernel buffer.
pub const CAPACITY: usize = 1024;
/// Longest one poll blocks before the reader checks for a stop request.
const TICK: Duration = Duration::from_millis(20);

/// Where events come from. The pump's reader thread owns one; `poll`/`read` fall back to one
/// when the pump is stopped.
pub trait EventSource: Send {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool>;
    fn read(&mut self) -> io::Result<Event>;
}

/// The real terminal, through crossterm.
pub struct CrosstermEvents;

impl EventSource for CrosstermEvents {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        event::poll(timeout)
    }

    fn read(&mut self) -> io::Result<Event> {
        event::read()
    }
}

struct Failure {
    kind: io::ErrorKind,
    message: String,
}

impl Failure {
    fn error(&self) -> io::Error {
        io::Error::new(self.kind, self.message.clone())
    }
}

struct State {
    events: VecDeque<Event>,
    /// The reader thread is alive and owns the source.
    running: bool,
    /// Asks the reader thread to exit at its next tick.
    stop: bool,
    /// A read error ended the reader; reported to the next poll or read.
    failure: Option<Failure>,
}

pub struct Pump {
    state: Mutex<State>,
    changed: Condvar,
    /// Set from the panic hook (no lock: the panicking thread may hold one). The reader exits.
    halt: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Pump {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                events: VecDeque::new(),
                running: false,
                stop: false,
                failure: None,
            }),
            changed: Condvar::new(),
            halt: AtomicBool::new(false),
            thread: Mutex::new(None),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Start the reader thread for `source`. Idempotent while running.
    pub fn start(self: &Arc<Self>, source: Box<dyn EventSource>) -> io::Result<()> {
        let mut handle = self.thread.lock().unwrap_or_else(|e| e.into_inner());
        if self.state().running {
            return Ok(());
        }
        // The previous reader has exited (it cleared `running`); reap it before reusing the slot.
        if let Some(old) = handle.take() {
            let _ = old.join();
        }
        {
            let mut state = self.state();
            state.running = true;
            state.stop = false;
            state.failure = None;
        }
        self.halt.store(false, Ordering::SeqCst);
        let reader = Arc::clone(self);
        let spawned = thread::Builder::new()
            .name("dial-input".into())
            .spawn(move || reader.run(source));
        match spawned {
            Ok(join) => {
                *handle = Some(join);
                Ok(())
            }
            Err(error) => {
                self.state().running = false;
                Err(error)
            }
        }
    }

    /// Stop the reader and wait until it has dropped its source. Queued events are kept.
    pub fn stop(&self) {
        let mut handle = self.thread.lock().unwrap_or_else(|e| e.into_inner());
        {
            let mut state = self.state();
            state.stop = true;
            self.changed.notify_all();
        }
        if let Some(join) = handle.take() {
            let _ = join.join();
        }
        let mut state = self.state();
        state.running = false;
        state.stop = false;
        self.changed.notify_all();
    }

    /// Ask the reader to exit without waiting for it (panic hook).
    pub fn halt(&self) {
        self.halt.store(true, Ordering::SeqCst);
    }

    fn halted(&self) -> bool {
        self.halt.load(Ordering::SeqCst)
    }

    fn run(&self, mut source: Box<dyn EventSource>) {
        let outcome = self.pump(source.as_mut());
        let mut state = self.state();
        if let Err(error) = outcome {
            state.failure = Some(Failure {
                kind: error.kind(),
                message: error.to_string(),
            });
        }
        state.running = false;
        self.changed.notify_all();
    }

    fn pump(&self, source: &mut dyn EventSource) -> io::Result<()> {
        loop {
            {
                let mut state = self.state();
                // A full queue pauses reading; the wait is bounded so halt and stop are seen.
                while state.events.len() >= CAPACITY && !state.stop && !self.halted() {
                    state = match self.changed.wait_timeout(state, TICK) {
                        Ok((state, _)) => state,
                        Err(poisoned) => poisoned.into_inner().0,
                    };
                }
                if state.stop || self.halted() {
                    return Ok(());
                }
            }
            if !source.poll(TICK)? {
                continue;
            }
            let event = source.read()?;
            let mut state = self.state();
            state.events.push_back(event);
            self.changed.notify_all();
        }
    }

    /// Next event: a queued one, else the reader's next one, else a direct read from `idle` when
    /// the pump is stopped. Waits at most `timeout`.
    pub fn poll_with(&self, timeout: Duration, idle: &mut dyn EventSource) -> io::Result<bool> {
        let deadline = Instant::now() + timeout;
        let mut state = self.state();
        loop {
            if !state.events.is_empty() {
                return Ok(true);
            }
            if let Some(failure) = &state.failure {
                return Err(failure.error());
            }
            if !state.running {
                drop(state);
                return idle.poll(deadline.saturating_duration_since(Instant::now()));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            state = match self.changed.wait_timeout(state, deadline - now) {
                Ok((state, _)) => state,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    /// Next event, waiting as needed. Falls back to `idle` when the pump is stopped.
    pub fn read_with(&self, idle: &mut dyn EventSource) -> io::Result<Event> {
        let mut state = self.state();
        loop {
            if let Some(event) = state.events.pop_front() {
                self.changed.notify_all();
                return Ok(event);
            }
            if let Some(failure) = &state.failure {
                return Err(failure.error());
            }
            if !state.running {
                drop(state);
                return idle.read();
            }
            state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// The real terminal's pump, shared with the panic hook.
static PUMP: LazyLock<Arc<Pump>> = LazyLock::new(|| Arc::new(Pump::new()));

/// Start the real terminal's input reader.
pub fn start() -> io::Result<()> {
    let pump: &Arc<Pump> = &PUMP;
    pump.start(Box::new(CrosstermEvents))
}

/// Stop the real terminal's input reader before the terminal changes hands.
pub fn stop() {
    PUMP.stop();
}

/// Panic hook: ask the reader to exit; no lock is taken.
pub fn halt_for_panic() {
    PUMP.halt();
}

/// Wait up to `timeout` for an event; `Ok(true)` when [`read`] will not block.
pub fn poll(timeout: Duration) -> io::Result<bool> {
    PUMP.poll_with(timeout, &mut CrosstermEvents)
}

/// Next input event.
pub fn read() -> io::Result<Event> {
    PUMP.read_with(&mut CrosstermEvents)
}

#[cfg(test)]
mod tests;

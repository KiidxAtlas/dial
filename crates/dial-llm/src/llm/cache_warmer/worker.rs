//! Off-thread driver for [`Warmer`]. One worker per session owns its replayer and schedule, so the
//! agent loop only sends events and drains reports; it never waits on a replay.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::Result;

use super::{Mode, Stop, Tier, Warmer};
use crate::llm::Chat;
use crate::llm::catalog::TokenPricing;
use crate::llm::usage::Scope;

/// How often the worker re-checks the clock while nothing is due; bounds replay latency.
const POLL: Duration = Duration::from_millis(50);
/// Queue depth for events and reports. Warming is best-effort, so a full queue drops messages.
const QUEUE: usize = 32;

/// Sends one replay request. The agent crate implements it over a real backend; tests use fakes.
pub trait Replayer: Send {
    /// Sends `chat` (already capped at one output token). Returns the cache read tokens the
    /// provider reported, or `None` when it reported no cache usage at all.
    fn replay(&mut self, chat: &Chat, scope: Option<&Scope>) -> Result<Option<u64>>;
}

/// Builds the replayer for a model. Runs on the worker thread, so the backend is owned there.
pub type Factory = Box<dyn FnMut(&str) -> Result<Box<dyn Replayer>> + Send>;

/// Monotonic time source; injectable so tests can advance time without sleeping.
pub type Clock = Box<dyn Fn() -> Instant + Send>;

/// Messages from the agent. Sends never block: when the queue is full the event is dropped.
pub enum Event {
    /// A real request settled; it becomes the prefix to keep warm.
    Settled {
        chat: Chat,
        model: String,
        prompt_tokens: u64,
        pricing: Option<TokenPricing>,
        tier: Tier,
        scope: Option<Scope>,
    },
    /// The agent run ended. A `streaming` warmer stops here; an `idle` warmer keeps going.
    RunIdle,
    /// The conversation context changed (rollover, compaction, session switch).
    ContextChanged,
    /// The model changed; the armed prefix belongs to the old model.
    ModelChanged(String),
    /// The user changed the warming mode.
    ModeChanged(Mode),
    /// Stop the worker thread.
    Shutdown,
}

/// What the worker observed. Drained by the agent for diagnostics and accounting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Report {
    /// A replay was sent; `cache_read_tokens` is what the provider reported reading.
    Replayed { cache_read_tokens: Option<u64> },
    /// Warming stopped or refused to arm.
    Stopped(Stop),
}

/// Agent-side handle. Dropping it disconnects the queue and the worker exits after any replay
/// already in flight.
pub struct Handle {
    tx: SyncSender<Event>,
    reports: Receiver<Report>,
}

impl Handle {
    /// Starts a worker thread that warms with `mode` and builds replayers through `factory`.
    pub fn spawn(mode: Mode, factory: Factory, clock: Clock) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let (report_tx, reports) = mpsc::sync_channel(QUEUE);
        let worker = Worker {
            mode,
            warmer: Warmer::default(),
            model: None,
            replayer: None,
            scope: None,
            factory,
            clock,
            reports: report_tx,
        };
        std::thread::Builder::new()
            .name("dial-cache-warm".into())
            .spawn(move || worker.run(rx))?;
        Ok(Self { tx, reports })
    }

    /// Queues an event without blocking. Returns `false` when it was dropped.
    pub fn send(&self, event: Event) -> bool {
        self.tx.try_send(event).is_ok()
    }

    /// Reports produced since the last drain, without blocking.
    pub fn drain(&self) -> Vec<Report> {
        let mut out = Vec::new();
        loop {
            match self.reports.try_recv() {
                Ok(report) => out.push(report),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return out,
            }
        }
    }

    /// Marks an agent run; dropping the guard reports `RunIdle` on every exit path.
    pub fn run_guard(&self) -> RunGuard {
        RunGuard(self.tx.clone())
    }
}

/// Reports the end of an agent run when dropped.
pub struct RunGuard(SyncSender<Event>);

impl Drop for RunGuard {
    fn drop(&mut self) {
        let _ = self.0.try_send(Event::RunIdle);
    }
}

struct Worker {
    mode: Mode,
    warmer: Warmer,
    /// Model the armed prefix was settled on; replays go to this model.
    model: Option<String>,
    replayer: Option<Box<dyn Replayer>>,
    scope: Option<Scope>,
    factory: Factory,
    clock: Clock,
    reports: SyncSender<Report>,
}

impl Worker {
    fn run(mut self, rx: Receiver<Event>) {
        loop {
            let now = (self.clock)();
            let wait = self
                .warmer
                .due()
                .map_or(POLL, |due| due.saturating_duration_since(now).min(POLL));
            match rx.recv_timeout(wait) {
                Ok(Event::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
                Ok(event) => self.handle(event),
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.tick();
        }
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::Settled {
                chat,
                model,
                prompt_tokens,
                pricing,
                tier,
                scope,
            } => {
                if self.model.as_deref() != Some(model.as_str()) {
                    self.replayer = None;
                }
                self.model = Some(model);
                self.scope = scope;
                let now = (self.clock)();
                if let Err(stop) =
                    self.warmer
                        .arm(&chat, prompt_tokens, pricing, tier, self.mode, now)
                {
                    self.report(Report::Stopped(stop));
                }
            }
            Event::RunIdle => {
                if let Some(stop) = self.warmer.run_ended() {
                    self.report(Report::Stopped(stop));
                }
            }
            Event::ContextChanged => self.disarm_for(Stop::Changed),
            Event::ModelChanged(model) => {
                if self.model.as_deref() != Some(model.as_str()) {
                    self.model = None;
                    self.replayer = None;
                    self.disarm_for(Stop::Changed);
                }
            }
            Event::ModeChanged(mode) => {
                self.mode = mode;
                if let Some(stop) = self.warmer.set_mode(mode) {
                    self.report(Report::Stopped(stop));
                }
            }
            Event::Shutdown => {}
        }
    }

    /// Disarms and reports, but only when something was actually armed.
    fn disarm_for(&mut self, stop: Stop) {
        if self.warmer.due().is_some() {
            self.warmer.disarm();
            self.report(Report::Stopped(stop));
        }
    }

    fn tick(&mut self) {
        let now = (self.clock)();
        match self.warmer.replay(now) {
            Ok(Some(chat)) => self.fire(&chat),
            Ok(None) => {}
            Err(stop) => self.report(Report::Stopped(stop)),
        }
    }

    fn fire(&mut self, chat: &Chat) {
        let Some(model) = self.model.clone() else {
            return;
        };
        if self.replayer.is_none() {
            match (self.factory)(&model) {
                Ok(replayer) => self.replayer = Some(replayer),
                Err(_) => {
                    self.warmer.disarm();
                    self.report(Report::Stopped(Stop::Unavailable));
                    return;
                }
            }
        }
        let Some(replayer) = self.replayer.as_mut() else {
            return;
        };
        match replayer.replay(chat, self.scope.as_ref()) {
            Ok(read) => {
                // A reply without cache usage cannot prove a hit, so it counts as a miss.
                let stop = self.warmer.record_replay(read.unwrap_or(0));
                self.report(Report::Replayed {
                    cache_read_tokens: read,
                });
                if let Some(stop) = stop {
                    self.report(Report::Stopped(stop));
                }
            }
            Err(_) => {
                self.replayer = None;
                self.warmer.disarm();
                self.report(Report::Stopped(Stop::Unavailable));
            }
        }
    }

    /// Best-effort: a full report queue drops the report rather than stalling the worker.
    fn report(&self, report: Report) {
        let _ = self.reports.try_send(report);
    }
}

#[cfg(test)]
mod tests;

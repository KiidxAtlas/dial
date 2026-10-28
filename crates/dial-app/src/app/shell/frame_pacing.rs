//! Adaptive repaint pacing for streaming output, plus a bounded record of UI stalls attributed to
//! the phase that caused them.
//!
//! The repaint interval follows the measured cost of the previous frame: spending at most about
//! half the time composing keeps input and the event loop responsive. A backlogged terminal defers
//! composition entirely. Deferred work is not queued; composition reads the current transcript at
//! paint time, so the next unforced repaint after the terminal drains paints everything at once.

use super::PAINT_EVERY;
use std::collections::VecDeque;
use std::time::Duration;

/// Slowest repaint interval, reached when a frame costs at least `MAX_PAINT_EVERY / 2`.
pub(super) const MAX_PAINT_EVERY: Duration = Duration::from_millis(200);
/// A callback phase at least this slow is recorded as a stall.
pub(super) const STALL_AFTER: Duration = Duration::from_millis(100);
/// Stalls kept for diagnosis; older entries are dropped first.
const STALL_CAPACITY: usize = 16;

/// The part of a streaming callback that consumed the time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum Phase {
    /// Draining terminal input.
    Input,
    /// Refreshing resources, workers, status and adapter state.
    Refresh,
    /// Writing the frame to the terminal.
    Paint,
}

/// One recorded stall.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) struct Stall {
    pub(in crate::app) phase: Phase,
    pub(in crate::app) elapsed: Duration,
}

/// Pacing state for streaming repaints. Owned by the shell so it survives across turns.
#[derive(Debug)]
pub(in crate::app) struct FramePacing {
    last_cost: Duration,
    stalls: VecDeque<Stall>,
    frames: u64,
    worst_frame: Duration,
}

impl Default for FramePacing {
    fn default() -> Self {
        Self {
            last_cost: Duration::ZERO,
            stalls: VecDeque::with_capacity(STALL_CAPACITY),
            frames: 0,
            worst_frame: Duration::ZERO,
        }
    }
}

impl FramePacing {
    /// The minimum time between streaming repaints: twice the last frame's cost, bounded below by
    /// `PAINT_EVERY` and above by `MAX_PAINT_EVERY`.
    pub(in crate::app) fn interval(&self) -> Duration {
        (self.last_cost * 2).clamp(PAINT_EVERY, MAX_PAINT_EVERY)
    }

    /// Record a completed frame's duration. It drives the next interval and the frame measures.
    pub(in crate::app) fn record_frame(&mut self, elapsed: Duration) {
        self.last_cost = elapsed;
        self.frames += 1;
        self.worst_frame = self.worst_frame.max(elapsed);
    }

    /// Record a phase's duration if it was slow enough to be a stall. The log is bounded.
    pub(in crate::app) fn record(&mut self, phase: Phase, elapsed: Duration) {
        if elapsed < STALL_AFTER {
            return;
        }
        if self.stalls.len() == STALL_CAPACITY {
            self.stalls.pop_front();
        }
        self.stalls.push_back(Stall { phase, elapsed });
    }

    /// Recorded stalls, oldest first.
    pub(in crate::app) fn stalls(&self) -> impl Iterator<Item = Stall> + '_ {
        self.stalls.iter().copied()
    }

    /// Summary for `/debug performance`: frame measures, the current interval, and recorded stalls.
    pub(in crate::app) fn report(&self) -> String {
        let stalls: Vec<String> = self
            .stalls()
            .map(|stall| format!("{} {} ms", stall.phase.label(), stall.elapsed.as_millis()))
            .collect();
        let stalls = if stalls.is_empty() {
            "none".to_owned()
        } else {
            stalls.join(", ")
        };
        format!(
            "frames painted: {}\nlast frame: {} ms, repaint interval: {} ms\nworst frame: {} ms\nstalls of {} ms or more (oldest first): {}",
            self.frames,
            self.last_cost.as_millis(),
            self.interval().as_millis(),
            self.worst_frame.as_millis(),
            STALL_AFTER.as_millis(),
            stalls,
        )
    }
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Refresh => "refresh",
            Self::Paint => "paint",
        }
    }
}

#[cfg(test)]
mod tests;

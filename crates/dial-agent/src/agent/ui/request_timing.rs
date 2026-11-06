//! Observed inference wall time, not an estimate of provider-side compute or token latency.
use crate::agent::RequestPhase;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const PHASES: usize = 6;

fn index(phase: RequestPhase) -> usize {
    match phase {
        RequestPhase::Waiting => 0,
        RequestPhase::Loading => 1,
        RequestPhase::Processing => 2,
        RequestPhase::Generating => 3,
        RequestPhase::Backoff => 4,
        RequestPhase::Retrying => 0,
        RequestPhase::Cancelling => 5,
    }
}

/// A gap this long between streamed deltas is a provider stall, not token generation.
const STALL: Duration = Duration::from_secs(10);

#[derive(Default)]
pub(super) struct Timing {
    phases: [Duration; PHASES],
    /// Silent gaps inside generation; a subset of the generation phase, not added to totals.
    stalls: Duration,
    requests: u64,
    failed: u64,
    retries: u64,
}
impl Timing {
    pub(super) fn snapshot(&self) -> Value {
        json!({
            "requests": self.requests,
            "failed_requests": self.failed,
            "retry_backoffs": self.retries,
            "response_wait_ms": self.phases[0].as_millis(),
            "loading_ms": self.phases[1].as_millis(),
            "prompt_processing_ms": self.phases[2].as_millis(),
            "generation_ms": self.phases[3].as_millis(),
            "generation_stall_ms": self.stalls.as_millis(),
            "retry_backoff_ms": self.phases[4].as_millis(),
            "cancellation_ms": self.phases[5].as_millis(),
            "total_ms": self.phases.iter().sum::<Duration>().as_millis(),
            "attribution": "Observed wall time by reported phase; response wait includes transport and unreported provider work. Generation includes reasoning and tool arguments; generation_stall_ms is the part of generation spent in silent stream gaps of at least 10 seconds and is already counted in generation_ms. Concurrent requests must not be summed as turn wall time."
        })
    }
    pub(super) fn add(&mut self, other: &Self) {
        for (total, value) in self.phases.iter_mut().zip(other.phases) {
            *total += value;
        }
        self.stalls += other.stalls;
        self.requests += other.requests;
        self.failed += other.failed;
        self.retries += other.retries;
    }
}

pub(super) struct RequestClock {
    start: Instant,
    changed: Instant,
    phase: RequestPhase,
    first_generation: Option<Duration>,
    last_delta: Option<Instant>,
    timing: Timing,
}
impl RequestClock {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            start: now,
            changed: now,
            phase: RequestPhase::Waiting,
            first_generation: None,
            last_delta: None,
            timing: Timing {
                requests: 1,
                ..Timing::default()
            },
        }
    }
    pub(super) fn phase(&mut self, phase: RequestPhase, now: Instant) {
        if self.phase == RequestPhase::Generating {
            self.stall_gap(now);
        }
        self.timing.phases[index(self.phase)] += now.saturating_duration_since(self.changed);
        if phase == RequestPhase::Backoff && self.phase != phase {
            self.timing.retries += 1;
        }
        if phase == RequestPhase::Generating {
            self.first_generation
                .get_or_insert(now.saturating_duration_since(self.start));
        }
        self.phase = phase;
        self.changed = now;
    }
    /// A streamed text, reasoning or argument delta arrived.
    pub(super) fn delta(&mut self, now: Instant) {
        if self.phase == RequestPhase::Generating {
            self.stall_gap(now);
        }
        self.last_delta = Some(now);
    }
    /// Counts the silence since the last delta (or generation start) once it reaches [`STALL`].
    fn stall_gap(&mut self, now: Instant) {
        let since = self.last_delta.unwrap_or(self.changed).max(self.changed);
        let gap = now.saturating_duration_since(since);
        if gap >= STALL {
            self.timing.stalls += gap;
        }
    }
    pub(super) fn finish(mut self, succeeded: bool, now: Instant) -> (Timing, Value) {
        self.phase(self.phase, now);
        self.timing.failed = u64::from(!succeeded);
        let mut report = self.timing.snapshot();
        report["first_generation_ms"] = json!(self.first_generation.map(|time| time.as_millis()));
        (self.timing, report)
    }
}

#[cfg(test)]
mod tests;

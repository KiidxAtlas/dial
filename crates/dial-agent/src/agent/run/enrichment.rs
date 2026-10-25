//! Optional post-edit hints are polled, never awaited. Exact effect and instruction identities
//! bind results to the context that requested them; they never become verification evidence.
use crate::agent::verify::evidence::Revision;
use crate::tools::knowledge::codebase::{Codebase, enrichment::Task};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::agent) struct Stamp {
    pub revision: Revision,
    pub instruction: u64,
}

#[derive(Default)]
pub(in crate::agent) struct Enrichment {
    pending: Option<(Stamp, Task)>,
}

impl Enrichment {
    /// Cancel and discard on every effect/steering change, even if the result is already ready.
    pub fn invalidate(&mut self, current: Stamp) {
        if self
            .pending
            .as_ref()
            .is_some_and(|(stamp, _)| *stamp != current)
        {
            self.pending = None;
        }
    }

    /// Returns false for a busy worker; optional enrichment never queues behind an older job.
    pub fn submit(&mut self, cache: &Codebase, paths: Vec<String>, current: Stamp) -> bool {
        self.invalidate(current);
        if self.pending.is_some() {
            return false;
        }
        self.pending = cache.try_impact_hints(paths).map(|task| (current, task));
        self.pending.is_some()
    }

    /// Ready hints can be attached to a later tool result or a bounded advisory request note.
    /// There is deliberately no wait, timeout, heartbeat or join on the turn's critical path.
    pub fn poll(&mut self, current: Stamp) -> Option<String> {
        self.invalidate(current);
        let hints = match self.pending.as_ref()?.1.try_take() {
            Ok(hints) => hints,
            Err(std::sync::mpsc::TryRecvError::Empty) => return None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.pending = None;
                return None;
            }
        };
        self.pending = None;
        (!hints.text.is_empty()).then_some(hints.text)
    }
}

#[cfg(test)]
mod tests;

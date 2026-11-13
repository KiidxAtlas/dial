//! Journal I/O counters exposed through `Session::io_metrics`.

use super::*;

/// Process-local, observational counters for one session handle's journal writes and syncs.
/// `*_attempts` counts calls that reached the operation, `*_failures` those that returned an
/// error, and `*_micros` sums wall-clock time spent (saturating). Nothing here is journaled,
/// persisted, or read by session logic, and it carries no message or control content.
/// Timings are inclusive: control publication can contain runtime flush, which can contain
/// reclaim sync. Their totals overlap and are not a partition of elapsed wall time.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct SessionIoMetrics {
    /// `sync_data` acknowledgements in `append_context_entry` (`append_custom_entry`,
    /// `replace_context_notes`, `reset_context`), not all context-transition sync paths.
    pub context_sync_attempts: u64,
    pub context_sync_failures: u64,
    pub context_sync_micros: u64,
    /// `flush_runtime` calls, including the flush every `append` performs first.
    pub runtime_flush_attempts: u64,
    pub runtime_flush_failures: u64,
    pub runtime_flush_micros: u64,
    /// Syncs before stream-journal reclamation; counted only when reclamation is due.
    pub runtime_reclaim_sync_attempts: u64,
    pub runtime_reclaim_sync_failures: u64,
    pub runtime_reclaim_sync_micros: u64,
    /// `set_control_state` calls.
    pub control_publication_attempts: u64,
    pub control_publication_failures: u64,
    pub control_publication_micros: u64,
    /// Successful publications whose patch was empty (equal to the merged branch state). They are
    /// still appended; this only observes them.
    pub equal_control_publications: u64,
    pub equal_control_publication_micros: u64,
}

impl SessionIoMetrics {
    /// Counter growth from `earlier` to `self`, saturating at zero if the snapshots are reversed.
    pub fn since(&self, earlier: &SessionIoMetrics) -> SessionIoMetrics {
        SessionIoMetrics {
            context_sync_attempts: self
                .context_sync_attempts
                .saturating_sub(earlier.context_sync_attempts),
            context_sync_failures: self
                .context_sync_failures
                .saturating_sub(earlier.context_sync_failures),
            context_sync_micros: self
                .context_sync_micros
                .saturating_sub(earlier.context_sync_micros),
            runtime_flush_attempts: self
                .runtime_flush_attempts
                .saturating_sub(earlier.runtime_flush_attempts),
            runtime_flush_failures: self
                .runtime_flush_failures
                .saturating_sub(earlier.runtime_flush_failures),
            runtime_flush_micros: self
                .runtime_flush_micros
                .saturating_sub(earlier.runtime_flush_micros),
            runtime_reclaim_sync_attempts: self
                .runtime_reclaim_sync_attempts
                .saturating_sub(earlier.runtime_reclaim_sync_attempts),
            runtime_reclaim_sync_failures: self
                .runtime_reclaim_sync_failures
                .saturating_sub(earlier.runtime_reclaim_sync_failures),
            runtime_reclaim_sync_micros: self
                .runtime_reclaim_sync_micros
                .saturating_sub(earlier.runtime_reclaim_sync_micros),
            control_publication_attempts: self
                .control_publication_attempts
                .saturating_sub(earlier.control_publication_attempts),
            control_publication_failures: self
                .control_publication_failures
                .saturating_sub(earlier.control_publication_failures),
            control_publication_micros: self
                .control_publication_micros
                .saturating_sub(earlier.control_publication_micros),
            equal_control_publications: self
                .equal_control_publications
                .saturating_sub(earlier.equal_control_publications),
            equal_control_publication_micros: self
                .equal_control_publication_micros
                .saturating_sub(earlier.equal_control_publication_micros),
        }
    }
}

/// Saturating increment and elapsed-micros accumulation for the metric counters above.
pub(super) fn bump(counter: &mut u64) {
    *counter = counter.saturating_add(1);
}

pub(super) fn add_elapsed(counter: &mut u64, started: Instant) -> u64 {
    let micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    *counter = counter.saturating_add(micros);
    micros
}

// ---------------------------------------------------------------- time / ids

impl Session {
    /// Snapshot of this handle's process-local write/sync counters; diff snapshots with `since`.
    pub fn io_metrics(&self) -> SessionIoMetrics {
        self.io
    }
}

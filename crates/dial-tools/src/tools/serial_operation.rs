//! Transferable workspace exclusion: a running native cell keeps the same lease when adopted.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Exclusive workspace operation ownership. Unlike a thread-bound mutex guard, this lease
/// can move with a running cell into its background owner. Dropping it releases the workspace.
#[derive(Debug)]
#[must_use = "dropping the lease releases workspace operation ownership"]
pub struct SerialOperation(Arc<AtomicBool>);

impl SerialOperation {
    pub(super) fn try_acquire(state: &Arc<AtomicBool>) -> Option<Self> {
        state
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| Self(state.clone()))
    }
}

impl Drop for SerialOperation {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests;

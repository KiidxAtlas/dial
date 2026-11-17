//! Background reads of the plain RPC controller, bounded and scoped to one connection.
//!
//! A read runs on its own thread so the controller keeps reading requests. Admission is capped at
//! [`MAX_CONCURRENT_READS`]: past it a read is refused with a coded `busy` at once, never queued.
//! Every job belongs to its connection. [`ReadJobs::close`] runs when the connection ends (EOF,
//! shutdown or a dead transport): new reads are refused, answers that have not been delivered are
//! suppressed, and cancellable work sees the connection's cancel token. Work that cannot be
//! cancelled finishes on its own thread under its owner's limits and never blocks the controller.
#[cfg(test)]
use super::RequestError;
use super::coded;
use anyhow::Result;
use dial_protocol::ErrorCode;
use serde_json::Value;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
};

/// Reads one connection may have in flight at once.
pub(super) const MAX_CONCURRENT_READS: usize = 4;

/// The background reads of one control connection.
pub(super) struct ReadJobs {
    in_flight: Arc<AtomicUsize>,
    /// Connection lifetime. Set once the connection ends; it is also the cancel token that
    /// cancellable work receives.
    closed: Arc<AtomicBool>,
}

impl ReadJobs {
    pub(super) fn new() -> Self {
        Self {
            in_flight: Arc::new(AtomicUsize::new(0)),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Starts `work` on its own thread and passes its result to `deliver`, unless the connection
    /// closed before the work finished. `work` receives the connection's cancel token.
    ///
    /// # Errors
    /// Nothing is started and the reason is returned (the caller answers the request with it)
    /// when the connection has closed or [`MAX_CONCURRENT_READS`] reads are already in flight.
    pub(super) fn spawn(
        &self,
        work: impl FnOnce(Arc<AtomicBool>) -> Result<Value> + Send + 'static,
        deliver: impl FnOnce(Result<Value>) + Send + 'static,
    ) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(coded(ErrorCode::Busy, "the control connection is closing"));
        }
        let slot = Slot::acquire(&self.in_flight).ok_or_else(|| {
            coded(
                ErrorCode::Busy,
                format!(
                    "{MAX_CONCURRENT_READS} reads are already running; retry when one finishes"
                ),
            )
        })?;
        let closed = self.closed.clone();
        thread::Builder::new()
            .name("rpc-read".into())
            .spawn(move || {
                // Held for the whole job, so the slot frees when the work and its answer are done.
                let _slot = slot;
                let result = work(closed.clone());
                if !closed.load(Ordering::SeqCst) {
                    deliver(result);
                }
            })
            .map(drop)
            .map_err(|error| {
                coded(
                    ErrorCode::Failed,
                    format!("could not start a read: {error}"),
                )
            })
    }

    /// Ends the connection's reads: refuses new ones and suppresses answers not yet delivered.
    pub(super) fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    /// Reads currently holding a slot.
    #[cfg(test)]
    pub(super) fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }
}

/// One admitted read; its slot is released when the read's thread finishes or unwinds.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn acquire(in_flight: &Arc<AtomicUsize>) -> Option<Slot> {
        let mut current = in_flight.load(Ordering::SeqCst);
        loop {
            if current >= MAX_CONCURRENT_READS {
                return None;
            }
            match in_flight.compare_exchange_weak(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Some(Slot(in_flight.clone())),
                Err(actual) => current = actual,
            }
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The refusal code of a request `spawn` declined, for tests and callers that inspect it.
#[cfg(test)]
pub(super) fn refusal_code(error: &anyhow::Error) -> Option<ErrorCode> {
    error
        .downcast_ref::<RequestError>()
        .map(|refused| refused.code)
}

#[cfg(test)]
mod tests;

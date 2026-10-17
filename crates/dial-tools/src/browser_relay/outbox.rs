//! Bounded per-connection output queue for the browser relay.
//!
//! Every frame queued for one socket is charged against a frame and byte budget until the socket
//! writer takes it off the queue. Nothing waits for space: a full queue or an exhausted byte budget
//! marks the connection overloaded, the frame is refused, and the owner disconnects that one peer.
//! The shape follows the collaboration relay's bounded recipient queue (`src/collab/server.rs`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{
    self, Receiver, RecvError, RecvTimeoutError, SyncSender, TryRecvError, TrySendError,
};
use std::time::Duration;

/// Frames that may wait for one socket.
pub const MAX_QUEUED_FRAMES: usize = 1024;
/// Bytes that may wait for one socket, counted over frame text. Large screenshots fit under it.
pub const MAX_QUEUED_BYTES: usize = 64 * 1024 * 1024;

/// Refusal of a frame: the connection is overloaded or already gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused;

struct Shared {
    queued_bytes: AtomicUsize,
    overloaded: AtomicBool,
}

/// Sending half. Clones share one budget and one overload flag.
#[derive(Clone)]
pub struct Outbox {
    tx: SyncSender<String>,
    shared: Arc<Shared>,
}

/// Receiving half, owned by the socket writer. Taking a frame releases its budget.
pub struct OutboxReceiver {
    rx: Receiver<String>,
    shared: Arc<Shared>,
}

/// Creates a bounded queue for one connection.
pub fn channel() -> (Outbox, OutboxReceiver) {
    let (tx, rx) = mpsc::sync_channel(MAX_QUEUED_FRAMES);
    let shared = Arc::new(Shared {
        queued_bytes: AtomicUsize::new(0),
        overloaded: AtomicBool::new(false),
    });
    (
        Outbox {
            tx,
            shared: Arc::clone(&shared),
        },
        OutboxReceiver { rx, shared },
    )
}

impl Outbox {
    /// Queues one frame without waiting. Overflow marks the connection overloaded and refuses the
    /// frame; a receiver that is already gone refuses it without marking anything.
    pub fn send(&self, frame: String) -> Result<(), Refused> {
        if self.shared.overloaded.load(Ordering::SeqCst) {
            return Err(Refused);
        }
        let len = frame.len();
        let previous = self.shared.queued_bytes.fetch_add(len, Ordering::SeqCst);
        if previous.saturating_add(len) > MAX_QUEUED_BYTES {
            self.shared.queued_bytes.fetch_sub(len, Ordering::SeqCst);
            self.shared.overloaded.store(true, Ordering::SeqCst);
            return Err(Refused);
        }
        match self.tx.try_send(frame) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.shared.queued_bytes.fetch_sub(len, Ordering::SeqCst);
                self.shared.overloaded.store(true, Ordering::SeqCst);
                Err(Refused)
            }
            Err(TrySendError::Disconnected(_)) => {
                self.shared.queued_bytes.fetch_sub(len, Ordering::SeqCst);
                Err(Refused)
            }
        }
    }

    /// Whether this connection has been refused a frame for exceeding its budget.
    pub fn is_overloaded(&self) -> bool {
        self.shared.overloaded.load(Ordering::SeqCst)
    }
}

impl OutboxReceiver {
    pub fn try_recv(&self) -> Result<String, TryRecvError> {
        let frame = self.rx.try_recv()?;
        self.release(&frame);
        Ok(frame)
    }

    pub fn recv(&self) -> Result<String, RecvError> {
        let frame = self.rx.recv()?;
        self.release(&frame);
        Ok(frame)
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<String, RecvTimeoutError> {
        let frame = self.rx.recv_timeout(timeout)?;
        self.release(&frame);
        Ok(frame)
    }

    /// Whether the writer must disconnect: a frame was refused because the budget ran out.
    pub fn is_overloaded(&self) -> bool {
        self.shared.overloaded.load(Ordering::SeqCst)
    }

    fn release(&self, frame: &str) {
        self.shared
            .queued_bytes
            .fetch_sub(frame.len(), Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests;

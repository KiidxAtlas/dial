//! Bounded semantic notifications: repeated invalidations share one pending slot.
use super::Signal;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::{Arc, mpsc};
#[cfg(test)]
use std::time::Duration;

#[derive(Clone)]
pub(super) struct Sender {
    pending: Arc<Mutex<VecDeque<Signal>>>,
    wake: mpsc::SyncSender<()>,
}

pub(super) struct Receiver {
    pending: Arc<Mutex<VecDeque<Signal>>>,
    wake: mpsc::Receiver<()>,
}

pub(super) fn channel() -> (Sender, Receiver) {
    let pending = Arc::new(Mutex::new(VecDeque::new()));
    let (tx, rx) = mpsc::sync_channel(1);
    (
        Sender {
            pending: pending.clone(),
            wake: tx,
        },
        Receiver { pending, wake: rx },
    )
}

impl Sender {
    pub fn send(&self, signal: Signal) {
        let mut pending = self.pending.lock();
        // Degradation is also an invalidation, so it must survive a later Healthy notice.
        // Moving repeated kinds to the back preserves the final health state of a burst.
        pending.retain(|old| std::mem::discriminant(old) != std::mem::discriminant(&signal));
        if matches!(signal, Signal::Closed) {
            pending.push_front(signal);
        } else {
            pending.push_back(signal);
        }
        let _ = self.wake.try_send(());
    }
}

impl Receiver {
    pub fn coalesce(&self, signal: &Signal) {
        self.pending
            .lock()
            .retain(|next| std::mem::discriminant(next) != std::mem::discriminant(signal));
    }

    pub fn recv(&self) -> Result<Signal, mpsc::RecvError> {
        loop {
            if let Some(signal) = self.pending.lock().pop_front() {
                return Ok(signal);
            }
            self.wake.recv()?;
        }
    }

    #[cfg(test)]
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Signal, mpsc::RecvTimeoutError> {
        let end = std::time::Instant::now() + timeout;
        loop {
            if let Some(signal) = self.pending.lock().pop_front() {
                return Ok(signal);
            }
            self.wake
                .recv_timeout(end.saturating_duration_since(std::time::Instant::now()))?;
        }
    }

    #[cfg(test)]
    pub fn try_recv(&self) -> Result<Signal, mpsc::TryRecvError> {
        self.pending
            .lock()
            .pop_front()
            .ok_or(mpsc::TryRecvError::Empty)
    }
}

#[cfg(test)]
mod tests;

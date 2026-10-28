//! Local ownership of admitted transport work. Cancellation is not remote rollback.
use super::*;

pub(super) const MAX_IN_FLIGHT: usize = 16;
pub(in super::super) const MAX_REQUEST_LIFETIME: Duration = Duration::from_secs(3600);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Queued,
    Dispatching,
    Delivered,
    Cancelled,
}

pub(in super::super) struct Operation {
    pub cancel: Arc<AtomicBool>,
    phase: Mutex<Phase>,
    delivery: std::sync::Condvar,
    pub deadline: Instant,
}

impl Operation {
    pub fn new(lifetime: Duration) -> Arc<Self> {
        Self::with_cancel(lifetime, Arc::new(AtomicBool::new(false)))
    }

    pub fn with_cancel(lifetime: Duration, cancel: Arc<AtomicBool>) -> Arc<Self> {
        Arc::new(Self {
            cancel,
            phase: Mutex::new(Phase::Queued),
            delivery: std::sync::Condvar::new(),
            deadline: Instant::now() + lifetime,
        })
    }

    /// Linearization point shared with cancellation: a cancelled queue entry cannot dispatch.
    pub fn dispatch(&self) -> bool {
        let mut phase = lock(&self.phase);
        if self.stopped() || *phase == Phase::Cancelled {
            return false;
        }
        *phase = Phase::Dispatching;
        true
    }

    pub fn delivered(&self) {
        *lock(&self.phase) = Phase::Delivered;
        self.delivery.notify_all();
    }

    /// A protocol reply can beat the writer's post-write bookkeeping. Wait without spinning,
    /// but wake at the caller's polling interval so cancellation and deadlines still apply.
    pub(super) fn wait_for_delivery(&self, timeout: Duration) {
        let phase = lock(&self.phase);
        let _ = self
            .delivery
            .wait_timeout_while(phase, timeout, |phase| *phase == Phase::Dispatching)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }

    pub fn stopped(&self) -> bool {
        self.cancel.load(Ordering::SeqCst) || Instant::now() >= self.deadline
    }

    pub(super) fn phase(&self) -> Phase {
        *lock(&self.phase)
    }

    pub(super) fn cancel(&self) -> Phase {
        let mut phase = lock(&self.phase);
        self.cancel.store(true, Ordering::SeqCst);
        let previous = *phase;
        if previous == Phase::Queued {
            *phase = Phase::Cancelled;
        }
        previous
    }
}

pub(super) struct Outgoing {
    pub message: Value,
    pub operation: Arc<Operation>,
    pub delivered: Option<Sender<Result<()>>>,
}

/// Joining is essential: removing a waiter is not settlement of the transport worker.
pub(super) struct OwnedSend {
    pub operation: Arc<Operation>,
    pub worker: Option<thread::JoinHandle<()>>,
}
impl Drop for OwnedSend {
    fn drop(&mut self) {
        self.operation.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Admission remains held until the request's owned worker has settled.
pub(super) struct Admission<'a>(&'a Mutex<usize>);
impl<'a> Admission<'a> {
    pub fn acquire(count: &'a Mutex<usize>) -> Option<Self> {
        let mut current = lock(count);
        if *current >= MAX_IN_FLIGHT {
            return None;
        }
        *current += 1;
        Some(Self(count))
    }
}
impl Drop for Admission<'_> {
    fn drop(&mut self) {
        *lock(self.0) -= 1;
    }
}

//! Per-call steering signals for a running tool.
//!
//! The agent installs a [`CallSignals`] on the worker thread that runs a tool call. Tools never
//! receive the agent itself; they poll these thread-local flags from their own wait loops:
//!
//! - [`steering_requested`] is cooperative. The user queued a new message while this call runs.
//!   Long-running work SHOULD step aside without losing effects (move itself to a background job,
//!   return partial output). It must never kill or roll back side effects; the hard interrupt
//!   flag a tool already receives is the only kill switch.
//! - [`wait_interrupted`] is for pure waits (waiting on a background job, a subagent, a server
//!   becoming ready). A wait has nothing to finish, so it ends now and reports what it has.
//!
//! Outside an agent tool call both flags read false, so direct calls and tests are unaffected.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Default, Debug)]
pub struct CallSignals {
    steer: Arc<AtomicBool>,
    wait: Arc<AtomicBool>,
}

impl CallSignals {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask running work to step aside. Idempotent.
    pub fn request_steering(&self) {
        self.steer.store(true, Ordering::Release);
    }

    /// End pure waits. Idempotent.
    pub fn interrupt_waits(&self) {
        self.wait.store(true, Ordering::Release);
        crate::tools::exec::background::notify_cancel();
        crate::platform::wake::signal();
    }

    /// Owner-only reset after draining the inbox under its lock.
    pub fn clear(&self) {
        self.steer.store(false, Ordering::Release);
        self.wait.store(false, Ordering::Release);
    }

    pub fn steering_requested(&self) -> bool {
        self.steer.load(Ordering::Acquire)
    }

    pub fn wait_interrupted(&self) -> bool {
        self.wait.load(Ordering::Acquire)
    }
}

thread_local! {
    static CURRENT: RefCell<Option<CallSignals>> = const { RefCell::new(None) };
}

/// Run `f` with `signals` installed for this thread, restoring the previous signals afterwards
/// (nested tool dispatch keeps the outer call's signals once the inner call returns).
pub fn scope<R>(signals: &CallSignals, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<CallSignals>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            CURRENT.with(|current| *current.borrow_mut() = previous);
        }
    }
    let previous = CURRENT.with(|current| current.borrow_mut().replace(signals.clone()));
    let _restore = Restore(previous);
    f()
}

/// The signals of the tool call running on this thread. Capture before spawning helper threads
/// and install them there with [`scope`].
#[cfg(test)]
pub fn current() -> Option<CallSignals> {
    CURRENT.with(|current| current.borrow().clone())
}

/// The user queued steering while this call runs; step aside cooperatively.
pub fn steering_requested() -> bool {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .is_some_and(CallSignals::steering_requested)
    })
}

/// A pure wait in this call must end now.
pub fn wait_interrupted() -> bool {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .is_some_and(CallSignals::wait_interrupted)
    })
}

#[cfg(test)]
mod tests;

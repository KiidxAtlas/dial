//! Process interrupt (Ctrl-C) bridge: a shared cancellation flag that the signal handler sets.

use super::sys;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

static INTERRUPT: AtomicBool = AtomicBool::new(false);

fn on_interrupt() {
    if INTERRUPT.swap(true, Ordering::SeqCst) {
        sys::exit_now(130);
    }
}

pub fn install_sigint() -> Arc<AtomicBool> {
    sys::on_interrupt(on_interrupt);
    let shared = Arc::new(AtomicBool::new(false));
    let mirror = shared.clone();
    std::thread::spawn(move || {
        // The thread's own clone is the last one once the caller drops its flag; stop then so
        // each `install_sigint` call does not leave a poller running for the rest of the process.
        while Arc::strong_count(&mirror) > 1 {
            // Never overwrite a cancellation raised by the raw-terminal event loop.
            if INTERRUPT.load(Ordering::SeqCst) {
                mirror.store(true, Ordering::SeqCst);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });
    shared
}

pub fn clear_interrupt() {
    INTERRUPT.store(false, Ordering::SeqCst);
}

//! Lifetime of the scripted loopback servers used by the image transport fixtures.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;

/// A scripted loopback server thread. [`Server::join`] stops it as soon as the client is done
/// instead of letting it wait out an idle period, which held every test open for seconds.
pub(super) struct Server {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Vec<String>>,
}

impl Server {
    /// Runs `serve` on its own thread. `serve` returns the requests it received once the flag is
    /// set and no connection is pending, or at its own safety bound if the server is never joined.
    pub(super) fn spawn(serve: impl FnOnce(&AtomicBool) -> Vec<String> + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        Self {
            stop,
            handle: std::thread::spawn(move || serve(&flag)),
        }
    }

    /// Call once the client has returned: no request is in flight, so the server stops after
    /// accepting anything already queued. Returns every request received.
    pub(super) fn join(self) -> std::thread::Result<Vec<String>> {
        self.stop.store(true, Ordering::SeqCst);
        self.handle.join()
    }
}

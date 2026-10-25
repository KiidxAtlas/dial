//! Read-only runtime update checks. Installation is exclusively an explicit `/setup update` job.
use crate::setup::deps::{Env, updates};
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, TryRecvError},
};
use std::time::{SystemTime, UNIX_EPOCH};

const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

#[derive(Default)]
pub(super) struct RuntimeUpdates {
    loaded: bool,
    last_attempt: Option<u64>,
    pending: Option<Receiver<String>>,
    cancel: Arc<AtomicBool>,
}

fn due(now: u64, last: u64) -> bool {
    // A future stamp (clock correction or corrupt state) must not disable checks indefinitely.
    now.checked_sub(last)
        .is_none_or(|elapsed| elapsed >= CHECK_INTERVAL_SECS)
}

impl RuntimeUpdates {
    pub fn poll(&mut self, user_dir: &Path) -> Option<String> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        let env = Env::current(user_dir);
        self.poll_at(user_dir, now, move |cancel| {
            updates::check(&env, cancel).map(|report| report.text())
        })
    }

    fn poll_at(
        &mut self,
        user_dir: &Path,
        now: u64,
        check: impl FnOnce(&AtomicBool) -> anyhow::Result<String> + Send + 'static,
    ) -> Option<String> {
        if let Some(receiver) = &self.pending {
            match receiver.try_recv() {
                Ok(message) => {
                    self.pending = None;
                    return Some(message);
                }
                Err(TryRecvError::Disconnected) => {
                    self.pending = None;
                    return Some("Runtime update check stopped unexpectedly. Use /setup updates to retry; no updates were installed.".into());
                }
                Err(TryRecvError::Empty) => return None,
            }
        }
        let stamp = user_dir.join("runtime-update-check");
        if !self.loaded {
            self.loaded = true;
            self.last_attempt = std::fs::read_to_string(&stamp)
                .ok()
                .and_then(|value| value.trim().parse().ok());
        }
        if self.last_attempt.is_some_and(|last| !due(now, last)) {
            return None;
        }
        self.last_attempt = Some(now);
        let (send, receive) = mpsc::channel();
        self.pending = Some(receive);
        let cancel = self.cancel.clone();
        std::thread::spawn(move || {
            // Throttle attempts, including offline failures. Explicit checks bypass this stamp.
            let saved =
                crate::config::settings_store::write_private(&stamp, now.to_string().as_bytes());
            let mut message = match check(&cancel) {
                Ok(text) => text,
                Err(error) => format!(
                    "Runtime update check failed: {error:#}. Use /setup updates to retry; no updates were installed."
                ),
            };
            if let Err(error) = saved {
                message.push_str(&format!(
                    "\nCould not save update-check timestamp: {error:#}"
                ));
            }
            let _ = send.send(message);
        });
        None
    }
}

impl Drop for RuntimeUpdates {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        // Never join a network/package-manager query on the UI thread. The bounded checker
        // observes cancellation and cleans up its own process group.
    }
}

#[cfg(test)]
mod tests;

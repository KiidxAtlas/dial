//! Tells the user when a newer Dial release is published. Read-only: installing is `/update` or
//! `dial update`, which follow the same [`release::UpdatePlan`]. The latest tag is cached so every
//! launch can show the notice at once, and GitHub is asked at most once a day, off the UI thread.
use crate::release::{self, UpdatePlan};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;
const QUERY_TIMEOUT: Duration = Duration::from_secs(15);
/// `<unix seconds of the last attempt>\n<latest tag, empty when unknown>`.
const STAMP: &str = "dial-release-check";

#[derive(Default)]
pub(super) struct ReleaseNotice {
    loaded: bool,
    checked_at: Option<u64>,
    latest: Option<String>,
    pending: Option<Receiver<Option<String>>>,
    message: Option<String>,
}

/// The chrome line for `latest`, or `None` when this executable is not older.
fn message(latest: &str, current: &str, plan: &UpdatePlan) -> Option<String> {
    if !release::is_newer(latest, current) {
        return None;
    }
    let version = latest.trim_start_matches('v');
    let how = match plan {
        UpdatePlan::Manual(how) => how.as_str(),
        _ => "run `/update`",
    };
    Some(format!(
        "Dial {version} is available (you have {current}) · {how}"
    ))
}

fn read_stamp(path: &Path) -> (Option<u64>, Option<String>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (None, None);
    };
    let mut lines = text.lines();
    let checked_at = lines.next().and_then(|line| line.trim().parse().ok());
    let latest = lines
        .next()
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .and_then(|tag| release::tag(tag).ok());
    (checked_at, latest)
}

fn due(now: u64, last: u64) -> bool {
    // A future stamp (clock correction or corrupt state) must not disable checks indefinitely.
    now.checked_sub(last)
        .is_none_or(|elapsed| elapsed >= CHECK_INTERVAL_SECS)
}

impl ReleaseNotice {
    /// Background maintenance is editor chrome, never conversation history.
    pub fn notice(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// Loads the cached result and starts a due check. Returns whether the notice changed.
    pub fn poll(&mut self, user_dir: &Path) -> bool {
        let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
            return false;
        };
        self.poll_at(user_dir, now.as_secs(), || {
            release::latest_tag(QUERY_TIMEOUT)
        })
    }

    fn poll_at(
        &mut self,
        user_dir: &Path,
        now: u64,
        query: impl FnOnce() -> anyhow::Result<String> + Send + 'static,
    ) -> bool {
        let before = self.message.clone();
        let stamp = user_dir.join(STAMP);
        if !self.loaded {
            self.loaded = true;
            (self.checked_at, self.latest) = read_stamp(&stamp);
        }
        if let Some(receiver) = &self.pending {
            match receiver.try_recv() {
                Ok(latest) => {
                    self.pending = None;
                    if latest.is_some() {
                        self.latest = latest;
                    }
                }
                Err(TryRecvError::Disconnected) => self.pending = None,
                Err(TryRecvError::Empty) => {}
            }
        } else if self.checked_at.is_none_or(|last| due(now, last)) {
            self.checked_at = Some(now);
            let (send, receive) = mpsc::channel();
            self.pending = Some(receive);
            let known = self.latest.clone();
            std::thread::spawn(move || {
                // A failed query is quiet and throttled like a successful one; the previous
                // answer stays cached so an offline launch still shows a known update.
                let latest = query().ok();
                let saved = latest
                    .as_ref()
                    .or(known.as_ref())
                    .cloned()
                    .unwrap_or_default();
                let _ = crate::config::settings_store::write_private(
                    &stamp,
                    format!("{now}\n{saved}\n").as_bytes(),
                );
                let _ = send.send(latest);
            });
        }
        self.message = self
            .latest
            .as_deref()
            .and_then(|latest| message(latest, release::CURRENT, &release::current_plan()));
        self.message != before
    }
}

#[cfg(test)]
mod tests;

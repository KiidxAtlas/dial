//! Tells the user when a newer Dial release is published. Read-only: installing is `/update` or
//! `dial update`, which follow the same [`release::UpdatePlan`]. The latest tag is cached so every
//! launch can show the notice at once, and GitHub is asked at most once a day, off the UI thread.
use crate::release::{self, UpdatePlan};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;
/// How long the chrome line stays pinned for one release; then that release is remembered as seen
/// and only the inbox keeps the notice.
const SHOW_FOR_SECS: u64 = 5 * 60;
const QUERY_TIMEOUT: Duration = Duration::from_secs(15);
/// `<unix seconds of the last attempt>\n<latest tag, empty when unknown>[\n<tag whose chrome line
/// was already shown>]`.
#[cfg(test)]
const STAMP: &str = "dial-release-check";

#[derive(Default)]
pub(super) struct ReleaseNotice {
    loaded: bool,
    checked_at: Option<u64>,
    latest: Option<String>,
    /// The release whose chrome line was shown its full time, in this or an earlier session.
    seen: Option<String>,
    /// When this session first pinned the chrome line for `latest`.
    shown_since: Option<u64>,
    pending: Option<Receiver<Option<String>>>,
    message: Option<String>,
}

fn stamp_text(checked_at: u64, latest: &str, seen: Option<&str>) -> String {
    match seen {
        Some(seen) => format!("{checked_at}\n{latest}\n{seen}\n"),
        None => format!("{checked_at}\n{latest}\n"),
    }
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

fn read_stamp(path: &Path) -> (Option<u64>, Option<String>, Option<String>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (None, None, None);
    };
    let mut lines = text.lines();
    let checked_at = lines.next().and_then(|line| line.trim().parse().ok());
    let mut tag = || {
        lines
            .next()
            .map(str::trim)
            .filter(|tag| !tag.is_empty())
            .and_then(|tag| release::tag(tag).ok())
    };
    let latest = tag();
    let seen = tag();
    (checked_at, latest, seen)
}

fn due(now: u64, last: u64) -> bool {
    // A future stamp (clock correction or corrupt state) must not disable checks indefinitely.
    now.checked_sub(last)
        .is_none_or(|elapsed| elapsed >= CHECK_INTERVAL_SECS)
}

impl ReleaseNotice {
    /// The update message for the inbox while a newer release exists.
    pub fn notice(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// The pinned chrome line: shown for [`SHOW_FOR_SECS`] once per release, never forever. The
    /// inbox keeps the message after the line leaves.
    pub fn chrome_notice(&self) -> Option<&str> {
        self.message.as_deref().filter(|_| self.seen != self.latest)
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
        let before = (self.message.clone(), self.chrome_notice().is_some());
        let stamp = crate::config::paths::release_check_file(user_dir);
        if !self.loaded {
            self.loaded = true;
            (self.checked_at, self.latest, self.seen) = read_stamp(&stamp);
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
            let seen = self.seen.clone();
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
                    stamp_text(now, &saved, seen.as_deref()).as_bytes(),
                );
                let _ = send.send(latest);
            });
        }
        self.message = self
            .latest
            .as_deref()
            .and_then(|latest| message(latest, release::CURRENT, &release::current_plan()));
        self.retire_chrome(user_dir, now);
        (self.message.clone(), self.chrome_notice().is_some()) != before
    }

    /// Counts the pinned line's time and, once it has been up long enough, records the release as
    /// seen in the stamp so later launches do not pin it again.
    fn retire_chrome(&mut self, user_dir: &Path, now: u64) {
        if self.chrome_notice().is_none() {
            self.shown_since = None;
            return;
        }
        let since = *self.shown_since.get_or_insert(now);
        // The background check rewrites the stamp; record the release once it has finished.
        if now.saturating_sub(since) < SHOW_FOR_SECS || self.pending.is_some() {
            return;
        }
        self.seen = self.latest.clone();
        self.shown_since = None;
        let stamp = crate::config::paths::release_check_file(user_dir);
        let _ = crate::config::settings_store::write_private(
            &stamp,
            stamp_text(
                self.checked_at.unwrap_or(now),
                self.latest.as_deref().unwrap_or_default(),
                self.seen.as_deref(),
            )
            .as_bytes(),
        );
    }
}

#[cfg(test)]
mod tests;

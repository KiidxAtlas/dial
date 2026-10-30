//! Bounded provider retries for transient requests, throttles and explicitly incomplete streams.
//!
//! Only explicit "wait this long" headers are honoured as a minimum delay (`retry-after`,
//! `retry-after-ms`, `x-ratelimit-reset`, `x-ratelimit-reset-ms`).
//! Rate-limit *window* headers (`x-ratelimit-reset-requests/-tokens`,
//! `anthropic-ratelimit-*-reset`) say when a bucket refills, not how long a throttled request
//! must wait, so they are recorded only as a quota reset time and never force a wait.
use super::errors::ErrorClass;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A request-local handle for explicitly skipping a pending provider retry wait.
/// Commands issued during inference or after the wait ends never affect a later retry.
#[derive(Debug, Clone, Default)]
pub struct RetryControl {
    state: Arc<parking_lot::Mutex<WaitState>>,
}

#[derive(Debug, Default, PartialEq, Eq)]
enum WaitState {
    #[default]
    Idle,
    Waiting,
    Requested,
}

impl RetryControl {
    /// Release the current wait, including a server-requested delay. Returns false if there is
    /// no pending wait or an immediate retry has already been requested.
    pub fn request_now(&self) -> bool {
        let mut state = self.state.lock();
        if *state != WaitState::Waiting {
            return false;
        }
        *state = WaitState::Requested;
        true
    }

    pub(crate) fn begin_wait(&self) -> PendingWait {
        *self.state.lock() = WaitState::Waiting;
        PendingWait(self.clone())
    }
}

pub(crate) struct PendingWait(RetryControl);

impl PendingWait {
    pub(crate) fn wait(
        &self,
        delay: Duration,
        interrupt: &AtomicBool,
    ) -> Result<(), super::Interrupted> {
        let start = Instant::now();
        loop {
            if interrupt.load(Ordering::SeqCst) {
                return Err(super::Interrupted);
            }
            if *self.0.state.lock() == WaitState::Requested || start.elapsed() >= delay {
                return Ok(());
            }
            std::thread::sleep(
                delay
                    .saturating_sub(start.elapsed())
                    .min(Duration::from_millis(50)),
            );
        }
    }
}

impl Drop for PendingWait {
    fn drop(&mut self) {
        *self.0.state.lock() = WaitState::Idle;
    }
}

/// Attempts on one model, including the first, before a failure a configured fallback accepts
/// moves the request on to that fallback instead of waiting out further retries.
pub(crate) const FAILOVER_ATTEMPTS: usize = 2;
/// Longest wait before a retry while a fallback is ready; a longer one fails over instead.
pub(crate) const FAILOVER_MAX_WAIT: Duration = Duration::from_secs(10);
/// Keep a ready fallback's brief first retry independent of the ordinary automatic interval.
const FAILOVER_INTERVAL: Duration = Duration::from_secs(1);

/// Server timing hints from one non-2xx response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ServerHints {
    /// Explicit minimum wait before retrying this request.
    pub(crate) delay: Option<Duration>,
    /// When the provider says its rate-limit or quota window refills.
    pub(crate) reset_at: Option<SystemTime>,
}

/// Read the explicit retry delay and the quota-window reset time from response headers.
pub(crate) fn server_delay(headers: &ureq::http::HeaderMap) -> ServerHints {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    hints(header, SystemTime::now())
}

impl From<Option<Duration>> for ServerHints {
    fn from(delay: Option<Duration>) -> Self {
        Self {
            delay,
            reset_at: None,
        }
    }
}

/// Google RPC `RetryInfo.retryDelay` ("37s", "1.5s") carried in an error body, used when no
/// retry header was sent (Gemini 429/503).
pub(crate) fn body_delay(body: &[u8]) -> Option<Duration> {
    let text = std::str::from_utf8(body).ok()?;
    let at = text.find("\"retryDelay\"")?;
    let rest = text[at + "\"retryDelay\"".len()..]
        .trim_start()
        .strip_prefix(':')?
        .trim_start();
    let value = rest.strip_prefix('"')?.split('"').next()?;
    duration_text(value).filter(|delay| !delay.is_zero())
}

fn hints<'a>(header: impl Fn(&str) -> Option<&'a str>, now: SystemTime) -> ServerHints {
    let delay = [
        header("retry-after-ms")
            .and_then(number)
            .filter(|ms| *ms > 0.0)
            .map(|ms| Duration::from_secs_f64(ms / 1000.0)),
        header("retry-after").and_then(|value| retry_after(value, now)),
        header("x-ratelimit-reset-ms").and_then(|value| reset_value(value, true, now)),
        header("x-ratelimit-reset").and_then(|value| reset_value(value, false, now)),
    ]
    .into_iter()
    .flatten()
    .max();
    let reset_at = [
        "anthropic-ratelimit-unified-reset",
        "anthropic-ratelimit-requests-reset",
        "anthropic-ratelimit-tokens-reset",
        "anthropic-ratelimit-input-tokens-reset",
        "anthropic-ratelimit-output-tokens-reset",
        "x-ratelimit-reset-requests",
        "x-ratelimit-reset-tokens",
        "x-codex-primary-reset-after-seconds",
        "x-codex-secondary-reset-after-seconds",
    ]
    .into_iter()
    .filter_map(|name| {
        let value = header(name)?;
        if name.ends_with("-after-seconds") {
            return number(value).map(|s| now + Duration::from_secs_f64(s));
        }
        duration_text(value)
            .map(|d| now + d)
            .or_else(|| reset_date(value))
    })
    .filter(|at| *at > now)
    .max();
    ServerHints { delay, reset_at }
}

fn retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    if let Some(seconds) = number(value) {
        return (seconds > 0.0).then(|| Duration::from_secs_f64(seconds));
    }
    httpdate::parse_http_date(value.trim())
        .ok()
        .and_then(|date| date.duration_since(now).ok())
        .filter(|delay| !delay.is_zero())
}

/// `x-ratelimit-reset[-ms]`: a delta, or an epoch timestamp in seconds/milliseconds.
fn reset_value(value: &str, millis: bool, now: SystemTime) -> Option<Duration> {
    let n = number(value).filter(|n| *n > 0.0)?;
    let target = if n > 1e12 {
        UNIX_EPOCH.checked_add(Duration::from_secs_f64(n / 1000.0))?
    } else if n > 1e9 {
        UNIX_EPOCH.checked_add(Duration::from_secs_f64(n))?
    } else {
        return Some(Duration::from_secs_f64(if millis { n / 1000.0 } else { n }));
    };
    target
        .duration_since(now)
        .ok()
        .filter(|delay| !delay.is_zero())
}

fn number(value: &str) -> Option<f64> {
    value
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && *n >= 0.0 && *n <= 1e15)
}

fn duration_text(mut value: &str) -> Option<Duration> {
    value = value.trim();
    if value.is_empty() || value.starts_with(|c: char| !c.is_ascii_digit()) {
        return None;
    }
    let mut seconds = 0.0;
    while !value.is_empty() {
        let at = value.find(|c: char| !c.is_ascii_digit() && c != '.')?;
        let n = number(&value[..at])?;
        value = &value[at..];
        let (unit, factor) = [("ms", 0.001), ("s", 1.0), ("m", 60.0), ("h", 3600.0)]
            .into_iter()
            .find(|(unit, _)| value.starts_with(unit))?;
        seconds += n * factor;
        value = &value[unit.len()..];
    }
    (seconds.is_finite() && seconds < 1e15).then(|| Duration::from_secs_f64(seconds))
}

fn reset_date(value: &str) -> Option<SystemTime> {
    if let Some(epoch) = number(value) {
        return UNIX_EPOCH.checked_add(Duration::from_secs_f64(epoch));
    }
    // Anthropic emits UTC RFC3339 reset timestamps, optionally with fractional seconds.
    let value = value.trim().strip_suffix('Z')?;
    let whole = value.split('.').next()?;
    if whole.len() != 19 {
        return None;
    }
    let seconds = crate::platform::time::parse_utc(whole)?;
    (seconds >= 0).then(|| UNIX_EPOCH + Duration::from_secs(seconds as u64))
}

/// Retry limits for one model request (`[retry]`).
#[derive(Debug, Clone)]
pub(crate) struct Policy {
    pub(crate) max_attempts: usize,
    pub(crate) max_delay: Duration,
    pub(crate) wait_for_quota_reset: bool,
    pub(crate) rotate_accounts: bool,
    /// Automatic retry interval, bounded by `max_delay`.
    pub(crate) backoff: Duration,
    /// A fallback model is ready for failures of this class (`retry.fallback_on`): such failures
    /// are retried briefly here, then handed to the caller to fail over.
    pub(crate) failover: Option<crate::config::types::FallbackOn>,
}

impl Policy {
    pub(crate) fn new(config: &crate::config::types::RetryConfig, backoff: Duration) -> Self {
        Self {
            max_attempts: config.max_attempts.clamp(1, 50) as usize,
            max_delay: Duration::from_secs(config.max_delay_secs.clamp(1, 3600)),
            wait_for_quota_reset: config.wait_for_quota_reset,
            rotate_accounts: config.rotate_accounts,
            backoff,
            failover: None,
        }
    }

    pub(crate) fn with_failover(
        mut self,
        failover: Option<crate::config::types::FallbackOn>,
    ) -> Self {
        self.failover = failover;
        self
    }

    /// Whether a ready fallback takes over failures of `class` after a brief retry.
    fn fails_over(&self, class: &ErrorClass) -> bool {
        self.failover
            .is_some_and(|on| crate::llm::fallback::fallback_applies(class, on))
    }

    /// Attempts allowed for a request failing with `class`, including the first.
    pub(crate) fn attempts(&self, class: &ErrorClass) -> usize {
        if self.fails_over(class) {
            self.max_attempts.min(FAILOVER_ATTEMPTS)
        } else {
            self.max_attempts
        }
    }
}

/// What the retry loop should do after a failed attempt.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Decision {
    Retry {
        delay: Duration,
        reason: &'static str,
    },
    /// Give up; the text explains why for the error context.
    Stop(Option<String>),
}

/// Decide whether attempt `attempt` (1-based) that failed with `class` is retried, and after how long.
/// `hinted` is the server's explicit minimum delay; it is never shortened, and a hint beyond
/// `max_delay` ends the request rather than waiting longer than the operator allowed.
pub(crate) fn decide(
    class: &ErrorClass,
    hinted: Option<Duration>,
    attempt: usize,
    policy: &Policy,
) -> Decision {
    let (reason, minimum) = match class {
        ErrorClass::Transient => ("transient provider failure", hinted),
        ErrorClass::RateLimited { retry_after } => ("rate limited", hinted.max(*retry_after)),
        ErrorClass::QuotaExhausted { reset_at } if policy.wait_for_quota_reset => {
            let until_reset = reset_at.and_then(|at| at.duration_since(SystemTime::now()).ok());
            match until_reset.or(hinted) {
                Some(wait) if wait <= policy.max_delay => {
                    ("usage quota exhausted; waiting for its reset", Some(wait))
                }
                Some(wait) => {
                    return Decision::Stop(Some(format!(
                        "usage quota resets in {}s, beyond retry.max_delay_secs ({}s)",
                        wait.as_secs(),
                        policy.max_delay.as_secs()
                    )));
                }
                None => {
                    return Decision::Stop(Some(
                        "usage quota exhausted and the provider gave no reset time".into(),
                    ));
                }
            }
        }
        _ => return Decision::Stop(None),
    };
    let failing_over = policy.fails_over(class);
    if attempt >= policy.attempts(class) {
        return Decision::Stop(Some(if failing_over {
            format!(
                "provider request failed after {attempt} attempts; moving to the configured fallback model"
            )
        } else {
            format!(
                "provider request failed after {attempt} attempts; the turn is paused. Say \"continue\" to retry"
            )
        }));
    }
    let interval = if failing_over {
        policy.backoff.min(FAILOVER_INTERVAL)
    } else {
        policy.backoff
    }
    .min(policy.max_delay);
    let delay = minimum.map_or(interval, |minimum| minimum.max(interval));
    if failing_over && delay > FAILOVER_MAX_WAIT {
        return Decision::Stop(Some(format!(
            "provider asked to wait {:.0}s; moving to the configured fallback model instead",
            delay.as_secs_f64()
        )));
    }
    if delay > policy.max_delay {
        return Decision::Stop(Some(format!(
            "provider asked to wait {:.0}s, beyond retry.max_delay_secs ({}s)",
            delay.as_secs_f64(),
            policy.max_delay.as_secs()
        )));
    }
    Decision::Retry { delay, reason }
}

#[cfg(test)]
mod tests;

//! Bounded provider retries for transient requests, throttles and explicitly incomplete streams.
//!
//! Only explicit "wait this long" headers are honoured as a minimum delay (`retry-after`,
//! `retry-after-ms`, `x-ratelimit-reset`, `x-ratelimit-reset-ms`).
//! Rate-limit *window* headers (`x-ratelimit-reset-requests/-tokens`,
//! `anthropic-ratelimit-*-reset`) say when a bucket refills, not how long a throttled request
//! must wait, so they are recorded only as a quota reset time and never force a wait.
use super::errors::ErrorClass;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Longest exponential backoff step when the server gives no explicit delay.
const BACKOFF_CAP: Duration = Duration::from_secs(60);

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
    /// First exponential backoff step.
    pub(crate) backoff: Duration,
}

impl Policy {
    pub fn new(config: &crate::config::types::RetryConfig, backoff: Duration) -> Self {
        Self {
            max_attempts: config.max_attempts.clamp(1, 50) as usize,
            max_delay: Duration::from_secs(config.max_delay_secs.clamp(1, 3600)),
            wait_for_quota_reset: config.wait_for_quota_reset,
            rotate_accounts: config.rotate_accounts,
            backoff,
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
    if attempt >= policy.max_attempts {
        return Decision::Stop(Some(format!(
            "provider request failed after {attempt} attempts; the turn is paused. Say \"continue\" to retry"
        )));
    }
    let exponential = policy
        .backoff
        .saturating_mul(1u32 << (attempt - 1).min(16))
        .min(BACKOFF_CAP)
        .min(policy.max_delay);
    let jitter = exponential
        .mul_f64(0.75 + rand::random::<f64>() * 0.5)
        .min(policy.max_delay);
    let delay = minimum.map_or(jitter, |minimum| minimum.max(jitter));
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

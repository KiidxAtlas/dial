//! Bounded provider retries; response-body failures never enter this policy.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(super) const MAX_ATTEMPTS: usize = 5;
pub(super) const MAX_WAIT: Duration = Duration::from_secs(60);

pub(super) fn server_delay(headers: &ureq::http::HeaderMap) -> Option<Duration> {
    let now = SystemTime::now();
    let mut delays = Vec::new();
    for name in [
        "retry-after",
        "retry-after-ms",
        "x-ratelimit-reset-requests",
        "x-ratelimit-reset-tokens",
        "anthropic-ratelimit-requests-reset",
        "anthropic-ratelimit-tokens-reset",
    ] {
        let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) else {
            continue;
        };
        let delay = if name == "retry-after-ms" {
            number(value).map(|n| Duration::from_secs_f64(n / 1000.0))
        } else if name == "retry-after" {
            number(value).map(Duration::from_secs_f64).or_else(|| {
                httpdate::parse_http_date(value)
                    .ok()
                    .map(|date| date.duration_since(now).unwrap_or_default())
            })
        } else {
            duration_text(value).or_else(|| {
                reset_date(value).map(|date| date.duration_since(now).unwrap_or_default())
            })
        };
        if let Some(delay) = delay {
            delays.push(delay);
        }
    }
    delays.into_iter().max()
}

fn number(value: &str) -> Option<f64> {
    value
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && *n >= 0.0 && *n <= 1e12)
}

fn duration_text(mut value: &str) -> Option<Duration> {
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
    let value = value.strip_suffix('Z')?;
    let whole = value.split('.').next()?;
    if whole.len() != 19 {
        return None;
    }
    let time = chrono::NaiveDateTime::parse_from_str(whole, "%Y-%m-%dT%H:%M:%S").ok()?;
    let seconds = time.and_utc().timestamp();
    (seconds >= 0).then(|| UNIX_EPOCH + Duration::from_secs(seconds as u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_provider_delays() {
        for (header, value, expected) in [
            ("retry-after", "4", 4.0),
            ("retry-after-ms", "250", 0.25),
            ("x-ratelimit-reset-tokens", "1m2s", 62.0),
        ] {
            let mut headers = ureq::http::HeaderMap::new();
            headers.insert(header, value.parse().unwrap());
            assert_eq!(server_delay(&headers).unwrap().as_secs_f64(), expected);
        }
        assert_eq!(
            reset_date("1970-01-01T00:00:01Z"),
            Some(UNIX_EPOCH + Duration::from_secs(1))
        );
        let mut headers = ureq::http::HeaderMap::new();
        headers.insert(
            "retry-after",
            "Thu, 01 Jan 1970 00:00:01 GMT".parse().unwrap(),
        );
        assert_eq!(server_delay(&headers), Some(Duration::ZERO));
    }
}

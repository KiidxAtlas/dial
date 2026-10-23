//! Helpers shared by the provider usage modules: status thresholds, tolerant JSON numbers and
//! ISO-8601 timestamp parsing.
//!
//! There is exactly one timestamp grammar ([`parse_iso`]); the two public entry points differ only
//! in what they do with a missing UTC offset:
//! - [`parse_iso_ms`]: an explicit zone (`Z` or `±HH[:MM]`) is required and the result must be a
//!   non-negative Unix-ms `u64`. This is the contract for APIs that always send zoned timestamps
//!   (Claude, Factory, xAI, Kimi, Copilot, reset credits).
//! - [`parse_iso_ms_lenient`]: a date-only or offset-less value is read as UTC and the result is a
//!   signed `i64` (JavaScript `Date.parse` behavior for the shapes Google sends: Gemini,
//!   Antigravity, Cursor).

use serde_json::Value;

use super::UsageStatus;

/// Status band for a used fraction: ok below 90%, warning below 100%, exhausted at 100%+.
pub(super) fn usage_status(used_fraction: f64) -> UsageStatus {
    if used_fraction >= 1.0 {
        UsageStatus::Exhausted
    } else if used_fraction >= 0.9 {
        UsageStatus::Warning
    } else {
        UsageStatus::Ok
    }
}

/// [`usage_status`] for a possibly unknown fraction (`None` is [`UsageStatus::Unknown`]).
pub(super) fn usage_status_opt(used_fraction: Option<f64>) -> UsageStatus {
    used_fraction.map_or(UsageStatus::Unknown, usage_status)
}

/// Finite number from a JSON number or numeric string.
pub(super) fn to_number(value: &Value) -> Option<f64> {
    let number = match value {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    number.is_finite().then_some(number)
}

/// Unix milliseconds of a timestamp with an explicit zone (`Z` or `±HH[:MM]`); `None` when the zone
/// is missing, the text is malformed or the instant precedes the epoch.
pub(super) fn parse_iso_ms(text: &str) -> Option<u64> {
    match parse_iso(text)? {
        (ms, true) => u64::try_from(ms).ok(),
        (_, false) => None,
    }
}

/// Unix milliseconds of a timestamp whose missing offset (or missing time of day) means UTC.
pub(super) fn parse_iso_ms_lenient(text: &str) -> Option<i64> {
    parse_iso(text).map(|(ms, _)| ms)
}

/// `YYYY-MM-DD` optionally followed by `T`/`t`/space, `HH:MM[:SS[.fff]]` (a `,` also separates the
/// fraction) and `Z` or `±HH[:MM]`/`±HHMM`. Fractions beyond milliseconds are truncated and
/// out-of-range fields (including day-of-month and leap seconds) are rejected. Returns Unix ms and
/// whether an explicit zone was present.
fn parse_iso(text: &str) -> Option<(i64, bool)> {
    let bytes = text.trim().as_bytes();
    let mut at = 0;
    let year = digits(bytes, &mut at, 4)?;
    eat(bytes, &mut at, b'-')?;
    let month = digits(bytes, &mut at, 2)?;
    eat(bytes, &mut at, b'-')?;
    let day = digits(bytes, &mut at, 2)?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }

    let (mut hour, mut minute, mut second, mut millis, mut offset_min) = (0, 0, 0, 0, 0);
    let mut zoned = false;
    if at < bytes.len() {
        if !matches!(bytes[at], b'T' | b't' | b' ') {
            return None;
        }
        at += 1;
        hour = digits(bytes, &mut at, 2)?;
        eat(bytes, &mut at, b':')?;
        minute = digits(bytes, &mut at, 2)?;
        if bytes.get(at).copied() == Some(b':') {
            at += 1;
            second = digits(bytes, &mut at, 2)?;
            if matches!(bytes.get(at).copied(), Some(b'.' | b',')) {
                at += 1;
                let start = at;
                while bytes.get(at).is_some_and(u8::is_ascii_digit) {
                    at += 1;
                }
                if at == start {
                    return None;
                }
                for k in 0..3 {
                    let digit = if start + k < at {
                        i64::from(bytes[start + k] - b'0')
                    } else {
                        0
                    };
                    millis = millis * 10 + digit;
                }
            }
        }
        match bytes.get(at).copied() {
            None => {}
            Some(b'Z' | b'z') => {
                at += 1;
                zoned = true;
            }
            Some(sign @ (b'+' | b'-')) => {
                at += 1;
                let offset_hours = digits(bytes, &mut at, 2)?;
                if bytes.get(at).copied() == Some(b':') {
                    at += 1;
                }
                let offset_minutes = if at == bytes.len() {
                    0
                } else {
                    digits(bytes, &mut at, 2)?
                };
                let total = offset_hours * 60 + offset_minutes;
                offset_min = if sign == b'-' { -total } else { total };
                zoned = true;
            }
            Some(_) => return None,
        }
    }
    if at != bytes.len() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days = i64::try_from(days_from_civil(year, month, day)).ok()?;
    Some((
        (days * 86_400 + hour * 3_600 + minute * 60 + second) * 1_000 + millis
            - offset_min * 60_000,
        zoned,
    ))
}

fn digits(bytes: &[u8], at: &mut usize, count: usize) -> Option<i64> {
    let slice = bytes.get(*at..*at + count)?;
    if !slice.iter().all(u8::is_ascii_digit) {
        return None;
    }
    *at += count;
    Some(
        slice
            .iter()
            .fold(0, |acc, b| acc * 10 + i64::from(b - b'0')),
    )
}

fn eat(bytes: &[u8], at: &mut usize, want: u8) -> Option<()> {
    if bytes.get(*at).copied() == Some(want) {
        *at += 1;
        Some(())
    } else {
        None
    }
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(year) => 29,
        _ => 28,
    }
}

/// Days since 1970-01-01 (proleptic Gregorian, Howard Hinnant's algorithm). `day` may overflow the
/// month linearly. Wide arithmetic: callers pass arbitrary parsed fields, so intermediates must not
/// overflow.
pub(super) fn days_from_civil(year: i64, month: i64, day: i64) -> i128 {
    let (year, month, day) = (i128::from(year), i128::from(month), i128::from(day));
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests;

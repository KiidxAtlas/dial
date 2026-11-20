pub mod calendar;

use calendar::{days_from_civil, is_valid_date};

/// Seconds since the Unix epoch for an exact `YYYY-MM-DDTHH:MM:SS` UTC timestamp.
pub fn parse_utc(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() != 19
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let field = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = text.get(range)?;
        part.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    let (year, month, day) = (field(0..4)?, field(5..7)?, field(8..10)?);
    let (hour, minute, second) = (field(11..13)?, field(14..16)?, field(17..19)?);
    if !is_valid_date(year, month, day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some(days_from_civil(year, month, day)? * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Format the current local time. Supports `%Y %m %d %H %M %S %%`, the subset Dial uses; any
/// other conversion makes the result empty rather than silently wrong.
#[cfg(windows)]
pub fn local_strftime(format: &str) -> String {
    super::imp::time::local_strftime(format)
}

/// `YYYY-MM-DD HH:MM:SS` in the local time zone for a Unix-ms instant; `None` when the platform
/// cannot convert it.
pub fn local_stamp(unix_ms: u64) -> Option<String> {
    let (year, month, day, hour, minute, second) = super::imp::time::local_fields(unix_ms)?;
    Some(format!(
        "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}"
    ))
}

#[cfg(test)]
mod tests;

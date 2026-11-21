//! Wall-clock helpers shared by every layer: Unix seconds now, and RFC 3339 UTC rendering.

/// Whole seconds since the Unix epoch; a clock set before 1970 reads as zero.
pub fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// RFC 3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`) for a Unix time in seconds.
pub fn format_utc(secs: u64) -> String {
    format_utc_parts((secs / 86_400) as i64, (secs % 86_400) as i64)
}

/// RFC 3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`) for Unix seconds before or after 1970.
pub fn format_utc_seconds(secs: i64) -> String {
    format_utc_parts(secs.div_euclid(86_400), secs.rem_euclid(86_400))
}

fn format_utc_parts(days: i64, rem: i64) -> String {
    let date = crate::platform::time::calendar::civil_from_days(days);
    format!(
        "{date}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// The instant `ago` before now, used to make a throttle or cache due at once. A monotonic clock
/// can start near its origin (shortly after boot), where `Instant::now() - ago` would panic; then
/// the earliest representable instant, `now`, is returned and the first refresh waits `ago`.
pub fn instant_ago(ago: std::time::Duration) -> std::time::Instant {
    let now = std::time::Instant::now();
    now.checked_sub(ago).unwrap_or(now)
}

#[cfg(test)]
mod tests;

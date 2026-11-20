use super::*;
use std::time::{Duration, Instant};

#[test]
fn instant_ago_is_in_the_past_and_never_panics() {
    let before = Instant::now();
    let ago = instant_ago(Duration::from_millis(5));
    assert!(ago <= before);
    assert!(ago.elapsed() >= Duration::from_millis(5));
    // Larger than any clock's distance from its origin: clamps instead of panicking.
    assert!(instant_ago(Duration::MAX) >= before);
}

#[test]
fn utc_clock_renders_leap_day_without_losing_time_of_day() {
    assert_eq!(format_utc_seconds(0), "1970-01-01T00:00:00Z");
    assert_eq!(format_utc_seconds(951_782_401), "2000-02-29T00:00:01Z");
    assert_eq!(format_utc(951_868_799), "2000-02-29T23:59:59Z");
    assert_eq!(format_utc(951_868_800), "2000-03-01T00:00:00Z");
    assert_eq!(format_utc_seconds(-1), "1969-12-31T23:59:59Z");
}

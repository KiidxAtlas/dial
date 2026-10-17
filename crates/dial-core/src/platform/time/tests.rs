use super::*;

#[test]
fn parses_exact_utc_timestamps_and_rejects_malformed_ones() {
    assert_eq!(parse_utc("1970-01-01T00:00:00"), Some(0));
    assert_eq!(parse_utc("2000-03-01T00:00:00"), Some(951_868_800));
    assert_eq!(parse_utc("2024-02-29T23:59:59"), Some(1_709_251_199));
    assert_eq!(parse_utc("2023-02-29T00:00:00"), None);
    assert_eq!(parse_utc("2024-13-01T00:00:00"), None);
    assert_eq!(parse_utc("2024-01-01 00:00:00"), None);
    assert_eq!(parse_utc("2024-01-01T00:00:0"), None);
    assert_eq!(parse_utc("2024-01-01T0+:00:00"), None);
}

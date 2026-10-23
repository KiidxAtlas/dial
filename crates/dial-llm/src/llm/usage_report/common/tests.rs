use super::*;
use serde_json::json;

#[test]
fn status_thresholds_are_shared() {
    assert_eq!(usage_status(0.89), UsageStatus::Ok);
    assert_eq!(usage_status(0.9), UsageStatus::Warning);
    assert_eq!(usage_status(1.0), UsageStatus::Exhausted);
    assert_eq!(usage_status_opt(None), UsageStatus::Unknown);
    assert_eq!(usage_status_opt(Some(1.5)), UsageStatus::Exhausted);
}

#[test]
fn to_number_accepts_finite_numbers_and_strings() {
    assert_eq!(to_number(&json!(2.5)), Some(2.5));
    assert_eq!(to_number(&json!(" 7 ")), Some(7.0));
    assert_eq!(to_number(&json!("NaN")), None);
    assert_eq!(to_number(&json!(null)), None);
}

#[test]
fn zoned_contract_requires_an_offset_and_non_negative_instant() {
    assert_eq!(parse_iso_ms("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(parse_iso_ms("1970-01-01 00:00:01z"), Some(1_000));
    assert_eq!(
        parse_iso_ms("2026-05-01T02:00:00+02"),
        Some(1_777_593_600_000)
    );
    assert_eq!(
        parse_iso_ms("2026-05-01T02:00:00+02:00"),
        Some(1_777_593_600_000)
    );
    assert_eq!(parse_iso_ms("2026-01-01T00:00:00"), None);
    assert_eq!(parse_iso_ms("1970-01-02"), None);
    assert_eq!(parse_iso_ms("1969-12-31T23:59:59Z"), None);
    assert_eq!(parse_iso_ms("2026-02-30T00:00:00Z"), None);
}

#[test]
fn lenient_contract_defaults_to_utc_and_allows_negative() {
    assert_eq!(parse_iso_ms_lenient("1970-01-02"), Some(86_400_000));
    assert_eq!(parse_iso_ms_lenient("1970-01-01T00:00:01"), Some(1_000));
    assert_eq!(parse_iso_ms_lenient("1969-12-31T23:59:59Z"), Some(-1_000));
    assert_eq!(
        parse_iso_ms_lenient("1970-01-01T00:00:00+0130"),
        Some(-5_400_000)
    );
    assert_eq!(parse_iso_ms_lenient("1970-01-01T00:00:00Zjunk"), None);
}

#[test]
fn days_from_civil_matches_known_dates() {
    assert_eq!(days_from_civil(1970, 1, 1), 0);
    assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    assert_eq!(days_from_civil(1969, 12, 31), -1);
}

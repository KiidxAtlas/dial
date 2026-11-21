use super::*;

#[test]
fn known_dates_have_known_day_counts() {
    let cases = [
        ((1970, 1, 1), 0),
        ((1969, 12, 31), -1),
        ((1600, 3, 1), -135_080),
        ((2000, 3, 1), 11_017),
        ((2024, 1, 1), 19_723),
        ((2024, 2, 29), 19_782),
    ];
    for ((year, month, day), days) in cases {
        assert_eq!(
            days_from_civil(year, month, day),
            Some(days),
            "{year}-{month}-{day}"
        );
        assert_eq!(
            civil_from_days(days),
            CivilDate { year, month, day },
            "day {days}"
        );
    }
}

#[test]
fn the_400_year_cycle_is_exactly_146097_days() {
    let cycle = days_from_civil(2000, 3, 1).unwrap() - days_from_civil(1600, 3, 1).unwrap();
    assert_eq!(cycle, 146_097);
}

#[test]
fn leap_years_follow_the_gregorian_rules() {
    let cases = [
        (1900, false),
        (2000, true),
        (2100, false),
        (2024, true),
        (2023, false),
    ];
    for (year, leap) in cases {
        assert_eq!(is_leap_year(year), leap, "{year}");
        assert_eq!(
            days_in_month(year, 2),
            Some(if leap { 29 } else { 28 }),
            "{year}"
        );
    }
}

#[test]
fn month_lengths_and_date_validity() {
    assert_eq!(days_in_month(2026, 0), None);
    assert_eq!(days_in_month(2026, 13), None);
    let lengths = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for (index, length) in lengths.into_iter().enumerate() {
        assert_eq!(days_in_month(2026, index as i64 + 1), Some(length));
    }
    let cases = [
        ((2024, 2, 29), true),
        ((2023, 2, 29), false),
        ((2026, 4, 31), false),
        ((2026, 4, 30), true),
        ((2026, 1, 0), false),
        ((2026, 12, 31), true),
        ((2026, 0, 1), false),
        ((2026, 13, 1), false),
    ];
    for ((year, month, day), valid) in cases {
        assert_eq!(
            is_valid_date(year, month, day),
            valid,
            "{year}-{month}-{day}"
        );
    }
}

#[test]
fn day_past_the_month_end_counts_linearly() {
    assert_eq!(days_from_civil(2024, 1, 32), days_from_civil(2024, 2, 1));
    assert_eq!(days_from_civil(2023, 2, 29), days_from_civil(2023, 3, 1));
    assert_eq!(days_from_civil(2026, 1, 0), days_from_civil(2025, 12, 31));
    assert_eq!(days_from_civil(2026, i64::MIN, 1), None);
    assert_eq!(days_from_civil(2026, 13, 1), None);
}

#[test]
fn day_counts_stay_exact_at_the_edges_of_i64() {
    // Inputs far outside any calendar still produce exact results or a clean `None`.
    assert_eq!(days_from_civil(i64::MAX, 12, 31), None);
    assert_eq!(days_from_civil(i64::MIN, 1, 1), None);
    assert_eq!(days_from_civil(i64::MAX / 2, 1, i64::MAX / 2), None);
    assert_eq!(days_from_civil(1, 1, i64::MIN), None);
    assert_eq!(days_from_civil(1, 1, i64::MAX), Some(i64::MAX - 719_163));
    assert_eq!(days_from_civil(2, 1, i64::MAX), Some(i64::MAX - 718_798));
    assert_eq!(days_from_civil(2024, 1, i64::MAX), None);
    assert!(days_from_civil(1_000_000_000_000, 1, 1).is_some());
}

#[test]
fn civil_from_days_is_total_and_inverts_days_from_civil() {
    let mut samples = vec![i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX];
    samples.extend((-200_000..200_000).step_by(9_973));
    samples.extend([-1_000_000_000_000_000, 1_000_000_000_000_000]);
    for days in samples {
        let date = civil_from_days(days);
        assert!((1..=12).contains(&date.month), "{days} -> {date:?}");
        assert!((1..=days_in_month(date.year, date.month).unwrap()).contains(&date.day));
        assert_eq!(
            days_from_civil(date.year, date.month, date.day),
            Some(days),
            "{days}"
        );
    }
}

#[test]
fn display_is_the_iso_day() {
    assert_eq!(
        CivilDate {
            year: 2026,
            month: 5,
            day: 10
        }
        .to_string(),
        "2026-05-10"
    );
    assert_eq!(civil_from_days(0).to_string(), "1970-01-01");
}

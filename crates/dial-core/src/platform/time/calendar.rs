//! Proleptic Gregorian calendar arithmetic: the one owner of day counts and calendar fields.
//!
//! Every crate that reads or writes a date uses these functions. The grammars that
//! turn text into a date (how strictly a day is checked, which offsets are accepted, whether a
//! missing zone means UTC or local time) stay with their callers; this module only converts
//! between a [`CivilDate`] and days since 1970-01-01.
//!
//! Arithmetic is exact over the whole `i64` range: intermediates are computed in `i128`, and a
//! result that does not fit is reported as `None` rather than wrapping.

use std::fmt;

pub const SECONDS_PER_DAY: i64 = 86_400;
pub const MILLIS_PER_DAY: i64 = 86_400_000;

/// A calendar date in the proleptic Gregorian calendar. Fields are not range-checked; use
/// [`is_valid_date`] when the value came from outside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CivilDate {
    pub year: i64,
    pub month: i64,
    pub day: i64,
}

/// `YYYY-MM-DD`, the form every caller writes. Years outside 0..=9999 keep their sign and
/// full width, as `{:04}` renders them.
impl fmt::Display for CivilDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

pub const fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// Days in `month` (1..=12) of `year`; `None` for any other month.
pub const fn days_in_month(year: i64, month: i64) -> Option<i64> {
    Some(match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => return None,
    })
}

/// Whether `year-month-day` names a day that exists (no leap-second or overflowing day).
pub const fn is_valid_date(year: i64, month: i64, day: i64) -> bool {
    match days_in_month(year, month) {
        Some(last) => day >= 1 && day <= last,
        None => false,
    }
}

/// Days since 1970-01-01 for `year-month-day` (Howard Hinnant's algorithm). `month` must be
/// 1..=12. `day` is linear: a day past the end of the month counts into the following months, so
/// `(2024, 1, 32)` is 2024-02-01. Invalid months and unrepresentable day counts return `None`.
pub fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=12).contains(&month) {
        return None;
    }
    let year = i128::from(year) - i128::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = i128::from(if month > 2 { month - 3 } else { month + 9 });
    let day_of_year = (153 * month_index + 2) / 5 + i128::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    i64::try_from(era * 146_097 + day_of_era - 719_468).ok()
}

/// Inverse of [`days_from_civil`]. Total: every `i64` day count lies inside the `i64` year range,
/// so no input is refused.
pub fn civil_from_days(days: i64) -> CivilDate {
    let days = i128::from(days) + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i128::from(month <= 2);
    CivilDate {
        // |days| <= 2^63 gives |year| < 2^63 / 365 + 2, so the year always fits.
        year: year as i64,
        month: month as i64,
        day: day as i64,
    }
}

#[cfg(test)]
mod tests;

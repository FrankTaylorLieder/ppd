//! Converting between Unix time and ISO 8601 UTC date-times
//! (`2026-10-15T10:12:18Z`), for the readings API.
//!
//! The day-count conversions are Howard Hinnant's `days_from_civil` /
//! `civil_from_days` algorithms (proleptic Gregorian calendar), which work
//! in whole 400-year eras so need no tables or leap-year special cases.

use core::fmt::Write as _;
use heapless::String as HString;

/// Formats Unix time as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn format_utc(unix_time: i64) -> HString<20> {
    let days = unix_time.div_euclid(86_400);
    let secs = unix_time.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let mut text = HString::new();
    let _ = write!(
        text,
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    );
    text
}

/// Parses a UTC date-time to Unix time. Accepts `YYYY-MM-DD`, optionally
/// followed by `THH:MM` or `THH:MM:SS`, optionally followed by `Z`. Times
/// are always taken as UTC; no other offsets are accepted.
pub fn parse_utc(text: &str) -> Option<i64> {
    let text = text.strip_suffix('Z').unwrap_or(text);
    let (date, time) = match text.split_once('T') {
        Some((date, time)) => (date, Some(time)),
        None => (text, None),
    };

    let mut date_parts = date.split('-');
    let year: i64 = number(date_parts.next()?, 4)?;
    let month: u32 = number(date_parts.next()?, 2)?;
    let day: u32 = number(date_parts.next()?, 2)?;
    if date_parts.next().is_some()
        || !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
    {
        return None;
    }

    let (mut hour, mut minute, mut second) = (0, 0, 0);
    if let Some(time) = time {
        let mut time_parts = time.split(':');
        hour = number(time_parts.next()?, 2)?;
        minute = number(time_parts.next()?, 2)?;
        if let Some(part) = time_parts.next() {
            second = number(part, 2)?;
        }
        if time_parts.next().is_some() || hour > 23 || minute > 59 || second > 59 {
            return None;
        }
    }

    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// Parses exactly `digits` ASCII digits.
fn number<T: TryFrom<u32>>(text: &str, digits: usize) -> Option<T> {
    if text.len() != digits || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value = text
        .bytes()
        .fold(0u32, |acc, b| acc * 10 + (b - b'0') as u32);
    T::try_from(value).ok()
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 for a calendar date.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    // Count years from March, so the leap day is the last day of the year.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_from_march = (month + 9) % 12;
    let day_of_year = (153 * month_from_march as i64 + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Calendar date `(year, month, day)` for days since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_from_march + 2) / 5 + 1) as u32;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    } as u32;
    let year = year_of_era + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

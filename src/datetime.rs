// SPDX-License-Identifier: Apache-2.0

//! W3CDTF dates, read as the instants the Document schema asks for.
//!
//! # Why this exists
//!
//! `DocumentMeta.created` and `.modified` are `google.protobuf.Timestamp`s,
//! not strings, and the schema's contract for them is explicit: the typed
//! field carries the parsed instant, the `_raw` twin keeps the source's own
//! spelling, and the twin is the *only* field set when the value does not
//! parse. So this service has to actually read a date rather than pass one
//! through, and it has to be able to say "this is not a date" without
//! guessing.
//!
//! # Why it is hand-written
//!
//! The crate carries no date library on purpose. `zip` is built without its
//! `time` feature (see `Cargo.toml`) because entry timestamps are not on the
//! wire, and pulling `chrono` back in for one field would undo that. What is
//! needed here is a fixed grammar and an integer calendar conversion, both
//! small and both testable, in the same spirit as the hand-written FNV hash
//! in `document_fold` and the SMIL clock reader in `smil`.
//!
//! # The grammar
//!
//! W3CDTF, the profile the EPUB specification points at, in every truncation
//! it allows:
//!
//! ```text
//! YYYY
//! YYYY-MM
//! YYYY-MM-DD
//! YYYY-MM-DDThh:mmTZD
//! YYYY-MM-DDThh:mm:ssTZD
//! YYYY-MM-DDThh:mm:ss.sTZD
//! ```
//!
//! with `TZD` being `Z`, `+hh:mm` or `-hh:mm`. Real books are looser than the
//! profile in two harmless ways that are accepted here: a lower-case `t` or a
//! space where the separator belongs, and a `+hhmm` or `+hh` offset. A
//! truncated value is read as the first instant it can name, so `1843` is
//! `1843-01-01T00:00:00Z`.
//!
//! # Timezones are not invented, they are defaulted the way the format says
//!
//! A `Timestamp` is an instant and a date with no offset is not, so something
//! has to bridge the two. W3CDTF's own answer is UTC, which is also what the
//! EPUB specification requires of `dcterms:modified`, so that is the reading
//! taken here. It is a reading rather than a fact, which is exactly why the
//! `_raw` twin is written alongside every parsed value and not only alongside
//! the ones that fail: whatever this module decided, the source's own spelling
//! survives next to it.
//!
//! Anything that does not match the grammar yields [`None`], and the caller
//! writes the raw twin alone. Nothing here ever fails a call.

use prost_types::Timestamp;

/// Seconds in a day.
const SECONDS_PER_DAY: i64 = 86_400;

/// Read a W3CDTF date or datetime as an instant.
///
/// Returns [`None`] for anything outside the grammar in the module
/// documentation, including the empty string. A `None` is the signal to write
/// the source's spelling into the `_raw` twin and leave the typed field unset.
#[must_use]
pub fn parse(value: &str) -> Option<Timestamp> {
    let text = value.trim();
    if text.is_empty() {
        return None;
    }

    // The date and the time of day are separated by `T` in the profile; a
    // lower-case `t` and a bare space are the two spellings real producers
    // reach for instead, and both are unambiguous here because neither can
    // occur inside a date.
    let (date, clock) = match text.find(['T', 't', ' ']) {
        Some(index) => (&text[..index], &text[index + 1..]),
        None => (text, ""),
    };

    let (year, month, day) = date_parts(date)?;
    let (hour, minute, second, nanos, offset_minutes) = if clock.is_empty() {
        (0, 0, 0, 0, 0)
    } else {
        clock_parts(clock)?
    };

    let seconds = days_from_civil(year, month, day) * SECONDS_PER_DAY
        + i64::from(hour) * 3_600
        + i64::from(minute) * 60
        + i64::from(second)
        - i64::from(offset_minutes) * 60;
    Some(Timestamp { seconds, nanos })
}

/// Read `YYYY`, `YYYY-MM` or `YYYY-MM-DD`, defaulting what is truncated away.
///
/// The year is four digits exactly: that is what the profile says, and it is
/// also the whole range `Timestamp` can represent, so a five-digit year is
/// better refused than silently turned into something else.
fn date_parts(date: &str) -> Option<(i64, u32, u32)> {
    let mut parts = date.split('-');
    let year = parts.next()?;
    if year.len() != 4 || !year.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let year: i64 = year.parse().ok()?;
    if year == 0 {
        return None;
    }

    let month = two_digits(parts.next(), 1)?;
    let day = two_digits(parts.next(), 1)?;
    if parts.next().is_some() || !(1..=12).contains(&month) {
        return None;
    }
    if day < 1 || day > days_in_month(year, month) {
        return None;
    }
    Some((year, month, day))
}

/// Read a one or two digit field, or the default when the source truncated it.
///
/// The profile writes two digits; producers that write one are common enough
/// (`1843-1-1`) that refusing them would lose real dates for no gain.
fn two_digits(field: Option<&str>, default: u32) -> Option<u32> {
    let Some(field) = field else {
        return Some(default);
    };
    if field.is_empty() || field.len() > 2 || !field.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    field.parse().ok()
}

/// Read `hh:mm[:ss[.fraction]]` and its timezone designator.
///
/// Returns the wall-clock fields plus the offset in minutes east of UTC, which
/// the caller subtracts. An absent designator is UTC; see the module
/// documentation for why that is a defensible default rather than a guess.
fn clock_parts(clock: &str) -> Option<(u32, u32, u32, i32, i32)> {
    let (time, offset_minutes) = split_offset(clock)?;

    let mut parts = time.split(':');
    let hour = two_digits(parts.next(), 0)?;
    let minute = two_digits(parts.next(), 0)?;
    let (second, nanos) = match parts.next() {
        None => (0, 0),
        Some(field) => {
            let (whole, fraction) = match field.split_once('.') {
                Some((whole, fraction)) => (whole, fraction),
                None => (field, ""),
            };
            (two_digits(Some(whole), 0)?, nanoseconds(fraction)?)
        }
    };
    if parts.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some((hour, minute, second, nanos, offset_minutes))
}

/// Split a timezone designator off the end of a time of day.
fn split_offset(clock: &str) -> Option<(&str, i32)> {
    if let Some(time) = clock.strip_suffix(['Z', 'z']) {
        return Some((time, 0));
    }
    // The sign is looked for from the right because a time of day contains no
    // other `+` or `-`, and a designator is always last.
    let Some(index) = clock.rfind(['+', '-']) else {
        return Some((clock, 0));
    };
    let (time, designator) = clock.split_at(index);
    let sign = if designator.starts_with('-') { -1 } else { 1 };

    let digits = &designator[1..];
    let (hours, minutes) = match digits.split_once(':') {
        Some((hours, minutes)) => (hours, minutes),
        None if digits.len() == 4 => digits.split_at(2),
        None => (digits, "0"),
    };
    let hours = two_digits(Some(hours), 0)?;
    let minutes = two_digits(Some(minutes), 0)?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some((
        time,
        sign * (i32::try_from(hours).ok()? * 60 + i32::try_from(minutes).ok()?),
    ))
}

/// Read a decimal fraction of a second as nanoseconds.
///
/// Longer fractions are truncated rather than rounded: a producer that wrote
/// picoseconds into a book's modification date meant nothing by the last three
/// digits, and rounding could carry into a second that was never written.
fn nanoseconds(fraction: &str) -> Option<i32> {
    if fraction.is_empty() {
        return Some(0);
    }
    if !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let mut nanos = 0i32;
    for position in 0..9 {
        let digit = fraction
            .as_bytes()
            .get(position)
            .map_or(0, |byte| i32::from(byte - b'0'));
        nanos = nanos * 10 + digit;
    }
    Some(nanos)
}

/// Days in a month of a proleptic Gregorian year.
const fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days between the Unix epoch and a proleptic Gregorian date.
///
/// Hinnant's `days_from_civil`, which is integer-only and exact over the whole
/// range a four-digit year can express. Spelled out here for the same reason
/// the FNV hash is: a calendar conversion that silently changes behaviour with
/// a dependency bump would move every stored instant with it.
const fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let month = month as i64;
    let day = day as i64;
    // The year is shifted to start in March, which puts the leap day last and
    // makes the day-of-year formula below a straight line.
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seconds of a value that must parse.
    fn seconds(value: &str) -> i64 {
        parse(value)
            .unwrap_or_else(|| panic!("{value:?} should parse"))
            .seconds
    }

    #[test]
    fn the_epoch_is_where_the_calendar_says_it_is() {
        assert_eq!(seconds("1970-01-01T00:00:00Z"), 0);
        assert_eq!(seconds("1970-01-02"), SECONDS_PER_DAY);
        assert_eq!(seconds("1969-12-31"), -SECONDS_PER_DAY);
    }

    #[test]
    fn every_truncation_the_profile_allows_names_its_first_instant() {
        assert_eq!(seconds("2026"), seconds("2026-01-01T00:00:00Z"));
        assert_eq!(seconds("2026-08"), seconds("2026-08-01T00:00:00Z"));
        assert_eq!(seconds("2026-08-25"), seconds("2026-08-25T00:00:00Z"));
        assert_eq!(seconds("2026-08-25T12:30"), seconds("2026-08-25T12:30:00Z"));
    }

    #[test]
    fn a_timezone_designator_moves_the_instant() {
        let utc = seconds("2026-08-25T12:00:00Z");
        assert_eq!(seconds("2026-08-25T12:00:00+00:00"), utc);
        assert_eq!(seconds("2026-08-25T07:00:00-05:00"), utc);
        assert_eq!(seconds("2026-08-25T07:00:00-0500"), utc);
        assert_eq!(seconds("2026-08-25T07:00:00-05"), utc);
        assert_eq!(
            seconds("2026-08-25T12:00:00"),
            utc,
            "no designator reads as UTC, which is what the profile says"
        );
    }

    #[test]
    fn the_separator_may_be_any_of_the_three_spellings_books_use() {
        let instant = seconds("2026-08-25T12:00:00Z");
        assert_eq!(seconds("2026-08-25t12:00:00Z"), instant);
        assert_eq!(seconds("2026-08-25 12:00:00Z"), instant);
        assert_eq!(seconds("  2026-08-25T12:00:00Z  "), instant);
    }

    #[test]
    fn fractional_seconds_become_nanoseconds_and_never_round_up() {
        assert_eq!(parse("2026-08-25T00:00:00.5Z").unwrap().nanos, 500_000_000);
        assert_eq!(parse("2026-08-25T00:00:00.000001Z").unwrap().nanos, 1_000);
        assert_eq!(
            parse("2026-08-25T00:00:00.9999999999Z").unwrap().nanos,
            999_999_999,
            "a tenth digit is dropped, never carried into the second"
        );
    }

    #[test]
    fn leap_days_are_the_gregorian_ones() {
        assert!(parse("2024-02-29").is_some());
        assert!(parse("2000-02-29").is_some(), "a 400-year leap year");
        assert!(parse("1900-02-29").is_none(), "a 100-year exception");
        assert!(parse("2023-02-29").is_none());
        assert_eq!(
            seconds("2024-03-01") - seconds("2024-02-28"),
            2 * SECONDS_PER_DAY
        );
    }

    #[test]
    fn what_is_not_a_date_says_so_rather_than_guessing() {
        for value in [
            "",
            "   ",
            "not a date",
            "Q3 2011",
            "sometime in 1843",
            "1843-13-01",
            "1843-00-01",
            "1843-10-32",
            "1843-10-01T25:00:00Z",
            "1843-10-01T12:60:00Z",
            "1843-10-01T12:00:60Z",
            "1843-10-01T12:00:00:00",
            "843-10-01",
            "18430-10-01",
            "0000-01-01",
            "1843-10-01-05",
            "1843-1o-01",
            "1843-10-01T12:00:00+99:00",
        ] {
            assert!(parse(value).is_none(), "{value:?} is not a date");
        }
    }

    #[test]
    fn a_real_books_dcterms_modified_reads_as_the_instant_it_names() {
        // The mandatory EPUB 3 spelling, which is the one this service sees
        // most: UTC, whole seconds, `Z`.
        let stamp = parse("2026-08-25T00:00:00Z").expect("a datetime");
        assert_eq!(stamp.nanos, 0);
        assert_eq!(stamp.seconds % SECONDS_PER_DAY, 0, "midnight UTC");
    }
}

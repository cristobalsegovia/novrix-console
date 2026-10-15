//! Pure ISO-8601 timestamp formatting (Requirement 14.4, Property 44).
//!
//! This module turns a Unix timestamp (seconds since 1970-01-01T00:00:00Z) into
//! an ISO-8601 string that carries the date, the time to the second, and a
//! timezone offset. It is deliberately free of any `std::time` or `SystemTime`
//! calls so it can be exhaustively unit-/property-tested with fixed inputs:
//! `format_iso8601_utc(ts)` is a total function of its argument.
//!
//! The offset is emitted as `Z` (UTC, `+00:00`). See the module-level note in
//! `logging/mod.rs` for why UTC-with-`Z` is used rather than the host's local
//! offset: it keeps the ISO-8601 "timezone offset" requirement satisfied with no
//! new time-crate dependency.

/// Seconds since the Unix epoch (1970-01-01T00:00:00Z).
///
/// A small newtype so callers (and the [`Clock`](super::Clock) port) pass an
/// unambiguous value rather than a bare `i64`. Negative values (pre-epoch) are
/// supported for completeness and determinism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnixTimestamp(pub i64);

impl UnixTimestamp {
    /// Seconds since the Unix epoch.
    pub fn as_secs(self) -> i64 {
        self.0
    }
}

/// Days in each month of a non-leap year, January first.
const DAYS_IN_MONTH: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

/// Proleptic-Gregorian leap-year test.
fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Format a Unix timestamp as an ISO-8601 UTC instant,
/// `YYYY-MM-DDThh:mm:ssZ` (date, time to the second, and the `Z` offset).
///
/// This is a self-contained civil-calendar conversion (no dependency on
/// `std::time`): it splits the timestamp into whole days and the second-of-day,
/// walks the day count out from 1970 to find the year/month/day, and formats the
/// pieces with zero padding. Pre-epoch (negative) timestamps are handled by
/// flooring the division so the time-of-day is always in `0..86_400`.
pub fn format_iso8601_utc(ts: UnixTimestamp) -> String {
    const SECS_PER_DAY: i64 = 86_400;
    let total = ts.as_secs();

    // Floor-divide so negative timestamps keep a positive seconds-of-day.
    let days = total.div_euclid(SECS_PER_DAY);
    let secs_of_day = total.rem_euclid(SECS_PER_DAY);

    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;

    // Walk from the epoch (1970-01-01) to the target date, one year at a time,
    // then one month at a time. `remaining` counts days left after the epoch.
    let mut year: i64 = 1970;
    let mut remaining = days;

    if remaining >= 0 {
        loop {
            let year_len = if is_leap_year(year) { 366 } else { 365 };
            if remaining < year_len {
                break;
            }
            remaining -= year_len;
            year += 1;
        }
    } else {
        // Pre-epoch: step backwards, charging each earlier year's length.
        while remaining < 0 {
            year -= 1;
            let year_len = if is_leap_year(year) { 366 } else { 365 };
            remaining += year_len;
        }
    }

    let mut month = 0usize; // 0-based index into DAYS_IN_MONTH
    loop {
        let mut month_len = DAYS_IN_MONTH[month];
        if month == 1 && is_leap_year(year) {
            month_len = 29;
        }
        if remaining < month_len {
            break;
        }
        remaining -= month_len;
        month += 1;
    }

    let day = remaining + 1; // 1-based day of month

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year,
        month + 1,
        day,
        hour,
        minute,
        second
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_the_unix_epoch() {
        assert_eq!(
            format_iso8601_utc(UnixTimestamp(0)),
            "1970-01-01T00:00:00Z"
        );
    }

    #[test]
    fn formats_a_known_instant() {
        // 2024-05-02T13:45:09Z == 1_714_657_509 seconds since the epoch.
        assert_eq!(
            format_iso8601_utc(UnixTimestamp(1_714_657_509)),
            "2024-05-02T13:45:09Z"
        );
    }

    #[test]
    fn handles_a_leap_day() {
        // 2020-02-29T00:00:00Z == 1_582_934_400.
        assert_eq!(
            format_iso8601_utc(UnixTimestamp(1_582_934_400)),
            "2020-02-29T00:00:00Z"
        );
    }

    #[test]
    fn handles_end_of_year_rollover() {
        // 1999-12-31T23:59:59Z == 946_684_799.
        assert_eq!(
            format_iso8601_utc(UnixTimestamp(946_684_799)),
            "1999-12-31T23:59:59Z"
        );
    }

    #[test]
    fn handles_pre_epoch_timestamp() {
        // 1969-12-31T23:59:59Z == -1.
        assert_eq!(
            format_iso8601_utc(UnixTimestamp(-1)),
            "1969-12-31T23:59:59Z"
        );
    }

    #[test]
    fn prefix_shape_is_iso8601_with_offset() {
        // Smoke check of Property 44's shape: date + time-to-second + offset.
        let s = format_iso8601_utc(UnixTimestamp(1_714_657_509));
        assert_eq!(s.len(), 20); // YYYY-MM-DDThh:mm:ssZ
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
        assert_eq!(&s[10..11], "T");
        assert_eq!(&s[13..14], ":");
        assert_eq!(&s[16..17], ":");
        assert!(s.ends_with('Z'));
    }

    mod properties {
        use super::*;
        use proptest::prelude::*;

        /// Parse a two-field, N-wide zero-padded unsigned integer out of `s` at
        /// `range`, asserting the slice is all ASCII digits. Returns the value
        /// so a property can range-check the civil-calendar fields.
        fn parse_field(s: &str, start: usize, end: usize) -> Option<i64> {
            let slice = &s[start..end];
            if slice.bytes().all(|b| b.is_ascii_digit()) {
                slice.parse::<i64>().ok()
            } else {
                None
            }
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(256))]

            // Feature: remote-connection-manager, Property 44: For any log entry, its prefix is a valid ISO-8601 timestamp that includes the date, the time to the second, and a timezone offset.
            #[test]
            fn iso8601_prefix_is_well_formed(
                // Bound to a sane civil-calendar window (~1901..=2242) so the
                // four-digit year field never overflows; the formatter is total
                // for all i64, but the ISO-8601 `YYYY` field assumes four digits.
                secs in -2_177_452_800i64..=8_589_934_591i64
            ) {
                let s = format_iso8601_utc(UnixTimestamp(secs));

                // Shape: exactly `YYYY-MM-DDThh:mm:ssZ` (date, time-to-second,
                // and the `Z` UTC offset designator).
                prop_assert_eq!(s.len(), 20, "unexpected length for {}", s);
                prop_assert_eq!(&s[4..5], "-");
                prop_assert_eq!(&s[7..8], "-");
                prop_assert_eq!(&s[10..11], "T");
                prop_assert_eq!(&s[13..14], ":");
                prop_assert_eq!(&s[16..17], ":");
                prop_assert!(s.ends_with('Z'), "missing UTC offset: {}", s);

                // Each field is all digits and within its civil-calendar range.
                let year = parse_field(&s, 0, 4);
                let month = parse_field(&s, 5, 7);
                let day = parse_field(&s, 8, 10);
                let hour = parse_field(&s, 11, 13);
                let minute = parse_field(&s, 14, 16);
                let second = parse_field(&s, 17, 19);

                prop_assert!(year.is_some(), "non-numeric year: {}", s);
                let month = month.expect("non-numeric month");
                let day = day.expect("non-numeric day");
                let hour = hour.expect("non-numeric hour");
                let minute = minute.expect("non-numeric minute");
                let second = second.expect("non-numeric second");

                prop_assert!((1..=12).contains(&month), "month out of range: {}", s);
                prop_assert!((1..=31).contains(&day), "day out of range: {}", s);
                prop_assert!((0..=23).contains(&hour), "hour out of range: {}", s);
                prop_assert!((0..=59).contains(&minute), "minute out of range: {}", s);
                prop_assert!((0..=59).contains(&second), "second out of range: {}", s);
            }
        }
    }
}

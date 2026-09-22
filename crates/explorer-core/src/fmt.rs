//! Rendering helpers shared by the JSON API and the HTML pages.

/// Format a unix timestamp as `timestamp_utc`: `%Y-%m-%d %H:%M:%S` in UTC.
///
/// Implemented rather than pulled in as a dependency. The conversion is a
/// closed-form integer algorithm (Howard Hinnant's `civil_from_days`), it is
/// exactly testable against real captures, and a date library is a large
/// surface to take on for one format string in a project whose whole argument
/// is a small audited dependency tree.
///
/// Monero timestamps are `u64` seconds. Values beyond year 9999 are clamped
/// rather than wrapped, so a hostile block header cannot produce a nonsense
/// year with a plausible shape.
#[must_use]
pub fn timestamp_utc(secs: u64) -> String {
    const MAX: u64 = 253_402_300_799; // 9999-12-31 23:59:59
    let secs = secs.min(MAX);

    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, m, d) = civil_from_days(days);

    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// Days since 1970-01-01 to a proleptic Gregorian (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    // Both are bounded by the algorithm, not by the input: `doy` is a
    // day-of-year in 0..=365 and `mp` a month index in 0..=11, so the day and
    // month land in 1..=31 and 1..=12 whatever `z` was. `secs` is clamped by
    // the caller, so `z` cannot reach the range where this stops holding.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "bounded to 1..=31 and 1..=12 by the algorithm"
    )]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "bounded to 1..=12 by the algorithm"
    )]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// A plain decimal number, or nothing.
///
/// `str::parse::<u64>` accepts a leading `+`, so `"+12"` would otherwise come
/// back as 12. A height in a path is digits or it is wrong, and repairing it
/// would answer a question the caller did not ask.
#[must_use]
pub fn decimal(input: &str) -> Option<u64> {
    if input.is_empty() || !input.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    input.parse().ok()
}

/// Seconds since the Unix epoch, by this machine's clock.
///
/// The only honest answer to "how long ago". The two cheaper-looking
/// substitutes are both wrong in the case a reader cares about most:
///
/// * monerod's `adjusted_time` is derived from recent block timestamps, not
///   from a clock, so a chain that has stopped reports its newest block as
///   brand new. Measured on a stalled testnet: 1h53m of silence shown as two
///   minutes.
/// * Extrapolating from a block's depth assumes a block every two minutes,
///   which is the very thing the age is being consulted about.
#[must_use]
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The `age` string: the gap between two timestamps, rendered `h:m:s`,
/// `d:h:m:s`, or `y:d:h:m:s` depending on magnitude.
///
/// Two details look like bugs and are part of the format:
///
/// * the difference is **absolute**, so a block whose timestamp is ahead of
///   the server clock reports a positive age rather than a negative one,
/// * a year is a flat 31,536,000 seconds, 365 days with no leap handling, so
///   the `y` field drifts against the calendar.
///
/// The day field is three digits wide in the `y:d:h:m:s` form and two in the
/// `d:h:m:s` form. That is the format, not a typo.
#[must_use]
pub fn age(t1: u64, t2: u64) -> String {
    const YEAR: u64 = 31_536_000;
    const DAY: u64 = 86_400;

    let mut diff = t1.abs_diff(t2);
    let years = diff / YEAR;
    diff -= years * YEAR;
    let days = diff / DAY;
    diff -= days * DAY;
    let hours = diff / 3600;
    diff -= hours * 3600;
    let minutes = diff / 60;
    let seconds = diff - minutes * 60;

    if years > 0 {
        format!("{years:02}:{days:03}:{hours:02}:{minutes:02}:{seconds:02}")
    } else if days > 0 {
        format!("{days:02}:{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]

    use super::*;

    /// Both values are lifted from real captures checked in under
    /// `fixtures/`, so these pin against recorded output rather than against
    /// my own arithmetic.
    #[test]
    fn renders_the_timestamps_the_captures_hold() {
        // fixtures/mainnet/gold_block_2000000.json
        assert_eq!(timestamp_utc(1_577_680_194), "2019-12-30 04:29:54");
        // fixtures/testnet/get_transactions_ring.json, block 134721
        assert_eq!(timestamp_utc(1_789_744_451), "2026-09-18 15:14:11");
    }

    #[test]
    fn handles_the_epoch_and_the_leap_day_cases() {
        assert_eq!(timestamp_utc(0), "1970-01-01 00:00:00");
        assert_eq!(timestamp_utc(86_399), "1970-01-01 23:59:59");
        assert_eq!(timestamp_utc(86_400), "1970-01-02 00:00:00");
        // 2000 is a leap year (divisible by 400); 1900 was not.
        assert_eq!(timestamp_utc(951_782_400), "2000-02-29 00:00:00");
        // 2100 is not a leap year, so 2100-03-01 follows 2100-02-28.
        assert_eq!(timestamp_utc(4_107_542_400), "2100-03-01 00:00:00");
    }

    /// A hostile or corrupt timestamp must not render as a plausible date.
    #[test]
    fn an_absurd_timestamp_clamps_instead_of_wrapping() {
        assert_eq!(timestamp_utc(u64::MAX), "9999-12-31 23:59:59");
        assert_eq!(timestamp_utc(253_402_300_799), "9999-12-31 23:59:59");
    }

    /// Seconds, not milliseconds. The unit is invisible in isolation and only
    /// shows up as an age of about fifty thousand years.
    #[test]
    fn now_counts_seconds_since_the_epoch() {
        let t = now();
        assert!(
            (1_577_836_800..4_102_444_800).contains(&t),
            "{t} is not a second count between 2020 and 2100"
        );
    }

    #[test]
    fn age_widens_its_format_as_the_gap_grows() {
        assert_eq!(age(1000, 1000), "00:00:00");
        // The gap a captured response rendered as "00:04:06".
        assert_eq!(age(1_789_790_544 + 246, 1_789_790_544), "00:04:06");
        assert_eq!(age(3661, 0), "01:01:01");
        assert_eq!(age(86_399, 0), "23:59:59");
        // One day: the format gains a field.
        assert_eq!(age(86_400, 0), "01:00:00:00");
        // One 365-day "year": another field, and the day slot widens to three.
        assert_eq!(age(31_536_000, 0), "01:000:00:00:00");
        assert_eq!(age(31_536_000 + 86_400 * 5 + 3661, 0), "01:005:01:01:01");
    }

    /// The difference is absolute, so a block timestamped ahead of the server
    /// clock -- which happens, monerod allows some drift -- reports a positive
    /// age rather than underflowing.
    #[test]
    fn age_is_absolute_so_a_future_timestamp_does_not_underflow() {
        assert_eq!(age(0, 3661), "01:01:01");
        assert_eq!(age(100, 200), age(200, 100));
    }

    /// A year is a flat 31,536,000 seconds. Pinned so that switching to a
    /// calendar year is a deliberate change, not a tidy-up.
    #[test]
    fn a_year_is_three_hundred_and_sixty_five_days_exactly() {
        assert_eq!(age(365 * 86_400, 0), "01:000:00:00:00");
        assert_eq!(age(366 * 86_400, 0), "01:001:00:00:00");
    }

    /// Nothing is repaired on the way in. `1,234` is not height 1234, a
    /// leading sign is not a number, and multi-byte input is rejected whole
    /// rather than having its ASCII picked out of it.
    #[test]
    fn a_height_is_digits_or_it_is_nothing() {
        assert_eq!(decimal("1234"), Some(1234));
        assert_eq!(decimal("0"), Some(0));
        assert_eq!(decimal("1,234"), None);
        assert_eq!(decimal("+12"), None);
        assert_eq!(decimal("-12"), None);
        assert_eq!(decimal(" 12"), None);
        assert_eq!(decimal("12 "), None);
        assert_eq!(decimal("1e3"), None);
        assert_eq!(decimal(""), None);
        assert_eq!(decimal("1\u{00e9}2"), None);
    }

    /// A number too large for the type is refused, not truncated.
    #[test]
    fn a_height_past_the_end_of_the_type_is_refused() {
        assert_eq!(decimal("18446744073709551615"), Some(u64::MAX));
        assert_eq!(decimal("18446744073709551616"), None);
    }
}

//! Calendar dates for signing and for reading the `Date` header.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Seconds in a day: UNIX time has no leap seconds.
const SECONDS_PER_DAY: u64 = 86_400;

/// A proleptic Gregorian date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CivilDate {
    year: u64,
    month: u64,
    day: u64,
}

impl CivilDate {
    /// The date `days` after 1970-01-01, from Howard Hinnant's
    /// `civil_from_days`.
    fn from_days(days: u64) -> Self {
        let z = days + 719_468;
        let era = z / 146_097;
        let doe = z % 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + u64::from(month <= 2);
        Self { year, month, day }
    }

    /// Days from 1970-01-01, the inverse of [`Self::from_days`], from Howard
    /// Hinnant's `days_from_civil`. `None` before 1970.
    fn to_days(self) -> Option<u64> {
        let year = self.year.checked_sub(u64::from(self.month <= 2))?;
        let era = year / 400;
        let yoe = year % 400;
        let doy = (153 * ((self.month + 9) % 12) + 2) / 5 + self.day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        (era * 146_097 + doe).checked_sub(719_468)
    }
}

/// `time` in UTC in the `YYYYMMDDTHHMMSSZ` form AWS signs.
pub(super) fn amz_date(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_secs();
    let (days, rem) = (secs / SECONDS_PER_DAY, secs % SECONDS_PER_DAY);
    let CivilDate { year, month, day } = CivilDate::from_days(days);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Month names as an IMF-fixdate spells them.
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Read an IMF-fixdate, `Sun, 06 Nov 1994 08:49:37 GMT`, the `Date` form
/// RFC 9110 requires of senders. Fractional seconds, which the SDK also
/// accepts, are dropped. Any other text is `None`.
pub(super) fn parse_imf_fixdate(text: &str) -> Option<SystemTime> {
    let (weekday, rest) = text.split_once(", ")?;
    let mut fields = rest.split(' ');
    let (day, month, year, time, zone) = (
        fields.next()?,
        fields.next()?,
        fields.next()?,
        fields.next()?,
        fields.next()?,
    );
    if weekday.len() != 3 || zone != "GMT" || fields.next().is_some() || day.len() != 2 {
        return None;
    }
    let month = MONTHS.iter().position(|m| *m == month)? as u64 + 1;
    let (day, year) = (digits(day.as_bytes())?, digits(year.as_bytes())?);
    let time = time.split_once('.').map_or(time, |(whole, _)| whole);
    let mut clock = time.split(':').map(|field| digits(field.as_bytes()));
    let (hour, minute, second) = (clock.next()??, clock.next()??, clock.next()??);
    if clock.next().is_some() || year > 9999 {
        return None;
    }
    let time = TimeOfDay {
        hour,
        minute,
        second,
        nanos: 0,
    };
    instant(CivilDate { year, month, day }, time)
}

/// `field` read as a decimal number. Empty text, any byte that is not an
/// ASCII digit, or a value past `u64` is `None`.
fn digits(field: &[u8]) -> Option<u64> {
    if field.is_empty() {
        return None;
    }
    field.iter().try_fold(0u64, |n, &d| {
        d.is_ascii_digit()
            .then(|| n.checked_mul(10)?.checked_add(u64::from(d - b'0')))
            .flatten()
    })
}

/// A time of day, as read from a timestamp.
#[derive(Debug, Clone, Copy)]
struct TimeOfDay {
    hour: u64,
    minute: u64,
    second: u64,
    nanos: u32,
}

/// The time at `time` on `date`, in UTC. A date that does not exist, a
/// time out of range, a leap second or a time before 1970 is `None`, as
/// the SDK's parsers refuse them.
fn instant(date: CivilDate, time: TimeOfDay) -> Option<SystemTime> {
    let TimeOfDay {
        hour,
        minute,
        second,
        nanos,
    } = time;
    if !(1..=12).contains(&date.month)
        || date.day == 0
        || date.day > days_in_month(date.year, date.month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let days = date.to_days()?;
    let seconds = days * SECONDS_PER_DAY + hour * 3600 + minute * 60 + second;
    UNIX_EPOCH.checked_add(Duration::new(seconds, nanos))
}

/// Read an RFC 3339 date-time in UTC, `2021-09-21T04:16:53Z`, with
/// optional fractional seconds, as the SDK reads metadata and container
/// answers: `T` or `t` between date and time, and `Z` as the only offset.
/// Any other text, a date that does not exist, a leap second or a time
/// before 1970 is `None`.
pub(super) fn parse_rfc3339(text: &str) -> Option<SystemTime> {
    let (date, time, offset) = date_time(text, Separator::T)?;
    if offset != "Z" {
        return None;
    }
    instant(date, time)
}

/// Read an RFC 3339 date-time with its offset, as the `time` crate reads a
/// `credential_process` expiry: any byte between date and time, then `Z`,
/// `z`, or `+HH:MM` or `-HH:MM` with the hour at most 23. Any other text, a
/// date that does not exist, a leap second or a local time before 1970 is
/// `None`. A month-end leap second, which `time` reads as the nanosecond
/// before the next second, is refused like any other.
pub(super) fn parse_rfc3339_with_offset(text: &str) -> Option<SystemTime> {
    let (date, time, offset) = date_time(text, Separator::AnyByte)?;
    let local = instant(date, time)?;
    if offset.eq_ignore_ascii_case("z") {
        return Some(local);
    }
    let [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] = *offset.as_bytes() else {
        return None;
    };
    let (hour, minute) = (digits(&[h1, h2])?, digits(&[m1, m2])?);
    if hour > 23 || minute > 59 {
        return None;
    }
    let offset = Duration::from_secs(hour * 3600 + minute * 60);
    match sign {
        b'+' => local.checked_sub(offset),
        _ => local.checked_add(offset),
    }
}

/// What may separate the date from the time.
#[derive(Debug, Clone, Copy)]
enum Separator {
    /// `T` or `t`.
    T,
    /// Any one byte.
    AnyByte,
}

/// The date, the time of day and the offset text of an RFC 3339
/// date-time, with fractional seconds kept to the nanosecond.
fn date_time(text: &str, separator: Separator) -> Option<(CivilDate, TimeOfDay, &str)> {
    let bytes = text.as_bytes();
    let field = |range: std::ops::Range<usize>| digits(bytes.get(range)?);
    let separators = [(4, b'-'), (7, b'-'), (13, b':'), (16, b':')];
    let separated = match separator {
        Separator::T => matches!(bytes.get(10), Some(b'T' | b't')),
        Separator::AnyByte => bytes.len() > 10,
    };
    if separators.iter().any(|&(i, c)| bytes.get(i) != Some(&c)) || !separated {
        return None;
    }
    let (year, month, day) = (field(0..4)?, field(5..7)?, field(8..10)?);
    let (hour, minute, second) = (field(11..13)?, field(14..16)?, field(17..19)?);
    let mut rest = &text[19..];
    let mut nanos = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let len = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if len == 0 {
            return None;
        }
        let kept = &fraction[..len.min(9)];
        nanos = kept.parse::<u32>().ok()? * 10u32.pow(9 - kept.len() as u32);
        rest = &fraction[len..];
    }
    let time = TimeOfDay {
        hour,
        minute,
        second,
        nanos,
    };
    Some((CivilDate { year, month, day }, time, rest))
}

/// The days in `month` of `year`.
fn days_in_month(year: u64, month: u64) -> u64 {
    match month {
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The forms the SDK reads: `Z`, either case of `T`, and fractional
    /// seconds kept to the nanosecond. Breaks if an offset or a lowercase
    /// `z` is taken, or if an impossible date reads.
    #[test]
    fn reads_rfc3339_as_the_sdk_does() {
        let at = |secs, nanos| Some(UNIX_EPOCH + Duration::new(secs, nanos));
        let cases = [
            ("2021-09-21T04:16:53Z", at(1_632_197_813, 0)),
            ("2021-09-21t04:16:53Z", at(1_632_197_813, 0)),
            ("2021-09-21T04:16:53.5Z", at(1_632_197_813, 500_000_000)),
            (
                "2021-09-21T04:16:53.1234567891Z",
                at(1_632_197_813, 123_456_789),
            ),
            ("2024-02-29T00:00:00Z", at(1_709_164_800, 0)),
            ("2021-09-21T06:16:53+02:00", None),
            ("2021-09-21T04:16:53z", None),
            ("2021-09-21T04:16:53", None),
            ("2021-09-21 04:16:53Z", None),
            ("2023-02-29T00:00:00Z", None),
            ("2021-09-21T04:16:60Z", None),
            ("2021-09-21T04:16:53.Z", None),
            ("1969-12-31T23:59:59Z", None),
            ("2021-9-21T04:16:53Z", None),
        ];
        for (text, expected) in cases {
            assert_eq!(parse_rfc3339(text), expected, "{text}");
        }
    }

    /// The forms the `time` crate reads for a `credential_process` expiry:
    /// an offset moves the instant, `z` and any separator byte are taken,
    /// and an offset hour past 23 is refused. Breaks if an offset is
    /// ignored or applied the wrong way, or the UTC reader's rules leak in.
    #[test]
    fn reads_rfc3339_with_an_offset_as_time_does() {
        let at = |secs| Some(UNIX_EPOCH + Duration::from_secs(secs));
        let cases = [
            ("2022-05-02T18:36:00+00:00", at(1_651_516_560)),
            ("2022-05-02T20:36:00+02:00", at(1_651_516_560)),
            ("2022-05-02T16:06:00-02:30", at(1_651_516_560)),
            ("2022-05-02T18:36:00z", at(1_651_516_560)),
            ("2022-05-02 18:36:00Z", at(1_651_516_560)),
            ("2022-05-02T18:36:00+24:00", None),
            ("2022-05-02T18:36:00+02:60", None),
            ("2022-05-02T18:36:00+0200", None),
            ("2022-05-02T18:36:00", None),
            ("2022-05-02T18:36:60Z", None),
            ("2016-12-31T23:59:60Z", None),
        ];
        for (text, expected) in cases {
            assert_eq!(parse_rfc3339_with_offset(text), expected, "{text}");
        }
    }

    /// The two conversions invert each other across leap days and century
    /// years, and nothing before 1970 converts.
    #[test]
    fn counts_days_from_a_date() {
        for days in [0, 11_016, 19_782, 20_088, 47_541, 2_932_896] {
            let date = CivilDate::from_days(days);
            assert_eq!(date.to_days(), Some(days), "{date:?}");
        }
        let before = CivilDate {
            year: 1969,
            month: 12,
            day: 31,
        };
        assert_eq!(before.to_days(), None);
    }

    /// The parser agrees with `httpdate` on what `httpdate` writes, takes
    /// the SDK's fractional seconds, and refuses the obsolete forms,
    /// anything not in GMT, a day the month does not have and a leap
    /// second, as the SDK does.
    #[test]
    fn reads_an_imf_fixdate() {
        for secs in [0, 951_782_400, 1_709_251_199, 1_735_689_599, 4_107_542_400] {
            let time = UNIX_EPOCH + Duration::from_secs(secs);
            let text = httpdate::fmt_http_date(time);
            assert_eq!(parse_imf_fixdate(&text), Some(time), "{text}");
        }
        assert_eq!(
            parse_imf_fixdate("Mon, 16 Dec 2019 23:48:18.123 GMT"),
            Some(UNIX_EPOCH + Duration::from_secs(1_576_540_098))
        );
        for bad in [
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
            "Sun, 06 Nov 1994 08:49:37 UTC",
            "Sun, 06 Nov 1994 08:49 GMT",
            "Sun, 32 Nov 1994 08:49:37 GMT",
            "Sun, 06 Foo 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 24:00:00 GMT",
            "Sun, +6 Nov 1994 08:49:37 GMT",
            "Sat, 01 Jan 0000 00:00:00 GMT",
            "Fri, 30 Feb 2024 00:00:00 GMT",
            "Sat, 31 Dec 2016 23:59:60 GMT",
            "Sun, 06 Nov 99999999999999999999 08:49:37 GMT",
        ] {
            assert_eq!(parse_imf_fixdate(bad), None, "{bad}");
        }
    }
}

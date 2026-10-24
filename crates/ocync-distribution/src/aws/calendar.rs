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
    let number = |field: &str| -> Option<u64> {
        field
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| field.parse().ok())
            .flatten()
    };
    let month = MONTHS.iter().position(|m| *m == month)? as u64 + 1;
    let (day, year) = (number(day)?, number(year)?);
    let time = time.split_once('.').map_or(time, |(whole, _)| whole);
    let mut clock = time.split(':').map(number);
    let (hour, minute, second) = (clock.next()??, clock.next()??, clock.next()??);
    if clock.next().is_some()
        || !(1..=31).contains(&day)
        || year > 9999
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = CivilDate { year, month, day }.to_days()?;
    let seconds = days * SECONDS_PER_DAY + hour * 3600 + minute * 60 + second;
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    /// the SDK's fractional seconds, and refuses the obsolete forms and
    /// anything not in GMT.
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
        ] {
            assert_eq!(parse_imf_fixdate(bad), None, "{bad}");
        }
    }
}

//! HTTP date formatting (RFC 9110 §5.6.7 IMF-fixdate), hand-rolled.

use std::time::{SystemTime, UNIX_EPOCH};

const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Formats `time` as an HTTP date in IMF-fixdate form, always in GMT — the
/// format of the `Date`, `Last-Modified` and `Expires` headers.
///
/// A time before the Unix epoch is clamped to the epoch.
///
/// ```
/// use std::time::{Duration, UNIX_EPOCH};
/// use ferroly::http::format_http_date;
///
/// let t = UNIX_EPOCH + Duration::from_secs(784_111_777);
/// assert_eq!(format_http_date(t), "Sun, 06 Nov 1994 08:49:37 GMT");
/// ```
pub fn format_http_date(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        // 1970-01-01 was a Thursday.
        DAYS[(days % 7) as usize],
        day,
        MONTHS[(month - 1) as usize],
        year,
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Converts days since 1970-01-01 to a proleptic Gregorian `(year, month, day)`.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    // Shift the epoch to 0000-03-01 so leap days fall at the end of a year.
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

/// Parses a `Retry-After` value (delay seconds, or an IMF-fixdate converted
/// against `now_unix`; a past date is a zero delay). Shared by
/// `clients::parse_retry_after` and the OTLP exporter.
#[cfg(any(feature = "clients", feature = "obs"))]
pub(crate) fn parse_retry_after_at(value: &str, now_unix: u64) -> Option<std::time::Duration> {
    use std::time::Duration;
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        return value.parse().ok().map(Duration::from_secs);
    }
    let at = parse_imf_fixdate(value)?;
    Some(Duration::from_secs(at.saturating_sub(now_unix)))
}

/// Parses `"Sun, 06 Nov 1994 08:49:37 GMT"` into Unix seconds.
#[cfg(any(feature = "clients", feature = "obs"))]
fn parse_imf_fixdate(value: &str) -> Option<u64> {
    let mut fields = value.split(' ');
    let weekday = fields.next()?;
    if weekday.len() != 4 || !weekday.ends_with(',') {
        return None;
    }
    let day = two_digits(fields.next()?)?;
    let month = match fields.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year = fields.next()?;
    if year.len() != 4 || !year.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let year: u64 = year.parse().ok()?;
    let mut clock = fields.next()?.split(':');
    let hour = two_digits(clock.next()?)?;
    let minute = two_digits(clock.next()?)?;
    let second = two_digits(clock.next()?)?;
    if clock.next().is_some() || fields.next() != Some("GMT") || fields.next().is_some() {
        return None;
    }
    if year < 1970 || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    // Days since 1970-01-01 (the civil-from-days algorithm, inverted).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y / 400;
    let year_of_era = y % 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = (era * 146_097 + day_of_era).checked_sub(719_468)?;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

#[cfg(any(feature = "clients", feature = "obs"))]
fn two_digits(text: &str) -> Option<u64> {
    if text.len() == 2 && text.bytes().all(|b| b.is_ascii_digit()) {
        text.parse().ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(secs: u64) -> String {
        format_http_date(UNIX_EPOCH + Duration::from_secs(secs))
    }

    #[test]
    fn formats_known_dates() {
        assert_eq!(at(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(at(784_111_777), "Sun, 06 Nov 1994 08:49:37 GMT");
        // Leap day, end of year, and the first second of the next year.
        assert_eq!(at(951_782_400), "Tue, 29 Feb 2000 00:00:00 GMT");
        assert_eq!(at(1_709_251_199), "Thu, 29 Feb 2024 23:59:59 GMT");
        assert_eq!(at(1_735_689_599), "Tue, 31 Dec 2024 23:59:59 GMT");
        assert_eq!(at(1_735_689_600), "Wed, 01 Jan 2025 00:00:00 GMT");
        // 2100 is not a leap year.
        assert_eq!(at(4_107_542_400), "Mon, 01 Mar 2100 00:00:00 GMT");
        assert_eq!(
            format_http_date(UNIX_EPOCH - Duration::from_secs(5)),
            "Thu, 01 Jan 1970 00:00:00 GMT"
        );
    }
}

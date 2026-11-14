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

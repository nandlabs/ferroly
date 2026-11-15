//! Date-time support: an RFC 3339 [`DateTime`] value type, and the
//! `Encode` / `Decode` impls for [`std::time::SystemTime`].
//!
//! [`DateTime`] is an instant plus the UTC offset it was written with,
//! encoded as an RFC 3339 string such as `2026-11-15T09:30:00.25+01:00`.
//! `SystemTime` (which has no offset) encodes as an RFC 3339 string in UTC
//! (`…Z`) and decodes from any RFC 3339 date-time or from a number of seconds
//! since the Unix epoch.
//!
//! ```
//! use std::time::{Duration, SystemTime, UNIX_EPOCH};
//! use ferroly::codec::{json, DateTime};
//!
//! let at = DateTime::parse("2026-11-15T09:30:00.25+01:00").unwrap();
//! assert_eq!(at.unix_timestamp(), 1_794_731_400);
//! assert_eq!((at.year(), at.month(), at.day(), at.hour()), (2026, 11, 15, 9));
//! assert_eq!(at.to_string(), "2026-11-15T09:30:00.25+01:00");
//! assert_eq!(at.to_utc().unwrap().to_string(), "2026-11-15T08:30:00.25Z");
//!
//! let t = UNIX_EPOCH + Duration::from_secs(1_794_731_400);
//! assert_eq!(json::encode(&t), r#""2026-11-15T08:30:00Z""#);
//! assert_eq!(json::decode::<SystemTime>(r#""2026-11-15T09:30:00+01:00""#).unwrap(), t);
//! assert_eq!(json::decode::<SystemTime>("1794731400").unwrap(), t);
//! ```

use std::fmt;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ferroly::codec::{CodecError, Decode, Decoder, Encode, Encoder, Value};

const SECS_PER_DAY: i64 = 86_400;
/// Seconds from the Unix epoch to 0000-01-01T00:00:00Z.
const MIN_LOCAL: i64 = -62_167_219_200;
/// Seconds from the Unix epoch to 9999-12-31T23:59:59Z.
const MAX_LOCAL: i64 = 253_402_300_799;
/// The largest offset RFC 3339 can write: 23:59.
const MAX_OFFSET_MINUTES: i16 = 23 * 60 + 59;

/// An RFC 3339 date-time: an instant with nanosecond precision and the UTC
/// offset it is expressed in.
///
/// Its local date (the instant shifted by the offset) lies in years
/// 0000–9999, the range RFC 3339 can write. Equality, hashing and ordering
/// compare the instant first and then the offset, so `08:30Z` and
/// `09:30+01:00` are the same instant ([`same_instant`](Self::same_instant))
/// but not equal values.
///
/// Parsing accepts `T`, `t` or a space between date and time, `Z`/`z` or
/// `±hh:mm` for the offset (`-00:00` is read as UTC), and any number of
/// fraction digits (beyond nine they are truncated). A leap second (`:60`)
/// is read as the first instant of the next minute. Encoding writes `T`, `Z`
/// for a zero offset, and only as many fraction digits as needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DateTime {
    /// Seconds since the Unix epoch (the UTC instant).
    secs: i64,
    nanos: u32,
    /// Minutes east of UTC.
    offset: i16,
}

impl DateTime {
    /// The UTC date-time `secs` seconds and `nanos` nanoseconds after the
    /// Unix epoch (`secs` may be negative). `None` when `nanos` is not below
    /// one second or the date falls outside years 0000–9999.
    pub fn from_unix(secs: i64, nanos: u32) -> Option<DateTime> {
        DateTime {
            secs,
            nanos,
            offset: 0,
        }
        .checked()
    }

    /// The UTC date-time of `time`, or `None` outside years 0000–9999.
    pub fn from_system_time(time: SystemTime) -> Option<DateTime> {
        let (secs, nanos) = split_system_time(time)?;
        DateTime::from_unix(secs, nanos)
    }

    /// The current time, in UTC.
    pub fn now() -> DateTime {
        // The clock is within range for the next eight thousand years.
        DateTime::from_system_time(SystemTime::now()).unwrap_or(DateTime {
            secs: MAX_LOCAL,
            nanos: 0,
            offset: 0,
        })
    }

    /// Parses an RFC 3339 date-time such as `2026-11-15T09:30:00Z` or
    /// `1985-04-12T23:20:50.52-04:00`.
    pub fn parse(text: &str) -> Result<DateTime, CodecError> {
        parse_rfc3339(text.as_bytes()).ok_or_else(|| {
            let shown: String = text.chars().take(40).collect();
            CodecError::Message(format!("invalid RFC 3339 date-time {shown:?}"))
        })
    }

    /// The same instant expressed at `minutes` east of UTC. `None` when the
    /// offset is beyond ±23:59 or the local date would leave years
    /// 0000–9999.
    pub fn with_offset(self, minutes: i16) -> Option<DateTime> {
        DateTime {
            offset: minutes,
            ..self
        }
        .checked()
    }

    /// The same instant in UTC (offset zero). `None` when the UTC date
    /// leaves years 0000–9999, which happens only within a day of either end
    /// (`0000-01-01T00:30:00+01:00` is in year −1 in UTC); RFC 3339 cannot
    /// write such a date, so the value could not round-trip.
    pub fn to_utc(self) -> Option<DateTime> {
        DateTime { offset: 0, ..self }.checked()
    }

    /// The instant as a [`SystemTime`]. `None` when the platform's
    /// `SystemTime` cannot represent it (on 64-bit Unix and on Windows it
    /// always can; WASI, for one, has no times before 1970).
    pub fn to_system_time(&self) -> Option<SystemTime> {
        let base = if self.secs >= 0 {
            UNIX_EPOCH.checked_add(Duration::from_secs(self.secs as u64))
        } else {
            UNIX_EPOCH.checked_sub(Duration::from_secs(self.secs.unsigned_abs()))
        };
        base?.checked_add(Duration::from_nanos(u64::from(self.nanos)))
    }

    /// Whether `self` and `other` are the same instant, whatever their
    /// offsets.
    pub fn same_instant(&self, other: &DateTime) -> bool {
        (self.secs, self.nanos) == (other.secs, other.nanos)
    }

    /// Whole seconds since the Unix epoch (negative before 1970).
    pub fn unix_timestamp(&self) -> i64 {
        self.secs
    }

    /// The nanoseconds past [`unix_timestamp`](Self::unix_timestamp),
    /// below one billion.
    pub fn nanosecond(&self) -> u32 {
        self.nanos
    }

    /// The UTC offset, in minutes east of UTC.
    pub fn offset_minutes(&self) -> i16 {
        self.offset
    }

    /// The local year (in the value's own offset).
    pub fn year(&self) -> i64 {
        self.civil().0
    }

    /// The local month, 1–12.
    pub fn month(&self) -> u32 {
        self.civil().1
    }

    /// The local day of the month, 1–31.
    pub fn day(&self) -> u32 {
        self.civil().2
    }

    /// The local hour, 0–23.
    pub fn hour(&self) -> u32 {
        (self.local().rem_euclid(SECS_PER_DAY) / 3600) as u32
    }

    /// The local minute, 0–59.
    pub fn minute(&self) -> u32 {
        (self.local().rem_euclid(3600) / 60) as u32
    }

    /// The local second, 0–59.
    pub fn second(&self) -> u32 {
        self.local().rem_euclid(60) as u32
    }

    /// Seconds since the epoch, shifted by the offset.
    fn local(&self) -> i64 {
        self.secs + i64::from(self.offset) * 60
    }

    fn civil(&self) -> (i64, u32, u32) {
        civil_from_days(self.local().div_euclid(SECS_PER_DAY))
    }

    /// `Some(self)` when the invariants hold.
    fn checked(self) -> Option<DateTime> {
        let ok = self.nanos < 1_000_000_000
            && self.offset.unsigned_abs() <= MAX_OFFSET_MINUTES.unsigned_abs()
            && (MIN_LOCAL..=MAX_LOCAL)
                .contains(&self.secs.checked_add(i64::from(self.offset) * 60)?);
        ok.then_some(self)
    }
}

impl fmt::Display for DateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (year, month, day) = self.civil();
        write!(
            f,
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
            self.hour(),
            self.minute(),
            self.second()
        )?;
        if self.nanos != 0 {
            let digits = format!("{:09}", self.nanos);
            write!(f, ".{}", digits.trim_end_matches('0'))?;
        }
        if self.offset == 0 {
            f.write_str("Z")
        } else {
            let sign = if self.offset < 0 { '-' } else { '+' };
            let abs = self.offset.unsigned_abs();
            write!(f, "{sign}{:02}:{:02}", abs / 60, abs % 60)
        }
    }
}

impl FromStr for DateTime {
    type Err = CodecError;

    fn from_str(text: &str) -> Result<DateTime, CodecError> {
        DateTime::parse(text)
    }
}

impl Encode for DateTime {
    fn encode(&self) -> Value {
        Value::Str(self.to_string())
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        e.encode_str(&self.to_string());
    }
}

impl Decode for DateTime {
    /// From an RFC 3339 string (in both the strict and lenient modes).
    fn decode(value: &Value) -> Result<Self, CodecError> {
        match value {
            Value::Str(s) => DateTime::parse(s.trim()),
            _ => Err(CodecError::expected("RFC 3339 date-time string")),
        }
    }
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
        match d.decode_value()? {
            Value::Str(s) => DateTime::parse(&s),
            _ => Err(CodecError::expected("RFC 3339 date-time string")),
        }
    }
}

/// A [`SystemTime`] encodes as an RFC 3339 string in UTC
/// (`2026-11-15T08:30:00.5Z`). Outside years 0000–9999, which RFC 3339 cannot
/// write, it encodes as `{ "secs": i64, "nanos": u32 }` relative to the Unix
/// epoch instead.
///
/// It decodes from an RFC 3339 string with any offset, from that object, or
/// from a number of seconds since the epoch (an integer, or a float for
/// fractions; negative before 1970).
impl Encode for SystemTime {
    fn encode(&self) -> Value {
        match DateTime::from_system_time(*self) {
            Some(at) => Value::Str(at.to_string()),
            None => {
                // Beyond the `i64` range (no 64-bit platform's `SystemTime`
                // reaches it) the nearest end, keeping the sign.
                let (secs, nanos) = split_or_saturate(*self).unwrap_or_else(|end| end);
                Value::Object(vec![
                    ("secs".to_string(), Value::Int(secs)),
                    ("nanos".to_string(), Value::UInt(u64::from(nanos))),
                ])
            }
        }
    }
}

impl Decode for SystemTime {
    fn decode(value: &Value) -> Result<Self, CodecError> {
        system_time_from(value, true)
    }
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
        system_time_from(&d.decode_value()?, false)
    }
}

fn system_time_from(value: &Value, lenient: bool) -> Result<SystemTime, CodecError> {
    const WHAT: &str = "time (RFC 3339 string, {secs, nanos} or seconds)";
    let out_of_range = || CodecError::out_of_range("SystemTime");
    let from_parts = |secs: i64, nanos: u32| -> Result<SystemTime, CodecError> {
        let base = if secs >= 0 {
            UNIX_EPOCH.checked_add(Duration::from_secs(secs as u64))
        } else {
            UNIX_EPOCH.checked_sub(Duration::from_secs(secs.unsigned_abs()))
        };
        base.and_then(|t| t.checked_add(Duration::from_nanos(u64::from(nanos))))
            .ok_or_else(out_of_range)
    };
    let from_float = |f: f64| -> Result<SystemTime, CodecError> {
        if !f.is_finite() {
            return Err(CodecError::expected(WHAT));
        }
        let d = Duration::try_from_secs_f64(f.abs()).map_err(|_| out_of_range())?;
        if f >= 0.0 {
            UNIX_EPOCH.checked_add(d)
        } else {
            UNIX_EPOCH.checked_sub(d)
        }
        .ok_or_else(out_of_range)
    };
    match value {
        Value::Str(s) => {
            let text = if lenient { s.trim() } else { s.as_str() };
            match DateTime::parse(text) {
                Ok(at) => at.to_system_time().ok_or_else(out_of_range),
                Err(e) if !lenient => Err(e),
                // Lenient: a text source may carry the number of seconds.
                Err(e) => match text.parse::<i64>() {
                    Ok(secs) => from_parts(secs, 0),
                    Err(_) => text.parse::<f64>().map_err(|_| e).and_then(from_float),
                },
            }
        }
        Value::Int(i) => from_parts(*i, 0),
        Value::UInt(u) => from_parts(i64::try_from(*u).map_err(|_| out_of_range())?, 0),
        Value::Float(f) => from_float(*f),
        Value::Object(entries) => {
            let mut secs = None;
            let mut nanos = 0u32;
            for (key, v) in entries {
                match key.as_str() {
                    "secs" => {
                        secs = Some(if lenient {
                            i64::decode(v)?
                        } else {
                            ferroly::codec::from_value_strict::<i64>(v)?
                        })
                    }
                    "nanos" => {
                        nanos = if lenient {
                            u32::decode(v)?
                        } else {
                            ferroly::codec::from_value_strict::<u32>(v)?
                        }
                    }
                    _ => {}
                }
            }
            let secs = secs.ok_or_else(|| CodecError::missing_field("secs"))?;
            if nanos >= 1_000_000_000 {
                return Err(CodecError::out_of_range("nanos"));
            }
            from_parts(secs, nanos)
        }
        _ => Err(CodecError::expected(WHAT)),
    }
}

/// `time` as signed seconds and nanoseconds since the Unix epoch (`nanos`
/// always counts forward), or `None` beyond the `i64` range.
fn split_system_time(time: SystemTime) -> Option<(i64, u32)> {
    split_or_saturate(time).ok()
}

/// [`split_system_time`], or when `time` is beyond the `i64` range the
/// nearest end of it (`i64::MIN` seconds before the epoch, `i64::MAX` after),
/// as the error.
fn split_or_saturate(time: SystemTime) -> Result<(i64, u32), (i64, u32)> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs())
            .map(|secs| (secs, d.subsec_nanos()))
            .map_err(|_| (i64::MAX, 999_999_999)),
        Err(before) => {
            let d = before.duration();
            // In `i128`, so that exactly 2^63 seconds before the epoch is
            // still `i64::MIN`.
            let (secs, nanos) = match d.subsec_nanos() {
                0 => (-i128::from(d.as_secs()), 0),
                n => (-i128::from(d.as_secs()) - 1, 1_000_000_000 - n),
            };
            i64::try_from(secs)
                .map(|secs| (secs, nanos))
                .map_err(|_| (i64::MIN, 0))
        }
    }
}

// ---- RFC 3339 parsing ------------------------------------------------------

/// Parses `date-time` from RFC 3339 §5.6.
fn parse_rfc3339(b: &[u8]) -> Option<DateTime> {
    let num = |range: std::ops::Range<usize>| -> Option<u32> {
        let digits = b.get(range)?;
        digits.iter().try_fold(0u32, |acc, &d| {
            d.is_ascii_digit().then(|| acc * 10 + u32::from(d - b'0'))
        })
    };
    let at = |i: usize, c: u8| b.get(i) == Some(&c);
    if !(at(4, b'-') && at(7, b'-') && at(13, b':') && at(16, b':')) {
        return None;
    }
    if !matches!(b.get(10), Some(b'T' | b't' | b' ')) {
        return None;
    }
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(i64::from(year), month)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut i = 19;
    let mut nanos = 0u32;
    if at(i, b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            if i - start < 9 {
                nanos = nanos * 10 + u32::from(b[i] - b'0');
            }
            i += 1;
        }
        let digits = i - start;
        if digits == 0 {
            return None;
        }
        for _ in digits..9 {
            nanos *= 10;
        }
    }
    let offset: i16 = match b.get(i) {
        Some(b'Z' | b'z') => {
            i += 1;
            0
        }
        Some(sign @ (b'+' | b'-')) => {
            if !at(i + 3, b':') {
                return None;
            }
            let (h, m) = (num(i + 1..i + 3)?, num(i + 4..i + 6)?);
            if h > 23 || m > 59 {
                return None;
            }
            let minutes = (h * 60 + m) as i16;
            i += 6;
            if *sign == b'-' {
                -minutes
            } else {
                minutes
            }
        }
        _ => return None,
    };
    if i != b.len() {
        return None;
    }
    let days = days_from_civil(i64::from(year), month, day);
    let local =
        days * SECS_PER_DAY + i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second);
    DateTime {
        secs: local - i64::from(offset) * 60,
        nanos,
        offset,
    }
    // A leap second at the very end of 9999 has no local date to land on.
    .checked()
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's
/// algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The proleptic Gregorian date of `days` since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroly::codec::{from_value_strict, json, toml, xml, yaml};

    fn dt(s: &str) -> DateTime {
        DateTime::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn parses_and_formats_rfc3339() {
        let cases = [
            ("1970-01-01T00:00:00Z", 0, 0, 0, "1970-01-01T00:00:00Z"),
            (
                "1985-04-12T23:20:50.52Z",
                482_196_050,
                520_000_000,
                0,
                "1985-04-12T23:20:50.52Z",
            ),
            (
                "1996-12-19T16:39:57-08:00",
                851_042_397,
                0,
                -480,
                "1996-12-19T16:39:57-08:00",
            ),
            (
                "1990-12-31T23:59:60Z",
                662_688_000,
                0,
                0,
                "1991-01-01T00:00:00Z",
            ),
            (
                "1937-01-01T12:00:27.87+00:20",
                -1_041_337_173,
                870_000_000,
                20,
                "1937-01-01T12:00:27.87+00:20",
            ),
            (
                "2000-02-29t01:02:03z",
                951_786_123,
                0,
                0,
                "2000-02-29T01:02:03Z",
            ),
            (
                "2026-11-15 09:30:00-00:00",
                1_794_735_000,
                0,
                0,
                "2026-11-15T09:30:00Z",
            ),
            (
                "2026-11-15T09:30:00.1234567891Z",
                1_794_735_000,
                123_456_789,
                0,
                "2026-11-15T09:30:00.123456789Z",
            ),
            (
                "0000-01-01T00:00:00Z",
                MIN_LOCAL,
                0,
                0,
                "0000-01-01T00:00:00Z",
            ),
            (
                "9999-12-31T23:59:59.999999999Z",
                MAX_LOCAL,
                999_999_999,
                0,
                "9999-12-31T23:59:59.999999999Z",
            ),
            (
                "1969-12-31T23:59:59.5Z",
                -1,
                500_000_000,
                0,
                "1969-12-31T23:59:59.5Z",
            ),
        ];
        for (text, secs, nanos, offset, shown) in cases {
            let at = dt(text);
            assert_eq!(
                (at.unix_timestamp(), at.nanosecond(), at.offset_minutes()),
                (secs, nanos, offset),
                "{text}"
            );
            assert_eq!(at.to_string(), shown, "{text}");
            assert_eq!(dt(shown), at, "{shown} round-trips");
        }
    }

    #[test]
    fn rejects_malformed_or_impossible_dates() {
        for text in [
            "",
            "2026-11-15",
            "2026-11-15T09:30:00",
            "2026-11-15T09:30Z",
            "2026-13-01T00:00:00Z",
            "2026-00-01T00:00:00Z",
            "2026-02-29T00:00:00Z",
            "1900-02-29T00:00:00Z",
            "2026-04-31T00:00:00Z",
            "2026-11-15T24:00:00Z",
            "2026-11-15T09:60:00Z",
            "2026-11-15T09:30:61Z",
            "2026-11-15T09:30:00.Z",
            "2026-11-15T09:30:00+0100",
            "2026-11-15T09:30:00+24:00",
            "2026-11-15T09:30:00+01:60",
            "2026-11-15T09:30:00Zjunk",
            "2026-11-15X09:30:00Z",
            "+2026-11-15T09:30:00Z",
            "２０２６-11-15T09:30:00Z",
            "9999-12-31T23:59:60Z",
        ] {
            assert!(DateTime::parse(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn civil_conversion_agrees_both_ways() {
        for days in (-719_528..2_932_896).step_by(997) {
            let (y, m, d) = civil_from_days(days);
            assert!((1..=12).contains(&m) && d >= 1 && d <= days_in_month(y, m));
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }

    #[test]
    fn components_offsets_and_instants() {
        let a = dt("2026-11-15T09:30:05.25+01:00");
        assert_eq!(
            (
                a.year(),
                a.month(),
                a.day(),
                a.hour(),
                a.minute(),
                a.second()
            ),
            (2026, 11, 15, 9, 30, 5)
        );
        let utc = a.to_utc().unwrap();
        assert_eq!(utc.hour(), 8);
        assert!(utc.same_instant(&a) && utc != a);
        assert_eq!(utc.with_offset(60), Some(a));
        assert_eq!(
            a.with_offset(-330).unwrap().to_string(),
            "2026-11-15T03:00:05.25-05:30"
        );
        assert!(a.with_offset(24 * 60).is_none());
        assert!(dt("9999-12-31T23:00:00Z").with_offset(120).is_none());
        assert!(DateTime::from_unix(0, 1_000_000_000).is_none());
        assert!(DateTime::from_unix(MAX_LOCAL + 1, 0).is_none());
        assert_eq!(
            DateTime::from_unix(-1, 0).unwrap().to_string(),
            "1969-12-31T23:59:59Z"
        );
        assert!(dt("2026-01-01T00:00:00Z") < dt("2026-01-01T00:00:00.000000001Z"));
        assert!(DateTime::now().year() >= 2026);
    }

    #[test]
    fn date_times_encode_and_decode_in_every_format() {
        #[derive(Encode, Decode, PartialEq, Debug)]
        struct Event {
            at: DateTime,
            seen: Option<SystemTime>,
        }
        let event = Event {
            at: dt("2026-11-15T09:30:00.5+01:00"),
            seen: Some(UNIX_EPOCH + Duration::new(1_794_731_400, 250_000_000)),
        };
        let text = json::encode(&event);
        assert_eq!(
            text,
            r#"{"at":"2026-11-15T09:30:00.5+01:00","seen":"2026-11-15T08:30:00.25Z"}"#
        );
        assert_eq!(json::decode::<Event>(&text).unwrap(), event);
        assert_eq!(yaml::decode::<Event>(&yaml::encode(&event)).unwrap(), event);
        assert_eq!(xml::decode::<Event>(&xml::encode(&event)).unwrap(), event);
        assert_eq!(toml::decode::<Event>(&toml::encode(&event)).unwrap(), event);
        assert_eq!(from_value_strict::<Event>(&event.encode()).unwrap(), event);
        assert!(json::decode::<DateTime>("0").is_err());
        assert!(json::decode::<DateTime>(r#""yesterday""#).is_err());
    }

    #[test]
    fn system_time_forms() {
        let t = UNIX_EPOCH + Duration::from_secs(1_794_731_400);
        for text in [
            r#""2026-11-15T08:30:00Z""#,
            r#""2026-11-15T03:30:00-05:00""#,
            "1794731400",
            "1794731400.0",
            r#"{"secs":1794731400,"nanos":0}"#,
        ] {
            assert_eq!(json::decode::<SystemTime>(text).unwrap(), t, "{text}");
        }
        // Before the epoch.
        let before = UNIX_EPOCH - Duration::from_millis(1500);
        assert_eq!(json::encode(&before), r#""1969-12-31T23:59:58.5Z""#);
        assert_eq!(
            json::decode::<SystemTime>(r#""1969-12-31T23:59:58.5Z""#).unwrap(),
            before
        );
        assert_eq!(json::decode::<SystemTime>("-1.5").unwrap(), before);
        // Strict mode does not read a number out of a string; lenient does.
        assert!(json::decode::<SystemTime>(r#""1794731400""#).is_err());
        assert_eq!(
            SystemTime::decode(&Value::Str(" 1794731400 ".into())).unwrap(),
            t
        );
        assert!(json::decode::<SystemTime>("true").is_err());
        assert!(json::decode::<SystemTime>(r#"{"secs":1,"nanos":1000000000}"#).is_err());
    }

    #[test]
    fn far_system_times_fall_back_to_seconds() {
        let far = UNIX_EPOCH + Duration::from_secs(MAX_LOCAL as u64 + 10);
        let value = far.encode();
        assert_eq!(
            value,
            Value::Object(vec![
                ("secs".into(), Value::Int(MAX_LOCAL + 10)),
                ("nanos".into(), Value::UInt(0)),
            ])
        );
        assert_eq!(SystemTime::decode(&value).unwrap(), far);
        assert_eq!(
            json::decode::<SystemTime>(&json::encode(&far)).unwrap(),
            far
        );
    }

    #[test]
    fn extreme_offsets_are_rejected_not_wrapped() {
        // CORE-11: `i16::MIN.abs()` overflowed (a panic in debug builds, a
        // negative offset accepted in release builds).
        let a = dt("2026-11-15T09:30:00Z");
        for minutes in [i16::MIN, i16::MIN + 1, i16::MAX, 24 * 60, -24 * 60] {
            assert_eq!(a.with_offset(minutes), None, "{minutes}");
        }
        for minutes in [MAX_OFFSET_MINUTES, -MAX_OFFSET_MINUTES, 0] {
            assert_eq!(a.with_offset(minutes).unwrap().offset_minutes(), minutes);
        }
    }

    #[test]
    fn to_utc_never_leaves_the_writable_range() {
        // CORE-12: `to_utc` used to return a value in year -1 or 10000,
        // which encoded as a string `parse` rejects.
        for text in [
            "0000-01-01T00:30:00+01:00",
            "0000-01-01T23:58:59+23:59",
            "9999-12-31T23:30:00-01:00",
            "9999-12-31T00:01:00-23:59",
        ] {
            assert_eq!(dt(text).to_utc(), None, "{text}");
        }
        for text in [
            "0000-01-01T01:00:00+01:00",
            "9999-12-31T22:59:59.5-01:00",
            "2026-11-15T09:30:00.25+01:00",
        ] {
            let utc = dt(text).to_utc().unwrap();
            assert!(utc.same_instant(&dt(text)) && utc.offset_minutes() == 0);
            assert_eq!(DateTime::parse(&utc.to_string()), Ok(utc), "{text}");
        }
    }

    #[test]
    fn to_system_time_round_trips_the_whole_range() {
        // CORE-14: the conversion is checked. 64-bit Unix represents the
        // whole 0000–9999 range; Windows' `SystemTime` starts in 1601, so
        // earlier instants are `None` there (and a decode error), never a
        // panic.
        for text in [
            "0000-01-01T00:00:00Z",
            "1601-01-01T00:00:00Z",
            "1969-12-31T23:59:59.5Z",
            // 100 ns steps: the resolution of Windows' `SystemTime`.
            "9999-12-31T23:59:59.9999999Z",
        ] {
            let at = dt(text);
            match at.to_system_time() {
                Some(t) => {
                    assert_eq!(DateTime::from_system_time(t), Some(at), "{text}");
                    assert_eq!(json::decode::<SystemTime>(&format!("{text:?}")).unwrap(), t);
                }
                None => {
                    assert!(cfg!(windows) && text.starts_with("0000"), "{text}");
                    assert!(json::decode::<SystemTime>(&format!("{text:?}")).is_err());
                }
            }
        }
    }

    #[test]
    fn far_system_times_keep_their_sign() {
        // CORE-13: exactly 2^63 s before the epoch used to re-encode as
        // `i64::MAX` seconds after it.
        for doc in [
            r#"{"secs":-9223372036854775808,"nanos":0}"#,
            r#"{"secs":-9223372036854775808,"nanos":5}"#,
            r#"{"secs":-9223372036854775807,"nanos":999999999}"#,
            r#"{"secs":9223372036854775807,"nanos":999999999}"#,
        ] {
            // A platform whose `SystemTime` cannot reach that far (Windows)
            // rejects the document instead.
            if let Ok(t) = json::decode::<SystemTime>(doc) {
                assert_eq!(json::encode(&t), doc);
            }
        }
        // About 35,000 years before the epoch: before Windows' 1601 start.
        if let Some(far) = UNIX_EPOCH.checked_sub(Duration::from_secs(1 << 40)) {
            assert_eq!(split_or_saturate(far), Ok((-(1 << 40), 0)));
        }
    }
}

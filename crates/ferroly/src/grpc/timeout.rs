//! The `grpc-timeout` header: up to eight digits and a unit.

use std::time::Duration;

const MAX_VALUE: u64 = 99_999_999;

/// Parses a `grpc-timeout` value: one to eight ASCII digits followed by a
/// unit — `H` hours, `M` minutes, `S` seconds, `m` milliseconds, `u`
/// microseconds, `n` nanoseconds. Anything else is `None`.
///
/// ```
/// use std::time::Duration;
/// use ferroly::grpc::{format_timeout, parse_timeout};
///
/// assert_eq!(parse_timeout("100m"), Some(Duration::from_millis(100)));
/// assert_eq!(parse_timeout("2H"), Some(Duration::from_secs(7200)));
/// assert_eq!(parse_timeout("123456789S"), None); // nine digits
/// assert_eq!(format_timeout(Duration::from_millis(1500)), "1500000u");
/// ```
pub fn parse_timeout(value: &str) -> Option<Duration> {
    let value = value.as_bytes();
    let (&unit, digits) = value.split_last()?;
    if digits.is_empty() || digits.len() > 8 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let n = digits
        .iter()
        .fold(0u64, |n, d| n * 10 + u64::from(d - b'0'));
    Some(match unit {
        b'H' => Duration::from_secs(n * 3600),
        b'M' => Duration::from_secs(n * 60),
        b'S' => Duration::from_secs(n),
        b'm' => Duration::from_millis(n),
        b'u' => Duration::from_micros(n),
        b'n' => Duration::from_nanos(n),
        _ => return None,
    })
}

/// Formats a duration as a `grpc-timeout` value, in the finest unit whose
/// value fits in eight digits, rounding up so the peer never sees a shorter
/// deadline than the caller set. Longer than 99 999 999 hours is capped
/// there; zero is `0n`.
pub fn format_timeout(timeout: Duration) -> String {
    let nanos = timeout.as_nanos();
    if nanos == 0 {
        return "0n".to_string();
    }
    const UNITS: [(u128, char); 6] = [
        (1, 'n'),
        (1_000, 'u'),
        (1_000_000, 'm'),
        (1_000_000_000, 'S'),
        (60_000_000_000, 'M'),
        (3_600_000_000_000, 'H'),
    ];
    for (size, unit) in UNITS {
        let value = nanos.div_ceil(size);
        if value <= u128::from(MAX_VALUE) {
            return format!("{value}{unit}");
        }
    }
    format!("{MAX_VALUE}H")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_every_unit_and_reject_bad_values() {
        assert_eq!(parse_timeout("1H"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_timeout("1M"), Some(Duration::from_secs(60)));
        assert_eq!(parse_timeout("1S"), Some(Duration::from_secs(1)));
        assert_eq!(parse_timeout("1m"), Some(Duration::from_millis(1)));
        assert_eq!(parse_timeout("1u"), Some(Duration::from_micros(1)));
        assert_eq!(parse_timeout("1n"), Some(Duration::from_nanos(1)));
        assert_eq!(parse_timeout("0n"), Some(Duration::ZERO));
        assert_eq!(
            parse_timeout("99999999H"),
            Some(Duration::from_secs(99_999_999 * 3600))
        );
        for bad in [
            "",
            "S",
            "1",
            "1s",
            "1h",
            "-1S",
            "+1S",
            " 1S",
            "1S ",
            "1.5S",
            "123456789n",
            "1x",
            "١S",
        ] {
            assert_eq!(parse_timeout(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn format_picks_the_finest_unit_and_rounds_up() {
        assert_eq!(format_timeout(Duration::ZERO), "0n");
        assert_eq!(format_timeout(Duration::from_nanos(5)), "5n");
        assert_eq!(format_timeout(Duration::from_millis(99)), "99000000n");
        assert_eq!(format_timeout(Duration::from_millis(100)), "100000u");
        assert_eq!(format_timeout(Duration::from_nanos(100_000_001)), "100001u");
        assert_eq!(format_timeout(Duration::from_secs(100)), "100000m");
        assert_eq!(format_timeout(Duration::from_secs(100_000)), "100000S");
        assert_eq!(format_timeout(Duration::from_secs(100_000_000)), "1666667M");
        assert_eq!(
            format_timeout(Duration::from_secs(10_000_000_000)),
            "2777778H"
        );
        assert_eq!(format_timeout(Duration::MAX), "99999999H");
        for d in [1u64, 999, 123_456_789, 86_400_000_000_000] {
            let d = Duration::from_nanos(d);
            let back = parse_timeout(&format_timeout(d)).unwrap();
            assert!(back >= d && back - d < Duration::from_secs(60), "{d:?}");
        }
    }
}

//! Allocation-free scalar writers shared by the JSON, XML, YAML and TOML
//! encoders.
//!
//! The `Value::Int`/`UInt`/`Float` arms would otherwise call `x.to_string()`,
//! heap-allocating a `String` per scalar. These append directly into the output
//! buffer instead: integers via a stack `itoa`, floats via `fmt::Write` (no
//! intermediate `String`).

use std::fmt::Write as _;

/// Appends the decimal form of `u` to `out` without allocating.
pub(crate) fn write_u64(out: &mut String, u: u64) {
    // u64::MAX is 20 digits.
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    let mut n = u;
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    // buf[i..] is guaranteed ASCII digits.
    out.push_str(std::str::from_utf8(&buf[i..]).unwrap());
}

/// Appends the decimal form of `i` to `out` without allocating.
pub(crate) fn write_i64(out: &mut String, i: i64) {
    if i < 0 {
        out.push('-');
    }
    write_u64(out, i.unsigned_abs());
}

/// Appends `f` to `out` in plain form (`250`, `1.5`). Non-finite values use
/// the spellings Rust's own float parser reads back: `NaN`, `inf`, `-inf`.
/// Used by the XML encoder, whose scalars are text re-parsed leniently.
pub(crate) fn write_f64(out: &mut String, f: f64) {
    if f.is_nan() {
        out.push_str("NaN");
    } else {
        let _ = write!(out, "{f}");
    }
}

/// Appends a **finite** `f`, adding a trailing `.0` to integer-valued floats so
/// the value reads back as a float rather than an integer. Shared by the typed
/// formats (JSON, YAML, TOML), which each spell non-finite values themselves.
pub(crate) fn write_finite_f64(out: &mut String, f: f64) {
    let start = out.len();
    let _ = write!(out, "{f}");
    if !out[start..]
        .bytes()
        .any(|b| matches!(b, b'.' | b'e' | b'E'))
    {
        out.push_str(".0");
    }
}

/// Appends `f` in JSON form (see [`write_finite_f64`]). Non-finite values
/// become `null`, since JSON has no representation for them.
pub(crate) fn write_json_f64(out: &mut String, f: f64) {
    if f.is_finite() {
        write_finite_f64(out, f);
    } else {
        out.push_str("null");
    }
}

/// Appends a **finite** `f` as the shortest decimal that reads back as the
/// same `f32` (`0.1`, not the `0.10000000149011612` its `f64` widening
/// prints), with the same trailing `.0` rule as [`write_finite_f64`].
///
/// Rust's float `Display` already emits the shortest round-trip digits for
/// the value's own type, in plain (non-exponent) notation, which is the style
/// the `f64` writer uses.
///
/// The decoders read every number as an `f64` and narrow it with `as f32`, so
/// the text must also survive that double rounding. Each text is read back
/// that way before it is kept; when it does not give `f` again, the exact
/// `f64` form is written instead. On the current toolchains exactly one
/// finite magnitude takes that fallback (`7.038531e-26`, whose short text
/// parses to the `f64` exactly halfway between two `f32`s), but the check
/// does not rely on that.
pub(crate) fn write_finite_f32(out: &mut String, f: f32) {
    let start = out.len();
    let _ = write!(out, "{f}");
    let reads_back = out[start..]
        .parse::<f64>()
        .is_ok_and(|d| (d as f32).to_bits() == f.to_bits());
    if !reads_back {
        out.truncate(start);
        write_finite_f64(out, f64::from(f));
        return;
    }
    if !out[start..]
        .bytes()
        .any(|b| matches!(b, b'.' | b'e' | b'E'))
    {
        out.push_str(".0");
    }
}

/// Appends `f` in JSON form (see [`write_finite_f32`]); non-finite values
/// become `null`, as for `f64`.
pub(crate) fn write_json_f32(out: &mut String, f: f32) {
    if f.is_finite() {
        write_finite_f32(out, f);
    } else {
        out.push_str("null");
    }
}

/// A fixed stack buffer for formatting one float without allocating.
struct StackBuf {
    buf: [u8; 64],
    len: usize,
}

impl std::fmt::Write for StackBuf {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let end = self.len + s.len();
        if end > self.buf.len() {
            return Err(std::fmt::Error);
        }
        self.buf[self.len..end].copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// The `f64` nearest the shortest decimal form of `f` (`0.1f32` becomes
/// `0.1f64`), so a format writing the `f64` prints the short text. Converting
/// the result back with `as f32` always gives `f` again: when the short form
/// would not (see [`write_finite_f32`]), the exact widening is returned.
/// Non-finite values widen directly. Allocation-free: the text goes through a
/// stack buffer.
pub(crate) fn f32_to_f64_shortest(f: f32) -> f64 {
    if !f.is_finite() {
        return f64::from(f);
    }
    let mut buf = StackBuf {
        buf: [0; 64],
        len: 0,
    };
    // The longest plain `f32` text (a small subnormal with nine significant
    // digits, signed) is under 60 bytes, so this cannot fail; fall back to the
    // exact widening if it somehow does.
    if write!(buf, "{f}").is_err() {
        return f64::from(f);
    }
    std::str::from_utf8(&buf.buf[..buf.len])
        .ok()
        .and_then(|text| text.parse::<f64>().ok())
        .filter(|d| (*d as f32).to_bits() == f.to_bits())
        .unwrap_or_else(|| f64::from(f))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bits of the only finite `f32` magnitude (`7.038531e-26`) whose
    /// shortest text (`0.00000000000000000000000007038531`) does not read back
    /// through `f64`: that text parses to the `f64` lying exactly halfway between
    /// two `f32`s, and the narrowing tie goes to the neighbour. Found by checking
    /// all 2^32 bit patterns (`tests/codec_f32.rs`,
    /// `every_finite_f32_round_trips_exactly`). The writers find it by reading
    /// their text back, not by this constant.
    const SHORTEST_F32_MISREADS: u32 = 0x15AE_43FD;

    #[test]
    fn int_writers() {
        let mut s = String::new();
        write_u64(&mut s, 0);
        write_u64(&mut s, u64::MAX);
        assert_eq!(s, "018446744073709551615");

        let mut s = String::new();
        write_i64(&mut s, -12345);
        write_i64(&mut s, i64::MIN);
        assert_eq!(s, "-12345-9223372036854775808");
    }

    #[test]
    fn float_writers() {
        let mut s = String::new();
        write_json_f64(&mut s, 250.0);
        assert_eq!(s, "250.0");
        let mut s = String::new();
        write_json_f64(&mut s, -2.25);
        assert_eq!(s, "-2.25");
        let mut s = String::new();
        write_json_f64(&mut s, f64::NAN);
        assert_eq!(s, "null");
        let mut s = String::new();
        write_f64(&mut s, 250.0);
        assert_eq!(s, "250");

        // Non-finite plain forms are the ones `str::parse::<f64>` accepts.
        for f in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut s = String::new();
            write_f64(&mut s, f);
            let back: f64 = s.parse().unwrap();
            assert!(back == f || (back.is_nan() && f.is_nan()), "{s}");
        }
    }

    #[test]
    fn f32_writers_are_short() {
        let mut s = String::new();
        write_json_f32(&mut s, 0.1);
        assert_eq!(s, "0.1");
        let mut s = String::new();
        write_json_f32(&mut s, 250.0);
        assert_eq!(s, "250.0");
        let mut s = String::new();
        write_json_f32(&mut s, -0.0);
        assert_eq!(s, "-0.0");
        let mut s = String::new();
        write_json_f32(&mut s, f32::NAN);
        assert_eq!(s, "null");
        assert_eq!(f32_to_f64_shortest(0.1), 0.1f64);
        assert_eq!(f32_to_f64_shortest(-1.0e-7), -1.0e-7f64);
        assert!(f32_to_f64_shortest(f32::NAN).is_nan());
        assert_eq!(f32_to_f64_shortest(f32::NEG_INFINITY), f64::NEG_INFINITY);
        // The one value whose short text misreads through `f64` keeps its
        // exact form on both paths.
        for f in [
            f32::from_bits(SHORTEST_F32_MISREADS),
            -f32::from_bits(SHORTEST_F32_MISREADS),
        ] {
            assert_eq!(f32_to_f64_shortest(f), f64::from(f));
            let mut s = String::new();
            write_json_f32(&mut s, f);
            assert_eq!(s, format!("{}", f64::from(f)));
            assert_eq!(s.parse::<f64>().unwrap() as f32, f);
        }
        // The longest texts still fit the stack buffer.
        for f in [
            f32::MAX,
            f32::MIN,
            f32::MIN_POSITIVE,
            -f32::from_bits(1),
            -1.1754942e-38,
        ] {
            assert_eq!(f32_to_f64_shortest(f) as f32, f);
            assert_eq!(
                f32_to_f64_shortest(f),
                format!("{f}").parse::<f64>().unwrap()
            );
        }
    }
}

//! Allocation-free scalar writers shared by the JSON, XML, and YAML encoders.
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

/// Appends `f` to `out` (plain form). Non-finite values become `null`.
/// Used by the XML and YAML encoders, whose scalars are re-parsed leniently.
pub(crate) fn write_f64(out: &mut String, f: f64) {
    if f.is_finite() {
        let _ = write!(out, "{f}");
    } else {
        out.push_str("null");
    }
}

/// Appends `f` in JSON form: a trailing `.0` is added to integer-valued floats
/// so the value round-trips as a float rather than an integer. Non-finite
/// values become `null`.
pub(crate) fn write_json_f64(out: &mut String, f: f64) {
    if !f.is_finite() {
        out.push_str("null");
        return;
    }
    let start = out.len();
    let _ = write!(out, "{f}");
    if !out[start..]
        .bytes()
        .any(|b| matches!(b, b'.' | b'e' | b'E' | b'n' | b'i'))
    {
        out.push_str(".0");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}

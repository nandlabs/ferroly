//! Human-readable byte sizes: formatting (`1.5 MiB`, `2.0 GB`) and the
//! [`ByteSize`] type, which parses sizes such as `512MiB` or `2G` from flags
//! and configuration.
//!
//! Two unit systems are supported:
//!
//! | System | Units | Base | Function |
//! |---|---|---|---|
//! | binary (IEC) | `B`, `KiB`, `MiB`, `GiB`, `TiB`, `PiB`, `EiB` | 1024 | [`format_bytes`] |
//! | decimal (SI) | `B`, `kB`, `MB`, `GB`, `TB`, `PB`, `EB` | 1000 | [`format_bytes_si`] |
//!
//! Bytes are always shown as an integer (`1023 B`); larger units with one
//! decimal by default ([`Precision`] changes that), rounded half to even. A
//! value that would round up to `1024.0` of a unit (`1000.0` in SI) is shown
//! as `1.0` of the next unit instead.
//!
//! ```
//! use ferroly::fsutils::size::{format_bytes, format_bytes_si, ByteSize};
//!
//! assert_eq!(format_bytes(0), "0 B");
//! assert_eq!(format_bytes(1536), "1.5 KiB");
//! assert_eq!(format_bytes(2_040_109_465), "1.9 GiB");
//! assert_eq!(format_bytes(u64::MAX), "16.0 EiB");
//! assert_eq!(format_bytes_si(1_500_000), "1.5 MB");
//!
//! let budget: ByteSize = "512MiB".parse().unwrap();
//! assert_eq!(budget.as_u64(), 512 * 1024 * 1024);
//! assert_eq!(budget.to_string(), "512.0 MiB");
//! assert_eq!(format!("{budget:#}"), "536.9 MB");
//! ```
//!
//! With the `codec` feature, [`ByteSize`] implements `Encode`, `Decode` and
//! `JsonSchema`, so it can be a configuration field written either as a
//! number of bytes (`max_size: 1048576`) or as a string (`max_size: 2G`).

use std::fmt::{self, Write as _};
use std::str::FromStr;

/// The binary (IEC) unit names, one per power of 1024.
const BINARY_UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
/// The decimal (SI) unit names, one per power of 1000.
const SI_UNITS: [&str; 7] = ["B", "kB", "MB", "GB", "TB", "PB", "EB"];

/// How many decimals a formatted size shows (bytes are always an integer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Precision {
    /// Exactly this many digits after the decimal point; `Decimals(0)` prints
    /// no decimal point at all (`2 KiB`).
    Decimals(u8),
}

impl Default for Precision {
    /// One decimal: `1.5 MiB`.
    fn default() -> Self {
        Precision::Decimals(1)
    }
}

impl Precision {
    fn decimals(self) -> usize {
        match self {
            Precision::Decimals(d) => usize::from(d),
        }
    }
}

/// Formats `n` bytes with binary units (`KiB`, `MiB`, … powers of 1024) and
/// one decimal: `0 B`, `1023 B`, `1.0 KiB`, `1.5 MiB`, `16.0 EiB`.
pub fn format_bytes(n: u64) -> String {
    format_bytes_with(n, Precision::default())
}

/// Formats `n` bytes with decimal units (`kB`, `MB`, … powers of 1000), the
/// way disk vendors report storage, and one decimal: `999 B`, `1.0 kB`,
/// `1.5 MB`, `18.4 EB`.
pub fn format_bytes_si(n: u64) -> String {
    format_bytes_si_with(n, Precision::default())
}

/// [`format_bytes`] with a chosen [`Precision`]:
/// `format_bytes_with(1_234_567, Precision::Decimals(2))` is `"1.18 MiB"`.
pub fn format_bytes_with(n: u64, precision: Precision) -> String {
    format_in(n, 1024, &BINARY_UNITS, precision.decimals())
}

/// [`format_bytes_si`] with a chosen [`Precision`]:
/// `format_bytes_si_with(1_234_567, Precision::Decimals(2))` is `"1.23 MB"`.
pub fn format_bytes_si_with(n: u64, precision: Precision) -> String {
    format_in(n, 1000, &SI_UNITS, precision.decimals())
}

/// Picks the largest unit not exceeding `n` (moving one unit up when rounding
/// reaches `base` of the chosen one) and formats `n` in it.
fn format_in(n: u64, base: u64, units: &[&str; 7], decimals: usize) -> String {
    if n < base {
        return format!("{n} B");
    }
    let mut exp = 0;
    let mut scale = 1u64;
    while exp + 1 < units.len() && n / scale >= base {
        scale *= base;
        exp += 1;
    }
    loop {
        let (whole, frac) = divide_rounded(n, scale, decimals);
        if whole >= u128::from(base) && exp + 1 < units.len() {
            // `1023.95 KiB` rounds to `1024.0 KiB`: show `1.0 MiB` instead.
            scale *= base;
            exp += 1;
            continue;
        }
        let mut out = whole.to_string();
        if decimals > 0 {
            out.push('.');
            out.push_str(&frac);
        }
        out.push(' ');
        out.push_str(units[exp]);
        return out;
    }
}

/// `n / scale` to exactly `decimals` digits, rounded half to even, as the
/// integer part and the decimal digits. Exact for any number of decimals:
/// the digits come from long division, never from floating point.
fn divide_rounded(n: u64, scale: u64, decimals: usize) -> (u128, String) {
    let scale = u128::from(scale);
    let mut whole = u128::from(n) / scale;
    let mut rem = u128::from(n) % scale;
    let mut digits: Vec<u8> = Vec::with_capacity(decimals);
    for _ in 0..decimals {
        rem *= 10;
        digits.push((rem / scale) as u8);
        rem %= scale;
    }
    // Round on what is left: above half rounds up, exactly half rounds to
    // the even last digit.
    let last_odd = match digits.last() {
        Some(d) => d % 2 == 1,
        None => whole % 2 == 1,
    };
    if rem * 2 > scale || (rem * 2 == scale && last_odd) {
        let mut carry = true;
        for d in digits.iter_mut().rev() {
            if *d == 9 {
                *d = 0;
            } else {
                *d += 1;
                carry = false;
                break;
            }
        }
        if carry {
            whole += 1;
        }
    }
    let frac = digits.iter().map(|d| char::from(b'0' + d)).collect();
    (whole, frac)
}

// ---- ByteSize ----------------------------------------------------------------

/// A size in bytes that parses from and formats to human-readable text.
///
/// `Display` uses binary units ([`format_bytes`]); the alternate form `{:#}`
/// uses decimal units ([`format_bytes_si`]), a precision (`{:.2}`) sets the
/// number of decimals, and a width pads the text with the fill and alignment
/// given (left-aligned by default, like a string: `{:>10}` right-aligns a
/// size column).
///
/// `FromStr` accepts a number with an optional unit, case-insensitively and
/// with optional whitespace between them:
///
/// | Input | Meaning |
/// |---|---|
/// | `1024`, `512B` | bytes |
/// | `64KiB`, `1.5 MiB`, `1gib` | binary units (`KiB` … `EiB`, powers of 1024) |
/// | `2G`, `512k` | a bare `K`/`M`/`G`/`T` is binary too (the common CLI convention) |
/// | `500MB`, `2 GB`, `1kB` | decimal units (`kB` … `EB`, powers of 1000) |
///
/// Petabytes and exabytes must be spelled out (`PiB`/`PB`, `EiB`/`EB`): a
/// bare `P` or `E` is an unknown unit, so a stray exponent (`1e`, `2E`) is an
/// error rather than an exbibyte. Exponent notation (`1e6`) is not accepted.
///
/// With a unit of `K` or larger, fractions are allowed and rounded down to
/// whole bytes (`1.5K` is 1536). A plain byte count, with no unit or `B`,
/// must be whole: `1.5` and `0.3B` are rejected, `1.0` is 1. Negative values,
/// unknown units, malformed numbers and values above `u64::MAX` are rejected
/// with a [`ParseByteSizeError`] naming the input.
///
/// ```
/// use ferroly::fsutils::size::ByteSize;
///
/// assert_eq!("2G".parse::<ByteSize>().unwrap(), ByteSize::gib(2));
/// assert_eq!("500 MB".parse::<ByteSize>().unwrap().as_u64(), 500_000_000);
/// assert_eq!(format!("{:.2}", ByteSize(1_234_567)), "1.18 MiB");
/// assert_eq!(format!("[{:>9}]", ByteSize(1536)), "[  1.5 KiB]");
/// assert!("-1K".parse::<ByteSize>().is_err());
/// assert!("1.5".parse::<ByteSize>().is_err());
/// ```
///
/// With the `codec` feature it encodes as an integer number of bytes and
/// decodes from either an integer or a string in the syntax above (a float
/// only when it is a whole number of bytes); its `JsonSchema` admits a
/// non-negative integer or a size string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ByteSize(pub u64);

impl ByteSize {
    /// `n` bytes.
    pub const fn b(n: u64) -> ByteSize {
        ByteSize(n)
    }

    /// `n` KiB (`n × 1024` bytes), saturating at `u64::MAX`.
    pub const fn kib(n: u64) -> ByteSize {
        ByteSize(n.saturating_mul(1 << 10))
    }

    /// `n` MiB (`n × 1024²` bytes), saturating at `u64::MAX`.
    pub const fn mib(n: u64) -> ByteSize {
        ByteSize(n.saturating_mul(1 << 20))
    }

    /// `n` GiB (`n × 1024³` bytes), saturating at `u64::MAX`.
    pub const fn gib(n: u64) -> ByteSize {
        ByteSize(n.saturating_mul(1 << 30))
    }

    /// `n` TiB (`n × 1024⁴` bytes), saturating at `u64::MAX`.
    pub const fn tib(n: u64) -> ByteSize {
        ByteSize(n.saturating_mul(1 << 40))
    }

    /// The number of bytes.
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl From<u64> for ByteSize {
    fn from(n: u64) -> Self {
        ByteSize(n)
    }
}

impl From<ByteSize> for u64 {
    fn from(size: ByteSize) -> Self {
        size.0
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let precision = match f.precision() {
            Some(d) => Precision::Decimals(u8::try_from(d).unwrap_or(u8::MAX)),
            None => Precision::default(),
        };
        let text = if f.alternate() {
            format_bytes_si_with(self.0, precision)
        } else {
            format_bytes_with(self.0, precision)
        };
        // Pad by hand: the precision has already been used for the decimals,
        // so `Formatter::pad` would truncate the text to it.
        let len = text.chars().count();
        let pad = match f.width() {
            Some(width) if width > len => width - len,
            _ => return f.write_str(&text),
        };
        let (before, after) = match f.align() {
            Some(fmt::Alignment::Right) => (pad, 0),
            Some(fmt::Alignment::Center) => (pad / 2, pad - pad / 2),
            Some(fmt::Alignment::Left) | None => (0, pad),
        };
        let fill = f.fill();
        for _ in 0..before {
            f.write_char(fill)?;
        }
        f.write_str(&text)?;
        for _ in 0..after {
            f.write_char(fill)?;
        }
        Ok(())
    }
}

/// Why a [`ByteSize`] could not be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ByteSizeErrorKind {
    /// The input is empty (or only whitespace).
    Empty,
    /// The input starts with a minus sign.
    Negative,
    /// The number part is missing or malformed (`K`, `1.2.3`, `1 2`), or a
    /// plain byte count is not whole (`1.5`, `0.3B`).
    InvalidNumber,
    /// The unit is not one of the accepted units (`12XB`, a bare `E`, or
    /// exponent notation such as `1e6`).
    UnknownUnit,
    /// The size is larger than `u64::MAX` bytes.
    Overflow,
}

/// The error returned when a string is not a valid [`ByteSize`]. It names the
/// offending input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseByteSizeError {
    input: String,
    kind: ByteSizeErrorKind,
}

impl ParseByteSizeError {
    fn new(input: &str, kind: ByteSizeErrorKind) -> Self {
        ParseByteSizeError {
            input: input.to_string(),
            kind,
        }
    }

    /// What was wrong with the input.
    pub fn kind(&self) -> ByteSizeErrorKind {
        self.kind
    }

    /// The input that failed to parse.
    pub fn input(&self) -> &str {
        &self.input
    }
}

impl fmt::Display for ParseByteSizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let why = match self.kind {
            ByteSizeErrorKind::Empty => "it is empty",
            ByteSizeErrorKind::Negative => "sizes cannot be negative",
            ByteSizeErrorKind::InvalidNumber => {
                "expected a whole number of bytes or a number with a unit, such as 512, 1.5K or 2GiB"
            }
            ByteSizeErrorKind::UnknownUnit => {
                "unknown unit (expected B, K/M/G/T, KiB…EiB or kB…EB)"
            }
            ByteSizeErrorKind::Overflow => "larger than u64::MAX bytes",
        };
        write!(f, "invalid byte size {:?}: {why}", self.input)
    }
}

impl std::error::Error for ParseByteSizeError {}

/// The multiplier for a unit suffix (case-insensitive), or `None` for an
/// unknown one.
fn unit_multiplier(unit: &str) -> Option<u64> {
    let unit = unit.to_ascii_lowercase();
    let (prefix, rest) = match unit.char_indices().nth(1) {
        Some((i, _)) => unit.split_at(i),
        None => (unit.as_str(), ""),
    };
    if unit.is_empty() || unit == "b" {
        return Some(1);
    }
    let exp = match prefix {
        "k" => 1,
        "m" => 2,
        "g" => 3,
        "t" => 4,
        "p" => 5,
        "e" => 6,
        _ => return None,
    };
    match rest {
        // A bare prefix (`2G`) and the IEC form (`2GiB`) are binary. Bare
        // `P` and `E` are not accepted: `1e` / `2E` is far more likely a
        // broken exponent than a request for exbibytes.
        "" if exp <= 4 => Some(1u64 << (10 * exp)),
        "ib" => Some(1u64 << (10 * exp)),
        // `2GB` is decimal.
        "b" => Some(1000u64.pow(exp)),
        _ => None,
    }
}

impl FromStr for ByteSize {
    type Err = ParseByteSizeError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let err = |kind| ParseByteSizeError::new(input, kind);
        let s = input.trim();
        if s.is_empty() {
            return Err(err(ByteSizeErrorKind::Empty));
        }
        if s.starts_with('-') {
            return Err(err(ByteSizeErrorKind::Negative));
        }
        let number_end = s
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(s.len());
        let (number, unit) = s.split_at(number_end);
        let (int_part, frac_part) = match number.split_once('.') {
            Some((i, f)) => (i, f),
            None => (number, ""),
        };
        if (int_part.is_empty() && frac_part.is_empty()) || frac_part.contains('.') {
            return Err(err(ByteSizeErrorKind::InvalidNumber));
        }
        let unit = unit.trim_start();
        let multiplier = match unit_multiplier(unit) {
            Some(m) => m,
            // A unit that starts with a number character (`1.5 2K`) or a
            // sign / exponent is a malformed number rather than a unit.
            None if unit.starts_with(|c: char| c.is_ascii_digit() || c == '.' || c == '+') => {
                return Err(err(ByteSizeErrorKind::InvalidNumber))
            }
            None => return Err(err(ByteSizeErrorKind::UnknownUnit)),
        };
        // A plain byte count must be whole: `1.5` (or `0.3B`) is not a
        // number of bytes, and a decoded float `1.5` is rejected the same way.
        if multiplier == 1 && frac_part.bytes().any(|b| b != b'0') {
            return Err(err(ByteSizeErrorKind::InvalidNumber));
        }
        let multiplier = u128::from(multiplier);

        // The whole part, exactly; anything past u64::MAX is an overflow.
        let mut whole: u128 = 0;
        for b in int_part.bytes() {
            whole = whole
                .checked_mul(10)
                .and_then(|w| w.checked_add(u128::from(b - b'0')))
                .ok_or_else(|| err(ByteSizeErrorKind::Overflow))?;
            if whole > u128::from(u64::MAX) {
                return Err(err(ByteSizeErrorKind::Overflow));
            }
        }
        let whole = whole
            .checked_mul(multiplier)
            .ok_or_else(|| err(ByteSizeErrorKind::Overflow))?;

        // floor(0.d1 d2 … dn × multiplier), exactly: folding the digits from
        // the last one, floor((d·m + floor(x)) / 10) = floor((d·m + x) / 10).
        let mut frac: u128 = 0;
        for b in frac_part.bytes().rev() {
            frac = (u128::from(b - b'0') * multiplier + frac) / 10;
        }

        let total = whole + frac;
        u64::try_from(total)
            .map(ByteSize)
            .map_err(|_| err(ByteSizeErrorKind::Overflow))
    }
}

#[cfg(feature = "codec")]
mod codec_impls {
    use super::ByteSize;
    use ferroly::codec::schema::JsonSchema;
    use ferroly::codec::{CodecError, Decode, Decoder, Encode, Encoder, Value};

    const EXPECTED: &str = "byte size (a number of bytes, or a string such as \"512MiB\")";

    /// The shape of a size string `FromStr` accepts, for the schema: a whole
    /// byte count with an optional `B`, or any number with a unit of `K` or
    /// larger, with optional whitespace (`\s`, plus U+0085, which `str::trim`
    /// also strips). It cannot express the `u64::MAX` limit.
    const PATTERN: &str = concat!(
        r"^[\s\u0085]*(?:",
        r"(?:\d+\.?0*|\.0+)[\s\u0085]*[Bb]?",
        r"|(?:\d+\.?\d*|\.\d+)[\s\u0085]*(?:[KkMmGgTt](?:[Ii]?[Bb])?|[PpEe][Ii]?[Bb])",
        r")[\s\u0085]*$",
    );

    /// A size as written in a configuration file: an integer number of bytes
    /// or a string such as `"2G"`. A float is accepted only when it is a
    /// whole number of bytes that fits.
    fn from_value(value: &Value) -> Result<ByteSize, CodecError> {
        match value {
            Value::UInt(n) => Ok(ByteSize(*n)),
            Value::Int(n) => u64::try_from(*n)
                .map(ByteSize)
                .map_err(|_| CodecError::out_of_range("ByteSize")),
            Value::Float(f) => {
                if f.is_finite() && f.fract() != 0.0 {
                    Err(CodecError::parse(format!(
                        "invalid byte size {f}: not a whole number of bytes"
                    )))
                } else if *f >= 0.0 && *f < 18_446_744_073_709_551_616.0 {
                    Ok(ByteSize(*f as u64))
                } else {
                    Err(CodecError::out_of_range("ByteSize"))
                }
            }
            Value::Str(s) => s
                .parse::<ByteSize>()
                .map_err(|e| CodecError::parse(e.to_string())),
            _ => Err(CodecError::expected(EXPECTED)),
        }
    }

    /// Encodes as an integer number of bytes, which every format carries
    /// exactly and which decodes back to the same size.
    impl Encode for ByteSize {
        fn encode(&self) -> Value {
            Value::UInt(self.0)
        }
        fn encode_to<E: Encoder>(&self, e: &mut E) {
            e.encode_u64(self.0);
        }
    }

    /// Decodes from an integer number of bytes or a size string, in both the
    /// lenient and the strict mode (a size string is not a coerced scalar).
    impl Decode for ByteSize {
        fn decode(value: &Value) -> Result<Self, CodecError> {
            from_value(value)
        }
        fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
            from_value(&d.decode_value()?)
        }
    }

    /// A non-negative integer number of bytes, or a size string (`"2G"`,
    /// `"512 MiB"`). `minimum` applies only to the number and `pattern` only
    /// to the string, so this crate's validator and full JSON-Schema tools
    /// read it the same way. The `u64::MAX` limit is not described.
    impl JsonSchema for ByteSize {
        fn json_schema() -> Value {
            Value::Object(vec![
                (
                    "type".to_string(),
                    Value::Array(vec![
                        Value::Str("integer".to_string()),
                        Value::Str("string".to_string()),
                    ]),
                ),
                ("minimum".to_string(), Value::Int(0)),
                ("pattern".to_string(), Value::Str(PATTERN.to_string())),
            ])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KI: u64 = 1 << 10;
    const MI: u64 = 1 << 20;
    const GI: u64 = 1 << 30;
    const TI: u64 = 1 << 40;
    const PI: u64 = 1 << 50;
    const EI: u64 = 1 << 60;

    #[test]
    fn binary_table() {
        let table: &[(u64, &str)] = &[
            (0, "0 B"),
            (1, "1 B"),
            (1023, "1023 B"),
            (1024, "1.0 KiB"),
            (1025, "1.0 KiB"),
            (1536, "1.5 KiB"),
            // 1.25 KiB and 1.75 KiB are exact halves: round to even.
            (1280, "1.2 KiB"),
            (1792, "1.8 KiB"),
            (10 * KI, "10.0 KiB"),
            // 1023.94 KiB stays, 1023.95 KiB rounds up into the next unit.
            (1_048_514, "1023.9 KiB"),
            (1_048_525, "1.0 MiB"),
            (MI - 1, "1.0 MiB"),
            (MI, "1.0 MiB"),
            (MI + MI / 2, "1.5 MiB"),
            (GI - 1, "1.0 GiB"),
            (GI, "1.0 GiB"),
            (2_040_109_465, "1.9 GiB"),
            (16 * GI, "16.0 GiB"),
            (TI - 1, "1.0 TiB"),
            (TI, "1.0 TiB"),
            (PI - 1, "1.0 PiB"),
            (PI, "1.0 PiB"),
            (EI - 1, "1.0 EiB"),
            (EI, "1.0 EiB"),
            (u64::MAX, "16.0 EiB"),
        ];
        for &(n, want) in table {
            assert_eq!(format_bytes(n), want, "format_bytes({n})");
        }
        // The last value below each rounding edge stays in the smaller unit.
        for (exp, name) in BINARY_UNITS.iter().enumerate().take(6).skip(1) {
            let unit = 1u64 << (10 * exp);
            // 1023.9 units, truncated to whole bytes: shown in this unit.
            let below = unit * 10239 / 10;
            assert_eq!(format_bytes(below), format!("1023.9 {name}"), "{below}");
            // 1023.95 units and up: shown as 1.0 of the next unit.
            let edge = (u128::from(unit) * 102_395).div_ceil(100) as u64;
            assert_eq!(format_bytes(edge), format!("1.0 {}", BINARY_UNITS[exp + 1]));
        }
    }

    #[test]
    fn si_table() {
        let table: &[(u64, &str)] = &[
            (0, "0 B"),
            (999, "999 B"),
            (1000, "1.0 kB"),
            (1023, "1.0 kB"),
            (1024, "1.0 kB"),
            (1500, "1.5 kB"),
            // 1.25 kB / 1.35 kB are exact halves: round to even.
            (1250, "1.2 kB"),
            (1350, "1.4 kB"),
            (999_949, "999.9 kB"),
            // 999.95 kB: an exact half with an odd last digit rounds up to
            // 1000.0 kB, shown as 1.0 MB.
            (999_950, "1.0 MB"),
            (999_999, "1.0 MB"),
            (1_000_000, "1.0 MB"),
            (1_500_000, "1.5 MB"),
            (999_999_999, "1.0 GB"),
            (1_000_000_000, "1.0 GB"),
            (999_999_999_999, "1.0 TB"),
            (1_000_000_000_000, "1.0 TB"),
            (999_999_999_999_999, "1.0 PB"),
            (1_000_000_000_000_000, "1.0 PB"),
            (999_999_999_999_999_999, "1.0 EB"),
            (1_000_000_000_000_000_000, "1.0 EB"),
            (u64::MAX, "18.4 EB"),
        ];
        for &(n, want) in table {
            assert_eq!(format_bytes_si(n), want, "format_bytes_si({n})");
        }
    }

    #[test]
    fn precision() {
        let two = Precision::Decimals(2);
        assert_eq!(format_bytes_with(1_234_567, two), "1.18 MiB");
        assert_eq!(format_bytes_si_with(1_234_567, two), "1.23 MB");
        assert_eq!(format_bytes_with(1023, two), "1023 B");
        assert_eq!(format_bytes_with(u64::MAX, two), "16.00 EiB");
        // 1023.995 KiB and up round to 1024.00 KiB → 1.00 MiB.
        assert_eq!(format_bytes_with(1_048_570, two), "1023.99 KiB");
        assert_eq!(format_bytes_with(1_048_571, two), "1.00 MiB");
        assert_eq!(format_bytes_with(1536, Precision::Decimals(0)), "2 KiB");
        assert_eq!(
            format_bytes_with(1024 + 512, Precision::Decimals(0)),
            "2 KiB"
        );
        assert_eq!(format_bytes_with(2560, Precision::Decimals(0)), "2 KiB"); // 2.5 → even
        assert_eq!(
            format_bytes_with(1023 * KI + 1023, Precision::Decimals(0)),
            "1 MiB"
        );
        assert_eq!(
            format_bytes_with(1 + KI, Precision::Decimals(10)),
            "1.0009765625 KiB"
        );
        assert_eq!(Precision::default(), Precision::Decimals(1));
    }

    #[test]
    fn display_forms() {
        let size = ByteSize::mib(512);
        assert_eq!(size.to_string(), "512.0 MiB");
        assert_eq!(format!("{size:#}"), "536.9 MB");
        assert_eq!(format!("{size:.0}"), "512 MiB");
        assert_eq!(format!("{:#.3}", ByteSize(1_234_567)), "1.235 MB");
    }

    #[test]
    fn display_honours_width_fill_and_alignment() {
        let size = ByteSize(1536);
        assert_eq!(format!("[{size:>10}]"), "[   1.5 KiB]");
        assert_eq!(format!("[{size:<10}]"), "[1.5 KiB   ]");
        assert_eq!(format!("[{size:10}]"), "[1.5 KiB   ]"); // left, like a string
        assert_eq!(format!("[{size:*^12.2}]"), "[**1.50 KiB**]");
        assert_eq!(format!("[{size:-^12}]"), "[--1.5 KiB---]");
        assert_eq!(format!("[{size:>#10}]"), "[    1.5 kB]");
        assert_eq!(format!("[{:>8}]", ByteSize(7)), "[     7 B]");
        // Never truncated: a width below the text's length changes nothing.
        assert_eq!(format!("[{size:>3}]"), "[1.5 KiB]");
        // Counted in characters, not bytes.
        assert_eq!(format!("[{size:é>9}]"), "[éé1.5 KiB]");
        // A right-aligned column lines up.
        let column: Vec<String> = [0, 1023, 1536, u64::MAX]
            .iter()
            .map(|&n| format!("{:>9}", ByteSize(n)))
            .collect();
        assert_eq!(column, ["      0 B", "   1023 B", "  1.5 KiB", " 16.0 EiB"]);
    }

    fn parse(s: &str) -> u64 {
        s.parse::<ByteSize>()
            .unwrap_or_else(|e| panic!("{s:?}: {e}"))
            .as_u64()
    }

    #[test]
    fn parses_every_unit_form() {
        assert_eq!(parse("0"), 0);
        assert_eq!(parse("1024"), 1024);
        assert_eq!(parse("512B"), 512);
        assert_eq!(parse("512 b"), 512);
        assert_eq!(parse("64KiB"), 64 * KI);
        assert_eq!(parse("1.5 MiB"), MI + MI / 2);
        assert_eq!(parse("1gib"), GI);
        assert_eq!(parse("2G"), 2 * GI);
        assert_eq!(parse("2g"), 2 * GI);
        assert_eq!(parse("512k"), 512 * KI);
        assert_eq!(parse("3T"), 3 * TI);
        assert_eq!(parse("1PiB"), PI);
        assert_eq!(parse("15 eib"), 15 * EI);
        assert_eq!(parse("2PB"), 2_000_000_000_000_000);
        assert_eq!(parse("500MB"), 500_000_000);
        assert_eq!(parse("2 GB"), 2_000_000_000);
        assert_eq!(parse("1kB"), 1000);
        assert_eq!(parse("1KB"), 1000);
        assert_eq!(parse("  7 TB  "), 7_000_000_000_000);
        assert_eq!(parse("1EB"), 1_000_000_000_000_000_000);
        // With a unit of K or larger, fractions round down to whole bytes.
        assert_eq!(parse("1.5K"), 1536);
        assert_eq!(parse(".5KiB"), 512);
        // A plain byte count may have a decimal point, but only zeros after.
        assert_eq!(parse("1."), 1);
        assert_eq!(parse("1.0"), 1);
        assert_eq!(parse("2.000 B"), 2);
        assert_eq!(parse(".0"), 0);
        assert_eq!(parse("0.001KiB"), 1); // 1.024 bytes
        assert_eq!(parse("0.0009KiB"), 0); // 0.9216 bytes
                                           // A long fraction is still exact: 1/3 GiB = 357913941.33… bytes.
        assert_eq!(
            parse("0.333333333333333333333333333333333333333333G"),
            GI / 3
        );
    }

    #[test]
    fn round_trips_whole_unit_values() {
        let sizes = [
            ByteSize(0),
            ByteSize(1),
            ByteSize(1023),
            ByteSize::kib(1),
            ByteSize::kib(64),
            ByteSize::mib(512),
            ByteSize::mib(1023),
            ByteSize::gib(16),
            ByteSize::tib(3),
            ByteSize(PI * 5),
            ByteSize(EI * 15),
            ByteSize(MI + MI / 2), // 1.5 MiB is exact at one decimal
        ];
        for size in sizes {
            let text = size.to_string();
            assert_eq!(text.parse::<ByteSize>().unwrap(), size, "{text}");
        }
        for n in [
            0u64,
            1,
            999,
            1000,
            5_000_000,
            12_000_000_000,
            7 * 10u64.pow(18),
        ] {
            let size = ByteSize(n);
            let text = format!("{size:#}");
            assert_eq!(text.parse::<ByteSize>().unwrap(), size, "{text}");
        }
    }

    fn kind(s: &str) -> ByteSizeErrorKind {
        let err = s.parse::<ByteSize>().unwrap_err();
        assert_eq!(err.input(), s);
        assert!(err.to_string().contains(&format!("{s:?}")), "{err}");
        err.kind()
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(kind("-1K"), ByteSizeErrorKind::Negative);
        assert_eq!(kind("-0"), ByteSizeErrorKind::Negative);
        assert_eq!(kind("12XB"), ByteSizeErrorKind::UnknownUnit);
        assert_eq!(kind("12 KIBS"), ByteSizeErrorKind::UnknownUnit);
        assert_eq!(kind("1Kb/s"), ByteSizeErrorKind::UnknownUnit);
        assert_eq!(kind("1 bytes"), ByteSizeErrorKind::UnknownUnit);
        assert_eq!(kind(""), ByteSizeErrorKind::Empty);
        assert_eq!(kind("   "), ByteSizeErrorKind::Empty);
        assert_eq!(kind("K"), ByteSizeErrorKind::InvalidNumber);
        assert_eq!(kind("."), ByteSizeErrorKind::InvalidNumber);
        assert_eq!(kind("1.2.3"), ByteSizeErrorKind::InvalidNumber);
        assert_eq!(kind("1 2"), ByteSizeErrorKind::InvalidNumber);
        assert_eq!(kind("+1"), ByteSizeErrorKind::InvalidNumber);
        assert_eq!(kind("1e6"), ByteSizeErrorKind::UnknownUnit);
        // A fractional number of bytes is not a byte count.
        assert_eq!(kind("1.5"), ByteSizeErrorKind::InvalidNumber);
        assert_eq!(kind("0.3B"), ByteSizeErrorKind::InvalidNumber);
        assert_eq!(kind("1.9999"), ByteSizeErrorKind::InvalidNumber);
        assert_eq!(kind("0.01 b"), ByteSizeErrorKind::InvalidNumber);
        // Bare `P` and `E` are not units: a broken exponent never becomes
        // petabytes or exbibytes.
        for bad in ["1E", "1e", "1 e", "2E", "1P", "3 p", "1.5e"] {
            assert_eq!(kind(bad), ByteSizeErrorKind::UnknownUnit, "{bad}");
        }
        // Values over u64::MAX, in each way they can arise.
        assert_eq!(parse("18446744073709551615"), u64::MAX);
        assert_eq!(kind("18446744073709551616"), ByteSizeErrorKind::Overflow);
        assert_eq!(
            kind("99999999999999999999999999999999999999999999"),
            ByteSizeErrorKind::Overflow
        );
        assert_eq!(kind("16EiB"), ByteSizeErrorKind::Overflow);
        assert_eq!(kind("17179869184G"), ByteSizeErrorKind::Overflow);
        assert_eq!(kind("18.5EB"), ByteSizeErrorKind::Overflow);
        assert_eq!(
            kind("16.0000000000000000001EiB"),
            ByteSizeErrorKind::Overflow
        );
        // Just below 16 EiB: the fraction is exact, never rounded up.
        assert_eq!(parse("15.9999999999999999999EiB"), u64::MAX);
        assert_eq!(parse("15.99999999999999999EiB"), 18_446_744_073_709_551_604);
    }

    #[cfg(feature = "codec")]
    mod codec {
        use super::super::ByteSize;
        use ferroly::codec::schema::{JsonSchema, Schema};
        use ferroly::codec::{json, toml, yaml, Decode, Encode, Value};

        #[derive(
            Debug,
            PartialEq,
            ferroly::codec::Decode,
            ferroly::codec::Encode,
            ferroly::codec::schema::JsonSchema,
        )]
        struct Cache {
            max_size: ByteSize,
        }

        fn cache(n: u64) -> Cache {
            Cache {
                max_size: ByteSize(n),
            }
        }

        #[test]
        fn decodes_from_integer_and_string() {
            // YAML and the lenient `Value` path.
            assert_eq!(
                yaml::decode::<Cache>("max_size: 2G").unwrap(),
                cache(2 << 30)
            );
            assert_eq!(
                yaml::decode::<Cache>("max_size: 4096").unwrap(),
                cache(4096)
            );
            assert_eq!(
                yaml::decode::<Cache>("max_size: 1.5 MiB").unwrap(),
                cache(3 << 19)
            );
            // JSON and TOML decode strictly; a size string is still accepted.
            assert_eq!(
                json::decode::<Cache>(r#"{"max_size":"500MB"}"#).unwrap(),
                cache(500_000_000)
            );
            assert_eq!(
                json::decode::<Cache>(r#"{"max_size":18446744073709551615}"#).unwrap(),
                cache(u64::MAX)
            );
            assert_eq!(
                toml::decode::<Cache>("max_size = \"64KiB\"").unwrap(),
                cache(64 << 10)
            );
            assert_eq!(toml::decode::<Cache>("max_size = 10").unwrap(), cache(10));
            assert_eq!(
                ByteSize::decode(&Value::Float(2048.0)).unwrap(),
                ByteSize(2048)
            );
        }

        #[test]
        fn rejects_bad_config_values() {
            for bad in [
                r#"{"max_size":-1}"#,
                r#"{"max_size":"-1K"}"#,
                r#"{"max_size":"12XB"}"#,
                r#"{"max_size":"16EiB"}"#,
                r#"{"max_size":1.5}"#,
                r#"{"max_size":true}"#,
                r#"{"max_size":[1]}"#,
            ] {
                assert!(json::decode::<Cache>(bad).is_err(), "{bad}");
            }
            let err = json::decode::<Cache>(r#"{"max_size":"12XB"}"#).unwrap_err();
            assert!(err.to_string().contains("12XB"), "{err}");
        }

        /// The string `"1.5"` and the number `1.5` are refused alike, and
        /// the number with a message that says why.
        #[test]
        fn fractional_byte_counts_are_rejected_from_strings_and_numbers() {
            for bad in ["max_size: 1.5", "max_size: \"1.5\"", "max_size: 0.3B"] {
                assert!(yaml::decode::<Cache>(bad).is_err(), "{bad}");
            }
            let err = ByteSize::decode(&Value::Float(1.5)).unwrap_err();
            assert!(
                err.to_string().contains("not a whole number of bytes"),
                "{err}"
            );
            let err = ByteSize::decode(&Value::Str("1.5".into())).unwrap_err();
            assert!(err.to_string().contains("whole number of bytes"), "{err}");
            // Out of range is still out of range.
            for f in [-1.0, 1e20, f64::INFINITY, f64::NAN] {
                assert!(ByteSize::decode(&Value::Float(f)).is_err(), "{f}");
            }
            assert_eq!(
                yaml::decode::<Cache>("max_size: 1.5K").unwrap(),
                cache(1536)
            );
            assert_eq!(yaml::decode::<Cache>("max_size: 2.0").unwrap(), cache(2));
        }

        #[test]
        fn json_schema_admits_integers_and_size_strings() {
            assert_eq!(
                json::to_string(&Cache::json_schema()),
                json::to_string(&json::from_str(&format!(
                    r#"{{"type":"object","properties":{{"max_size":{}}},"required":["max_size"]}}"#,
                    json::to_string(&ByteSize::json_schema())
                ))
                .unwrap())
            );
            let schema = Schema::of::<Cache>().unwrap();
            let check = |doc: &str| schema.validate(&json::from_str(doc).unwrap()).is_ok();
            assert!(check(r#"{"max_size":4096}"#));
            assert!(check(r#"{"max_size":0}"#));
            assert!(check(r#"{"max_size":"2G"}"#));
            assert!(check(r#"{"max_size":" 512 MiB "}"#));
            assert!(!check(r#"{"max_size":true}"#));
            assert!(!check(r#"{"max_size":-1}"#));
            assert!(!check(r#"{"max_size":1.5}"#));
            assert!(!check(r#"{"max_size":"12XB"}"#));
            assert!(!check(r#"{"max_size":[1]}"#));
            assert!(!check(r#"{}"#));
        }

        /// The schema's string pattern accepts exactly the strings `FromStr`
        /// accepts, apart from the `u64::MAX` limit it cannot express.
        #[test]
        fn json_schema_pattern_agrees_with_from_str() {
            use super::super::ByteSizeErrorKind;

            let schema = Schema::of::<ByteSize>().unwrap();
            let inputs = [
                "0",
                "1024",
                "512B",
                "512 b",
                "64KiB",
                "1.5 MiB",
                "1gib",
                "2G",
                "2g",
                "512k",
                "3T",
                "1PiB",
                "2PB",
                "15 eib",
                "1EB",
                "500MB",
                "2 GB",
                "1kB",
                "1KB",
                "  7 TB  ",
                "\t1K\n",
                "1.5K",
                ".5KiB",
                "1.",
                "1.0",
                "2.000 B",
                ".0",
                "0.001KiB",
                "16EiB",
                "18446744073709551616",
                "",
                "   ",
                "-1K",
                "-0",
                "12XB",
                "12 KIBS",
                "1Kb/s",
                "1 bytes",
                "K",
                ".",
                "1.2.3",
                "1 2",
                "+1",
                "1e6",
                "1.5",
                "0.3B",
                "1.9999",
                "1E",
                "1e",
                "1 e",
                "1P",
                "1.5e",
                "1 K B",
                "1KiBB",
                "\u{a0}1K",
                "1\u{85}",
                "１K",
            ];
            for input in inputs {
                let parsed = input.parse::<ByteSize>();
                let accepts =
                    parsed.is_ok() || parsed.unwrap_err().kind() == ByteSizeErrorKind::Overflow;
                let valid = schema.validate(&Value::Str(input.to_string())).is_ok();
                assert_eq!(valid, accepts, "{input:?}");
            }
        }

        #[test]
        fn encodes_as_bytes_and_round_trips() {
            assert_eq!(ByteSize::mib(2).encode(), Value::UInt(2 << 20));
            let text = json::encode(&cache(1536));
            assert_eq!(text, r#"{"max_size":1536}"#);
            assert_eq!(json::decode::<Cache>(&text).unwrap(), cache(1536));
        }

        #[cfg(feature = "config")]
        #[test]
        fn decodes_from_layered_config() {
            use crate::fsutils::TempDir;
            use ferroly::config::Config;

            let dir = TempDir::new("ferroly-bytesize-config").unwrap();
            let path = dir.path().join("app.yaml");
            std::fs::write(&path, "cache:\n  max_size: 2G\n  min_size: 1024\n").unwrap();
            let config = Config::builder().merge_file(&path).unwrap().build();
            assert_eq!(
                config.get::<ByteSize>("cache.max_size").unwrap(),
                ByteSize::gib(2)
            );
            assert_eq!(
                config.get::<ByteSize>("cache.min_size").unwrap(),
                ByteSize(1024)
            );

            let layered = Config::builder()
                .merge_value(Value::Object(vec![(
                    "max_size".to_string(),
                    Value::UInt(4096),
                )]))
                .build();
            assert_eq!(layered.extract::<Cache>().unwrap(), cache(4096));
            let layered = Config::builder()
                .merge_value(Value::Object(vec![(
                    "max_size".to_string(),
                    Value::Str("256 MiB".to_string()),
                )]))
                .build();
            assert_eq!(layered.extract::<Cache>().unwrap(), cache(256 << 20));
        }
    }
}

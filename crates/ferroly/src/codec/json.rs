//! Hand-written JSON codec over the [`Value`] model.
//!
//! The parser scans the input as bytes (JSON structural tokens are all ASCII,
//! and `"`/`\` never occur inside a UTF-8 multibyte sequence), slicing string
//! and number spans directly from the input rather than rebuilding them
//! character by character. The encoder appends scalars in place via the
//! shared `fmt` writers, avoiding a `String` allocation per value.
//!
//! Parsing follows RFC 8259 strictly: numbers must match the JSON grammar
//! (`01`, `1.`, `-.5` are rejected), a number that overflows to infinity is an
//! error, and raw control characters inside strings are rejected. Typed
//! decoding is **strict** — a JSON string is never coerced into a number or
//! bool. Parse errors report the line and column of the offending byte, and
//! every entry point enforces a [`Limits`] budget.

use std::borrow::Cow;
use std::fmt::Write as _;

use ferroly::codec::{emit_value, CodecError, Decode, Decoder, Encode, Encoder, Limits, Value};

use super::fmt;
use super::limits::{error_at, Budget};

/// Encodes any [`Encode`] type to a compact JSON string, streaming straight to
/// the output with no intermediate [`Value`] tree.
///
/// Non-finite floats (`NaN`, `±inf`) have no JSON form and are written as
/// `null`; use [`try_encode`] to get an error instead.
pub fn encode<T: Encode + ?Sized>(value: &T) -> String {
    let mut e = JsonEncoder::new(None);
    value.encode_to(&mut e);
    e.out
}

/// Encodes any [`Encode`] type to an indented, human-readable JSON string
/// (two-space indent, one member per line).
///
/// ```
/// use ferroly::codec::{json, Value};
///
/// let v = json::from_str(r#"{"a":[1,2],"b":{}}"#).unwrap();
/// assert_eq!(
///     json::encode_pretty(&v),
///     "{\n  \"a\": [\n    1,\n    2\n  ],\n  \"b\": {}\n}"
/// );
/// ```
pub fn encode_pretty<T: Encode + ?Sized>(value: &T) -> String {
    let mut e = JsonEncoder::new(Some(2));
    value.encode_to(&mut e);
    e.out
}

/// Like [`encode`], but fails instead of silently writing `null` for a value
/// JSON cannot represent (a non-finite float).
pub fn try_encode<T: Encode + ?Sized>(value: &T) -> Result<String, CodecError> {
    let mut e = JsonEncoder::new(None);
    value.encode_to(&mut e);
    if e.non_finite {
        return Err(CodecError::unrepresentable(
            "JSON has no representation for NaN or infinite floats",
        ));
    }
    Ok(e.out)
}

/// Encodes any [`Encode`] type to compact JSON bytes.
pub fn encode_to_vec<T: Encode + ?Sized>(value: &T) -> Vec<u8> {
    encode(value).into_bytes()
}

/// Decodes any [`Decode`] type from a JSON string, pulling values straight from
/// the parser without materializing the full [`Value`] tree.
pub fn decode<T: Decode>(input: &str) -> Result<T, CodecError> {
    decode_with(input, Limits::DEFAULT)
}

/// Like [`decode`], with an explicit parser budget.
pub fn decode_with<T: Decode>(input: &str, limits: Limits) -> Result<T, CodecError> {
    let mut p = Parser::new(input, limits)?;
    let value = T::decode_from(&mut p)?;
    p.finish()?;
    Ok(value)
}

/// Decodes any [`Decode`] type from JSON bytes.
pub fn decode_from_slice<T: Decode>(input: &[u8]) -> Result<T, CodecError> {
    decode_from_slice_with(input, Limits::DEFAULT)
}

/// Like [`decode_from_slice`], with an explicit parser budget.
pub fn decode_from_slice_with<T: Decode>(input: &[u8], limits: Limits) -> Result<T, CodecError> {
    decode_with(utf8(input)?, limits)
}

/// Renders a [`Value`] as a compact JSON string.
pub fn to_string(value: &Value) -> String {
    let mut e = JsonEncoder::new(None);
    emit_value(&mut e, value);
    e.out
}

/// Renders a [`Value`] as an indented JSON string (see [`encode_pretty`]).
pub fn to_string_pretty(value: &Value) -> String {
    let mut e = JsonEncoder::new(Some(2));
    emit_value(&mut e, value);
    e.out
}

/// Parses a JSON string into a [`Value`].
pub fn from_str(input: &str) -> Result<Value, CodecError> {
    from_str_with(input, Limits::DEFAULT)
}

/// Like [`from_str`], with an explicit parser budget.
pub fn from_str_with(input: &str, limits: Limits) -> Result<Value, CodecError> {
    let mut p = Parser::new(input, limits)?;
    let v = p.parse_value()?;
    p.finish()?;
    Ok(v)
}

/// Parses JSON bytes into a [`Value`].
pub fn from_slice(input: &[u8]) -> Result<Value, CodecError> {
    from_slice_with(input, Limits::DEFAULT)
}

/// Like [`from_slice`], with an explicit parser budget.
pub fn from_slice_with(input: &[u8], limits: Limits) -> Result<Value, CodecError> {
    from_str_with(utf8(input)?, limits)
}

fn utf8(input: &[u8]) -> Result<&str, CodecError> {
    std::str::from_utf8(input).map_err(|e| CodecError::parse(e.to_string()))
}

// ---- encoder ----------------------------------------------------------

/// Streaming JSON output sink: appends tokens directly, tracking a per-container
/// "first entry" flag so element/entry separators are emitted correctly.
struct JsonEncoder {
    out: String,
    first: Vec<bool>,
    /// `Some(width)` for indented output, `None` for compact.
    indent: Option<usize>,
    /// Set when a non-finite float was written as `null`.
    non_finite: bool,
}

impl JsonEncoder {
    fn new(indent: Option<usize>) -> Self {
        Self {
            out: String::with_capacity(64),
            first: Vec::with_capacity(8),
            indent,
            non_finite: false,
        }
    }

    /// Writes the leading `,` before an element/entry, except the first; in
    /// indented mode, also starts the entry on its own line.
    fn sep(&mut self) {
        if let Some(first) = self.first.last_mut() {
            if *first {
                *first = false;
            } else {
                self.out.push(',');
            }
        }
        self.newline(self.first.len());
    }

    /// In indented mode, breaks the line and indents to `depth` levels.
    fn newline(&mut self, depth: usize) {
        if let Some(width) = self.indent {
            self.out.push('\n');
            for _ in 0..depth * width {
                self.out.push(' ');
            }
        }
    }

    /// Closes a container: non-empty ones put the bracket on its own line.
    fn close(&mut self, bracket: char) {
        let was_empty = self.first.pop().unwrap_or(true);
        if !was_empty {
            self.newline(self.first.len());
        }
        self.out.push(bracket);
    }
}

impl Encoder for JsonEncoder {
    fn encode_null(&mut self) {
        self.out.push_str("null");
    }
    fn encode_bool(&mut self, v: bool) {
        self.out.push_str(if v { "true" } else { "false" });
    }
    fn encode_i64(&mut self, v: i64) {
        fmt::write_i64(&mut self.out, v);
    }
    fn encode_u64(&mut self, v: u64) {
        fmt::write_u64(&mut self.out, v);
    }
    fn encode_f64(&mut self, v: f64) {
        if !v.is_finite() {
            self.non_finite = true;
        }
        fmt::write_json_f64(&mut self.out, v);
    }
    fn encode_str(&mut self, v: &str) {
        write_string(&mut self.out, v);
    }
    fn encode_bytes(&mut self, v: &[u8]) {
        // Represent raw bytes as a JSON array of integers.
        self.out.push('[');
        for (i, byte) in v.iter().enumerate() {
            if i > 0 {
                self.out.push(',');
            }
            fmt::write_u64(&mut self.out, *byte as u64);
        }
        self.out.push(']');
    }
    fn begin_seq(&mut self, _len: usize) {
        self.out.push('[');
        self.first.push(true);
    }
    fn seq_entry<V: Encode + ?Sized>(&mut self, v: &V) {
        self.sep();
        v.encode_to(self);
    }
    fn end_seq(&mut self) {
        self.close(']');
    }
    fn begin_map(&mut self, _len: usize) {
        self.out.push('{');
        self.first.push(true);
    }
    fn map_entry<V: Encode + ?Sized>(&mut self, key: &str, v: &V) {
        self.sep();
        write_string(&mut self.out, key);
        self.out.push(':');
        if self.indent.is_some() {
            self.out.push(' ');
        }
        v.encode_to(self);
    }
    fn end_map(&mut self) {
        self.close('}');
    }
}

/// Writes a quoted, escaped JSON string, bulk-copying runs that need no escape.
fn write_string(out: &mut String, s: &str) {
    out.push('"');
    let bytes = s.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        // Bytes >= 0x20 other than `"`/`\` (including all UTF-8 continuation
        // bytes) need no escaping and stay in the current clean run.
        let escape: &str = match b {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            0x08 => "\\b",
            0x0c => "\\f",
            c if c < 0x20 => {
                out.push_str(&s[start..i]);
                let _ = write!(out, "\\u{:04x}", c as u32);
                start = i + 1;
                continue;
            }
            _ => continue,
        };
        out.push_str(&s[start..i]);
        out.push_str(escape);
        start = i + 1;
    }
    out.push_str(&s[start..]);
    out.push('"');
}

// ---- parser --------------------------------------------------------------

struct Parser<'a> {
    input: &'a str,
    bytes: &'a [u8],
    pos: usize,
    /// Depth and entry counters, checked against the caller's [`Limits`] so
    /// hostile input (`[[[[…`, or millions of tiny elements) is rejected
    /// instead of overflowing the stack or exhausting memory.
    budget: Budget,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str, limits: Limits) -> Result<Self, CodecError> {
        Ok(Self {
            input,
            bytes: input.as_bytes(),
            // Tolerate a leading byte-order mark (three bytes in UTF-8).
            pos: if input.starts_with('\u{feff}') { 3 } else { 0 },
            budget: Budget::new(limits, input.len())?,
        })
    }

    /// A parse error positioned at the current byte.
    fn err(&self, msg: impl std::fmt::Display) -> CodecError {
        error_at(self.input, self.pos, msg)
    }

    /// A parse error positioned at an earlier byte (e.g. a token's start).
    fn err_at(&self, pos: usize, msg: impl std::fmt::Display) -> CodecError {
        error_at(self.input, pos, msg)
    }

    /// Requires that only whitespace remains.
    fn finish(&mut self) -> Result<(), CodecError> {
        self.skip_ws();
        if self.pos != self.bytes.len() {
            return Err(self.err("trailing characters after JSON value"));
        }
        Ok(())
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.bytes.get(self.pos).copied();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn parse_value(&mut self) -> Result<Value, CodecError> {
        self.skip_ws();
        match self.peek() {
            Some(b'{') => {
                self.budget.enter()?;
                let v = self.parse_object();
                self.budget.leave();
                v
            }
            Some(b'[') => {
                self.budget.enter()?;
                let v = self.parse_array();
                self.budget.leave();
                v
            }
            Some(b'"') => Ok(Value::Str(self.parse_string()?)),
            Some(b't') | Some(b'f') => self.parse_bool(),
            Some(b'n') => self.parse_null(),
            Some(b) if b == b'-' || b.is_ascii_digit() => self.parse_number(),
            Some(_) => {
                // Report the whole character, not just its first byte.
                let c = self.input[self.pos..].chars().next().unwrap_or('?');
                Err(self.err(format!("unexpected character {c:?}")))
            }
            None => Err(self.err("unexpected end of input")),
        }
    }

    fn expect(&mut self, b: u8) -> Result<(), CodecError> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.err(format!("expected '{}'", b as char)))
        }
    }

    /// Consumes a container's opening bracket for the streaming decoder, or
    /// reports that the next value has the wrong shape.
    fn open(&mut self, bracket: u8, what: &'static str) -> Result<(), CodecError> {
        self.skip_ws();
        match self.peek() {
            Some(b) if b == bracket => {
                self.pos += 1;
                Ok(())
            }
            None => Err(self.err("unexpected end of input")),
            Some(_) => Err(CodecError::expected(what)),
        }
    }

    fn parse_object(&mut self) -> Result<Value, CodecError> {
        self.expect(b'{')?;
        let mut entries = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(entries));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(b':')?;
            self.budget.entry()?;
            let value = self.parse_value()?;
            entries.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.err("expected ',' or '}' in object")),
            }
        }
        Ok(Value::Object(entries))
    }

    fn parse_array(&mut self) -> Result<Value, CodecError> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.budget.entry()?;
            let value = self.parse_value()?;
            items.push(value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.err("expected ',' or ']' in array")),
            }
        }
        Ok(Value::Array(items))
    }

    fn parse_string(&mut self) -> Result<String, CodecError> {
        self.parse_str().map(Cow::into_owned)
    }

    /// Parses a string, borrowing the span directly when it has no escapes
    /// (the common case) so no `String` is allocated until one is needed.
    fn parse_str(&mut self) -> Result<Cow<'a, str>, CodecError> {
        if self.peek() != Some(b'"') {
            return Err(self.err("expected '\"'"));
        }
        self.pos += 1;
        let start = self.pos;
        loop {
            match self.peek() {
                None => return Err(self.err("unterminated string")),
                // Common case: no escapes — slice the span directly.
                Some(b'"') => {
                    let s: &'a str = &self.input[start..self.pos];
                    self.pos += 1;
                    return Ok(Cow::Borrowed(s));
                }
                Some(b'\\') => return self.parse_string_escaped(start).map(Cow::Owned),
                Some(b) if b < 0x20 => {
                    return Err(self.err("unescaped control character in string"))
                }
                // Advance over any other byte, including UTF-8 continuations.
                Some(_) => self.pos += 1,
            }
        }
    }

    /// Slow path once a backslash is seen: flush clean runs, decode escapes.
    fn parse_string_escaped(&mut self, start: usize) -> Result<String, CodecError> {
        let mut s = String::with_capacity(self.pos - start + 16);
        let mut run = start;
        loop {
            match self.peek() {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => {
                    s.push_str(&self.input[run..self.pos]);
                    self.pos += 1;
                    return Ok(s);
                }
                Some(b'\\') => {
                    s.push_str(&self.input[run..self.pos]);
                    self.pos += 1; // consume '\'
                    match self.bump() {
                        Some(b'"') => s.push('"'),
                        Some(b'\\') => s.push('\\'),
                        Some(b'/') => s.push('/'),
                        Some(b'n') => s.push('\n'),
                        Some(b't') => s.push('\t'),
                        Some(b'r') => s.push('\r'),
                        Some(b'b') => s.push('\x08'),
                        Some(b'f') => s.push('\x0c'),
                        Some(b'u') => s.push(self.parse_unicode_escape()?),
                        _ => {
                            return Err(
                                self.err_at(self.pos.saturating_sub(1), "invalid escape sequence")
                            )
                        }
                    }
                    run = self.pos;
                }
                Some(b) if b < 0x20 => {
                    return Err(self.err("unescaped control character in string"))
                }
                Some(_) => self.pos += 1,
            }
        }
    }

    fn parse_unicode_escape(&mut self) -> Result<char, CodecError> {
        let high = self.read_hex4()?;
        // Handle UTF-16 surrogate pairs.
        if (0xD800..=0xDBFF).contains(&high) {
            if self.bump() != Some(b'\\') || self.bump() != Some(b'u') {
                return Err(self.err("expected low surrogate"));
            }
            let low = self.read_hex4()?;
            // The low half must be in the low-surrogate range; otherwise the
            // `low - 0xDC00` below would underflow (panicking in debug builds,
            // silently wrapping in release). Reject before doing the arithmetic.
            if !(0xDC00..=0xDFFF).contains(&low) {
                return Err(self.err("invalid low surrogate"));
            }
            let c = 0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00);
            char::from_u32(c).ok_or_else(|| self.err("invalid surrogate pair"))
        } else {
            char::from_u32(high).ok_or_else(|| self.err("invalid unicode escape"))
        }
    }

    fn read_hex4(&mut self) -> Result<u32, CodecError> {
        let mut v = 0u32;
        for _ in 0..4 {
            let d = match self.peek() {
                None => return Err(self.err("short unicode escape")),
                Some(b) => (b as char)
                    .to_digit(16)
                    .ok_or_else(|| self.err("invalid hex digit"))?,
            };
            self.pos += 1;
            v = v * 16 + d;
        }
        Ok(v)
    }

    fn parse_bool(&mut self) -> Result<Value, CodecError> {
        if self.take_literal(b"true") {
            Ok(Value::Bool(true))
        } else if self.take_literal(b"false") {
            Ok(Value::Bool(false))
        } else {
            Err(self.err("invalid literal"))
        }
    }

    fn parse_null(&mut self) -> Result<Value, CodecError> {
        if self.take_literal(b"null") {
            Ok(Value::Null)
        } else {
            Err(self.err("invalid literal"))
        }
    }

    fn take_literal(&mut self, lit: &[u8]) -> bool {
        if self.bytes[self.pos..].starts_with(lit) {
            self.pos += lit.len();
            true
        } else {
            false
        }
    }

    /// Consumes a run of ASCII digits, returning how many there were.
    fn digits(&mut self) -> usize {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        self.pos - start
    }

    /// Parses a number by the RFC 8259 grammar:
    /// `-? (0 | [1-9][0-9]*) (\.[0-9]+)? ([eE][+-]?[0-9]+)?`.
    fn parse_number(&mut self) -> Result<Value, CodecError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(self.err_at(start, "invalid number: leading zero"));
                }
            }
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return Err(self.err("invalid number: expected a digit")),
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            if self.digits() == 0 {
                return Err(self.err("invalid number: expected a digit after '.'"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if self.digits() == 0 {
                return Err(self.err("invalid number: expected a digit in the exponent"));
            }
        }
        let text = &self.input[start..self.pos];
        if !is_float {
            if let Ok(i) = text.parse::<i64>() {
                return Ok(Value::Int(i));
            }
            if let Ok(u) = text.parse::<u64>() {
                return Ok(Value::UInt(u));
            }
            // An integer beyond 64 bits falls through to a float.
        }
        match text.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(Value::Float(f)),
            _ => Err(self.err_at(start, format!("number out of range: {text}"))),
        }
    }
}

// The parser doubles as a streaming [`Decoder`]: it pulls tokens on demand so
// `Decode::decode_from` builds structs directly, without a full `Value` tree.
impl<'a> Decoder for Parser<'a> {
    fn decode_value(&mut self) -> Result<Value, CodecError> {
        self.parse_value()
    }

    fn decode_string(&mut self) -> Result<String, CodecError> {
        self.skip_ws();
        if self.peek() != Some(b'"') {
            return Err(CodecError::expected("string"));
        }
        self.parse_string()
    }

    fn peek_null(&mut self) -> bool {
        self.skip_ws();
        self.bytes[self.pos..].starts_with(b"null")
    }

    fn decode_null(&mut self) -> Result<(), CodecError> {
        self.skip_ws();
        if self.take_literal(b"null") {
            Ok(())
        } else {
            Err(CodecError::expected("null"))
        }
    }

    fn read_seq<F>(&mut self, mut f: F) -> Result<(), CodecError>
    where
        F: FnMut(&mut Self) -> Result<(), CodecError>,
    {
        self.open(b'[', "array")?;
        self.budget.enter()?;
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            self.budget.leave();
            return Ok(());
        }
        loop {
            self.budget.entry()?;
            f(self)?;
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.err("expected ',' or ']' in array")),
            }
        }
        self.budget.leave();
        Ok(())
    }

    fn read_map<F>(&mut self, mut f: F) -> Result<(), CodecError>
    where
        F: FnMut(&mut Self, &str) -> Result<(), CodecError>,
    {
        self.open(b'{', "object")?;
        self.budget.enter()?;
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            self.budget.leave();
            return Ok(());
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.err("expected object key"));
            }
            let key = self.parse_str()?;
            self.skip_ws();
            self.expect(b':')?;
            self.budget.entry()?;
            f(self, &key)?;
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.err("expected ',' or '}' in object")),
            }
        }
        self.budget.leave();
        Ok(())
    }

    fn skip_value(&mut self) -> Result<(), CodecError> {
        self.parse_value().map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX_DEPTH: usize = Limits::DEFAULT_MAX_DEPTH;

    #[test]
    fn rejects_lone_high_surrogate_without_panic() {
        // A high surrogate followed by a non-low-surrogate escape must error,
        // not panic (the `low - 0xDC00` subtraction used to overflow here).
        for input in [r#""\uD800A""#, r#""\uD800 ""#, r#""\uDABC\uD800""#] {
            let r = from_str(input);
            assert!(r.is_err(), "expected error for {input:?}, got {r:?}");
        }
    }

    #[test]
    fn accepts_valid_surrogate_pair() {
        // U+1F600 GRINNING FACE encoded as a UTF-16 surrogate pair. The input is
        // assembled from pieces so the escape provably reaches the parser as the
        // twelve ASCII characters, not as an already-decoded character.
        let input = ["\"", "\\u", "D83D", "\\u", "DE00", "\""].concat();
        assert_eq!(input.len(), 14);
        assert!(input.is_ascii());
        let v = from_str(&input).unwrap();
        assert_eq!(v.as_str(), Some("\u{1F600}"));
        assert_eq!(decode::<String>(&input).unwrap(), "\u{1F600}");
        // The literal character is accepted too.
        let literal = from_str("\"\u{1F600}\"").unwrap();
        assert_eq!(literal.as_str(), Some("\u{1F600}"));
    }

    #[test]
    fn duplicate_keys_resolve_last_wins() {
        let v = from_str(r#"{"a":1,"b":2,"a":3}"#).unwrap();
        // `get` returns the last occurrence, matching struct decoding.
        assert_eq!(v.get("a").and_then(Value::as_i64), Some(3));
        assert_eq!(v.get("b").and_then(Value::as_i64), Some(2));
    }

    #[test]
    fn rejects_deeply_nested_input_without_stack_overflow() {
        let deep_array = format!(
            "{}{}",
            "[".repeat(MAX_DEPTH + 50),
            "]".repeat(MAX_DEPTH + 50)
        );
        assert!(from_str(&deep_array).is_err());
        let deep_object = format!(
            "{}true{}",
            r#"{"a":"#.repeat(MAX_DEPTH + 50),
            "}".repeat(MAX_DEPTH + 50)
        );
        assert!(from_str(&deep_object).is_err());

        // Nesting within the limit still parses.
        let ok = format!(
            "{}1{}",
            "[".repeat(MAX_DEPTH - 1),
            "]".repeat(MAX_DEPTH - 1)
        );
        assert!(from_str(&ok).is_ok());
    }

    #[test]
    fn leading_byte_order_mark_is_skipped() {
        assert_eq!(
            from_str("\u{feff}[1]").unwrap(),
            Value::Array(vec![Value::Int(1)])
        );
        assert_eq!(decode::<Vec<u8>>("\u{feff} [1]").unwrap(), vec![1]);
        // Only at the very start, and only once.
        assert!(from_str("[\u{feff}1]").is_err());
        assert!(from_str("\u{feff}\u{feff}[1]").is_err());
        assert!(from_str("\u{feff}").is_err());
    }

    #[test]
    fn number_grammar_is_strict() {
        for bad in [
            "01",
            "-01",
            "1.",
            "-.5",
            ".5",
            "1.e3",
            "1e",
            "1e+",
            "+1",
            "-",
            "--1",
            "0x10",
            "1_0",
            "1.2.3",
            "00",
            "- 1",
            "Infinity",
            "NaN",
            "-Infinity",
        ] {
            assert!(from_str(bad).is_err(), "{bad:?} must be rejected");
            let wrapped = format!("[{bad}]");
            assert!(from_str(&wrapped).is_err(), "{wrapped:?} must be rejected");
        }
        for (good, want) in [
            ("0", Value::Int(0)),
            ("-0", Value::Int(0)),
            ("10", Value::Int(10)),
            ("-7", Value::Int(-7)),
            ("0.5", Value::Float(0.5)),
            ("-0.5e-2", Value::Float(-0.005)),
            ("1E3", Value::Float(1000.0)),
            ("1e+3", Value::Float(1000.0)),
            ("18446744073709551615", Value::UInt(u64::MAX)),
            ("18446744073709551616", Value::Float(18446744073709551616.0)),
        ] {
            assert_eq!(from_str(good).unwrap(), want, "{good:?}");
        }
    }

    #[test]
    fn rejects_numbers_that_overflow_to_infinity() {
        for bad in ["1e999", "-1e999", "[1e400]", r#"{"a":1e309}"#] {
            assert!(from_str(bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(decode::<f64>("1e999").is_err());
        // A tiny number underflows to zero, which is finite and fine.
        assert_eq!(from_str("1e-999").unwrap(), Value::Float(0.0));
    }

    #[test]
    fn rejects_raw_control_characters_in_strings() {
        for bad in [
            "\"a\nb\"",
            "\"a\tb\"",
            "\"\u{0}\"",
            "\"\u{1f}\"",
            "\"x\\n\u{1}\"",
            "{\"a\nb\":1}",
        ] {
            assert!(from_str(bad).is_err(), "{bad:?} must be rejected");
        }
        #[derive(ferroly::codec::Decode, Debug)]
        struct S {
            #[allow(dead_code)]
            a: i32,
        }
        assert!(decode::<S>("{\"a\tb\":1,\"a\":2}").is_err());
        // The escaped forms are fine, and so is DEL (0x7f).
        assert_eq!(
            from_str("\"a\\n\\t\\u0001\u{7f}\"").unwrap(),
            Value::Str("a\n\t\u{1}\u{7f}".into())
        );
    }

    #[test]
    fn parse_errors_report_line_and_column() {
        let err = from_str("{\n  \"a\": 1,\n  \"b\": ?\n}").unwrap_err();
        assert_eq!(
            err.to_string(),
            "parse error: unexpected character '?' at line 3, column 8"
        );
        let err = from_str("[1, 2,\n 3 4]").unwrap_err();
        assert!(err.to_string().ends_with("at line 2, column 4"), "{err}");
        // Columns count characters, not bytes.
        let err = from_str("[\"héé\", x]").unwrap_err();
        assert!(err.to_string().ends_with("at line 1, column 9"), "{err}");
        let err = decode::<Vec<i32>>("[1,\n2,,]").unwrap_err();
        assert!(err.to_string().ends_with("at line 2, column 3"), "{err}");
    }

    #[test]
    fn pretty_printing_round_trips_and_indents() {
        let src =
            r#"{"name":"api","tags":["a","b"],"empty":[],"none":{},"nested":{"k":[{"x":1}]}}"#;
        let v = from_str(src).unwrap();
        let pretty = to_string_pretty(&v);
        assert_eq!(
            pretty,
            "{\n  \"name\": \"api\",\n  \"tags\": [\n    \"a\",\n    \"b\"\n  ],\n  \"empty\": [],\n  \"none\": {},\n  \"nested\": {\n    \"k\": [\n      {\n        \"x\": 1\n      }\n    ]\n  }\n}"
        );
        assert_eq!(from_str(&pretty).unwrap(), v);
        assert_eq!(encode_pretty(&v), pretty);
        // Scalars are unchanged.
        assert_eq!(encode_pretty(&5i32), "5");
        assert_eq!(encode_pretty("x"), "\"x\"");
    }

    #[test]
    fn try_encode_rejects_non_finite_floats() {
        assert_eq!(try_encode(&1.5f64).unwrap(), "1.5");
        assert!(matches!(
            try_encode(&f64::NAN),
            Err(CodecError::Unrepresentable(_))
        ));
        assert!(try_encode(&vec![1.0, f64::INFINITY]).is_err());
        // The infallible form keeps writing `null`.
        assert_eq!(encode(&f64::NAN), "null");
    }

    #[test]
    fn limits_bound_depth_size_and_entries() {
        let limits = Limits::new().with_max_depth(3);
        assert!(from_str_with("[[[1]]]", limits).is_ok());
        assert!(matches!(
            from_str_with("[[[[1]]]]", limits),
            Err(CodecError::LimitExceeded(_))
        ));
        assert!(decode_with::<Vec<Vec<Vec<i32>>>>("[[[1]]]", limits).is_ok());
        assert!(decode_with::<Vec<Vec<Vec<Vec<i32>>>>>("[[[[1]]]]", limits).is_err());

        let limits = Limits::new().with_max_input_bytes(8);
        assert!(from_str_with("[1,2,3]", limits).is_ok());
        assert!(matches!(
            from_str_with("[1,2,3,4]", limits),
            Err(CodecError::LimitExceeded(_))
        ));
        assert!(from_slice_with(b"[1,2,3,4]", limits).is_err());
        assert!(decode_from_slice_with::<Vec<u8>>(b"[1,2,3,4]", limits).is_err());

        let limits = Limits::new().with_max_entries(4);
        assert!(from_str_with(r#"{"a":[1,2,3]}"#, limits).is_ok());
        assert!(matches!(
            from_str_with(r#"{"a":[1,2,3,4]}"#, limits),
            Err(CodecError::LimitExceeded(_))
        ));
        assert!(decode_with::<Vec<i32>>("[1,2,3,4]", limits).is_ok());
        assert!(decode_with::<Vec<i32>>("[1,2,3,4,5]", limits).is_err());
    }

    #[test]
    fn typed_decoding_does_not_coerce_strings() {
        assert!(decode::<i32>("\" 42 \"").is_err());
        assert!(decode::<u8>("\"7\"").is_err());
        assert!(decode::<f64>("\"1.5\"").is_err());
        assert!(decode::<bool>("\"true\"").is_err());
        assert!(decode::<Vec<u16>>("[\"1\"]").is_err());
        #[derive(ferroly::codec::Decode, Debug)]
        struct S {
            #[allow(dead_code)]
            port: u16,
        }
        assert!(decode::<S>(r#"{"port":"8080"}"#).is_err());
        assert!(decode::<S>(r#"{"port":8080}"#).is_ok());
        // Numbers still convert between integer and float where exact.
        assert_eq!(decode::<f64>("3").unwrap(), 3.0);
        assert_eq!(decode::<i32>("3.0").unwrap(), 3);
    }
}

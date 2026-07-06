//! Hand-written JSON codec over the [`Value`] model.
//!
//! The parser scans the input as bytes (JSON structural tokens are all ASCII,
//! and `"`/`\` never occur inside a UTF-8 multibyte sequence), slicing string
//! and number spans directly from the input rather than rebuilding them
//! character by character. The encoder appends scalars in place via the
//! shared `fmt` writers, avoiding a `String` allocation per value.

use std::fmt::Write as _;

use ferroly::codec::{emit_value, CodecError, Decode, Decoder, Encode, Encoder, Value};

use super::fmt;

/// Encodes any [`Encode`] type to a compact JSON string, streaming straight to
/// the output with no intermediate [`Value`] tree.
pub fn encode<T: Encode>(value: &T) -> String {
    let mut e = JsonEncoder::new();
    value.encode_to(&mut e);
    e.out
}

/// Encodes any [`Encode`] type to compact JSON bytes.
pub fn encode_to_vec<T: Encode>(value: &T) -> Vec<u8> {
    encode(value).into_bytes()
}

/// Decodes any [`Decode`] type from a JSON string, pulling values straight from
/// the parser without materializing the full [`Value`] tree.
pub fn decode<T: Decode>(input: &str) -> Result<T, CodecError> {
    let mut p = Parser::new(input);
    let value = T::decode_from(&mut p)?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err(CodecError::parse("trailing characters after JSON value"));
    }
    Ok(value)
}

/// Decodes any [`Decode`] type from JSON bytes.
pub fn decode_from_slice<T: Decode>(input: &[u8]) -> Result<T, CodecError> {
    let s = std::str::from_utf8(input).map_err(|e| CodecError::parse(e.to_string()))?;
    decode(s)
}

/// Renders a [`Value`] as a compact JSON string.
pub fn to_string(value: &Value) -> String {
    let mut e = JsonEncoder::new();
    emit_value(&mut e, value);
    e.out
}

/// Parses a JSON string into a [`Value`].
pub fn from_str(input: &str) -> Result<Value, CodecError> {
    let mut p = Parser::new(input);
    p.skip_ws();
    let v = p.parse_value()?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err(CodecError::parse("trailing characters after JSON value"));
    }
    Ok(v)
}

/// Parses JSON bytes into a [`Value`].
pub fn from_slice(input: &[u8]) -> Result<Value, CodecError> {
    let s = std::str::from_utf8(input).map_err(|e| CodecError::parse(e.to_string()))?;
    from_str(s)
}

// ---- encoder ----------------------------------------------------------

/// Streaming JSON output sink: appends tokens directly, tracking a per-container
/// "first entry" flag so element/entry separators are emitted correctly.
struct JsonEncoder {
    out: String,
    first: Vec<bool>,
}

impl JsonEncoder {
    fn new() -> Self {
        Self {
            out: String::with_capacity(64),
            first: Vec::with_capacity(8),
        }
    }

    /// Writes the leading `,` before an element/entry, except the first.
    fn sep(&mut self) {
        if let Some(first) = self.first.last_mut() {
            if *first {
                *first = false;
            } else {
                self.out.push(',');
            }
        }
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
        self.out.push(']');
        self.first.pop();
    }
    fn begin_map(&mut self, _len: usize) {
        self.out.push('{');
        self.first.push(true);
    }
    fn map_entry<V: Encode + ?Sized>(&mut self, key: &str, v: &V) {
        self.sep();
        write_string(&mut self.out, key);
        self.out.push(':');
        v.encode_to(self);
    }
    fn end_map(&mut self) {
        self.out.push('}');
        self.first.pop();
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

/// Maximum nesting depth for objects/arrays. Bounds stack usage so hostile
/// deeply-nested input (`[[[[…`) is rejected instead of overflowing the stack.
/// Matches serde_json's default recursion limit.
const MAX_DEPTH: usize = 128;

struct Parser<'a> {
    input: &'a str,
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            bytes: input.as_bytes(),
            pos: 0,
            depth: 0,
        }
    }

    /// Enters a nested container, rejecting input past [`MAX_DEPTH`]. Paired with
    /// [`leave`](Self::leave) on every non-error return.
    fn enter(&mut self) -> Result<(), CodecError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(CodecError::parse("nesting too deep"));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
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
                self.enter()?;
                let v = self.parse_object();
                self.leave();
                v
            }
            Some(b'[') => {
                self.enter()?;
                let v = self.parse_array();
                self.leave();
                v
            }
            Some(b'"') => Ok(Value::Str(self.parse_string()?)),
            Some(b't') | Some(b'f') => self.parse_bool(),
            Some(b'n') => self.parse_null(),
            Some(b) if b == b'-' || b.is_ascii_digit() => self.parse_number(),
            Some(b) => Err(CodecError::parse(format!(
                "unexpected character '{}'",
                b as char
            ))),
            None => Err(CodecError::parse("unexpected end of input")),
        }
    }

    fn expect(&mut self, b: u8) -> Result<(), CodecError> {
        if self.bump() == Some(b) {
            Ok(())
        } else {
            Err(CodecError::parse(format!("expected '{}'", b as char)))
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
            let value = self.parse_value()?;
            entries.push((key, value));
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b'}') => break,
                _ => return Err(CodecError::parse("expected ',' or '}' in object")),
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
            let value = self.parse_value()?;
            items.push(value);
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b']') => break,
                _ => return Err(CodecError::parse("expected ',' or ']' in array")),
            }
        }
        Ok(Value::Array(items))
    }

    fn parse_string(&mut self) -> Result<String, CodecError> {
        self.expect(b'"')?;
        let start = self.pos;
        loop {
            match self.peek() {
                None => return Err(CodecError::parse("unterminated string")),
                // Common case: no escapes — slice the span directly, one copy.
                Some(b'"') => {
                    let s = self.input[start..self.pos].to_string();
                    self.pos += 1;
                    return Ok(s);
                }
                Some(b'\\') => return self.parse_string_escaped(start),
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
                None => return Err(CodecError::parse("unterminated string")),
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
                        _ => return Err(CodecError::parse("invalid escape sequence")),
                    }
                    run = self.pos;
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
                return Err(CodecError::parse("expected low surrogate"));
            }
            let low = self.read_hex4()?;
            // The low half must be in the low-surrogate range; otherwise the
            // `low - 0xDC00` below would underflow (panicking in debug builds,
            // silently wrapping in release). Reject before doing the arithmetic.
            if !(0xDC00..=0xDFFF).contains(&low) {
                return Err(CodecError::parse("invalid low surrogate"));
            }
            let c = 0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00);
            char::from_u32(c).ok_or_else(|| CodecError::parse("invalid surrogate pair"))
        } else {
            char::from_u32(high).ok_or_else(|| CodecError::parse("invalid unicode escape"))
        }
    }

    fn read_hex4(&mut self) -> Result<u32, CodecError> {
        let mut v = 0u32;
        for _ in 0..4 {
            let b = self
                .bump()
                .ok_or_else(|| CodecError::parse("short unicode escape"))?;
            let d = (b as char)
                .to_digit(16)
                .ok_or_else(|| CodecError::parse("invalid hex digit"))?;
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
            Err(CodecError::parse("invalid literal"))
        }
    }

    fn parse_null(&mut self) -> Result<Value, CodecError> {
        if self.take_literal(b"null") {
            Ok(Value::Null)
        } else {
            Err(CodecError::parse("invalid literal"))
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

    fn parse_number(&mut self) -> Result<Value, CodecError> {
        let start = self.pos;
        let mut is_float = false;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while let Some(b) = self.peek() {
            match b {
                b'0'..=b'9' => self.pos += 1,
                b'.' | b'e' | b'E' | b'+' | b'-' => {
                    is_float = true;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        let text = &self.input[start..self.pos];
        if is_float {
            text.parse::<f64>()
                .map(Value::Float)
                .map_err(|_| CodecError::parse(format!("invalid number: {text}")))
        } else if let Ok(i) = text.parse::<i64>() {
            Ok(Value::Int(i))
        } else if let Ok(u) = text.parse::<u64>() {
            Ok(Value::UInt(u))
        } else {
            text.parse::<f64>()
                .map(Value::Float)
                .map_err(|_| CodecError::parse(format!("invalid number: {text}")))
        }
    }
}

// The parser doubles as a streaming [`Decoder`]: it pulls tokens on demand so
// `Decode::decode_from` builds structs directly, without a full `Value` tree.
impl<'a> Decoder for Parser<'a> {
    fn decode_value(&mut self) -> Result<Value, CodecError> {
        self.skip_ws();
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
        self.skip_ws();
        self.expect(b'[')?;
        self.enter()?;
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            self.leave();
            return Ok(());
        }
        loop {
            f(self)?;
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b']') => break,
                _ => return Err(CodecError::parse("expected ',' or ']' in array")),
            }
        }
        self.leave();
        Ok(())
    }

    fn read_map<F>(&mut self, mut f: F) -> Result<(), CodecError>
    where
        F: FnMut(&mut Self, &str) -> Result<(), CodecError>,
    {
        self.skip_ws();
        self.expect(b'{')?;
        self.enter()?;
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            self.leave();
            return Ok(());
        }
        loop {
            self.skip_ws();
            // Read the key, borrowing the span directly when it has no escapes
            // (the common case) so no `String` is allocated per key.
            if self.peek() != Some(b'"') {
                return Err(CodecError::parse("expected object key"));
            }
            self.pos += 1;
            let start = self.pos;
            let key: std::borrow::Cow<'a, str> = loop {
                match self.peek() {
                    None => return Err(CodecError::parse("unterminated string")),
                    Some(b'"') => {
                        let s: &'a str = &self.input[start..self.pos];
                        self.pos += 1;
                        break std::borrow::Cow::Borrowed(s);
                    }
                    Some(b'\\') => {
                        break std::borrow::Cow::Owned(self.parse_string_escaped(start)?)
                    }
                    Some(_) => self.pos += 1,
                }
            };
            self.skip_ws();
            self.expect(b':')?;
            f(self, &key)?;
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b'}') => break,
                _ => return Err(CodecError::parse("expected ',' or '}' in object")),
            }
        }
        self.leave();
        Ok(())
    }

    fn skip_value(&mut self) -> Result<(), CodecError> {
        self.skip_ws();
        self.parse_value().map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_lone_high_surrogate_without_panic() {
        // A high surrogate followed by a non-low-surrogate escape must error,
        // not panic (the `low - 0xDC00` subtraction used to overflow here).
        for input in [r#""\uD800A""#, r#""\uD800 ""#, r#""\uDABC\uD800""#] {
            let r = from_str(input);
            assert!(r.is_err(), "expected error for {input:?}, got {r:?}");
        }
    }

    #[test]
    fn accepts_valid_surrogate_pair() {
        // U+1F600 GRINNING FACE encoded as a UTF-16 surrogate pair.
        let v = from_str(r#""😀""#).unwrap();
        assert_eq!(v.as_str(), Some("\u{1F600}"));
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
}

//! A dependency-free TOML codec covering the common config subset.
//!
//! Supported: bare/quoted/dotted keys, `key = value` pairs, table headers
//! (`[a.b]`), arrays of tables (`[[a.b]]`), inline tables (`{ a = 1 }`), arrays
//! (`[1, 2, 3]`, may span lines with trailing commas), basic (`"..."`) and
//! literal (`'...'`) strings with the standard escapes, integers (decimal with
//! `_` separators and `0x`/`0o`/`0b` prefixes), floats (including `inf`/`-inf`/
//! `nan`), booleans, and `#` comments. The encoder emits nested structs as
//! `[section]` headers and `Vec`-of-structs as `[[section]]` arrays of tables.
//!
//! Not supported (use JSON for these): multi-line strings (`"""` / `'''`) and
//! native date-times — a date-time token decodes as its raw [`Value::Str`].
//! TOML has no null type, so [`Value::Null`] entries are omitted on encode
//! (a missing key decodes back to `None` for `Option` fields).
//!
//! The parser scans the input by byte position and bulk-copies string runs
//! that need no unescaping; the encoder appends scalars in place via the shared
//! `fmt` writers.
//!
//!# Example
//! ```rust
//! use ferroly::codec::{toml, Encode, Decode};
//! #[derive(Encode, Decode, PartialEq, Debug)]
//!struct Config {
//!     name: String,
//!     port: u16,
//! }
//!
//! # fn main() {
//! 	let my_config = Config {
//! 	    name: "api".into(),
//!     	port: 8080,
//! 	};
//!
//! // the part where i encode the struct to a toml string representation
//! 	let toml_string = toml::encode(&my_config);
//!
//! // and then decode it back into the rust's data structure ;)
//!		let decoded: Config = toml::decode(&toml_string).unwrap();
//!
//! 	assert_eq!(my_config, decoded);
//! # }
//! ```
use std::fmt::Write as _;

use ferroly::codec::{CodecError, Decode, Encode, Value};

use super::fmt;

/// Encodes any [`Encode`] type to a TOML string.
pub fn encode<T: Encode>(value: &T) -> String {
    let mut out = String::with_capacity(64);
    match value.encode() {
        Value::Object(entries) => write_table(&mut out, &entries, ""),
        // A TOML document is always a table; a bare scalar/array top-level value
        // is wrapped under a conventional key so the output stays valid TOML.
        other => {
            out.push_str("value = ");
            write_inline(&mut out, &other);
            out.push('\n');
        }
    }
    out
}

/// Encodes to TOML bytes.
pub fn encode_to_vec<T: Encode>(value: &T) -> Vec<u8> {
    encode(value).into_bytes()
}

/// Decodes any [`Decode`] type from a TOML string.
pub fn decode<T: Decode>(input: &str) -> Result<T, CodecError> {
    T::decode(&from_str(input)?)
}

/// Decodes from TOML bytes.
pub fn decode_from_slice<T: Decode>(input: &[u8]) -> Result<T, CodecError> {
    let s = std::str::from_utf8(input).map_err(|e| CodecError::parse(e.to_string()))?;
    decode(s)
}

/// Parses a TOML document into a [`Value::Object`].
pub fn from_str(input: &str) -> Result<Value, CodecError> {
    Parser::new(input).parse()
}

// ---- encoder -------------------------------------------------------------

/// Writes an object as a TOML table under the dotted `prefix` (empty at the
/// document root). Scalar and inline entries come first, then `[section]`
/// sub-tables and `[[section]]` arrays of tables, matching TOML's requirement
/// that a table's own keys precede its child tables.
fn write_table(out: &mut String, entries: &[(String, Value)], prefix: &str) {
    for (k, v) in entries {
        if is_null(v) || is_section(v) {
            continue;
        }
        write_key(out, k);
        out.push_str(" = ");
        write_inline(out, v);
        out.push('\n');
    }
    for (k, v) in entries {
        match v {
            Value::Object(o) if !o.is_empty() => {
                let header = header_join(prefix, k);
                out.push('\n');
                out.push('[');
                out.push_str(&header);
                out.push_str("]\n");
                write_table(out, o, &header);
            }
            Value::Array(a) if is_array_of_tables(a) => {
                let header = header_join(prefix, k);
                for item in a {
                    if let Value::Object(o) = item {
                        out.push('\n');
                        out.push_str("[[");
                        out.push_str(&header);
                        out.push_str("]]\n");
                        write_table(out, o, &header);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Whether a value is emitted as its own `[section]` / `[[section]]` block
/// rather than an inline `key = value`.
fn is_section(v: &Value) -> bool {
    match v {
        Value::Object(o) => !o.is_empty(),
        Value::Array(a) => is_array_of_tables(a),
        _ => false,
    }
}

fn is_null(v: &Value) -> bool {
    matches!(v, Value::Null)
}

/// A non-empty array whose every element is a table — emitted as `[[...]]`.
fn is_array_of_tables(a: &[Value]) -> bool {
    !a.is_empty() && a.iter().all(|v| matches!(v, Value::Object(_)))
}

/// Writes a value in inline form: scalars, `[...]` arrays, and `{ ... }` inline
/// tables. `Null` (only reachable inside an array/inline table) becomes `""`.
fn write_inline(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("\"\""),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => fmt::write_i64(out, *i),
        Value::UInt(u) => fmt::write_u64(out, *u),
        Value::Float(f) => write_float(out, *f),
        Value::Str(s) => write_basic_string(out, s),
        Value::Bytes(b) => {
            out.push('[');
            for (i, x) in b.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                fmt::write_u64(out, *x as u64);
            }
            out.push(']');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, e) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_inline(out, e);
            }
            out.push(']');
        }
        Value::Object(o) => {
            if o.iter().all(|(_, val)| is_null(val)) {
                out.push_str("{}");
                return;
            }
            out.push_str("{ ");
            let mut first = true;
            for (k, val) in o {
                if is_null(val) {
                    continue;
                }
                if !first {
                    out.push_str(", ");
                }
                first = false;
                write_key(out, k);
                out.push_str(" = ");
                write_inline(out, val);
            }
            out.push_str(" }");
        }
    }
}

/// Emits a float, guaranteeing it reads back as a float (adds `.0` to
/// integer-valued numbers) and using TOML's `inf`/`-inf`/`nan` literals.
fn write_float(out: &mut String, f: f64) {
    if f.is_nan() {
        out.push_str("nan");
    } else if f.is_infinite() {
        out.push_str(if f < 0.0 { "-inf" } else { "inf" });
    } else {
        let start = out.len();
        let _ = write!(out, "{f}");
        if !out[start..]
            .bytes()
            .any(|b| matches!(b, b'.' | b'e' | b'E'))
        {
            out.push_str(".0");
        }
    }
}

fn write_key(out: &mut String, k: &str) {
    out.push_str(&key_repr(k));
}

/// A key as a bare key when possible, otherwise a quoted basic string.
fn key_repr(k: &str) -> String {
    if !k.is_empty()
        && k.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        k.to_string()
    } else {
        let mut s = String::new();
        write_basic_string(&mut s, k);
        s
    }
}

fn header_join(prefix: &str, k: &str) -> String {
    let key = key_repr(k);
    if prefix.is_empty() {
        key
    } else {
        format!("{prefix}.{key}")
    }
}

/// Writes a double-quoted basic string, bulk-copying runs that need no escape.
fn write_basic_string(out: &mut String, s: &str) {
    out.push('"');
    let bytes = s.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let esc: &str = match b {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            0x08 => "\\b",
            0x0c => "\\f",
            c if c < 0x20 => {
                out.push_str(&s[start..i]);
                let _ = write!(out, "\\u{:04X}", c);
                start = i + 1;
                continue;
            }
            _ => continue,
        };
        out.push_str(&s[start..i]);
        out.push_str(esc);
        start = i + 1;
    }
    out.push_str(&s[start..]);
    out.push('"');
}

// ---- parser --------------------------------------------------------------

/// Maximum nesting depth for arrays / inline tables; bounds recursion.
const MAX_DEPTH: usize = 128;

struct Parser<'a> {
    src: &'a str,
    b: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Parser {
            src,
            b: src.as_bytes(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn starts_with(&self, s: &str) -> bool {
        self.b[self.pos..].starts_with(s.as_bytes())
    }

    /// Consumes spaces, tabs, and carriage returns (not newlines).
    fn skip_inline_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r')) {
            self.pos += 1;
        }
    }

    /// Consumes inline whitespace, newlines, and `#` comments — used between
    /// statements and inside arrays / inline tables.
    fn skip_ws_nl_comments(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\r' | b'\n') => self.pos += 1,
                Some(b'#') => {
                    while !matches!(self.peek(), None | Some(b'\n')) {
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
    }

    fn parse(&mut self) -> Result<Value, CodecError> {
        let mut root: Vec<(String, Value)> = Vec::new();
        let mut current: Vec<String> = Vec::new();
        loop {
            self.skip_ws_nl_comments();
            match self.peek() {
                None => break,
                Some(b'[') => {
                    if self.b.get(self.pos + 1) == Some(&b'[') {
                        self.pos += 2;
                        let path = self.parse_key_path()?;
                        self.expect(b']')?;
                        self.expect(b']')?;
                        append_array_table(&mut root, &path)?;
                        current = path;
                    } else {
                        self.pos += 1;
                        let path = self.parse_key_path()?;
                        self.expect(b']')?;
                        get_or_create(&mut root, &path)?;
                        current = path;
                    }
                    self.finish_line()?;
                }
                Some(_) => {
                    let keypath = self.parse_key_path()?;
                    self.skip_inline_ws();
                    self.expect(b'=')?;
                    self.skip_inline_ws();
                    let val = self.parse_value(0)?;
                    let base = resolve(&mut root, &current)?;
                    set_nested(base, &keypath, val)?;
                    self.finish_line()?;
                }
            }
        }
        Ok(Value::Object(root))
    }

    /// Expects the next non-inline-whitespace byte to be `c` and consumes it.
    fn expect(&mut self, c: u8) -> Result<(), CodecError> {
        self.skip_inline_ws();
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(())
        } else {
            Err(CodecError::parse(format!(
                "expected '{}' at byte {}",
                c as char, self.pos
            )))
        }
    }

    /// After a statement: optional trailing comment, then newline or EOF.
    fn finish_line(&mut self) -> Result<(), CodecError> {
        self.skip_inline_ws();
        if self.peek() == Some(b'#') {
            while !matches!(self.peek(), None | Some(b'\n')) {
                self.pos += 1;
            }
        }
        match self.peek() {
            None => Ok(()),
            Some(b'\n') => {
                self.pos += 1;
                Ok(())
            }
            Some(c) => Err(CodecError::parse(format!(
                "unexpected trailing input '{}' at byte {}",
                c as char, self.pos
            ))),
        }
    }

    /// Parses a dotted key path (`a.b.c`), each segment bare or quoted.
    fn parse_key_path(&mut self) -> Result<Vec<String>, CodecError> {
        let mut path = Vec::new();
        loop {
            self.skip_inline_ws();
            path.push(self.parse_key_segment()?);
            self.skip_inline_ws();
            if self.peek() == Some(b'.') {
                self.pos += 1;
            } else {
                break;
            }
        }
        Ok(path)
    }

    fn parse_key_segment(&mut self) -> Result<String, CodecError> {
        match self.peek() {
            Some(b'"') => self.parse_basic_string(),
            Some(b'\'') => self.parse_literal_string(),
            _ => {
                let start = self.pos;
                while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
                {
                    self.pos += 1;
                }
                if self.pos == start {
                    return Err(CodecError::parse(format!("expected key at byte {start}")));
                }
                Ok(self.src[start..self.pos].to_string())
            }
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<Value, CodecError> {
        if depth > MAX_DEPTH {
            return Err(CodecError::parse("nesting too deep"));
        }
        self.skip_inline_ws();
        match self.peek() {
            Some(b'"') => {
                if self.starts_with("\"\"\"") {
                    return Err(CodecError::parse("multi-line strings are not supported"));
                }
                Ok(Value::Str(self.parse_basic_string()?))
            }
            Some(b'\'') => {
                if self.starts_with("'''") {
                    return Err(CodecError::parse("multi-line strings are not supported"));
                }
                Ok(Value::Str(self.parse_literal_string()?))
            }
            Some(b'[') => self.parse_array(depth),
            Some(b'{') => self.parse_inline_table(depth),
            None => Err(CodecError::parse("expected a value")),
            Some(_) => self.parse_atom(),
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<Value, CodecError> {
        self.pos += 1; // '['
        let mut items = Vec::new();
        loop {
            self.skip_ws_nl_comments();
            if self.peek() == Some(b']') {
                self.pos += 1;
                break;
            }
            items.push(self.parse_value(depth + 1)?);
            self.skip_ws_nl_comments();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(CodecError::parse("expected ',' or ']' in array")),
            }
        }
        Ok(Value::Array(items))
    }

    fn parse_inline_table(&mut self, depth: usize) -> Result<Value, CodecError> {
        self.pos += 1; // '{'
        let mut entries: Vec<(String, Value)> = Vec::new();
        self.skip_ws_nl_comments();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(entries));
        }
        loop {
            self.skip_ws_nl_comments();
            let keypath = self.parse_key_path()?;
            self.expect(b'=')?;
            self.skip_inline_ws();
            let val = self.parse_value(depth + 1)?;
            set_nested(&mut entries, &keypath, val)?;
            self.skip_ws_nl_comments();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(CodecError::parse("expected ',' or '}' in inline table")),
            }
        }
        Ok(Value::Object(entries))
    }

    fn parse_basic_string(&mut self) -> Result<String, CodecError> {
        self.pos += 1; // opening '"'
        let mut out = String::new();
        loop {
            let start = self.pos;
            while !matches!(self.peek(), None | Some(b'"' | b'\\' | b'\n')) {
                self.pos += 1;
            }
            out.push_str(&self.src[start..self.pos]);
            match self.peek() {
                None | Some(b'\n') => return Err(CodecError::parse("unterminated string")),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    self.parse_escape(&mut out)?;
                }
                _ => unreachable!(),
            }
        }
    }

    fn parse_escape(&mut self, out: &mut String) -> Result<(), CodecError> {
        let c = self
            .peek()
            .ok_or_else(|| CodecError::parse("unterminated escape"))?;
        self.pos += 1;
        let ch = match c {
            b'"' => '"',
            b'\\' => '\\',
            b'b' => '\u{8}',
            b't' => '\t',
            b'n' => '\n',
            b'f' => '\u{c}',
            b'r' => '\r',
            b'u' => self.parse_hex(4)?,
            b'U' => self.parse_hex(8)?,
            other => {
                return Err(CodecError::parse(format!(
                    "invalid escape '\\{}'",
                    other as char
                )))
            }
        };
        out.push(ch);
        Ok(())
    }

    fn parse_hex(&mut self, n: usize) -> Result<char, CodecError> {
        if self.pos + n > self.b.len() {
            return Err(CodecError::parse("truncated unicode escape"));
        }
        let hex = &self.src[self.pos..self.pos + n];
        self.pos += n;
        let code = u32::from_str_radix(hex, 16)
            .map_err(|_| CodecError::parse("invalid unicode escape"))?;
        char::from_u32(code).ok_or_else(|| CodecError::parse("invalid unicode scalar"))
    }

    fn parse_literal_string(&mut self) -> Result<String, CodecError> {
        self.pos += 1; // opening '\''
        let start = self.pos;
        while !matches!(self.peek(), None | Some(b'\'' | b'\n')) {
            self.pos += 1;
        }
        if self.peek() != Some(b'\'') {
            return Err(CodecError::parse("unterminated literal string"));
        }
        let s = self.src[start..self.pos].to_string();
        self.pos += 1; // closing '\''
        Ok(s)
    }

    /// Reads a bare value token (number, bool, `inf`/`nan`, or an unrecognized
    /// atom such as a date-time) up to the next value terminator.
    fn parse_atom(&mut self) -> Result<Value, CodecError> {
        let start = self.pos;
        while !matches!(
            self.peek(),
            None | Some(b' ' | b'\t' | b'\r' | b'\n' | b',' | b']' | b'}' | b'#')
        ) {
            self.pos += 1;
        }
        let tok = self.src[start..self.pos].trim();
        if tok.is_empty() {
            return Err(CodecError::parse(format!(
                "expected a value at byte {start}"
            )));
        }
        Ok(atom(tok))
    }
}

/// Interprets a bare token as a scalar [`Value`], falling back to a string for
/// anything not recognized as a bool/number (e.g. a date-time).
fn atom(tok: &str) -> Value {
    match tok {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        "inf" | "+inf" => return Value::Float(f64::INFINITY),
        "-inf" => return Value::Float(f64::NEG_INFINITY),
        "nan" | "+nan" | "-nan" => return Value::Float(f64::NAN),
        _ => {}
    }
    if let Some(v) = radix_int(tok) {
        return v;
    }
    let cleaned = tok.replace('_', "");
    if !cleaned.contains(['.', 'e', 'E']) {
        if let Ok(i) = cleaned.parse::<i64>() {
            return Value::Int(i);
        }
        if let Ok(u) = cleaned.parse::<u64>() {
            return Value::UInt(u);
        }
    }
    if let Ok(f) = cleaned.parse::<f64>() {
        if cleaned.contains(['.', 'e', 'E']) {
            return Value::Float(f);
        }
    }
    Value::Str(tok.to_string())
}

/// Parses `0x`/`0o`/`0b`-prefixed integers, returning `Int`/`UInt`.
fn radix_int(tok: &str) -> Option<Value> {
    let (rest, radix) = if let Some(r) = tok.strip_prefix("0x") {
        (r, 16)
    } else if let Some(r) = tok.strip_prefix("0o") {
        (r, 8)
    } else if let Some(r) = tok.strip_prefix("0b") {
        (r, 2)
    } else {
        return None;
    };
    let digits = rest.replace('_', "");
    if let Ok(i) = i64::from_str_radix(&digits, radix) {
        Some(Value::Int(i))
    } else {
        u64::from_str_radix(&digits, radix).ok().map(Value::UInt)
    }
}

// ---- document tree helpers -----------------------------------------------

/// Walks `path`, descending into existing tables (and into the last element of
/// an array of tables), returning the referenced table. Errors if a segment is
/// a non-table value.
fn resolve<'a>(
    mut cur: &'a mut Vec<(String, Value)>,
    path: &[String],
) -> Result<&'a mut Vec<(String, Value)>, CodecError> {
    for seg in path {
        let idx = cur
            .iter()
            .position(|(k, _)| k == seg)
            .ok_or_else(|| CodecError::parse(format!("no such table '{seg}'")))?;
        cur = descend(&mut cur[idx].1, seg)?;
    }
    Ok(cur)
}

/// Like [`resolve`] but creates missing tables along the way.
fn get_or_create<'a>(
    mut cur: &'a mut Vec<(String, Value)>,
    path: &[String],
) -> Result<&'a mut Vec<(String, Value)>, CodecError> {
    for seg in path {
        let idx = match cur.iter().position(|(k, _)| k == seg) {
            Some(i) => i,
            None => {
                cur.push((seg.clone(), Value::Object(Vec::new())));
                cur.len() - 1
            }
        };
        cur = descend(&mut cur[idx].1, seg)?;
    }
    Ok(cur)
}

/// Returns the table a value refers to: the object itself, or the last element
/// of an array of tables.
fn descend<'a>(v: &'a mut Value, seg: &str) -> Result<&'a mut Vec<(String, Value)>, CodecError> {
    match v {
        Value::Object(o) => Ok(o),
        Value::Array(a) => match a.last_mut() {
            Some(Value::Object(o)) => Ok(o),
            _ => Err(CodecError::parse(format!("'{seg}' is not a table"))),
        },
        _ => Err(CodecError::parse(format!("'{seg}' is not a table"))),
    }
}

/// Appends a fresh table to the array of tables at `path` (`[[path]]`).
fn append_array_table(root: &mut Vec<(String, Value)>, path: &[String]) -> Result<(), CodecError> {
    let (last, parents) = path
        .split_last()
        .ok_or_else(|| CodecError::parse("empty table header"))?;
    let base = get_or_create(root, parents)?;
    let idx = match base.iter().position(|(k, _)| k == last) {
        Some(i) => match &base[i].1 {
            Value::Array(_) => i,
            _ => {
                return Err(CodecError::parse(format!(
                    "'{last}' is not an array of tables"
                )))
            }
        },
        None => {
            base.push((last.clone(), Value::Array(Vec::new())));
            base.len() - 1
        }
    };
    if let Value::Array(a) = &mut base[idx].1 {
        a.push(Value::Object(Vec::new()));
    }
    Ok(())
}

/// Sets `keypath` (relative to `base`, dotted keys create sub-tables) to `val`,
/// erroring on a duplicate definition.
fn set_nested(
    base: &mut Vec<(String, Value)>,
    keypath: &[String],
    val: Value,
) -> Result<(), CodecError> {
    let (last, parents) = keypath
        .split_last()
        .ok_or_else(|| CodecError::parse("empty key"))?;
    let tbl = get_or_create(base, parents)?;
    if tbl.iter().any(|(k, _)| k == last) {
        return Err(CodecError::parse(format!("duplicate key '{last}'")));
    }
    tbl.push((last.clone(), val));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroly::codec::{Decode, Encode};

    #[derive(Encode, Decode, PartialEq, Debug)]
    struct Nested {
        enabled: bool,
        threshold: f64,
    }

    #[derive(Encode, Decode, PartialEq, Debug)]
    struct Config {
        name: String,
        port: u16,
        tags: Vec<String>,
        nested: Nested,
    }

    #[test]
    fn round_trips_nested_struct() {
        let cfg = Config {
            name: "api".into(),
            port: 8080,
            tags: vec!["a".into(), "b".into()],
            nested: Nested {
                enabled: true,
                threshold: 1.5,
            },
        };
        let toml = encode(&cfg);
        let back: Config = decode(&toml).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn parses_handwritten_toml() {
        let toml = r#"
            # a comment
            name = "service"
            port = 9000
            tags = ["x", "y"]

            [nested]
            enabled = false
            threshold = 2.0
        "#;
        let cfg: Config = decode(toml).unwrap();
        assert_eq!(cfg.name, "service");
        assert_eq!(cfg.port, 9000);
        assert_eq!(cfg.tags, vec!["x".to_string(), "y".to_string()]);
        assert!(!cfg.nested.enabled);
    }

    #[test]
    fn round_trips_array_of_tables() {
        #[derive(Encode, Decode, PartialEq, Debug)]
        struct Server {
            host: String,
            port: u16,
        }
        #[derive(Encode, Decode, PartialEq, Debug)]
        struct Cluster {
            servers: Vec<Server>,
        }
        let c = Cluster {
            servers: vec![
                Server {
                    host: "a".into(),
                    port: 1,
                },
                Server {
                    host: "b".into(),
                    port: 2,
                },
            ],
        };
        let toml = encode(&c);
        assert!(toml.contains("[[servers]]"));
        let back: Cluster = decode(&toml).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn parses_array_of_tables_and_dotted_keys() {
        let toml = "\
            [[server]]\n\
            host = \"a\"\n\
            [[server]]\n\
            host = \"b\"\n\
            [db]\n\
            conn.max = 10\n";
        let v = from_str(toml).unwrap();
        let servers = v.get("server").and_then(Value::as_array).unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[1].get("host").and_then(Value::as_str), Some("b"));
        let max = v
            .get("db")
            .and_then(|d| d.get("conn"))
            .and_then(|c| c.get("max"))
            .and_then(Value::as_i64);
        assert_eq!(max, Some(10));
    }

    #[test]
    fn parses_inline_table() {
        let toml = "point = { x = 1, y = -2 }\n";
        let v = from_str(toml).unwrap();
        let p = v.get("point").unwrap();
        assert_eq!(p.get("x").and_then(Value::as_i64), Some(1));
        assert_eq!(p.get("y").and_then(Value::as_i64), Some(-2));
    }

    #[test]
    fn parses_number_forms() {
        let toml = "a = 1_000\nb = 0xFF\nc = 0o17\nd = 0b1010\ne = 1e3\nf = -inf\n";
        let v = from_str(toml).unwrap();
        assert_eq!(v.get("a").and_then(Value::as_i64), Some(1000));
        assert_eq!(v.get("b").and_then(Value::as_i64), Some(255));
        assert_eq!(v.get("c").and_then(Value::as_i64), Some(15));
        assert_eq!(v.get("d").and_then(Value::as_i64), Some(10));
        assert_eq!(v.get("e"), Some(&Value::Float(1000.0)));
        assert_eq!(v.get("f"), Some(&Value::Float(f64::NEG_INFINITY)));
    }

    #[test]
    fn quotes_ambiguous_strings() {
        #[derive(Encode, Decode, PartialEq, Debug)]
        struct W {
            v: String,
        }
        let w = W { v: "true".into() };
        let toml = encode(&w);
        assert!(toml.contains("\"true\""));
        let back: W = decode(&toml).unwrap();
        assert_eq!(back.v, "true");
    }

    #[test]
    fn omits_none_and_decodes_missing_as_none() {
        #[derive(Encode, Decode, PartialEq, Debug)]
        struct O {
            a: i32,
            b: Option<i32>,
        }
        let o = O { a: 1, b: None };
        let toml = encode(&o);
        assert!(!toml.contains('b'));
        let back: O = decode(&toml).unwrap();
        assert_eq!(back, o);

        let some = O { a: 1, b: Some(7) };
        let back: O = decode(&encode(&some)).unwrap();
        assert_eq!(back, some);
    }

    #[test]
    fn round_trips_empty_containers() {
        let cfg = Config {
            name: "api".into(),
            port: 80,
            tags: vec![],
            nested: Nested {
                enabled: false,
                threshold: 0.0,
            },
        };
        let back: Config = decode(&encode(&cfg)).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn rejects_duplicate_keys() {
        assert!(from_str("a = 1\na = 2\n").is_err());
    }

    #[test]
    fn rejects_deeply_nested_input_without_stack_overflow() {
        let n = MAX_DEPTH + 50;
        let mut toml = String::from("a = ");
        toml.push_str(&"[".repeat(n));
        toml.push_str(&"]".repeat(n));
        toml.push('\n');
        assert!(from_str(&toml).is_err());
    }
}

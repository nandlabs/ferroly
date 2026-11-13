//! A dependency-free TOML codec covering the common config subset.
//!
//! Supported: bare/quoted/dotted keys, `key = value` pairs, table headers
//! (`[a.b]`), arrays of tables (`[[a.b]]`), inline tables (`{ a = 1 }`), arrays
//! (`[1, 2, 3]`, may span lines with trailing commas), basic (`"..."`) and
//! literal (`'...'`) strings with the standard escapes, multi-line basic
//! (`"""..."""`) and multi-line literal (`'''...'''`) strings, integers
//! (decimal with `_` separators and `0x`/`0o`/`0b` prefixes), floats (including
//! `inf`/`-inf`/`nan`), booleans, and `#` comments. The encoder emits nested
//! structs as `[section]` headers and `Vec`-of-structs as `[[section]]` arrays
//! of tables.
//!
//! **Date-times are not a native type**: an RFC 3339 date-time, local
//! date-time, local date, or local time token (`1979-05-27T07:32:00Z`,
//! `1979-05-27`, `07:32:00`) decodes as its raw text in a [`Value::Str`], and
//! is not validated beyond its shape. Any other bare token is an error.
//!
//! TOML has no null type, so [`Value::Null`] entries are omitted on encode
//! (a missing key decodes back to `None` for `Option` fields). A `Null` inside
//! an array has no faithful form; [`encode`] writes `""` for it and
//! [`try_encode`] reports an error instead.
//!
//! Typed decoding is **strict**: TOML values are typed, so a string is never
//! coerced into a number or bool.
//!
//! The parser is strict about the document as well. A table is defined once: a
//! repeated `[header]`, a header for a table that dotted keys already built,
//! and dotted keys reaching into a table declared elsewhere are errors, never a
//! silent merge. Line endings are LF or CRLF (a lone carriage return is an
//! error), raw control characters other than tab are rejected in strings and
//! comments, and a CRLF inside a multi-line string decodes to LF. One
//! deliberate leniency: an inline table may span lines and end with a trailing
//! comma.
//!
//! The parser scans the input by byte position and bulk-copies string runs
//! that need no unescaping; table lookups are indexed, so a document with many
//! keys or tables parses in near-linear time. It enforces a [`Limits`] budget —
//! the depth limit covers dotted keys and table headers as well as nested
//! arrays and inline tables — and reports the line and column of errors.

use std::fmt::Write as _;

use ferroly::codec::{from_value_strict, CodecError, Decode, Encode, Limits, Value};

use super::fmt;
use super::limits::{depth_error, error_at, Budget, KeyIndex};

/// Encodes any [`Encode`] type to a TOML string.
///
/// A TOML document is always a table, so a bare scalar or array is wrapped
/// under the key `value`; a `Null` inside an array is written as `""`. Use
/// [`try_encode`] to get an error for either instead.
pub fn encode<T: Encode + ?Sized>(value: &T) -> String {
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

/// Like [`encode`], but fails for values TOML cannot represent: a top-level
/// value that is not a table, or a `Null` inside an array.
pub fn try_encode<T: Encode + ?Sized>(value: &T) -> Result<String, CodecError> {
    let value = value.encode();
    let entries = match &value {
        Value::Object(entries) => entries,
        _ => {
            return Err(CodecError::unrepresentable(
                "a TOML document must be a table at the top level",
            ))
        }
    };
    if entries.iter().any(|(_, v)| has_null_in_array(v)) {
        return Err(CodecError::unrepresentable(
            "TOML has no null, so an array cannot hold one",
        ));
    }
    let mut out = String::with_capacity(64);
    write_table(&mut out, entries, "");
    Ok(out)
}

/// Whether `v` contains an array with a `Null` element anywhere inside it.
fn has_null_in_array(v: &Value) -> bool {
    match v {
        Value::Array(a) => a.iter().any(|e| e.is_null() || has_null_in_array(e)),
        Value::Object(o) => o.iter().any(|(_, e)| has_null_in_array(e)),
        _ => false,
    }
}

/// Encodes to TOML bytes.
pub fn encode_to_vec<T: Encode + ?Sized>(value: &T) -> Vec<u8> {
    encode(value).into_bytes()
}

/// Decodes any [`Decode`] type from a TOML string.
pub fn decode<T: Decode>(input: &str) -> Result<T, CodecError> {
    decode_with(input, Limits::DEFAULT)
}

/// Like [`decode`], with an explicit parser budget.
pub fn decode_with<T: Decode>(input: &str, limits: Limits) -> Result<T, CodecError> {
    from_value_strict(&from_str_with(input, limits)?)
}

/// Decodes from TOML bytes.
pub fn decode_from_slice<T: Decode>(input: &[u8]) -> Result<T, CodecError> {
    decode_from_slice_with(input, Limits::DEFAULT)
}

/// Like [`decode_from_slice`], with an explicit parser budget.
pub fn decode_from_slice_with<T: Decode>(input: &[u8], limits: Limits) -> Result<T, CodecError> {
    let s = std::str::from_utf8(input).map_err(|e| CodecError::parse(e.to_string()))?;
    decode_with(s, limits)
}

/// Parses a TOML document into a [`Value::Object`].
pub fn from_str(input: &str) -> Result<Value, CodecError> {
    from_str_with(input, Limits::DEFAULT)
}

/// Like [`from_str`], with an explicit parser budget.
pub fn from_str_with(input: &str, limits: Limits) -> Result<Value, CodecError> {
    Parser::new(input, limits)?.parse()
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
        fmt::write_finite_f64(out, f);
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
            c if c < 0x20 || c == 0x7f => {
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

// ---- document tree -------------------------------------------------------

/// A node of the document while it is being built. Tables carry a key index so
/// lookups stay O(1); the tree is converted to a [`Value`] once parsing ends.
enum Node {
    /// A finished value: a scalar, an array, or an inline table (which TOML
    /// does not allow to be extended afterwards).
    Value(Value),
    /// A table defined by a header or by dotted keys.
    Table(Table),
    /// An array of tables (`[[name]]`).
    Tables(Vec<Table>),
}

/// The section a table that exists only implicitly (as a parent in some
/// header's path) belongs to: none yet.
const IMPLICIT: usize = 0;

#[derive(Default)]
struct Table {
    entries: Vec<(String, Node)>,
    index: KeyIndex,
    /// The section — the run of key/value lines under one header — that
    /// declared this table with `[header]` or built it with dotted keys, or
    /// [`IMPLICIT`]. TOML lets a table be defined only once: a second header
    /// for it, or dotted keys reaching into it from another section, is an
    /// error rather than a silent merge.
    owner: usize,
}

impl Table {
    fn position(&mut self, key: &str) -> Option<usize> {
        self.index.find(&self.entries, key)
    }

    fn push(&mut self, key: String, node: Node) -> usize {
        let idx = self.entries.len();
        self.index.inserted(&key, idx);
        self.entries.push((key, node));
        idx
    }

    /// Walks a header path from this table, creating implicit tables for
    /// missing segments (when `create`) and stepping into the last element of
    /// an array of tables. `Err` carries a message naming the offending key.
    fn descend(&mut self, path: &[String], create: bool) -> Result<&mut Table, String> {
        let mut cur = self;
        for key in path {
            let idx = match cur.position(key) {
                Some(i) => i,
                None if create => cur.push(key.clone(), Node::Table(Table::default())),
                None => return Err(format!("no such table '{key}'")),
            };
            cur = match &mut cur.entries[idx].1 {
                Node::Table(t) => t,
                Node::Tables(v) => match v.last_mut() {
                    Some(t) => t,
                    None => return Err(format!("'{key}' is not a table")),
                },
                Node::Value(_) => return Err(format!("'{key}' is not a table")),
            };
        }
        Ok(cur)
    }

    /// Steps into the sub-table a dotted key names, creating it when absent.
    /// Only a table that is still implicit, or that this same `section` is
    /// building, may be extended this way.
    fn dotted_child(&mut self, key: &str, section: usize) -> Result<&mut Table, String> {
        let idx = match self.position(key) {
            Some(i) => i,
            None => self.push(key.to_string(), Node::Table(Table::default())),
        };
        match &mut self.entries[idx].1 {
            Node::Table(t) if t.owner == IMPLICIT || t.owner == section => {
                t.owner = section;
                Ok(t)
            }
            Node::Table(_) => Err(format!(
                "table '{key}' is already defined; a dotted key cannot add to it here"
            )),
            _ => Err(format!("'{key}' is not a table")),
        }
    }

    /// Sets `keypath` (dotted keys create sub-tables) to `value`, erroring on a
    /// duplicate definition.
    fn set(&mut self, keypath: &[String], value: Value, section: usize) -> Result<(), String> {
        let (last, parents) = match keypath.split_last() {
            Some(split) => split,
            None => return Err("empty key".to_string()),
        };
        let mut table = self;
        for key in parents {
            table = table.dotted_child(key, section)?;
        }
        if table.position(last).is_some() {
            return Err(format!("duplicate key '{last}'"));
        }
        table.push(last.clone(), Node::Value(value));
        Ok(())
    }

    /// Declares the table at `path` (`[path]`) as belonging to `section`.
    fn declare(&mut self, path: &[String], section: usize) -> Result<(), String> {
        let (last, parents) = match path.split_last() {
            Some(split) => split,
            None => return Err("empty table header".to_string()),
        };
        let base = self.descend(parents, true)?;
        let idx = match base.position(last) {
            Some(i) => i,
            None => base.push(last.clone(), Node::Table(Table::default())),
        };
        match &mut base.entries[idx].1 {
            Node::Table(t) if t.owner == IMPLICIT => {
                t.owner = section;
                Ok(())
            }
            Node::Table(_) => Err(format!("table '{last}' is defined more than once")),
            Node::Tables(_) => Err(format!(
                "'{last}' is an array of tables; add to it with [[...]]"
            )),
            Node::Value(_) => Err(format!("'{last}' is not a table")),
        }
    }

    /// Appends a fresh table to the array of tables at `path` (`[[path]]`).
    fn append_array_table(&mut self, path: &[String], section: usize) -> Result<(), String> {
        let (last, parents) = match path.split_last() {
            Some(split) => split,
            None => return Err("empty table header".to_string()),
        };
        let base = self.descend(parents, true)?;
        let idx = match base.position(last) {
            Some(i) => i,
            None => base.push(last.clone(), Node::Tables(Vec::new())),
        };
        match &mut base.entries[idx].1 {
            Node::Tables(v) => {
                v.push(Table {
                    owner: section,
                    ..Table::default()
                });
                Ok(())
            }
            _ => Err(format!("'{last}' is not an array of tables")),
        }
    }

    /// Converts the finished tree into a [`Value`]. Recursion is bounded by the
    /// depth limit enforced while the tree was built.
    fn into_value(self) -> Value {
        Value::Object(
            self.entries
                .into_iter()
                .map(|(k, node)| {
                    let v = match node {
                        Node::Value(v) => v,
                        Node::Table(t) => t.into_value(),
                        Node::Tables(v) => {
                            Value::Array(v.into_iter().map(Table::into_value).collect())
                        }
                    };
                    (k, v)
                })
                .collect(),
        )
    }
}

// ---- parser --------------------------------------------------------------

/// The characters TOML forbids raw in strings and comments: every control
/// character except tab. (Line breaks are handled by the callers.)
fn is_control(c: u8) -> bool {
    (c < 0x20 && c != b'\t') || c == 0x7f
}

struct Parser<'a> {
    src: &'a str,
    b: &'a [u8],
    pos: usize,
    /// Entry counter and depth cap from the caller's [`Limits`]. Depth is
    /// tracked explicitly (table path + dotted key + value nesting).
    budget: Budget,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str, limits: Limits) -> Result<Self, CodecError> {
        Ok(Parser {
            src,
            b: src.as_bytes(),
            // Tolerate a leading byte-order mark (three bytes in UTF-8).
            pos: if src.starts_with('\u{feff}') { 3 } else { 0 },
            budget: Budget::new(limits, src.len())?,
        })
    }

    fn err(&self, msg: impl std::fmt::Display) -> CodecError {
        error_at(self.src, self.pos, msg)
    }

    fn err_at(&self, pos: usize, msg: impl std::fmt::Display) -> CodecError {
        error_at(self.src, pos, msg)
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn starts_with(&self, s: &str) -> bool {
        self.b[self.pos..].starts_with(s.as_bytes())
    }

    /// The length of the line break at the cursor: 1 for LF, 2 for CRLF. A
    /// carriage return on its own is not a line break (and not whitespace).
    fn newline_len(&self) -> Option<usize> {
        match self.peek() {
            Some(b'\n') => Some(1),
            Some(b'\r') if self.b.get(self.pos + 1) == Some(&b'\n') => Some(2),
            _ => None,
        }
    }

    /// Consumes spaces and tabs (not line breaks).
    fn skip_inline_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.pos += 1;
        }
    }

    /// Consumes a `#` comment up to (not including) the end of its line.
    fn skip_comment(&mut self) -> Result<(), CodecError> {
        loop {
            match self.peek() {
                None => return Ok(()),
                Some(_) if self.newline_len().is_some() => return Ok(()),
                Some(c) if is_control(c) => {
                    return Err(self.err("control character in a comment"));
                }
                Some(_) => self.pos += 1,
            }
        }
    }

    /// Consumes whitespace, line breaks, and `#` comments — used between
    /// statements and inside arrays / inline tables.
    fn skip_ws_nl_comments(&mut self) -> Result<(), CodecError> {
        loop {
            match self.peek() {
                Some(b' ' | b'\t') => self.pos += 1,
                Some(b'#') => self.skip_comment()?,
                Some(_) => match self.newline_len() {
                    Some(n) => self.pos += n,
                    None => return Ok(()),
                },
                None => return Ok(()),
            }
        }
    }

    fn parse(&mut self) -> Result<Value, CodecError> {
        let mut root = Table::default();
        // The path of the table the following key/value lines belong to, and
        // the number of the section they form (the root section is 1).
        let mut current: Vec<String> = Vec::new();
        let mut section = 1usize;
        loop {
            self.skip_ws_nl_comments()?;
            match self.peek() {
                None => break,
                Some(b'[') => {
                    let at = self.pos;
                    let is_array = self.b.get(self.pos + 1) == Some(&b'[');
                    self.pos += if is_array { 2 } else { 1 };
                    let path = self.parse_key_path(0)?;
                    self.expect(b']')?;
                    if is_array {
                        // The closing brackets must be adjacent.
                        if self.peek() != Some(b']') {
                            return Err(self.err("expected ']]'"));
                        }
                        self.pos += 1;
                    }
                    self.budget.entry()?;
                    section += 1;
                    let declared = if is_array {
                        root.append_array_table(&path, section)
                    } else {
                        root.declare(&path, section)
                    };
                    declared.map_err(|m| self.err_at(at, m))?;
                    current = path;
                    self.finish_line()?;
                }
                Some(_) => {
                    // Resolve the current table once, then read every key/value
                    // line that follows — not once per line.
                    let at = self.pos;
                    let table = root
                        .descend(&current, false)
                        .map_err(|m| error_at(self.src, at, m))?;
                    loop {
                        let at = self.pos;
                        let keypath = self.parse_key_path(current.len())?;
                        self.skip_inline_ws();
                        self.expect(b'=')?;
                        self.skip_inline_ws();
                        let value = self.parse_value(current.len() + keypath.len())?;
                        self.budget.entry()?;
                        table
                            .set(&keypath, value, section)
                            .map_err(|m| error_at(self.src, at, m))?;
                        self.finish_line()?;
                        self.skip_ws_nl_comments()?;
                        if matches!(self.peek(), None | Some(b'[')) {
                            break;
                        }
                    }
                }
            }
        }
        Ok(root.into_value())
    }

    /// Expects the next non-whitespace byte on this line to be `c` and
    /// consumes it.
    fn expect(&mut self, c: u8) -> Result<(), CodecError> {
        self.skip_inline_ws();
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.err(format!("expected '{}'", c as char)))
        }
    }

    /// After a statement: optional trailing comment, then a line break or EOF.
    fn finish_line(&mut self) -> Result<(), CodecError> {
        self.skip_inline_ws();
        if self.peek() == Some(b'#') {
            self.skip_comment()?;
        }
        match (self.peek(), self.newline_len()) {
            (None, _) => Ok(()),
            (_, Some(n)) => {
                self.pos += n;
                Ok(())
            }
            _ => Err(self.err("unexpected trailing input")),
        }
    }

    /// Parses a dotted key path (`a.b.c`), each segment bare or quoted. `base`
    /// is the depth the path starts at; a path that would nest deeper than the
    /// depth limit is rejected here, before any table is built for it.
    fn parse_key_path(&mut self, base: usize) -> Result<Vec<String>, CodecError> {
        let mut path = Vec::new();
        loop {
            self.skip_inline_ws();
            if base + path.len() >= self.budget.max_depth() {
                return Err(depth_error(self.budget.max_depth()));
            }
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
                    return Err(self.err("expected a key"));
                }
                Ok(self.src[start..self.pos].to_string())
            }
        }
    }

    /// Parses the value at a key path of `depth` segments (so `depth` is the
    /// nesting level the value itself sits at, counted like JSON nesting).
    fn parse_value(&mut self, depth: usize) -> Result<Value, CodecError> {
        self.skip_inline_ws();
        match self.peek() {
            Some(b'"') => {
                if self.starts_with("\"\"\"") {
                    Ok(Value::Str(self.parse_multiline(b'"')?))
                } else {
                    Ok(Value::Str(self.parse_basic_string()?))
                }
            }
            Some(b'\'') => {
                if self.starts_with("'''") {
                    Ok(Value::Str(self.parse_multiline(b'\'')?))
                } else {
                    Ok(Value::Str(self.parse_literal_string()?))
                }
            }
            Some(b'[') => self.parse_array(depth),
            Some(b'{') => self.parse_inline_table(depth),
            None => Err(self.err("expected a value")),
            Some(_) => self.parse_atom(),
        }
    }

    /// Rejects a container that would sit deeper than the depth limit.
    fn check_depth(&self, depth: usize) -> Result<(), CodecError> {
        if depth >= self.budget.max_depth() {
            return Err(depth_error(self.budget.max_depth()));
        }
        Ok(())
    }

    fn parse_array(&mut self, depth: usize) -> Result<Value, CodecError> {
        self.check_depth(depth)?;
        self.pos += 1; // '['
        let mut items = Vec::new();
        loop {
            self.skip_ws_nl_comments()?;
            if self.peek() == Some(b']') {
                self.pos += 1;
                break;
            }
            self.budget.entry()?;
            items.push(self.parse_value(depth + 1)?);
            self.skip_ws_nl_comments()?;
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

    fn parse_inline_table(&mut self, depth: usize) -> Result<Value, CodecError> {
        self.check_depth(depth)?;
        self.pos += 1; // '{'
        let mut table = Table::default();
        self.skip_ws_nl_comments()?;
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(Vec::new()));
        }
        loop {
            self.skip_ws_nl_comments()?;
            let at = self.pos;
            let keypath = self.parse_key_path(depth)?;
            self.expect(b'=')?;
            self.skip_inline_ws();
            let value = self.parse_value(depth + keypath.len())?;
            self.budget.entry()?;
            // An inline table is one self-contained section.
            table
                .set(&keypath, value, 1)
                .map_err(|m| self.err_at(at, m))?;
            self.skip_ws_nl_comments()?;
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.err("expected ',' or '}' in inline table")),
            }
        }
        Ok(table.into_value())
    }

    /// Parses a single-line string delimited by `quote`: a basic string
    /// (`"`, with escapes) or a literal string (`'`, without).
    fn parse_single_line(&mut self, quote: u8) -> Result<String, CodecError> {
        let open = self.pos;
        self.pos += 1; // opening quote
        let mut out = String::new();
        let mut run = self.pos;
        loop {
            match self.peek() {
                Some(c) if c == quote => {
                    out.push_str(&self.src[run..self.pos]);
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') if quote == b'"' => {
                    out.push_str(&self.src[run..self.pos]);
                    self.pos += 1;
                    out.push(self.parse_escape()?);
                    run = self.pos;
                }
                None | Some(b'\n' | b'\r') => {
                    return Err(self.err_at(open, "unterminated string"));
                }
                Some(c) if is_control(c) => {
                    return Err(self.err("control character in a string (use an escape)"));
                }
                Some(_) => self.pos += 1,
            }
        }
    }

    fn parse_basic_string(&mut self) -> Result<String, CodecError> {
        self.parse_single_line(b'"')
    }

    fn parse_literal_string(&mut self) -> Result<String, CodecError> {
        self.parse_single_line(b'\'')
    }

    /// Parses a multi-line string delimited by three `quote`s. A line break
    /// right after the opening delimiter is dropped, and every line break in
    /// the content is normalized to LF (so a file saved with CRLF endings
    /// decodes to the same text). In the basic form (`"""`) escapes apply and
    /// a backslash at the end of a line swallows the whitespace that follows;
    /// the literal form (`'''`) has no escapes.
    fn parse_multiline(&mut self, quote: u8) -> Result<String, CodecError> {
        let open = self.pos;
        self.pos += 3;
        if let Some(n) = self.newline_len() {
            self.pos += n;
        }
        let mut out = String::new();
        let mut run = self.pos;
        loop {
            match self.peek() {
                None => return Err(self.err_at(open, "unterminated multi-line string")),
                Some(c) if c == quote => {
                    out.push_str(&self.src[run..self.pos]);
                    let closed = self.close_multiline(quote, &mut out)?;
                    if closed {
                        return Ok(out);
                    }
                    run = self.pos;
                }
                Some(b'\\') if quote == b'"' => {
                    out.push_str(&self.src[run..self.pos]);
                    self.pos += 1;
                    if self.line_ending_backslash() {
                        // Trim the line break and all whitespace that follows.
                        loop {
                            match (self.peek(), self.newline_len()) {
                                (Some(b' ' | b'\t'), _) => self.pos += 1,
                                (_, Some(n)) => self.pos += n,
                                _ => break,
                            }
                        }
                    } else {
                        out.push(self.parse_escape()?);
                    }
                    run = self.pos;
                }
                Some(b'\n') => self.pos += 1,
                Some(b'\r') => {
                    if self.newline_len().is_none() {
                        return Err(self.err("a carriage return must be followed by a line feed"));
                    }
                    // CRLF -> LF.
                    out.push_str(&self.src[run..self.pos]);
                    out.push('\n');
                    self.pos += 2;
                    run = self.pos;
                }
                Some(c) if is_control(c) => {
                    return Err(self.err("control character in a string (use an escape)"));
                }
                Some(_) => self.pos += 1,
            }
        }
    }

    /// Whether the backslash just consumed ends its line (only spaces and tabs
    /// up to the line break).
    fn line_ending_backslash(&self) -> bool {
        let mut i = self.pos;
        while matches!(self.b.get(i), Some(b' ' | b'\t')) {
            i += 1;
        }
        match self.b.get(i) {
            Some(b'\n') => true,
            Some(b'\r') => self.b.get(i + 1) == Some(&b'\n'),
            _ => false,
        }
    }

    /// Handles a run of quote characters inside a multi-line string. Fewer
    /// than three are content; three close the string, and up to two more
    /// directly before the delimiter are content (`""""x""""` is `"x"`).
    /// Returns `true` once the string is closed.
    fn close_multiline(&mut self, quote: u8, out: &mut String) -> Result<bool, CodecError> {
        let start = self.pos;
        while self.peek() == Some(quote) {
            self.pos += 1;
        }
        let run = self.pos - start;
        let content = match run {
            0..=2 => run,
            3..=5 => run - 3,
            _ => return Err(self.err_at(start, "too many quotes in a multi-line string")),
        };
        for _ in 0..content {
            out.push(quote as char);
        }
        Ok(run >= 3)
    }

    /// Decodes one escape; the cursor is just past the backslash.
    fn parse_escape(&mut self) -> Result<char, CodecError> {
        let at = self.pos.saturating_sub(1);
        let c = self
            .peek()
            .ok_or_else(|| self.err_at(at, "unterminated escape"))?;
        self.pos += 1;
        Ok(match c {
            b'"' => '"',
            b'\\' => '\\',
            b'b' => '\u{8}',
            b't' => '\t',
            b'n' => '\n',
            b'f' => '\u{c}',
            b'r' => '\r',
            b'u' => self.parse_hex(4, at)?,
            b'U' => self.parse_hex(8, at)?,
            _ => return Err(self.err_at(at, "invalid escape sequence")),
        })
    }

    /// Reads exactly `n` hex digits as **bytes** (so a multibyte character
    /// after a short escape cannot be split) and nothing but hex digits (so a
    /// sign is not accepted).
    fn parse_hex(&mut self, n: usize, at: usize) -> Result<char, CodecError> {
        let digits = self
            .b
            .get(self.pos..self.pos + n)
            .filter(|d| d.iter().all(u8::is_ascii_hexdigit));
        let digits = match digits {
            Some(d) => d,
            None => {
                return Err(self.err_at(
                    at,
                    format!("unicode escape needs exactly {n} hexadecimal digits"),
                ))
            }
        };
        let mut code = 0u32;
        for &d in digits {
            code = code * 16 + (d as char).to_digit(16).unwrap_or(0);
        }
        self.pos += n;
        char::from_u32(code).ok_or_else(|| self.err_at(at, "invalid unicode scalar value"))
    }

    /// Reads a bare value token — a number, a bool, `inf`/`nan`, or a
    /// date-time — up to the next value terminator.
    fn parse_atom(&mut self) -> Result<Value, CodecError> {
        let start = self.pos;
        self.skip_atom();
        // A date and a time may be separated by a single space instead of `T`.
        if is_date(&self.b[start..self.pos])
            && self.peek() == Some(b' ')
            && matches!(self.b.get(self.pos + 1), Some(b'0'..=b'9'))
            && matches!(self.b.get(self.pos + 3), Some(b':'))
        {
            self.pos += 1;
            self.skip_atom();
        }
        let tok = &self.src[start..self.pos];
        if tok.is_empty() {
            return Err(self.err("expected a value"));
        }
        atom(tok).map_err(|m| self.err_at(start, m))
    }

    fn skip_atom(&mut self) {
        while !matches!(
            self.peek(),
            None | Some(b' ' | b'\t' | b'\r' | b'\n' | b',' | b']' | b'}' | b'#')
        ) {
            self.pos += 1;
        }
    }
}

/// Interprets a bare token as a scalar [`Value`]. A date-time token becomes a
/// string; anything unrecognized is an error (TOML has no bare strings).
fn atom(tok: &str) -> Result<Value, String> {
    match tok {
        "true" => return Ok(Value::Bool(true)),
        "false" => return Ok(Value::Bool(false)),
        "inf" | "+inf" => return Ok(Value::Float(f64::INFINITY)),
        "-inf" => return Ok(Value::Float(f64::NEG_INFINITY)),
        "nan" | "+nan" | "-nan" => return Ok(Value::Float(f64::NAN)),
        _ => {}
    }
    if let Some(v) = radix_int(tok) {
        return v;
    }
    if let Some(v) = decimal_number(tok) {
        return v;
    }
    if is_date_time(tok.as_bytes()) {
        return Ok(Value::Str(tok.to_string()));
    }
    Err(format!("invalid value '{tok}' (strings must be quoted)"))
}

/// Validates a digit group that may use `_` separators: it must start and end
/// with a digit and never have two underscores in a row. Returns the digits
/// with the separators removed.
fn digit_group(s: &str, is_digit: fn(&u8) -> bool) -> Option<String> {
    let b = s.as_bytes();
    if b.is_empty() || !is_digit(&b[0]) || !is_digit(&b[b.len() - 1]) {
        return None;
    }
    let mut out = String::with_capacity(b.len());
    let mut prev_underscore = false;
    for c in b {
        if *c == b'_' {
            if prev_underscore {
                return None;
            }
            prev_underscore = true;
        } else if is_digit(c) {
            prev_underscore = false;
            out.push(*c as char);
        } else {
            return None;
        }
    }
    Some(out)
}

fn is_bin_digit(c: &u8) -> bool {
    matches!(c, b'0' | b'1')
}

fn is_oct_digit(c: &u8) -> bool {
    matches!(c, b'0'..=b'7')
}

/// Parses `0x`/`0o`/`0b`-prefixed integers. `None` when `tok` has no such
/// prefix; `Some(Err)` when it has one but is malformed or out of range.
fn radix_int(tok: &str) -> Option<Result<Value, String>> {
    let (rest, radix, is_digit): (&str, u32, fn(&u8) -> bool) =
        if let Some(r) = tok.strip_prefix("0x") {
            (r, 16, u8::is_ascii_hexdigit)
        } else if let Some(r) = tok.strip_prefix("0o") {
            (r, 8, is_oct_digit)
        } else {
            (tok.strip_prefix("0b")?, 2, is_bin_digit)
        };
    let parsed = digit_group(rest, is_digit).and_then(|digits| {
        if let Ok(i) = i64::from_str_radix(&digits, radix) {
            Some(Value::Int(i))
        } else {
            u64::from_str_radix(&digits, radix).ok().map(Value::UInt)
        }
    });
    Some(parsed.ok_or_else(|| format!("invalid integer '{tok}'")))
}

/// Parses a decimal integer or float by the TOML grammar. `None` when `tok`
/// does not look like a number at all (so it may still be a date-time).
fn decimal_number(tok: &str) -> Option<Result<Value, String>> {
    let unsigned = tok.strip_prefix(['+', '-']).unwrap_or(tok);
    if !unsigned.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    // Anything with `-` or `:` past the sign is a date/time shape, not a number
    // (an exponent sign is the one exception, and it always follows `e`/`E`).
    let b = unsigned.as_bytes();
    for (i, c) in b.iter().enumerate() {
        match c {
            b':' => return None,
            b'-' | b'+' if !matches!(b[i - 1], b'e' | b'E') => return None,
            _ => {}
        }
    }
    Some(parse_decimal(tok, unsigned))
}

fn parse_decimal(tok: &str, unsigned: &str) -> Result<Value, String> {
    let invalid = || format!("invalid number '{tok}'");
    let negative = tok.starts_with('-');
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(i) => (&unsigned[..i], Some(&unsigned[i + 1..])),
        None => (unsigned, None),
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (mantissa, None),
    };
    let int_digits = digit_group(int_part, u8::is_ascii_digit).ok_or_else(invalid)?;
    // No leading zeros: `0` is fine, `01` is not.
    if int_digits.len() > 1 && int_digits.starts_with('0') {
        return Err(invalid());
    }
    if frac_part.is_none() && exponent.is_none() {
        let signed = if negative {
            format!("-{int_digits}")
        } else {
            int_digits
        };
        if let Ok(i) = signed.parse::<i64>() {
            return Ok(Value::Int(i));
        }
        return signed
            .parse::<u64>()
            .map(Value::UInt)
            .map_err(|_| format!("integer '{tok}' is out of range"));
    }
    let mut text = String::with_capacity(tok.len());
    if negative {
        text.push('-');
    }
    text.push_str(&int_digits);
    if let Some(frac) = frac_part {
        text.push('.');
        text.push_str(&digit_group(frac, u8::is_ascii_digit).ok_or_else(invalid)?);
    }
    if let Some(exp) = exponent {
        text.push('e');
        let digits = match exp.strip_prefix(['+', '-']) {
            Some(d) => {
                text.push_str(&exp[..1]);
                d
            }
            None => exp,
        };
        text.push_str(&digit_group(digits, u8::is_ascii_digit).ok_or_else(invalid)?);
    }
    // A literal that overflows to infinity is rejected rather than silently
    // becoming `inf`, which TOML spells out explicitly.
    match text.parse::<f64>() {
        Ok(f) if f.is_finite() => Ok(Value::Float(f)),
        Ok(_) => Err(format!("number '{tok}' is out of range")),
        Err(_) => Err(invalid()),
    }
}

/// `n` ASCII digits at the start of `b`.
fn digits_prefix(b: &[u8], n: usize) -> bool {
    b.len() >= n && b[..n].iter().all(u8::is_ascii_digit)
}

/// `YYYY-MM-DD` exactly.
fn is_date(b: &[u8]) -> bool {
    b.len() == 10 && date_len(b) == Some(10)
}

fn date_len(b: &[u8]) -> Option<usize> {
    let ok = digits_prefix(b, 4)
        && b.get(4) == Some(&b'-')
        && digits_prefix(b.get(5..)?, 2)
        && b.get(7) == Some(&b'-')
        && digits_prefix(b.get(8..)?, 2);
    if ok {
        Some(10)
    } else {
        None
    }
}

/// `HH:MM[:SS[.fraction]]`, returning its length.
fn time_len(b: &[u8]) -> Option<usize> {
    if !(digits_prefix(b, 2) && b.get(2) == Some(&b':') && digits_prefix(b.get(3..)?, 2)) {
        return None;
    }
    let mut i = 5;
    if b.get(i) == Some(&b':') {
        if !digits_prefix(b.get(i + 1..)?, 2) {
            return None;
        }
        i += 3;
        if b.get(i) == Some(&b'.') {
            let frac = b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
            if frac == 0 {
                return None;
            }
            i += 1 + frac;
        }
    }
    Some(i)
}

/// Whether `b` has the shape of a TOML date-time: an offset or local
/// date-time, a local date, or a local time. Only the shape is checked — field
/// ranges (month 13, hour 25) are not.
fn is_date_time(b: &[u8]) -> bool {
    if let Some(n) = time_len(b) {
        return n == b.len();
    }
    let mut i = match date_len(b) {
        Some(n) => n,
        None => return false,
    };
    if i == b.len() {
        return true;
    }
    if !matches!(b[i], b'T' | b't' | b' ') {
        return false;
    }
    i += 1;
    match time_len(&b[i..]) {
        Some(n) => i += n,
        None => return false,
    }
    match b.get(i) {
        None => true,
        Some(b'Z' | b'z') => i + 1 == b.len(),
        Some(b'+' | b'-') => {
            let off = &b[i + 1..];
            off.len() == 5 && digits_prefix(off, 2) && off[2] == b':' && digits_prefix(&off[3..], 2)
        }
        Some(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroly::codec::{Decode, Encode};

    const MAX_DEPTH: usize = Limits::DEFAULT_MAX_DEPTH;

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
        assert!(from_str("a.b = 1\na.b = 2\n").is_err());
        assert!(from_str("a = 1\na.b = 2\n").is_err());
        assert!(from_str("t = { a = 1, a = 2 }\n").is_err());
        assert!(from_str("a = { b = 1 }\n[a.c]\n").is_err());
        assert!(from_str("a = 1\n[[a]]\n").is_err());
        assert!(from_str("[a]\n[[a]]\n").is_err());
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

    #[test]
    fn unicode_escapes_read_bytes_not_chars() {
        // A multibyte character right after a short escape used to slice the
        // input in the middle of a character and panic.
        for bad in [
            "a = \"\\u000é\"",
            "a = \"\\u00é\"",
            "a = \"\\U0000000é\"",
            "a = \"\\U000é\"",
            "a = \"\\u\"",
            "a = \"\\u12\"",
            "a = \"\\U0001F60\"",
            "a = \"\\uD800\"",
            "a = \"\\U00110000\"",
            // `from_str_radix` accepts a sign; an escape must not.
            "a = \"\\u+041\"",
            "a = \"\\u-041\"",
            "a = \"\\U+0000041\"",
            "a = \"\\q\"",
            "a = \"\\",
            "\"\\u000é\" = 1",
        ] {
            let r = from_str(bad);
            assert!(r.is_err(), "{bad:?} must be rejected, got {r:?}");
        }
        assert_eq!(
            from_str("a = \"\\u00e9\\U0001F600\\t\"").unwrap().get("a"),
            Some(&Value::Str("é😀\t".into()))
        );
    }

    #[test]
    fn depth_limit_covers_dotted_keys_and_table_headers() {
        // 100k segments used to build a 100k-deep tree whose recursive drop
        // overflowed the stack. Now the path is rejected while it is parsed.
        let deep = vec!["a"; 100_000].join(".");
        for doc in [
            format!("{deep} = 1\n"),
            format!("[{deep}]\n"),
            format!("[[{deep}]]\n"),
            format!("t = {{ {deep} = 1 }}\n"),
        ] {
            assert!(matches!(from_str(&doc), Err(CodecError::LimitExceeded(_))));
        }
        // Depth adds up across the table path, the dotted key, and the value.
        let limits = Limits::new().with_max_depth(4);
        assert!(from_str_with("[a.b]\nc.d = 1\n", limits).is_ok());
        assert!(from_str_with("[a.b]\nc.d.e = 1\n", limits).is_err());
        assert!(from_str_with("[a.b]\nc = [1]\n", limits).is_ok());
        assert!(from_str_with("[a.b]\nc = [[1]]\n", limits).is_err());
        assert!(from_str_with("[a.b]\nc = { d = 1 }\n", limits).is_ok());
        assert!(from_str_with("[a.b]\nc = { d = { e = 1 } }\n", limits).is_err());
        assert!(from_str_with("[a.b.c.d]\n", limits).is_ok());
        assert!(from_str_with("[a.b.c.d.e]\n", limits).is_err());
        // The default limit admits exactly MAX_DEPTH segments.
        let ok = vec!["a"; MAX_DEPTH].join(".");
        assert!(from_str(&format!("{ok} = 1\n")).is_ok());
        let over = vec!["a"; MAX_DEPTH + 1].join(".");
        assert!(from_str(&format!("{over} = 1\n")).is_err());
    }

    #[test]
    fn multi_line_strings() {
        let v = from_str(
            "a = \"\"\"\nline 1\nline 2\"\"\"\n\
             b = '''\nraw \\n text\n  kept'''\n\
             c = \"\"\"one \\\n      two\"\"\"\n\
             d = \"\"\"x\"\"y\"\"\"\n\
             e = \"\"\"\"quoted\"\"\"\"\n\
             f = '''it's'''\n\
             g = \"\"\"\"\"\"\n\
             h = \"\"\"tab\\there \\u00e9\"\"\"\n\
             i = '''''two'''''\n",
        )
        .unwrap();
        assert_eq!(v.get("a").and_then(Value::as_str), Some("line 1\nline 2"));
        assert_eq!(
            v.get("b").and_then(Value::as_str),
            Some("raw \\n text\n  kept")
        );
        assert_eq!(v.get("c").and_then(Value::as_str), Some("one two"));
        assert_eq!(v.get("d").and_then(Value::as_str), Some("x\"\"y"));
        assert_eq!(v.get("e").and_then(Value::as_str), Some("\"quoted\""));
        assert_eq!(v.get("f").and_then(Value::as_str), Some("it's"));
        assert_eq!(v.get("g").and_then(Value::as_str), Some(""));
        assert_eq!(v.get("h").and_then(Value::as_str), Some("tab\there é"));
        assert_eq!(v.get("i").and_then(Value::as_str), Some("''two''"));
        for bad in [
            "a = \"\"\"never closed",
            "a = '''never closed",
            "a = \"\"\"x\"\"\"\"\"\"\"",
            "a = \"\"\"bad \\q\"\"\"",
            "a = \"\"\"x\"\"\" y",
        ] {
            assert!(from_str(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn date_times_decode_as_strings_and_bare_words_are_rejected() {
        let v = from_str(
            "a = 1979-05-27T07:32:00Z\n\
             b = 1979-05-27T00:32:00.999999-07:00\n\
             c = 1979-05-27 07:32:00\n\
             d = 1979-05-27\n\
             e = 07:32:00\n\
             f = [1979-05-27, 07:32:00.5]\n\
             g = { at = 1979-05-27t07:32:00z }\n",
        )
        .unwrap();
        assert_eq!(
            v.get("a").and_then(Value::as_str),
            Some("1979-05-27T07:32:00Z")
        );
        assert_eq!(
            v.get("b").and_then(Value::as_str),
            Some("1979-05-27T00:32:00.999999-07:00")
        );
        assert_eq!(
            v.get("c").and_then(Value::as_str),
            Some("1979-05-27 07:32:00")
        );
        assert_eq!(v.get("d").and_then(Value::as_str), Some("1979-05-27"));
        assert_eq!(v.get("e").and_then(Value::as_str), Some("07:32:00"));
        assert_eq!(
            v.pointer("/f/1").and_then(Value::as_str),
            Some("07:32:00.5")
        );
        assert_eq!(
            v.pointer("/g/at").and_then(Value::as_str),
            Some("1979-05-27t07:32:00z")
        );
        #[derive(Decode, Debug)]
        struct Event {
            at: String,
        }
        let e: Event = decode("at = 2024-01-02T03:04:05Z\n").unwrap();
        assert_eq!(e.at, "2024-01-02T03:04:05Z");
        for bad in [
            "a = hello",
            "a = True",
            "a = 1979-05-27T",
            "a = 1979-5-27",
            "a = 07:32:00.",
            "a = 1979-05-27T07:32:00+0700",
            "a = 1979-05-27 x",
            "a = yes",
        ] {
            assert!(from_str(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn number_grammar_is_strict() {
        for bad in [
            "a = 01",
            "a = -01",
            "a = 1.",
            "a = .5",
            "a = 1__0",
            "a = _1",
            "a = 1_",
            "a = 1._5",
            "a = 1e",
            "a = 1e+",
            "a = 1e999",
            "a = 0x",
            "a = 0xZZ",
            "a = 0x_1",
            "a = 0b12",
            "a = 0o8",
            "a = 99999999999999999999",
            "a = +0x1",
            "a = 1.2.3",
            "a = --1",
        ] {
            assert!(from_str(bad).is_err(), "{bad:?} must be rejected");
        }
        let v = from_str(
            "a = +7\nb = -0\nc = 1_000.5\nd = 5e-1\ne = 6.02E+2_3\nf = 18446744073709551615\n\
             g = +nan\nh = 0.0\ni = -1_0\nj = 0xdead_BEEF\n",
        )
        .unwrap();
        assert_eq!(v.get("a"), Some(&Value::Int(7)));
        assert_eq!(v.get("b"), Some(&Value::Int(0)));
        assert_eq!(v.get("c"), Some(&Value::Float(1000.5)));
        assert_eq!(v.get("d"), Some(&Value::Float(0.5)));
        assert_eq!(v.get("e"), Some(&Value::Float(6.02e23)));
        assert_eq!(v.get("f"), Some(&Value::UInt(u64::MAX)));
        assert!(matches!(v.get("g"), Some(Value::Float(f)) if f.is_nan()));
        assert_eq!(v.get("h"), Some(&Value::Float(0.0)));
        assert_eq!(v.get("i"), Some(&Value::Int(-10)));
        assert_eq!(v.get("j"), Some(&Value::Int(0xdead_beef)));
    }

    #[test]
    fn typed_decoding_does_not_coerce_strings() {
        #[derive(Decode, Debug)]
        struct P {
            #[allow(dead_code)]
            port: u16,
        }
        #[derive(Decode, Debug)]
        struct B {
            #[allow(dead_code)]
            on: bool,
        }
        assert!(decode::<P>("port = 8080\n").is_ok());
        assert!(decode::<P>("port = \"8080\"\n").is_err());
        assert!(decode::<P>("port = \" 8080 \"\n").is_err());
        assert!(decode::<P>("port = 8080.0\n").is_ok());
        assert!(decode::<P>("port = 8080.5\n").is_err());
        assert!(decode::<B>("on = true\n").is_ok());
        assert!(decode::<B>("on = \"true\"\n").is_err());
        // A scalar is not silently promoted to a one-element array either.
        #[derive(Decode, Debug)]
        struct T {
            #[allow(dead_code)]
            tags: Vec<String>,
        }
        assert!(decode::<T>("tags = [\"a\"]\n").is_ok());
        assert!(decode::<T>("tags = \"a\"\n").is_err());
    }

    #[test]
    fn try_encode_rejects_unrepresentable_values() {
        let ok = Value::Object(vec![("a".into(), Value::Array(vec![1.into()]))]);
        assert_eq!(try_encode(&ok).unwrap(), encode(&ok));
        let null_in_array = Value::Object(vec![(
            "a".into(),
            Value::Array(vec![1.into(), Value::Null]),
        )]);
        assert!(matches!(
            try_encode(&null_in_array),
            Err(CodecError::Unrepresentable(_))
        ));
        let nested = Value::Object(vec![(
            "t".into(),
            Value::Object(vec![(
                "a".into(),
                Value::Array(vec![Value::Array(vec![Value::Null])]),
            )]),
        )]);
        assert!(try_encode(&nested).is_err());
        assert!(try_encode(&5i32).is_err());
        assert!(try_encode(&vec![1, 2]).is_err());
        // A null field is simply omitted, which is representable.
        let null_field = Value::Object(vec![("a".into(), Value::Null)]);
        assert_eq!(try_encode(&null_field).unwrap(), "");
    }

    #[test]
    fn parse_errors_report_line_and_column() {
        let err = from_str("a = 1\nb = \n").unwrap_err();
        assert_eq!(
            err.to_string(),
            "parse error: expected a value at line 2, column 5"
        );
        let err = from_str("a = 1\n[t]\nb = 2\nb = 3\n").unwrap_err();
        assert_eq!(
            err.to_string(),
            "parse error: duplicate key 'b' at line 4, column 1"
        );
        let err = from_str("a = 1\nb = \"x\\q\"\n").unwrap_err();
        assert!(err.to_string().ends_with("at line 2, column 7"), "{err}");
        let err = from_str("a = 1\nx = nope\n").unwrap_err();
        assert!(err.to_string().ends_with("at line 2, column 5"), "{err}");
    }

    #[test]
    fn limits_bound_size_and_entries() {
        let limits = Limits::new().with_max_entries(3);
        assert!(from_str_with("a = 1\nb = [1, 2]\n", limits).is_err());
        assert!(from_str_with("a = 1\nb = [1]\n", limits).is_ok());
        assert!(matches!(
            from_str_with("[a]\n[b]\n[c]\n[d]\n", limits),
            Err(CodecError::LimitExceeded(_))
        ));
        let limits = Limits::new().with_max_input_bytes(5);
        assert!(from_str_with("a = 1", limits).is_ok());
        assert!(from_str_with("ab = 1", limits).is_err());
        assert!(decode_with::<Value>("ab = 1", limits).is_err());
        assert!(decode_from_slice_with::<Value>(b"ab = 1", limits).is_err());
    }

    #[test]
    fn leading_byte_order_mark_is_skipped() {
        let v = from_str("\u{feff}a = 1\n").unwrap();
        assert_eq!(v.get("a"), Some(&Value::Int(1)));
        assert_eq!(from_str("\u{feff}").unwrap(), Value::Object(vec![]));
        assert!(from_str("a = 1\n\u{feff}b = 2\n").is_err());
    }

    #[test]
    fn header_edge_cases() {
        assert!(from_str("[a]]\n").is_err());
        assert!(from_str("[[a]\n").is_err());
        assert!(from_str("[[a] ]\n").is_err());
        assert!(from_str("[]\n").is_err());
        assert!(from_str("[a.]\n").is_err());
        assert!(from_str("[a] b = 1\n").is_err());
        let v = from_str("[ a . \"b c\" ]\nk = 1\n[a]\nz = 2\n").unwrap();
        assert_eq!(v.pointer("/a/b c/k"), Some(&Value::Int(1)));
        assert_eq!(v.pointer("/a/z"), Some(&Value::Int(2)));
    }

    #[test]
    fn a_table_is_defined_only_once() {
        // Found by differential testing against a reference parser: these
        // used to merge silently instead of being reported.
        for bad in [
            // The same header twice.
            "[a]\nx = 1\n[a]\ny = 2\n",
            "[a.b]\n[a.b]\n",
            "[a]\n[b]\n[a]\n",
            // A table built with dotted keys cannot be reopened by a header…
            "a.b = 1\n[a]\n",
            "[t]\na.b = 1\n[t.a]\n",
            // …nor extended with dotted keys from another section.
            "[t.a]\nx = 1\n[t]\na.y = 2\n",
            // An array of tables is not a table, and vice versa.
            "[[a]]\n[a]\n",
            "[a]\n[[a]]\n",
            "[a.b]\n[[a]]\n",
            "[[t]]\n[t.s]\n[t.s]\n",
            // Inline tables and plain arrays are closed once written.
            "a = { b = 1 }\n[a]\n",
            "a = { b = 1 }\na.c = 2\n",
            "a = []\n[[a]]\n",
            "a = { b = { c = 1 }, b.d = 2 }\n",
        ] {
            let r = from_str(bad);
            assert!(r.is_err(), "{bad:?} must be rejected, got {r:?}");
        }
        // What the TOML specification explicitly allows.
        let ok = [
            // A parent declared after its child.
            ("[a.b]\nx = 1\n[a]\ny = 2\n", r#"{"a":{"b":{"x":1},"y":2}}"#),
            // Several dotted keys into one table within a section.
            (
                "[t]\na.b = 1\na.c = 2\na.d.e = 3\n",
                r#"{"t":{"a":{"b":1,"c":2,"d":{"e":3}}}}"#,
            ),
            // A new sub-table under a dotted-key table.
            (
                "[f]\napple.color = \"red\"\n[f.apple.texture]\nsmooth = true\n",
                r#"{"f":{"apple":{"color":"red","texture":{"smooth":true}}}}"#,
            ),
            // Dotted keys into a table that so far existed only implicitly.
            ("[x.a.b]\n[x]\na.c = 1\n", r#"{"x":{"a":{"b":{},"c":1}}}"#),
            // Each element of an array of tables is a fresh table.
            (
                "[[t]]\n[t.s]\nk = 1\n[[t]]\n[t.s]\nk = 2\n",
                r#"{"t":[{"s":{"k":1}},{"s":{"k":2}}]}"#,
            ),
            (
                "[[a.b]]\n[[a.b]]\n[a]\nz = 1\n",
                r#"{"a":{"b":[{},{}],"z":1}}"#,
            ),
        ];
        for (doc, want) in ok {
            let v = from_str(doc).unwrap_or_else(|e| panic!("{doc:?}: {e}"));
            assert_eq!(ferroly::codec::json::to_string(&v), want, "{doc:?}");
        }
    }

    #[test]
    fn line_endings_and_control_characters() {
        // CRLF line endings are fine everywhere, and inside a multi-line string
        // they decode to LF, so the text does not depend on how the file was
        // saved.
        let lf = "a = 1 # c\n[t]\nb = \"\"\"\none\ntwo\\\n   three\"\"\"\nc = '''\nx\ny'''\nd = [\n  1,\n  2,\n]\n";
        let crlf = lf.replace('\n', "\r\n");
        let want = from_str(lf).unwrap();
        assert_eq!(from_str(&crlf).unwrap(), want);
        assert_eq!(
            want.pointer("/t/b").and_then(Value::as_str),
            Some("one\ntwothree")
        );
        assert_eq!(want.pointer("/t/c").and_then(Value::as_str), Some("x\ny"));

        // A carriage return on its own is neither whitespace nor a line break.
        for bad in [
            "a = 1\r",
            "a =\r1\n",
            "a\r= 1\n",
            "a = 1\rb = 2\n",
            "[a\r]\n",
            "a = [1,\r2]\n",
            "a = \"x\ry\"\n",
            "a = 'x\ry'\n",
            "a = \"\"\"x\ry\"\"\"\n",
            "a = '''x\ry'''\n",
            "# c\rd\n",
        ] {
            let r = from_str(bad);
            assert!(r.is_err(), "{bad:?} must be rejected, got {r:?}");
        }
        // Raw control characters are rejected in strings and comments; tab is
        // the one exception, and escapes carry the rest.
        for c in ['\u{0}', '\u{1}', '\u{8}', '\u{b}', '\u{1f}', '\u{7f}'] {
            for doc in [
                format!("a = \"x{c}y\"\n"),
                format!("a = 'x{c}y'\n"),
                format!("a = \"\"\"x{c}y\"\"\"\n"),
                format!("a = '''x{c}y'''\n"),
                format!("a = 1 # x{c}y\n"),
                format!("\"k{c}\" = 1\n"),
            ] {
                assert!(from_str(&doc).is_err(), "{doc:?} must be rejected");
            }
        }
        let v = from_str(
            "a = \"tab\there\" # tab\tin a comment\nb = 'lit\ttab'\nc = \"\\u0001\\u007F\"\n",
        )
        .unwrap();
        assert_eq!(v.get("a").and_then(Value::as_str), Some("tab\there"));
        assert_eq!(v.get("b").and_then(Value::as_str), Some("lit\ttab"));
        assert_eq!(v.get("c").and_then(Value::as_str), Some("\u{1}\u{7f}"));
        // …and the encoder writes control characters as escapes, so its own
        // output always parses.
        let tricky = Value::Object(vec![(
            "k\u{1}".into(),
            Value::Str("\u{0}\r\n\u{7f}\t".into()),
        )]);
        assert_eq!(from_str(&encode(&tricky)).unwrap(), tricky);
    }

    #[test]
    fn out_of_range_numbers_say_so() {
        let err = from_str("a = 1e999\n").unwrap_err().to_string();
        assert!(err.contains("number '1e999' is out of range"), "{err}");
        let err = from_str("a = 99999999999999999999\n")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("integer '99999999999999999999' is out of range"),
            "{err}"
        );
        let err = from_str("a = 1__0\n").unwrap_err().to_string();
        assert!(err.contains("invalid number '1__0'"), "{err}");
    }
}

//! A dependency-free YAML codec covering a well-defined subset of YAML 1.2.
//!
//! Supported:
//!
//! - Block mappings (`key: value`) and block sequences (`- item`), nested by
//!   indentation — including a sequence at the same indentation as its key and
//!   compact items (`- name: a` followed by further keys of the same mapping).
//! - Flow collections on a single line: `[a, b]`, `{a: 1, b: [2, 3]}`.
//! - Plain scalars resolved by the YAML 1.2 core schema (`null`/`~`, `true`/
//!   `false`, decimal/`0x`/`0o` integers, floats, `.inf`/`.nan`), single-quoted
//!   strings (`''` escape) and double-quoted strings (the full escape set,
//!   including `\xXX`, `\uXXXX`, `\UXXXXXXXX`).
//! - Literal (`|`) and folded (`>`) block scalars with chomping (`-`/`+`) and
//!   explicit indentation indicators.
//! - `#` comments (whole-line, and trailing after whitespace), a leading `---`
//!   and a trailing `...`.
//!
//! Anything outside the subset is **an error, never a silently different
//! value**: anchors (`&a`), aliases (`*a`), tags (`!t`), directives (`%YAML`),
//! explicit keys (`? `), multi-line flow collections, multi-line plain or
//! quoted scalars, tab indentation, a second `---` document, and any line the
//! parser cannot place. Two spots where YAML 1.1 and 1.2 disagree are also
//! errors rather than a guess: unquoted flow text that starts with `?` or `:`,
//! and a top-level block scalar with unindented content.
//!
//! The words `yes`/`no`/`on`/`off` are strings, as in YAML 1.2 (not booleans
//! as in YAML 1.1); the encoder quotes them so older parsers agree.
//!
//! The encoder always emits block style and quotes any string that would
//! otherwise read back as something else, so `encode` → `decode` round-trips.
//! Typed decoding uses the lenient [`Decode::decode`] path.

use std::fmt::{self as stdfmt, Write as _};

use ferroly::codec::{CodecError, Decode, Encode, Limits, Value};

use super::fmt;
use super::limits::Budget;

/// Encodes any [`Encode`] type to a YAML string.
pub fn encode<T: Encode + ?Sized>(value: &T) -> String {
    let mut out = String::with_capacity(64);
    match value.encode() {
        // A block collection needs at least one line; the empty ones use flow.
        Value::Object(o) if o.is_empty() => out.push_str("{}\n"),
        Value::Array(a) if a.is_empty() => out.push_str("[]\n"),
        other => write_block(&mut out, &other, 0),
    }
    out
}

/// Encodes to YAML bytes.
pub fn encode_to_vec<T: Encode + ?Sized>(value: &T) -> Vec<u8> {
    encode(value).into_bytes()
}

/// Decodes any [`Decode`] type from a YAML string.
pub fn decode<T: Decode>(input: &str) -> Result<T, CodecError> {
    decode_with(input, Limits::DEFAULT)
}

/// Like [`decode`], with an explicit parser budget.
pub fn decode_with<T: Decode>(input: &str, limits: Limits) -> Result<T, CodecError> {
    T::decode(&from_str_with(input, limits)?)
}

/// Decodes from YAML bytes.
pub fn decode_from_slice<T: Decode>(input: &[u8]) -> Result<T, CodecError> {
    decode_from_slice_with(input, Limits::DEFAULT)
}

/// Like [`decode_from_slice`], with an explicit parser budget.
pub fn decode_from_slice_with<T: Decode>(input: &[u8], limits: Limits) -> Result<T, CodecError> {
    let s = std::str::from_utf8(input).map_err(|e| CodecError::parse(e.to_string()))?;
    decode_with(s, limits)
}

/// Parses a YAML document into a [`Value`]. An empty document is `Null`.
pub fn from_str(input: &str) -> Result<Value, CodecError> {
    from_str_with(input, Limits::DEFAULT)
}

/// Like [`from_str`], with an explicit parser budget.
pub fn from_str_with(input: &str, limits: Limits) -> Result<Value, CodecError> {
    let budget = Budget::new(limits, input.len())?;
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let lines = input
        .lines()
        .enumerate()
        .map(|(i, raw)| {
            let indent = raw.len() - raw.trim_start_matches(' ').len();
            Line {
                no: i + 1,
                indent,
                text: raw[indent..].trim_end_matches([' ', '\t', '\r']),
                raw,
            }
        })
        .collect();
    Parser {
        lines,
        idx: 0,
        unterminated: !input.is_empty() && !input.ends_with('\n'),
        budget,
    }
    .document()
}

// ---- encoder ----------------------------------------------------------

fn write_block(out: &mut String, value: &Value, indent: usize) {
    match value {
        Value::Object(o) if !o.is_empty() => {
            for (k, v) in o {
                pad(out, indent);
                write_str(out, k);
                out.push(':');
                write_after_marker(out, v, indent);
            }
        }
        Value::Array(a) if !a.is_empty() => {
            for v in a {
                pad(out, indent);
                out.push('-');
                write_after_marker(out, v, indent);
            }
        }
        other => {
            pad(out, indent);
            write_scalar(out, other);
            out.push('\n');
        }
    }
}

/// Writes the value that follows a `key:` or `-` marker on the same line.
fn write_after_marker(out: &mut String, value: &Value, indent: usize) {
    match value {
        Value::Object(o) if !o.is_empty() => {
            out.push('\n');
            write_block(out, value, indent + 2);
        }
        Value::Array(a) if !a.is_empty() => {
            out.push('\n');
            write_block(out, value, indent + 2);
        }
        scalar => {
            out.push(' ');
            write_scalar(out, scalar);
            out.push('\n');
        }
    }
}

fn pad(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push(' ');
    }
}

/// Appends a scalar (or an empty collection, in flow form) directly into `out`.
fn write_scalar(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => fmt::write_i64(out, *i),
        Value::UInt(u) => fmt::write_u64(out, *u),
        Value::Float(f) => {
            if f.is_nan() {
                out.push_str(".nan");
            } else if f.is_infinite() {
                out.push_str(if *f < 0.0 { "-.inf" } else { ".inf" });
            } else {
                fmt::write_finite_f64(out, *f);
            }
        }
        Value::Str(s) => write_str(out, s),
        // Bytes have no YAML scalar form here; a flow sequence of integers
        // reads back as an array, matching the JSON codec.
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
        // Non-empty containers are handled by write_block/write_after_marker.
        Value::Array(_) => out.push_str("[]"),
        Value::Object(_) => out.push_str("{}"),
    }
}

/// Writes a string (a value or a key) plain when that is unambiguous, quoted
/// otherwise.
fn write_str(out: &mut String, s: &str) {
    if needs_quote(s) {
        write_quoted(out, s);
    } else {
        out.push_str(s);
    }
}

fn needs_quote(s: &str) -> bool {
    if s.is_empty() || s != s.trim() {
        return true;
    }
    // Conservative: any character that is structural somewhere in YAML, plus
    // every control and line-break character.
    if s.chars().any(|c| {
        matches!(
            c,
            ':' | '#' | '"' | '\'' | '\u{7f}' | '\u{85}' | '\u{2028}' | '\u{2029}' | '\u{feff}'
        ) || (c as u32) < 0x20
    }) {
        return true;
    }
    if s.starts_with([
        '-', '?', '[', ']', '{', '}', ',', '&', '*', '!', '|', '>', '%', '@', '`',
    ]) || s.starts_with("...")
    {
        return true;
    }
    // YAML 1.1 parsers read these as booleans.
    if matches!(
        s.to_ascii_lowercase().as_str(),
        "y" | "n" | "yes" | "no" | "on" | "off"
    ) {
        return true;
    }
    // Would otherwise be read back as a non-string scalar.
    !matches!(resolve_plain(s), Value::Str(_))
}

/// Writes a double-quoted scalar, bulk-copying runs that need no escape.
fn write_quoted(out: &mut String, s: &str) {
    out.push('"');
    let mut start = 0;
    for (i, c) in s.char_indices() {
        let esc: &str = match c {
            '"' => "\\\"",
            '\\' => "\\\\",
            '\n' => "\\n",
            '\r' => "\\r",
            '\t' => "\\t",
            '\u{85}' => "\\N",
            '\u{2028}' => "\\L",
            '\u{2029}' => "\\P",
            '\u{feff}' => "\\uFEFF",
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&s[start..i]);
                let _ = write!(out, "\\x{:02X}", c as u32);
                start = i + c.len_utf8();
                continue;
            }
            _ => continue,
        };
        out.push_str(&s[start..i]);
        out.push_str(esc);
        start = i + c.len_utf8();
    }
    out.push_str(&s[start..]);
    out.push('"');
}

// ---- scalar resolution ---------------------------------------------------

/// Resolves a plain (unquoted) scalar by the YAML 1.2 core schema.
fn resolve_plain(s: &str) -> Value {
    match s {
        "" | "~" | "null" | "Null" | "NULL" => Value::Null,
        "true" | "True" | "TRUE" => Value::Bool(true),
        "false" | "False" | "FALSE" => Value::Bool(false),
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" => Value::Float(f64::INFINITY),
        "-.inf" | "-.Inf" | "-.INF" => Value::Float(f64::NEG_INFINITY),
        ".nan" | ".NaN" | ".NAN" => Value::Float(f64::NAN),
        _ => resolve_number(s).unwrap_or_else(|| Value::Str(s.to_string())),
    }
}

fn int_value(u: u64) -> Value {
    match i64::try_from(u) {
        Ok(i) => Value::Int(i),
        Err(_) => Value::UInt(u),
    }
}

fn digit_run(b: &[u8]) -> usize {
    b.iter().take_while(|c| c.is_ascii_digit()).count()
}

/// Recognizes the core-schema integer and float forms; anything else is a
/// string.
fn resolve_number(s: &str) -> Option<Value> {
    if let Some(hex) = s.strip_prefix("0x") {
        if hex.is_empty() || !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        return u64::from_str_radix(hex, 16).ok().map(int_value);
    }
    if let Some(oct) = s.strip_prefix("0o") {
        if oct.is_empty() || !oct.bytes().all(|c| (b'0'..=b'7').contains(&c)) {
            return None;
        }
        return u64::from_str_radix(oct, 8).ok().map(int_value);
    }
    let b = s.strip_prefix(['+', '-']).unwrap_or(s).as_bytes();
    let int_digits = digit_run(b);
    if int_digits > 0 && int_digits == b.len() {
        if let Ok(i) = s.parse::<i64>() {
            return Some(Value::Int(i));
        }
        if let Ok(u) = s.parse::<u64>() {
            return Some(Value::UInt(u));
        }
        // An integer beyond 64 bits falls through to a float, as in JSON.
        return s.parse::<f64>().ok().map(Value::Float);
    }
    // [-+]? ( \. [0-9]+ | [0-9]+ ( \. [0-9]* )? ) ( [eE] [-+]? [0-9]+ )?
    let mut i = int_digits;
    let mut frac_digits = 0;
    if b.get(i) == Some(&b'.') {
        i += 1;
        frac_digits = digit_run(&b[i..]);
        i += frac_digits;
    }
    if int_digits == 0 && frac_digits == 0 {
        return None;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exp_digits = digit_run(&b[i..]);
        if exp_digits == 0 {
            return None;
        }
        i += exp_digits;
    }
    if i != b.len() {
        return None;
    }
    s.parse::<f64>().ok().map(Value::Float)
}

// ---- parser --------------------------------------------------------------

fn perr(line: usize, column: usize, msg: impl stdfmt::Display) -> CodecError {
    CodecError::Parse(format!("{msg} at line {line}, column {column}"))
}

/// One input line: its 1-based number, the count of leading spaces, the content
/// after them (trailing whitespace trimmed), and the untouched line.
#[derive(Clone, Copy)]
struct Line<'a> {
    no: usize,
    indent: usize,
    text: &'a str,
    raw: &'a str,
}

impl Line<'_> {
    fn err(&self, msg: impl stdfmt::Display) -> CodecError {
        perr(self.no, self.indent + 1, msg)
    }
}

/// `---` / `...` on a line of their own (optionally followed by more).
fn is_marker(text: &str, marker: &str) -> bool {
    match text.strip_prefix(marker) {
        Some(rest) => rest.is_empty() || rest.starts_with([' ', '\t']),
        None => false,
    }
}

/// A block sequence entry: `-` followed by whitespace or the end of the line.
fn is_seq_entry(text: &str) -> bool {
    is_marker(text, "-")
}

fn is_block_scalar_header(text: &str) -> bool {
    text.starts_with(['|', '>'])
}

/// Rejects a plain scalar (or plain key) that starts with syntax outside the
/// supported subset, naming the feature so the error is actionable.
fn check_plain_start(s: &str, flow: bool) -> Result<(), &'static str> {
    let b = s.as_bytes();
    let followed_by_space = matches!(b.get(1), None | Some(b' ' | b'\t'));
    if flow {
        // Inside a flow collection YAML 1.1 and 1.2 disagree about a scalar
        // that starts with one of these indicators (key marker or text?), so
        // neither reading is taken silently.
        match b.first() {
            Some(b'?' | b':') => {
                return Err(
                    "inside a flow collection, text starting with '?' or ':' must be quoted",
                )
            }
            Some(b'-') if matches!(b.get(1), None | Some(b' ' | b'\t' | b',' | b']' | b'}')) => {
                return Err("a sequence entry is not allowed here")
            }
            _ => {}
        }
    }
    match b.first() {
        Some(b'#') => Err("a scalar cannot start with '#'; quote it"),
        Some(b'&') => Err("anchors (&name) are not supported"),
        Some(b'*') => Err("aliases (*name) are not supported"),
        Some(b'!') => Err("tags (!name) are not supported"),
        Some(b'%' | b'@' | b'`') => {
            Err("a plain scalar cannot start with a reserved indicator; quote it")
        }
        Some(b'|' | b'>') => Err("a block scalar indicator is not allowed here"),
        Some(b',' | b']' | b'}') => Err("unexpected flow indicator"),
        Some(b'?') if followed_by_space => Err("explicit keys ('? key') are not supported"),
        Some(b'-') if followed_by_space => Err("a sequence entry is not allowed here"),
        Some(b':') if followed_by_space => Err("missing key before ':'"),
        _ => Ok(()),
    }
}

/// A cursor over one line's content, for quoted scalars and flow collections.
struct Inline<'a> {
    s: &'a str,
    pos: usize,
    line: usize,
    /// 0-based column of `s[0]` within the line.
    col0: usize,
}

impl<'a> Inline<'a> {
    fn new(s: &'a str, line: usize, col0: usize) -> Self {
        Inline {
            s,
            pos: 0,
            line,
            col0,
        }
    }

    fn err(&self, msg: impl stdfmt::Display) -> CodecError {
        self.err_at(self.pos, msg)
    }

    fn err_at(&self, pos: usize, msg: impl stdfmt::Display) -> CodecError {
        let mut p = pos.min(self.s.len());
        while !self.s.is_char_boundary(p) {
            p -= 1;
        }
        perr(self.line, self.col0 + self.s[..p].chars().count() + 1, msg)
    }

    fn peek(&self) -> Option<u8> {
        self.s.as_bytes().get(self.pos).copied()
    }

    fn rest(&self) -> &'a str {
        &self.s[self.pos..]
    }

    fn skip_spaces(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.pos += 1;
        }
    }

    /// Whether the byte before the cursor is whitespace (or the cursor is at
    /// the start) — the condition for `#` to begin a comment.
    fn after_space(&self) -> bool {
        self.pos == 0 || matches!(self.s.as_bytes()[self.pos - 1], b' ' | b'\t')
    }

    /// Requires that only whitespace and an optional comment remain.
    fn expect_end(&mut self) -> Result<(), CodecError> {
        self.skip_spaces();
        match self.peek() {
            None => Ok(()),
            Some(b'#') if self.after_space() => Ok(()),
            Some(_) => Err(self.err("unexpected characters after the value")),
        }
    }

    /// Parses a single- or double-quoted scalar starting at the cursor.
    fn quoted(&mut self) -> Result<String, CodecError> {
        let open = self.pos;
        let bytes = self.s.as_bytes();
        let quote = bytes[open];
        self.pos += 1;
        let mut out = String::new();
        let mut run = self.pos;
        loop {
            match bytes.get(self.pos) {
                None => {
                    return Err(self.err_at(
                        open,
                        "unterminated quoted string (it must end on the line it starts on)",
                    ))
                }
                Some(&c) if c == quote => {
                    out.push_str(&self.s[run..self.pos]);
                    if quote == b'\'' && bytes.get(self.pos + 1) == Some(&b'\'') {
                        out.push('\'');
                        self.pos += 2;
                        run = self.pos;
                        continue;
                    }
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') if quote == b'"' => {
                    out.push_str(&self.s[run..self.pos]);
                    self.pos += 1;
                    out.push(self.escape()?);
                    run = self.pos;
                }
                Some(_) => self.pos += 1,
            }
        }
    }

    /// Decodes one double-quote escape; the cursor is just past the backslash.
    fn escape(&mut self) -> Result<char, CodecError> {
        let at = self.pos - 1;
        let c = match self.peek() {
            Some(c) => c,
            None => return Err(self.err_at(at, "unterminated escape sequence")),
        };
        self.pos += 1;
        let invalid = |this: &Self| this.err_at(at, "invalid unicode escape");
        Ok(match c {
            b'0' => '\0',
            b'a' => '\u{7}',
            b'b' => '\u{8}',
            b't' | b'\t' => '\t',
            b'n' => '\n',
            b'v' => '\u{b}',
            b'f' => '\u{c}',
            b'r' => '\r',
            b'e' => '\u{1b}',
            b' ' => ' ',
            b'"' => '"',
            b'/' => '/',
            b'\\' => '\\',
            b'N' => '\u{85}',
            b'_' => '\u{a0}',
            b'L' => '\u{2028}',
            b'P' => '\u{2029}',
            b'x' => char::from_u32(self.hex(2, at)?).ok_or_else(|| invalid(self))?,
            b'U' => char::from_u32(self.hex(8, at)?).ok_or_else(|| invalid(self))?,
            b'u' => {
                let high = self.hex(4, at)?;
                if (0xD800..=0xDBFF).contains(&high) {
                    // A JSON-style UTF-16 surrogate pair.
                    if !self.rest().starts_with("\\u") {
                        return Err(invalid(self));
                    }
                    self.pos += 2;
                    let low = self.hex(4, at)?;
                    if !(0xDC00..=0xDFFF).contains(&low) {
                        return Err(invalid(self));
                    }
                    char::from_u32(0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00))
                        .ok_or_else(|| invalid(self))?
                } else {
                    char::from_u32(high).ok_or_else(|| invalid(self))?
                }
            }
            _ => return Err(self.err_at(at, "unknown escape sequence")),
        })
    }

    /// Reads exactly `n` hex digits (bytes, so a multibyte character can never
    /// be split).
    fn hex(&mut self, n: usize, at: usize) -> Result<u32, CodecError> {
        let digits = self
            .s
            .as_bytes()
            .get(self.pos..self.pos + n)
            .filter(|d| d.iter().all(u8::is_ascii_hexdigit));
        let digits = match digits {
            Some(d) => d,
            None => {
                return Err(self.err_at(at, format!("escape needs exactly {n} hexadecimal digits")))
            }
        };
        let mut v = 0u32;
        for &d in digits {
            v = v * 16 + (d as char).to_digit(16).unwrap_or(0);
        }
        self.pos += n;
        Ok(v)
    }

    /// Reads a plain scalar in block context: it runs to the end of the line
    /// or a trailing comment. A `: ` inside it would be a nested mapping on
    /// one line, which YAML does not allow.
    fn plain_block(&mut self) -> Result<&'a str, CodecError> {
        check_plain_start(self.rest(), false).map_err(|m| self.err(m))?;
        let b = self.s.as_bytes();
        let start = self.pos;
        let mut end = b.len();
        for (i, &c) in b.iter().enumerate().skip(start) {
            match c {
                b'#' if i > start && matches!(b[i - 1], b' ' | b'\t') => {
                    end = i;
                    break;
                }
                b':' if matches!(b.get(i + 1), None | Some(b' ' | b'\t')) => {
                    return Err(self.err_at(
                        i,
                        "mapping values are not allowed here (quote the text if the ':' belongs to it)",
                    ));
                }
                _ => {}
            }
        }
        self.pos = b.len();
        Ok(self.s[start..end].trim_end_matches([' ', '\t']))
    }

    /// Reads a plain scalar in flow context: it ends at `,` `]` `}`, at a `:`
    /// that separates a key from its value, or at a comment. The cursor is left
    /// on the terminator.
    fn plain_flow(&mut self) -> Result<&'a str, CodecError> {
        check_plain_start(self.rest(), true).map_err(|m| self.err(m))?;
        let b = self.s.as_bytes();
        let start = self.pos;
        let mut end = b.len();
        for (i, &c) in b.iter().enumerate().skip(start) {
            match c {
                b',' | b']' | b'}' => {
                    end = i;
                    break;
                }
                b'[' | b'{' => {
                    return Err(self.err_at(i, "unexpected flow indicator inside a plain scalar"));
                }
                // In flow context a `:` followed by whitespace or by any flow
                // indicator is the key/value separator, not part of the text.
                b':' if matches!(
                    b.get(i + 1),
                    None | Some(b' ' | b'\t' | b',' | b'[' | b']' | b'{' | b'}')
                ) =>
                {
                    end = i;
                    break;
                }
                b'#' if i > start && matches!(b[i - 1], b' ' | b'\t') => {
                    end = i;
                    break;
                }
                _ => {}
            }
        }
        self.pos = end;
        Ok(self.s[start..end].trim_end_matches([' ', '\t']))
    }
}

const UNTERMINATED_FLOW: &str =
    "unterminated flow collection (flow collections must fit on one line)";

struct Parser<'a> {
    lines: Vec<Line<'a>>,
    idx: usize,
    /// Whether the input's last line lacks a line break (it matters to a block
    /// scalar that runs to the end of the input).
    unterminated: bool,
    /// Depth and entry counters checked against the caller's [`Limits`].
    budget: Budget,
}

impl<'a> Parser<'a> {
    fn cur(&self) -> Option<Line<'a>> {
        self.lines.get(self.idx).copied()
    }

    /// Skips blank and comment-only lines.
    fn skip_blank(&mut self) {
        while let Some(l) = self.lines.get(self.idx) {
            if l.text.is_empty() || l.text.starts_with('#') {
                self.idx += 1;
            } else {
                break;
            }
        }
    }

    /// The next line that carries structure, or `None` at the end of the
    /// document (end of input, or a `---` / `...` marker).
    fn structural(&mut self) -> Result<Option<Line<'a>>, CodecError> {
        self.skip_blank();
        match self.cur() {
            None => Ok(None),
            Some(l) if l.text.starts_with('\t') => {
                Err(l.err("tabs are not allowed for indentation"))
            }
            Some(l) if l.indent == 0 && (is_marker(l.text, "---") || is_marker(l.text, "...")) => {
                Ok(None)
            }
            Some(l) => Ok(Some(l)),
        }
    }

    fn document(mut self) -> Result<Value, CodecError> {
        self.skip_blank();
        if let Some(l) = self.cur() {
            if l.indent == 0 && l.text.starts_with('%') {
                return Err(l.err("directives (%YAML, %TAG) are not supported"));
            }
            if l.indent == 0 && is_marker(l.text, "---") {
                let rest = l.text[3..].trim_start_matches([' ', '\t']);
                if !rest.is_empty() && !rest.starts_with('#') {
                    return Err(perr(
                        l.no,
                        l.text.len() - rest.len() + 1,
                        "content on the '---' line is not supported",
                    ));
                }
                self.idx += 1;
            }
        }

        let value = match self.structural()? {
            Some(l) => self.parse_node(l.indent, None)?,
            None => Value::Null,
        };

        // Fail closed: every remaining line must be accounted for.
        if let Some(l) = self.structural()? {
            return Err(l.err(
                "unexpected content (inconsistent indentation, or syntax outside the supported subset)",
            ));
        }
        if let Some(l) = self.cur() {
            if !is_marker(l.text, "...") {
                return Err(l.err("multiple documents are not supported"));
            }
            self.idx += 1;
            self.skip_blank();
            if let Some(next) = self.cur() {
                return Err(
                    next.err(if next.indent == 0 && is_marker(next.text, "---") {
                        "multiple documents are not supported"
                    } else {
                        "unexpected content after the document end marker"
                    }),
                );
            }
        }
        Ok(value)
    }

    /// Parses the node that starts on the current line, which is indented by
    /// `indent`. `parent` is the indentation of the enclosing collection (it
    /// bounds a block scalar's content).
    fn parse_node(&mut self, indent: usize, parent: Option<usize>) -> Result<Value, CodecError> {
        let line = self.lines[self.idx];
        if is_seq_entry(line.text) {
            return self.parse_sequence(indent);
        }
        if split_key(&line)?.is_some() {
            return self.parse_mapping(indent);
        }
        if is_block_scalar_header(line.text) {
            self.idx += 1;
            return self.block_scalar(&line, line.text, parent);
        }
        let value = self.inline_value(line.text, line.no, line.indent)?;
        self.idx += 1;
        Ok(value)
    }

    fn parse_mapping(&mut self, indent: usize) -> Result<Value, CodecError> {
        self.budget.enter()?;
        let mut entries = Vec::new();
        while let Some(line) = self.structural()? {
            if line.indent < indent || (line.indent == indent && is_seq_entry(line.text)) {
                break;
            }
            if line.indent > indent {
                return Err(line.err("unexpected indentation"));
            }
            let (key, rest) = match split_key(&line)? {
                Some(pair) => pair,
                None => return Err(line.err("expected 'key: value'")),
            };
            self.budget.entry()?;
            let value = if rest.is_empty() || rest.starts_with('#') {
                self.idx += 1;
                match self.structural()? {
                    Some(next) if next.indent > indent => {
                        self.parse_node(next.indent, Some(indent))?
                    }
                    // A sequence may sit at the same indentation as its key.
                    Some(next) if next.indent == indent && is_seq_entry(next.text) => {
                        self.parse_sequence(indent)?
                    }
                    _ => Value::Null,
                }
            } else if is_block_scalar_header(rest) {
                self.idx += 1;
                self.block_scalar(&line, rest, Some(indent))?
            } else {
                let col0 = line.indent + (line.text.len() - rest.len());
                let value = self.inline_value(rest, line.no, col0)?;
                self.idx += 1;
                value
            };
            entries.push((key, value));
        }
        self.budget.leave();
        Ok(Value::Object(entries))
    }

    fn parse_sequence(&mut self, indent: usize) -> Result<Value, CodecError> {
        self.budget.enter()?;
        let mut items = Vec::new();
        while let Some(line) = self.structural()? {
            if line.indent < indent || (line.indent == indent && !is_seq_entry(line.text)) {
                break;
            }
            if line.indent > indent {
                return Err(line.err("unexpected indentation"));
            }
            self.budget.entry()?;
            let after = line.text[1..].trim_start_matches([' ', '\t']);
            if after.is_empty() || after.starts_with('#') {
                self.idx += 1;
                items.push(match self.structural()? {
                    Some(next) if next.indent > indent => {
                        self.parse_node(next.indent, Some(indent))?
                    }
                    _ => Value::Null,
                });
            } else {
                // The item starts on the `-` line. Re-read the rest of the line
                // as a node indented to its own column: this is what makes
                // `- key: value` (with further keys aligned below it), nested
                // `- - x`, and plain scalars all parse through one path.
                let column = indent + (line.text.len() - after.len());
                let current = &mut self.lines[self.idx];
                current.indent = column;
                current.text = after;
                items.push(self.parse_node(column, Some(indent))?);
            }
        }
        self.budget.leave();
        Ok(Value::Array(items))
    }

    /// Parses a value that sits entirely on one line: a flow collection, a
    /// quoted scalar, or a plain scalar.
    fn inline_value(
        &mut self,
        text: &'a str,
        line: usize,
        col0: usize,
    ) -> Result<Value, CodecError> {
        let mut sc = Inline::new(text, line, col0);
        match sc.peek() {
            Some(b'[' | b'{') => {
                let value = self.flow_value(&mut sc)?;
                sc.expect_end()?;
                Ok(value)
            }
            Some(b'"' | b'\'') => {
                let value = Value::Str(sc.quoted()?);
                sc.expect_end()?;
                Ok(value)
            }
            Some(b'#') | None => Ok(Value::Null),
            Some(_) => Ok(resolve_plain(sc.plain_block()?)),
        }
    }

    fn flow_value(&mut self, sc: &mut Inline<'a>) -> Result<Value, CodecError> {
        sc.skip_spaces();
        match sc.peek() {
            Some(b'[') => self.flow_sequence(sc),
            Some(b'{') => self.flow_mapping(sc),
            Some(b'"' | b'\'') => Ok(Value::Str(sc.quoted()?)),
            None => Err(sc.err(UNTERMINATED_FLOW)),
            Some(_) => {
                let plain = sc.plain_flow()?;
                if plain.is_empty() {
                    return Err(sc.err("expected a value"));
                }
                Ok(resolve_plain(plain))
            }
        }
    }

    fn flow_sequence(&mut self, sc: &mut Inline<'a>) -> Result<Value, CodecError> {
        self.budget.enter()?;
        sc.pos += 1; // '['
        let mut items = Vec::new();
        loop {
            sc.skip_spaces();
            match sc.peek() {
                Some(b']') => {
                    sc.pos += 1;
                    break;
                }
                None => return Err(sc.err(UNTERMINATED_FLOW)),
                Some(_) => {}
            }
            self.budget.entry()?;
            items.push(self.flow_value(sc)?);
            sc.skip_spaces();
            match sc.peek() {
                Some(b',') => sc.pos += 1,
                Some(b']') => {
                    sc.pos += 1;
                    break;
                }
                None => return Err(sc.err(UNTERMINATED_FLOW)),
                Some(b':') => {
                    return Err(
                        sc.err("a mapping inside a flow sequence must be written with braces")
                    )
                }
                Some(_) => return Err(sc.err("expected ',' or ']'")),
            }
        }
        self.budget.leave();
        Ok(Value::Array(items))
    }

    fn flow_mapping(&mut self, sc: &mut Inline<'a>) -> Result<Value, CodecError> {
        self.budget.enter()?;
        sc.pos += 1; // '{'
        let mut entries = Vec::new();
        loop {
            sc.skip_spaces();
            let key = match sc.peek() {
                Some(b'}') => {
                    sc.pos += 1;
                    break;
                }
                None => return Err(sc.err(UNTERMINATED_FLOW)),
                Some(b'"' | b'\'') => sc.quoted()?,
                Some(b'[' | b'{') => {
                    return Err(sc.err("collections as mapping keys are not supported"))
                }
                Some(_) => {
                    let plain = sc.plain_flow()?;
                    if plain.is_empty() {
                        return Err(sc.err("expected a key"));
                    }
                    plain.to_string()
                }
            };
            self.budget.entry()?;
            sc.skip_spaces();
            let value = if sc.peek() == Some(b':') {
                sc.pos += 1;
                sc.skip_spaces();
                if matches!(sc.peek(), Some(b',' | b'}')) {
                    Value::Null
                } else {
                    self.flow_value(sc)?
                }
            } else {
                Value::Null
            };
            entries.push((key, value));
            sc.skip_spaces();
            match sc.peek() {
                Some(b',') => sc.pos += 1,
                Some(b'}') => {
                    sc.pos += 1;
                    break;
                }
                None => return Err(sc.err(UNTERMINATED_FLOW)),
                Some(_) => return Err(sc.err("expected ',' or '}'")),
            }
        }
        self.budget.leave();
        Ok(Value::Object(entries))
    }

    /// Reads a literal (`|`) or folded (`>`) block scalar. `header` is the text
    /// from the indicator on; the cursor is on the first content line.
    fn block_scalar(
        &mut self,
        header_line: &Line<'a>,
        header: &str,
        parent: Option<usize>,
    ) -> Result<Value, CodecError> {
        let folded = header.starts_with('>');
        let mut keep = false;
        let mut strip = false;
        let mut explicit: Option<usize> = None;
        let mut rest = &header[1..];
        while let Some(c) = rest.bytes().next() {
            match c {
                b'+' if !keep && !strip => keep = true,
                b'-' if !keep && !strip => strip = true,
                b'1'..=b'9' if explicit.is_none() => explicit = Some(usize::from(c - b'0')),
                _ => break,
            }
            rest = &rest[1..];
        }
        let trailing = rest.trim_start_matches([' ', '\t']);
        if !(trailing.is_empty() || (trailing.starts_with('#') && trailing.len() < rest.len())) {
            return Err(header_line.err("invalid block scalar header"));
        }

        // Content must be indented past the parent collection. The YAML 1.2
        // grammar lets a top-level block scalar start at column 0; like libyaml
        // this parser asks for at least one space there, so that an unindented
        // document under a stray `|` is an error rather than one big string.
        let min_indent = parent.map_or(1, |p| p + 1);
        let mut content_indent = explicit.map(|n| parent.map_or(n, |p| p + n));
        let mut body: Vec<&'a str> = Vec::new();
        while let Some(l) = self.cur() {
            let blank = l.raw.trim_matches([' ', '\t', '\r']).is_empty();
            let ci = match content_indent {
                Some(ci) => ci,
                None => {
                    if blank {
                        body.push("");
                        self.idx += 1;
                        continue;
                    }
                    if l.indent < min_indent {
                        break;
                    }
                    content_indent = Some(l.indent);
                    l.indent
                }
            };
            if blank {
                // Spaces beyond the content indentation are content.
                body.push(l.raw.get(ci..).unwrap_or("").trim_end_matches('\r'));
                self.idx += 1;
                continue;
            }
            if l.indent < ci
                || (l.indent == 0 && (is_marker(l.text, "---") || is_marker(l.text, "...")))
            {
                break;
            }
            body.push(l.raw[ci..].trim_end_matches('\r'));
            self.idx += 1;
        }

        let trailing_breaks = body.iter().rev().take_while(|l| l.is_empty()).count();
        body.truncate(body.len() - trailing_breaks);

        let mut out = String::new();
        if folded {
            fold_lines(&mut out, &body);
        } else {
            for (i, l) in body.iter().enumerate() {
                if i > 0 {
                    out.push('\n');
                }
                out.push_str(l);
            }
        }
        // Count the line breaks that follow the last content line. When the
        // scalar runs to the end of an input whose final line has no line break,
        // that break is simply not there to keep.
        let missing = usize::from(self.idx >= self.lines.len() && self.unterminated);
        let breaks = if body.is_empty() {
            trailing_breaks.saturating_sub(missing)
        } else {
            trailing_breaks + 1 - missing
        };
        if keep {
            for _ in 0..breaks {
                out.push('\n');
            }
        } else if !strip && !body.is_empty() && breaks > 0 {
            out.push('\n');
        }
        Ok(Value::Str(out))
    }
}

/// Joins the lines of a folded (`>`) block scalar: a single line break between
/// two ordinary lines becomes a space, blank lines become line breaks, and
/// breaks around more-indented lines are kept.
fn fold_lines(out: &mut String, body: &[&str]) {
    let mut prev_more: Option<bool> = None;
    let mut empties = 0usize;
    for l in body {
        if l.is_empty() {
            empties += 1;
            continue;
        }
        let more = l.starts_with([' ', '\t']);
        let breaks = match prev_more {
            None => empties,
            Some(false) if !more => {
                if empties == 0 {
                    out.push(' ');
                }
                empties
            }
            Some(_) => empties + 1,
        };
        for _ in 0..breaks {
            out.push('\n');
        }
        out.push_str(l);
        prev_more = Some(more);
        empties = 0;
    }
}

/// Splits a block line into `key` and the text after its `:`. Returns `None`
/// when the line is not a mapping entry. Quote-aware: a `:` inside a quoted key
/// (or a quoted scalar) is not a separator.
fn split_key<'a>(line: &Line<'a>) -> Result<Option<(String, &'a str)>, CodecError> {
    let text = line.text;
    let bytes = text.as_bytes();
    match bytes.first() {
        Some(b'"' | b'\'') => {
            let mut sc = Inline::new(text, line.no, line.indent);
            let key = sc.quoted()?;
            sc.skip_spaces();
            let separator = sc.peek() == Some(b':')
                && matches!(bytes.get(sc.pos + 1), None | Some(b' ' | b'\t'));
            Ok(if separator {
                Some((key, text[sc.pos + 1..].trim_start_matches([' ', '\t'])))
            } else {
                None
            })
        }
        Some(b'[' | b'{') => Ok(None),
        _ => {
            for (i, &c) in bytes.iter().enumerate() {
                match c {
                    b':' if matches!(bytes.get(i + 1), None | Some(b' ' | b'\t')) => {
                        let key = text[..i].trim_end_matches([' ', '\t']);
                        if key.is_empty() {
                            return Err(line.err("missing key before ':'"));
                        }
                        check_plain_start(key, false).map_err(|m| line.err(m))?;
                        let rest = text[i + 1..].trim_start_matches([' ', '\t']);
                        return Ok(Some((key.to_string(), rest)));
                    }
                    b'#' if i > 0 && matches!(bytes[i - 1], b' ' | b'\t') => return Ok(None),
                    _ => {}
                }
            }
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroly::codec::{json, Decode, Encode};

    const MAX_DEPTH: usize = Limits::DEFAULT_MAX_DEPTH;

    /// Parses YAML and renders the result as compact JSON, for terse asserts.
    fn j(yaml: &str) -> String {
        match from_str(yaml) {
            Ok(v) => json::to_string(&v),
            Err(e) => panic!("{yaml:?} failed to parse: {e}"),
        }
    }

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
        let yaml = encode(&cfg);
        let back: Config = decode(&yaml).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn parses_handwritten_yaml() {
        let yaml = "name: service\nport: 9000\ntags:\n  - x\n  - y\nnested:\n  enabled: false\n  threshold: 2\n";
        let cfg: Config = decode(yaml).unwrap();
        assert_eq!(cfg.name, "service");
        assert_eq!(cfg.port, 9000);
        assert_eq!(cfg.tags, vec!["x".to_string(), "y".to_string()]);
        assert!(!cfg.nested.enabled);
    }

    #[test]
    fn quotes_ambiguous_strings() {
        #[derive(Encode, Decode, PartialEq, Debug)]
        struct W {
            v: String,
        }
        let w = W { v: "true".into() };
        let yaml = encode(&w);
        assert!(yaml.contains("\"true\""));
        let back: W = decode(&yaml).unwrap();
        assert_eq!(back.v, "true");
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

        // Empty flow forms decode to empty collections, not the strings "[]"/"{}".
        assert_eq!(
            from_str("v: []").unwrap().get("v"),
            Some(&Value::Array(vec![]))
        );
        assert_eq!(
            from_str("v: {}").unwrap().get("v"),
            Some(&Value::Object(vec![]))
        );
    }

    #[test]
    fn rejects_deeply_nested_input_without_stack_overflow() {
        let n = MAX_DEPTH + 50;
        let mut yaml = String::new();
        for i in 0..n {
            yaml.push_str(&" ".repeat(2 * i));
            yaml.push_str("k:\n");
        }
        yaml.push_str(&" ".repeat(2 * n));
        yaml.push_str("leaf\n");
        assert!(from_str(&yaml).is_err());

        let flow = format!("{}{}", "[".repeat(n), "]".repeat(n));
        assert!(from_str(&flow).is_err());
        let seq = format!("{}x", "- ".repeat(n));
        assert!(from_str(&seq).is_err());
    }

    #[test]
    fn sequence_at_the_same_indentation_as_its_key() {
        assert_eq!(
            j("tags:\n- a\n- b\nport: 1"),
            r#"{"tags":["a","b"],"port":1}"#
        );
        assert_eq!(
            j("outer:\n  tags:\n  - a\n  - b\n  port: 1\nlast: x"),
            r#"{"outer":{"tags":["a","b"],"port":1},"last":"x"}"#
        );
        // An indented sequence still works, and so does an empty value.
        assert_eq!(
            j("tags:\n  - a\nnone:\nport: 1"),
            r#"{"tags":["a"],"none":null,"port":1}"#
        );
    }

    #[test]
    fn compact_mappings_in_sequences_keep_every_key() {
        assert_eq!(
            j("- name: a\n  port: 1\n- name: b"),
            r#"[{"name":"a","port":1},{"name":"b"}]"#
        );
        assert_eq!(
            j("servers:\n  - host: a\n    ports:\n      - 1\n      - 2\n    tls: true\n  - host: b\n"),
            r#"{"servers":[{"host":"a","ports":[1,2],"tls":true},{"host":"b"}]}"#
        );
        assert_eq!(
            j("- name: a\n  tags:\n  - x\n  - y\n- name: b"),
            r#"[{"name":"a","tags":["x","y"]},{"name":"b"}]"#
        );
        assert_eq!(j("- - a\n  - b\n- - c"), r#"[["a","b"],["c"]]"#);
        assert_eq!(
            j("-\n  name: a\n  port: 1\n- 2"),
            r#"[{"name":"a","port":1},2]"#
        );
        assert_eq!(j("-   key:   v"), r#"[{"key":"v"}]"#);
        // A key that is not aligned with the first one is an error.
        assert!(from_str("- name: a\n   port: 1\n").is_err());
        assert!(from_str("- name: a\n port: 1\n").is_err());
    }

    #[test]
    fn trailing_comments_are_stripped_outside_quotes() {
        assert_eq!(j("port: 8080 # comment"), r#"{"port":8080}"#);
        assert_eq!(j("a: x#y"), r#"{"a":"x#y"}"#);
        assert_eq!(j("a: \"x # y\" # real"), r#"{"a":"x # y"}"#);
        assert_eq!(j("a: 'x # y'   # real"), r#"{"a":"x # y"}"#);
        assert_eq!(j("a: # nothing\nb: 1"), r#"{"a":null,"b":1}"#);
        assert_eq!(j("- a # one\n- b#c\n-  # empty\n"), r#"["a","b#c",null]"#);
        assert_eq!(
            j("# lead\na: 1\n  # indented comment\nb: 2\n"),
            r#"{"a":1,"b":2}"#
        );
        assert_eq!(j("a: [1, 2] # c"), r#"{"a":[1,2]}"#);
        assert_eq!(
            j("url: http://x.y/z#frag"),
            r#"{"url":"http://x.y/z#frag"}"#
        );
        assert_eq!(j("it's: fine # c"), r#"{"it's":"fine"}"#);
    }

    #[test]
    fn flow_collections_parse() {
        assert_eq!(j("tags: [a, b]"), r#"{"tags":["a","b"]}"#);
        assert_eq!(j("m: {a: 1, b: two}"), r#"{"m":{"a":1,"b":"two"}}"#);
        assert_eq!(
            j("x: [1, [2, 3], {k: [true, null]}, 'q, r', \"s]\"]"),
            r#"{"x":[1,[2,3],{"k":[true,null]},"q, r","s]"]}"#
        );
        assert_eq!(j("[a, b, ]"), r#"["a","b"]"#);
        assert_eq!(j("{a: , b, \"c\":1}"), r#"{"a":null,"b":null,"c":1}"#);
        assert_eq!(j("- [1, 2]\n- {a: b}"), r#"[[1,2],{"a":"b"}]"#);
        assert_eq!(j("a: [ ]"), r#"{"a":[]}"#);
        assert_eq!(j("a: { }"), r#"{"a":{}}"#);
        assert_eq!(j("a: [x y, 1.5, -3, ~]"), r#"{"a":["x y",1.5,-3,null]}"#);
        assert_eq!(
            j("{url: http://a/b, t: 12:30}"),
            r#"{"url":"http://a/b","t":"12:30"}"#
        );
        for bad in [
            "a: [1, 2",
            "a: [1,\n  2]",
            "a: {b: 1",
            "a: [1, , 2]",
            "a: [1 2] x",
            "a: [a: 1]",
            "a: {[1]: 2}",
            "a: [1]]",
            "a: {b: c: d}",
            "a: [&x 1]",
            "a: [*x]",
            "a: [!t 1]",
            "a: ]",
            "a: }",
            "a: [x[1]]",
        ] {
            assert!(from_str(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn flow_context_indicators_are_not_guessed_at() {
        // Found by differential testing against a reference parser: each of
        // these was read as text although YAML gives it a structural meaning.
        //
        // `?` / `:` at the start of a flow scalar: an explicit-key or value
        // marker in YAML 1.1, plain text in 1.2. Neither reading is taken.
        for bad in [
            "a: [x, ?, y]",
            "a: [?b]",
            "a: {?k: v}",
            "a: [:x]",
            "a: {k: v, :w: 1}",
            "a: [-]",
            "a: [- x]",
            "a: [-, x]",
            // ` #` starts a comment, so the collection never closes.
            "a: {k:   #v, w: 1}",
            "a: [x,  #y]",
            "a: [ #x]",
            // `#` cannot start a scalar at all.
            "a: [#x]",
            "a: {#k: v}",
        ] {
            let r = from_str(bad);
            assert!(r.is_err(), "{bad:?} must be rejected, got {r:?}");
        }
        // Quoted, all of them are plain data.
        assert_eq!(
            j(r##"a: ["?", ":x", "-", "#y"]"##),
            r##"{"a":["?",":x","-","#y"]}"##
        );
        // Inside the text (not at its start) these characters are ordinary.
        assert_eq!(
            j("a: [x?y, a-b, -c, http://h/p?q=1, x#y]"),
            r##"{"a":["x?y","a-b","-c","http://h/p?q=1","x#y"]}"##
        );
        // A `:` followed by a flow indicator separates key and value.
        assert_eq!(j("{a:[1], b:{c: 2}}"), r#"{"a":[1],"b":{"c":2}}"#);
        assert_eq!(j("{a:, b: 1}"), r#"{"a":null,"b":1}"#);
        // In block context `?x` and `:x` remain ordinary text.
        assert_eq!(j("a: ?x\nb: :y\n"), r#"{"a":"?x","b":":y"}"#);
    }

    #[test]
    fn top_level_block_scalar_needs_indented_content() {
        // A stray `|` above an unindented document used to swallow the whole
        // document as one string.
        assert!(from_str("|\nb: 1\nc: 2\n").is_err());
        assert!(from_str(">\ntext\n").is_err());
        assert_eq!(j("|\n b: 1\n c: 2\n"), r#""b: 1\nc: 2\n""#);
        assert_eq!(j("--- # doc\n>-\n  folded\n  text\n"), r#""folded text""#);
        assert_eq!(j("|2\n   x\n"), r#"" x\n""#);
        assert_eq!(j("|\n"), r#""""#);
    }

    #[test]
    fn unsupported_syntax_is_rejected_not_stringified() {
        for bad in [
            "a: &anchor 1",
            "a: *alias",
            "a: !tag 1",
            "a: !!str 1",
            "&a key: 1",
            "*a : 1",
            "- &a x",
            "- *a",
            "- !t x",
            "? complex\n: value",
            "%YAML 1.2\n---\na: 1",
            "a: 1\n---\nb: 2",
            "---\na: 1\n---\n",
            "--- a",
            "a: 1\n...\nb: 2",
            "a: b: c",
            "a: - b",
            "a: @x",
            "a: `x`",
            "a: %x",
            ": v",
            "a: 1\n  b: 2",
            "a:\n  b: 1\n c: 2",
            "a: one\n  two",
            "- a\n  b",
            "a: 1\n- b",
            "- a\nb: 1",
            "plain\nmore",
            "a: \"unterminated",
            "a: 'unterminated",
            "a: \"x\" y",
            "a: 'x' y",
            "a: \"\\q\"",
            "a: \"\\x4\"",
            "a: \"\\u12\"",
            "a: \"\\uD800\"",
            "a: \"\\U00110000\"",
            "\tkey: v",
            "a:\n\t- b",
            "a: |x\n  t",
            "a: >12\n  t",
            "key",
            "a: 1\nkey",
        ] {
            let r = from_str(bad);
            // The two bare-scalar documents are the only valid inputs here.
            if bad == "key" {
                assert_eq!(r.unwrap(), Value::Str("key".into()));
            } else {
                assert!(r.is_err(), "{bad:?} must be rejected, got {r:?}");
            }
        }
    }

    #[test]
    fn document_markers() {
        assert_eq!(j("---\na: 1\n"), r#"{"a":1}"#);
        assert_eq!(j("--- # comment\na: 1\n...\n"), r#"{"a":1}"#);
        assert_eq!(j("a: 1\n...\n# trailing\n\n"), r#"{"a":1}"#);
        assert_eq!(j("---\n"), "null");
        assert_eq!(j(""), "null");
        assert_eq!(j("# only a comment\n\n"), "null");
        assert_eq!(j("\u{feff}a: 1"), r#"{"a":1}"#);
        // A quoted or indented `---` is data, not a marker.
        assert_eq!(j("a: \"---\""), r#"{"a":"---"}"#);
        assert_eq!(j("a:\n  - ---x"), r#"{"a":["---x"]}"#);
    }

    #[test]
    fn quoted_scalars_and_escapes() {
        assert_eq!(j(r#"a: 'it''s'"#), r#"{"a":"it's"}"#);
        assert_eq!(j(r#"a: ''"#), r#"{"a":""}"#);
        assert_eq!(j(r#"a: ''''"#), r#"{"a":"'"}"#);
        assert_eq!(j(r#"a: 'no \n escape'"#), r#"{"a":"no \\n escape"}"#);
        assert_eq!(j(r#"a: "\u00e9\u4E2D""#), r#"{"a":"é中"}"#);
        assert_eq!(j(r#"a: "\uD83D\uDE00\U0001F600""#), r#"{"a":"😀😀"}"#);
        assert_eq!(
            j(r#"a: "\x41\t\n\\\"\/\ \0""#),
            "{\"a\":\"A\\t\\n\\\\\\\"/ \\u0000\"}"
        );
        assert_eq!(
            from_str(r#""\a\b\e\f\r\v\N\_\L\P""#).unwrap(),
            Value::Str("\u{7}\u{8}\u{1b}\u{c}\r\u{b}\u{85}\u{a0}\u{2028}\u{2029}".into())
        );
        // Quoted keys, including ones containing ": ".
        assert_eq!(j(r#""a: b": 1"#), r#"{"a: b":1}"#);
        assert_eq!(j(r#"'a: b' : 'c: d'"#), r#"{"a: b":"c: d"}"#);
        assert_eq!(j(r#"- "a: b""#), r#"["a: b"]"#);
        assert_eq!(j(r#""a: b""#), r#""a: b""#);
        assert_eq!(j(r#""é": "ü""#), r#"{"é":"ü"}"#);
        // A multibyte character right after a short hex escape is not a panic.
        assert!(from_str("a: \"\\u000é\"").is_err());
        assert!(from_str("a: \"\\x4é\"").is_err());
        assert!(from_str("a: \"\\U0000004é\"").is_err());
    }

    #[test]
    fn plain_scalars_follow_the_core_schema() {
        let cases: Vec<(&str, Value)> = vec![
            ("null", Value::Null),
            ("~", Value::Null),
            ("Null", Value::Null),
            ("true", Value::Bool(true)),
            ("FALSE", Value::Bool(false)),
            ("0", Value::Int(0)),
            ("-5", Value::Int(-5)),
            ("+7", Value::Int(7)),
            ("007", Value::Int(7)),
            ("0x1F", Value::Int(31)),
            ("0o17", Value::Int(15)),
            ("18446744073709551615", Value::UInt(u64::MAX)),
            ("1.5", Value::Float(1.5)),
            ("-0.25", Value::Float(-0.25)),
            (".5", Value::Float(0.5)),
            ("5.", Value::Float(5.0)),
            ("1e3", Value::Float(1000.0)),
            ("1.5E-2", Value::Float(0.015)),
            (".inf", Value::Float(f64::INFINITY)),
            ("-.INF", Value::Float(f64::NEG_INFINITY)),
            // Strings: none of these is a number or keyword in YAML 1.2.
            ("inf", Value::Str("inf".into())),
            ("nan", Value::Str("nan".into())),
            ("NaN", Value::Str("NaN".into())),
            ("infinity", Value::Str("infinity".into())),
            ("yes", Value::Str("yes".into())),
            ("off", Value::Str("off".into())),
            ("1.2.3", Value::Str("1.2.3".into())),
            ("12:30", Value::Str("12:30".into())),
            ("1e", Value::Str("1e".into())),
            ("0x", Value::Str("0x".into())),
            ("1_000", Value::Str("1_000".into())),
            ("-", Value::Null),
            ("- -5", Value::Array(vec![Value::Int(-5)])),
            ("-x", Value::Str("-x".into())),
        ];
        for (src, want) in cases {
            let got = from_str(src).unwrap_or_else(|e| panic!("{src:?}: {e}"));
            if src == "-" {
                // A lone `-` is a sequence with one empty item.
                assert_eq!(got, Value::Array(vec![Value::Null]));
            } else {
                assert_eq!(got, want, "{src:?}");
            }
        }
        assert!(matches!(from_str(".nan").unwrap(), Value::Float(f) if f.is_nan()));
    }

    #[test]
    fn block_scalars() {
        assert_eq!(
            j("a: |\n  line 1\n  line 2\nb: 1"),
            r#"{"a":"line 1\nline 2\n","b":1}"#
        );
        assert_eq!(j("a: |-\n  x\n  y\n"), r#"{"a":"x\ny"}"#);
        assert_eq!(j("a: |+\n  x\n\n\nb: 1"), r#"{"a":"x\n\n\n","b":1}"#);
        assert_eq!(
            j("a: |\n  x\n\n  y\n   indented # not a comment\n"),
            r#"{"a":"x\n\ny\n indented # not a comment\n"}"#
        );
        assert_eq!(
            j("a: >\n  folded\n  text\n\n  next\n"),
            r#"{"a":"folded text\nnext\n"}"#
        );
        assert_eq!(j("a: >-\n  a\n    b\n  c\n"), r#"{"a":"a\n  b\nc"}"#);
        assert_eq!(j("- |\n  item\n- >\n  f\n  g\n"), r#"["item\n","f g\n"]"#);
        assert_eq!(j("a: |2\n    two extra\n"), r#"{"a":"  two extra\n"}"#);
        assert_eq!(j("a: | # comment\n  x\n"), r#"{"a":"x\n"}"#);
        assert_eq!(j("a: |\nb: 1"), r#"{"a":"","b":1}"#);
        assert_eq!(j("|\n top\n level\n"), r#""top\nlevel\n""#);
        assert_eq!(
            j("a:\n  b: |\n    deep\n  c: 2\n"),
            r#"{"a":{"b":"deep\n","c":2}}"#
        );
        // At the end of an input with no final line break there is no break to
        // clip or keep.
        assert_eq!(j("a: |\n  x\n  y"), r#"{"a":"x\ny"}"#);
        assert_eq!(j("a: |+\n  x"), r#"{"a":"x"}"#);
        assert_eq!(j("a: |+\n  x\n\n"), r#"{"a":"x\n\n"}"#);
        assert_eq!(j("a: |+\n  x\n\n  "), r#"{"a":"x\n\n"}"#);
        assert_eq!(j("a: >\n  x\n  y"), r#"{"a":"x y"}"#);
        assert_eq!(j("a: |+\n\n\n"), r#"{"a":"\n\n"}"#);
        assert_eq!(j("a: |+\n\n  "), r#"{"a":"\n"}"#);
        assert_eq!(j("a: |\n  x\nb: 1"), r#"{"a":"x\n","b":1}"#);
    }

    #[test]
    fn encoder_output_round_trips_awkward_values() {
        let strings = [
            "",
            " ",
            "a: b",
            "a #b",
            "#x",
            "- x",
            "-",
            "?",
            "[x]",
            "{x}",
            "x, y",
            "&a",
            "*a",
            "!t",
            "|",
            ">",
            "%x",
            "@x",
            "`x",
            "'q'",
            "\"q\"",
            "back\\slash",
            "tab\there",
            "line\nbreak",
            "cr\rlf",
            "null",
            "~",
            "true",
            "False",
            "yes",
            "No",
            "on",
            "1",
            "-5",
            "1.5",
            ".5",
            "1e3",
            "0x10",
            ".inf",
            ".nan",
            "---",
            "...",
            "a:",
            ":a",
            "key:value",
            " lead",
            "trail ",
            "\u{1}",
            "\u{7f}",
            "\u{85}",
            "\u{2028}",
            "\u{feff}x",
            "é中😀",
            "0o7",
            "+1",
            "007",
            "1_000",
            "inf",
            "nan",
        ];
        for s in strings {
            // As a top-level scalar, a mapping value, a mapping key, an item.
            let top = Value::Str(s.to_string());
            assert_eq!(from_str(&encode(&top)).unwrap(), top, "top-level {s:?}");
            let nested = Value::Object(vec![
                (s.to_string(), Value::Str(s.to_string())),
                (
                    "list".to_string(),
                    Value::Array(vec![Value::Str(s.to_string()), Value::Str(s.to_string())]),
                ),
            ]);
            let yaml = encode(&nested);
            assert_eq!(
                from_str(&yaml).unwrap(),
                nested,
                "nested {s:?} via {yaml:?}"
            );
        }
    }

    #[test]
    fn encoder_output_round_trips_top_level_shapes() {
        let values = [
            Value::Int(-5),
            Value::Int(0),
            Value::UInt(u64::MAX),
            Value::Float(-2.5),
            Value::Float(250.0),
            Value::Float(f64::INFINITY),
            Value::Float(f64::NEG_INFINITY),
            Value::Bool(true),
            Value::Null,
            Value::Object(vec![]),
            Value::Array(vec![]),
            Value::Array(vec![Value::Int(-1), Value::Int(-2)]),
            Value::Array(vec![Value::Array(vec![]), Value::Object(vec![])]),
            Value::Array(vec![Value::Array(vec![Value::Array(vec![Value::Int(1)])])]),
            Value::Array(vec![Value::Null, Value::Null]),
            Value::Object(vec![
                ("a".into(), Value::Object(vec![])),
                ("b".into(), Value::Array(vec![])),
                ("c".into(), Value::Null),
                (
                    "d".into(),
                    Value::Array(vec![Value::Object(vec![
                        ("x".into(), Value::Int(1)),
                        ("y".into(), Value::Array(vec![Value::Int(2)])),
                    ])]),
                ),
            ]),
        ];
        for v in values {
            let yaml = encode(&v);
            assert_eq!(from_str(&yaml).unwrap(), v, "via {yaml:?}");
        }
        assert_eq!(encode(&Value::Object(vec![])), "{}\n");
        assert_eq!(encode(&Value::Array(vec![])), "[]\n");
        assert_eq!(encode(&-5i32), "-5\n");
        let nan = from_str(&encode(&f64::NAN)).unwrap();
        assert!(matches!(nan, Value::Float(f) if f.is_nan()));
        // Bytes are a flow sequence of integers, as in JSON.
        assert_eq!(
            encode(&Value::Object(vec![("b".into(), Value::Bytes(vec![1, 2]))])),
            "b: [1, 2]\n"
        );
    }

    #[test]
    fn parse_errors_report_line_and_column() {
        let err = from_str("a: 1\nb:\n  - x\n  y: 2\n").unwrap_err();
        assert!(err.to_string().ends_with("at line 4, column 3"), "{err}");
        let err = from_str("a: 1\nb: [1, 2\n").unwrap_err();
        assert!(err.to_string().ends_with("at line 2, column 9"), "{err}");
        let err = from_str("a: 1\nb: é *x: 1\n").unwrap_err();
        assert!(err.to_string().ends_with("at line 2, column 8"), "{err}");
        let err = from_str("k: \"ab\\q\"").unwrap_err();
        assert_eq!(
            err.to_string(),
            "parse error: unknown escape sequence at line 1, column 7"
        );
    }

    #[test]
    fn limits_bound_depth_size_and_entries() {
        let limits = Limits::new().with_max_depth(2);
        assert!(from_str_with("a:\n  b: 1\n", limits).is_ok());
        assert!(matches!(
            from_str_with("a:\n  b:\n    c: 1\n", limits),
            Err(CodecError::LimitExceeded(_))
        ));
        assert!(from_str_with("a: [[1]]", limits).is_err());
        let limits = Limits::new().with_max_entries(3);
        assert!(from_str_with("- 1\n- 2\n- 3\n", limits).is_ok());
        assert!(matches!(
            from_str_with("- 1\n- 2\n- 3\n- 4\n", limits),
            Err(CodecError::LimitExceeded(_))
        ));
        assert!(from_str_with("[1, 2, 3, 4]", limits).is_err());
        let limits = Limits::new().with_max_input_bytes(4);
        assert!(from_str_with("a: 1", limits).is_ok());
        assert!(from_str_with("ab: 1", limits).is_err());
        assert!(decode_with::<Value>("ab: 1", limits).is_err());
        assert!(decode_from_slice_with::<Value>(b"ab: 1", limits).is_err());
    }
}

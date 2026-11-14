//! A pragmatic, dependency-free XML codec over the [`Value`] model.
//!
//! XML has no canonical mapping to a generic data model, so this codec uses a
//! straightforward convention geared at struct-like data:
//!
//! - An object becomes child elements: `{name: "x"}` → `<name>x</name>`.
//! - An array becomes repeated sibling elements sharing the field's tag:
//!   `{tags: ["a", "b"]}` → `<tags>a</tags><tags>b</tags>`.
//! - Scalars become escaped text; `null` becomes an empty element (`<a/>`).
//! - The whole document is wrapped in a `<root>` element.
//!
//! Three shapes cannot be told apart by that convention alone, so the encoder
//! marks them with an attribute and the parser honours it:
//!
//! - `type="array"` — an explicit array whose items are the element's children
//!   (written as `<item>`). Used for empty arrays, nested arrays, a lone `null`
//!   item, and a top-level array: `[[1, 2], [3]]` under key `m` becomes
//!   `<m type="array"><item type="array"><item>1</item>…`.
//! - `type="object"` — an empty object (`<a type="object"/>`).
//! - `xml:space="preserve"` — the standard XML attribute: text is taken
//!   verbatim (not trimmed) and empty text is the empty string rather than
//!   `null`. Used for `""` and for strings with leading/trailing whitespace.
//!
//! Map keys that are not valid XML names are escaped as `_xHHHH_` (the hex code
//! point of each offending character, as in `a_x0020_b` for `"a b"`), so a key
//! can never inject markup; the parser reverses the escape.
//!
//! All other attributes are skipped; namespaces and mixed content (text
//! alongside child elements, which is rejected) are not modeled. Decoded
//! scalars are always strings — the lenient [`Decode`] impls parse numbers and
//! bools out of them. Use JSON where full fidelity matters.
//!
//! Literal line breaks are normalized to LF as XML requires. Beyond that the
//! parser is pragmatic rather than conforming: it tolerates some input a
//! validating parser rejects (`]]>` in text, undeclared namespace prefixes,
//! references to characters XML 1.0 excludes), but never reads different data
//! from a well-formed document.
//!
//! The parser scans bytes (structural characters and entities are ASCII),
//! slicing element names and clean text runs directly from the input. It
//! enforces a [`Limits`] budget and reports the line and column of errors.

use std::fmt::Write as _;

use ferroly::codec::{CodecError, Decode, Encode, Limits, Value};

use super::fmt;
use super::limits::{error_at, Budget, KeyIndex};

/// Encodes any [`Encode`] type to an XML string.
///
/// Characters XML 1.0 cannot carry (most C0 control characters) are written as
/// numeric character references, which this codec reads back but a strict XML
/// parser rejects; use [`try_encode`] to get an error instead.
pub fn encode<T: Encode + ?Sized>(value: &T) -> String {
    let mut w = Writer::new();
    w.single("root", &value.encode());
    w.out
}

/// Like [`encode`], but fails when the value holds a character that is not
/// legal anywhere in an XML 1.0 document (U+0000–U+001F other than tab, LF and
/// CR, or U+FFFE/U+FFFF).
pub fn try_encode<T: Encode + ?Sized>(value: &T) -> Result<String, CodecError> {
    let mut w = Writer::new();
    w.single("root", &value.encode());
    match w.illegal {
        Some(c) => Err(CodecError::unrepresentable(format!(
            "character U+{:04X} is not allowed in an XML 1.0 document",
            c as u32
        ))),
        None => Ok(w.out),
    }
}

/// Encodes to XML bytes.
pub fn encode_to_vec<T: Encode + ?Sized>(value: &T) -> Vec<u8> {
    encode(value).into_bytes()
}

/// Decodes any [`Decode`] type from an XML string.
///
/// XML text carries no types, so this uses the lenient [`Decode::decode`]
/// path: numbers and bools are parsed out of the element text.
pub fn decode<T: Decode>(input: &str) -> Result<T, CodecError> {
    decode_with(input, Limits::DEFAULT)
}

/// Like [`decode`], with an explicit parser budget.
pub fn decode_with<T: Decode>(input: &str, limits: Limits) -> Result<T, CodecError> {
    T::decode(&from_str_with(input, limits)?)
}

/// Decodes from XML bytes.
pub fn decode_from_slice<T: Decode>(input: &[u8]) -> Result<T, CodecError> {
    decode_from_slice_with(input, Limits::DEFAULT)
}

/// Like [`decode_from_slice`], with an explicit parser budget.
pub fn decode_from_slice_with<T: Decode>(input: &[u8], limits: Limits) -> Result<T, CodecError> {
    let s = std::str::from_utf8(input).map_err(|e| CodecError::parse(e.to_string()))?;
    decode_with(s, limits)
}

/// Parses an XML document into a [`Value`] (the root element's content).
pub fn from_str(input: &str) -> Result<Value, CodecError> {
    from_str_with(input, Limits::DEFAULT)
}

/// Like [`from_str`], with an explicit parser budget.
pub fn from_str_with(input: &str, limits: Limits) -> Result<Value, CodecError> {
    let mut p = Parser::new(input, limits)?;
    if p.starts_with("\u{feff}".as_bytes()) {
        p.pos += 3; // byte-order mark
    }
    p.skip_misc()?;
    let (_tag, value) = p.parse_element(false)?;
    p.skip_misc()?;
    if p.pos != p.bytes.len() {
        return Err(p.err("unexpected content after the root element"));
    }
    Ok(value)
}

// ---- encoder ----------------------------------------------------------

/// The tag used for the items of an explicit (`type="array"`) array.
const ITEM_TAG: &str = "item";

struct Writer {
    out: String,
    /// The first character written that XML 1.0 cannot represent.
    illegal: Option<char>,
}

impl Writer {
    fn new() -> Self {
        Writer {
            out: String::with_capacity(64),
            illegal: None,
        }
    }

    /// Writes an object member: an array in sibling form when that is
    /// unambiguous, anything else as a single element.
    fn field(&mut self, tag: &str, value: &Value) {
        match value {
            Value::Array(items) if sibling_form(items) => {
                for item in items {
                    self.single(tag, item);
                }
            }
            other => self.single(tag, other),
        }
    }

    /// Writes `value` as exactly one element named `tag`.
    fn single(&mut self, tag: &str, value: &Value) {
        match value {
            Value::Null => self.empty(tag, ""),
            Value::Object(o) if o.is_empty() => self.empty(tag, " type=\"object\""),
            Value::Object(o) => {
                self.open(tag, "");
                for (k, v) in o {
                    self.field(k, v);
                }
                self.close(tag);
            }
            Value::Array(items) if items.is_empty() => self.empty(tag, " type=\"array\""),
            Value::Array(items) => {
                self.open(tag, " type=\"array\"");
                for item in items {
                    self.single(ITEM_TAG, item);
                }
                self.close(tag);
            }
            Value::Str(s) => {
                // Empty or padded text would be read back as null / trimmed.
                let padded =
                    s.is_empty() || s.starts_with(is_xml_space) || s.ends_with(is_xml_space);
                self.open(
                    tag,
                    if padded {
                        " xml:space=\"preserve\""
                    } else {
                        ""
                    },
                );
                self.text(s);
                self.close(tag);
            }
            Value::Bool(b) => {
                self.open(tag, "");
                self.out.push_str(if *b { "true" } else { "false" });
                self.close(tag);
            }
            Value::Int(i) => {
                self.open(tag, "");
                fmt::write_i64(&mut self.out, *i);
                self.close(tag);
            }
            Value::UInt(u) => {
                self.open(tag, "");
                fmt::write_u64(&mut self.out, *u);
                self.close(tag);
            }
            Value::Float(f) => {
                self.open(tag, "");
                fmt::write_f64(&mut self.out, *f);
                self.close(tag);
            }
            Value::Bytes(b) => {
                self.open(tag, "");
                for (i, byte) in b.iter().enumerate() {
                    if i > 0 {
                        self.out.push(' ');
                    }
                    fmt::write_u64(&mut self.out, *byte as u64);
                }
                self.close(tag);
            }
        }
    }

    fn open(&mut self, tag: &str, attrs: &str) {
        self.out.push('<');
        write_name(&mut self.out, tag);
        self.out.push_str(attrs);
        self.out.push('>');
    }

    fn close(&mut self, tag: &str) {
        self.out.push_str("</");
        write_name(&mut self.out, tag);
        self.out.push('>');
    }

    fn empty(&mut self, tag: &str, attrs: &str) {
        self.out.push('<');
        write_name(&mut self.out, tag);
        self.out.push_str(attrs);
        self.out.push_str("/>");
    }

    /// Writes escaped character data, bulk-copying runs that need no escape.
    fn text(&mut self, s: &str) {
        let mut start = 0;
        for (i, c) in s.char_indices() {
            let esc: &str = match c {
                '<' => "&lt;",
                '>' => "&gt;",
                '&' => "&amp;",
                // A literal CR would be normalized to LF by a conforming parser.
                '\r' => "&#13;",
                '\t' | '\n' => continue,
                c if (c as u32) < 0x20 || c == '\u{fffe}' || c == '\u{ffff}' => {
                    // Not representable in XML 1.0; a character reference is
                    // the closest thing, and this codec's own parser reads it.
                    self.illegal.get_or_insert(c);
                    self.out.push_str(&s[start..i]);
                    let _ = write!(self.out, "&#x{:X};", c as u32);
                    start = i + c.len_utf8();
                    continue;
                }
                _ => continue,
            };
            self.out.push_str(&s[start..i]);
            self.out.push_str(esc);
            start = i + c.len_utf8();
        }
        self.out.push_str(&s[start..]);
    }
}

/// Whether an array can be written as repeated sibling elements and still be
/// recognizable when read back. Empty arrays, arrays holding arrays, and a lone
/// `null` item need the explicit `type="array"` form.
fn sibling_form(items: &[Value]) -> bool {
    !(items.is_empty()
        || items.iter().any(|v| matches!(v, Value::Array(_)))
        || (items.len() == 1 && items[0].is_null()))
}

/// The four characters XML treats as whitespace.
fn is_xml_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

fn trim_xml_space(s: &str) -> &str {
    s.trim_matches(is_xml_space)
}

/// XML 1.0 `NameStartChar`, minus `:` (escaped to keep names namespace-neutral).
fn is_name_start(c: char) -> bool {
    matches!(c,
        'A'..='Z' | 'a'..='z' | '_'
        | '\u{C0}'..='\u{D6}' | '\u{D8}'..='\u{F6}' | '\u{F8}'..='\u{2FF}'
        | '\u{370}'..='\u{37D}' | '\u{37F}'..='\u{1FFF}' | '\u{200C}'..='\u{200D}'
        | '\u{2070}'..='\u{218F}' | '\u{2C00}'..='\u{2FEF}' | '\u{3001}'..='\u{D7FF}'
        | '\u{F900}'..='\u{FDCF}' | '\u{FDF0}'..='\u{FFFD}' | '\u{10000}'..='\u{EFFFF}')
}

/// XML 1.0 `NameChar`, minus `:`.
fn is_name_char(c: char) -> bool {
    is_name_start(c)
        || matches!(c,
            '-' | '.' | '0'..='9' | '\u{B7}' | '\u{300}'..='\u{36F}' | '\u{203F}'..='\u{2040}')
}

/// Writes `name` as a valid XML element name. Characters that are not legal at
/// their position are written as `_xHHHH_`; an underscore that would itself
/// read as such an escape is escaped too, so [`unescape_name`] is an exact
/// inverse. The empty name is written as `_x_`.
fn write_name(out: &mut String, name: &str) {
    // Fast path: a plain ASCII identifier with no escape look-alike.
    let b = name.as_bytes();
    if !b.is_empty()
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
        && !name.contains("_x")
    {
        out.push_str(name);
        return;
    }
    if name.is_empty() {
        out.push_str("_x_");
        return;
    }
    if name == "_x_" {
        out.push_str("_x005F_x_");
        return;
    }
    for (i, c) in name.char_indices() {
        let legal = if i == 0 {
            is_name_start(c)
        } else {
            is_name_char(c)
        };
        let looks_escaped = c == '_' && parse_name_escape(&name[i..]).is_some();
        if legal && !looks_escaped {
            out.push(c);
        } else {
            let _ = write!(out, "_x{:04X}_", c as u32);
        }
    }
}

/// Recognizes `_x` + one to six hex digits + `_` at the start of `s`, returning
/// the escape's byte length and the character it stands for.
fn parse_name_escape(s: &str) -> Option<(usize, char)> {
    let b = s.as_bytes();
    if b.len() < 4 || b[0] != b'_' || b[1] != b'x' {
        return None;
    }
    let mut end = 2;
    while end < b.len() && end < 8 && b[end].is_ascii_hexdigit() {
        end += 1;
    }
    if end == 2 || b.get(end) != Some(&b'_') {
        return None;
    }
    let code = u32::from_str_radix(&s[2..end], 16).ok()?;
    Some((end + 1, char::from_u32(code)?))
}

/// Reverses [`write_name`].
fn unescape_name(raw: &str) -> String {
    if !raw.contains("_x") {
        return raw.to_string();
    }
    if raw == "_x_" {
        return String::new();
    }
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut run = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'_' {
            if let Some((len, c)) = parse_name_escape(&raw[i..]) {
                out.push_str(&raw[run..i]);
                out.push(c);
                i += len;
                run = i;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&raw[run..]);
    out
}

// ---- parser --------------------------------------------------------------

/// The longest entity body (`&…;`) the parser will scan for.
const MAX_ENTITY_LEN: usize = 32;

/// What a `type="…"` attribute declared the element to be.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Plain,
    Array,
    Object,
}

/// An element's children: repeated tags collapse into an array. Lookup goes
/// through a [`KeyIndex`], so a wide element stays linear to parse.
#[derive(Default)]
struct Children {
    entries: Vec<(String, Value)>,
    /// Whether the entry at the same position is an array built from repeated
    /// siblings (as opposed to a single child that happens to be an array).
    repeated: Vec<bool>,
    index: KeyIndex,
}

impl Children {
    fn push(&mut self, tag: String, value: Value) {
        match self.index.find(&self.entries, &tag) {
            Some(i) => {
                let slot = &mut self.entries[i].1;
                if self.repeated[i] {
                    if let Value::Array(a) = slot {
                        a.push(value);
                    }
                } else {
                    let prev = std::mem::take(slot);
                    *slot = Value::Array(vec![prev, value]);
                    self.repeated[i] = true;
                }
            }
            None => {
                self.index.inserted(&tag, self.entries.len());
                self.entries.push((tag, value));
                self.repeated.push(false);
            }
        }
    }
}

struct Parser<'a> {
    input: &'a str,
    bytes: &'a [u8],
    pos: usize,
    /// Depth and entry counters checked against the caller's [`Limits`], so
    /// hostile deeply-nested or very wide input is rejected.
    budget: Budget,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str, limits: Limits) -> Result<Self, CodecError> {
        Ok(Self {
            input,
            bytes: input.as_bytes(),
            pos: 0,
            budget: Budget::new(limits, input.len())?,
        })
    }

    fn err(&self, msg: impl std::fmt::Display) -> CodecError {
        error_at(self.input, self.pos, msg)
    }

    fn err_at(&self, pos: usize, msg: impl std::fmt::Display) -> CodecError {
        error_at(self.input, pos, msg)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn starts_with(&self, s: &[u8]) -> bool {
        self.bytes[self.pos..].starts_with(s)
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    /// Skips a construct that starts at the current position with an opener of
    /// `open_len` bytes and runs through `terminator`, returning the span
    /// between them. A missing terminator is an error, never a panic.
    fn skip_through(
        &mut self,
        open_len: usize,
        terminator: &[u8],
        what: &str,
    ) -> Result<&'a str, CodecError> {
        let from = self.pos + open_len;
        let found = self.bytes[from..]
            .windows(terminator.len())
            .position(|w| w == terminator);
        match found {
            Some(offset) => {
                let body = &self.input[from..from + offset];
                self.pos = from + offset + terminator.len();
                Ok(body)
            }
            None => Err(self.err(format!("unterminated {what}"))),
        }
    }

    /// Skips whitespace, comments, and processing instructions (including the
    /// XML declaration) outside the root element.
    fn skip_misc(&mut self) -> Result<(), CodecError> {
        loop {
            self.skip_ws();
            if self.starts_with(b"<?") {
                self.skip_through(2, b"?>", "processing instruction")?;
            } else if self.starts_with(b"<!--") {
                self.skip_through(4, b"-->", "comment")?;
            } else if self.starts_with(b"<!") {
                return Err(self.err("DOCTYPE declarations are not supported"));
            } else {
                return Ok(());
            }
        }
    }

    /// Parses a single element, returning `(tag, value)`. `preserve` is the
    /// `xml:space` setting inherited from the parent.
    fn parse_element(&mut self, mut preserve: bool) -> Result<(String, Value), CodecError> {
        let start = self.pos;
        if self.peek() != Some(b'<') {
            return Err(self.err("expected '<' to start an element"));
        }
        self.pos += 1;
        let raw_tag = self.read_name()?;
        let mut kind = Kind::Plain;
        let (mut seen_type, mut seen_space) = (false, false);

        // Attributes: quote-aware, so a `>` inside a value does not end the tag.
        let self_closing = loop {
            self.skip_ws();
            match self.peek() {
                None => return Err(self.err_at(start, "unterminated start tag")),
                Some(b'>') => {
                    self.pos += 1;
                    break false;
                }
                Some(b'/') => {
                    if self.bytes.get(self.pos + 1) != Some(&b'>') {
                        return Err(self.err("malformed start tag"));
                    }
                    self.pos += 2;
                    break true;
                }
                Some(_) => {
                    let at = self.pos;
                    let (name, value) = self.parse_attribute()?;
                    // The two attributes the codec acts on must be unambiguous.
                    let seen = match name {
                        "type" => Some(&mut seen_type),
                        "xml:space" => Some(&mut seen_space),
                        _ => None,
                    };
                    if let Some(seen) = seen {
                        if std::mem::replace(seen, true) {
                            return Err(self.err_at(at, format!("duplicate attribute '{name}'")));
                        }
                    }
                    match (name, value) {
                        ("type", "array") => kind = Kind::Array,
                        ("type", "object") => kind = Kind::Object,
                        ("xml:space", "preserve") => preserve = true,
                        ("xml:space", "default") => preserve = false,
                        _ => {}
                    }
                }
            }
        };

        let value = if self_closing {
            empty_value(kind, preserve)
        } else {
            self.parse_content(raw_tag, kind, preserve)?
        };
        Ok((unescape_name(raw_tag), value))
    }

    /// Parses one `name="value"` attribute, returning the raw (undecoded) value.
    fn parse_attribute(&mut self) -> Result<(&'a str, &'a str), CodecError> {
        let name = self
            .read_name()
            .map_err(|_| self.err("malformed start tag"))?;
        self.skip_ws();
        if self.peek() != Some(b'=') {
            return Err(self.err(format!("attribute '{name}' has no value")));
        }
        self.pos += 1;
        self.skip_ws();
        let quote = match self.peek() {
            Some(q @ (b'"' | b'\'')) => q,
            _ => return Err(self.err("attribute value must be quoted")),
        };
        let open = self.pos;
        self.pos += 1;
        let start = self.pos;
        loop {
            match self.peek() {
                None => return Err(self.err_at(open, "unterminated attribute value")),
                Some(b) if b == quote => break,
                Some(_) => self.pos += 1,
            }
        }
        let value = &self.input[start..self.pos];
        self.pos += 1; // closing quote
        Ok((name, value))
    }

    /// Parses element content up to and including the matching close tag.
    fn parse_content(
        &mut self,
        tag: &str,
        kind: Kind,
        preserve: bool,
    ) -> Result<Value, CodecError> {
        let content_start = self.pos;
        let mut children = Children::default();
        let mut items: Vec<Value> = Vec::new();
        let mut has_children = false;
        let mut text = String::new();
        let mut run = self.pos; // start of the current clean text run

        loop {
            match self.peek() {
                None => {
                    return Err(self.err(format!("unexpected end of input inside <{tag}>")));
                }
                Some(b'<') => {
                    text.push_str(&self.input[run..self.pos]);
                    if self.starts_with(b"<!--") {
                        self.skip_through(4, b"-->", "comment")?;
                    } else if self.starts_with(b"<![CDATA[") {
                        let cdata = self.skip_through(9, b"]]>", "CDATA section")?;
                        if cdata.contains('\r') {
                            text.push_str(&cdata.replace("\r\n", "\n").replace('\r', "\n"));
                        } else {
                            text.push_str(cdata);
                        }
                    } else if self.starts_with(b"<?") {
                        self.skip_through(2, b"?>", "processing instruction")?;
                    } else if self.starts_with(b"</") {
                        let close_at = self.pos;
                        self.pos += 2;
                        let close = self.read_name()?;
                        self.skip_ws();
                        if self.peek() != Some(b'>') {
                            return Err(self.err("malformed close tag"));
                        }
                        self.pos += 1;
                        if close != tag {
                            return Err(self.err_at(
                                close_at,
                                format!("mismatched tags: <{tag}> vs </{close}>"),
                            ));
                        }
                        break;
                    } else {
                        self.budget.enter()?;
                        self.budget.entry()?;
                        let (child_tag, child_val) = self.parse_element(preserve)?;
                        self.budget.leave();
                        has_children = true;
                        if kind == Kind::Array {
                            items.push(child_val);
                        } else {
                            children.push(child_tag, child_val);
                        }
                    }
                    run = self.pos;
                }
                Some(b'&') => {
                    text.push_str(&self.input[run..self.pos]);
                    self.pos += 1; // consume '&'
                    text.push(self.read_entity()?);
                    run = self.pos;
                }
                // XML normalizes every literal line break (CRLF or a lone CR)
                // to LF; a CR that must survive is written as `&#13;`.
                Some(b'\r') => {
                    text.push_str(&self.input[run..self.pos]);
                    text.push('\n');
                    self.pos += 1;
                    if self.peek() == Some(b'\n') {
                        self.pos += 1;
                    }
                    run = self.pos;
                }
                Some(_) => self.pos += 1,
            }
        }

        let blank = text.chars().all(is_xml_space);
        if has_children {
            if !blank {
                return Err(self.err_at(
                    content_start,
                    format!(
                        "mixed content in <{tag}>: text alongside child elements is not supported"
                    ),
                ));
            }
            return Ok(if kind == Kind::Array {
                Value::Array(items)
            } else {
                Value::Object(children.entries)
            });
        }
        if blank && (kind != Kind::Plain || !preserve) {
            return Ok(empty_value(kind, false));
        }
        if preserve {
            return Ok(Value::Str(text));
        }
        let trimmed = trim_xml_space(&text);
        Ok(Value::Str(if trimmed.len() == text.len() {
            text
        } else {
            trimmed.to_string()
        }))
    }

    fn read_name(&mut self) -> Result<&'a str, CodecError> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            // ASCII name chars, or any multibyte (>= 0x80) lead/continuation byte.
            if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':') || b >= 0x80 {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(self.err("expected a name"));
        }
        Ok(&self.input[start..self.pos])
    }

    /// Reads an entity reference body (after `&`), consuming the trailing `;`.
    fn read_entity(&mut self) -> Result<char, CodecError> {
        let start = self.pos;
        let end = self.bytes[start..]
            .iter()
            .take(MAX_ENTITY_LEN + 1)
            .position(|&b| b == b';')
            .map(|offset| start + offset);
        let end = match end {
            Some(end) => end,
            None => return Err(self.err_at(start - 1, "unterminated entity reference")),
        };
        // Both ends sit next to ASCII bytes (`&` and `;`), so they are char
        // boundaries and the slice cannot panic.
        let ent = &self.input[start..end];
        self.pos = end + 1;
        let decoded = match ent {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => {
                if let Some(hex) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                    char_reference(hex, 16)
                } else if let Some(dec) = ent.strip_prefix('#') {
                    char_reference(dec, 10)
                } else {
                    return Err(self.err_at(start - 1, format!("unknown entity &{ent};")));
                }
            }
        };
        decoded.ok_or_else(|| self.err_at(start - 1, format!("bad character reference &{ent};")))
    }
}

/// Decodes the digits of a numeric character reference. Only digits of the
/// given radix are accepted — in particular no sign, which the integer parsers
/// in `std` would otherwise allow.
fn char_reference(digits: &str, radix: u32) -> Option<char> {
    if digits.is_empty() || !digits.bytes().all(|b| (b as char).is_digit(radix)) {
        return None;
    }
    u32::from_str_radix(digits, radix)
        .ok()
        .and_then(char::from_u32)
}

/// The value of an element with no content.
fn empty_value(kind: Kind, preserve: bool) -> Value {
    match kind {
        Kind::Array => Value::Array(Vec::new()),
        Kind::Object => Value::Object(Vec::new()),
        Kind::Plain if preserve => Value::Str(String::new()),
        Kind::Plain => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroly::codec::{Decode, Encode};

    const MAX_DEPTH: usize = Limits::DEFAULT_MAX_DEPTH;

    #[derive(Encode, Decode, PartialEq, Debug)]
    struct Config {
        name: String,
        port: u16,
        tags: Vec<String>,
    }

    #[test]
    fn round_trips_struct_with_array() {
        let cfg = Config {
            name: "api".into(),
            port: 8080,
            tags: vec!["a".into(), "b".into()],
        };
        let xml = encode(&cfg);
        assert!(xml.contains("<name>api</name>"));
        assert!(xml.contains("<port>8080</port>"));
        assert!(xml.contains("<tags>a</tags><tags>b</tags>"));
        let back: Config = decode(&xml).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn escapes_and_unescapes_text() {
        let v = from_str("<root><msg>a &lt; b &amp; c</msg></root>").unwrap();
        assert_eq!(v.get("msg").unwrap().as_str(), Some("a < b & c"));
    }

    #[test]
    fn skips_declaration_and_comments() {
        let xml = "<?xml version=\"1.0\"?><!-- hi --><root><a>1</a></root>";
        let v = from_str(xml).unwrap();
        assert_eq!(v.get("a").unwrap().as_str(), Some("1"));
    }

    #[test]
    fn rejects_deeply_nested_input_without_stack_overflow() {
        let n = MAX_DEPTH + 50;
        let xml = format!("{}{}", "<a>".repeat(n), "</a>".repeat(n));
        assert!(from_str(&xml).is_err());
    }

    #[test]
    fn round_trips_zero_and_one_element_arrays() {
        // Empty array: encodes to an explicit empty array element.
        let empty = Config {
            name: "api".into(),
            port: 80,
            tags: vec![],
        };
        let xml = encode(&empty);
        assert!(xml.contains("<tags type=\"array\"/>"), "{xml}");
        let back: Config = decode(&xml).unwrap();
        assert_eq!(back, empty);

        // A plain empty element (hand-written, or from an older encoder) still
        // decodes to an empty vec.
        let back: Config =
            decode("<root><name>api</name><port>80</port><tags></tags></root>").unwrap();
        assert_eq!(back, empty);

        // Single-element array: a lone `<tags>solo</tags>` decodes to a scalar,
        // which the typed `Vec` decode reconstructs as a one-element vec.
        let one = Config {
            name: "api".into(),
            port: 80,
            tags: vec!["solo".into()],
        };
        let back: Config = decode(&encode(&one)).unwrap();
        assert_eq!(back, one);
    }

    #[test]
    fn unterminated_prolog_and_comments_error_instead_of_panicking() {
        for input in [
            "<?",
            "<?xml",
            "<?xml version=\"1.0\"",
            "<!--",
            "<!-- x",
            "<!---",
            "  <?",
            "<?xml?><!--",
            "<root><!--",
            "<root><!-- x --",
            "<root><?pi",
            "<root><![CDATA[x",
            "<root></root><!--",
            "<root></root><?",
            "<",
            "<root",
            "<root ",
            "<root a",
            "<root a=",
            "<root a=\"",
            "<root a=\"x",
            "<root>&",
            "<root>&amp",
            "<root>&#x",
            "</",
            "<root></",
            "<root></root",
        ] {
            let r = from_str(input);
            assert!(r.is_err(), "expected an error for {input:?}, got {r:?}");
        }
    }

    #[test]
    fn character_references_take_digits_only() {
        assert_eq!(
            from_str("<r>&#x41;&#66;&#X43;</r>").unwrap(),
            Value::Str("ABC".into())
        );
        for bad in [
            "<r>&#x+41;</r>",
            "<r>&#+65;</r>",
            "<r>&#x-41;</r>",
            "<r>&#-65;</r>",
            "<r>&#x;</r>",
            "<r>&#;</r>",
            "<r>&#xZZ;</r>",
            "<r>&#x110000;</r>",
            "<r>&#xD800;</r>",
            "<r>&nope;</r>",
            "<r>&#x41</r>",
        ] {
            assert!(from_str(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn attributes_are_skipped_quote_aware() {
        let v =
            from_str(r#"<root a=">" b='/>' c = "x y"><k d="</k>">v</k><e f="1"/></root>"#).unwrap();
        assert_eq!(v.get("k").and_then(Value::as_str), Some("v"));
        assert_eq!(v.get("e"), Some(&Value::Null));
        assert_eq!(v.as_object().unwrap().len(), 2);
        // Malformed attributes are errors.
        assert!(from_str("<root a>x</root>").is_err());
        assert!(from_str("<root a=b>x</root>").is_err());
        assert!(from_str("<root / >x</root>").is_err());
    }

    #[test]
    fn rejects_trailing_content_after_the_root() {
        assert!(from_str("<a>1</a><b>2</b>").is_err());
        assert!(from_str("<a>1</a>junk").is_err());
        assert!(from_str("<a/>x").is_err());
        assert!(from_str("<a>1</a></a>").is_err());
        // Trailing whitespace, comments and processing instructions are fine.
        assert_eq!(
            from_str("<a>1</a> \n<!-- bye --><?pi x?>\n").unwrap(),
            Value::Str("1".into())
        );
        assert!(from_str("").is_err());
        assert!(from_str("   ").is_err());
        assert!(from_str("<!DOCTYPE a><a/>").is_err());
    }

    #[test]
    fn rejects_mixed_content() {
        assert!(from_str("<a>text<b>1</b></a>").is_err());
        assert!(from_str("<a><b>1</b>text</a>").is_err());
        assert!(from_str("<a>\n  <b>1</b>\n</a>").is_ok());
    }

    #[test]
    fn map_keys_cannot_inject_markup() {
        let hostile = [
            "a><injected>1</injected><b",
            "a b",
            "1st",
            "",
            "_x_",
            "_x0041_",
            "a_x41_b",
            "x:y",
            "-dash",
            "tab\there",
            "é",
            "<",
            "a/",
            "k\u{0}",
            "max_x",
            "snake_case",
            "\u{1F600}",
        ];
        let obj = Value::Object(
            hostile
                .iter()
                .enumerate()
                .map(|(i, k)| (k.to_string(), Value::Str(format!("v{i}"))))
                .collect(),
        );
        let xml = encode(&obj);
        assert!(!xml.contains("<injected>"), "{xml}");
        let back = from_str(&xml).unwrap();
        // Every key survives exactly, and nothing extra was created.
        let entries = back.as_object().unwrap();
        assert_eq!(entries.len(), hostile.len(), "{xml}");
        for (i, k) in hostile.iter().enumerate() {
            assert_eq!(entries[i].0, *k, "{xml}");
            assert_eq!(entries[i].1, Value::Str(format!("v{i}")));
        }
        assert!(xml.contains("<a_x0020_b>"), "{xml}");
        assert!(xml.contains("<snake_case>"), "{xml}");
    }

    #[test]
    fn empty_and_padded_strings_use_xml_space() {
        for s in ["", " ", " a", "a ", "\ta\n", "  two  words  ", "\r"] {
            let v = Value::Object(vec![("s".into(), Value::Str(s.into()))]);
            let xml = encode(&v);
            assert!(xml.contains("xml:space=\"preserve\""), "{xml}");
            assert_eq!(from_str(&xml).unwrap(), v, "{xml}");
        }
        // Unpadded text needs no attribute, and literal padding is trimmed.
        assert_eq!(
            encode(&Value::Object(vec![("s".into(), "a b".into())])),
            "<root><s>a b</s></root>"
        );
        assert_eq!(
            from_str("<root><s>\n  a b\n</s></root>").unwrap().get("s"),
            Some(&Value::Str("a b".into()))
        );
        // The attribute is inherited by child elements, as XML specifies.
        assert_eq!(
            from_str(
                "<root xml:space=\"preserve\"><s> a </s><t xml:space=\"default\"> b </t></root>"
            )
            .unwrap(),
            Value::Object(vec![("s".into(), " a ".into()), ("t".into(), "b".into())])
        );
    }

    #[test]
    fn explicit_arrays_objects_and_nulls_are_exact() {
        let cases = [
            r#"{"a":[]}"#,
            r#"{"a":[null]}"#,
            r#"{"a":[null,null]}"#,
            r#"{"a":[[]]}"#,
            r#"{"a":[["1","2"],["3"]]}"#,
            r#"{"a":[[["x"]]]}"#,
            r#"{"a":{}}"#,
            r#"{"a":null}"#,
            r#"{"a":["",""]}"#,
            r#"{"a":[{"k":"v"},{"k":"w"}]}"#,
            r#"[]"#,
            r#"["1","2"]"#,
            r#"[["1"],[]]"#,
            r#"{}"#,
            r#"null"#,
            r#""""#,
            r#""x""#,
        ];
        for src in cases {
            let v = ferroly::codec::json::from_str(src).unwrap();
            let xml = encode(&v);
            assert_eq!(from_str(&xml).unwrap(), v, "{src} via {xml}");
        }
        assert_eq!(
            encode(&ferroly::codec::json::from_str(r#"{"m":[[1,2],[3]]}"#).unwrap()),
            "<root><m type=\"array\"><item type=\"array\"><item>1</item><item>2</item></item>\
             <item type=\"array\"><item>3</item></item></m></root>"
        );
    }

    #[test]
    fn literal_line_breaks_are_normalized_to_lf() {
        // As the XML specification requires: CRLF and a lone CR in the source
        // both read as LF, in text and in CDATA. Found by differential testing
        // against a conforming parser.
        let v = from_str(
            "<r xml:space=\"preserve\"><a>x\r\ny\rz</a><b><![CDATA[p\r\nq\rr]]></b><c>k&#13;l</c></r>",
        )
        .unwrap();
        assert_eq!(v.get("a").and_then(Value::as_str), Some("x\ny\nz"));
        assert_eq!(v.get("b").and_then(Value::as_str), Some("p\nq\nr"));
        // A character reference is not a literal line break, so it survives —
        // which is how the encoder keeps a CR intact.
        assert_eq!(v.get("c").and_then(Value::as_str), Some("k\rl"));
        let with_cr = Value::Object(vec![("s".into(), Value::Str("a\r\nb\rc".into()))]);
        assert_eq!(from_str(&encode(&with_cr)).unwrap(), with_cr);
        // A document saved with CRLF line endings parses like its LF twin.
        let lf = "<root>\n  <a>1</a>\n  <b>two\nlines</b>\n</root>\n";
        assert_eq!(
            from_str(&lf.replace('\n', "\r\n")).unwrap(),
            from_str(lf).unwrap()
        );
    }

    #[test]
    fn the_codec_attributes_may_not_repeat() {
        assert!(from_str(r#"<a type="array" type="object"/>"#).is_err());
        assert!(from_str(r#"<a xml:space="preserve" xml:space="default">x</a>"#).is_err());
        assert!(from_str(r#"<a><b type="x" k="1" type="array"/></a>"#).is_err());
        // Other attributes are skipped, repeated or not.
        assert_eq!(
            from_str(r#"<a k="1" k="2" type="object"/>"#).unwrap(),
            Value::Object(vec![])
        );
    }

    #[test]
    fn cdata_sections_are_literal_text() {
        let v = from_str("<r><a><![CDATA[x < y && z]]></a><b>p<![CDATA[]]>q</b></r>").unwrap();
        assert_eq!(v.get("a").and_then(Value::as_str), Some("x < y && z"));
        assert_eq!(v.get("b").and_then(Value::as_str), Some("pq"));
    }

    #[test]
    fn control_characters_round_trip_and_try_encode_rejects_them() {
        let v = Value::Object(vec![("s".into(), Value::Str("a\u{1}b\rc\u{ffff}".into()))]);
        let xml = encode(&v);
        assert!(xml.contains("&#x1;") && xml.contains("&#13;"), "{xml}");
        assert_eq!(from_str(&xml).unwrap(), v);
        assert!(matches!(
            try_encode(&v),
            Err(CodecError::Unrepresentable(_))
        ));
        let ok = Value::Object(vec![("s".into(), Value::Str("tab\tnl\ncr\r".into()))]);
        assert_eq!(try_encode(&ok).unwrap(), encode(&ok));
    }

    #[test]
    fn non_finite_floats_round_trip_through_text() {
        #[derive(Encode, Decode, Debug)]
        struct F {
            a: f64,
            b: f64,
            c: f64,
        }
        let back: F = decode(&encode(&F {
            a: f64::NAN,
            b: f64::INFINITY,
            c: f64::NEG_INFINITY,
        }))
        .unwrap();
        assert!(back.a.is_nan());
        assert_eq!(back.b, f64::INFINITY);
        assert_eq!(back.c, f64::NEG_INFINITY);
    }

    #[test]
    fn parse_errors_report_line_and_column() {
        let err = from_str("<root>\n  <a>1</b>\n</root>").unwrap_err();
        assert_eq!(
            err.to_string(),
            "parse error: mismatched tags: <a> vs </b> at line 2, column 7"
        );
        let err = from_str("<root>\n<a>&bogus;</a></root>").unwrap_err();
        assert!(err.to_string().ends_with("at line 2, column 4"), "{err}");
    }

    #[test]
    fn limits_bound_depth_size_and_entries() {
        let limits = Limits::new().with_max_depth(2);
        assert!(from_str_with("<a><b><c>1</c></b></a>", limits).is_ok());
        assert!(matches!(
            from_str_with("<a><b><c><d>1</d></c></b></a>", limits),
            Err(CodecError::LimitExceeded(_))
        ));
        let limits = Limits::new().with_max_entries(2);
        assert!(from_str_with("<a><b/><b/></a>", limits).is_ok());
        assert!(matches!(
            from_str_with("<a><b/><b/><b/></a>", limits),
            Err(CodecError::LimitExceeded(_))
        ));
        let limits = Limits::new().with_max_input_bytes(4);
        assert!(from_str_with("<a/>", limits).is_ok());
        assert!(from_str_with("<ab/>", limits).is_err());
        assert!(decode_with::<String>("<ab/>", limits).is_err());
        assert!(decode_from_slice_with::<String>(b"<ab/>", limits).is_err());
    }
}

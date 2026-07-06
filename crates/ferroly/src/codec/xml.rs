//! A pragmatic, dependency-free XML codec over the [`Value`] model.
//!
//! XML has no canonical mapping to a generic data model, so this codec uses a
//! straightforward convention geared at struct-like data:
//!
//! - An object becomes child elements: `{name: "x"}` → `<name>x</name>`.
//! - An array becomes repeated sibling elements sharing the field's tag:
//!   `{tags: ["a", "b"]}` → `<tags>a</tags><tags>b</tags>`.
//! - Scalars become escaped text; `null` becomes an empty element.
//! - The whole document is wrapped in a `<root>` element.
//!
//! Attributes, namespaces, mixed content, and top-level arrays are not modeled;
//! decoded scalars are always strings (the numeric/bool `Decode` impls
//! parse them). Use JSON where full fidelity matters.
//!
//! The parser scans bytes (structural characters and entities are ASCII),
//! slicing element names and clean text runs directly from the input.

use ferroly::codec::{CodecError, Decode, Encode, Value};

use super::fmt;

/// Encodes any [`Encode`] type to an XML string.
pub fn encode<T: Encode>(value: &T) -> String {
    let mut out = String::with_capacity(64);
    out.push_str("<root>");
    write_children(&mut out, "item", &value.encode());
    out.push_str("</root>");
    out
}

/// Encodes to XML bytes.
pub fn encode_to_vec<T: Encode>(value: &T) -> Vec<u8> {
    encode(value).into_bytes()
}

/// Decodes any [`Decode`] type from an XML string.
pub fn decode<T: Decode>(input: &str) -> Result<T, CodecError> {
    T::decode(&from_str(input)?)
}

/// Decodes from XML bytes.
pub fn decode_from_slice<T: Decode>(input: &[u8]) -> Result<T, CodecError> {
    let s = std::str::from_utf8(input).map_err(|e| CodecError::parse(e.to_string()))?;
    decode(s)
}

/// Parses an XML document into a [`Value`] (the root element's children).
pub fn from_str(input: &str) -> Result<Value, CodecError> {
    let mut p = Parser::new(input);
    p.skip_prolog();
    let (_tag, value) = p.parse_element()?;
    Ok(value)
}

// ---- encoder ----------------------------------------------------------

fn write_children(out: &mut String, item_tag: &str, value: &Value) {
    match value {
        Value::Object(o) => {
            for (k, v) in o {
                write_element(out, k, v);
            }
        }
        Value::Array(a) => {
            for v in a {
                write_element(out, item_tag, v);
            }
        }
        other => write_scalar(out, other),
    }
}

fn write_element(out: &mut String, tag: &str, value: &Value) {
    match value {
        Value::Array(a) => {
            if a.is_empty() {
                // Emit an empty element so an empty array round-trips: it decodes
                // to `Null`, which `Vec::decode` maps back to an empty vec.
                out.push('<');
                out.push_str(tag);
                out.push_str("></");
                out.push_str(tag);
                out.push('>');
            } else {
                // Repeat the element tag once per array item.
                for v in a {
                    write_element(out, tag, v);
                }
            }
        }
        Value::Object(_) => {
            out.push('<');
            out.push_str(tag);
            out.push('>');
            write_children(out, "item", value);
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
        Value::Null => {
            out.push('<');
            out.push_str(tag);
            out.push_str("></");
            out.push_str(tag);
            out.push('>');
        }
        scalar => {
            out.push('<');
            out.push_str(tag);
            out.push('>');
            write_scalar(out, scalar);
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
    }
}

fn write_scalar(out: &mut String, value: &Value) {
    match value {
        Value::Null => {}
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => fmt::write_i64(out, *i),
        Value::UInt(u) => fmt::write_u64(out, *u),
        Value::Float(f) => fmt::write_f64(out, *f),
        Value::Str(s) => escape_text(out, s),
        Value::Bytes(b) => {
            for (i, byte) in b.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                fmt::write_u64(out, *byte as u64);
            }
        }
        // Containers are handled by write_element/write_children.
        Value::Array(_) | Value::Object(_) => {}
    }
}

fn escape_text(out: &mut String, s: &str) {
    let bytes = s.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let esc = match b {
            b'<' => "&lt;",
            b'>' => "&gt;",
            b'&' => "&amp;",
            _ => continue,
        };
        out.push_str(&s[start..i]);
        out.push_str(esc);
        start = i + 1;
    }
    out.push_str(&s[start..]);
}

// ---- parser --------------------------------------------------------------

/// Maximum element nesting depth; bounds stack usage so hostile deeply-nested
/// input (`<a><a><a>…`) is rejected instead of overflowing the stack.
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

    /// Skips the XML declaration, comments, and whitespace before the root.
    fn skip_prolog(&mut self) {
        loop {
            self.skip_ws();
            if self.starts_with(b"<?") {
                while self.pos < self.bytes.len() && !self.starts_with(b"?>") {
                    self.pos += 1;
                }
                self.pos += 2; // consume "?>"
            } else if self.starts_with(b"<!--") {
                self.skip_comment();
            } else {
                break;
            }
        }
    }

    fn skip_comment(&mut self) {
        self.pos += 4; // "<!--"
        while self.pos < self.bytes.len() && !self.starts_with(b"-->") {
            self.pos += 1;
        }
        self.pos += 3; // "-->"
    }

    /// Parses a single element, returning `(tag, value)`.
    fn parse_element(&mut self) -> Result<(String, Value), CodecError> {
        self.skip_ws();
        if self.peek() != Some(b'<') {
            return Err(CodecError::parse("expected '<' to start element"));
        }
        self.pos += 1;
        let tag = self.read_name()?;
        // Skip any attributes up to '>' or '/>'.
        while let Some(b) = self.peek() {
            if b == b'>' || (b == b'/' && self.bytes.get(self.pos + 1) == Some(&b'>')) {
                break;
            }
            self.pos += 1;
        }
        if self.starts_with(b"/>") {
            self.pos += 2;
            return Ok((tag, Value::Null));
        }
        if self.peek() != Some(b'>') {
            return Err(CodecError::parse("malformed start tag"));
        }
        self.pos += 1; // consume '>'

        let value = self.parse_content(&tag)?;
        Ok((tag, value))
    }

    /// Parses element content up to and including the matching close tag.
    fn parse_content(&mut self, tag: &str) -> Result<Value, CodecError> {
        let mut children: Vec<(String, Value)> = Vec::new();
        let mut text = String::new();
        let mut run = self.pos; // start of the current clean text run

        loop {
            match self.peek() {
                None => return Err(CodecError::parse("unexpected end of input")),
                Some(b'<') => {
                    text.push_str(&self.input[run..self.pos]);
                    if self.starts_with(b"<!--") {
                        self.skip_comment();
                        run = self.pos;
                        continue;
                    }
                    if self.starts_with(b"</") {
                        self.pos += 2;
                        let close = self.read_name()?;
                        self.skip_ws();
                        if self.peek() != Some(b'>') {
                            return Err(CodecError::parse("malformed close tag"));
                        }
                        self.pos += 1;
                        if close != tag {
                            return Err(CodecError::parse(format!(
                                "mismatched tags: <{tag}> vs </{close}>"
                            )));
                        }
                        break;
                    }
                    self.depth += 1;
                    if self.depth > MAX_DEPTH {
                        return Err(CodecError::parse("nesting too deep"));
                    }
                    let child = self.parse_element();
                    self.depth -= 1;
                    let (child_tag, child_val) = child?;
                    push_child(&mut children, child_tag, child_val);
                    run = self.pos;
                }
                Some(b'&') => {
                    text.push_str(&self.input[run..self.pos]);
                    self.pos += 1; // consume '&'
                    text.push(self.read_entity()?);
                    run = self.pos;
                }
                Some(_) => self.pos += 1,
            }
        }

        if children.is_empty() {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                Ok(Value::Null)
            } else {
                Ok(Value::Str(trimmed.to_string()))
            }
        } else {
            Ok(Value::Object(children))
        }
    }

    fn read_name(&mut self) -> Result<String, CodecError> {
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
            return Err(CodecError::parse("expected element name"));
        }
        Ok(self.input[start..self.pos].to_string())
    }

    /// Reads an entity reference body (after `&`), consuming the trailing `;`.
    fn read_entity(&mut self) -> Result<char, CodecError> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b == b';' {
                break;
            }
            self.pos += 1;
        }
        let ent = &self.input[start..self.pos];
        if self.peek() == Some(b';') {
            self.pos += 1;
        }
        Ok(match ent {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                u32::from_str_radix(&ent[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
                    .ok_or_else(|| CodecError::parse("bad char reference"))?
            }
            _ if ent.starts_with('#') => ent[1..]
                .parse::<u32>()
                .ok()
                .and_then(char::from_u32)
                .ok_or_else(|| CodecError::parse("bad char reference"))?,
            _ => return Err(CodecError::parse(format!("unknown entity &{ent};"))),
        })
    }
}

/// Inserts a child, collapsing repeated tags into an array. A linear scan over
/// `children`: element field counts are small, so this beats a per-element
/// hash map (which would allocate for every element).
fn push_child(children: &mut Vec<(String, Value)>, tag: String, value: Value) {
    if let Some((_, existing)) = children.iter_mut().find(|(k, _)| *k == tag) {
        match existing {
            Value::Array(a) => a.push(value),
            _ => {
                let prev = std::mem::replace(existing, Value::Null);
                *existing = Value::Array(vec![prev, value]);
            }
        }
    } else {
        children.push((tag, value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroly::codec::{Decode, Encode};

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
        // Empty array: encodes to an empty element, decodes back to an empty vec.
        let empty = Config {
            name: "api".into(),
            port: 80,
            tags: vec![],
        };
        let back: Config = decode(&encode(&empty)).unwrap();
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
}

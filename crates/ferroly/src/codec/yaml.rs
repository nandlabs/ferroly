//! A dependency-free YAML codec covering the common block-style subset.
//!
//! Supported: block mappings (`key: value`), block sequences (`- item`), nested
//! structure by indentation, scalars (strings, ints, floats, bools, null), and
//! single/double-quoted strings. The encoder always emits block style.
//!
//! Not supported (use JSON for these): anchors/aliases, tags, flow style
//! (`{a: 1}` / `[1, 2]` except the empty forms `{}`/`[]`), multi-document
//! streams, and block scalars (`|` / `>`). Inline mapping items in a sequence
//! (`- key: value`) parse only their first key.
//!
//! The parser borrows line slices from the input (no per-line allocation) and
//! the encoder appends scalars in place via the shared `fmt` writers.

use ferroly::codec::{CodecError, Decode, Encode, Value};

use super::fmt;

/// Encodes any [`Encode`] type to a YAML string.
pub fn encode<T: Encode>(value: &T) -> String {
    let mut out = String::with_capacity(64);
    write_block(&mut out, &value.encode(), 0);
    out
}

/// Encodes to YAML bytes.
pub fn encode_to_vec<T: Encode>(value: &T) -> Vec<u8> {
    encode(value).into_bytes()
}

/// Decodes any [`Decode`] type from a YAML string.
pub fn decode<T: Decode>(input: &str) -> Result<T, CodecError> {
    T::decode(&from_str(input)?)
}

/// Decodes from YAML bytes.
pub fn decode_from_slice<T: Decode>(input: &[u8]) -> Result<T, CodecError> {
    let s = std::str::from_utf8(input).map_err(|e| CodecError::parse(e.to_string()))?;
    decode(s)
}

/// Parses a YAML document into a [`Value`].
pub fn from_str(input: &str) -> Result<Value, CodecError> {
    let lines = preprocess(input);
    if lines.is_empty() {
        return Ok(Value::Null);
    }
    let mut idx = 0;
    let base = lines[0].0;
    parse_block(&lines, &mut idx, base, 0)
}

// ---- encoder ----------------------------------------------------------

fn write_block(out: &mut String, value: &Value, indent: usize) {
    match value {
        Value::Object(o) if !o.is_empty() => {
            for (k, v) in o {
                pad(out, indent);
                write_key(out, k);
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
        Value::Object(_) => out.push_str(" {}\n"),
        Value::Array(_) => out.push_str(" []\n"),
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

/// Appends a scalar directly into `out` (no intermediate `String`).
fn write_scalar(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => fmt::write_i64(out, *i),
        Value::UInt(u) => fmt::write_u64(out, *u),
        Value::Float(f) => fmt::write_f64(out, *f),
        Value::Str(s) => {
            if needs_quote(s) {
                write_quoted(out, s);
            } else {
                out.push_str(s);
            }
        }
        Value::Bytes(b) => {
            let mut inner = String::new();
            for (i, x) in b.iter().enumerate() {
                if i > 0 {
                    inner.push(',');
                }
                fmt::write_u64(&mut inner, *x as u64);
            }
            write_quoted(out, &inner);
        }
        // Containers are handled by write_block/write_after_marker.
        Value::Array(_) | Value::Object(_) => out.push_str("null"),
    }
}

fn write_key(out: &mut String, k: &str) {
    if needs_quote(k) {
        write_quoted(out, k);
    } else {
        out.push_str(k);
    }
}

fn needs_quote(s: &str) -> bool {
    if s.is_empty() || s != s.trim() {
        return true;
    }
    if s.contains([':', '#', '\n', '"', '\'']) {
        return true;
    }
    if s.starts_with([
        '-', '?', '[', ']', '{', '}', '&', '*', '!', '|', '>', '%', '@', '`',
    ]) {
        return true;
    }
    // Would otherwise be read back as a non-string scalar.
    matches!(s, "null" | "~" | "true" | "false")
        || s.parse::<i64>().is_ok()
        || s.parse::<f64>().is_ok()
}

/// Writes a double-quoted scalar, bulk-copying runs that need no escape.
fn write_quoted(out: &mut String, s: &str) {
    out.push('"');
    let bytes = s.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let esc = match b {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
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

/// Splits significant lines into `(indent, content)`, borrowing from `input`.
fn preprocess(input: &str) -> Vec<(usize, &str)> {
    let mut lines = Vec::new();
    for raw in input.lines() {
        let content = raw.trim_start();
        if content.is_empty() || content.starts_with('#') || content == "---" {
            continue;
        }
        let indent = raw.len() - content.len();
        lines.push((indent, content.trim_end()));
    }
    lines
}

/// Maximum block nesting depth; bounds recursion/stack on hostile input.
const MAX_DEPTH: usize = 128;

fn parse_block(
    lines: &[(usize, &str)],
    idx: &mut usize,
    indent: usize,
    depth: usize,
) -> Result<Value, CodecError> {
    if depth > MAX_DEPTH {
        return Err(CodecError::parse("nesting too deep"));
    }
    let content = lines[*idx].1;
    if content.starts_with('-') {
        parse_sequence(lines, idx, indent, depth)
    } else if mapping_key(content).is_some() {
        parse_mapping(lines, idx, indent, depth)
    } else {
        *idx += 1;
        Ok(parse_scalar(content))
    }
}

fn parse_mapping(
    lines: &[(usize, &str)],
    idx: &mut usize,
    indent: usize,
    depth: usize,
) -> Result<Value, CodecError> {
    let mut entries = Vec::new();
    while *idx < lines.len() {
        let (ind, content) = lines[*idx];
        if ind < indent || content.starts_with('-') {
            break;
        }
        if ind > indent {
            return Err(CodecError::parse("unexpected indentation in mapping"));
        }
        let (key, rest) = mapping_key(content)
            .ok_or_else(|| CodecError::parse(format!("expected 'key: value', got: {content}")))?;
        *idx += 1;
        if rest.is_empty() {
            if *idx < lines.len() && lines[*idx].0 > indent {
                let child_indent = lines[*idx].0;
                entries.push((key, parse_block(lines, idx, child_indent, depth + 1)?));
            } else {
                entries.push((key, Value::Null));
            }
        } else {
            entries.push((key, parse_scalar(rest)));
        }
    }
    Ok(Value::Object(entries))
}

fn parse_sequence(
    lines: &[(usize, &str)],
    idx: &mut usize,
    indent: usize,
    depth: usize,
) -> Result<Value, CodecError> {
    let mut items = Vec::new();
    while *idx < lines.len() {
        let (ind, content) = lines[*idx];
        if ind < indent || !content.starts_with('-') {
            break;
        }
        if ind > indent {
            return Err(CodecError::parse("unexpected indentation in sequence"));
        }
        let after = content[1..].trim_start();
        if after.is_empty() {
            *idx += 1;
            if *idx < lines.len() && lines[*idx].0 > indent {
                let child_indent = lines[*idx].0;
                items.push(parse_block(lines, idx, child_indent, depth + 1)?);
            } else {
                items.push(Value::Null);
            }
        } else if let Some((key, rest)) = mapping_key(after) {
            // Inline single-key mapping item (`- key: value`).
            *idx += 1;
            items.push(Value::Object(vec![(key, parse_scalar(rest))]));
        } else {
            *idx += 1;
            items.push(parse_scalar(after));
        }
    }
    Ok(Value::Array(items))
}

/// Splits `key: value` at the first `:` followed by a space or end-of-line.
/// Returns the (unquoted) key and a borrowed slice of the remainder.
fn mapping_key(s: &str) -> Option<(String, &str)> {
    let bytes = s.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b':' {
            let next = bytes.get(i + 1);
            if next.is_none() || next == Some(&b' ') {
                let key = unquote(s[..i].trim());
                let rest = s[i + 1..].trim();
                return Some((key, rest));
            }
        }
    }
    None
}

fn parse_scalar(s: &str) -> Value {
    let t = s.trim();
    if (t.starts_with('"') && t.ends_with('"') && t.len() >= 2)
        || (t.starts_with('\'') && t.ends_with('\'') && t.len() >= 2)
    {
        return Value::Str(unquote(t));
    }
    match t {
        "" | "null" | "~" => Value::Null,
        // Empty flow collections — symmetric with the encoder, which emits
        // `key: []` / `key: {}` for empty arrays/objects.
        "[]" => Value::Array(Vec::new()),
        "{}" => Value::Object(Vec::new()),
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => {
            if let Ok(i) = t.parse::<i64>() {
                Value::Int(i)
            } else if let Ok(u) = t.parse::<u64>() {
                Value::UInt(u)
            } else if let Ok(f) = t.parse::<f64>() {
                Value::Float(f)
            } else {
                Value::Str(t.to_string())
            }
        }
    }
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        let inner = &t[1..t.len() - 1];
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some(other) => out.push(other),
                    None => {}
                }
            } else {
                out.push(c);
            }
        }
        out
    } else if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
        t[1..t.len() - 1].to_string()
    } else {
        t.to_string()
    }
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
    }
}

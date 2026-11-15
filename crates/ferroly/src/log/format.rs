//! Record rendering: timestamps, the plain and JSON line layouts, and the
//! escaping that keeps one record on one line.
//!
//! The timestamp, quoting and field-resolution rules live in the crate's
//! `textfmt` module, shared with the `obs` plain-text span exporter, so both
//! print the same value the same way.

use std::fmt::Write as _;

use ferroly::codec::Value;

use super::redact::Redactor;
use super::Level;
pub use crate::textfmt::Precision;
use crate::textfmt::{is_break, write_json_value, write_plain_fields, write_token, NESTED_KEY};
pub(super) use crate::textfmt::{rfc3339, Field};

// ---- escaping ----------------------------------------------------------------

/// Writes a plain-format message: free text that must stay on one line. Line
/// breaks of every kind become a space (as they always have); other control
/// characters are written as `\uXXXX`. Tabs are kept.
fn write_message(out: &mut String, msg: &str) {
    if !msg.chars().any(|c| is_break(c) && c != '\t') {
        out.push_str(msg);
        return;
    }
    for c in msg.chars() {
        match c {
            '\n' | '\r' | '\u{0b}' | '\u{0c}' | '\u{85}' | '\u{2028}' | '\u{2029}' => out.push(' '),
            '\t' => out.push('\t'),
            c if is_break(c) => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

/// Writes a JSON string literal.
fn write_json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if is_break(c) => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---- field resolution ----------------------------------------------------------

/// Turns the raw field sources (in precedence order, later wins) into the
/// fields to print: values redacted, repeated keys collapsed to the last value
/// (keeping the first position), and keys that collide with `reserved` moved
/// to the nested list.
pub(super) fn resolve<'a>(
    reserved: &dyn Fn(&str) -> bool,
    redactor: &Redactor,
    sources: impl Iterator<Item = (&'a str, &'a Value)>,
) -> (Vec<Field<'a>>, Vec<Field<'a>>) {
    crate::textfmt::resolve(reserved, &|key, value| redactor.apply(key, value), sources)
}

// ---- line layouts ----------------------------------------------------------------

/// Everything one record prints.
pub(super) struct Record<'a> {
    pub ts: &'a str,
    pub level: Level,
    pub msg: &'a str,
    pub target: Option<&'a str>,
    pub caller: Option<&'a str>,
    pub top: &'a [Field<'a>],
    pub nested: &'a [Field<'a>],
}

/// `ts LEVEL message [target=…] [caller=…] key=value … [fields.key=value …]`,
/// newline-terminated.
pub(super) fn plain_line(rec: &Record<'_>) -> String {
    let mut s = String::with_capacity(64 + rec.msg.len());
    let _ = write!(s, "{} {:<5} ", rec.ts, rec.level.as_upper_str());
    write_message(&mut s, rec.msg);
    if let Some(target) = rec.target {
        s.push_str(" target=");
        write_token(&mut s, target);
    }
    if let Some(caller) = rec.caller {
        s.push_str(" caller=");
        write_token(&mut s, caller);
    }
    write_plain_fields(&mut s, rec.top, rec.nested);
    s.push('\n');
    s
}

/// One JSON object, newline-terminated:
/// `{"ts":…,"level":…,"msg":…[,"target":…][,"caller":…],<fields>[,"fields":{…}]}`.
pub(super) fn json_line(rec: &Record<'_>) -> String {
    let mut s = String::with_capacity(96 + rec.msg.len());
    s.push_str("{\"ts\":");
    write_json_str(&mut s, rec.ts);
    s.push_str(",\"level\":\"");
    s.push_str(rec.level.as_str());
    s.push_str("\",\"msg\":");
    write_json_str(&mut s, rec.msg);
    if let Some(target) = rec.target {
        s.push_str(",\"target\":");
        write_json_str(&mut s, target);
    }
    if let Some(caller) = rec.caller {
        s.push_str(",\"caller\":");
        write_json_str(&mut s, caller);
    }
    for (k, v) in rec.top {
        s.push(',');
        write_json_str(&mut s, k);
        s.push(':');
        write_json_value(&mut s, v);
    }
    if !rec.nested.is_empty() {
        s.push_str(",\"");
        s.push_str(NESTED_KEY);
        s.push_str("\":{");
        for (i, (k, v)) in rec.nested.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            write_json_str(&mut s, k);
            s.push(':');
            write_json_value(&mut s, v);
        }
        s.push('}');
    }
    s.push_str("}\n");
    s
}

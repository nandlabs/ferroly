//! Record rendering: timestamps, the plain and JSON line layouts, and the
//! escaping that keeps one record on one line.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use ferroly::codec::{json, Value};

use super::redact::Redactor;
use super::Level;

/// The number of fractional-second digits in a record's timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Precision {
    /// Whole seconds: `2026-07-05T12:34:56Z`.
    Seconds,
    /// Milliseconds (the default): `2026-07-05T12:34:56.789Z`.
    #[default]
    Millis,
    /// Microseconds: `2026-07-05T12:34:56.789012Z`.
    Micros,
    /// Nanoseconds: `2026-07-05T12:34:56.789012345Z`.
    Nanos,
}

/// The largest UTC offset RFC 3339 can express (`±23:59`), in seconds.
const MAX_OFFSET_SECS: i32 = 23 * 3600 + 59 * 60;

/// Formats a `SystemTime` as RFC 3339, dependency-free. `offset_secs` shifts
/// the wall-clock fields east of UTC and is rendered as `+HH:MM` (`Z` for 0).
pub(super) fn rfc3339(t: SystemTime, precision: Precision, offset_secs: i32) -> String {
    let dur = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    // Whole minutes only: RFC 3339 offsets have no seconds field.
    let offset = offset_secs.clamp(-MAX_OFFSET_SECS, MAX_OFFSET_SECS) / 60 * 60;
    let local = dur.as_secs() as i64 + i64::from(offset);
    let days = local.div_euclid(86_400);
    let rem = local.rem_euclid(86_400);
    let (hour, min, sec) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days);
    let mut out = String::with_capacity(35);
    let _ = write!(
        out,
        "{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}"
    );
    let nanos = dur.subsec_nanos();
    match precision {
        Precision::Seconds => {}
        Precision::Millis => {
            let _ = write!(out, ".{:03}", nanos / 1_000_000);
        }
        Precision::Micros => {
            let _ = write!(out, ".{:06}", nanos / 1_000);
        }
        Precision::Nanos => {
            let _ = write!(out, ".{nanos:09}");
        }
    }
    if offset == 0 {
        out.push('Z');
    } else {
        let sign = if offset < 0 { '-' } else { '+' };
        let abs = offset.unsigned_abs();
        let _ = write!(out, "{sign}{:02}:{:02}", abs / 3600, (abs % 3600) / 60);
    }
    out
}

/// Days-since-1970 → `(year, month, day)` (Howard Hinnant's algorithm).
pub(super) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if month <= 2 { y + 1 } else { y }, month, day)
}

// ---- escaping ----------------------------------------------------------------

/// Characters that can end or split a line in some consumer: every control
/// character (C0, DEL, C1 — which includes NEL) plus the Unicode line and
/// paragraph separators.
fn is_break(c: char) -> bool {
    c.is_control() || c == '\u{2028}' || c == '\u{2029}'
}

/// True when `s` cannot be written bare in the plain format: it is empty, or
/// holds whitespace, a control character, or one of the delimiter characters
/// `=`, `"`, `\`.
fn needs_quotes(s: &str) -> bool {
    s.is_empty()
        || s.chars()
            .any(|c| c.is_whitespace() || is_break(c) || matches!(c, '"' | '=' | '\\'))
}

/// Writes a plain-format key or string value: bare when safe, otherwise
/// double-quoted with `\"`, `\\`, `\n`, `\r`, `\t` and `\uXXXX` escapes.
fn write_token(out: &mut String, s: &str) {
    if !needs_quotes(s) {
        out.push_str(s);
        return;
    }
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

/// Appends compact JSON for `v`. The codec escapes C0 controls; this also
/// escapes DEL, the C1 controls and U+2028/U+2029, which some line-oriented
/// consumers treat as line breaks. Those characters can only occur inside JSON
/// strings, where a `\uXXXX` escape is always valid.
fn write_json_value(out: &mut String, v: &Value) {
    let encoded = json::to_string(v);
    if !encoded.chars().any(is_break) {
        out.push_str(&encoded);
        return;
    }
    for c in encoded.chars() {
        if is_break(c) {
            let _ = write!(out, "\\u{:04x}", c as u32);
        } else {
            out.push(c);
        }
    }
}

// ---- field resolution ----------------------------------------------------------

/// One resolved field: a borrowed key and a value that is borrowed unless
/// redaction replaced it.
pub(super) type Field<'a> = (&'a str, Cow<'a, Value>);

/// The name of the JSON object (and plain-format key prefix) that receives
/// fields whose keys collide with the record's own keys.
pub(super) const NESTED_KEY: &str = "fields";

fn upsert<'a>(list: &mut Vec<Field<'a>>, key: &'a str, value: Cow<'a, Value>) {
    match list.iter_mut().find(|(k, _)| *k == key) {
        Some(slot) => slot.1 = value,
        None => list.push((key, value)),
    }
}

/// Turns the raw field sources (in precedence order, later wins) into the
/// fields to print: values redacted, repeated keys collapsed to the last value
/// (keeping the first position), and keys that collide with `reserved` moved
/// to the nested list.
pub(super) fn resolve<'a>(
    reserved: &dyn Fn(&str) -> bool,
    redactor: &Redactor,
    sources: impl Iterator<Item = (&'a str, &'a Value)>,
) -> (Vec<Field<'a>>, Vec<Field<'a>>) {
    let mut top: Vec<Field<'a>> = Vec::new();
    let mut nested: Vec<Field<'a>> = Vec::new();
    for (key, value) in sources {
        let value = redactor.apply(key, value);
        if reserved(key) {
            upsert(&mut nested, key, value);
        } else {
            upsert(&mut top, key, value);
        }
    }
    // When the nested object is needed, a user field that is itself called
    // `fields` moves inside it, so the top level never has two `fields` keys.
    if !nested.is_empty() {
        if let Some(pos) = top.iter().position(|(k, _)| *k == NESTED_KEY) {
            let (key, value) = top.remove(pos);
            upsert(&mut nested, key, value);
        }
    }
    (top, nested)
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

fn write_plain_value(out: &mut String, v: &Value) {
    match v {
        Value::Str(s) => write_token(out, s),
        other => write_json_value(out, other),
    }
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
    for (k, v) in rec.top {
        s.push(' ');
        write_token(&mut s, k);
        s.push('=');
        write_plain_value(&mut s, v);
    }
    for (k, v) in rec.nested {
        s.push(' ');
        // The prefixed key is quoted as a whole when the key needs it.
        if needs_quotes(k) {
            write_token(&mut s, &format!("{NESTED_KEY}.{k}"));
        } else {
            s.push_str(NESTED_KEY);
            s.push('.');
            s.push_str(k);
        }
        s.push('=');
        write_plain_value(&mut s, v);
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn civil_date_known_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(18_993), (2022, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn timestamp_precision_and_offset() {
        // 2026-07-05T12:34:56.789012300Z. The sub-second part is a multiple
        // of 100 ns because Windows `SystemTime` cannot hold finer values.
        let t = UNIX_EPOCH + Duration::new(1_783_254_896, 789_012_300);
        assert_eq!(rfc3339(t, Precision::Seconds, 0), "2026-07-05T12:34:56Z");
        assert_eq!(rfc3339(t, Precision::Millis, 0), "2026-07-05T12:34:56.789Z");
        assert_eq!(
            rfc3339(t, Precision::Micros, 0),
            "2026-07-05T12:34:56.789012Z"
        );
        assert_eq!(
            rfc3339(t, Precision::Nanos, 0),
            "2026-07-05T12:34:56.789012300Z"
        );
        assert_eq!(
            rfc3339(t, Precision::Millis, 5 * 3600 + 30 * 60),
            "2026-07-05T18:04:56.789+05:30"
        );
        assert_eq!(
            rfc3339(t, Precision::Seconds, -13 * 3600),
            "2026-07-04T23:34:56-13:00"
        );
        // An offset that reaches back before the epoch still formats.
        assert_eq!(
            rfc3339(UNIX_EPOCH, Precision::Seconds, -3600),
            "1969-12-31T23:00:00-01:00"
        );
    }

    #[test]
    fn tokens_are_quoted_only_when_needed() {
        let render = |s: &str| {
            let mut out = String::new();
            write_token(&mut out, s);
            out
        };
        assert_eq!(render("user:42"), "user:42");
        assert_eq!(render("/a/b?c"), "/a/b?c");
        assert_eq!(render(""), "\"\"");
        assert_eq!(render("a b"), "\"a b\"");
        assert_eq!(render("a=b"), "\"a=b\"");
        assert_eq!(render("a\"b"), "\"a\\\"b\"");
        assert_eq!(render("a\\b"), "\"a\\\\b\"");
        assert_eq!(render("a\nb\rc\td"), "\"a\\nb\\rc\\td\"");
        assert_eq!(render("a\u{1b}[0m"), "\"a\\u001b[0m\"");
        assert_eq!(render("a\u{2028}b\u{85}c"), "\"a\\u2028b\\u0085c\"");
    }
}

#![cfg(feature = "codec")]
//! Parse-time scaling: a document with many distinct keys must parse in
//! near-linear time. The XML and TOML parsers used to scan every existing key
//! on each insert, which made 50k keys take many seconds (quadratic); these
//! tests fail loudly if that behaviour comes back.
//!
//! The bound is deliberately loose — a linear parse of these inputs takes tens
//! of milliseconds even in an unoptimized build, while the quadratic version
//! needs well over ten seconds — so a slow CI machine cannot trip it.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use ferroly::codec::{json, toml, xml, yaml, CodecError, Limits, Value};

const KEYS: usize = 50_000;
const BOUND: Duration = Duration::from_secs(5);

fn timed<T>(what: &str, f: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let out = f();
    let elapsed = start.elapsed();
    assert!(
        elapsed < BOUND,
        "{what} took {elapsed:?} for {KEYS} keys (bound {BOUND:?})"
    );
    out
}

fn assert_all_keys(v: &Value) {
    let entries = v.as_object().expect("an object");
    assert_eq!(entries.len(), KEYS);
    assert_eq!(entries[0].0, "k0");
    assert_eq!(entries[KEYS - 1].0, format!("k{}", KEYS - 1));
}

#[test]
fn xml_with_50k_distinct_children_parses_quickly() {
    let mut doc = String::from("<root>");
    for i in 0..KEYS {
        let _ = write!(doc, "<k{i}>{i}</k{i}>");
    }
    doc.push_str("</root>");
    let v = timed("xml::from_str", || xml::from_str(&doc).unwrap());
    assert_all_keys(&v);
    assert_eq!(v.get("k123").and_then(Value::as_str), Some("123"));
}

#[test]
fn xml_with_50k_repeated_children_parses_quickly() {
    // The other shape: one tag repeated, interleaved with distinct ones.
    let mut doc = String::from("<root>");
    for i in 0..KEYS {
        let _ = write!(doc, "<item>{i}</item><u{i}/>");
    }
    doc.push_str("</root>");
    let v = timed("xml::from_str", || xml::from_str(&doc).unwrap());
    assert_eq!(
        v.get("item").and_then(Value::as_array).map(Vec::len),
        Some(KEYS)
    );
    assert_eq!(v.as_object().map(Vec::len), Some(KEYS + 1));
}

#[test]
fn toml_with_50k_keys_in_one_table_parses_quickly() {
    let mut doc = String::new();
    for i in 0..KEYS {
        let _ = writeln!(doc, "k{i} = {i}");
    }
    let v = timed("toml::from_str", || toml::from_str(&doc).unwrap());
    assert_all_keys(&v);
    assert_eq!(v.get("k123"), Some(&Value::Int(123)));
}

#[test]
fn toml_with_50k_tables_parses_quickly() {
    // Every key/value line used to re-resolve its table from the root.
    let mut doc = String::new();
    for i in 0..KEYS {
        let _ = writeln!(doc, "[k{i}]\na = {i}\nb.c = true");
    }
    let v = timed("toml::from_str", || toml::from_str(&doc).unwrap());
    assert_all_keys(&v);
    assert_eq!(v.pointer("/k123/a"), Some(&Value::Int(123)));
    assert_eq!(v.pointer("/k123/b/c"), Some(&Value::Bool(true)));
}

#[test]
fn toml_with_50k_dotted_keys_and_array_tables_parses_quickly() {
    let mut doc = String::new();
    for i in 0..KEYS {
        let _ = writeln!(doc, "wide.k{i} = {i}");
    }
    for i in 0..KEYS {
        let _ = writeln!(doc, "[[rows]]\nid = {i}");
    }
    let v = timed("toml::from_str", || toml::from_str(&doc).unwrap());
    assert_all_keys(v.get("wide").unwrap());
    assert_eq!(
        v.get("rows").and_then(Value::as_array).map(Vec::len),
        Some(KEYS)
    );
}

#[test]
fn toml_inline_table_with_50k_keys_parses_quickly() {
    let mut doc = String::from("t = { ");
    for i in 0..KEYS {
        if i > 0 {
            doc.push_str(", ");
        }
        let _ = write!(doc, "k{i} = {i}");
    }
    doc.push_str(" }\n");
    let v = timed("toml::from_str", || toml::from_str(&doc).unwrap());
    assert_all_keys(v.get("t").unwrap());
}

#[test]
fn json_and_yaml_with_50k_keys_parse_quickly() {
    let mut j = String::from("{");
    let mut y = String::new();
    for i in 0..KEYS {
        if i > 0 {
            j.push(',');
        }
        let _ = write!(j, "\"k{i}\":{i}");
        let _ = writeln!(y, "k{i}: {i}");
    }
    j.push('}');
    assert_all_keys(&timed("json::from_str", || json::from_str(&j).unwrap()));
    assert_all_keys(&timed("yaml::from_str", || yaml::from_str(&y).unwrap()));
}

#[test]
fn merging_wide_objects_is_not_quadratic() {
    let wide = |offset: i64| {
        Value::Object(
            (0..KEYS as i64)
                .map(|i| (format!("k{i}"), Value::Int(i + offset)))
                .collect(),
        )
    };
    let mut base = wide(0);
    let overlay = wide(1);
    timed("Value::merge", || base.merge(overlay));
    assert_all_keys(&base);
    assert_eq!(base.get("k10"), Some(&Value::Int(11)));
}

#[test]
fn the_entry_limit_stops_a_wide_document_early() {
    let limits = Limits::new().with_max_entries(1_000);
    let mut x = String::from("<root>");
    let mut t = String::new();
    let mut y = String::new();
    let mut j = String::from("[");
    for i in 0..KEYS {
        let _ = write!(x, "<k{i}/>");
        let _ = writeln!(t, "k{i} = 1");
        let _ = writeln!(y, "k{i}: 1");
        let _ = write!(j, "{i},");
    }
    x.push_str("</root>");
    j.push_str("0]");
    for result in [
        xml::from_str_with(&x, limits),
        toml::from_str_with(&t, limits),
        yaml::from_str_with(&y, limits),
        json::from_str_with(&j, limits),
    ] {
        assert!(matches!(result, Err(CodecError::LimitExceeded(_))));
    }
    // And the streaming JSON decoder is bounded the same way.
    assert!(matches!(
        json::decode_with::<Vec<u32>>(&j, limits),
        Err(CodecError::LimitExceeded(_))
    ));
}

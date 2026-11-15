#![cfg(feature = "codec")]
//! The JSON-Schema `pattern` keyword through the public schema API: matching
//! semantics, violation paths, compile-time rejection of unsupported syntax,
//! the size caps, and linear-time matching of pathological patterns.

use std::time::{Duration, Instant};

use ferroly::codec::schema::{self, Schema, SchemaError};
use ferroly::codec::{json, Value};

fn v(s: &str) -> Value {
    json::from_str(s).unwrap()
}

fn compile(s: &str) -> Schema {
    Schema::compile(&v(s)).unwrap_or_else(|e| panic!("{s}: {e}"))
}

fn compile_err(s: &str) -> SchemaError {
    Schema::compile(&v(s)).expect_err(s)
}

fn s(text: &str) -> Value {
    Value::Str(text.to_string())
}

#[test]
fn a_string_failing_pattern_is_a_violation_with_its_path() {
    let schema = compile(
        r#"{
            "type": "object",
            "properties": {
                "sku":  { "type": "string", "pattern": "^[A-Z]{3}-\\d{4}$" },
                "tags": { "type": "array", "items": { "pattern": "^[a-z]+$" } }
            }
        }"#,
    );
    assert!(schema
        .validate(&v(r#"{"sku":"ABC-1234","tags":["a","bc"]}"#))
        .is_ok());

    let errs = schema
        .validate(&v(r#"{"sku":"ABC-123","tags":["ok","Not ok","fine"]}"#))
        .unwrap_err();
    assert_eq!(errs.len(), 2, "{errs:?}");
    assert_eq!(errs[0].path, "/sku");
    assert_eq!(
        errs[0].message,
        r#"does not match pattern "^[A-Z]{3}-\d{4}$""#
    );
    assert_eq!(errs[1].path, "/tags/1");
    assert_eq!(
        errs[1].to_string(),
        r#"/tags/1: does not match pattern "^[a-z]+$""#
    );

    // The one-shot free function agrees.
    let errs = schema::validate(
        &v(r#"{"properties":{"sku":{"pattern":"^\\d+$"}}}"#),
        &v(r#"{"sku":"12a"}"#),
    )
    .unwrap_err();
    assert_eq!(errs[0].path, "/sku");
}

#[test]
fn pattern_is_an_unanchored_search() {
    let schema = compile(r#"{ "pattern": "es" }"#);
    assert!(schema.validate(&s("test")).is_ok());
    assert!(schema.validate(&s("es")).is_ok());
    assert!(schema.validate(&s("tset")).is_err());
    // An empty pattern matches every string.
    assert!(compile(r#"{ "pattern": "" }"#).validate(&s("")).is_ok());
}

#[test]
fn non_strings_are_ignored() {
    let schema = compile(r#"{ "pattern": "^x$" }"#);
    for data in ["5", "1.5", "true", "null", "[\"y\"]", "{\"a\":\"y\"}"] {
        assert!(schema.validate(&v(data)).is_ok(), "{data}");
    }
    assert!(schema.validate(&s("y")).is_err());
}

#[test]
fn unicode_scalar_values() {
    let schema = compile(r#"{ "pattern": "^[α-ω]+ \\u{1F600}.$" }"#);
    assert!(schema.validate(&s("λογος 😀é")).is_ok());
    assert!(schema.validate(&s("logos 😀é")).is_err());
    // `.` is one scalar value, even outside the BMP.
    let one = compile(r#"{ "pattern": "^.$" }"#);
    assert!(one.validate(&s("😀")).is_ok());
    assert!(one.validate(&s("ab")).is_err());
    // Pattern and minLength agree on what a character is.
    let both = compile(r#"{ "pattern": "^.{2}$", "maxLength": 2 }"#);
    assert!(both.validate(&s("日本")).is_ok());
}

#[test]
fn unsupported_syntax_is_a_compile_error_with_the_keyword_path() {
    for (pattern, needle) in [
        (r"(a)\\1", "backreferences"),
        ("(?=a)", "lookahead"),
        ("(?<!a)b", "lookbehind"),
        ("(?i)a", "inline flags"),
        (r"\\p{Lu}", "property escapes"),
        (r"\\q", "unknown escape"),
        ("a{2", "malformed"),
        ("[a", "unclosed"),
        ("(a", "unclosed"),
        ("+", "nothing to repeat"),
    ] {
        let err = compile_err(&format!(r#"{{ "pattern": "{pattern}" }}"#));
        assert_eq!(err.path, "/pattern", "{pattern}");
        assert!(
            err.message.starts_with("invalid 'pattern'") && err.message.contains(needle),
            "{pattern}: {err}"
        );
    }

    // Nested keywords report a JSON Pointer to the keyword (RFC 6901 escapes).
    let err = compile_err(r#"{ "properties": { "a/b~c": { "pattern": "(?!x)" } } }"#);
    assert_eq!(err.path, "/properties/a~1b~0c/pattern");
    let err = compile_err(r#"{ "items": { "items": { "pattern": "\\k<n>" } } }"#);
    assert_eq!(err.path, "/items/items/pattern");
    assert!(err
        .to_string()
        .starts_with("schema error at /items/items/pattern: "));

    // A non-string pattern and a malformed `type` are rejected too.
    let err = compile_err(r#"{ "pattern": 5 }"#);
    assert_eq!(
        (err.path.as_str(), err.message.as_str()),
        ("/pattern", "'pattern' must be a string")
    );
    let err = compile_err(r#"{ "properties": { "x": { "type": "strnig" } } }"#);
    assert_eq!(err.path, "/properties/x/type");
    assert_eq!(err.message, "unknown type 'strnig'");

    // The first problem in document order is the one reported.
    let err =
        compile_err(r#"{ "properties": { "a": { "pattern": "(" }, "b": { "pattern": ")" } } }"#);
    assert_eq!(err.path, "/properties/a/pattern");

    // `SchemaError` is a std error.
    let _: &dyn std::error::Error = &err;
}

#[test]
fn the_free_function_fails_closed_on_a_bad_pattern() {
    let schema = v(r#"{ "properties": { "x": { "pattern": "(?=a)" } } }"#);
    for data in [r#"{"x":"a"}"#, r#"{"x":5}"#] {
        let errs = schema::validate(&schema, &v(data)).unwrap_err();
        assert_eq!(errs.len(), 1, "{data}");
        assert_eq!(errs[0].path, "/x");
        assert!(errs[0]
            .message
            .starts_with("schema error: invalid 'pattern'"));
    }
    // Where the sub-schema is not applied, nothing is reported (as for `type`).
    assert!(schema::validate(&schema, &v("{}")).is_ok());
}

#[test]
fn size_caps_reject_oversized_patterns() {
    // Counted repetition is sized before it is expanded.
    let started = Instant::now();
    for pattern in [
        "(?:a{1000}){1000}",
        "(?:(?:a{100}){100}){100}",
        "a{4294967295}",
    ] {
        let err = compile_err(&format!(r#"{{ "pattern": "{pattern}" }}"#));
        assert!(err.message.contains("instructions"), "{pattern}: {err}");
    }
    assert!(started.elapsed() < Duration::from_secs(2));

    // Pattern length (the message quotes only an excerpt).
    let long = "a".repeat(5000);
    let err = compile_err(&format!(r#"{{ "pattern": "{long}" }}"#));
    assert!(err.message.contains("4096-byte limit"), "{err}");
    assert!(err.message.len() < 300);

    // Nesting depth.
    let deep = format!("{}a{}", "(".repeat(65), ")".repeat(65));
    let err = compile_err(&format!(r#"{{ "pattern": "{deep}" }}"#));
    assert!(err.message.contains("nested deeper"), "{err}");

    // Large-but-legal patterns still compile.
    compile(r#"{ "pattern": "^[a-z0-9]{1,64}@[a-z0-9.-]{1,255}$" }"#);
}

#[test]
fn pathological_patterns_finish_in_linear_time() {
    // Classic catastrophic-backtracking shapes against a long near-miss input.
    let input = Value::Str(format!("{}!", "a".repeat(200_000)));
    let started = Instant::now();
    for pattern in [
        "(a+)+$",
        "^(a|aa)+$",
        "^(a*)*b",
        "(?:a?){20}a{20}$",
        "^(\\\\w+\\\\s?)*$",
    ] {
        let schema = compile(&format!(r#"{{ "pattern": "{pattern}" }}"#));
        assert!(schema.validate(&input).is_err(), "{pattern}");
    }
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "took {:?}",
        started.elapsed()
    );
}

#[test]
fn a_compiled_schema_is_shareable_and_reusable() {
    fn assert_send_sync<T: Send + Sync + Clone>() {}
    assert_send_sync::<Schema>();

    let schema = std::sync::Arc::new(compile(r#"{ "items": { "pattern": "^\\d+$" } }"#));
    let handles: Vec<_> = (0..4)
        .map(|i| {
            let schema = std::sync::Arc::clone(&schema);
            std::thread::spawn(move || {
                let data = Value::Array(vec![s(&i.to_string()), s("x")]);
                schema.validate(&data).unwrap_err().len()
            })
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().unwrap(), 1);
    }
}

#[test]
fn compiled_and_one_shot_validation_agree_on_existing_keywords() {
    let schema_src = v(r#"{
        "type": "object",
        "required": ["name", "age", "tags"],
        "properties": {
            "name": { "type": "string", "minLength": 1, "pattern": "^[A-Z]" },
            "age":  { "type": "integer", "minimum": 0, "maximum": 150 },
            "role": { "enum": ["admin", "user"] },
            "tags": { "type": "array", "items": { "type": "string" }, "minItems": 1 },
            "k":    { "const": {"a": [1, 2.0]} }
        }
    }"#);
    let compiled = Schema::compile(&schema_src).unwrap();
    for data in [
        r#"{"name":"Ada","age":36,"tags":["x"]}"#,
        r#"{"name":"","age":200,"role":"root","tags":[]}"#,
        r#"{"name":"ada","age":1.5,"tags":[1],"k":{"a":[1,2]}}"#,
        r#"{"name":5,"k":{"a":[2,1]}}"#,
        "[]",
    ] {
        let data = v(data);
        assert_eq!(
            compiled.validate(&data),
            schema::validate(&schema_src, &data),
            "{data:?}"
        );
    }
}

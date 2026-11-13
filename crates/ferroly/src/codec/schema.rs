//! A pragmatic JSON-Schema **subset** validator over the [`Value`] model.
//!
//! Validates an already-decoded [`Value`] against a schema (itself a [`Value`],
//! typically parsed from a JSON-Schema document).
//!
//! Supported keywords: `type` (a name or an array of names), `enum`, `const`,
//! `required`, `properties`, `items`, `minimum`, `maximum`, `minLength`,
//! `maxLength`, `minItems`, `maxItems`. Composition (`allOf`/`anyOf`/`$ref`/
//! `if`), and `pattern` (which would need a regex engine ferroly deliberately
//! does not carry) are **not** modeled — use full JSON-Schema tooling if you
//! need them. Keywords outside the supported set are ignored.
//!
//! `const` and `enum` compare by JSON value: `5`, `5u64` and `5.0` are equal,
//! and objects are equal regardless of member order. `"integer"` accepts any
//! whole number, including a float such as `1.0`. A `type` keyword the
//! validator cannot interpret (not a name or array of names, or an unknown
//! type name) is reported as a violation rather than silently passing.
//!
//! ```
//! use ferroly::codec::{json, schema};
//!
//! let schema = json::from_str(r#"{
//!   "type": "object",
//!   "required": ["name", "age"],
//!   "properties": {
//!     "name": { "type": "string", "minLength": 1 },
//!     "age":  { "type": "integer", "minimum": 0 }
//!   }
//! }"#).unwrap();
//!
//! let ok = json::from_str(r#"{ "name": "Ada", "age": 36 }"#).unwrap();
//! assert!(schema::validate(&schema, &ok).is_ok());
//!
//! let bad = json::from_str(r#"{ "age": -1 }"#).unwrap();
//! let errs = schema::validate(&schema, &bad).unwrap_err();
//! assert_eq!(errs.len(), 2); // missing "name" + age below minimum
//! ```

use ferroly::codec::{find, Value};

/// A single validation failure: the JSON-pointer-ish `path` and a `message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Location of the failure (e.g. `/properties/age`).
    pub path: String,
    /// Human-readable description.
    pub message: String,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {}",
            if self.path.is_empty() {
                "/"
            } else {
                &self.path
            },
            self.message
        )
    }
}

/// Validates `data` against `schema`, collecting **all** violations.
pub fn validate(schema: &Value, data: &Value) -> Result<(), Vec<Violation>> {
    let mut out = Vec::new();
    check(schema, data, "", &mut out);
    if out.is_empty() {
        Ok(())
    } else {
        Err(out)
    }
}

fn field<'a>(schema: &'a Value, key: &str) -> Option<&'a Value> {
    schema.get(key)
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::UInt(u) => Some(*u as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

/// Whether `data` has the JSON-Schema type `ty`; `None` for a type name the
/// validator does not know.
fn type_matches(ty: &str, data: &Value) -> Option<bool> {
    Some(match ty {
        "string" => matches!(data, Value::Str(_)),
        // JSON Schema: any number with a zero fractional part is an integer,
        // so `1.0` qualifies.
        "integer" => match data {
            Value::Int(_) | Value::UInt(_) => true,
            Value::Float(f) => f.is_finite() && f.fract() == 0.0,
            _ => false,
        },
        "number" => matches!(data, Value::Int(_) | Value::UInt(_) | Value::Float(_)),
        "boolean" => matches!(data, Value::Bool(_)),
        "object" => matches!(data, Value::Object(_)),
        "array" => matches!(data, Value::Array(_)),
        "null" => matches!(data, Value::Null),
        _ => return None,
    })
}

/// Checks the `type` keyword, which is either one type name or an array of
/// them (any may match). Returns the violation message, if any. A malformed
/// keyword or an unknown type name fails closed rather than being ignored.
fn check_type(keyword: &Value, data: &Value) -> Option<String> {
    const MALFORMED: &str =
        "schema error: 'type' must be a type name or a non-empty array of type names";
    let names: Vec<&str> = match keyword {
        Value::Str(name) => vec![name.as_str()],
        Value::Array(items) => {
            let names: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
            if names.is_empty() || names.len() != items.len() {
                return Some(MALFORMED.to_string());
            }
            names
        }
        _ => return Some(MALFORMED.to_string()),
    };
    let mut matched = false;
    for name in &names {
        match type_matches(name, data) {
            Some(true) => matched = true,
            Some(false) => {}
            None => return Some(format!("schema error: unknown type '{name}'")),
        }
    }
    if matched {
        None
    } else if names.len() == 1 {
        Some(format!("expected type '{}'", names[0]))
    } else {
        Some(format!(
            "expected one of the types '{}'",
            names.join("', '")
        ))
    }
}

/// A whole number held by any numeric variant, as an `i128` (which covers both
/// `i64` and `u64` exactly). `None` for non-numbers and fractional floats.
fn whole(v: &Value) -> Option<i128> {
    match v {
        Value::Int(i) => Some(i128::from(*i)),
        Value::UInt(u) => Some(i128::from(*u)),
        // Beyond 2^63 a float can no longer be told apart from its neighbours;
        // the cast saturates, which is fine for an equality test.
        Value::Float(f) if f.is_finite() && f.fract() == 0.0 => Some(*f as i128),
        _ => None,
    }
}

/// JSON-Schema equality, as used by `const` and `enum`: numbers compare by
/// value regardless of how they are stored (`5`, `5u`, `5.0` are equal),
/// objects compare without regard to member order, arrays compare in order.
fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => x == y,
        (
            Value::Int(_) | Value::UInt(_) | Value::Float(_),
            Value::Int(_) | Value::UInt(_) | Value::Float(_),
        ) => match (whole(a), whole(b)) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        },
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| json_eq(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, p)| find(y, k).is_some_and(|q| json_eq(p, q)))
                && y.iter().all(|(k, _)| find(x, k).is_some())
        }
        _ => a == b,
    }
}

fn check(schema: &Value, data: &Value, path: &str, out: &mut Vec<Violation>) {
    let push = |out: &mut Vec<Violation>, msg: String| {
        out.push(Violation {
            path: path.to_string(),
            message: msg,
        });
    };

    // type
    if let Some(keyword) = field(schema, "type") {
        if let Some(message) = check_type(keyword, data) {
            push(out, message);
            return; // further keyword checks would be noise
        }
    }

    // const / enum
    if let Some(c) = field(schema, "const") {
        if !json_eq(data, c) {
            push(out, "value does not equal const".to_string());
        }
    }
    if let Some(Value::Array(choices)) = field(schema, "enum") {
        if !choices.iter().any(|c| json_eq(c, data)) {
            push(out, "value not in enum".to_string());
        }
    }

    // numbers
    if let Some(n) = num(data) {
        if let Some(min) = field(schema, "minimum").and_then(num) {
            if n < min {
                push(out, format!("must be >= {min}"));
            }
        }
        if let Some(max) = field(schema, "maximum").and_then(num) {
            if n > max {
                push(out, format!("must be <= {max}"));
            }
        }
    }

    // strings
    if let Value::Str(s) = data {
        let len = s.chars().count() as u64;
        if let Some(min) = field(schema, "minLength").and_then(num) {
            if (len as f64) < min {
                push(out, format!("length must be >= {min}"));
            }
        }
        if let Some(max) = field(schema, "maxLength").and_then(num) {
            if (len as f64) > max {
                push(out, format!("length must be <= {max}"));
            }
        }
    }

    // arrays
    if let Value::Array(items) = data {
        if let Some(min) = field(schema, "minItems").and_then(num) {
            if (items.len() as f64) < min {
                push(out, format!("must have >= {min} items"));
            }
        }
        if let Some(max) = field(schema, "maxItems").and_then(num) {
            if (items.len() as f64) > max {
                push(out, format!("must have <= {max} items"));
            }
        }
        if let Some(item_schema) = field(schema, "items") {
            for (i, elem) in items.iter().enumerate() {
                check(item_schema, elem, &format!("{path}/{i}"), out);
            }
        }
    }

    // objects
    if let Value::Object(_) = data {
        if let Some(Value::Array(req)) = field(schema, "required") {
            for key in req {
                if let Some(k) = key.as_str() {
                    if data.get(k).is_none() {
                        push(out, format!("missing required property '{k}'"));
                    }
                }
            }
        }
        if let Some(Value::Object(props)) = field(schema, "properties") {
            for (key, sub) in props {
                if let Some(value) = data.get(key) {
                    check(sub, value, &format!("{path}/{key}"), out);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroly::codec::json;

    fn v(s: &str) -> Value {
        json::from_str(s).unwrap()
    }

    #[test]
    fn passes_valid_and_reports_all_failures() {
        let schema = v(r#"{
            "type": "object",
            "required": ["name", "age", "tags"],
            "properties": {
                "name": { "type": "string", "minLength": 1 },
                "age":  { "type": "integer", "minimum": 0, "maximum": 150 },
                "role": { "enum": ["admin", "user"] },
                "tags": { "type": "array", "items": { "type": "string" }, "minItems": 1 }
            }
        }"#);

        assert!(validate(&schema, &v(r#"{"name":"Ada","age":36,"tags":["x"]}"#)).is_ok());

        let errs = validate(
            &schema,
            &v(r#"{"name":"","age":200,"role":"root","tags":[]}"#),
        )
        .unwrap_err();
        // missing "tags"? no — present but empty (minItems). Failures:
        // name minLength, age maximum, role enum, tags minItems.
        assert_eq!(errs.len(), 4, "{errs:?}");
        assert!(errs.iter().any(|e| e.path == "/name"));
        assert!(errs.iter().any(|e| e.path == "/age"));
        assert!(errs.iter().any(|e| e.path == "/role"));
        assert!(errs.iter().any(|e| e.path == "/tags"));
    }

    #[test]
    fn type_mismatch_and_nested_items() {
        let schema = v(r#"{ "type": "array", "items": { "type": "integer", "minimum": 1 } }"#);
        let errs = validate(&schema, &v(r#"[1, 0, 3]"#)).unwrap_err();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].path, "/1");

        let errs = validate(&schema, &v(r#"{"not":"an array"}"#)).unwrap_err();
        assert_eq!(errs[0].message, "expected type 'array'");
    }

    #[test]
    fn type_may_be_an_array_of_names() {
        let schema = v(r#"{ "type": ["string", "null"] }"#);
        assert!(validate(&schema, &v(r#""x""#)).is_ok());
        assert!(validate(&schema, &v("null")).is_ok());
        let errs = validate(&schema, &v("5")).unwrap_err();
        assert_eq!(errs.len(), 1);
        assert_eq!(
            errs[0].message,
            "expected one of the types 'string', 'null'"
        );

        // Nested: the array form applies inside properties and items too.
        let schema = v(r#"{
            "type": "object",
            "properties": { "port": { "type": ["integer", "string"] } }
        }"#);
        assert!(validate(&schema, &v(r#"{"port":8080}"#)).is_ok());
        assert!(validate(&schema, &v(r#"{"port":"8080"}"#)).is_ok());
        assert!(validate(&schema, &v(r#"{"port":true}"#)).is_err());
        assert!(validate(&schema, &v(r#"{"port":[1]}"#)).is_err());
    }

    #[test]
    fn uninterpretable_type_keyword_fails_closed() {
        for schema in [
            r#"{ "type": [] }"#,
            r#"{ "type": ["string", 5] }"#,
            r#"{ "type": 5 }"#,
            r#"{ "type": {"a":1} }"#,
            r#"{ "type": "strnig" }"#,
            r#"{ "type": ["string", "nul"] }"#,
        ] {
            let errs = validate(&v(schema), &v(r#""x""#)).unwrap_err();
            assert_eq!(errs.len(), 1, "{schema}");
            assert!(
                errs[0].message.starts_with("schema error:"),
                "{schema}: {errs:?}"
            );
        }
    }

    #[test]
    fn integer_accepts_whole_floats_only() {
        let schema = v(r#"{ "type": "integer" }"#);
        assert!(validate(&schema, &Value::Int(-3)).is_ok());
        assert!(validate(&schema, &Value::UInt(u64::MAX)).is_ok());
        assert!(validate(&schema, &Value::Float(1.0)).is_ok());
        assert!(validate(&schema, &Value::Float(-0.0)).is_ok());
        assert!(validate(&schema, &Value::Float(1e20)).is_ok());
        assert!(validate(&schema, &Value::Float(1.5)).is_err());
        assert!(validate(&schema, &Value::Float(f64::NAN)).is_err());
        assert!(validate(&schema, &Value::Float(f64::INFINITY)).is_err());
        assert!(validate(&schema, &Value::Str("1".into())).is_err());
        // Bounds still apply to a float that passed as an integer.
        let bounded = v(r#"{ "type": "integer", "minimum": 2 }"#);
        assert!(validate(&bounded, &Value::Float(1.0)).is_err());
        assert!(validate(&bounded, &Value::Float(2.0)).is_ok());
    }

    #[test]
    fn enum_and_const_compare_numbers_by_value() {
        let schema = v(r#"{ "enum": [5, "five", 2.5] }"#);
        assert!(validate(&schema, &Value::Int(5)).is_ok());
        assert!(validate(&schema, &Value::UInt(5)).is_ok());
        assert!(validate(&schema, &Value::Float(5.0)).is_ok());
        assert!(validate(&schema, &Value::Float(2.5)).is_ok());
        assert!(validate(&schema, &Value::Str("five".into())).is_ok());
        assert!(validate(&schema, &Value::UInt(6)).is_err());
        assert!(validate(&schema, &Value::Float(5.5)).is_err());
        assert!(validate(&schema, &Value::Str("5".into())).is_err());
        assert!(validate(&schema, &Value::Bool(true)).is_err());

        let schema = v(r#"{ "const": 5.0 }"#);
        assert!(validate(&schema, &Value::Int(5)).is_ok());
        assert!(validate(&schema, &Value::UInt(5)).is_ok());
        assert!(validate(&schema, &Value::Int(-5)).is_err());

        // Values past i64 keep their identity.
        let schema = Value::Object(vec![("const".into(), Value::UInt(u64::MAX))]);
        assert!(validate(&schema, &Value::UInt(u64::MAX)).is_ok());
        assert!(validate(&schema, &Value::Int(-1)).is_err());
        assert!(validate(&schema, &Value::Int(i64::MAX)).is_err());

        // Numbers nested in arrays and objects compare the same way.
        let schema = v(r#"{ "const": {"a": [1, 2.0], "b": {"c": 3}} }"#);
        let data = Value::Object(vec![
            (
                "a".into(),
                Value::Array(vec![Value::UInt(1), Value::Int(2)]),
            ),
            (
                "b".into(),
                Value::Object(vec![("c".into(), Value::Float(3.0))]),
            ),
        ]);
        assert!(validate(&schema, &data).is_ok());
    }

    #[test]
    fn object_const_ignores_member_order() {
        let schema = v(r#"{ "const": {"a": 1, "b": {"x": true, "y": [1, 2]}} }"#);
        assert!(validate(&schema, &v(r#"{"b":{"y":[1,2],"x":true},"a":1}"#)).is_ok());
        // Array order still matters; so do missing, extra and changed members.
        assert!(validate(&schema, &v(r#"{"b":{"y":[2,1],"x":true},"a":1}"#)).is_err());
        assert!(validate(&schema, &v(r#"{"a":1}"#)).is_err());
        assert!(validate(&schema, &v(r#"{"a":1,"b":{"x":true,"y":[1,2]},"c":0}"#)).is_err());
        assert!(validate(&schema, &v(r#"{"a":2,"b":{"x":true,"y":[1,2]}}"#)).is_err());
        assert!(validate(&schema, &v(r#"{"a":1,"z":{"x":true,"y":[1,2]}}"#)).is_err());

        let schema = v(r#"{ "enum": [{"k": 1, "j": 2}] }"#);
        assert!(validate(&schema, &v(r#"{"j":2,"k":1}"#)).is_ok());
        assert!(validate(&schema, &v(r#"{"j":2,"k":3}"#)).is_err());
    }
}

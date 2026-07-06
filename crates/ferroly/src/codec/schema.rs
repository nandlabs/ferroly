//! A pragmatic JSON-Schema **subset** validator over the [`Value`] model.
//!
//! Validates an already-decoded [`Value`] against a schema (itself a [`Value`],
//! typically parsed from a JSON-Schema document).
//!
//! Supported keywords: `type`, `enum`, `const`, `required`, `properties`,
//! `items`, `minimum`, `maximum`, `minLength`, `maxLength`, `minItems`,
//! `maxItems`. Composition (`allOf`/`anyOf`/`$ref`/`if`), and `pattern` (which
//! would need a regex engine ferroly deliberately does not carry) are **not**
//! modeled — use full JSON-Schema tooling if you need them.
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

use ferroly::codec::Value;

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

fn type_matches(ty: &str, data: &Value) -> bool {
    match ty {
        "string" => matches!(data, Value::Str(_)),
        "integer" => matches!(data, Value::Int(_) | Value::UInt(_)),
        "number" => matches!(data, Value::Int(_) | Value::UInt(_) | Value::Float(_)),
        "boolean" => matches!(data, Value::Bool(_)),
        "object" => matches!(data, Value::Object(_)),
        "array" => matches!(data, Value::Array(_)),
        "null" => matches!(data, Value::Null),
        _ => true, // unknown type keyword: don't fail
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
    if let Some(ty) = field(schema, "type").and_then(Value::as_str) {
        if !type_matches(ty, data) {
            push(out, format!("expected type '{ty}'"));
            return; // further keyword checks would be noise
        }
    }

    // const / enum
    if let Some(c) = field(schema, "const") {
        if data != c {
            push(out, "value does not equal const".to_string());
        }
    }
    if let Some(Value::Array(choices)) = field(schema, "enum") {
        if !choices.iter().any(|c| c == data) {
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
}

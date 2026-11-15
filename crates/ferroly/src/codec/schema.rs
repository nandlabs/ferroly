//! A pragmatic JSON-Schema **subset** validator over the [`Value`] model.
//!
//! Validates an already-decoded [`Value`] against a schema (itself a [`Value`],
//! typically parsed from a JSON-Schema document).
//!
//! Supported keywords: `type` (a name or an array of names), `enum`, `const`,
//! `required`, `properties`, `items`, `minimum`, `maximum`, `minLength`,
//! `maxLength`, `pattern`, `minItems`, `maxItems`. Composition (`allOf`/
//! `anyOf`/`$ref`/`if`) is **not** modeled — use full JSON-Schema tooling if
//! you need it. Keywords outside the supported set are ignored.
//!
//! `const` and `enum` compare by JSON value: `5`, `5u64` and `5.0` are equal,
//! and objects are equal regardless of member order. `"integer"` accepts any
//! whole number, including a float such as `1.0`. A `type` keyword the
//! validator cannot interpret (not a name or array of names, or an unknown
//! type name) is reported as a violation rather than silently passing.
//!
//! # Compile once, validate many
//!
//! [`Schema::compile`] checks a schema up front — every `pattern` is compiled
//! to a matcher and every `type` keyword is interpreted — and reports the first
//! problem as a [`SchemaError`] pointing at the keyword. The compiled
//! [`Schema`] is then reused for any number of [`Schema::validate`] calls. The
//! free [`validate`] function is the one-shot form: it compiles on every call
//! and reports an uninterpretable keyword as a violation whose message starts
//! with `schema error:` instead of failing up front.
//!
//! # `pattern`
//!
//! `pattern` holds an ECMA-262 regular expression; a string matches if the
//! expression matches **anywhere** in it (anchor with `^…$` for a full match).
//! Non-string values are not checked. The matcher is built in: it accepts the
//! interoperable ECMA-262 subset JSON Schema recommends — literals and escapes,
//! `.`, `^`, `$`, `\b`, `\B`, `\d \D \w \W \s \S`, bracket classes with
//! ranges, groups (`(…)`, `(?:…)`, `(?<name>…)`), alternation, and the greedy
//! and lazy quantifiers `* + ? {n} {n,} {n,m}` — with the strict escape rules of
//! the `u` flag, matching on Unicode scalar values. Syntax outside that subset
//! (backreferences, lookaround, inline flags, `\p{…}`, unknown escapes, an
//! unescaped `{`, `}` or `]`) is a schema error, never a silent literal.
//!
//! Matching never backtracks: it simulates the pattern's automaton over the
//! input once, so it runs in time linear in the string length for any pattern
//! (`(a+)+$` included). A pattern may be at most 4096 bytes, nest groups at
//! most 64 deep, and compile to at most 10 000 instructions; counted
//! repetition is sized before it is expanded, so `(?:a{1000}){1000}` is
//! rejected rather than built.
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
//!
//! Compiling once, with a `pattern`:
//!
//! ```
//! use ferroly::codec::{json, schema::Schema};
//!
//! let schema = Schema::compile(&json::from_str(r#"{
//!   "type": "object",
//!   "properties": { "sku": { "type": "string", "pattern": "^[A-Z]{3}-\\d{4}$" } }
//! }"#).unwrap()).unwrap();
//!
//! assert!(schema.validate(&json::from_str(r#"{ "sku": "ABC-1234" }"#).unwrap()).is_ok());
//! let errs = schema.validate(&json::from_str(r#"{ "sku": "abc-1234" }"#).unwrap()).unwrap_err();
//! assert_eq!(errs[0].path, "/sku");
//!
//! // Unsupported syntax is rejected when the schema is compiled.
//! let err = Schema::compile(&json::from_str(r#"{ "pattern": "(?=x)" }"#).unwrap()).unwrap_err();
//! assert_eq!(err.path, "/pattern");
//! assert!(err.message.contains("lookahead"));
//! ```

use std::fmt::Write as _;

use super::regex::Regex;
use ferroly::codec::{find, Value};

mod generate;

/// Derives [`JsonSchema`] from the same `#[ferroly(...)]` attributes as
/// `Encode` / `Decode`; see the trait for an example.
pub use ferroly_derive::JsonSchema;
#[doc(hidden)]
pub use generate::__private;
pub use generate::JsonSchema;

/// How many violations a validation collects by default before it stops
/// (see [`Schema::with_max_violations`]).
pub const DEFAULT_MAX_VIOLATIONS: usize = 100;

/// The message prefix of the marker [`Violation`] that ends a truncated list.
const TRUNCATED_PREFIX: &str = "too many violations: stopped after ";

/// A single validation failure: the JSON-pointer-ish `path` and a `message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Location of the failure in the validated data, as a JSON Pointer
    /// (RFC 6901), e.g. `/items/3/name`. Empty for the value itself.
    pub path: String,
    /// Human-readable description.
    pub message: String,
}

impl Violation {
    /// The marker that ends a list cut short after `limit` violations.
    fn truncated(limit: usize) -> Violation {
        Violation {
            path: String::new(),
            message: format!("{TRUNCATED_PREFIX}{limit}"),
        }
    }

    /// Whether this is the marker a validation appends when it stopped at
    /// its violation cap ([`Schema::with_max_violations`]) with more
    /// violations left unreported. It is always the last element.
    ///
    /// ```
    /// use ferroly::codec::{json, schema::Schema};
    ///
    /// let schema = Schema::compile(&json::from_str(r#"{"items":{"type":"string"}}"#).unwrap())
    ///     .unwrap()
    ///     .with_max_violations(2);
    /// let errs = schema.validate(&json::from_str("[1, 2, 3, 4]").unwrap()).unwrap_err();
    /// assert_eq!(errs.len(), 3); // two violations, then the marker
    /// assert!(errs[2].is_truncation());
    /// assert!(!errs[0].is_truncation());
    /// ```
    pub fn is_truncation(&self) -> bool {
        self.path.is_empty() && self.message.starts_with(TRUNCATED_PREFIX)
    }
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

/// Why [`Schema::compile`] rejected a schema: a keyword the validator cannot
/// interpret, such as an unsupported or malformed `pattern`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SchemaError {
    /// JSON Pointer (RFC 6901) to the offending keyword within the schema,
    /// e.g. `/properties/name/pattern`.
    pub path: String,
    /// Human-readable description.
    pub message: String,
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "schema error at {}: {}", self.path, self.message)
    }
}

impl std::error::Error for SchemaError {}

/// A schema compiled for repeated validation: each `pattern` is compiled once
/// here rather than on every [`validate`](Schema::validate) call.
///
/// Cheap to share: it is `Send + Sync`, so one compiled schema can serve
/// every request handler (behind an `Arc` or a `static`).
#[derive(Debug, Clone)]
pub struct Schema {
    root: Node,
    max_violations: usize,
}

impl Schema {
    /// Compiles `schema`, failing on the first keyword the validator cannot
    /// interpret: a `pattern` that is not a string, uses unsupported syntax, or
    /// exceeds a size cap, or a malformed `type`.
    pub fn compile(schema: &Value) -> Result<Schema, SchemaError> {
        let mut first = None;
        let root = Node::build(schema, "", &mut first);
        match first {
            Some(err) => Err(err),
            None => Ok(Schema {
                root,
                max_violations: DEFAULT_MAX_VIOLATIONS,
            }),
        }
    }

    /// Compiles the schema a type describes through [`JsonSchema`] (usually
    /// `#[derive(JsonSchema)]`). A generated schema always compiles; the
    /// error is for a hand-written [`JsonSchema`] impl with, say, a bad
    /// `pattern`.
    pub fn of<T: JsonSchema + ?Sized>() -> Result<Schema, SchemaError> {
        Schema::compile(&T::json_schema())
    }

    /// Returns the schema with a different violation cap (default
    /// [`DEFAULT_MAX_VIOLATIONS`], at least 1). Validation stops as soon as it
    /// finds one violation more than the cap, and reports the first `max`
    /// followed by a marker ([`Violation::is_truncation`]); so the work and
    /// memory a hostile input can cost are bounded. `1` reports only the
    /// first violation.
    #[must_use]
    pub fn with_max_violations(mut self, max: usize) -> Schema {
        self.max_violations = max.max(1);
        self
    }

    /// The violation cap (see [`with_max_violations`](Self::with_max_violations)).
    pub fn max_violations(&self) -> usize {
        self.max_violations
    }

    /// Validates `data`, collecting every violation up to the cap
    /// ([`with_max_violations`](Self::with_max_violations)); a list that was
    /// cut short ends with a [truncation marker](Violation::is_truncation).
    pub fn validate(&self, data: &Value) -> Result<(), Vec<Violation>> {
        run(&self.root, data, self.max_violations)
    }
}

/// Validates `data` against `schema`, collecting every violation up to
/// [`DEFAULT_MAX_VIOLATIONS`] (then a [truncation
/// marker](Violation::is_truncation)).
///
/// Compiles the schema on every call; use [`Schema::compile`] to compile it
/// once and to reject an uninterpretable schema up front. Here a keyword the
/// validator cannot interpret is reported as a violation (message prefix
/// `schema error:`) wherever its sub-schema is applied.
pub fn validate(schema: &Value, data: &Value) -> Result<(), Vec<Violation>> {
    run(
        &Node::build(schema, "", &mut None),
        data,
        DEFAULT_MAX_VIOLATIONS,
    )
}

fn run(root: &Node, data: &Value, max: usize) -> Result<(), Vec<Violation>> {
    let mut out = Collector {
        found: Vec::new(),
        max,
        full: false,
    };
    check(root, data, "", &mut out);
    if out.full {
        out.found.push(Violation::truncated(max));
    }
    if out.found.is_empty() {
        Ok(())
    } else {
        Err(out.found)
    }
}

/// The violations found so far, with the cap that stops the walk.
struct Collector {
    found: Vec<Violation>,
    max: usize,
    /// A violation beyond the cap was found: stop walking.
    full: bool,
}

impl Collector {
    fn push(&mut self, path: &str, message: String) {
        if self.found.len() < self.max {
            self.found.push(Violation {
                path: path.to_string(),
                message,
            });
        } else {
            self.full = true;
        }
    }
}

/// A compiled `pattern`: its matcher, and the message a mismatch reports
/// (built once and quoting at most 64 characters of the source, so a
/// violation never copies a long pattern).
#[derive(Debug, Clone)]
struct Pattern {
    mismatch: std::sync::Arc<str>,
    regex: Regex,
}

/// One schema object with its keywords extracted (and `pattern` compiled).
/// An `Err` keyword holds the message for a keyword that cannot be applied.
#[derive(Debug, Clone, Default)]
struct Node {
    ty: Option<Result<Vec<String>, String>>,
    konst: Option<Value>,
    choices: Option<Vec<Value>>,
    minimum: Option<f64>,
    maximum: Option<f64>,
    min_length: Option<f64>,
    max_length: Option<f64>,
    pattern: Option<Result<Pattern, String>>,
    min_items: Option<f64>,
    max_items: Option<f64>,
    required: Vec<String>,
    properties: Vec<(String, Node)>,
    items: Option<Box<Node>>,
}

/// Escapes a key as a JSON Pointer reference token (RFC 6901).
fn pointer_token(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// At most 64 characters of `s`, for quoting a rejected keyword in a message.
fn excerpt(s: &str) -> String {
    match s.char_indices().nth(64) {
        Some((cut, _)) => format!("{}…", &s[..cut]),
        None => s.to_string(),
    }
}

impl Node {
    /// Extracts `schema`'s keywords. `ptr` is the schema's own JSON Pointer;
    /// the first uninterpretable keyword is recorded in `first`.
    fn build(schema: &Value, ptr: &str, first: &mut Option<SchemaError>) -> Node {
        let field = |key: &str| schema.get(key);
        let ty = field("type").map(type_names);
        let pattern = field("pattern").map(|keyword| match keyword {
            Value::Str(source) => Regex::new(source)
                .map(|regex| Pattern {
                    mismatch: format!("does not match pattern \"{}\"", excerpt(source)).into(),
                    regex,
                })
                .map_err(|e| format!("invalid 'pattern' \"{}\": {e}", excerpt(source))),
            _ => Err("'pattern' must be a string".to_string()),
        });
        for (keyword, message) in [
            ("type", ty.as_ref().and_then(|t| t.as_ref().err())),
            ("pattern", pattern.as_ref().and_then(|p| p.as_ref().err())),
        ] {
            if let (None, Some(message)) = (&first, message) {
                *first = Some(SchemaError {
                    path: format!("{ptr}/{keyword}"),
                    message: message.clone(),
                });
            }
        }
        let required = match field("required") {
            Some(Value::Array(req)) => req
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        };
        let properties = match field("properties") {
            Some(Value::Object(props)) => props
                .iter()
                .map(|(key, sub)| {
                    let sub_ptr = format!("{ptr}/properties/{}", pointer_token(key));
                    (key.clone(), Node::build(sub, &sub_ptr, first))
                })
                .collect(),
            _ => Vec::new(),
        };
        let items =
            field("items").map(|sub| Box::new(Node::build(sub, &format!("{ptr}/items"), first)));
        Node {
            ty,
            konst: field("const").cloned(),
            choices: match field("enum") {
                Some(Value::Array(choices)) => Some(choices.clone()),
                _ => None,
            },
            minimum: field("minimum").and_then(num),
            maximum: field("maximum").and_then(num),
            min_length: field("minLength").and_then(num),
            max_length: field("maxLength").and_then(num),
            pattern,
            min_items: field("minItems").and_then(num),
            max_items: field("maxItems").and_then(num),
            required,
            properties,
            items,
        }
    }
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

/// Interprets the `type` keyword, which is either one type name or an array of
/// them (any may match). A malformed keyword or an unknown type name is an
/// error rather than being ignored.
fn type_names(keyword: &Value) -> Result<Vec<String>, String> {
    const MALFORMED: &str = "'type' must be a type name or a non-empty array of type names";
    let names: Vec<String> = match keyword {
        Value::Str(name) => vec![name.clone()],
        Value::Array(items) => {
            let names: Vec<String> = items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            if names.is_empty() || names.len() != items.len() {
                return Err(MALFORMED.to_string());
            }
            names
        }
        _ => return Err(MALFORMED.to_string()),
    };
    match names
        .iter()
        .find(|name| type_matches(name, &Value::Null).is_none())
    {
        Some(name) => Err(format!("unknown type '{}'", excerpt(name))),
        None => Ok(names),
    }
}

/// Checks the interpreted `type` keyword. Returns the violation message, if
/// any.
fn check_type(names: &[String], data: &Value) -> Option<String> {
    if names
        .iter()
        .any(|name| type_matches(name, data) == Some(true))
    {
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

fn check(node: &Node, data: &Value, path: &str, out: &mut Collector) {
    if out.full {
        return;
    }
    let push = |out: &mut Collector, msg: String| out.push(path, msg);

    // type
    match &node.ty {
        Some(Err(message)) => {
            push(out, format!("schema error: {message}"));
            return;
        }
        Some(Ok(names)) => {
            if let Some(message) = check_type(names, data) {
                push(out, message);
                return; // further keyword checks would be noise
            }
        }
        None => {}
    }

    // An uninterpretable pattern fails closed for every value it is applied
    // to, like an uninterpretable `type`.
    if let Some(Err(message)) = &node.pattern {
        push(out, format!("schema error: {message}"));
    }

    // const / enum
    if let Some(c) = &node.konst {
        if !json_eq(data, c) {
            push(out, "value does not equal const".to_string());
        }
    }
    if let Some(choices) = &node.choices {
        if !choices.iter().any(|c| json_eq(c, data)) {
            push(out, "value not in enum".to_string());
        }
    }

    // numbers
    if let Some(n) = num(data) {
        if let Some(min) = node.minimum {
            if n < min {
                push(out, format!("must be >= {min}"));
            }
        }
        if let Some(max) = node.maximum {
            if n > max {
                push(out, format!("must be <= {max}"));
            }
        }
    }

    // strings
    if let Value::Str(s) = data {
        let len = s.chars().count() as u64;
        if let Some(min) = node.min_length {
            if (len as f64) < min {
                push(out, format!("length must be >= {min}"));
            }
        }
        if let Some(max) = node.max_length {
            if (len as f64) > max {
                push(out, format!("length must be <= {max}"));
            }
        }
        if let Some(Ok(pattern)) = &node.pattern {
            if !pattern.regex.is_match(s) {
                push(out, pattern.mismatch.to_string());
            }
        }
    }

    // arrays
    if let Value::Array(items) = data {
        if let Some(min) = node.min_items {
            if (items.len() as f64) < min {
                push(out, format!("must have >= {min} items"));
            }
        }
        if let Some(max) = node.max_items {
            if (items.len() as f64) > max {
                push(out, format!("must have <= {max} items"));
            }
        }
        if let Some(item_node) = &node.items {
            let mut item_path = String::with_capacity(path.len() + 8);
            for (i, elem) in items.iter().enumerate() {
                if out.full {
                    return;
                }
                item_path.clear();
                let _ = write!(item_path, "{path}/{i}");
                check(item_node, elem, &item_path, out);
            }
        }
    }

    // objects
    if let Value::Object(_) = data {
        for k in &node.required {
            if data.get(k).is_none() {
                push(out, format!("missing required property '{}'", excerpt(k)));
            }
        }
        for (key, sub) in &node.properties {
            if out.full {
                return;
            }
            if let Some(value) = data.get(key) {
                check(sub, value, &format!("{path}/{}", pointer_token(key)), out);
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
    fn violations_stop_at_the_cap_with_a_marker() {
        // MISC-TV-1: an array of a million failing items used to produce a
        // million violations.
        let schema = v(r#"{ "items": { "type": "string" } }"#);
        let data = Value::Array(vec![Value::Int(1); 1_000_000]);
        let errs = validate(&schema, &data).unwrap_err();
        assert_eq!(errs.len(), DEFAULT_MAX_VIOLATIONS + 1);
        assert!(errs.last().unwrap().is_truncation());
        assert!(errs[..DEFAULT_MAX_VIOLATIONS]
            .iter()
            .all(|e| !e.is_truncation() && e.message == "expected type 'string'"));
        assert_eq!(errs[99].path, "/99");

        let compiled = Schema::compile(&schema).unwrap().with_max_violations(3);
        assert_eq!(compiled.max_violations(), 3);
        let errs = compiled.validate(&data).unwrap_err();
        assert_eq!(errs.len(), 4);
        assert!(errs[3].is_truncation());
        assert_eq!(
            errs[3].to_string(),
            "/: too many violations: stopped after 3"
        );

        // Exactly at the cap: no marker, since nothing was left out.
        let three = Value::Array(vec![Value::Int(1); 3]);
        let errs = compiled.validate(&three).unwrap_err();
        assert_eq!(errs.len(), 3);
        assert!(errs.iter().all(|e| !e.is_truncation()));

        // A cap of 0 is treated as 1 (first violation only).
        let first = Schema::compile(&schema).unwrap().with_max_violations(0);
        let errs = first.validate(&data).unwrap_err();
        assert_eq!((errs.len(), errs[0].path.as_str()), (2, "/0"));
    }

    #[test]
    fn pattern_violations_quote_a_bounded_excerpt() {
        // MISC-RX-1: every violation used to copy the whole pattern.
        let pattern = format!("^{}$", "a".repeat(4000));
        let schema = Value::Object(vec![("pattern".into(), Value::Str(pattern))]);
        let compiled = Schema::compile(&schema).unwrap();
        let errs = compiled
            .validate(&Value::Array(vec![Value::Str(String::new()); 10]))
            .map(|_| ())
            .err();
        assert!(errs.is_none(), "pattern does not apply to arrays");
        let errs = compiled.validate(&Value::Str("b".into())).unwrap_err();
        assert!(errs[0].message.len() < 120, "{}", errs[0].message);
        assert!(errs[0].message.starts_with("does not match pattern \"^aaa"));
    }

    #[test]
    fn property_paths_are_json_pointers() {
        let schema =
            v(r#"{ "properties": { "a/b": { "type": "string" }, "m~n": { "type": "string" } } }"#);
        let errs = validate(&schema, &v(r#"{ "a/b": 1, "m~n": 2 }"#)).unwrap_err();
        let paths: Vec<&str> = errs.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["/a~1b", "/m~0n"]);
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

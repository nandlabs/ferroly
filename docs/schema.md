# Schema validation

**Feature:** `codec` (default) · **Module:** `ferroly::codec::schema`

A pragmatic JSON-Schema **subset** validator that runs over the codec's
[`Value`](codec.md#the-value-model) model. Validate an already-decoded value
against a schema that is itself a `Value` — typically a JSON-Schema document you
parsed with [`json::from_str`](codec.md#json-codec).

## Overview

`schema` handles the common structural-validation case: after you decode an
input into a `Value`, check that it has the shape your service expects —
required fields present, numbers in range, strings the right length, arrays
bounded, nested items well-formed.

Two design choices shape the module:

- **It validates a `Value`, not a stream.** Decode first (with any of the
  [codecs](codec.md)), then validate. The schema is also a `Value`, so the same
  parser loads both sides.
- **It collects *all* violations**, not just the first. `validate` returns a
  `Vec<Violation>`, each carrying a JSON-pointer-ish `path` and a message, so a
  single call can drive a complete error report back to a caller.

> Design note: the keyword set is intentionally the widely-used core of JSON
> Schema; anything needing a regex engine or a full reference resolver is
> deliberately out of scope (see [Limitations](#limitations)).

## Enabling

Part of the default `codec` feature — no extra dependency, no `tokio`:

```toml
[dependencies]
ferroly = "*"                 # codec (and schema) on by default
```

```rust
use ferroly::codec::{json, schema};
```

## Quick start

```rust
use ferroly::codec::{json, schema};

fn main() {
    // The schema is itself parsed into a Value.
    let schema = json::from_str(r#"{
        "type": "object",
        "required": ["name", "age"],
        "properties": {
            "name": { "type": "string", "minLength": 1 },
            "age":  { "type": "integer", "minimum": 0 }
        }
    }"#).unwrap();

    // Valid data passes.
    let ok = json::from_str(r#"{ "name": "Ada", "age": 36 }"#).unwrap();
    assert!(schema::validate(&schema, &ok).is_ok());

    // Invalid data reports every failure at once.
    let bad = json::from_str(r#"{ "age": -1 }"#).unwrap();
    let errs = schema::validate(&schema, &bad).unwrap_err();
    assert_eq!(errs.len(), 2); // missing "name" + age below minimum
    for v in &errs {
        println!("{v}"); // "<path>: <message>"
    }
}
```

## API reference

```rust
/// Validates `data` against `schema`, collecting every violation.
pub fn validate(schema: &Value, data: &Value) -> Result<(), Vec<Violation>>;

/// A single validation failure.
pub struct Violation {
    pub path: String,     // JSON-pointer-ish, e.g. "/properties/age" -> "/age"
    pub message: String,  // human-readable description
}
```

`Violation` derives `Debug`, `Clone`, `PartialEq`, `Eq`, and implements
`Display` as `"{path}: {message}"` (an empty path renders as `/`).

`validate` returns `Ok(())` when `data` fully satisfies `schema`, or
`Err(violations)` with **all** failures found in a single traversal.

## Supported keywords

The validator recognizes this keyword set. Keywords outside it are ignored —
see [Limitations](#limitations) for what that means for a schema that relies on
one.

| Keyword | Applies to | Behavior |
| --- | --- | --- |
| `type` | any | a type name — `string`, `integer`, `number`, `boolean`, `object`, `array`, `null` — or a **non-empty array of type names**, any of which may match (`["string", "null"]`). A mismatch is reported and further checks on that node are skipped (to avoid noise). A `type` the validator cannot interpret — an unknown name, an empty array, a non-string member, or any other shape — **fails closed**: it is reported as a violation whose message starts with `schema error:`. |
| `const` | any | value must equal the given value (see [Equality](#equality)). |
| `enum` | any | value must equal one of the array's members (see [Equality](#equality)). |
| `minimum` / `maximum` | numbers | inclusive bounds (compared as `f64`). |
| `minLength` / `maxLength` | strings | bounds on **character** count (not bytes). |
| `minItems` / `maxItems` | arrays | bounds on element count. |
| `required` | objects | each named property must be present. |
| `properties` | objects | each present property is validated against its sub-schema. |
| `items` | arrays | every element is validated against the item sub-schema, recursively. |

Type-mapping notes:

- `integer` matches `Value::Int`, `Value::UInt`, and a `Value::Float` with a
  zero fractional part (`1.0`), as JSON Schema defines it. `1.5`, `NaN`, and
  infinities are not integers.
- `number` matches `Int`, `UInt`, or `Float`.
- Numeric bounds (`minimum`/`maximum`) apply to any numeric `Value`, compared as
  `f64`.
- `properties` only validates keys that are **present**; use `required` to
  mandate presence. A property absent from the data is simply not checked.

### Equality

`const` and `enum` compare by **JSON value**, not by how the `Value` happens to
be stored:

- Numbers compare by value across `Int`, `UInt`, and `Float`: `5`, `5u64`, and
  `5.0` are all equal, so `"enum": [5]` accepts a `u32` field that encoded as
  `UInt(5)`. `5` and `5.5` are different, and values beyond `i64` keep their
  identity (`u64::MAX` is not `-1`).
- Objects compare **without regard to member order**: `{"a": 1, "b": 2}` equals
  `{"b": 2, "a": 1}`. Member counts and every value must match.
- Arrays compare element by element, **in order**.
- The rules apply recursively, so a number nested in an array or object inside
  a `const` compares by value too.

```rust
use ferroly::codec::{json, schema, Value};

// Any numeric representation of 5 is "5".
let schema_enum = json::from_str(r#"{ "enum": [5, "five"] }"#).unwrap();
assert!(schema::validate(&schema_enum, &Value::Int(5)).is_ok());
assert!(schema::validate(&schema_enum, &Value::UInt(5)).is_ok());
assert!(schema::validate(&schema_enum, &Value::Float(5.0)).is_ok());
assert!(schema::validate(&schema_enum, &Value::Float(5.5)).is_err());

// Object members may come in any order.
let schema_const = json::from_str(r#"{ "const": {"a": 1, "b": [1, 2]} }"#).unwrap();
let same = json::from_str(r#"{"b": [1, 2], "a": 1}"#).unwrap();
let reordered_array = json::from_str(r#"{"b": [2, 1], "a": 1}"#).unwrap();
assert!(schema::validate(&schema_const, &same).is_ok());
assert!(schema::validate(&schema_const, &reordered_array).is_err());
```

### Type unions

```rust
use ferroly::codec::{json, schema};

let nullable = json::from_str(r#"{ "type": ["string", "null"] }"#).unwrap();
assert!(schema::validate(&nullable, &json::from_str(r#""x""#).unwrap()).is_ok());
assert!(schema::validate(&nullable, &json::from_str("null").unwrap()).is_ok());

let errs = schema::validate(&nullable, &json::from_str("5").unwrap()).unwrap_err();
assert_eq!(errs[0].message, "expected one of the types 'string', 'null'");

// A typo in a type name is reported, not silently accepted.
let typo = json::from_str(r#"{ "type": "strnig" }"#).unwrap();
let errs = schema::validate(&typo, &json::from_str(r#""x""#).unwrap()).unwrap_err();
assert_eq!(errs[0].message, "schema error: unknown type 'strnig'");
```

## Validating against a JSON Schema

A realistic example: a user record with a typed, bounded shape, nested array
items, and an enum. One `validate` call surfaces every problem.

```rust
use ferroly::codec::{json, schema};

let schema = json::from_str(r#"{
    "type": "object",
    "required": ["name", "age", "tags"],
    "properties": {
        "name": { "type": "string", "minLength": 1 },
        "age":  { "type": "integer", "minimum": 0, "maximum": 150 },
        "role": { "enum": ["admin", "user"] },
        "tags": {
            "type": "array",
            "minItems": 1,
            "items": { "type": "string" }
        }
    }
}"#).unwrap();

// All good.
let ok = json::from_str(r#"{ "name": "Ada", "age": 36, "tags": ["x"] }"#).unwrap();
assert!(schema::validate(&schema, &ok).is_ok());

// Four independent failures, reported together.
let bad = json::from_str(r#"{ "name": "", "age": 200, "role": "root", "tags": [] }"#).unwrap();
let errs = schema::validate(&schema, &bad).unwrap_err();
assert_eq!(errs.len(), 4);
assert!(errs.iter().any(|e| e.path == "/name"));   // minLength
assert!(errs.iter().any(|e| e.path == "/age"));    // maximum
assert!(errs.iter().any(|e| e.path == "/role"));   // enum
assert!(errs.iter().any(|e| e.path == "/tags"));   // minItems
```

## Nested items and paths

Violations carry a path built as the validator descends: object properties
append `/<key>`, array items append `/<index>`. A failure inside a nested item
reports the exact location.

```rust
use ferroly::codec::{json, schema};

let schema = json::from_str(
    r#"{ "type": "array", "items": { "type": "integer", "minimum": 1 } }"#,
).unwrap();

// The element at index 1 is below the minimum.
let errs = schema::validate(&schema, &json::from_str("[1, 0, 3]").unwrap()).unwrap_err();
assert_eq!(errs.len(), 1);
assert_eq!(errs[0].path, "/1");

// A top-level type mismatch stops descent and reports the root.
let errs = schema::validate(&schema, &json::from_str(r#"{"not":"array"}"#).unwrap()).unwrap_err();
assert_eq!(errs[0].message, "expected type 'array'");
```

## Combining decode and validate

The idiomatic flow: dispatch-decode the request body to a `Value`, validate it,
then decode into your concrete type only once the shape is known-good.

```rust
use ferroly::codec::{decode, json, schema, Value};

fn accept(content_type: &str, body: &[u8], schema: &Value) -> Result<Value, String> {
    // 1. Decode to a dynamic Value using the request's content type.
    let data: Value = decode(content_type, body).map_err(|e| e.to_string())?;

    // 2. Validate the shape, collecting every problem.
    if let Err(violations) = schema::validate(schema, &data) {
        let report = violations.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("; ");
        return Err(report);
    }

    // 3. Proceed — decode into a concrete type, etc.
    Ok(data)
}
```

## Error handling

`schema::validate` does not use [`CodecError`](codec.md#error-handling). Instead
it returns `Err(Vec<Violation>)` so a caller gets the complete list in one pass.
Each `Violation` is `Display`-formatted as `"{path}: {message}"`; join them for a
report, or map them onto field-level API errors by `path`.

There is no separate "schema is malformed" error type. A `type` keyword the
validator cannot interpret is reported as an ordinary `Violation` (its message
starts with `schema error:`), so a broken schema fails validation instead of
passing everything. Other keywords of an unexpected shape (a non-numeric
`minimum`, a non-array `required`) contribute no checks, and keywords outside
the supported set are ignored — review schemas that come from an untrusted or
third-party source against the [supported keyword list](#supported-keywords).

## Limitations

Unsupported keywords are **ignored, not rejected**: a schema that relies on one
validates more loosely than its author intended (for example,
`"additionalProperties": false` does not reject extra properties). Check a
schema against the list below before relying on it.

Deliberately **unsupported** keywords, and why:

- **`pattern` / regex-based checks** — Ferroly carries no regex engine, by
  design. Enforce string patterns in application code.
- **Composition: `allOf`, `anyOf`, `oneOf`, `not`, `if`/`then`/`else`** — the
  validator does not combine sub-schemas.
- **`$ref` / `$defs` / `definitions`** — no reference resolution; schemas must be
  self-contained and inlined.
- **`additionalProperties`, `patternProperties`, `dependencies`,
  `propertyNames`, `uniqueItems`, `multipleOf`, `format`** — not modeled.

Other bounds:

- `minLength` / `maxLength` count Unicode **characters**, not bytes or grapheme
  clusters.
- `minimum` / `maximum` compare in `f64`, so extremely large integers can lose
  precision at the boundary. (`const` / `enum` compare integers exactly.)
- Duplicate keys in an object resolve last-wins, as elsewhere in the codec, for
  `properties`, `required`, and object equality.
- `properties` validates only present keys; combine with `required` to mandate
  presence.

If you need full JSON-Schema semantics (composition, `$ref`, `pattern`), validate
with dedicated JSON-Schema tooling and use this module for the fast,
dependency-free common case.

## See also

- [codec](codec.md) — the `Value` model, the codecs, and `CodecError`.
- [derive](derive.md) — deriving `Encode` / `Decode` for the types you validate
  into.
- [config](config.md) — layered configuration that decodes through the codecs.

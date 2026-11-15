# Schema validation

**Feature:** `codec` (default) · **Module:** `ferroly::codec::schema`

A pragmatic JSON-Schema **subset** validator that runs over the codec's
[`Value`](codec.md#the-value-model) model. Validate an already-decoded value
against a schema that is itself a `Value` — typically a JSON-Schema document you
parsed with [`json::from_str`](codec.md#json-codec).

## Overview

`schema` handles the common structural-validation case: after you decode an
input into a `Value`, check that it has the shape your service expects —
required fields present, numbers in range, strings the right length and
shape (`pattern`), arrays bounded, nested items well-formed.

Two design choices shape the module:

- **It validates a `Value`, not a stream.** Decode first (with any of the
  [codecs](codec.md)), then validate. The schema is also a `Value`, so the same
  parser loads both sides.
- **It collects every violation**, not just the first, up to a cap (100 by
  default, see [Bounded output](#bounded-output)). `validate` returns a
  `Vec<Violation>`, each carrying a JSON Pointer `path` and a message, so a
  single call can drive a complete error report back to a caller.

> Design note: the keyword set is intentionally the widely-used core of JSON
> Schema; composition and a full reference resolver are deliberately out of
> scope (see [Limitations](#limitations)). `pattern` is matched by a small
> built-in regex engine that runs in linear time (see [Patterns](#patterns)).

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
/// How many violations a validation collects before it stops.
pub const DEFAULT_MAX_VIOLATIONS: usize = 100;

/// Validates `data` against `schema`, collecting violations up to
/// DEFAULT_MAX_VIOLATIONS. Compiles the schema on every call.
pub fn validate(schema: &Value, data: &Value) -> Result<(), Vec<Violation>>;

/// A schema compiled once for repeated validation (Clone + Send + Sync).
pub struct Schema { /* private */ }

impl Schema {
    /// Compiles `schema`; fails on the first keyword it cannot interpret.
    pub fn compile(schema: &Value) -> Result<Schema, SchemaError>;
    /// Compiles the schema a type describes (`#[derive(JsonSchema)]`).
    pub fn of<T: JsonSchema + ?Sized>() -> Result<Schema, SchemaError>;
    /// A different violation cap (at least 1).
    pub fn with_max_violations(self, max: usize) -> Schema;
    pub fn max_violations(&self) -> usize;
    /// Validates `data`, collecting violations up to the cap.
    pub fn validate(&self, data: &Value) -> Result<(), Vec<Violation>>;
}

/// A single validation failure.
pub struct Violation {
    pub path: String,     // JSON Pointer into the data, e.g. "/items/3/name"
    pub message: String,  // human-readable description
}

impl Violation {
    /// The marker that ends a list cut short at the cap.
    pub fn is_truncation(&self) -> bool;
}

/// A type that describes its encoded form as a JSON-Schema document.
pub trait JsonSchema {
    fn json_schema() -> Value;
}
// plus `#[derive(JsonSchema)]`, re-exported next to the trait.

/// Why `Schema::compile` rejected a schema (#[non_exhaustive]).
pub struct SchemaError {
    pub path: String,     // JSON Pointer to the keyword, e.g. "/properties/sku/pattern"
    pub message: String,  // human-readable description
}
```

`Violation` derives `Debug`, `Clone`, `PartialEq`, `Eq`, and implements
`Display` as `"{path}: {message}"` (an empty path renders as `/`).
`SchemaError` derives the same traits, implements `std::error::Error`, and
displays as `"schema error at {path}: {message}"`.

`validate` returns `Ok(())` when `data` fully satisfies `schema`, or
`Err(violations)` with the failures found in a single traversal (up to the
cap).

### Bounded output

A hostile input can fail a schema in millions of places: an 8 MiB array of
numbers against `{"items":{"type":"string"}}` is four million violations. So
validation **stops** once it has found one violation more than its cap, and
returns the first `max` followed by a marker violation (empty `path`, message
`too many violations: stopped after <max>`) for which
`Violation::is_truncation()` is true. The cap is
`DEFAULT_MAX_VIOLATIONS` (100) for the free `validate` and for a compiled
`Schema` unless changed with `with_max_violations` (`1` reports only the first
violation). Exactly `max` violations come back without a marker.

```rust
use ferroly::codec::{json, schema::{Schema, DEFAULT_MAX_VIOLATIONS}};

let schema = Schema::compile(&json::from_str(r#"{"items":{"type":"string"}}"#).unwrap()).unwrap();
let data = json::from_str(&format!("[{}1]", "1,".repeat(10_000))).unwrap();
let errs = schema.validate(&data).unwrap_err();
assert_eq!(errs.len(), DEFAULT_MAX_VIOLATIONS + 1);
assert!(errs.last().unwrap().is_truncation());

let first_only = schema.clone().with_max_violations(1);
assert_eq!(first_only.validate(&data).unwrap_err().len(), 2);
```

A `pattern` violation quotes at most 64 characters of the pattern, so the
messages stay small however long the pattern is.

### Compile once, validate many

`Schema::compile` does the per-schema work once: it compiles every `pattern`
to a matcher and interprets every `type` keyword. Reuse the compiled `Schema`
for every value you validate — share it across threads behind an `Arc` or a
`static`. Compiling also rejects a schema the validator cannot interpret up
front, with a JSON Pointer to the offending keyword (property names are
escaped per RFC 6901: `/` as `~1`, `~` as `~0`).

```rust
use ferroly::codec::{json, schema::Schema};

let schema = Schema::compile(&json::from_str(r#"{
    "type": "object",
    "properties": {
        "sku":  { "type": "string", "pattern": "^[A-Z]{3}-\\d{4}$" },
        "tags": { "type": "array", "items": { "pattern": "^[a-z]+$" } }
    }
}"#).unwrap()).unwrap();

let ok = json::from_str(r#"{ "sku": "ABC-1234", "tags": ["a", "bc"] }"#).unwrap();
assert!(schema.validate(&ok).is_ok());

let bad = json::from_str(r#"{ "sku": "ABC-123", "tags": ["ok", "Not ok"] }"#).unwrap();
let errs = schema.validate(&bad).unwrap_err();
assert_eq!(errs[0].path, "/sku");
assert_eq!(errs[1].to_string(), r#"/tags/1: does not match pattern "^[a-z]+$""#);

// Unsupported syntax is a compile error that names the keyword.
let err = Schema::compile(&json::from_str(
    r#"{ "properties": { "sku": { "pattern": "(?<=x)y" } } }"#,
).unwrap()).unwrap_err();
assert_eq!(err.path, "/properties/sku/pattern");
assert!(err.message.contains("lookbehind assertions are not supported"));
```

The free `validate` function is the one-shot form: it compiles the schema on
every call (so prefer `Schema` on a hot path), and instead of failing up front
it reports an uninterpretable keyword as a `Violation` whose message starts
with `schema error:` wherever that sub-schema is applied.

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
| `pattern` | strings | the string must match the ECMA-262 regular expression **somewhere** (an unanchored search; anchor with `^…$` for a whole-string match). Non-strings are not checked. See [Patterns](#patterns) for the supported syntax; anything outside it, and a non-string `pattern`, is a schema error. |
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

## Patterns

`pattern` is matched by a small regex engine built into the module — no
dependency. It accepts the ECMA-262 subset that JSON Schema recommends for
interoperable patterns, with the strict escape rules of the `u` (Unicode) flag
that JSON Schema says patterns should be built with, and matches over Unicode
scalar values (so `.` and `[^a]` consume a whole emoji, and `^.{2}$` agrees
with `maxLength: 2`).

### Supported syntax

| Syntax | Meaning |
| --- | --- |
| `a`, `é`, `😀` | a literal character |
| `\n` `\t` `\r` `\f` `\v` `\0` | newline, tab, carriage return, form feed, vertical tab, NUL |
| `\xHH`, `\uHHHH`, `\u{H…}` | a character by code point (`\uD83D\uDE00` surrogate pairs combine; `\u{…}` up to `10FFFF`) |
| `\cA` … `\cZ` | a control character |
| `\.` `\\` `\/` `\^` `\$` `\*` `\+` `\?` `\(` `\)` `\[` `\]` `\{` `\}` `\\|` | an escaped syntax character, as a literal |
| `.` | any character except a line terminator (`\n`, `\r`, U+2028, U+2029) |
| `^`, `$` | start / end of the whole string (no multiline mode; `$` does not match before a trailing `\n`) |
| `\b`, `\B` | a word boundary / not a word boundary (word characters are `\w`) |
| `\d`, `\D` | an ASCII digit `[0-9]` / anything else |
| `\w`, `\W` | an ASCII word character `[A-Za-z0-9_]` / anything else |
| `\s`, `\S` | ECMA-262 white space and line terminators (tab, LF, VT, FF, CR, space, U+00A0, U+1680, U+2000–U+200A, U+2028, U+2029, U+202F, U+205F, U+3000, U+FEFF) / anything else |
| `[abc]`, `[a-z]`, `[^…]` | a bracket class, ranges, negation; members may be escapes, including `\d \w \s` and their negations, `\-`, and `\b` (backspace) |
| `(…)`, `(?:…)`, `(?<name>…)` | groups (capturing, non-capturing and named groups all just group) |
| `a\|b` | alternation |
| `*` `+` `?` `{n}` `{n,}` `{n,m}` | quantifiers; a trailing `?` (lazy, `*?` …) is accepted and does not change whether a string matches |

### Rejected syntax

Everything outside the table is a **schema error** — never silently ignored and
never read as a literal:

- backreferences `\1` … and `\k<name>`;
- lookahead `(?=…)` / `(?!…)` and lookbehind `(?<=…)` / `(?<!…)`;
- inline flags and modifiers `(?i)`, `(?i:…)`;
- Unicode property escapes `\p{…}` / `\P{…}`;
- octal escapes (`\0` followed by a digit) and any other unknown escape, such
  as `\a`, `\e` or `\-` outside a class (Unicode mode allows identity escapes
  only for syntax characters);
- an unescaped `{`, `}` or `]` (write `\{`, `\}`, `\]`), a quantifier with
  nothing to repeat (`*a`, `a**`, `a{2}{3}`) or on an assertion (`^*`, `\b+`),
  `{n,m}` with `n > m`, a reversed class range `[z-a]`, a class escape as a range
  endpoint `[\d-z]`;
- unbalanced `(` / `)` / `[`, a trailing `\`;
- a group name repeated where both groups could take part in one match
  (`(?<n>a)(?<n>b)`). A name may repeat across alternatives,
  `(?<y>\d{4})|(?<y>\d{2})`, as ES2025 allows.

A lone surrogate escape (`\uD800`, `\u{DC00}`, or the part of a class range
inside U+D800–U+DFFF) is accepted but matches nothing, since a Rust string holds
only Unicode scalar values: `^[^\uD800-\uDFFF]*$` matches every string, and
`[\uD800-\uDFFF]` matches none.

### Performance guarantee

The engine compiles each pattern to a Thompson NFA and runs it as a Pike VM:
all live threads advance together, one input character at a time, and each
program position holds at most one thread per step. It never backtracks, so a
match costs at most **O(program size × string length)** for any pattern —
catastrophic-backtracking ("ReDoS") shapes such as `(a+)+$` or `^(a|aa)+$`
finish in linear time on a long near-miss input. A pattern anchored with `^`
stops as soon as no thread survives. The thread lists live in a per-thread
scratch buffer reused across calls, so matching allocates nothing per call once
warm, and nothing proportional to the input.

Counted repetition does not multiply the live threads. The optional part of
`X{n,m}` is one counting instruction when `X` is a single character or class
(it keeps only the thread that has used the fewest copies, which can do
everything the others can), and a nested `(?:X(?:X(?:X)?)?)?` chain otherwise,
so `^[a-z0-9]{1,255}$` costs about what `^[a-z0-9]+$` does, and
`[a-z]{1,4900}b` scans a long string in linear time with a handful of threads.

Compilation is bounded by three caps, each reported as a schema error:

| Cap | Limit |
| --- | --- |
| pattern length | 4096 bytes |
| group nesting depth | 64 |
| compiled program size | 10 000 instructions |

Counted repetition expands into copies of its operand, so the program size is
computed **before** expansion and an oversized pattern is rejected without being
built: `(?:a{1000}){1000}` (a million instructions) fails at once, while
`^[a-z0-9]{1,64}@[a-z0-9.-]{1,255}$` compiles comfortably.

## Generating a schema from a type

`#[derive(JsonSchema)]` writes the schema for you, from the same derive model
and `#[ferroly(...)]` attributes as `Encode` / `Decode`, so it matches what
`Encode` writes. `Schema::of::<T>()` compiles it; `T::json_schema()` returns the
document (to publish, or to hand to full JSON-Schema tooling).

```rust
use ferroly::codec::schema::{JsonSchema, Schema};
use ferroly::codec::{json, Decode, Encode};

#[derive(Encode, Decode, JsonSchema)]
#[ferroly(rename_all = "camelCase")]
struct NewUser {
    user_name: String,
    age: Option<u8>,
}

let schema = Schema::of::<NewUser>().unwrap();
assert!(schema.validate(&json::from_str(r#"{"userName":"ada"}"#).unwrap()).is_ok());
assert!(schema.validate(&json::from_str(r#"{"age":300}"#).unwrap()).is_err());
```

How the model maps:

| Rust | Schema |
| --- | --- |
| named struct | `"type":"object"`, `properties` by wire key (`rename`, `rename_all`) |
| field | `required` unless it is an `Option`, has `default`, or is `skip_none`; `skip` fields are left out |
| `flatten` field | the inner type's `properties` (and `required`, unless the field is an `Option`) merged in |
| `deny_unknown_fields` | `"additionalProperties": false` |
| newtype / tuple / unit struct | the inner schema / an array with `prefixItems` and exact `minItems`/`maxItems` / `"type":"null"` |
| enum, unit variants only | `"type":"string"` with an `enum` of the names (aliases included) |
| enum with data variants | `anyOf` alternatives, plus the `type` union the validator checks |
| internally tagged enum | an object whose tag must be one of the variant names, plus `anyOf` |
| `bool`, integers, floats, `String`, `char` | `boolean`; `integer` with the type's `minimum`/`maximum` for 8–32-bit types (`minimum: 0` for unsigned); `number`; `string`; one-character `string` |
| `Option<T>` | `T`'s schema with `null` added to its `type` (and `enum`), and a `{"type":"null"}` alternative added to its `anyOf` |
| `Vec<T>`, `[T; N]`, sets | `array` of `T` (exact length for `[T; N]`, `uniqueItems` for sets) |
| `HashMap` / `BTreeMap<String, V>` | `object` with `additionalProperties: V` |
| `DateTime`, `SystemTime` | an RFC 3339 `string` with `format: date-time` and a shape `pattern` (`SystemTime` also a `number`, and the `{ "secs", "nanos" }` `object` it encodes as outside years 0000–9999) |
| `Value` | `{}` (anything) |
| a recursive type (`struct Tree { kids: Vec<Tree> }`) | described once; where it nests itself the schema is `{}` (anything), as the document has no `$ref` |

The validator ignores `anyOf`, `additionalProperties`, `prefixItems`,
`uniqueItems` and `format` (see [Limitations](#limitations)); the generated
document still carries them for other tools. Field aliases are not described:
the schema is the canonical, encoded form.

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

Violations carry a JSON Pointer (RFC 6901) built as the validator descends:
object properties append `/<key>` (with `~` written `~0` and `/` written `~1`),
array items append `/<index>`. A failure inside a nested item reports the exact
location.

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
it returns `Err(Vec<Violation>)` so a caller gets the list (up to the
[cap](#bounded-output)) in one pass.
Each `Violation` is `Display`-formatted as `"{path}: {message}"`; join them for a
report, or map them onto field-level API errors by `path`.

A schema the validator cannot interpret — a `type` it does not understand, or
a `pattern` that is not a string, uses unsupported syntax, or exceeds a cap —
fails closed either way:

- `Schema::compile` returns a `SchemaError` naming the first such keyword by
  JSON Pointer (`/properties/sku/pattern`), so you find out when the schema is
  loaded rather than on the first request.
- The free `validate` reports it as an ordinary `Violation` (message prefix
  `schema error:`) wherever that sub-schema is applied, so a broken schema fails
  validation instead of passing everything. A sub-schema that is never applied
  (for example, under a property the data does not have) reports nothing.

Other keywords of an unexpected shape (a non-numeric `minimum`, a non-array
`required`) contribute no checks, and keywords outside the supported set are
ignored — review schemas that come from an untrusted or third-party source
against the [supported keyword list](#supported-keywords).

## Limitations

Unsupported keywords are **ignored, not rejected**: a schema that relies on one
validates more loosely than its author intended (for example,
`"additionalProperties": false` does not reject extra properties). Check a
schema against the list below before relying on it.

Deliberately **unsupported** keywords, and why:

- **Composition: `allOf`, `anyOf`, `oneOf`, `not`, `if`/`then`/`else`** — the
  validator does not combine sub-schemas.
- **`$ref` / `$defs` / `definitions`** — no reference resolution; schemas must be
  self-contained and inlined.
- **`additionalProperties`, `patternProperties`, `dependencies`,
  `propertyNames`, `uniqueItems`, `multipleOf`, `format`** — not modeled.

Other bounds:

- `minLength` / `maxLength` count Unicode **characters**, not bytes or grapheme
  clusters.
- `pattern` supports the interoperable ECMA-262 subset only (see
  [Rejected syntax](#rejected-syntax)) and has no flags: no case-insensitive,
  multiline or dot-all mode, and `\d` / `\w` / `\b` are ASCII-only, as in
  ECMA-262.
- `minimum` / `maximum` compare in `f64`, so extremely large integers can lose
  precision at the boundary. (`const` / `enum` compare integers exactly.)
- Duplicate keys in an object resolve last-wins, as elsewhere in the codec, for
  `properties`, `required`, and object equality.
- `properties` validates only present keys; combine with `required` to mandate
  presence.

If you need full JSON-Schema semantics (composition, `$ref`,
`patternProperties`, the full ECMA-262 regex dialect), validate with dedicated
JSON-Schema tooling and use this module for the fast, dependency-free common
case.

## See also

- [codec](codec.md) — the `Value` model, the codecs, and `CodecError`.
- [derive](derive.md) — deriving `Encode` / `Decode` for the types you validate
  into.
- [config](config.md) — layered configuration that decodes through the codecs.

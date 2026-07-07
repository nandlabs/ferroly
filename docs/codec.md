# Codec

**Feature:** `codec` (default) · **Module:** `ferroly::codec`

The in-house, dependency-free encoding toolkit. It
provides a format-agnostic data model ([`Value`](#the-value-model)), the
[`Encode`](#the-encode-and-decode-traits) / [`Decode`](#the-encode-and-decode-traits)
traits with derives, hand-written JSON / XML / YAML / TOML codecs, a
content-type dispatch layer, and an optional
[JSON-Schema subset validator](schema.md).

## Overview

`ferroly::codec` solves the problem every service hits: turning Rust values into
bytes on the wire and back, across more than one format, without pulling in a
large external encoding stack. Everything here is hand-rolled and carries no
runtime dependencies.

Key concepts:

- **`Value`** — an ordered, format-neutral tree (`Null`, `Bool`, `Int`, `UInt`,
  `Float`, `Str`, `Bytes`, `Array`, `Object`). Every codec can produce and
  consume it, so it is the lingua franca for dynamic data.
- **`Encode` / `Decode`** — the two core traits. `Encode` turns a type into a
  `Value` (or streams it straight to an [`Encoder`](#streaming-fast-path)).
  `Decode` builds a type from a `Value` (or pulls it from a
  [`Decoder`](#streaming-fast-path)). Both have blanket impls for the standard
  library and a `#[derive]`.
- **Streaming fast-path** — JSON has a second path that bypasses the `Value`
  tree entirely, writing/reading field by field. This is what keeps JSON
  encoding fast, with decode at parity.
- **Format dispatch** — [`codec::encode`](#content-type-dispatch) /
  [`codec::decode`](#content-type-dispatch) pick a codec from a MIME content
  type, so an HTTP handler can serve JSON, XML, YAML, or TOML from one code path.

> Design note: the content-type registry is a closed
> [`Format`](#content-type-dispatch) enum matched at compile time, and the wire
> mapping comes from compile-time derives rather than any runtime reflection —
> the idiomatic Rust approach, with no reflection cost and no runtime encoding
> dependency.

Terminology throughout Ferroly: we say **encode** / **decode** / **encoding**,
the traits are `Encode` and `Decode`.

## Enabling

`codec` is a **default feature** and is pure std — no `tokio`, no other runtime
dependency. It is enabled unless you opt out of defaults:

```toml
[dependencies]
ferroly = "*"                 # codec is on by default

# or, if you disabled default features:
ferroly = { version = "*", default-features = false, features = ["codec"] }
```

Many higher-level features (`config`, `genai`, `auth`, `log`, `vectorstore`,
`messaging`, `rest`, `turbo`) depend on `codec` and turn it on transitively.

## Quick start

```rust
use ferroly::codec::{json, Decode, Encode};

#[derive(Encode, Decode, PartialEq, Debug)]
struct Point {
    x: i32,
    y: i32,
}

fn main() {
    let p = Point { x: 1, y: 2 };

    // Encode straight to a compact JSON string (streaming, no Value built).
    let s = json::encode(&p);
    assert_eq!(s, r#"{"x":1,"y":2}"#);

    // Decode back (pulled straight from the parser).
    let back: Point = json::decode(&s).unwrap();
    assert_eq!(back, p);
}
```

## API reference

### The `Value` model

```rust
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),          // for values exceeding i64::MAX
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),   // insertion-ordered
}
```

Accessors (all borrow `&self`):

| Method | Returns | Notes |
| --- | --- | --- |
| `as_object()` | `Option<&Vec<(String, Value)>>` | object entries |
| `as_array()` | `Option<&Vec<Value>>` | array elements |
| `as_str()` | `Option<&str>` | string only |
| `as_bool()` | `Option<bool>` | bool only |
| `as_i64()` | `Option<i64>` | `Int`, or `UInt` that fits |
| `as_u64()` | `Option<u64>` | `UInt`, or non-negative `Int` |
| `as_f64()` | `Option<f64>` | `Float`, `Int`, or `UInt` |
| `get(key)` | `Option<&Value>` | object lookup by key |
| `get_index(i)` | `Option<&Value>` | array index |
| `is_null()` | `bool` | true for `Value::Null` |

Free function:

```rust
pub fn find<'a>(obj: &'a [(String, Value)], key: &str) -> Option<&'a Value>;
```

`Value` derives `Debug`, `Clone`, `PartialEq`, and `Default` (defaulting to
`Null`).

### The `Encode` and `Decode` traits

```rust
pub trait Encode {
    fn encode(&self) -> Value;
    fn encode_to<E: Encoder>(&self, e: &mut E) { /* default: replay encode() */ }
}

pub trait Decode: Sized {
    fn decode(value: &Value) -> Result<Self, CodecError>;
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> { /* default */ }
}
```

| Method | Description |
| --- | --- |
| `Encode::encode(&self) -> Value` | Produce the `Value` representation of `self`. |
| `Encode::encode_to<E: Encoder>(&self, e)` | Stream `self` straight to an `Encoder` (default replays `encode()`; impls override). |
| `Decode::decode(value: &Value) -> Result<Self, _>` | Build `Self` from a `Value`. |
| `Decode::decode_from<D: Decoder>(d) -> Result<Self, _>` | Build `Self` straight from a `Decoder` (default pulls a `Value`; impls override). |

The streaming sink and source traits and the value-walker:

```rust
pub trait Encoder: Sized { /* scalar + container emit methods, see below */ }
pub trait Decoder: Sized { /* pull-based read methods, see below */ }
pub fn emit_value<E: Encoder>(e: &mut E, value: &Value);
```

`Encoder` is the push-based output sink a streaming format implements:

| Method | Description |
| --- | --- |
| `encode_null(&mut self)` | Emit a `null`. |
| `encode_bool(&mut self, v: bool)` | Emit a boolean. |
| `encode_i64(&mut self, v: i64)` | Emit a signed integer. |
| `encode_u64(&mut self, v: u64)` | Emit an unsigned integer. |
| `encode_f64(&mut self, v: f64)` | Emit a floating-point number. |
| `encode_str(&mut self, v: &str)` | Emit a string. |
| `encode_bytes(&mut self, v: &[u8])` | Emit a byte string. |
| `begin_seq(&mut self, len: usize)` | Begin a sequence of `len` items. |
| `seq_entry<V: Encode + ?Sized>(&mut self, v: &V)` | Emit one sequence element (with any separator). |
| `end_seq(&mut self)` | End the current sequence. |
| `begin_map(&mut self, len: usize)` | Begin a map of `len` entries. |
| `map_entry<V: Encode + ?Sized>(&mut self, key: &str, v: &V)` | Emit one map entry (key + separator + value). |
| `end_map(&mut self)` | End the current map. |

`Decoder` is the pull-based input source a streaming format implements:

| Method | Description |
| --- | --- |
| `decode_value(&mut self) -> Result<Value, _>` | Pull the next value as a `Value` (scalar leaves and the fallback path). |
| `decode_string(&mut self) -> Result<String, _>` | Pull the next value as a string in one allocation. |
| `peek_null(&mut self) -> bool` | `true` if the next value is `null`, without consuming it. |
| `decode_null(&mut self) -> Result<(), _>` | Consume a `null`. |
| `read_seq<F>(&mut self, f: F) -> Result<(), _>` | Read a sequence, invoking `f` at each element. |
| `read_map<F>(&mut self, f: F) -> Result<(), _>` | Read a map, invoking `f` with each key positioned at its value. |
| `skip_value(&mut self) -> Result<(), _>` | Skip the next value (an unknown field). |

`emit_value(e, &value)` walks any `Value` into any `Encoder` — the fallback for
`encode_to`'s default and the implementation of `Encode for Value`.

### Format submodules

Each of [`json`](#json-codec), [`xml`](#xml-codec), [`yaml`](#yaml-codec), and
[`toml`](#toml-codec) exposes the same shape:

```rust
pub fn encode<T: Encode>(value: &T) -> String;
pub fn encode_to_vec<T: Encode>(value: &T) -> Vec<u8>;
pub fn decode<T: Decode>(input: &str) -> Result<T, CodecError>;
pub fn decode_from_slice<T: Decode>(input: &[u8]) -> Result<T, CodecError>;
pub fn from_str(input: &str) -> Result<Value, CodecError>;
```

JSON additionally provides `to_string(&Value) -> String` and
`from_slice(&[u8]) -> Result<Value, CodecError>`. For rendering a `Value` to
XML/YAML/TOML, call `encode(&value)` (a `Value` is itself `Encode`).

### Content-type dispatch

```rust
pub enum Format { Json, Xml, Yaml, Toml }
impl Format { pub fn from_content_type(content_type: &str) -> Option<Format>; }

pub fn resolve(content_type: &str) -> Result<Format, CodecError>;
pub fn encode<T: Encode>(content_type: &str, value: &T) -> Result<Vec<u8>, CodecError>;
pub fn decode<T: Decode>(content_type: &str, bytes: &[u8]) -> Result<T, CodecError>;
```

### Errors

```rust
pub enum CodecError {
    UnsupportedContentType(String),
    Parse(String),
    Expected(&'static str),
    MissingField(String),
    UnknownVariant(String),
    OutOfRange(&'static str),
    Message(String),
}
```

See [Error handling](#error-handling).

## The `Value` model in depth

`Value` is the intermediate every codec can produce and consume. `Object`
preserves **insertion order** — it is a `Vec<(String, Value)>`, not a hash map —
so encoded output is stable and field order stays meaningful (important for
signatures, diffs, and human-readable config).

Integers split into `Int(i64)` and `UInt(u64)` so values above `i64::MAX`
survive a round trip; the [accessors](#the-value-model) bridge the two where the
value fits.

```rust
use ferroly::codec::{json, Value};

let v = json::from_str(r#"{"user":{"name":"Ada","roles":["admin","dev"]}}"#).unwrap();

// Navigate dynamically without a target struct.
let name = v.get("user").and_then(|u| u.get("name")).and_then(Value::as_str);
assert_eq!(name, Some("Ada"));

let first_role = v.get("user")
    .and_then(|u| u.get("roles"))
    .and_then(|r| r.get_index(0))
    .and_then(Value::as_str);
assert_eq!(first_role, Some("admin"));
```

### `From` conversions

`Value` implements `From` for every common scalar, so you can build trees by
hand with `.into()`:

| Source types | Produces |
| --- | --- |
| `i8, i16, i32, i64, isize` | `Value::Int` |
| `u8, u16, u32, u64, usize` | `Value::UInt` |
| `f32, f64` | `Value::Float` |
| `bool` | `Value::Bool` |
| `String`, `&str` | `Value::Str` |
| `Vec<Value>` | `Value::Array` |
| `Option<T: Into<Value>>` | inner value, or `Value::Null` for `None` |

```rust
use ferroly::codec::{json, Value};

let obj = Value::Object(vec![
    ("port".to_string(), 8080u16.into()),
    ("debug".to_string(), true.into()),
    ("name".to_string(), "api".into()),
    ("tags".to_string(), Value::Array(vec!["x".into(), "y".into()])),
    ("note".to_string(), Option::<&str>::None.into()),  // -> Null
]);
assert_eq!(json::to_string(&obj),
    r#"{"port":8080,"debug":true,"name":"api","tags":["x","y"],"note":null}"#);
```

Note there is **no** `From` for `Bytes` or `HashMap`; construct those `Value`
arms directly, or go through the `Encode` derive.

## The `Encode` / `Decode` traits and std impls

`Encode::encode` and `Decode::decode` are the `Value`-based path; the streaming
`encode_to` / `decode_from` are the [fast path](#streaming-fast-path). Concrete
impls provide both.

Standard-library coverage out of the box:

- **Primitives:** `bool`, all signed ints (`i8`..`i64`, `isize`), all unsigned
  ints (`u8`..`u64`, `usize`), `f32`, `f64`, `char`, `String`, `str`, `&str`.
- **Containers:** `Option<T>`, `Vec<T>`, `Box<T>`, `HashMap<String, V>`.
- **Unit:** `()` encodes to `Null`.
- **`Value` itself** implements both traits (a clone), so you can mix dynamic and
  static data.

Decode impls are deliberately **lenient**, matching real-world config and
loosely-typed inputs:

- Numeric decoders accept a numeric `Value` *or* a numeric string (`"42"`), and
  range-check the target width — an over-wide value yields
  [`OutOfRange`](#error-handling).
- `bool` decodes from `Value::Bool` or the strings `"true"` / `"false"`.
- Floats accept ints and vice versa where lossless-enough for the target.
- `char` requires a single-character string.

```rust
use ferroly::codec::{Decode, Value};

// Lenient: a numeric string decodes into an integer.
let n = i32::decode(&Value::Str("  42 ".into())).unwrap();
assert_eq!(n, 42);

// Range-checked: 300 does not fit u8.
assert!(u8::decode(&Value::Int(300)).is_err());

// Option<T>: Null -> None, anything else -> Some.
let some = Option::<String>::decode(&Value::Str("hi".into())).unwrap();
assert_eq!(some, Some("hi".to_string()));
```

## The `#[derive(Encode, Decode)]` macros

The derives generate **both** the `Value` path and the streaming path, so a
derived struct is fast on JSON and still works with XML/YAML and the dynamic
`Value` API. They are re-exported from the `codec` module, so
`use ferroly::codec::{Encode, Decode};` brings both the trait and the derive
into scope with a single import.

Supported shapes:

- **Structs with named fields.**
- **Enums with unit variants only** (they map to a string — see below).

Attributes:

| Attribute | Level | Effect |
| --- | --- | --- |
| `#[ferroly(rename_all = "...")]` | container | rename every field/variant by a case rule |
| `#[ferroly(rename = "...")]` | field / variant | override one wire key |
| `#[ferroly(skip_none)]` | field | omit the field entirely when it is `None` |

`rename_all` rules: `lowercase`, `UPPERCASE`, `snake_case`,
`SCREAMING_SNAKE_CASE`, `kebab-case`, `camelCase`, `PascalCase`.

```rust
use ferroly::codec::{json, Decode, Encode};

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(rename_all = "camelCase")]
struct User {
    user_name: String,           // -> "userName"
    #[ferroly(rename = "id")]
    identifier: u64,             // explicit override wins over rename_all
    #[ferroly(skip_none)]
    nickname: Option<String>,    // omitted from output when None
}

let u = User { user_name: "ada".into(), identifier: 7, nickname: None };
assert_eq!(json::encode(&u), r#"{"userName":"ada","id":7}"#);

// On decode, an absent Option field becomes None; an absent non-Option field
// is a MissingField error. Unknown keys are skipped.
let back: User = json::decode(r#"{"userName":"ada","id":7,"extra":1}"#).unwrap();
assert_eq!(back, u);
```

Unit-only enums encode to their (renamed) name as a string and decode back,
erroring with [`UnknownVariant`](#error-handling) on an unrecognized string:

```rust
use ferroly::codec::{json, Decode, Encode};

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(rename_all = "lowercase")]
enum Level { Debug, Info, Warn, Error }

assert_eq!(json::encode(&Level::Warn), r#""warn""#);
assert_eq!(json::decode::<Level>(r#""info""#).unwrap(), Level::Info);
assert!(json::decode::<Level>(r#""nope""#).is_err());
```

For error enums, see the [`FerrolyError`](derive.md) derive; full derive
internals are documented on the [derive page](derive.md).

## JSON codec

`ferroly::codec::json` is a hand-written, full-fidelity JSON parser and compact
encoder. It is the only codec with a **streaming fast-path** on both sides.

- `encode` / `encode_to_vec` stream a value straight to the output — no
  intermediate `Value`. Scalars are appended in place via allocation-free
  writers, and the encoder tracks a per-container "first entry" flag to place
  separators.
- `decode` / `decode_from_slice` pull tokens on demand from the parser, building
  the target type field by field. Object keys with no escapes are borrowed
  directly (no per-key `String`).
- `to_string` / `from_str` / `from_slice` operate on `Value` directly for
  dynamic use.

Fidelity details:

- Numbers parse to `Int`, then `UInt` (above `i64::MAX`), then `Float`; anything
  with `.`, `e`/`E`, or a sign in the exponent parses as `Float`.
- Integer-valued floats encode with a trailing `.0` so they round-trip as floats.
- Non-finite floats (`NaN`, `±inf`) encode as `null` (JSON has no representation).
- Strings handle the full escape set including `\uXXXX` and UTF-16 surrogate
  pairs; clean runs are bulk-copied.
- `Value::Bytes` encodes as a JSON array of byte integers (`[104,105]`).
- Trailing characters after a complete value are a parse error.

```rust
use ferroly::codec::{json, Decode, Encode};

#[derive(Encode, Decode, PartialEq, Debug)]
struct Metric { name: String, value: f64, tags: Vec<String> }

let m = Metric { name: "cpu".into(), value: 250.0, tags: vec!["host-a".into()] };
let s = json::encode(&m);
assert_eq!(s, r#"{"name":"cpu","value":250.0,"tags":["host-a"]}"#);

// Bytes round-trip.
let bytes = json::encode_to_vec(&m);
let back: Metric = json::decode_from_slice(&bytes).unwrap();
assert_eq!(back, m);
```

## XML codec

`ferroly::codec::xml` is a pragmatic, dependency-free XML codec. XML has no
canonical mapping to a generic model, so it uses a fixed struct-oriented
convention:

- An object becomes child elements: `{name: "x"}` → `<name>x</name>`.
- An array becomes repeated sibling elements sharing the tag:
  `{tags: ["a","b"]}` → `<tags>a</tags><tags>b</tags>`.
- Scalars become escaped text (`<`, `>`, `&`); `null` becomes an empty element.
- The whole document is wrapped in a `<root>` element.
- On decode, repeated sibling tags collapse back into an array; the XML
  declaration (`<?xml …?>`) and comments are skipped, and the standard entities
  (`&lt; &gt; &amp; &quot; &apos;` plus numeric `&#…;`/`&#x…;`) are decoded.
- **0- and 1-element arrays round-trip.** An empty `Vec` encodes as an empty
  element (`<tags></tags>`) and decodes back to an empty vec; a single-element
  `Vec` encodes as one `<tags>x</tags>` and is reconstructed as a one-element vec
  by the typed `Vec` decode (which accepts a lone value or `null` for a field
  known to be a sequence). JSON's normal decode path stays strict — this
  tolerance only applies to the `Value`-tree decode that XML/YAML use.

What XML does **not** model: attributes, namespaces, mixed content, and
top-level arrays. Decoded scalars are always strings — the numeric/bool `Decode`
impls parse them on the way into your struct. Use JSON where full fidelity
matters.

```rust
use ferroly::codec::{xml, Decode, Encode};

#[derive(Encode, Decode, PartialEq, Debug)]
struct Config { name: String, port: u16, tags: Vec<String> }

let cfg = Config { name: "api".into(), port: 8080, tags: vec!["a".into(), "b".into()] };
let doc = xml::encode(&cfg);
assert!(doc.contains("<name>api</name>"));
assert!(doc.contains("<port>8080</port>"));
assert!(doc.contains("<tags>a</tags><tags>b</tags>"));

let back: Config = xml::decode(&doc).unwrap();
assert_eq!(back, cfg);
```

## YAML codec

`ferroly::codec::yaml` covers the common block-style YAML subset — enough for
hand-written config.

Supported: block mappings (`key: value`), block sequences (`- item`), nesting by
indentation, scalars (strings, ints, floats, bools, null), and single/double
quoted strings. The encoder always emits block style, and quotes any scalar that
would otherwise read back as a non-string (e.g. `"true"`, `"42"`) or that starts
with a YAML-significant character.

Not supported (use JSON): anchors/aliases, tags, flow style (`{a: 1}` / `[1, 2]`
except the empty forms `{}` / `[]`), multi-document streams, and block scalars
(`|` / `>`). An inline mapping item in a sequence (`- key: value`) parses only
its first key. Comments (`#`) and `---` separators are ignored on input.

```rust
use ferroly::codec::{yaml, Decode, Encode};

#[derive(Encode, Decode, PartialEq, Debug)]
struct Server { name: String, port: u16, tags: Vec<String> }

// Parse hand-written YAML.
let text = "name: service\nport: 9000\ntags:\n  - x\n  - y\n";
let s: Server = yaml::decode(text).unwrap();
assert_eq!(s.port, 9000);
assert_eq!(s.tags, vec!["x".to_string(), "y".to_string()]);

// Round-trip through block style.
let out = yaml::encode(&s);
let back: Server = yaml::decode(&out).unwrap();
assert_eq!(back, s);
```

## TOML codec

`ferroly::codec::toml` covers the common TOML config subset. The encoder emits a
nested struct as a `[section]` header and a `Vec`-of-structs as `[[section]]`
arrays of tables; scalar keys are written before child tables, as TOML requires.

Supported: bare / quoted / dotted keys, `key = value` pairs, table headers
(`[a.b]`), arrays of tables (`[[a.b]]`), inline tables (`{ a = 1 }`), arrays
(which may span lines and end with a trailing comma), basic (`"…"`) and literal
(`'…'`) strings with the standard escapes, integers (decimal with `_` separators
and `0x` / `0o` / `0b` prefixes), floats (including `inf` / `-inf` / `nan`),
booleans, and `#` comments.

Not supported (use JSON): multi-line strings (`"""` / `'''`) and native
date-times — a date-time token decodes as its raw `Value::Str`. TOML has no null
type, so `Value::Null` (a `None` field) is omitted on encode; a missing key
decodes back to `None` for `Option` fields.

```rust
use ferroly::codec::{toml, Decode, Encode};

#[derive(Encode, Decode, PartialEq, Debug)]
struct Db { host: String, port: u16 }

#[derive(Encode, Decode, PartialEq, Debug)]
struct Config { name: String, db: Db }

// Parse hand-written TOML with a [section] header.
let text = "name = \"api\"\n\n[db]\nhost = \"localhost\"\nport = 5432\n";
let cfg: Config = toml::decode(text).unwrap();
assert_eq!(cfg.db.port, 5432);

// Round-trip.
let out = toml::encode(&cfg);
let back: Config = toml::decode(&out).unwrap();
assert_eq!(back, cfg);
```

## Content-type dispatch

When the format is decided at runtime (an HTTP `Content-Type` header, a config
file extension), use the dispatch free functions. The format set is the closed
[`Format`](#content-type-dispatch) enum, matched directly — no registry state.

`Format::from_content_type` ignores case and any `;charset=…` parameter, and
recognizes structured-syntax suffixes:

| Content type | Format |
| --- | --- |
| `application/json`, `text/json`, `*+json` | `Json` |
| `application/xml`, `text/xml`, `*+xml` | `Xml` |
| `application/yaml`, `text/yaml`, `application/x-yaml`, `text/x-yaml`, `application/yml`, `text/yml`, `*+yaml` | `Yaml` |
| `application/toml`, `text/toml`, `application/x-toml`, `*+toml` | `Toml` |
| anything else | `None` |

```rust
use ferroly::codec::{decode, encode, resolve, Decode, Encode, Format};

#[derive(Encode, Decode, PartialEq, Debug)]
struct Payload { id: u32, ok: bool }

let p = Payload { id: 1, ok: true };

// Encode by content type (e.g. echoing the request's Accept header).
let bytes = encode("application/vnd.api+json", &p).unwrap();
let back: Payload = decode("application/json; charset=utf-8", &bytes).unwrap();
assert_eq!(p, back);

// Resolve is exposed if you only need the format.
assert_eq!(resolve("text/xml").unwrap(), Format::Xml);
assert!(resolve("application/octet-stream").is_err());
```

## Streaming fast-path

The default trait methods `Encode::encode` and `Decode::decode` go through the
`Value` tree. The fast-path methods — `encode_to<E: Encoder>` and
`decode_from<D: Decoder>` — skip it. The JSON codec implements `Encoder` and
`Decoder`, so `json::encode` / `json::decode` on a derived type never build a
`Value` at all: fields are written to / read from the byte stream directly. This
is what keeps JSON encoding **fast**, with decode at parity.

- `Encoder` is a push-based output sink that owns all punctuation and separators
  (`begin_map` / `map_entry` / `end_map`, `begin_seq` / `seq_entry` / `end_seq`,
  and the scalar `encode_*` methods).
- `Decoder` is a pull-based source: `read_map` / `read_seq` drive a callback per
  entry, `decode_string` pulls a string with one allocation, `peek_null` /
  `decode_null` handle optionals, and `skip_value` discards unknown fields.
- `emit_value(e, &value)` is the bridge: it walks any `Value` into any `Encoder`.
  It is both the fallback for `encode_to`'s default and the implementation of
  `Encode for Value`, so a dynamic `Value` still gets the streaming output path.

The XML and YAML codecs go through `Value` (they call `value.encode()` and
`T::decode(&parsed)`); only JSON has the streaming path on both sides.

Most code never touches these traits directly — the derive emits the fast path
for you. You implement `Encoder` / `Decoder` only to add a *new streaming
format*, and override `encode_to` / `decode_from` only for a hand-written type
that wants to skip the `Value` tree:

```rust
use ferroly::codec::{CodecError, Decode, Decoder, Encode, Encoder, Value};

struct Pair(i64, i64);

impl Encode for Pair {
    fn encode(&self) -> Value {
        Value::Array(vec![Value::Int(self.0), Value::Int(self.1)])
    }
    fn encode_to<E: Encoder>(&self, e: &mut E) {
        e.begin_seq(2);
        e.seq_entry(&self.0);
        e.seq_entry(&self.1);
        e.end_seq();
    }
}

impl Decode for Pair {
    fn decode(v: &Value) -> Result<Self, CodecError> {
        let a = v.get_index(0).and_then(Value::as_i64).ok_or_else(|| CodecError::expected("pair"))?;
        let b = v.get_index(1).and_then(Value::as_i64).ok_or_else(|| CodecError::expected("pair"))?;
        Ok(Pair(a, b))
    }
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> {
        let mut nums = Vec::new();
        d.read_seq(|d| { nums.push(i64::decode_from(d)?); Ok(()) })?;
        Ok(Pair(nums[0], nums[1]))
    }
}
```

## Error handling

Every fallible codec operation returns `Result<_, CodecError>`. `CodecError`
implements `std::error::Error` and `Display`, and derives `Clone`, `PartialEq`,
`Eq`, and `Debug`.

| Variant | Constructor | Raised when |
| --- | --- | --- |
| `UnsupportedContentType(String)` | — | `resolve` / dispatch got an unknown MIME type |
| `Parse(String)` | `CodecError::parse(msg)` | syntax error while parsing JSON/XML/YAML |
| `Expected(&'static str)` | `CodecError::expected(what)` | a `Value` of the wrong shape (e.g. object expected) |
| `MissingField(String)` | `CodecError::missing_field(name)` | a required (non-`Option`) struct field was absent |
| `UnknownVariant(String)` | `CodecError::unknown_variant(name)` | an enum string matched no variant |
| `OutOfRange(&'static str)` | `CodecError::out_of_range(ty)` | a number did not fit the target integer type |
| `Message(String)` | — | a general message |

```rust
use ferroly::codec::{json, CodecError, Decode};

let err = json::decode::<i32>("{").unwrap_err();
assert!(matches!(err, CodecError::Parse(_)));
println!("{err}"); // "parse error: ..."

#[derive(Decode)]
struct Need { id: u32 }
let err = json::decode::<Need>("{}").unwrap_err();
assert!(matches!(err, CodecError::MissingField(_)));
```

Schema validation ([schema.md](schema.md)) uses a different, collect-all error
type ([`Violation`](schema.md)) rather than `CodecError`.

## Limitations

- **JSON** is the only full-fidelity, streaming codec. XML, YAML, and TOML target
  the common struct/config subset and go through `Value`.
- **XML**: no attributes, namespaces, mixed content, or top-level arrays;
  decoded scalars are always strings.
- **YAML**: block style only — no flow collections (beyond empty `{}`/`[]`),
  anchors/aliases, tags, block scalars, or multi-document streams.
- **TOML**: no multi-line strings (`"""`/`'''`) or native date-times (a
  date-time decodes as a `Str`); a top-level value must be a table (struct), and
  `None`/`Null` fields are omitted rather than represented.
- The `#[derive]` supports **named-field structs** and **unit-only enums** only;
  tuple structs, newtypes, and data-carrying enum variants are not derivable
  (implement the traits by hand, as in [Streaming fast-path](#streaming-fast-path)).
- `Value::Object` is a `Vec`, so key lookup (`get`) is linear — ideal for the
  small objects typical of config and API payloads, not for huge maps.
- **Duplicate object keys resolve last-wins** — `Value::get` and struct decoding
  both return the last occurrence (JSON's conventional semantics).
- **Non-finite floats** (`NaN`/`±Inf`) cannot be represented and encode as `null`.
- **`Value::Bytes` does not round-trip through the text codecs** — it encodes as an
  array of integers (JSON/YAML) or space-separated numbers (XML). Base64-encode
  binary data into a `Str` yourself if you need fidelity.

## See also

- [derive](derive.md) — internals of `Encode` / `Decode` / `FerrolyError`.
- [schema](schema.md) — validate a `Value` against a JSON-Schema subset.
- [config](config.md) — layered configuration built on the codecs.
- [genai](genai.md) — provider payloads encoded through this codec.
- [errutils](errutils.md) — error utilities complementing `CodecError`.

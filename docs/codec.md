# Codec

**Feature:** `codec` (default) · **Module:** `ferroly::codec`

The in-house, dependency-free encoding toolkit. It
provides a format-agnostic data model ([`Value`](#the-value-model)), the
[`Encode`](#the-encode-and-decode-traits) / [`Decode`](#the-encode-and-decode-traits)
traits with derives, hand-written JSON / XML / YAML / TOML codecs, a
content-type dispatch layer, a shared [parser budget](#limits-and-untrusted-input),
and an optional [JSON-Schema subset validator](schema.md).

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
  [`Decoder`](#streaming-fast-path)). Both have impls for the standard library
  and a `#[derive]`.
- **Strict and lenient decoding** — typed formats (JSON, TOML) never coerce a
  string into a number or bool; text-only sources (XML, environment variables,
  `.properties`) do. See [Strict and lenient decoding](#strict-and-lenient-decoding).
- **Streaming fast-path** — JSON has a second path that bypasses the `Value`
  tree entirely, writing/reading field by field.
- **One parser budget** — all four parsers enforce the same
  [`Limits`](#limits-and-untrusted-input) (nesting depth, input size, entry
  count) and report malformed input as an error with a line and column, never a
  panic.
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

Accessors:

| Method | Returns | Notes |
| --- | --- | --- |
| `as_object()` | `Option<&Vec<(String, Value)>>` | object entries |
| `as_object_mut()` | `Option<&mut Vec<(String, Value)>>` | object entries, mutable |
| `as_array()` | `Option<&Vec<Value>>` | array elements |
| `as_array_mut()` | `Option<&mut Vec<Value>>` | array elements, mutable |
| `as_str()` | `Option<&str>` | string only |
| `as_bool()` | `Option<bool>` | bool only |
| `as_i64()` | `Option<i64>` | `Int`, or `UInt` that fits |
| `as_u64()` | `Option<u64>` | `UInt`, or non-negative `Int` |
| `as_f64()` | `Option<f64>` | `Float`, `Int`, or `UInt` |
| `get(key)` | `Option<&Value>` | object lookup by key (last duplicate wins) |
| `get_mut(key)` | `Option<&mut Value>` | mutable object lookup (last duplicate wins) |
| `get_index(i)` | `Option<&Value>` | array index |
| `get_index_mut(i)` | `Option<&mut Value>` | mutable array index |
| `pointer(ptr)` | `Option<&Value>` | nested lookup by JSON Pointer (RFC 6901), e.g. `"/a/b/0"` |
| `pointer_mut(ptr)` | `Option<&mut Value>` | mutable form of `pointer` |
| `merge(other)` | `()` | deep-merge `other` into `self` |
| `take()` | `Value` | replace with `Null`, returning the original |
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
    fn decode(value: &Value) -> Result<Self, CodecError>;                 // lenient
    fn decode_from<D: Decoder>(d: &mut D) -> Result<Self, CodecError> { /* default */ }  // strict
}
```

| Method | Description |
| --- | --- |
| `Encode::encode(&self) -> Value` | Produce the `Value` representation of `self`. |
| `Encode::encode_to<E: Encoder>(&self, e)` | Stream `self` straight to an `Encoder` (default replays `encode()`; impls override). |
| `Decode::decode(value: &Value) -> Result<Self, _>` | Build `Self` from a `Value` — the **lenient** path. |
| `Decode::decode_from<D: Decoder>(d) -> Result<Self, _>` | Build `Self` straight from a `Decoder` — the **strict** path (default pulls a `Value` and calls `decode`; the std impls and the derive override it). |

The streaming sink and source traits, the value-walker, and the strict
`Value` decoder:

```rust
pub trait Encoder: Sized { /* scalar + container emit methods, see below */ }
pub trait Decoder: Sized { /* pull-based read methods, see below */ }
pub fn emit_value<E: Encoder>(e: &mut E, value: &Value);

pub struct ValueDecoder<'a> { /* a strict Decoder over a &Value */ }
impl<'a> ValueDecoder<'a> { pub fn new(value: &'a Value) -> Self; }
pub fn from_value_strict<T: Decode>(value: &Value) -> Result<T, CodecError>;
```

`Encoder` is the push-based output sink a streaming format implements:

| Method | Description |
| --- | --- |
| `encode_null(&mut self)` | Emit a `null`. |
| `encode_bool(&mut self, v: bool)` | Emit a boolean. |
| `encode_i64(&mut self, v: i64)` | Emit a signed integer. |
| `encode_u64(&mut self, v: u64)` | Emit an unsigned integer. |
| `encode_f64(&mut self, v: f64)` | Emit a floating-point number. |
| `encode_f32(&mut self, v: f32)` | Emit a single-precision float. Defaulted: widens to `f64` and calls `encode_f64`, so existing encoders are unchanged. |
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
| `decode_i128` / `decode_u128(&mut self)` | Pull a 128-bit integer, strictly. Defaulted (through `decode_value`); the JSON parser overrides them to read a wide integer literal from its digits, exactly. |

`emit_value(e, &value)` walks any `Value` into any `Encoder` — the fallback for
`encode_to`'s default and the implementation of `Encode for Value`.

### Format submodules

Each of [`json`](#json-codec), [`xml`](#xml-codec), [`yaml`](#yaml-codec), and
[`toml`](#toml-codec) exposes the same shape:

```rust
pub fn encode<T: Encode + ?Sized>(value: &T) -> String;
pub fn encode_to_vec<T: Encode + ?Sized>(value: &T) -> Vec<u8>;
pub fn decode<T: Decode>(input: &str) -> Result<T, CodecError>;
pub fn decode_from_slice<T: Decode>(input: &[u8]) -> Result<T, CodecError>;
pub fn from_str(input: &str) -> Result<Value, CodecError>;

// The same, with an explicit parser budget:
pub fn decode_with<T: Decode>(input: &str, limits: Limits) -> Result<T, CodecError>;
pub fn decode_from_slice_with<T: Decode>(input: &[u8], limits: Limits) -> Result<T, CodecError>;
pub fn from_str_with(input: &str, limits: Limits) -> Result<Value, CodecError>;
```

Extras per format:

| Function | Formats | Purpose |
| --- | --- | --- |
| `try_encode(&T) -> Result<String, CodecError>` | `json`, `xml`, `toml` | fail instead of writing a fallback for a value the format cannot represent |
| `encode_pretty(&T) -> String` | `json`, `xml` | indented output (YAML and TOML are always one entry per line) |
| `to_string(&Value)` / `to_string_pretty(&Value)` | `json` | render a `Value` compactly / indented |
| `from_slice(&[u8])` / `from_slice_with(&[u8], Limits)` | `json` | parse bytes into a `Value` |

For rendering a `Value` to XML/YAML/TOML, call `encode(&value)` (a `Value` is
itself `Encode`).

All four parsers skip a leading UTF-8 byte-order mark.

### Content-type dispatch

```rust
pub enum Format { Json, Xml, Yaml, Toml }
impl Format {
    pub fn from_content_type(content_type: &str) -> Option<Format>;
    /// Decode T from a Value this format parsed, with the format's rules
    /// (strict for JSON/TOML, lenient for XML/YAML).
    pub fn decode_value<T: Decode>(self, value: &Value) -> Result<T, CodecError>;
}

pub fn resolve(content_type: &str) -> Result<Format, CodecError>;
pub fn encode<T: Encode + ?Sized>(content_type: &str, value: &T) -> Result<Vec<u8>, CodecError>;
pub fn try_encode<T: Encode + ?Sized>(content_type: &str, value: &T) -> Result<Vec<u8>, CodecError>;
pub fn decode<T: Decode>(content_type: &str, bytes: &[u8]) -> Result<T, CodecError>;
pub fn decode_with<T: Decode>(content_type: &str, bytes: &[u8], limits: Limits) -> Result<T, CodecError>;
```

### Limits

```rust
#[non_exhaustive]
pub struct Limits {
    pub max_depth: usize,        // default 128
    pub max_input_bytes: usize,  // default 64 MiB
    pub max_entries: usize,      // default 10_000_000
    pub reject_duplicate_keys: bool, // default false
}
impl Limits {
    pub const DEFAULT: Limits;
    pub const DEFAULT_MAX_DEPTH: usize;
    pub const DEFAULT_MAX_INPUT_BYTES: usize;
    pub const DEFAULT_MAX_ENTRIES: usize;
    pub const fn new() -> Self;                       // == Limits::default()
    pub fn with_max_depth(self, n: usize) -> Self;
    pub fn with_max_input_bytes(self, n: usize) -> Self;
    pub fn with_max_entries(self, n: usize) -> Self;
    pub fn with_reject_duplicate_keys(self, reject: bool) -> Self;
}
```

See [Limits and untrusted input](#limits-and-untrusted-input).

### Errors

```rust
#[non_exhaustive]
pub enum CodecError {
    UnsupportedContentType(String),
    Parse(String),
    Expected(&'static str),
    MissingField(String),
    UnknownVariant(String),
    OutOfRange(&'static str),
    Message(String),
    LimitExceeded(String),
    Unrepresentable(String),
    TypeMismatch { expected: &'static str, found: &'static str },
    At { path: Vec<PathSegment>, source: Box<CodecError> },
}

pub enum PathSegment { Key(String), Index(usize) }

impl CodecError {
    pub fn expected(what: &'static str) -> Self;
    pub fn type_mismatch(expected: &'static str, found: &Value) -> Self;
    pub fn in_field(self, key: &str) -> Self;
    pub fn in_index(self, index: usize) -> Self;
    pub fn path(&self) -> &[PathSegment];
    pub fn path_string(&self) -> String;
    pub fn root_cause(&self) -> &CodecError;
}
```

See [Error handling](#error-handling) and [Field paths](#field-paths).

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

// Or address a nested value directly with a JSON Pointer.
assert_eq!(v.pointer("/user/roles/0").and_then(Value::as_str), Some("admin"));
```

### Path lookup, in-place edits, and merging

`pointer` / `pointer_mut` take an RFC 6901 JSON Pointer: `""` is the value
itself; otherwise a sequence of `/`-prefixed tokens, each an object key or an
array index. Inside a token, `~1` stands for `/` and `~0` for `~`. An index must
be plain decimal digits with no leading zero.

`merge` deep-merges another value in: when both sides are objects their members
are merged key by key (recursively) and keys only in the overlay are appended in
order; in every other case — scalars, arrays, mismatched kinds, and `Null` — the
overlay replaces the base.

```rust
use ferroly::codec::{json, Value};

let mut cfg = json::from_str(r#"{"db":{"host":"a","port":1},"tags":[1]}"#).unwrap();

// Edit in place.
*cfg.pointer_mut("/db/port").unwrap() = Value::Int(5432);
cfg.get_mut("tags").unwrap().as_array_mut().unwrap().push(Value::Int(2));

// Layer an overlay on top.
cfg.merge(json::from_str(r#"{"db":{"tls":true},"tags":[9]}"#).unwrap());
assert_eq!(
    json::to_string(&cfg),
    r#"{"db":{"host":"a","port":5432,"tls":true},"tags":[9]}"#
);
```

### `From` conversions

`Value` implements `From` for every common scalar, so you can build trees by
hand with `.into()`:

| Source types | Produces |
| --- | --- |
| `i8, i16, i32, i64, isize` | `Value::Int` |
| `u8, u16, u32, u64, usize` | `Value::UInt` |
| `f32, f64` | `Value::Float` (an `f32` via its shortest text, so `0.1f32` gives `Float(0.1)`; see [Floats](#floats)) |
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

| Types | Wire form |
| --- | --- |
| `bool` | boolean |
| `i8`..`i64`, `isize`, `u8`..`u64`, `usize` | integer |
| `i128`, `u128` | integer when it fits 64 bits, otherwise a decimal string; `json::decode` also reads a wider integer literal exactly (see [below](#strict-and-lenient-decoding)) |
| `f32`, `f64` | number; an `f32` as its own shortest text (see [Floats](#floats)) |
| `char`, `String`, `str` | string (`char` requires exactly one character) |
| `&T` (so `&str`, `&[T]`, `&Struct`) | as `T` (encode only) |
| `Option<T>` | `null` or `T` |
| `Vec<T>`, `[T]` (encode only), `[T; N]` | sequence (`[T; N]` requires exactly `N` elements) |
| `(A,)` … 12-tuples | fixed-length sequence |
| `HashSet<T>`, `BTreeSet<T>` | sequence (duplicates collapse on decode) |
| `HashMap<String, V>`, `BTreeMap<String, V>` | object (`BTreeMap` in key order) |
| `Box<T>`, `Rc<T>`, `Arc<T>` | as `T`; `Box<str>` / `Rc<str>` / `Arc<str>` as strings |
| `Cow<'_, T>` (e.g. `Cow<str>`, `Cow<[T]>`) | as `T`; decodes to `Cow::Owned` |
| `std::time::Duration` | `{"secs": u64, "nanos": u32}`; also decodes from a bare number of seconds |
| `std::time::SystemTime` | RFC 3339 string in UTC (`"2026-11-15T08:30:00.5Z"`); decodes from any RFC 3339 date-time, a `{"secs": i64, "nanos": u32}` object or a number of seconds since the epoch. Outside years 0000–9999 it encodes as that object |
| `codec::DateTime` | RFC 3339 string keeping its UTC offset (`"2026-11-15T09:30:00+01:00"`) |
| `PathBuf`, `Path` (encode only) | string (a non-UTF-8 path is encoded lossily) |
| `IpAddr`, `Ipv4Addr`, `Ipv6Addr`, `SocketAddr`, `SocketAddrV4`, `SocketAddrV6` | string in the standard text form |
| `()` | `null` |
| `Value` | itself (a clone) |

Not covered: other date/time types (dates without a time, local times without
an offset, time zones), non-string map keys, and `NonZero*` integers. Byte
slices encode as arrays of integers, not base64.

### Date-times

`codec::DateTime` is an RFC 3339 date-time: an instant with nanosecond
precision plus the UTC offset it was written with. It parses `T`, `t` or a
space between date and time, `Z`/`z` or `±hh:mm` (`-00:00` reads as UTC) and
any number of fraction digits (beyond nine they are truncated); a leap second
`:60` reads as the next minute's first instant. It writes `T`, `Z` for a zero
offset, and only the fraction digits needed. Its local date lies in years
0000–9999, so every value round-trips through its own string: `with_offset`
and `to_utc` return `None` when the shifted date would leave that range (only
within a day of either end), and `to_system_time` returns `None` when the
platform's `SystemTime` cannot hold the instant (WASI has no times before
1970). Equality compares instant and offset; use `same_instant` to ignore the
offset.

```rust
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use ferroly::codec::{json, DateTime};

let at = DateTime::parse("2026-11-15T09:30:00.25+01:00").unwrap();
assert_eq!((at.year(), at.month(), at.day(), at.hour()), (2026, 11, 15, 9));
assert_eq!(at.to_utc().unwrap().to_string(), "2026-11-15T08:30:00.25Z");
assert_eq!(at.to_system_time(), Some(UNIX_EPOCH + Duration::new(1_794_731_400, 250_000_000)));

let t: SystemTime = json::decode(r#""2026-11-15T09:30:00+01:00""#).unwrap();
assert_eq!(json::encode(&t), r#""2026-11-15T08:30:00Z""#);
```

TOML date-time values arrive as strings, so a `DateTime` or `SystemTime` field
decodes from TOML too (and is validated there).

### Floats

Every float is written as the **shortest decimal that reads back as the same
value of its own type**, in plain notation (no exponent), with a trailing `.0`
on integer-valued floats so they stay floats. For an `f32` that is its own
shortest form, not that of its `f64` widening:

```rust
use ferroly::codec::{json, toml, yaml, Encode};

assert_eq!(json::encode(&0.1f32), "0.1"); // not 0.10000000149011612
assert_eq!(json::encode(&1.0e-7f32), "0.0000001");
assert_eq!(json::encode(&250.0f32), "250.0");
assert_eq!(json::encode(&0.1f64), "0.1");
assert_eq!(json::encode(&f64::from(0.1f32)), "0.10000000149011612");

#[derive(Encode)]
struct Embedding { embedding: Vec<f32> }
let e = Embedding { embedding: vec![0.1, -0.020566292] };
assert_eq!(json::encode(&e), r#"{"embedding":[0.1,-0.020566292]}"#);
assert_eq!(yaml::encode(&e), "embedding:\n  - 0.1\n  - -0.020566292\n");
assert_eq!(toml::encode(&e), "embedding = [0.1, -0.020566292]\n");
```

The JSON encoder writes an `f32` through `Encoder::encode_f32`. On the `Value`
path (`0.1f32.encode()`, which YAML, TOML and XML use) an `f32` becomes the
`f64` nearest its shortest text, `Value::Float(0.1)`, so those formats print
the short form too; converting that `f64` back with `as f32` gives the original
value, and every finite `f32` reads back bit-for-bit (the test suite checks
all 2^32 bit patterns). Decoders read numbers as `f64` and narrow them, so
both paths read their text back that way before keeping it, and a value whose
shortest text would misread falls back to the digits of its exact `f64`
widening, as before. On current toolchains exactly one magnitude does,
`7.038531e-26`. Decoding is unchanged: `0.10000000149011612` and `0.1`
decode to the same `f32`. Non-finite `f32` values follow the same per-format
rules as `f64`.

The short form has a cost: each `f32` is formatted and then parsed back. On
an M-series machine (release build) that is roughly 75 ns per value on both
paths: `json::encode` of a million embedding-style `f32`s takes about 73 ms
(the same values widened to `f64` take about 137 ms, because their text is
longer), and `f32::encode()` / `Value::from(f32)` take about 74 ms per million
where an `f64` is a plain copy (under 1 ms). The `Value` cost is what YAML,
TOML and XML pay for each `f32` they encode, and what any code that builds
`Value`s from `f32`s pays. Two consequences of the conversion:
`Value::from(x)` is not `Value::Float(f64::from(x))`, and `as_f64()` on it
returns the shortest decimal, which can differ from the exact `f32` by up to
half an `f32` unit in the last place.

### Strict and lenient decoding

The scalar impls decode in one of two modes. The **entry point** picks the mode,
not the target type:

| Mode | Path | Used by | A string where a number/bool is expected |
| --- | --- | --- | --- |
| **Strict** | `Decode::decode_from` (a `Decoder`) | `json::decode`, `toml::decode`, `from_value_strict`, `ValueDecoder` | error |
| **Lenient** | `Decode::decode` (a `&Value`) | `xml::decode`, `yaml::decode`, the `config` module | parsed (`"42"`, `" true "`, `"1.5"`) |

Leniency exists because text-only sources have no other way to carry a scalar:
XML content, environment variables, and `.properties` files deliver everything
as a string. Lenient mode also lets `null` stand in for an empty `Vec` and a
lone value for a one-element `Vec` — which is how XML's repeated-sibling
convention reconstructs short arrays. Strict mode requires a real array.

Both modes share these rules:

- Integers are range-checked for the target width; an over-wide value is
  [`OutOfRange`](#error-handling).
- A float decodes into an integer type **only** when it is a whole number that
  fits: `3.0` → `3`, but `1.9` is `Expected("integer")` and `1e300` is
  `OutOfRange`. Nothing is truncated or saturated. A negative integer for an
  unsigned type is `Expected("unsigned integer")`.
- An integer literal wider than 64 bits is already a (rounded) float in a
  `Value`, so `i128` / `u128` reject a float at or beyond 2^53 in magnitude
  rather than decode a possibly wrong number. The streaming `json::decode`
  reads such a literal from its digits and decodes it exactly
  (`json::decode::<i128>("-9223372036854775809")` is that number); through a
  `Value` (`json::from_str`, YAML, `Format::decode_value`, turbo's
  `ctx.read_valid`, and inside an untagged enum or a `flatten` field, which
  buffer their content as a `Value`) send the value as a decimal string, which
  is also how `i128` / `u128` encode it.
- An integer decodes into a float type.
- Leniency is about strings only: a bool is never a number, a number is never a
  string.

```rust
use ferroly::codec::{from_value_strict, json, Decode, Value};

// Lenient: a numeric string decodes into an integer (whitespace trimmed).
assert_eq!(i32::decode(&Value::Str("  42 ".into())).unwrap(), 42);

// Strict: the same value is rejected — by JSON, by TOML, and by an explicit
// strict walk over a Value.
assert!(json::decode::<i32>(r#"" 42 ""#).is_err());
assert!(from_value_strict::<i32>(&Value::Str("42".into())).is_err());
assert_eq!(from_value_strict::<i32>(&Value::Int(42)).unwrap(), 42);

// Range-checked: 300 does not fit u8.
assert!(u8::decode(&Value::Int(300)).is_err());

// Float -> integer only when exact.
assert_eq!(json::decode::<i64>("3.0").unwrap(), 3);
assert!(json::decode::<i64>("1.9").is_err());
assert!(json::decode::<u64>("1e300").is_err());
```

A hand-written `Decode` impl that only provides `decode` keeps `decode`'s
(lenient) behaviour for its own subtree, because the default `decode_from` hands
it a `Value`. Override `decode_from` to be strict there too.

## The `#[derive(Encode, Decode)]` macros

The derives generate **both** the `Value` path and the streaming path, so a
derived struct is fast on JSON and still works with XML/YAML and the dynamic
`Value` API. They are re-exported from the `codec` module, so
`use ferroly::codec::{Encode, Decode};` brings both the trait and the derive
into scope with a single import.

Supported shapes (the [derive page](derive.md) has the full reference):

- **Structs**: named fields, tuple structs (an array), newtypes (transparent —
  just the inner value) and unit structs (`null`).
- **Enums**: unit variants map to a string; variants with data are externally
  tagged by default (`{"Variant": …}`), or internally tagged with
  `#[ferroly(tag = "...")]`, or `#[ferroly(untagged)]`.
- **Generics**: type parameters, lifetimes, const parameters and where-clauses.

Attributes:

| Attribute | Level | Effect |
| --- | --- | --- |
| `#[ferroly(rename_all = "...")]` | container / variant | rename every field/variant by a case rule |
| `#[ferroly(rename = "...")]` | field / variant | override one wire key |
| `#[ferroly(alias = "...")]` | field / variant | accept another key on decode (repeatable) |
| `#[ferroly(skip_none)]` | field | omit the field entirely when it is `None` |
| `#[ferroly(skip)]` | field | never encode; fill from `Default` on decode |
| `#[ferroly(default)]` / `default = "path"` | field | value for a missing field |
| `#[ferroly(flatten)]` | field | inline a nested struct or map |
| `#[ferroly(deny_unknown_fields)]` | container | reject keys the type does not declare |
| `#[ferroly(tag = "...")]` / `untagged` | enum | choose the enum representation |
| `#[ferroly(bound = "...")]` | container | override the inferred generic bounds |

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

## Limits and untrusted input

Any of the four parsers may be fed by a remote peer — `turbo` and `rest` pick the
parser from the request's `Content-Type`. They therefore share one budget,
enforced wherever a value is built:

| Limit | Default | What it bounds |
| --- | --- | --- |
| `max_depth` | 128 | container nesting. In TOML this includes dotted keys and table headers (`a.b.c = 1` is depth 3), not only nested arrays and inline tables. |
| `max_input_bytes` | 64 MiB | the input size, checked before parsing starts |
| `max_entries` | 10,000,000 | array elements plus object members in one document (XML: elements; TOML: keys, tables, and array elements) — the memory a small input can expand into |
| `reject_duplicate_keys` | `false` | when on, an object that repeats a key is a parse error (see [Duplicate keys](#duplicate-keys)) |

Exceeding one yields `CodecError::LimitExceeded`. The plain entry points use
`Limits::default()`; pass your own to the `*_with` variants:

```rust
use ferroly::codec::{self, json, CodecError, Limits};

// A request-sized budget for a public endpoint.
let limits = Limits::new()
    .with_max_input_bytes(1 << 20)   // 1 MiB
    .with_max_depth(32)
    .with_max_entries(100_000);

let deep = "[".repeat(100);
assert!(matches!(
    json::from_str_with(&deep, limits),
    Err(CodecError::LimitExceeded(_))
));

// Content-type dispatch takes the same budget.
let body = br#"{"id":1}"#;
let v: codec::Value = codec::decode_with("application/json", body, limits).unwrap();
assert_eq!(v.pointer("/id").and_then(codec::Value::as_i64), Some(1));
```

Guarantees for every parser, on any input:

- **No panics.** Truncated or malformed input is a `CodecError`.
- **Bounded recursion.** The parsers are recursive, so `max_depth` also bounds
  stack use. Raising it far above the default risks exhausting the stack.
- **Near-linear time.** Object construction is indexed, so a document with many
  distinct keys or tables does not degrade quadratically.

The streaming JSON decoder (`json::decode`) is covered by the same budget as the
tree parser (`json::from_str`).

### Duplicate keys

By default a repeated object key is kept and lookups (`Value::get`, struct
decoding) resolve to the **last** occurrence. With
`Limits::with_reject_duplicate_keys(true)` it is a `CodecError::Parse` with
the position of the second occurrence instead. The policy per format:

| Format | Default | With `reject_duplicate_keys` |
| --- | --- | --- |
| JSON (`from_str` and the streaming `decode`, including skipped keys) | last wins | error |
| YAML (block and flow mappings) | last wins | error |
| TOML | always an error (the TOML spec forbids it) | error |
| XML | repeated sibling elements are an array, not duplicate keys | not applicable |

Turn it on for untrusted input: when two components resolve a duplicate
differently (first-wins vs last-wins), a crafted document can mean different
things to each. It is off by default to keep existing behaviour.

```rust
use ferroly::codec::{json, Limits};

let strict = Limits::new().with_reject_duplicate_keys(true);
assert!(json::from_str_with(r#"{"role":"user","role":"admin"}"#, strict).is_err());
```

## JSON codec

`ferroly::codec::json` is a hand-written, full-fidelity JSON parser and encoder.
It is the only codec with a **streaming fast-path** on both sides, and it
decodes **strictly**.

- `encode` / `encode_to_vec` stream a value straight to the output — no
  intermediate `Value`. Scalars are appended in place via allocation-free
  writers, and the encoder tracks a per-container "first entry" flag to place
  separators.
- `encode_pretty` / `to_string_pretty` produce indented output (two spaces, one
  member per line; empty containers stay `[]` / `{}`).
- `try_encode` is `encode` that fails on a non-finite float instead of writing
  `null`.
- `decode` / `decode_from_slice` pull tokens on demand from the parser, building
  the target type field by field. Object keys with no escapes are borrowed
  directly (no per-key `String`).
- `to_string` / `from_str` / `from_slice` operate on `Value` directly for
  dynamic use.

Parsing follows RFC 8259:

- Numbers must match the JSON grammar,
  `-? (0 | [1-9][0-9]*) (\.[0-9]+)? ([eE][+-]?[0-9]+)?` — so `01`, `1.`, `-.5`,
  `+1`, and `1e` are errors.
- A number parses to `Int`, then `UInt` (above `i64::MAX`), then `Float`;
  anything with a fraction or exponent is a `Float`. An integer beyond 64 bits
  becomes a `Float` (and loses precision).
- A number that overflows to infinity (`1e999`) is an error, not `inf`.
- Raw control characters (U+0000–U+001F) inside a string are errors; write them
  escaped (`\n`, `\u0001`).
- Strings handle the full escape set including `\uXXXX` and UTF-16 surrogate
  pairs; clean runs are bulk-copied.
- Trailing characters after a complete value are a parse error.

Encoding:

- Integer-valued floats encode with a trailing `.0` so they round-trip as floats.
- Non-finite floats (`NaN`, `±inf`) encode as `null` (`try_encode` errors).
- `Value::Bytes` encodes as a JSON array of byte integers (`[104,105]`).

```rust
use ferroly::codec::{json, Decode, Encode};

#[derive(Encode, Decode, PartialEq, Debug)]
struct Metric { name: String, value: f64, tags: Vec<String> }

let m = Metric { name: "cpu".into(), value: 250.0, tags: vec!["host-a".into()] };
let s = json::encode(&m);
assert_eq!(s, r#"{"name":"cpu","value":250.0,"tags":["host-a"]}"#);

// Indented, for humans.
assert_eq!(
    json::encode_pretty(&m),
    "{\n  \"name\": \"cpu\",\n  \"value\": 250.0,\n  \"tags\": [\n    \"host-a\"\n  ]\n}"
);

// Bytes round-trip.
let bytes = json::encode_to_vec(&m);
let back: Metric = json::decode_from_slice(&bytes).unwrap();
assert_eq!(back, m);

// Strict: a quoted number is not a number.
assert!(json::decode::<Metric>(r#"{"name":"cpu","value":"250","tags":[]}"#).is_err());
```

## XML codec

`ferroly::codec::xml` is a pragmatic, dependency-free XML codec. XML has no
canonical mapping to a generic model, so it uses a fixed struct-oriented
convention:

- An object becomes child elements: `{name: "x"}` → `<name>x</name>`.
- An array becomes repeated sibling elements sharing the tag:
  `{tags: ["a","b"]}` → `<tags>a</tags><tags>b</tags>`.
- Scalars become escaped text; `null` becomes an empty element (`<a/>`).
- The whole document is wrapped in a `<root>` element.

Three shapes cannot be told apart by that convention alone, so the encoder marks
them with an attribute, and the parser honours it:

| Attribute | Meaning | Written for |
| --- | --- | --- |
| `type="array"` | an explicit array whose items are the element's children (written as `<item>`) | an empty array, an array holding arrays, an array whose only item is `null`, and a top-level array |
| `type="object"` | an empty object | `{}` |
| `xml:space="preserve"` | the standard XML attribute: the text is taken verbatim (not trimmed), and empty text is `""` rather than `null`. Inherited by child elements, as XML specifies. | `""` and strings with leading or trailing whitespace |

`xml::encode_pretty` writes the same document indented, one child element per line;
text content is untouched, so it decodes to the same data.

So `{"m": [[1, 2], [3]], "tags": [], "note": ""}` encodes as:

```xml
<root>
  <m type="array">
    <item type="array"><item>1</item><item>2</item></item>
    <item type="array"><item>3</item></item>
  </m>
  <tags type="array"/>
  <note xml:space="preserve"></note>
</root>
```

(shown indented; the encoder writes no whitespace between elements).

**Element names.** A map key that is not a valid XML name is escaped rather than
written raw, so a key can never inject markup: each character that is illegal at
its position is written as `_xHHHH_` (its hex code point), `:` is always
escaped, and the empty key is `_x_`. `"a b"` becomes `<a_x0020_b>`, `"1st"`
becomes `<_x0031_st>`. An underscore that would itself read as such an escape is
escaped too, and the parser reverses all of it, so keys round-trip exactly.

**Text.** `<`, `>`, and `&` are escaped; a carriage return is written as
`&#13;`. Characters XML 1.0 cannot carry at all (U+0000–U+001F other than tab,
LF, CR; U+FFFE; U+FFFF) are written as numeric character references, which this
codec reads back but a strict XML parser rejects — `xml::try_encode` returns
`CodecError::Unrepresentable` for them instead.

On decode:

- Repeated sibling tags collapse back into an array.
- The XML declaration, processing instructions, and comments are skipped;
  `<![CDATA[…]]>` sections are read as literal text.
- The standard entities (`&lt; &gt; &amp; &quot; &apos;`) and numeric references
  (`&#…;` / `&#x…;`, digits only) are decoded.
- Attributes other than the three above are skipped (quote-aware, so `a=">"`
  does not end the tag). `type` and `xml:space` may each appear only once on
  an element.
- Literal line breaks are normalized to LF, as XML specifies: a CRLF or a lone
  CR in text or CDATA reads as `\n`. (A CR written as `&#13;` survives.)
- Leading and trailing XML whitespace in text is trimmed unless
  `xml:space="preserve"` is in effect.
- Anything after the root element other than whitespace, comments, and
  processing instructions is an error. So are a `DOCTYPE`, an unterminated
  construct, and **mixed content** (non-whitespace text alongside child
  elements).
- A plain empty element (`<tags></tags>` or `<tags/>`) is `null`, which the
  lenient `Vec` decode reads as an empty vec and `Option` reads as `None`.

Typed round-trips that work: structs, nested structs, `Option`, `String`
(including `""` and padded strings), `Vec<T>` of any length, nested vecs,
`Vec<Option<T>>` (including `vec![None]`), `Option<Vec<T>>`, maps with arbitrary
keys (including the empty map), tuples, and non-finite floats.

What the mapping genuinely cannot represent:

- **Scalar types at the `Value` level.** Decoded scalars are always strings;
  the lenient `Decode` impls parse numbers and bools on the way into your
  struct. A `Value` round-trip turns `1` into `"1"`.
- **One-element arrays at the `Value` level.** `{"a": ["x"]}` is written as
  `<a>x</a>`, which reads back as the string `"x"`. A typed `Vec` field
  reconstructs the one-element vec; a dynamic `Value` does not.
- **`Value::Bytes`** — written as space-separated numbers, read back as a string.
- **Attributes, namespaces, and mixed content** of foreign documents are not
  modeled, so this codec is for data you control, not arbitrary XML or SOAP.

It is also **not a conforming, validating XML parser**. It is lenient about
well-formedness in ways that do not change what a document means: it accepts
`]]>` in text, element names that start with a digit, undeclared namespace
prefixes, a malformed or misplaced `<?xml …?>` declaration, references to
characters XML 1.0 excludes (`&#1;`), and it does not inspect the values of
attributes it skips. Whenever a conforming parser accepts a document that fits
the convention above, this codec reads the same data from it.

```rust
use ferroly::codec::{xml, Decode, Encode};

#[derive(Encode, Decode, PartialEq, Debug)]
struct Config { name: String, port: u16, tags: Vec<String>, grid: Vec<Vec<i32>> }

let cfg = Config {
    name: "api".into(),
    port: 8080,
    tags: vec!["a".into(), "b".into()],
    grid: vec![vec![1, 2], vec![3]],
};
let doc = xml::encode(&cfg);
assert!(doc.contains("<name>api</name>"));
assert!(doc.contains("<port>8080</port>"));
assert!(doc.contains("<tags>a</tags><tags>b</tags>"));

let back: Config = xml::decode(&doc).unwrap();
assert_eq!(back, cfg);
```

## YAML codec

`ferroly::codec::yaml` covers a well-defined subset of YAML 1.2 — the one people
use for hand-written config — and **rejects everything outside it**. A document
either parses to what a full YAML parser would produce, or it is an error; it is
never silently read as something else.

Supported:

- Block mappings (`key: value`) and block sequences (`- item`), nested by
  indentation. A sequence may sit at the same indentation as its key, and a
  sequence item may be a compact mapping (`- name: a` with further keys aligned
  beneath it).
- Flow collections on a single line: `[a, b]`, `{a: 1, b: [2, 3]}`.
- Plain scalars resolved by the YAML 1.2 core schema: `null` / `~`, `true` /
  `false`, integers (decimal, `0x`, `0o`), floats, `.inf` / `-.inf` / `.nan`.
- Single-quoted strings (with the `''` escape) and double-quoted strings (the
  full escape set, including `\xXX`, `\uXXXX`, and `\UXXXXXXXX`). Keys may be
  quoted too.
- Literal (`|`) and folded (`>`) block scalars, with chomping (`-` / `+`) and
  explicit indentation indicators.
- `#` comments — whole-line, and trailing after whitespace, outside quotes.
- A leading `---` and a trailing `...`.

Rejected with an error that names the feature and the line and column:

- anchors (`&a`), aliases (`*a`), and tags (`!t`, `!!str`);
- directives (`%YAML`) and a second `---` document;
- explicit keys (`? key`) and collections used as keys;
- flow collections that span lines; multi-line plain or quoted scalars;
- inside a flow collection, unquoted text that *starts* with `?` or `:` (YAML
  1.1 reads it as a key or value marker, YAML 1.2 as text — quote it), and a
  mapping written without braces inside a flow sequence (`[a: 1]`);
- a `#` with no whitespace before it directly after a quoted scalar or a flow
  collection (`'x'#c`);
- a top-level block scalar whose content is not indented by at least one space
  (so a stray `|` above an unindented document is an error, not one big string);
- tabs used for indentation;
- any line the parser cannot place (inconsistent indentation, a stray scalar).

Good to know:

- `yes` / `no` / `on` / `off` are **strings**, as in YAML 1.2 (YAML 1.1 read them
  as booleans). The encoder quotes them so older parsers agree.
- `007` is the integer `7`, and `12:30` is a string.
- Where YAML 1.1 and 1.2 differ on plain text, the parser follows 1.2: a `?`
  *inside* an unquoted flow scalar is ordinary text (`[http://h/p?q=1]`), and a
  tab may appear inside a plain scalar or before a comment.
- A block scalar that runs to the end of an input with no final line break has
  no trailing line break to keep, so `a: |\n  x` (no newline at the end) is
  `"x"`, not `"x\n"`.
- Duplicate keys resolve last-wins, as everywhere in the codec.
- Typed decoding is [lenient](#strict-and-lenient-decoding): `port: "8080"`
  decodes into a `u16`.

The encoder always emits block style (`{}` / `[]` for empty collections), and
quotes any string that would otherwise read back as something else — text
containing `: ` or ` #`, text that looks like a number or keyword, padded text,
control characters. Floats keep a `.0` so they stay floats; non-finite floats
are `.inf` / `-.inf` / `.nan`; `Value::Bytes` is a flow sequence of integers.
A `Value` built from JSON-style data therefore round-trips exactly.

```rust
use ferroly::codec::{yaml, Decode, Encode};

#[derive(Encode, Decode, PartialEq, Debug)]
struct Upstream { name: String, port: u16 }

#[derive(Encode, Decode, PartialEq, Debug)]
struct Server { name: String, port: u16, tags: Vec<String>, upstreams: Vec<Upstream> }

// Hand-written YAML: a trailing comment, a flow sequence, compact items.
let text = "\
name: service
port: 9000   # listen port
tags: [x, y]
upstreams:
- name: a
  port: 1
- name: b
  port: 2
";
let s: Server = yaml::decode(text).unwrap();
assert_eq!(s.port, 9000);
assert_eq!(s.tags, vec!["x".to_string(), "y".to_string()]);
assert_eq!(s.upstreams.len(), 2);

// Round-trip through block style.
let out = yaml::encode(&s);
let back: Server = yaml::decode(&out).unwrap();
assert_eq!(back, s);

// Outside the subset is an error, not a string.
assert!(yaml::from_str("a: &anchor 1\nb: *anchor\n").is_err());
```

## TOML codec

`ferroly::codec::toml` covers the common TOML config subset, and decodes
**strictly**. The encoder emits a nested struct as a `[section]` header and a
`Vec`-of-structs as `[[section]]` arrays of tables; scalar keys are written
before child tables, as TOML requires.

Supported: bare / quoted / dotted keys, `key = value` pairs, table headers
(`[a.b]`), arrays of tables (`[[a.b]]`), inline tables (`{ a = 1 }`), arrays
(which may span lines and end with a trailing comma), basic (`"…"`) and literal
(`'…'`) strings with the standard escapes, **multi-line basic (`"""…"""`) and
multi-line literal (`'''…'''`) strings**, integers (decimal with `_` separators
and `0x` / `0o` / `0b` prefixes), floats (including `inf` / `-inf` / `nan`),
booleans, and `#` comments.

Multi-line strings follow the TOML rules: a newline right after the opening
delimiter is dropped; in the basic form escapes apply and a backslash at the end
of a line swallows the following whitespace; the literal form has no escapes.

**Date-times are not a native type.** An offset date-time, local date-time,
local date, or local time token (`1979-05-27T07:32:00Z`,
`1979-05-27 07:32:00`, `1979-05-27`, `07:32:00`) decodes as its raw text in a
`Value::Str` — so it lands in a `String` field — and only its shape is checked
(month 13 is not rejected). On encode a string is always written quoted, so a
date-time read this way is written back as a TOML string, not a date-time.

Strictness:

- Any other bare token is an error — TOML has no unquoted strings
  (`a = hello`, `a = True`).
- Numbers follow the TOML grammar: no leading zeros (`01`), digits on both sides
  of a `.` (`1.`, `.5`), `_` only between digits (`1__0`, `_1`). A float literal
  that overflows to infinity (`1e999`) and an integer beyond 64 bits are errors
  (write `inf` if you mean it).
- Escapes take exactly four (`\u`) or eight (`\U`) hex digits and nothing else.
- A duplicate key, or a key that redefines a value as a table, is an error.
- **A table is defined once.** A second `[a]` header, a `[a]` header for a table
  that dotted keys already built (`a.b = 1` … `[a]`), dotted keys that reach
  into a table declared in another section, and `[a]` for an array of tables
  are all errors — never a silent merge. Declaring a parent after its child
  (`[a.b]` then `[a]`) and adding a sub-table under a dotted-key table are
  allowed, as the TOML specification says. Inline tables and plain arrays are
  closed once written.
- Line endings are LF or CRLF. A carriage return on its own is an error, and so
  is any other raw control character (except tab) in a string or a comment —
  write it as an escape. Inside a multi-line string a CRLF decodes to LF, so the
  text does not depend on how the file was saved.
- Typed decoding does not coerce: `port = "8080"` is not a `u16`.

One deliberate leniency: an inline table may span lines and end with a trailing
comma (`{ a = 1,\n b = 2, }`), which TOML 1.0 forbids.

TOML has no null, so `Value::Null` (a `None` field) is omitted on encode and a
missing key decodes back to `None` for `Option` fields. A `null` *inside an
array* has no faithful form: `toml::encode` writes `""` for it, and
`toml::try_encode` returns `CodecError::Unrepresentable`. A TOML document is
always a table, so `toml::encode` wraps a bare scalar or array as
`value = …` (which `toml::decode` does not unwrap) and `toml::try_encode`
rejects it.

```rust
use ferroly::codec::{toml, Decode, Encode};

#[derive(Encode, Decode, PartialEq, Debug)]
struct Db { host: String, port: u16 }

#[derive(Encode, Decode, PartialEq, Debug)]
struct Config { name: String, motd: String, db: Db }

// Hand-written TOML with a [section] header and a multi-line string.
let text = "\
name = \"api\"
motd = \"\"\"
Welcome.
Be kind.\"\"\"

[db]
host = \"localhost\"
port = 5432
";
let cfg: Config = toml::decode(text).unwrap();
assert_eq!(cfg.db.port, 5432);
assert_eq!(cfg.motd, "Welcome.\nBe kind.");

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

`decode` uses the default [`Limits`](#limits-and-untrusted-input);
`decode_with` takes an explicit budget and is the one to use when the peer
chooses the content type. Scalar handling follows the format — JSON and TOML
strict, XML and YAML lenient. `try_encode` is `encode` that fails when the
selected format cannot represent the value.

```rust
use ferroly::codec::{decode, decode_with, encode, resolve, Decode, Encode, Format, Limits};

#[derive(Encode, Decode, PartialEq, Debug)]
struct Payload { id: u32, ok: bool }

let p = Payload { id: 1, ok: true };

// Encode by content type (e.g. echoing the request's Accept header).
let bytes = encode("application/vnd.api+json", &p).unwrap();
let back: Payload = decode("application/json; charset=utf-8", &bytes).unwrap();
assert_eq!(p, back);

// With a budget sized for the endpoint.
let limits = Limits::new().with_max_input_bytes(64 * 1024);
let back: Payload = decode_with("application/json", &bytes, limits).unwrap();
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
`Value` at all: fields are written to / read from the byte stream directly.

- `Encoder` is a push-based output sink that owns all punctuation and separators
  (`begin_map` / `map_entry` / `end_map`, `begin_seq` / `seq_entry` / `end_seq`,
  and the scalar `encode_*` methods). `encode_f32` has a default that widens to
  `f64` and calls `encode_f64`, so an encoder written before it existed is
  unchanged; the JSON encoder overrides it to write the shortest `f32` text
  (see [Floats](#floats)).
- `Decoder` is a pull-based source: `read_map` / `read_seq` drive a callback per
  entry, `decode_string` pulls a string with one allocation, `peek_null` /
  `decode_null` handle optionals, and `skip_value` discards unknown fields. A
  `Decoder` is a typed source, so the std scalars decode
  [strictly](#strict-and-lenient-decoding) from it.
- `emit_value(e, &value)` is the bridge: it walks any `Value` into any `Encoder`.
  It is both the fallback for `encode_to`'s default and the implementation of
  `Encode for Value`, so a dynamic `Value` still gets the streaming output path.
- `ValueDecoder` is a `Decoder` over an already-parsed `Value`. The TOML codec
  decodes through it (parse to a `Value`, then a strict typed walk);
  `from_value_strict(&value)` is the shorthand.

The XML and YAML codecs go through `Value` on both sides (they call
`value.encode()` and the lenient `T::decode(&parsed)`); only JSON has the
streaming path end to end.

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
        match nums.as_slice() {
            [a, b] => Ok(Pair(*a, *b)),
            _ => Err(CodecError::expected("pair")),
        }
    }
}
```

(For this particular shape the built-in tuple impl — `(i64, i64)` — already does
the job.)

## Error handling

Every fallible codec operation returns `Result<_, CodecError>`. `CodecError`
implements `std::error::Error` and `Display`, derives `Clone`, `PartialEq`,
`Eq`, and `Debug`, and is `#[non_exhaustive]`.

| Variant | Constructor | Raised when |
| --- | --- | --- |
| `UnsupportedContentType(String)` | — | `resolve` / dispatch got an unknown MIME type |
| `Parse(String)` | `CodecError::parse(msg)` | syntax error while parsing JSON/XML/YAML/TOML; the message ends with `at line L, column C`. Never located (see [Field paths](#field-paths)), so a hand-written `Decode` rejecting a field's value should use `expected` or `Message` instead |
| `Expected(&'static str)` | `CodecError::expected(what)` | a value of the right type but the wrong shape or content: a fractional float for an integer, a negative number for an unsigned one, an array of the wrong length, an object without exactly one key for an externally tagged enum, a string that is not an IP address |
| `MissingField(String)` | `CodecError::missing_field(name)` | a required (non-`Option`) struct field was absent |
| `UnknownVariant(String)` | `CodecError::unknown_variant(name)` | an enum string matched no variant |
| `OutOfRange(&'static str)` | `CodecError::out_of_range(ty)` | a number did not fit the target integer type |
| `Message(String)` | — | a general message |
| `LimitExceeded(String)` | — | the input exceeded a [`Limits`](#limits-and-untrusted-input) budget (depth, size, or entries) |
| `Unrepresentable(String)` | `CodecError::unrepresentable(msg)` | a `try_encode` entry point met a value the format cannot represent |
| `TypeMismatch { expected, found }` | `CodecError::type_mismatch(expected, &value)` | a value of the wrong type; prints `expected unsigned integer, found string`. `found` is the kind the input held: `null`, `bool`, `number`, `string`, `bytes`, `array` or `object`. Raised for every kind (string, integer, unsigned integer, number, bool, array, object, null) by the std impls, the derive and both decoders, on the streaming and the `Value` path alike. On the lenient path a string that does not parse as the wanted scalar (`"many"` for a `u32`) is a `TypeMismatch` with `found: "string"` too |
| `At { path, source }` | `err.in_field(key)` / `err.in_index(i)` | `source` was raised inside a field, element or map entry; see [Field paths](#field-paths) |

Parse errors from all four formats carry a 1-based line and column (columns
count characters, not bytes):

```rust
use ferroly::codec::{json, toml, yaml, CodecError, Decode};

let err = json::from_str("{\n  \"a\": 1,\n  \"b\": ?\n}").unwrap_err();
assert_eq!(
    err.to_string(),
    "parse error: unexpected character '?' at line 3, column 8"
);

let err = toml::from_str("a = 1\nb = \n").unwrap_err();
assert_eq!(err.to_string(), "parse error: expected a value at line 2, column 5");

let err = yaml::from_str("a: 1\nb: *alias\n").unwrap_err();
assert!(err.to_string().contains("aliases (*name) are not supported at line 2"));

#[derive(Decode, Debug)]
struct Need { id: u32 }
let err = json::decode::<Need>("{}").unwrap_err();
assert!(matches!(err, CodecError::MissingField(_)));
```

Shape errors raised after parsing (`Expected`, `MissingField`, `OutOfRange`,
`UnknownVariant`, …) carry no line and column; instead they say *which field*
failed.

### Field paths

A decode error raised below the root is wrapped in `CodecError::At`, whose
`path` lists the wire names and indexes leading to it, outermost first.
`Display` prefixes the message with the path, rendered `a.b[2].c` (a key that
is not a plain word of ASCII letters, digits, `_` and `-` is quoted:
`limits["max.depth"]`). An error raised at the root has no path and prints as
it always has.

```rust
use ferroly::codec::{json, CodecError, Decode};

#[derive(Decode, Debug)]
struct Message { role: String, content: String }

#[derive(Decode, Debug)]
#[ferroly(rename_all = "camelCase")]
struct ChatRequest { model: String, messages: Vec<Message>, max_tokens: Option<u32> }

let err = json::decode::<ChatRequest>(
    r#"{"model":"m","messages":[{"role":"user","content":"hi"},{"role":7,"content":"x"}]}"#,
).unwrap_err();
assert_eq!(err.to_string(), "messages[1].role: expected string, found number");
assert_eq!(err.path_string(), "messages[1].role");
// An HTTP API can report the top-level parameter:
assert_eq!(err.path()[0].to_string(), "messages");

// Paths use wire names (after `rename` / `rename_all`).
let err = json::decode::<ChatRequest>(r#"{"model":"m","messages":[],"maxTokens":"8"}"#)
    .unwrap_err();
assert_eq!(err.path_string(), "maxTokens");
assert_eq!(err.to_string(), "maxTokens: expected unsigned integer, found string");
assert!(matches!(err.root_cause(), CodecError::TypeMismatch { found: "string", .. }));

// A missing field is reported at its parent.
let err = json::decode::<ChatRequest>(r#"{"model":"m","messages":[{"role":"user"}]}"#)
    .unwrap_err();
assert_eq!(err.to_string(), "messages[0]: missing field: content");
```

Where the segments come from:

| Shape | Segment |
| --- | --- |
| named struct field | the key the input used: the wire name after `rename` / `rename_all`, or the alias if one matched |
| tuple struct / tuple variant field | its index, `[i]` |
| newtype struct, `Box` / `Rc` / `Arc`, `Option` | none (transparent) |
| externally tagged variant | the variant key: `{"Circle":{"r":"x"}}` fails at `Circle.r` |
| internally tagged or untagged variant | none; an internally tagged variant's fields sit beside the tag, and an untagged enum that matches no variant fails where it sits |
| `flatten` field | none; its fields are reported by their own names |
| `Vec`, `[T; N]`, tuples, `HashSet`, `BTreeSet` | the element index, `[i]`; but on the lenient `Value` path a lone value standing in for a one-element sequence adds no index, so `{"items":{"x":"no"}}` fails at `items.x` there and at `items` (`expected array, found object`) on the strict paths |
| `HashMap<String, _>`, `BTreeMap<String, _>` | the entry key |

The path is built only on failure (`map_err` as the error propagates), and
`in_field` on an error that already has a path adds a segment to it rather than
another `At` layer, so a successful decode allocates nothing extra.
`Parse`, `LimitExceeded` and `UnsupportedContentType` describe the input as a
whole (a parse error already has its line and column), so they never get a
path and keep matching as before. A hand-written `Decode` impl can locate its
own errors the same way, with `err.in_field(key)` / `err.in_index(i)`. For
that reason such an impl should report a field value it rejects with
`CodecError::expected(..)` or `CodecError::Message(..)`, not
`CodecError::parse(..)`: a `Parse` error is returned unchanged by `in_field`
and would arrive with no path.

Keys come from the input, so the rendered path (`Display` and `path_string`)
is safe to log: control characters, Unicode line separators and bidirectional
controls inside a quoted key are escaped (`["a\nb"]`, `\u{1b}`), and a key
longer than 64 characters is cut and ends in `…`. `path()` keeps every key
exactly as it arrived; `PathSegment`'s own `Display` prints it raw.

`CodecError::At`'s fields are public. A hand-built value with an empty path
prints just its source, and one nested inside another renders as a single
joined path (`in_field` merges the layers); `path()` returns only the outer
layer's segments for such a value.

### Migration notes (0.6)

- **Errors inside a field are wrapped in `At`.** Code that matched the variant
  of a nested error directly (`Err(CodecError::MissingField(_))`) should match
  `err.root_cause()` instead. `Display` gains the path prefix
  (`a.b[1]: expected unsigned integer, found string`).
- **Type errors are `TypeMismatch` for every kind.** A value of the wrong type
  used to be `Expected("string")`, `Expected("integer")`,
  `Expected("unsigned integer")`, `Expected("number")`, `Expected("bool")`,
  `Expected("array")`, `Expected("object")` or `Expected("null")`; it is now
  `TypeMismatch { expected, found }` with the same `expected` wording, on both
  decode paths and at the root as well as inside a field. `Display` keeps the
  old text and adds the kind found: `expected unsigned integer` becomes
  `expected unsigned integer, found string`. Match
  `CodecError::TypeMismatch { expected: "integer", .. }` where you matched
  `CodecError::Expected("integer")`. `Expected` remains for a value of the
  right type that does not fit (`1.9` for an integer, `-1` for an unsigned
  one, an array of the wrong length).
- **Malformed JSON is always `Parse`.** The streaming decoder used to report
  some malformed tokens where a string, array, object or `null` was wanted as
  a type error (`{"name": nope}` gave `name: expected string`); it now lexes
  the token first and returns the `Parse` error with its line and column, as
  the `Value` path does.

Schema validation ([schema.md](schema.md)) uses a different, collect-all error
type ([`Violation`](schema.md#api-reference)) rather than `CodecError`, and
compiling a schema up front reports a
[`SchemaError`](schema.md#compile-once-validate-many).

## Limitations

- **JSON** is the only full-fidelity, streaming codec. XML, YAML, and TOML target
  the common struct/config subset and go through `Value`.
- **XML**: scalars are always strings at the `Value` level, and a one-element
  array is indistinguishable from its element there (typed decoding recovers
  both). No attributes (beyond the three the codec itself uses), namespaces, or
  mixed content.
- **YAML**: a strict subset — no anchors/aliases, tags, directives, explicit
  keys, multi-document streams, multi-line flow collections, or multi-line
  plain/quoted scalars. All of them are errors.
- **TOML**: date-times decode as strings and are not validated; a top-level
  value must be a table (struct); `None`/`Null` fields are omitted rather than
  represented, and a `null` inside an array cannot be represented.
- **Integers beyond 64 bits** in JSON or YAML input become `Float` in a `Value`
  and lose precision. An `i128` / `u128` target never takes the rounded value:
  `json::decode` reads the literal exactly, and the `Value` path rejects a
  float at or beyond 2^53. `i128` / `u128` encode wide values as decimal
  strings.
- **Derive limits**: map keys must be strings; a `PhantomData` field must be
  marked `#[ferroly(skip)]`; a flattened catch-all map also receives the keys of a
  flattened struct beside it. See the [derive page](derive.md).
- `Value::Object` is a `Vec`, so key lookup (`get`, `pointer`) is linear — ideal
  for the small objects typical of config and API payloads, not for huge maps.
  (Parsing and `merge` are indexed and stay near-linear.)
- **Duplicate object keys resolve last-wins** in JSON and YAML by default —
  `Value::get` and struct decoding both return the last occurrence;
  `Limits::reject_duplicate_keys` makes them an error. TOML always rejects
  duplicates, and XML collapses repeated tags into an array. See
  [Duplicate keys](#duplicate-keys).
- **Non-finite floats** (`NaN`/`±Inf`) have no JSON form and encode as `null`
  there (`json::try_encode` errors). YAML, TOML, and XML can carry them. This
  holds for `f32` and `f64` alike.
- **`Value::Bytes` does not round-trip as `Bytes`** — it encodes as an array of
  integers (JSON/YAML/TOML) or space-separated numbers (XML). Base64-encode
  binary data into a `Str` yourself if you need fidelity.
- Decoding is from a fully buffered input and encoding to a `String`; there is
  no reader- or writer-based API.

## See also

- [derive](derive.md) — internals of `Encode` / `Decode` / `FerrolyError`.
- [schema](schema.md) — validate a `Value` against a JSON-Schema subset,
  including linear-time regex `pattern` checks.
- [config](config.md) — layered configuration built on the codecs.
- [genai](genai.md) — provider payloads encoded through this codec.
- [errutils](errutils.md) — error utilities complementing `CodecError`.

# Derive macros

**Feature:** `codec` (default, for `Encode`/`Decode`); `FerrolyError` is
always available · **Crate:** `ferroly-derive` (re-exported from
`ferroly::codec`)

`ferroly-derive` is the proc-macro crate behind Ferroly's three derives —
`#[derive(Encode)]`, `#[derive(Decode)]`, and `#[derive(FerrolyError)]`. It
exists so the toolkit needs **no external derive crates at runtime**. The only
build-time cost is the standard proc-macro tooling.

## Overview

The derives split into two families:

- **`Encode` / `Decode`** target the codec's [`Value`](codec.md#the-value-model)
  model and its [streaming traits](codec.md#streaming-fast-path). Each derive
  emits **two** code paths — a `Value`-building path (used by XML, YAML, and any
  dynamic use) and a streaming path (used by JSON for its fast path). You get
  both from one `#[derive]`.
- **`FerrolyError`** generates `Display`, `std::error::Error` (with `source()`),
  and `From` impls from `#[error(...)]`, `#[from]`, and `#[source]` attributes.

> Design note: the derives run entirely at compile time, so there is no runtime
> reflection and no runtime encoding dependency — the wire mapping is baked into
> the generated `impl`s.

The `Encode` / `Decode` derives are re-exported from `ferroly::codec`, so a
single `use ferroly::codec::{Encode, Decode};` imports both the trait and its
derive. `FerrolyError` is re-exported alongside them.

## Enabling

`Encode` / `Decode` come with the default `codec` feature. `FerrolyError` needs
only `std` and is available whenever the crate is. No `tokio`, no runtime deps:

```toml
[dependencies]
ferroly = "*"                 # codec (with the derives) on by default
```

```rust
use ferroly::codec::{Decode, Encode, FerrolyError};
```

## Quick start

```rust
use ferroly::codec::{json, Decode, Encode, FerrolyError};

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(rename_all = "camelCase")]
struct Account {
    account_id: u64,          // -> "accountId"
    #[ferroly(skip_none)]
    display_name: Option<String>,
}

#[derive(FerrolyError, Debug)]
enum ApiError {
    #[error("account {0} not found")]
    NotFound(u64),
    #[error(transparent)]
    Codec(#[from] ferroly::codec::CodecError),
}

fn load(body: &str) -> Result<Account, ApiError> {
    let acct: Account = json::decode(body)?; // CodecError -> ApiError via #[from]
    Ok(acct)
}

fn main() {
    let a = Account { account_id: 7, display_name: None };
    assert_eq!(json::encode(&a), r#"{"accountId":7}"#);
    assert!(load("{").is_err());
}
```

## `#[derive(Encode)]` and `#[derive(Decode)]`

### Supported shapes

- **Structs with named fields.** Tuple structs, newtypes, and unit structs are
  rejected with a compile error.
- **Enums with unit variants only.** A data-carrying variant is a compile error
  (`"Encode enums support only unit variants"`).
- **Unions** are rejected.

### What the derive emits

For a **struct**, `Encode` generates:

- `encode(&self) -> Value` — pushes `(key, field.encode())` pairs into a
  `Vec` and wraps it in `Value::Object`. This is the path XML/YAML and dynamic
  callers use.
- `encode_to<E: Encoder>(&self, e)` — calls `e.begin_map(n)`, one
  `e.map_entry(key, &field)` per field, then `e.end_map()`. No `Value` is built;
  this is the [streaming fast path](codec.md#streaming-fast-path).

`Decode` generates:

- `decode(value)` — takes the value's object entries, allocates one
  `Option<FieldTy>` slot per field, and fills them in a **single pass** over the
  entries (`O(fields)`, not a lookup per field). Unknown keys are ignored.
- `decode_from<D: Decoder>(d)` — drives `d.read_map(...)`, matching each wire key
  and pulling the value straight from the decoder; unknown keys call
  `skip_value`.

After the scan, each field is built from its slot: an `Option<T>` field takes
`slot.flatten()` (absent → `None`); any other field is **required** and yields a
[`MissingField`](codec.md#error-handling) error if its slot is empty.

For a **unit-only enum**, `Encode` maps each variant to its (renamed) name as a
`Value::Str` / `encode_str`, and `Decode` matches the incoming string back to a
variant, erroring with [`UnknownVariant`](codec.md#error-handling) on no match.

### Attributes

| Attribute | Level | Effect |
| --- | --- | --- |
| `#[ferroly(rename_all = "...")]` | container | apply a case rule to every field/variant wire key |
| `#[ferroly(rename = "...")]` | field / variant | set one wire key explicitly (wins over `rename_all`) |
| `#[ferroly(skip_none)]` | field | when the field is `None`, omit it from the output entirely |

`rename_all` accepts: `lowercase`, `UPPERCASE`, `snake_case`,
`SCREAMING_SNAKE_CASE`, `kebab-case`, `camelCase`, `PascalCase`. Any other value
is treated as "no rename". Case conversion derives word boundaries from the
identifier's existing casing (uppercase letters for snake/kebab, `_` splits for
camel/Pascal).

The same attributes apply to both `Encode` and `Decode`, keeping the wire
mapping symmetric so a type round-trips.

```rust
use ferroly::codec::{json, Decode, Encode};

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(rename_all = "kebab-case")]
struct Settings {
    max_retries: u32,            // -> "max-retries"
    #[ferroly(rename = "ttl_seconds")]
    time_to_live: u64,           // explicit override
    #[ferroly(skip_none)]
    fallback_url: Option<String>,
}

let s = Settings { max_retries: 3, time_to_live: 60, fallback_url: None };
assert_eq!(json::encode(&s), r#"{"max-retries":3,"ttl_seconds":60}"#);

// skip_none fields are optional on the way back in.
let back: Settings = json::decode(r#"{"max-retries":3,"ttl_seconds":60}"#).unwrap();
assert_eq!(back, s);
```

Enum renaming works the same way (per-variant `rename` or container
`rename_all`):

```rust
use ferroly::codec::{json, Decode, Encode};

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(rename_all = "SCREAMING_SNAKE_CASE")]
enum Status {
    Active,
    #[ferroly(rename = "on_hold")]
    OnHold,
    Closed,
}

assert_eq!(json::encode(&Status::Active), r#""ACTIVE""#);
assert_eq!(json::encode(&Status::OnHold), r#""on_hold""#);
assert_eq!(json::decode::<Status>(r#""CLOSED""#).unwrap(), Status::Closed);
```

### Optional vs. required fields

`Decode` treats a field as optional **only if its type is spelled `Option<…>`**.
`skip_none` affects the *encode* side (whether `None` is written); on decode, an
`Option` field is always allowed to be absent regardless. A non-`Option` field
that is missing from the input is a hard error.

## `#[derive(FerrolyError)]`

A `thiserror`-subset for enums and structs. It generates `Display`,
`std::error::Error` (including `source()`), and `From` impls — enough to build
idiomatic layered error types with no runtime dependency.

Unlike `Encode`/`Decode`, this derive uses the bare helper attributes `error`,
`from`, and `source` (not the `ferroly(...)` namespace).

### Attributes

| Attribute | Where | Effect |
| --- | --- | --- |
| `#[error("...")]` | variant or struct | format string for `Display`. `{0}`,`{1}` reference tuple fields; `{name}` references named fields. |
| `#[error(transparent)]` | variant or struct (exactly one field) | delegate both `Display` and `source()` to the single wrapped error. |
| `#[from]` | a field | generate `impl From<FieldTy> for TheError`, and mark it as the `source()`. |
| `#[source]` | a field | mark this field as the error's `source()` (without a `From`). |

Notes on behavior:

- **Enums** require an `#[error(...)]` on *every* variant, or the derive errors.
- `{0}` positional placeholders are rewritten internally to capture tuple fields,
  so `#[error("code {0}: {1}")]` on `Variant(u32, String)` works via inline
  formatting.
- `#[from]` is valid on a single-field variant/struct; it implies `#[source]`.
- `#[error(transparent)]` requires **exactly one** field and forwards `Display`
  and `source` to it — ideal for wrapping a lower-level error unchanged.
- `source()` works for `Box<dyn Error>`, `dyn Error + Send + Sync`, and plain
  error types alike (the derive scopes a small `AsDynError` coercion helper
  per-expansion).

### Enum example

```rust
use ferroly::codec::FerrolyError;

#[derive(FerrolyError, Debug)]
enum ConfigError {
    // Named placeholder pulls from the named field.
    #[error("unknown key '{key}'")]
    UnknownKey { key: String },

    // Positional placeholders reference tuple fields.
    #[error("value out of range: {0} (max {1})")]
    OutOfRange(i64, i64),

    // #[from] generates From<CodecError> and sets source().
    #[error(transparent)]
    Decode(#[from] ferroly::codec::CodecError),

    // #[source] marks the cause without generating a From.
    #[error("failed to load {path}")]
    Load {
        path: String,
        #[source]
        cause: std::io::Error,
    },
}

fn parse(input: &str) -> Result<ConfigError, ConfigError> {
    // `?` on a CodecError converts via the generated From impl.
    let _v = ferroly::codec::json::from_str(input)?;
    Err(ConfigError::UnknownKey { key: "x".into() })
}

use std::error::Error;
let e = ConfigError::OutOfRange(500, 100);
assert_eq!(e.to_string(), "value out of range: 500 (max 100)");
assert!(ConfigError::UnknownKey { key: "z".into() }.source().is_none());
```

### Struct example

The same attributes apply at the container level for a struct error type:

```rust
use ferroly::codec::FerrolyError;

// Named-field struct with a #[source].
#[derive(FerrolyError, Debug)]
#[error("request {id} failed")]
struct RequestError {
    id: u64,
    #[source]
    cause: std::io::Error,
}

// Transparent newtype wrapper — forwards Display and source to the inner error.
#[derive(FerrolyError, Debug)]
#[error(transparent)]
struct Wrapped(std::io::Error);
```

## How the two paths fit together

For `Encode`/`Decode`, the generated `encode` / `decode` methods build or read a
[`Value`](codec.md#the-value-model), and the generated `encode_to` / `decode_from`
methods stream through the [`Encoder`](codec.md#streaming-fast-path) /
[`Decoder`](codec.md#streaming-fast-path) traits. The JSON codec supplies both a
streaming encoder and decoder, so `json::encode` / `json::decode` on a derived
type never touch `Value`; XML and YAML use the `Value` methods. You write one
`#[derive]` and get the right path automatically for whichever codec you call.

## Error handling

- The derives report **compile-time** errors for unsupported shapes (non-named
  structs, data-carrying enum variants, unions; a `FerrolyError` enum variant
  missing `#[error(...)]`; `transparent` on a multi-field carrier).
- At **runtime**, derived `Decode` returns [`CodecError`](codec.md#error-handling):
  `Expected("object")` / `Expected("string")` for the wrong container shape,
  `MissingField` for an absent required field, and `UnknownVariant` for an
  unrecognized enum string.
- Derived `FerrolyError` types implement `std::error::Error`, so they compose
  with `?` and `Box<dyn Error>` like any hand-written error.

## Limitations

- `Encode` / `Decode`: **named-field structs and unit-only enums only** — no
  tuple structs, newtypes, or data-carrying enum variants. Implement the traits
  by hand for those (see [Streaming fast-path](codec.md#streaming-fast-path)).
- No `default`, `flatten`, `skip` (beyond `skip_none`), or per-field
  `with`/adapter attributes — the attribute set is intentionally small.
- `rename_all` case rules split words heuristically from the identifier's
  casing; unusual identifiers may not convert as expected.
- `FerrolyError` covers the common `thiserror` surface (`error`, `from`,
  `source`, `transparent`) but not its full feature set (e.g. `#[backtrace]`).

## See also

- [codec](codec.md) — the `Value` model, `Encode`/`Decode`, codecs, and
  `CodecError`.
- [schema](schema.md) — validate the values you decode into these types.
- [errutils](errutils.md) — runtime error utilities that pair with
  `FerrolyError`.

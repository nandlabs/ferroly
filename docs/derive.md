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
  emits **two** code paths — a `Value`-building path (used by XML, YAML, TOML and
  any dynamic use) and a streaming path (used by JSON for its fast path). You get
  both from one `#[derive]`, and they always agree on the wire format.
- **`FerrolyError`** generates `Display`, `std::error::Error` (with `source()`),
  and `From` impls from `#[error(...)]`, `#[from]`, and `#[source]` attributes.

> Design note: the derives run entirely at compile time, so there is no runtime
> reflection and no runtime encoding dependency — the wire mapping is baked into
> the generated `impl`s.

The `Encode` / `Decode` derives are re-exported from `ferroly::codec`, so a
single `use ferroly::codec::{Encode, Decode};` imports both the trait and its
derive. `FerrolyError` is re-exported alongside them and at the crate root.

Every unsupported shape, unknown attribute, repeated attribute or conflicting
combination is a **compile error pointing at the offending tokens**. Nothing is
silently ignored.

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

| Shape | Wire representation |
| --- | --- |
| struct with named fields | object, one entry per field, in declaration order |
| tuple struct with two or more fields | array, one element per field |
| newtype struct (one field) | **transparent** — just the inner value |
| unit struct | `null` |
| enum, unit variants only | the variant name as a string |
| enum with data-carrying variants | externally tagged by default; see [Enum representations](#enum-representations) |

Generic type parameters, lifetimes, const parameters and where-clauses are all
supported; see [Generics and bounds](#generics-and-bounds). Unions are rejected.

```rust
use ferroly::codec::{json, Decode, Encode};

#[derive(Encode, Decode, PartialEq, Debug)]
struct Marker;                       // null

#[derive(Encode, Decode, PartialEq, Debug)]
struct UserId(u64);                  // 7

#[derive(Encode, Decode, PartialEq, Debug)]
struct Point(i32, i32);              // [1,2]

#[derive(Encode, Decode, PartialEq, Debug)]
struct Pin {                         // {"owner":7,"at":[1,2],"marker":null}
    owner: UserId,
    at: Point,
    marker: Marker,
}

let pin = Pin { owner: UserId(7), at: Point(1, 2), marker: Marker };
let text = json::encode(&pin);
assert_eq!(text, r#"{"owner":7,"at":[1,2],"marker":null}"#);
assert_eq!(json::decode::<Pin>(&text).unwrap(), pin);
```

Notes on the struct shapes:

- A struct with a **single named field** is still an object (`{"only":1}`);
  only the tuple form is transparent.
- A tuple struct decodes only from an array of **exactly** its length.
- A tuple field marked `#[ferroly(skip)]` does not count: `struct Id<T>(u64,
  #[ferroly(skip)] PhantomData<T>)` has one encoded field, so it is a
  transparent newtype.
- An empty tuple struct (`struct Nothing();`) is `[]`; an empty braced struct
  (`struct Empty {}`) is `{}`.

### Enum representations

Three representations are available. `Encode` and `Decode` always use the same
one for a given type.

#### Externally tagged (default)

A unit variant is its name as a string. Any other variant is an object with a
single entry: the variant name mapped to its content.

```rust
use ferroly::codec::{json, Decode, Encode};

#[derive(Encode, Decode, PartialEq, Debug)]
enum Shape {
    Dot,                         // "Dot"
    Circle(u32),                 // {"Circle":2}
    Line(i32, i32),              // {"Line":[1,2]}
    Rect { w: u32, h: u32 },     // {"Rect":{"w":3,"h":4}}
}

assert_eq!(json::encode(&Shape::Dot), r#""Dot""#);
assert_eq!(json::encode(&Shape::Circle(2)), r#"{"Circle":2}"#);
assert_eq!(json::encode(&Shape::Line(1, 2)), r#"{"Line":[1,2]}"#);
assert_eq!(json::encode(&Shape::Rect { w: 3, h: 4 }), r#"{"Rect":{"w":3,"h":4}}"#);
assert_eq!(
    json::decode::<Shape>(r#"{"Rect":{"w":3,"h":4}}"#).unwrap(),
    Shape::Rect { w: 3, h: 4 }
);
```

The content of a variant follows the struct rules above: a one-field tuple
variant is transparent, a longer one is an array, a struct variant is an object.
When decoding an enum that has data-carrying variants, a unit variant is also
accepted in the object form with `null` content (`{"Dot":null}`).

An enum whose variants are **all** unit variants keeps the plain-string form on
both sides and accepts nothing else.

#### Internally tagged

Selected with `#[ferroly(tag = "...")]` on the enum. The variant name is stored
under the given key **inside** the object, next to the variant's fields. Only unit and struct variants are allowed (a tuple variant
has no object to put the tag in, and is a compile error).

```rust
use ferroly::codec::{json, Decode, Encode};

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(tag = "type", rename_all = "snake_case")]
enum Event {
    Ping,                                  // {"type":"ping"}
    UserCreated { id: u64, name: String }, // {"type":"user_created","id":1,"name":"a"}
}

let e = Event::UserCreated { id: 1, name: "a".into() };
assert_eq!(json::encode(&e), r#"{"type":"user_created","id":1,"name":"a"}"#);
// The tag may appear anywhere in the object.
assert_eq!(
    json::decode::<Event>(r#"{"id":1,"name":"a","type":"user_created"}"#).unwrap(),
    e
);
assert_eq!(json::decode::<Event>(r#"{"type":"ping"}"#).unwrap(), Event::Ping);
```

The tag is written first. A field whose key equals the tag is a compile error.
Decoding fails with `MissingField(tag)` when the tag is absent, `Expected` when
it is not a string, and `UnknownVariant` when it names no variant.

#### Untagged

Selected with `#[ferroly(untagged)]` on the enum. Only the content is written;
nothing names the variant. Decoding tries each
variant **in declaration order** and takes the first one that decodes.

```rust
use ferroly::codec::{json, Decode, Encode};

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(untagged)]
enum Setting {
    Unset,                      // null
    Flag(bool),                 // true
    Range(u8, u8),              // [1,9]
    Named { name: String },     // {"name":"n"}
}

assert_eq!(json::encode(&Setting::Unset), "null");
assert_eq!(json::encode(&Setting::Range(1, 9)), "[1,9]");
assert_eq!(json::decode::<Setting>("true").unwrap(), Setting::Flag(true));
assert_eq!(
    json::decode::<Setting>(r#"{"name":"n"}"#).unwrap(),
    Setting::Named { name: "n".into() }
);
```

Order the variants from most to least specific. The standard `Decode` impls are
lenient in places (numbers and booleans also decode from strings, and through
the `Value` path a `Vec<T>` accepts a lone element), so an earlier variant can
claim data you meant for a later one — put `One(T)` before `Many(Vec<T>)`, and
`Text(String)` before a numeric variant if both may see quoted digits. If no
variant matches, decoding fails with a `Message` error naming the enum. Variant
`rename` / `alias` have no effect on an untagged enum.

### Container attributes

Placed on the struct or enum.

| Attribute | Applies to | Effect |
| --- | --- | --- |
| `#[ferroly(rename_all = "...")]` | struct, enum | apply a case rule to every field key (struct) or variant name (enum) |
| `#[ferroly(tag = "...")]` | enum | [internally tagged](#internally-tagged) representation |
| `#[ferroly(untagged)]` | enum | [untagged](#untagged) representation |
| `#[ferroly(deny_unknown_fields)]` | struct, enum | decoding fails on a key that no field claims |
| `#[ferroly(bound = "...")]` | struct, enum | replace the inferred trait bounds; see [Generics and bounds](#generics-and-bounds) |

`tag` and `untagged` are mutually exclusive and are compile errors on a struct.

#### `rename_all`

Accepted rules: `lowercase`, `UPPERCASE`, `snake_case`, `SCREAMING_SNAKE_CASE`,
`kebab-case`, `SCREAMING-KEBAB-CASE`, `camelCase`, `PascalCase`. Any other value
is a compile error.

An identifier is split into words at `_`, at a lowercase-or-digit → uppercase
step, and before the last capital of an acronym run, then re-joined:

| Identifier | `snake_case` | `kebab-case` | `camelCase` | `PascalCase` |
| --- | --- | --- | --- | --- |
| `max_conns` | `max_conns` | `max-conns` | `maxConns` | `MaxConns` |
| `OnHold` | `on_hold` | `on-hold` | `onHold` | `OnHold` |
| `HTTPServer` | `http_server` | `http-server` | `httpServer` | `HttpServer` |
| `userID` | `user_id` | `user-id` | `userId` | `UserId` |
| `Sha256Hash` | `sha256_hash` | `sha256-hash` | `sha256Hash` | `Sha256Hash` |

`lowercase` and `UPPERCASE` change the case of the identifier as written, without
splitting it (`OnHold` → `onhold`). `snake_case` and `SCREAMING_SNAKE_CASE` keep
a leading underscore (`_id` stays `_id`). When the heuristic does not give the
key you want (`IPv4Address` → `i_pv4_address`), set it with `rename`.

On an enum, the container's `rename_all` renames the **variants**. To rename
the fields of a struct variant, put `rename_all` on that variant.

#### `deny_unknown_fields`

By default a key that matches no field is skipped. With `deny_unknown_fields`
it is an error (`CodecError::Message("unknown field: <key>")`). On an enum the
rule applies to the fields of every struct variant (and, for an internally
tagged enum, to unit variants, which then accept only the tag); it does not
reach into the types of the fields. Aliases count as known keys; the name of a `skip` field
does not. It cannot be combined with a `flatten` field.

```rust
use ferroly::codec::{json, Decode};

#[derive(Decode, Debug)]
#[ferroly(deny_unknown_fields)]
struct Login {
    user: String,
}

assert!(json::decode::<Login>(r#"{"user":"a"}"#).is_ok());
assert!(json::decode::<Login>(r#"{"user":"a","admin":true}"#).is_err());
```

### Field attributes

Placed on a named field of a struct or of a struct variant.

| Attribute | Effect |
| --- | --- |
| `#[ferroly(rename = "...")]` | set the wire key explicitly (wins over `rename_all`) |
| `#[ferroly(alias = "...")]` | also accept this key when decoding; repeatable; never written |
| `#[ferroly(default)]` | when the key is absent, use `Default::default()` |
| `#[ferroly(default = "path")]` | when the key is absent, call the function at `path` |
| `#[ferroly(skip)]` | never encode and never decode the field; it is built from `Default::default()` (or the `default = "path"` function) |
| `#[ferroly(skip_none)]` | omit the field when it is `None`; absent decodes as `None` |
| `#[ferroly(flatten)]` | merge the field's own entries into this object |

A field of a **tuple** struct or variant accepts only `skip` (optionally with
`default = "path"`).

The same attributes drive both derives, so a type always reads what it writes.

```rust
use ferroly::codec::{json, Decode, Encode};

fn default_port() -> u16 {
    8080
}

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(rename_all = "kebab-case")]
struct Settings {
    max_retries: u32,                    // -> "max-retries"
    #[ferroly(rename = "ttl_seconds", alias = "ttl")]
    time_to_live: u64,                   // explicit key; "ttl" still accepted
    #[ferroly(default = "default_port")]
    port: u16,                           // 8080 when absent
    #[ferroly(default)]
    tags: Vec<String>,                   // empty when absent
    #[ferroly(skip_none)]
    fallback_url: Option<String>,        // omitted when None
    #[ferroly(skip)]
    loaded_from: String,                 // not part of the wire format
}

let s: Settings = json::decode(r#"{"max-retries":3,"ttl":60}"#).unwrap();
assert_eq!(s.time_to_live, 60);
assert_eq!(s.port, 8080);
assert!(s.tags.is_empty() && s.fallback_url.is_none());
assert_eq!(
    json::encode(&s),
    r#"{"max-retries":3,"ttl_seconds":60,"port":8080,"tags":[]}"#
);
```

#### Optional vs. required fields

When decoding, a field may be absent from the input if any of these holds:

- its type is **spelled** `Option<…>` (it becomes `None`);
- it carries `#[ferroly(default)]` or `#[ferroly(default = "path")]`;
- it carries `#[ferroly(skip_none)]` (it becomes `None`);
- it carries `#[ferroly(skip)]`.

Any other field missing from the input is a
[`MissingField`](codec.md#error-handling) error.

The `Option` check is syntactic: the derive sees the tokens of the type, not
what they resolve to. A type alias hides the `Option`, so mark such a field:

```rust
use ferroly::codec::{json, Decode};

type Maybe<T> = Option<T>;

#[derive(Decode, PartialEq, Debug)]
struct Profile {
    nickname: Option<String>,    // optional: spelled `Option`
    #[ferroly(default)]
    avatar: Maybe<String>,       // optional only because of `default`
}

let p: Profile = json::decode("{}").unwrap();
assert_eq!(p, Profile { nickname: None, avatar: None });
```

An explicit `null` decodes through the field's own `Decode` impl, so it is
`None` for an optional type and an error for, say, a `u32` — `default` covers an
**absent** key, not a `null` one.

#### `flatten`

`flatten` inlines a nested value's entries into the enclosing object instead of
nesting them under the field's key. The field's type must encode as an object:
another struct, or a map.

```rust
use std::collections::HashMap;
use ferroly::codec::{json, Decode, Encode, Value};

#[derive(Encode, Decode, PartialEq, Debug)]
struct Audit {
    created_by: String,
    revision: u32,
}

#[derive(Encode, Decode, PartialEq, Debug)]
struct Document {
    id: u8,
    #[ferroly(flatten)]
    audit: Audit,                        // "created_by" and "revision" sit beside "id"
    #[ferroly(flatten)]
    extra: HashMap<String, Value>,       // every other key lands here
}

let text = r#"{"id":1,"created_by":"me","revision":2,"color":"red"}"#;
let doc: Document = json::decode(text).unwrap();
assert_eq!(doc.audit.revision, 2);
assert_eq!(doc.extra["color"], Value::Str("red".into()));
assert_eq!(json::encode(&doc), text);
```

How it works, and what follows from it:

- **Decoding** collects every key that none of the struct's own fields claims
  and hands that set, as one object, to **each** `flatten` field. A flattened
  struct picks out the keys it knows and ignores the rest; a flattened map takes
  them all. The derive cannot tell which keys a nested type consumed, so a
  catch-all map also receives the keys of a flattened struct beside it: `extra`
  above holds `created_by` and `revision` as well as `color`.
- **Encoding** writes the flattened entries at the field's position, and never
  writes a key twice: the struct's own fields (and an enum's `tag`) take
  precedence, then `flatten` fields in declaration order. A later flattened
  entry whose key is already taken is left out. That is what makes the example
  above round-trip — the copies of `created_by` and `revision` in `extra` are
  dropped in favor of `audit`'s. Declare a catch-all map **after** any flattened
  structs so the structs' current values are the ones written.
- A `flatten` field of type `Option<T>` (spelled `Option`) writes nothing when
  `None`, and decodes to `None` whenever `T` cannot be decoded from the
  leftover keys (so a partially present group is `None`, not an error).
- A flattened value that does not encode as an object or `null` cannot be
  merged; it is written under the field's own key instead of being dropped.
- `flatten` costs one intermediate `Value` for that field on the streaming
  path; the rest of the struct still streams.
- It cannot be combined with `rename`, `alias`, `default`, `skip` or
  `skip_none`, nor used in a `deny_unknown_fields` container.

#### Duplicate keys

Within one struct (or one struct variant), every key — after `rename`,
`rename_all` and including every `alias` — must be distinct, and so must the
names and aliases of an enum's variants. A clash is a compile error at the
attribute or identifier that introduced it:

```rust,ignore
#[derive(Encode, Decode)]
struct Clash {
    a: u8,
    #[ferroly(rename = "a")]   // error: duplicate field key "a"
    b: u8,
}
```

#### Raw identifiers

A raw identifier contributes its plain name: `r#type` is the key `type`, both
directly and as the input to `rename_all`.

```rust
use ferroly::codec::{json, Decode, Encode};

#[derive(Encode, Decode, PartialEq, Debug)]
struct Token {
    r#type: String,
    r#match: u8,
}

let t = Token { r#type: "ident".into(), r#match: 1 };
assert_eq!(json::encode(&t), r#"{"type":"ident","match":1}"#);
```

### Variant attributes

Placed on an enum variant.

| Attribute | Effect |
| --- | --- |
| `#[ferroly(rename = "...")]` | set the variant's wire name explicitly (wins over `rename_all`) |
| `#[ferroly(alias = "...")]` | also accept this name when decoding; repeatable; never written |
| `#[ferroly(rename_all = "...")]` | apply a case rule to the keys of this struct variant's fields |

```rust
use ferroly::codec::{json, Decode, Encode};

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(rename_all = "SCREAMING_SNAKE_CASE")]
enum Status {
    Active,
    #[ferroly(rename = "on_hold", alias = "paused")]
    OnHold,
    #[ferroly(rename_all = "camelCase")]
    Closed { closed_by: String },
}

assert_eq!(json::encode(&Status::Active), r#""ACTIVE""#);
assert_eq!(json::encode(&Status::OnHold), r#""on_hold""#);
assert_eq!(json::decode::<Status>(r#""paused""#).unwrap(), Status::OnHold);
assert_eq!(
    json::encode(&Status::Closed { closed_by: "ops".into() }),
    r#"{"CLOSED":{"closedBy":"ops"}}"#
);
```

### Generics and bounds

The type's generic parameters, bounds and where-clause are copied onto the
generated `impl`. On top of that, each **type parameter** mentioned by the type
of an encoded field gets a bound: `T: Encode` on the `Encode` impl and
`T: Decode` on the `Decode` impl.

```rust
use ferroly::codec::{json, Decode, Encode};

#[derive(Encode, Decode, PartialEq, Debug)]
struct Page<T> {
    items: Vec<T>,
    total: u64,
}
// impl<T> Encode for Page<T> where T: Encode { ... }
// impl<T> Decode for Page<T> where T: Decode { ... }

let page = Page { items: vec![1u8, 2], total: 2 };
let text = json::encode(&page);
assert_eq!(text, r#"{"items":[1,2],"total":2}"#);
assert_eq!(json::decode::<Page<u8>>(&text).unwrap(), page);
```

Details of the inference:

- A parameter that appears only in `skip` fields, or only inside
  `PhantomData<…>`, gets no bound.
- For `Decode`, a parameter mentioned by a `#[ferroly(default)]` or
  `#[ferroly(skip)]` field (without a `default = "path"` function, and not
  spelled `Option<…>`) also gets `T: Default`.
- Lifetime and const parameters need no bounds. Note that `Decode` produces
  owned values, so a struct holding `&'a str` can derive `Encode` but not
  `Decode`.

Inference looks at parameters, not at the exact field types, so it can be wrong
for associated types or for wrappers with their own requirements. Override it
with `bound`, which **replaces** the inferred bounds with the where-predicates
you give:

```rust
use ferroly::codec::{json, Decode, Encode};

trait Source {
    type Output;
}

struct Sensor;
impl Source for Sensor {
    type Output = u32;
}

#[derive(Encode, Decode)]
#[ferroly(bound(encode = "S::Output: Encode", decode = "S::Output: Decode"))]
struct Reading<S: Source> {
    output: S::Output,
}

assert_eq!(json::encode(&Reading::<Sensor> { output: 21 }), r#"{"output":21}"#);
```

- `#[ferroly(bound = "T: Encode + Decode")]` — one set of predicates used for
  both derives.
- `#[ferroly(bound(encode = "...", decode = "..."))]` — per derive; a side you
  leave out keeps its inferred bounds.
- An empty string (`bound = ""`) means "no extra bounds".

Paths inside the string are resolved where the type is defined, so the traits
must be in scope there (or written in full, `::ferroly::codec::Encode`).

### What the derive emits

For a **struct with named fields**, `Encode` generates:

- `encode(&self) -> Value` — pushes `(key, field.encode())` pairs into a
  `Vec` and wraps it in `Value::Object`. This is the path XML/YAML/TOML and
  dynamic callers use.
- `encode_to<E: Encoder>(&self, e)` — calls `e.begin_map(n)`, one
  `e.map_entry(key, &field)` per field, then `e.end_map()`. No `Value` is built;
  this is the [streaming fast path](codec.md#streaming-fast-path). `n` is the
  exact number of entries written (fields omitted by `skip_none` are not
  counted).

`Decode` generates:

- `decode(value)` — takes the value's object entries, allocates one
  `Option<FieldTy>` slot per field, and fills them in a **single pass** over the
  entries (`O(fields)`, not a lookup per field). When a key repeats, the last
  occurrence wins.
- `decode_from<D: Decoder>(d)` — drives `d.read_map(...)`, matching each wire key
  and pulling the value straight from the decoder; unknown keys call
  `skip_value`.

The other shapes follow the same pattern with `begin_seq` / `seq_entry` /
`read_seq` for tuples, plain delegation for newtypes, and `encode_null` /
`decode_null` for unit structs.

Enums and the streaming decoder: a `Decoder` cannot look ahead, so only shapes
whose outline is known up front decode without building a `Value`:

| Enum | `decode_from` |
| --- | --- |
| unit variants only | streams (one string) |
| externally tagged, every variant carries data | streams (one map entry, content decoded in place) |
| externally tagged, unit and data variants mixed | reads one `Value`, then decodes from it |
| internally tagged, untagged | reads one `Value`, then decodes from it |

Encoding always streams, whatever the representation.

## `#[derive(FerrolyError)]`

A `thiserror`-subset for enums and structs. It generates `Display`,
`std::error::Error` (including `source()`), and `From` impls — enough to build
idiomatic layered error types with no runtime dependency.

Unlike `Encode`/`Decode`, this derive uses the bare helper attributes `error`,
`from`, and `source` (not the `ferroly(...)` namespace). Structs and enum
variants are handled by the same code, so every attribute works identically on
both.

### Attributes

| Attribute | Where | Effect |
| --- | --- | --- |
| `#[error("...")]` | struct, enum variant, or enum | format string for `Display`; see [Format strings](#format-strings) |
| `#[error(transparent)]` | struct or variant with exactly one field | `Display` shows the wrapped error's message, and `source()` returns the wrapped error |
| `#[from]` | the only field of a struct or variant | generate `impl From<FieldTy> for TheError`, and make the field the `source()` |
| `#[source]` | one field | make this field the error's `source()` (without a `From`) |

Notes on behavior:

- On an **enum**, every variant needs an `#[error(...)]`. An `#[error(...)]` on
  the enum itself is the fallback for variants that have none.
- `#[from]` requires its struct or variant to have **exactly one field** (tuple
  or named), because the generated `From` impl must build the whole value from
  it. On a multi-field carrier it is a compile error — use `#[source]` there.
  Two `#[from]` fields of the same type in one enum are also a compile error.
- `#[source]` may be given once per struct or variant. The field may be an
  error type, a `Box<dyn Error>` (with or without `Send + Sync`), or an
  `Option` of either — then `source()` is `None` while the field is `None`.
- `#[from]` / `#[source]` take no arguments and belong on fields; anything else
  is a compile error.

### Format strings

`#[error("...")]` takes a standard format string:

| Placeholder | Refers to |
| --- | --- |
| `{0}`, `{1}`, … | tuple fields by position |
| `{name}` | a named field (a raw identifier field `r#type` is `{type}`) |
| `{0:?}`, `{name:>8.2}`, `{0:#x}` | any format spec after `:` is passed through |
| `{0:>1$}`, `{value:.digits$}` | a field used as a width or precision argument |
| `{{` and `}}` | literal braces |

```rust
use ferroly::codec::FerrolyError;

#[derive(FerrolyError, Debug)]
enum ParseError {
    #[error("line {0}, column {1}: unexpected {2:?}")]
    Unexpected(usize, usize, char),
    #[error("unknown key '{key}' (did you mean {suggestion:?}?)")]
    UnknownKey { key: String, suggestion: Option<String> },
    #[error("expected a block: {{ ... }}")]
    MissingBlock,
    #[error("bad value for {type}")]
    BadValue { r#type: String },
}

assert_eq!(
    ParseError::Unexpected(3, 9, '}').to_string(),
    "line 3, column 9: unexpected '}'"
);
assert_eq!(ParseError::MissingBlock.to_string(), "expected a block: { ... }");
assert_eq!(
    ParseError::BadValue { r#type: "port".into() }.to_string(),
    "bad value for port"
);
```

A placeholder that names no field is left for the compiler to resolve, so it can
capture a constant or static that is in scope.

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

    // #[from] works on a named field too.
    #[error("cannot read the file")]
    Io {
        #[from]
        cause: std::io::Error,
    },

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
assert!(parse("{").unwrap_err().source().is_some());
```

### Struct example

The same attributes apply at the container level for a struct error type:

```rust
use ferroly::codec::FerrolyError;
use std::error::Error;

// Named-field struct with a #[source].
#[derive(FerrolyError, Debug)]
#[error("request {id} failed")]
struct RequestError {
    id: u64,
    #[source]
    cause: std::io::Error,
}

// #[from] on the only field: `?` converts an io::Error into ConfigUnreadable.
#[derive(FerrolyError, Debug)]
#[error("configuration unreadable")]
struct ConfigUnreadable(#[from] std::io::Error);

// Transparent newtype wrapper — shows the inner error's message.
#[derive(FerrolyError, Debug)]
#[error(transparent)]
struct Wrapped(#[from] std::io::Error);

fn read() -> Result<(), ConfigUnreadable> {
    Err(std::io::Error::other("no such file"))?;
    Ok(())
}

let e = RequestError { id: 7, cause: std::io::Error::other("disk") };
assert_eq!(e.to_string(), "request 7 failed");
assert_eq!(e.source().unwrap().to_string(), "disk");

let e = read().unwrap_err();
assert_eq!(e.to_string(), "configuration unreadable");
assert!(e.source().is_some());

let e: Wrapped = std::io::Error::other("boom").into();
assert_eq!(e.to_string(), "boom");
```

### Generic error types

Generic parameters, lifetimes and where-clauses are carried onto all generated
impls. The bounds the impls need are inferred from how each field is used:

- a field shown by the format string needs the matching formatting trait —
  `Display` for `{0}`, `Debug` for `{0:?}`, `LowerHex` for `{0:x}`, and so on;
- a `#[source]` / `#[from]` / transparent field needs
  `std::error::Error + 'static`.

The bound is placed on the field's type, and only when that type mentions a
type parameter.

```rust
use ferroly::codec::FerrolyError;
use std::error::Error;

#[derive(FerrolyError, Debug)]
#[error("invalid {what}: {value:?}")]
struct Invalid<T> {
    what: &'static str,
    value: T,                    // needs only `T: Debug`
}

#[derive(FerrolyError, Debug)]
enum Retry<E> {
    #[error("attempt failed")]
    Failed(#[from] E),           // needs `E: Error + 'static`
    #[error("gave up after {0} attempts")]
    Exhausted(u32),
}

assert_eq!(
    Invalid { what: "list", value: vec![1, 2] }.to_string(),
    "invalid list: [1, 2]"
);
let e: Retry<std::io::Error> = std::io::Error::other("timeout").into();
assert_eq!(e.source().unwrap().to_string(), "timeout");
```

## How the two paths fit together

For `Encode`/`Decode`, the generated `encode` / `decode` methods build or read a
[`Value`](codec.md#the-value-model), and the generated `encode_to` / `decode_from`
methods stream through the [`Encoder`](codec.md#streaming-fast-path) /
[`Decoder`](codec.md#streaming-fast-path) traits. The JSON codec supplies both a
streaming encoder and decoder, so `json::encode` / `json::decode` on a derived
type avoid `Value` wherever the shape allows (see
[What the derive emits](#what-the-derive-emits)); XML, YAML and TOML use the
`Value` methods. You write one `#[derive]` and get the right path automatically
for whichever codec you call.

## Error handling

- The derives report **compile-time** errors, pointing at the offending
  attribute or item, for: unions; unknown, repeated or malformed attributes; an
  unknown `rename_all` rule; duplicate keys; conflicting attribute combinations
  (`tag` with `untagged`, `skip` with `rename`, `flatten` with
  `deny_unknown_fields`, …); `tag` on an enum with tuple variants; a
  `FerrolyError` variant with no `#[error(...)]`; `transparent` on anything but
  a single-field carrier; and `#[from]` on a carrier with more than one field.
- At **runtime**, derived `Decode` returns [`CodecError`](codec.md#error-handling):

  | Error | When |
  | --- | --- |
  | `Expected(..)` | the wrong container shape (object, array of N elements, string, `null`, single-key object, string tag) |
  | `MissingField(key)` | a required field — or an internally tagged enum's tag — is absent |
  | `UnknownVariant(name)` | a variant name or tag value that matches no variant |
  | `Message(..)` | an unknown key under `deny_unknown_fields`; no matching variant of an untagged enum |

- Derived `FerrolyError` types implement `std::error::Error`, so they compose
  with `?` and `Box<dyn Error>` like any hand-written error.

## Limitations

- `Encode` / `Decode`:
  - no adjacently tagged enums (a separate tag key and content key);
    internally tagged enums cannot have tuple variants;
  - no per-field adapter functions (custom encode/decode for one field), no
    container-level `default`, and no way to skip a field in one direction only;
  - `Decode` builds owned values: there is no borrowing from the input, so
    reference fields can only be encoded;
  - `flatten` and the non-streaming enum shapes go through an intermediate
    `Value` (see [What the derive emits](#what-the-derive-emits)); a flattened
    catch-all map also receives the keys of flattened structs beside it (see
    [`flatten`](#flatten));
  - `rename_all` splits words heuristically; unusual identifiers may need an
    explicit `rename`.
- `FerrolyError` covers the common `thiserror` surface (`error`, `from`,
  `source`, `transparent`, generics) but not its full feature set: no
  `#[backtrace]`, no extra format arguments after the string
  (`#[error("{}", expr)]`), and a field named `source` is **not** treated as the
  source unless it is marked `#[source]`.
- Compile-fail behavior is covered by doctests in `ferroly-derive` that run
  against a small stand-in for `ferroly::codec` (the proc-macro crate cannot
  depend on `ferroly`); the generated code is exercised end-to-end by the
  `derive_*` integration tests in `ferroly`.

## See also

- [codec](codec.md) — the `Value` model, `Encode`/`Decode`, codecs, and
  `CodecError`.
- [schema](schema.md) — validate the values you decode into these types.
- [errutils](errutils.md) — runtime error utilities that pair with
  `FerrolyError`.

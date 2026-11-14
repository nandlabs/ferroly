# ferroly-derive

Derive macros for [Ferroly](https://crates.io/crates/ferroly):

- `#[derive(Encode)]` and `#[derive(Decode)]` — map a Rust type to and from
  `ferroly::codec::Value`, and so to JSON, XML, YAML and TOML.
- `#[derive(FerrolyError)]` — a typed error: `Display` from a format string,
  `source()` chaining, and `From` conversions for `?`.
- `#[ferroly::rt::test]` and `#[ferroly::rt::main]` — async entry points that
  need no direct `tokio` dependency.

**Do not depend on this crate directly.** The macros generate paths into
`ferroly`, and `ferroly` re-exports them where they belong:

```toml
[dependencies]
ferroly = { version = "0.4", features = ["codec"] }   # Encode, Decode
```

| Macro | Use it as | Needs feature |
|---|---|---|
| `Encode`, `Decode` | `ferroly::codec::{Encode, Decode}` | `codec` |
| `FerrolyError` | `ferroly::FerrolyError` | any (crate root) |
| `rt_test`, `rt_main` | `#[ferroly::rt::test]`, `#[ferroly::rt::main]` | `rt` |

The crate is build-time only: it depends on `proc-macro2`, `quote` and `syn`,
and adds nothing to your runtime dependency tree.

## `Encode` and `Decode`

One derive produces both the `Value` path (used by every format) and a
streaming path (used by JSON and TOML to avoid building an intermediate tree).

```rust
use ferroly::codec::{json, Decode, Encode};

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(rename_all = "camelCase", deny_unknown_fields)]
struct User {
    user_name: String,
    #[ferroly(rename = "id", alias = "user_id")]
    identifier: u64,
    #[ferroly(skip_none)]
    nickname: Option<String>,
    #[ferroly(default)]
    roles: Vec<String>,
}

fn main() {
    let user = User {
        user_name: "ada".into(),
        identifier: 7,
        nickname: None,
        roles: vec![],
    };
    assert_eq!(json::encode(&user), r#"{"userName":"ada","id":7,"roles":[]}"#);

    // `alias` accepts an old key; `default` fills a missing field.
    let old: User = json::decode(r#"{"userName":"ada","user_id":7}"#).unwrap();
    assert_eq!(old, user);

    // `deny_unknown_fields` rejects keys the type does not declare.
    assert!(json::decode::<User>(r#"{"userName":"ada","id":7,"extra":1}"#).is_err());
}
```

### Shapes

| Shape | Wire form |
|---|---|
| struct with named fields | an object |
| tuple struct | an array |
| newtype struct `Id(u64)` | the inner value, unchanged |
| unit struct | `null` |
| enum, unit variant | the variant name as a string |
| enum, variant with data | externally tagged `{"Variant": …}` by default |

Generic types work: each type parameter used by an encoded field gets an
`Encode` / `Decode` bound, and `#[ferroly(bound = "...")]` replaces the inferred
bounds when they are wrong for your type. Lifetimes, const parameters and
where-clauses are kept.

### Enum representations

```rust
use ferroly::codec::{json, Decode, Encode};

// Default: externally tagged.
#[derive(Encode, Decode, PartialEq, Debug)]
enum Shape {
    Circle(f64),
    Rect { w: f64, h: f64 },
    Empty,
}

// Internally tagged: the tag is a key inside the object.
#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(tag = "type", rename_all = "snake_case")]
enum Event {
    Started { at: u64 },
    Stopped { at: u64, reason: String },
}

// Untagged: the first variant that decodes wins, so order matters.
#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(untagged)]
enum Port {
    Number(u16),
    Named(String),
}

fn main() {
    assert_eq!(json::encode(&Shape::Circle(1.5)), r#"{"Circle":1.5}"#);
    assert_eq!(json::encode(&Shape::Empty), r#""Empty""#);

    let stopped = Event::Stopped { at: 7, reason: "deploy".into() };
    assert_eq!(json::encode(&stopped), r#"{"type":"stopped","at":7,"reason":"deploy"}"#);
    assert_eq!(json::decode::<Event>(r#"{"type":"started","at":1}"#).unwrap(), Event::Started { at: 1 });

    assert_eq!(json::decode::<Port>("8080").unwrap(), Port::Number(8080));
    assert_eq!(json::decode::<Port>(r#""http""#).unwrap(), Port::Named("http".into()));
}
```

### Attributes

Container (on the struct or enum):

| Attribute | Effect |
|---|---|
| `rename_all = "…"` | case rule for every field key (struct) or variant name (enum): `lowercase`, `UPPERCASE`, `snake_case`, `SCREAMING_SNAKE_CASE`, `kebab-case`, `SCREAMING-KEBAB-CASE`, `camelCase`, `PascalCase`. Acronyms count as one word: `HTTPServer` → `http_server`. |
| `tag = "…"` | internally tagged enum (unit and struct variants) |
| `untagged` | untagged enum |
| `deny_unknown_fields` | decoding fails on a key no field claims |
| `bound = "…"` / `bound(encode = "…", decode = "…")` | replace the inferred generic bounds |

Field:

| Attribute | Effect |
|---|---|
| `rename = "…"` | the wire key; wins over `rename_all` |
| `alias = "…"` | also accept this key when decoding; repeatable |
| `default` / `default = "path"` | value for a missing key — `Default::default()` or the function at `path` |
| `skip` | never encoded or decoded; built from `Default` (or `default = "path"`) |
| `skip_none` | omit the field when it is `None` |
| `flatten` | merge a nested struct's or map's entries into this object |

Variant: `rename`, `alias`, and `rename_all` (for a struct variant's fields).

A field typed exactly `Option<T>` is optional without any attribute. A type
alias for `Option` cannot be recognised by a macro, so mark such a field
`#[ferroly(default)]`.

### Mistakes are compile errors

The derive rejects, with an error pointing at the offending code:

- two fields or variants that map to the same key after `rename`, `rename_all`
  or `alias`;
- an unknown `rename_all` rule, an unknown or repeated attribute key, or a
  field-only attribute on a variant;
- `tag` together with `untagged`, `tag` on a struct, or a tuple variant in an
  internally tagged enum.

### Raw identifiers

A field named `r#type` uses the key `type`.

## `FerrolyError`

```rust
use ferroly::FerrolyError;

#[derive(Debug, FerrolyError)]
enum StoreError {
    #[error("object {key} not found in {bucket}")]
    NotFound { bucket: String, key: String },

    #[error("could not reach the store")]
    Io(#[from] std::io::Error),

    #[error("bad response from {0}: {1:?}")]
    BadResponse(String, Vec<u8>),

    #[error(transparent)]
    Other(Box<dyn std::error::Error + Send + Sync>),
}

#[derive(Debug, FerrolyError)]
#[error("request {id} failed")]
struct RequestError {
    id: u64,
    #[source]
    cause: StoreError,
}

fn fetch() -> Result<(), StoreError> {
    std::fs::read("/no/such/file")?; // `#[from]` makes `?` convert io::Error
    Ok(())
}

fn main() {
    let missing = StoreError::NotFound { bucket: "b".into(), key: "k".into() };
    assert_eq!(missing.to_string(), "object k not found in b");

    let failed = fetch().unwrap_err();
    assert!(matches!(failed, StoreError::Io(_)));
    assert!(std::error::Error::source(&failed).is_some());

    let wrapped = RequestError { id: 9, cause: missing };
    assert_eq!(wrapped.to_string(), "request 9 failed");
    assert!(std::error::Error::source(&wrapped).is_some());
}
```

| Attribute | Where | Effect |
|---|---|---|
| `#[error("…")]` | struct, variant, or enum (fallback for variants without one) | the `Display` text; `{field}`, `{0}`, `{0:?}`, `{0:>1$}` and `{{` / `}}` work as in `format!` |
| `#[error(transparent)]` | single-field struct or variant | forward `Display` and `source()` to the wrapped error |
| `#[from]` | the only field of a struct or variant | generate `From<FieldTy>` and make the field the `source()` |
| `#[source]` | one field | make the field the `source()`; may be an error, a `Box<dyn Error>`, or an `Option` of either |

Generic error types are supported; the `Display` and `Error` bounds are inferred
from how each field is used.

## `#[ferroly::rt::test]` and `#[ferroly::rt::main]`

```rust
#[ferroly::rt::test]
async fn adds() {
    assert_eq!(ferroly::rt::spawn(async { 1 + 1 }).await.unwrap(), 2);
}

#[ferroly::rt::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_two_workers() {}
# fn main() {}
```

They are tokio's own attributes, pointed at the copy of tokio that `ferroly`
re-exports, so every tokio option works and your crate needs no `tokio`
dependency. If you rename the `ferroly` dependency, pass the path yourself:
`#[renamed::rt::test(crate = "renamed::rt::__tokio")]`.

## Limitations

- No adjacently tagged enums; no per-field custom encode/decode functions; no
  way to skip a field in only one direction.
- `Decode` builds owned values; reference fields can be encoded but not decoded.
- `flatten`, internally tagged and untagged enums go through an intermediate
  `Value` rather than the streaming path. A flattened catch-all map also receives
  the keys of a flattened struct beside it.
- `PhantomData` fields must be marked `#[ferroly(skip)]`.
- `FerrolyError` has no `#[backtrace]` and no extra format arguments after the
  string (`#[error("{}", expr)]`).

The full reference, including what the macros expand to, is the
[derive guide](https://github.com/nandlabs/ferroly/blob/main/docs/derive.md).

## Compatibility

MSRV 1.75. Versioned in lockstep with `ferroly`; use the same version for both.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

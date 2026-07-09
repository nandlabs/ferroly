# ferroly::config

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `config` (enables `codec`) — module `ferroly::config`. No external dependencies; built entirely on the in-house [codec](codec.md).

## Overview

`config` is Ferroly's layered configuration loader. It composes configuration from
several sources — explicit values, files, environment variables, and command-line
flags — deep-merges them into a single tree, and binds the result to your own
structs through the [codec](codec.md) `Decode` trait.

Two ideas drive the whole module:

- **Everything is a `Value`.** Each source is parsed into a `ferroly::codec::Value`
  (the codec's format-neutral document model). Merging, dot-path lookup, and struct
  binding all operate on that one representation.
- **Ordered layering, later wins.** You add layers in priority order; each new layer
  is deep-merged *over* the accumulated result. The conventional order is
  defaults → file → environment → CLI flags, so an operator's flag beats an env var,
  which beats a config file, which beats a compiled-in default.

Configuration is bound to types with the codec `Decode` trait. Scalars coming from
env vars, `.properties` files, and CLI flags arrive as strings; the codec's `Decode`
implementations coerce them to the target field types (e.g. `"9000"` → `u16`,
`"true"` → `bool`).

## Enabling

```toml
[dependencies]
ferroly = { version = "0.3", features = ["config"] }
```

Enabling `config` automatically pulls in `codec`.

## Quick start

```rust
use ferroly::config::Config;
use ferroly::codec::Decode;

#[derive(Decode)]
struct Settings {
    port: u16,
    host: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = Config::builder()
        .merge_file("config.json")?   // base layer from a file
        .merge_env("APP_")            // env vars override the file
        .build();

    let settings: Settings = cfg.extract()?;
    println!("listening on {}:{}", settings.host, settings.port);
    Ok(())
}
```

## API reference

### `Config`

The resolved, merged configuration, backed by a single `codec::Value`.

| Method | Description |
|---|---|
| `Config::new() -> Config` | An empty configuration (an empty object). |
| `Config::builder() -> ConfigBuilder` | Start composing layers in priority order. |
| `Config::from_env(prefix: &str) -> Config` | Load *only* from environment variables with the given prefix. |
| `Config::from_file(path: impl AsRef<Path>) -> Result<Config, ConfigError>` | Load *only* from a file; format inferred from the extension. |
| `extract<T: Decode>(&self) -> Result<T, ConfigError>` | Bind the whole configuration into a typed value. |
| `get<T: Decode>(&self, key: &str) -> Result<T, ConfigError>` | Bind a single dot-pathed key (e.g. `"db.host"`). |
| `value(&self) -> &Value` | Borrow the merged underlying `codec::Value`. |

`Config` is `Clone` and `Default` (the default is an empty object, identical to
`Config::new()`). Its `Debug` impl is **redacting**: `{:?}` prints only the
top-level key names and `values: "<redacted>"`, so configuration secrets (tokens,
passwords, connection strings) can't leak into logs or panic messages:

```rust
use ferroly::config::Config;
use ferroly::codec::Value;

let cfg = Config::builder()
    .merge_value(Value::Object(vec![
        ("db_password".into(), Value::Str("s3cr3t".into())),
        ("api_token".into(),   Value::Str("t0ken".into())),
    ]))
    .build();

// Only key names appear; the secret values are never printed.
let shown = format!("{cfg:?}");
assert!(shown.contains("db_password") && shown.contains("api_token"));
assert!(shown.contains("<redacted>"));
assert!(!shown.contains("s3cr3t") && !shown.contains("t0ken"));
```

Environment variables are applied in **sorted key order**, so the merge result is
deterministic. On a scalar-vs-nested collision (`APP_DB` and `APP_DB__HOST`), the
nested form wins (its longer key sorts last and is applied last).

### `ConfigBuilder`

Accumulates layers; each `merge_*` returns `self` for chaining, deep-merging its
source over everything merged so far.

| Method | Description |
|---|---|
| `merge_value(self, value: Value) -> Self` | Merge an explicit `codec::Value` layer (e.g. compiled-in defaults). |
| `merge_file(self, path) -> Result<Self, ConfigError>` | Merge a config file; format inferred from the extension. |
| `merge_env(self, prefix: &str) -> Self` | Merge environment variables whose names start with `prefix`. |
| `merge_args<I: IntoIterator<Item = String>>(self, args: I) -> Self` | Merge command-line flags. |
| `build(self) -> Config` | Finalize into a `Config`. |

### `ConfigError`

```rust
pub enum ConfigError {
    UnsupportedFormat(String),  // file extension not a recognized format
    Io(String),                 // I/O error reading a file
    MissingKey(String),         // get(key) found no such key
    Codec(CodecError),          // parse/decode failure (From<CodecError>)
}
```

Implements `std::error::Error` (via the `FerrolyError` derive). `Codec` is
`#[from]`, so `?` on a codec operation converts automatically.

## Layering in depth

Layers are added by calling `merge_*` methods on the builder, in the order you want
priority to increase. Each call deep-merges its new `Value` **over** the running
accumulator, so **the last layer merged wins** on any conflicting key. The
recommended full stack, lowest to highest priority:

```rust
use ferroly::config::Config;
use ferroly::codec::Value;

let cfg = Config::builder()
    // 1. compiled-in defaults (lowest priority)
    .merge_value(Value::Object(vec![
        ("port".into(), Value::Str("8080".into())),
    ]))
    // 2. a config file
    .merge_file("config.yaml")?
    // 3. environment variables
    .merge_env("APP_")
    // 4. command-line flags (highest priority)
    .merge_args(std::env::args().skip(1))
    .build();
```

### Deep-merge semantics

Merging is recursive on objects and last-writer-wins on everything else:

- **Object + Object** → merged key-by-key. Keys present only in one side are kept;
  keys present in both recurse. This means a later layer can override a *single*
  nested field (say `db.host`) without discarding the sibling fields (`db.port`)
  supplied by an earlier layer.
- **Anything else** (scalar, array, or a type change) → the later value *replaces*
  the earlier one wholesale. Arrays are **not** concatenated or element-merged; a
  later array replaces an earlier one entirely.

### `merge_value` — explicit layers

Merges a `codec::Value` you build yourself. Ideal for compiled-in defaults, or for
injecting values computed at runtime.

### `merge_file` / `from_file` — files by extension

The format is chosen from the file extension (case-insensitive); the extension is
validated *before* the file is read:

| Extension(s) | Format | Parser |
|---|---|---|
| `.json` | JSON | `codec::json` |
| `.yaml`, `.yml` | YAML | `codec::yaml` |
| `.xml` | XML | `codec::xml` |
| `.properties`, `.props`, `.env` | Java-style properties | built-in |

Any other extension yields `ConfigError::UnsupportedFormat`. A read failure yields
`ConfigError::Io`; a malformed document yields `ConfigError::Codec`.

**`.properties` / `.props` / `.env` format.** Lines of `key = value`, with `#` and
`!` line comments and blank lines ignored. Keys nest on `.`, so:

```properties
# service.properties
name = svc
db.host = localhost
db.port = 5432
```

parses to `{ name: "svc", db: { host: "localhost", port: "5432" } }`. Values are
kept as strings (trimmed); the codec coerces them when you `extract`/`get`.

### `merge_env` / `from_env` — environment variables

Selects every process env var whose name starts with `prefix`, strips the prefix,
lowercases the remainder, and nests on the `__` (double-underscore) separator:

| Env var (prefix `APP_`) | Config path |
|---|---|
| `APP_PORT=9000` | `port` = `"9000"` |
| `APP_DB__HOST=localhost` | `db.host` = `"localhost"` |
| `APP_DB__POOL__MAX=10` | `db.pool.max` = `"10"` |

A bare prefix with nothing after it is ignored. All values are strings.

### `merge_args` — command-line flags

Parses an iterator of arguments (typically `std::env::args().skip(1)`). Only
`--`-prefixed tokens are considered; anything else is skipped. Keys nest on `.`:

| Flag form | Result |
|---|---|
| `--db.host localhost` | `db.host` = `"localhost"` (next token is the value) |
| `--db.port=5432` | `db.port` = `"5432"` (inline `=`) |
| `--verbose` | `verbose` = `"true"` (bare flag, no following value) |

A flag is treated as *bare* (value `"true"`) when the next token is absent or itself
starts with `--`. Merged last, flags take the highest precedence.

## Binding to types

`extract` binds the entire merged tree; `get` binds one dot-path. Both go through
the [codec](codec.md) `Decode` trait, which performs string-to-scalar coercion for
values that arrived as strings.

```rust
use ferroly::config::Config;
use ferroly::codec::Decode;

#[derive(Decode)]
struct Db { host: String, port: u16 }

let cfg = Config::from_env("APP_");

// whole-tree binding
#[derive(Decode)]
struct AppConfig { db: Db, verbose: bool }
let app: AppConfig = cfg.extract()?;

// single dot-pathed key
let host: String = cfg.get("db.host")?;
let port: u16    = cfg.get("db.port")?;
```

`get` splits the key on `.`, walks the object tree, and returns
`ConfigError::MissingKey` if any segment is absent. Empty path segments are skipped.

## Worked example: a full layered stack bound to a struct

```rust
use ferroly::config::{Config, ConfigError};
use ferroly::codec::{Value, Decode};

#[derive(Decode, Debug)]
struct Db {
    host: String,
    port: u16,
}

#[derive(Decode, Debug)]
struct Cfg {
    db: Db,
    verbose: bool,
}

fn load() -> Result<Cfg, ConfigError> {
    let cfg = Config::builder()
        // defaults
        .merge_value(Value::Object(vec![
            ("db".into(), Value::Object(vec![
                ("host".into(), Value::Str("localhost".into())),
                ("port".into(), Value::Str("5432".into())),
            ])),
            ("verbose".into(), Value::Str("false".into())),
        ]))
        // operator overrides via env: APP_DB__HOST=prod-db
        .merge_env("APP_")
        // final overrides via flags: --db.port=6000 --verbose
        .merge_args(std::env::args().skip(1))
        .build();

    cfg.extract()
}
```

With `APP_DB__HOST=prod-db` set and `--db.port=6000 --verbose` on the command line,
the result is `Cfg { db: Db { host: "prod-db", port: 6000 }, verbose: true }` — the
default `host` was replaced by the env layer and `port` by the flag layer, while the
untouched default `port` would have survived had no flag overridden it (deep-merge
preserves the sibling `host` supplied by the env layer).

## Error handling

- `from_file` / `merge_file` return `Err` eagerly: `UnsupportedFormat` for an
  unknown extension (checked before any disk access), `Io` for a read failure, and
  `Codec` for a malformed document.
- `merge_env`, `merge_args`, and `merge_value` are infallible — they cannot fail and
  return `Self` directly.
- `extract` and `get` return `Codec` errors when a value cannot be coerced to the
  target type, and `get` returns `MissingKey` when the path does not resolve.

## Limitations

- **No TOML/INI file support.** Only JSON, YAML, XML, and the properties family are
  recognized; an `.ini`/`.toml` path returns `UnsupportedFormat`.
- **Arrays replace, never merge.** A later layer's array wholly replaces an earlier
  one; there is no append or index-merge.
- **Env / properties / flag values are strings.** Rely on codec coercion during
  `extract`/`get`; there is no per-source typing.
- **`merge_env` reads the live process environment** at merge time; it is not a
  snapshot and picks up whatever `std::env::vars()` reports.

## See also

- [codec](codec.md) — the `Value` model, and the `Encode`/`Decode` traits used for
  parsing files and binding structs.
- [derive](derive.md) — the `FerrolyError` derive behind `ConfigError`, and the
  `Decode`/`Encode` derives.
- [lifecycle](lifecycle.md) — pairs naturally with `config` to build up a
  configured service.

---
**Related:** [codec](codec.md), [derive](derive.md), [lifecycle](lifecycle.md).

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

On top of the basic layers the module provides optional files and profile
overlays, `${VAR}` interpolation, secrets from files, list and key-case handling
for environment variables, required-key and schema validation, provenance (which
layer set each key), a redacted dump of the effective configuration, and a
shareable handle with manual reload.

## Enabling

```toml
[dependencies]
ferroly = { version = "0.4", features = ["config"] }
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
| `contains(&self, key: &str) -> bool` | Whether a dot-pathed key is present and non-null. |
| `require(&self, keys) -> Result<(), ConfigError>` | Check that every listed key is present; reports **all** missing keys. |
| `validate_schema(&self, schema: &Value) -> Result<(), ConfigError>` | Validate against a JSON-Schema subset; reports all violations. |
| `value(&self) -> &Value` | Borrow the merged underlying `codec::Value`. |
| `source(&self, key: &str) -> Option<&Source>` | The layer that supplied a leaf key. |
| `sources(&self) -> impl Iterator<Item = (&str, &Source)>` | Every leaf key with its origin, sorted by key. |
| `to_redacted_string(&self) -> String` | The effective configuration with sensitive values masked. |
| `to_redacted_string_with(&self, sensitive: impl Fn(&str) -> bool) -> String` | Same, with your own notion of a sensitive key. |
| `into_shared(self) -> SharedConfig` | Wrap in a shareable, reloadable handle. |

`Config` is `Clone` and `Default` (the default is an empty object, identical to
`Config::new()`). Its `Debug` impl is **redacting**: `{:?}` prints only the
top-level key names and `values: "<redacted>"`, so configuration secrets (tokens,
passwords, connection strings) can't leak into logs or panic messages. The same
holds for `ConfigBuilder` and `SharedConfig`:

```rust
use ferroly::config::Config;
use ferroly::codec::Value;

let builder = Config::builder()
    .merge_value(Value::Object(vec![
        ("db_password".into(), Value::Str("s3cr3t".into())),
        ("api_token".into(),   Value::Str("t0ken".into())),
    ]));

// Only key names appear; the secret values are never printed —
// neither from the builder…
assert!(!format!("{builder:?}").contains("s3cr3t"));

// …nor from the built configuration.
let cfg = builder.build();
let shown = format!("{cfg:?}");
assert!(shown.contains("db_password") && shown.contains("api_token"));
assert!(shown.contains("<redacted>"));
assert!(!shown.contains("s3cr3t") && !shown.contains("t0ken"));
```

### `ConfigBuilder`

Accumulates layers; each `merge_*` returns `self` for chaining, deep-merging its
source over everything merged so far.

| Method | Description |
|---|---|
| `merge_value(self, value: Value) -> Self` | Merge an explicit `codec::Value` layer (e.g. compiled-in defaults). |
| `merge_file(self, path) -> Result<Self, ConfigError>` | Merge a config file; format inferred from the extension. |
| `merge_file_optional(self, path) -> Result<Self, ConfigError>` | Same, but a missing file is skipped. |
| `merge_profile(self, base, profile: &str) -> Result<Self, ConfigError>` | Merge `base.<ext>` and then `base-<profile>.<ext>` if present. |
| `merge_env(self, prefix: &str) -> Self` | Merge environment variables whose names start with `prefix`. |
| `merge_env_with(self, prefix: &str, options: &EnvOptions) -> Result<Self, ConfigError>` | Same, with opt-in behaviours (secret files, lists, key-case matching). |
| `merge_args<I: IntoIterator<Item = String>>(self, args: I) -> Self` | Merge raw `--key=value` command-line flags. |
| `merge_cli(self, matches: &cli::Matches) -> Self` | Merge explicitly given arguments of a parsed command line (*`cli` feature*). |
| `merge_cli_defaults(self, matches: &cli::Matches) -> Self` | Merge only the defaults of a parsed command line (*`cli` feature*). |
| `interpolate(self) -> Result<Self, ConfigError>` | Expand `${VAR}` placeholders in everything merged so far. |
| `build(self) -> Config` | Finalize into a `Config`. |

### `ConfigError`

```rust
#[non_exhaustive]
pub enum ConfigError {
    UnsupportedFormat(String),                        // extension not a recognized format
    Io(String),                                       // I/O error reading a config or secret file
    MissingKey(String),                               // get(key) found no such key
    MissingKeys(Vec<String>),                         // require(..): every missing key
    Codec(CodecError),                                // extract() could not bind (From<CodecError>)
    Parse { path: String, message: String },          // a config file is malformed
    InvalidValue { key: String, source: CodecError }, // get(key): value does not convert
    Interpolation { key: String, reason: String },    // a ${…} placeholder failed
    Schema(Vec<String>),                              // validate_schema(..): every violation
    InvalidProfile(String),                           // merge_profile: profile is not a plain name
}
```

Implements `std::error::Error` (`source()` returns the wrapped `CodecError` for
`Codec` and `InvalidValue`). `From<CodecError>` is provided, so `?` on a
codec operation converts automatically, and `ConfigError` itself converts into
`ferroly::Error`.

**No variant carries a configuration value.** Messages name files, keys and
environment variables only, so a `ConfigError` is safe to log. In particular a
parse error never repeats the offending line of the file — see
[Secret hygiene](#secret-hygiene).

With the `errutils` feature, `ConfigError` implements
[`Classify`](errutils.md#classification-classify--errorkind): `InvalidInput`,
except `UnsupportedFormat` (`Unsupported`) and `Io` (`Unknown`).

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
    // 2. a config file, plus its per-environment overlay
    .merge_profile("config/app", &std::env::var("APP_PROFILE").unwrap_or_default())?
    // 3. environment variables
    .merge_env("APP_")
    // 4. command-line flags (highest priority)
    .merge_args(std::env::args().skip(1))
    // 5. expand ${…} placeholders across the merged result
    .interpolate()?
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
| `.toml` | TOML | `codec::toml` |
| `.xml` | XML | `codec::xml` |
| `.properties`, `.props`, `.env` | Java-style properties | built-in |

Any other extension yields `ConfigError::UnsupportedFormat`, whose message names
the file and lists every supported extension. A read failure yields
`ConfigError::Io` (the message starts with the path); a malformed document yields
`ConfigError::Parse { path, message }`.

The YAML, TOML and XML codecs cover the common configuration subset of their
formats — see the [codec](codec.md) page for exactly what each one models.

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

### `merge_file_optional` — files that may be absent

Identical to `merge_file`, except that a file that does not exist is skipped
without error. *Only* absence is forgiven: an unsupported extension, a permission
problem or malformed content still fail, so a broken override file is never
silently ignored.

```rust
use ferroly::config::Config;

let cfg = Config::builder()
    .merge_file("config/defaults.toml")?                 // must exist
    .merge_file_optional("/etc/myapp/config.toml")?      // machine-wide, if any
    .merge_file_optional("config.local.toml")?           // developer override, if any
    .build();
```

### `merge_profile` — per-environment overlays

`merge_profile(base, profile)` merges a base file and then, if present, its
profile overlay named `<base>-<profile>.<ext>`:

```rust
use ferroly::config::Config;

// config/app.toml, then config/app-prod.toml if it exists
let cfg = Config::builder()
    .merge_profile("config/app", "prod")?
    .build();
```

- **`base` without an extension** (`config/app`): every supported extension is
  tried, in the order `.json`, `.yaml`, `.yml`, `.toml`, `.xml`, `.properties`,
  `.props`, `.env`, and every file found is merged (normally there is just one).
- **`base` with an extension** (`config/app.yaml`): exactly that file, and
  `config/app-prod.yaml` as the overlay.
- **The base is required** — if no base file exists the call fails with
  `ConfigError::Io`. The overlay is optional. (Use `merge_file_optional` directly
  if the base may be absent too.)
- **An empty profile** loads only the base, which makes
  `std::env::var("APP_PROFILE").unwrap_or_default()` a convenient argument.
- **The profile is a name, not a path.** It may contain only letters, digits, `_`,
  `-` and `.` (and may not start with `.`); anything else is
  `ConfigError::InvalidProfile`. This keeps a profile taken from the environment
  from pointing outside the config directory.

### `merge_env` / `from_env` — environment variables

Selects every process env var whose name starts with `prefix`, strips the prefix,
lowercases the remainder, and nests on the `__` (double-underscore) separator:

| Env var (prefix `APP_`) | Config path |
|---|---|
| `APP_PORT=9000` | `port` = `"9000"` |
| `APP_DB__HOST=localhost` | `db.host` = `"localhost"` |
| `APP_DB__POOL__MAX=10` | `db.pool.max` = `"10"` |

Rules:

- A bare prefix with nothing after it is ignored. All values are strings.
- Variables are applied in **sorted key order**, so the merge result is
  deterministic. On a scalar-vs-nested collision (`APP_DB` and `APP_DB__HOST`), the
  nested form wins (its longer key sorts last and is applied last).
- A variable whose path would contain an **empty segment** — `APP_A____B`, a
  leading `APP___X` or a trailing `APP_X__` — is ignored; it never creates an
  empty-string key.
- A variable whose name or value is **not valid UTF-8** is ignored. Unrelated
  non-UTF-8 variables in the environment do not affect loading.

### `merge_env_with` — `EnvOptions`

`merge_env_with(prefix, &options)` applies the same mapping plus the behaviours you
switch on. It is fallible only because a secret file may be unreadable.

| `EnvOptions` method | Effect |
|---|---|
| `EnvOptions::new()` | no extras — identical to `merge_env` |
| `secret_files()` | `APP_X_FILE=/path` sets `x` to the content of that file |
| `list_keys(sep, keys)` | the listed dotted keys become lists, split on `sep` |
| `split_all(sep)` | every value containing `sep` becomes a list |
| `preserve_case()` | do not lowercase variable names |
| `case_insensitive()` | match keys of earlier layers ignoring case and `_`/`-` |

**Secrets from files.** Container platforms mount secrets as files and pass their
path in a `*_FILE` variable. With `secret_files()`:

```text
APP_DB__PASSWORD_FILE=/run/secrets/db_password   →   db.password = <content of the file>
```

The content is read as UTF-8 (at most 1 MiB) and its trailing newline is removed.
If both `APP_DB__PASSWORD` and `APP_DB__PASSWORD_FILE` are set, the file wins. A
file that cannot be read is a `ConfigError::Io` naming the variable and the path.
Values loaded this way are always masked by `to_redacted_string`. (With the option
on, a variable that merely *ends* in `_FILE` is treated as a pointer to a file —
do not enable it if you have ordinary keys named like that.)

**Lists.** Environment values are plain strings. To get a list either name the
keys that are lists or split everything:

```rust
use ferroly::config::{Config, EnvOptions};

// APP_ALLOWED_HOSTS="a.example, b.example"   →   ["a.example", "b.example"]
// APP_PORTS=8080                             →   ["8080"]
let cfg = Config::builder()
    .merge_env_with("APP_", &EnvOptions::new().list_keys(',', ["allowed_hosts", "ports"]))?
    .build();
let hosts: Vec<String> = cfg.get("allowed_hosts")?;
```

Items are trimmed and empty items dropped, so one item gives a one-element list
and an empty value an empty list. `split_all(',')` needs no key list but turns
*any* value containing the separator into a list — a sentence or a connection
string included — so prefer `list_keys` when such values exist.

**Reaching camelCase keys.** Names are lowercased by default, so
`APP_MAX_CONNECTIONS` becomes `max_connections` and never reaches a field that the
file (or a `#[ferroly(rename_all = "camelCase")]` struct) spells `maxConnections`.
Two options fix that:

- `case_insensitive()` compares each key segment with the keys **already present
  in the earlier layers**, ignoring case and `_`/`-`, and adopts the existing
  spelling: `APP_DB_POOL__MAX_CONNECTIONS` overrides `dbPool.maxConnections` when a
  default or file layer defines it. Segments without a counterpart are lowercased
  as usual, so merge your defaults or file first.
- `preserve_case()` keeps the variable's own spelling: `APP_dbPool__maxConnections`.

### `merge_args` — raw command-line flags

Parses an iterator of arguments (typically `std::env::args().skip(1)`). Only
`--`-prefixed tokens are read; anything else is skipped. A bare `--` ends
processing — the tokens after it are positionals and are never read as flags.
Keys nest on `.`:

| Flag form | Result |
|---|---|
| `--db.port=5432` | `db.port` = `"5432"` — always, known key or not |
| `--db.host localhost` | `db.host` = `"localhost"` **if** `db.host` already holds a non-boolean value |
| `--verbose` | `verbose` = `"true"` (a switch) |
| `--` | stop: everything after it is positional |

**The rule for `--flag value`.** Without a declaration there is no way to know
whether `--verbose input.txt` means "verbose = input.txt" or "the switch `verbose`,
then a positional". `merge_args` therefore lets the layers merged *before* it
decide:

- the key already holds a **non-boolean scalar or a list** → the next token is its
  new value (`--db.port 5432`, `--offset -5`);
- the key already holds a **boolean** (`true`/`false`, as a boolean or a string),
  **or does not exist yet** → the flag is a switch set to `"true"`, and the next
  token is left alone;
- a token starting with `--` is never taken as a value.

So merge your defaults first, or write `--key=value`, which is always unambiguous.
Merged last, flags take the highest precedence.

### `merge_cli` — parsed command lines

*Requires the `cli` feature in addition to `config`.* `merge_args` guesses;
[`ferroly::cli`](cli.md) knows. When the program declares its interface with
`cli::Command`, feed the parsed `Matches` to the builder instead:

```rust
use ferroly::cli::{Arg, Command};
use ferroly::config::Config;

let cmd = Command::new("svc")
    .flag(Arg::new("port").long("port").default("8080"))
    .flag(Arg::new("db.host").long("db-host"))
    .flag(Arg::new("verbose").short('v').count());
let m = cmd.parse_env_or_exit();

let cfg = Config::builder()
    .merge_cli_defaults(&m)              // the parser's defaults: lowest priority
    .merge_file_optional("svc.toml")?
    .merge_env("SVC_")
    .merge_cli(&m)                       // what the operator typed: highest priority
    .build();
```

- `merge_cli` merges the arguments that were given **explicitly** — on the command
  line or through an argument's `.env()` variable. Values that are only an
  argument's default are left out, so a default can never override a file.
- `merge_cli_defaults` merges exactly those defaults; use it as the first layer.
- The argument **name** is the config key, and dots nest (`db.host` → `db.host`).
- Switches become booleans, counters integers, repeatable arguments lists.
- Only the given `Matches` level is read; pass a subcommand's matches to merge its
  arguments too.

### `interpolate` — `${VAR}` placeholders

`interpolate()` expands placeholders in every string value merged so far (keys are
left alone). It is opt-in and a builder step: call it after the last `merge_*`.

| Syntax | Meaning |
|---|---|
| `${NAME}` | the value of `NAME`; an error if it is not set |
| `${NAME:-default}` | the value of `NAME`, or `default` when it is unset **or empty** |
| `$${` | a literal `${` |

`NAME` is looked up as a **dot-pathed key of this configuration** first and then as
an **environment variable** of exactly that name, so both `${db.host}` and
`${HOME}` work. A key that refers to its own name (`PORT: "${PORT}"`) reads the
environment variable instead of itself.

```rust
use ferroly::config::Config;

// app.toml:
//   data_dir = "${DATA_DIR:-/var/lib/app}"
//   cache    = "${data_dir}/cache"
//   [db]
//   host = "localhost"
//   url  = "postgres://${db.host}:5432/main"
let cfg = Config::builder()
    .merge_file("app.toml")?
    .interpolate()?
    .build();
```

- Defaults may contain placeholders: `${A:-${B:-fallback}}`.
- A referenced config key must be a scalar; numbers and booleans are inserted as
  text. Referring to a section or a list is an error.
- **Cycles are detected**: `a = "${b}"`, `b = "${a}"` fails with
  `ConfigError::Interpolation { key: "a", reason: "cycle: a -> b -> a" }` rather
  than looping. Nesting is limited to 32 levels and a single expanded value to
  1 MiB.
- Errors name the key and the placeholder, never a value.
- An unterminated `${` or an empty `${}` is an error. A `$` not followed by `{` is
  left as it is.

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
When the value is there but does not convert, the error is
`ConfigError::InvalidValue { key, source }` and names the full dotted path:

```text
invalid value for 'db.port': expected integer
```

`extract` reports the codec's error as `ConfigError::Codec`. The codec does not
track where in the tree a decode failed, so that error names the field but not its
full path; binding sections with `get::<Section>("db")` narrows it down.

## Validation

### Required keys

`require` checks a list of dot-pathed keys and reports **every** missing one at
once (a key is missing when it is absent or null), so a deployment is fixed in one
round rather than one key per restart:

```rust
use ferroly::config::{Config, ConfigError};

let cfg = Config::from_env("APP_");
match cfg.require(["port", "db.host", "db.password"]) {
    Ok(()) => {}
    Err(ConfigError::MissingKeys(keys)) => {
        // e.g. ["db.host", "db.password"]
        eprintln!("missing configuration: {}", keys.join(", "));
    }
    Err(e) => return Err(e.into()),
}
```

### Schema

`validate_schema` checks the merged tree against a JSON-Schema subset using
[`codec::schema`](schema.md) (`type`, `enum`, `const`, `required`, `properties`,
`items`, `minimum`/`maximum`, `minLength`/`maxLength`, `minItems`/`maxItems`) and
returns every violation in `ConfigError::Schema`:

```rust
use ferroly::codec::json;
use ferroly::config::Config;

let schema = json::from_str(r#"{
    "type": "object",
    "required": ["port"],
    "properties": {
        "port":  { "type": "integer", "minimum": 1, "maximum": 65535 },
        "debug": { "type": "boolean" }
    }
}"#)?;

let cfg = Config::from_env("APP_");   // APP_PORT=8080 arrives as the string "8080"
cfg.validate_schema(&schema)?;        // …and still satisfies "type": "integer"
```

Values from the environment, `.properties` files and flags are strings. Before
checking, a string is read as the number or boolean the schema asks for at that
position, mirroring what `extract` would do; the stored configuration is not
changed.

## Provenance

Every leaf value remembers the layer that last set it:

```rust
use ferroly::config::{Config, Source};

let cfg = Config::builder()
    .merge_file("app.toml")?
    .merge_env("APP_")
    .build();

match cfg.source("db.host") {
    Some(Source::Env(var)) => println!("db.host comes from ${var}"),
    Some(Source::File(path)) => println!("db.host comes from {path}"),
    other => println!("db.host: {other:?}"),
}

for (key, source) in cfg.sources() {
    println!("{key}: {source}");
}
```

| `Source` | Set by | `Display` |
|---|---|---|
| `Value` | `merge_value` | `value` |
| `File(path)` | `merge_file`, `merge_file_optional`, `merge_profile` | `file <path>` |
| `Env(var)` | `merge_env`, `merge_env_with` | `env <VAR>` |
| `SecretFile { var, path }` | `merge_env_with` + `secret_files()` | `secret file <path> (env <VAR>)` |
| `Args` | `merge_args` | `args` |
| `Cli` | `merge_cli`, `merge_cli_defaults` | `cli` |

Keys are full dotted paths to leaf values; a list counts as one leaf, and a key
naming a whole section has no source.

## Secret hygiene

- **`Debug` never prints values** — for `Config`, `ConfigBuilder` and
  `SharedConfig` alike.
- **Errors never carry values.** In particular, when a config file fails to parse,
  any text the parser copied from the file is replaced by `<redacted>` before the
  error leaves the module, and the line number is added instead:

  ```text
  invalid config file 'config/app.yaml': parse error: expected 'key: value', got: <redacted> (line 7)
  ```

- **`to_redacted_string`** is the way to log the effective configuration:

  ```rust
  use ferroly::config::Config;

  let cfg = Config::from_env("APP_");
  println!("{}", cfg.to_redacted_string());
  ```

  ```text
  name = "svc"  # file config/app.toml
  port = "9000"  # env APP_PORT
  db.host = "localhost"  # file config/app.toml
  db.password = <redacted>  # secret file /run/secrets/db_password (env APP_DB__PASSWORD_FILE)
  db.url = <redacted>  # file config/app.toml
  tags[0] = "a"  # file config/app.toml
  ```

  A value is masked when (a) its key path looks sensitive — `is_sensitive_key`
  matches `password`, `passwd`, `passphrase`, `pwd`, `secret`, `token`, `key`,
  `credential`, `auth`, `private`, `cert`, `signature`, `salt`, `cookie`, `session`
  or `dsn` anywhere in the path, case-insensitively — (b) it was read from a
  secret file, or (c) it is a URL carrying credentials (`scheme://user:pass@host`).
  The heuristic deliberately masks too much rather than too little.

  `to_redacted_string_with(|path| …)` replaces rule (a) with your own predicate
  (a masked section masks everything below it); rules (b) and (c) always apply.
  Combine it with the default via `ferroly::config::is_sensitive_key`.

## Sharing and reloading: `SharedConfig`

`SharedConfig` is a cheaply clonable handle to the *current* configuration. Readers
take a `snapshot()` — an `Arc<Config>` that stays valid and unchanged for as long
as they hold it — and a reload swaps in a new configuration for everyone who asks
afterwards.

| Method | Description |
|---|---|
| `SharedConfig::new(config)` / `config.into_shared()` | wrap a built configuration (no loader) |
| `SharedConfig::with_loader(f) -> Result<SharedConfig, ConfigError>` | build by running `f`, and keep `f` for reloads |
| `snapshot(&self) -> Arc<Config>` | the current configuration |
| `get::<T>(&self, key)` / `extract::<T>(&self)` | shortcuts on the current snapshot |
| `reload(&self) -> Result<Arc<Config>, ConfigError>` | run the loader again and swap the result in |
| `replace(&self, config) -> Arc<Config>` | swap in a configuration by hand; returns the previous one |
| `generation(&self) -> usize` | number of swaps so far |

```rust
use ferroly::config::{Config, SharedConfig};

let shared = SharedConfig::with_loader(|| {
    Ok(Config::builder()
        .merge_file("app.toml")?
        .merge_env("APP_")
        .build())
})?;

// hand clones to workers
let for_worker = shared.clone();
std::thread::spawn(move || {
    let cfg = for_worker.snapshot();          // stable for this unit of work
    let _port: u16 = cfg.get("port").unwrap();
});

// later — e.g. on SIGHUP or from an admin endpoint
match shared.reload() {
    Ok(_) => println!("configuration reloaded (generation {})", shared.generation()),
    Err(e) => eprintln!("reload failed, keeping the current configuration: {e}"),
}
```

- **A failed reload changes nothing**: the error is returned and the current
  configuration stays in place.
- The loader runs outside the lock, so readers never wait on file or environment
  access.
- **There is no file watching.** Reloading happens when you call `reload()`.
- Without a loader (`SharedConfig::new`), `reload()` returns the current snapshot
  unchanged; use `replace`.

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

    cfg.require(["db.host", "db.port"])?;
    cfg.extract()
}
```

With `APP_DB__HOST=prod-db` set and `--db.port=6000 --verbose` on the command line,
the result is `Cfg { db: Db { host: "prod-db", port: 6000 }, verbose: true }` — the
default `host` was replaced by the env layer and `port` by the flag layer, while the
untouched default `port` would have survived had no flag overridden it (deep-merge
preserves the sibling `host` supplied by the env layer). Because the defaults
declare `db.port` as a value and `verbose` as a boolean, `--db.port 6000` (with a
space) and a bare `--verbose` followed by a positional are both read correctly.

## Error handling

- `from_file` / `merge_file` / `merge_file_optional` / `merge_profile` return `Err`
  eagerly: `UnsupportedFormat` for an unknown extension (checked before any disk
  access), `Io` for a read failure, and `Parse` for a malformed document.
- `merge_env`, `merge_args`, `merge_cli` and `merge_value` are infallible — they
  return `Self` directly. `merge_env_with` fails only for an unreadable secret
  file; `interpolate` fails with `Interpolation`.
- `get` returns `MissingKey` when the path does not resolve and `InvalidValue`
  when the value cannot be coerced to the target type; `extract` returns `Codec`.
- `require` returns `MissingKeys`; `validate_schema` returns `Schema`.

## Limitations

- **No INI support**, and no remote or secret-store sources — files, environment
  and arguments only.
- **No file watching / hot reload** — `SharedConfig::reload()` is manual.
- **Arrays replace, never merge.** A later layer's array wholly replaces an earlier
  one; there is no append or index-merge.
- **Env / properties / flag values are strings.** Rely on codec coercion during
  `extract`/`get`; there is no per-source typing.
- **`.env` files are read as properties**, not with dotenv semantics: no `export`
  prefix, no quote removal, no inline comments.
- **`extract` errors do not carry the full key path** (the codec does not track
  it); `get` errors do.
- **Key paths use `.` as the separator**, so a key that itself contains a dot
  cannot be addressed by `get`/`require`/`source`.
- **`merge_env` reads the live process environment** at merge time; it is not a
  snapshot.

## See also

- [codec](codec.md) — the `Value` model, and the `Encode`/`Decode` traits used for
  parsing files and binding structs.
- [schema](schema.md) — the JSON-Schema subset behind `validate_schema`.
- [cli](cli.md) — the declarative argument parser whose results feed `merge_cli`.
- [derive](derive.md) — the `Decode`/`Encode` derives.
- [lifecycle](lifecycle.md) — pairs naturally with `config` to build up a
  configured service.

---
**Related:** [codec](codec.md), [cli](cli.md), [derive](derive.md), [lifecycle](lifecycle.md).

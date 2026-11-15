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
shareable handle with manual reload. It also catches
[unknown keys](#unknown-keys-schema-and-build_checked) per layer, with a "did
you mean" suggestion, [finds the config file](#config-file-discovery-search) in
the usual directories, and binds [secret references](#secret-references-secretref)
(`env:NAME` / `file:/path`) to a redacting `hash::Secret`.

## Enabling

```toml
[dependencies]
ferroly = { version = "0.6", features = ["config"] }
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
| `Config::from_layers(layers: impl IntoIterator<Item = Layer>) -> Result<Config, ConfigError>` | Build from an ordered [`Layer`](#layer--the-stack-as-a-list) list; later layers win. |
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
| `redact_literals(&mut self, keys) -> &mut Self` | In the redacted dump, mask everything under `keys` that is not a [secret reference](#secret-references-secretref). |
| `ignored_keys(&self) -> &[UnknownKey]` | Keys the [schema](#unknown-keys-schema-and-build_checked) did not know (left out of the merge). |
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
| `merge_secret_dir(self, dir) -> Result<Self, ConfigError>` | Merge a mounted secrets directory, one file per key ([Kubernetes / Docker secrets](#merge_secret_dir--mounted-secret-volumes)). |
| `merge_secret_dir_optional(self, dir) -> Result<Self, ConfigError>` | Same, but a missing directory is skipped. |
| `merge_args_with(self, args, options: ArgsOptions) -> Self` | Merge raw `--key value` / `--key=value` flags, each flag's arity decided by [`ArgsOptions`](#merge_args_with--raw-command-line-flags) (e.g. `types(&schema)`). |
| `merge_args(self, args) -> Self` | Shorthand for `merge_args_with(args, ArgsOptions::default())`: the [layers merged earlier](#the-earlier-layers-rule-argsoptionsdefault-merge_args) decide each flag's arity. |
| `merge_cli(self, matches: &cli::Matches) -> Self` | Merge explicitly given arguments and dynamic flags of a parsed command line (*`cli` feature*). |
| `merge_cli_with(self, matches: &cli::Matches, options: ArgsOptions) -> Self` | Same, typing the dynamic flags with `options` (*`cli` feature*). |
| `merge_cli_defaults(self, matches: &cli::Matches) -> Self` | Merge only the defaults of a parsed command line (*`cli` feature*). |
| `layers(self, layers: impl IntoIterator<Item = Layer>) -> Result<Self, ConfigError>` | Apply an ordered [`Layer`](#layer--the-stack-as-a-list) list, each with its `merge_*` call. |
| `merge_found(self, dirs: &[P], names: &[&str]) -> Result<Self, ConfigError>` | Merge the first config file found among `dirs × names` ([discovery](#config-file-discovery-search)). |
| `interpolate(self) -> Result<Self, ConfigError>` | Expand `${VAR}`, `${env:NAME}` and `${file:/path}` placeholders in everything merged so far (the last two only in [trusted layers](#trusted-layers-interpolateoptions)). |
| `interpolate_with(self, options: &InterpolateOptions) -> Result<Self, ConfigError>` | Same, choosing which layers may use `${env:…}` and `${file:…}`. |
| `schema(self, schema: Value) -> Self` | Check every later layer for [unknown keys](#unknown-keys-schema-and-build_checked). |
| `lenient(self) -> Self` | Let `build_checked` succeed despite unknown keys (they are still dropped). |
| `build(self) -> Config` | Finalize into a `Config`. |
| `build_checked(self) -> Result<Config, ConfigError>` | Same, but fails with `UnknownKeys` listing every unknown key. |

### `ConfigError`

```rust
#[non_exhaustive]
pub enum ConfigError {
    UnsupportedFormat(String),                        // extension not a recognized format
    Io(String),                                       // I/O error reading a config or secret file
    MissingKey(String),                               // get(key) found no such key
    MissingKeys(Vec<String>),                         // require(..): every missing key
    Codec(CodecError),                                // extract(): the root does not bind (From<CodecError>)
    Parse { path: String, message: String },          // a config file is malformed
    InvalidValue { key: String, source: CodecError }, // get/extract: the value at `key` does not convert
    Interpolation { key: String, reason: String },    // a ${…} placeholder failed
    Schema(Vec<String>),                              // validate_schema(..): every violation
    InvalidProfile(String),                           // merge_profile: profile is not a plain name
    UnknownKeys(Vec<UnknownKey>),                     // build_checked(): every unknown key
    Secret { reference: String, reason: String },     // SecretRef::resolve failed
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
    // 4. mounted secrets (Kubernetes / Docker), if present
    .merge_secret_dir_optional("/run/secrets")?
    // 5. command-line flags, parsed with `ferroly::cli` (highest priority):
    //    .merge_cli(&matches)
    // 6. expand ${…} placeholders across the merged result
    .interpolate()?
    .build();
```

### `Layer` — the stack as a list

The same stack can be written as data: an ordered list of `Layer`s given to
`Config::from_layers` (or to `ConfigBuilder::layers`, which continues an existing
chain). The list is applied front to back, so **the order of the list is the
priority order** — lowest first, highest last — and it sits in one place instead of
being spread over a chain of calls:

```rust
use ferroly::config::{Config, Layer};
use ferroly::codec::Value;

let profile = std::env::var("APP_PROFILE").unwrap_or_default();
let cfg = Config::from_layers([
    Layer::Value(Value::Object(vec![                 // 1. compiled-in defaults
        ("port".into(), Value::Str("8080".into())),
    ])),
    Layer::profile("config/app", profile),           // 2. file + per-environment overlay
    Layer::optional_file("config/local.toml"),       //    a developer's local overrides
    Layer::env("APP_"),                              // 3. environment variables
    Layer::optional_secret_dir("/run/secrets"),      // 4. mounted secrets
    // Layer::Cli(matches),                          // 5. parsed command line (`cli` feature)
])?;

// Placeholders are expanded after the layers, as with the chain:
let cfg = Config::builder()
    .layers([Layer::file("config/app.toml"), Layer::env("APP_")])?
    .interpolate()?
    .build();
```

Each variant is exactly one builder call — same behaviour, same errors, same
[provenance](#provenance):

| `Layer` | Constructor | Equivalent call |
|---|---|---|
| `Value(Value)` | `Layer::value(v)` | `merge_value(v)` |
| `File(PathBuf)` | `Layer::file(path)` | `merge_file(path)?` |
| `OptionalFile(PathBuf)` | `Layer::optional_file(path)` | `merge_file_optional(path)?` |
| `Profile { base, profile }` | `Layer::profile(base, profile)` | `merge_profile(base, &profile)?` |
| `Env(String)` | `Layer::env(prefix)` | `merge_env(&prefix)` |
| `EnvWith { prefix, options }` | `Layer::env_with(prefix, options)` | `merge_env_with(&prefix, &options)?` |
| `SecretDir(PathBuf)` | `Layer::secret_dir(dir)` | `merge_secret_dir(dir)?` |
| `OptionalSecretDir(PathBuf)` | `Layer::optional_secret_dir(dir)` | `merge_secret_dir_optional(dir)?` |
| `Args(Vec<String>)` | `Layer::args(args)` | `merge_args(args)` |
| `ArgsWith { args, options }` | `Layer::args_with(args, options)` | `merge_args_with(args, options)` |
| `Cli(Matches)` (*`cli` feature*) | — | `merge_cli(&matches)` |
| `CliWith { matches, options }` (*`cli` feature*) | — | `merge_cli_with(&matches, options)` |
| `CliDefaults(Matches)` (*`cli` feature*) | — | `merge_cli_defaults(&matches)` |

- Applying stops at the **first** layer that fails and returns its error unchanged;
  the layers after it are not read.
- `layers` mixes freely with the `merge_*` calls before and after it; an empty list
  changes nothing.
- `Layer` is `Clone`, so a stack can be kept and re-applied, e.g. in a
  [`SharedConfig`](#sharing-and-reloading-sharedconfig) loader. Its `Debug` impl
  shows the layer kind, paths and prefixes but never a value: `Value`, `Args`,
  `Cli` and `CliDefaults` payloads print as `<redacted>`.
- The enum is `#[non_exhaustive]`: new variants may be added alongside new
  builder methods.

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
kept as strings (trimmed); the codec coerces them when you `extract`/`get`. A key
with an empty segment, or with more than 128 segments, is skipped.

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
- So is a variable whose path has **more than 128 segments** — the nesting depth
  the codec allows a config file. The same limit applies to keys from flags
  (`merge_args*`, `merge_cli*`), `.properties` files and secret directories: a
  deeper key is ignored rather than merged, which would overflow the stack.
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

The content is read as UTF-8 (at most 1 MiB) and **every** trailing `\n` and `\r`
is removed — the rule this option has always had. Secret directories, `file:`
references and `${file:…}` placeholders remove only **one** trailing `\n` or `\r\n`,
so a deliberate trailing blank line in a secret survives there.
The cap counts the bytes actually read, so a pipe or a file that grows while it is
read is cut off and refused as well — the same rule as for secret directories,
`file:` references and `${file:…}` placeholders. If both `APP_DB__PASSWORD` and `APP_DB__PASSWORD_FILE` are set, the file wins. A
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

### `merge_secret_dir` — mounted secret volumes

Kubernetes secret volumes and Docker (Swarm / Compose) secrets mount **one file per
key** in a directory. `merge_secret_dir(dir)` reads such a directory:

```text
/etc/secrets/db.password      →  db.password = <content>
/etc/secrets/api__token       →  api.token   = <content>
/etc/secrets/..data/…         →  skipped (hidden: Kubernetes' own bookkeeping)
```

```rust
use ferroly::config::{Config, Layer};

let cfg = Config::from_layers([
    Layer::file("config/app.yaml"),
    Layer::optional_secret_dir("/run/secrets"),     // Docker
    Layer::optional_secret_dir("/etc/app-secrets"), // a Kubernetes secret volume
])?;
# Ok::<(), ferroly::config::ConfigError>(())
```

- **The file name is the key**, case kept; `.` and `__` both nest. Names starting with
  `.` are skipped, and so is anything that is not a regular file once symlinks are
  followed — Kubernetes exposes each key as a symlink into its hidden `..data`
  directory, which is followed; subdirectories and dangling links are skipped.
- **One revision per load.** When the directory has a `..data` entry (a Kubernetes
  volume), it is resolved once and every key is listed and read from that revision,
  so a load during an atomic update never mixes an old username with a new password.
  If the revision is swapped out and removed mid-read, the load starts over on the
  new one (up to three attempts).
- **The value is the content with one trailing newline removed** (`\n` or `\r\n`,
  as `echo` and `kubectl create secret --from-file` leave it); a second newline is
  kept. It must be UTF-8 and at most 1 MiB (counted on the bytes read).
- Values are **always masked** by `to_redacted_string`, and their
  [provenance](#provenance) is `Source::SecretDir(path)` — the visible path in the
  directory, not the revision directory.
- A missing directory is an error for `merge_secret_dir` and skipped by
  `merge_secret_dir_optional`; an unreadable directory, or an unreadable, oversized or
  non-UTF-8 file, is `ConfigError::Io` naming the file, never its content.
- Files are read in name order, so a key spelled two ways (`a.b` and `a__b`) resolves
  the same way every time — the later name wins. A file whose key is also the prefix
  of another file's key (`db` next to `db.password`) is a `ConfigError::Io` naming
  both files, since one of the two values would be lost.

### `merge_args_with` — raw command-line flags

`merge_args_with(args, options)` parses an iterator of arguments (typically
`std::env::args().skip(1)`) into a flags layer. Only `--`-prefixed tokens are read;
anything else is skipped. A bare `--` ends processing — the tokens after it are
positionals and are never read as flags. Keys nest on `.`, a key with an empty
segment or more than 128 segments is ignored, and the [source](#provenance) is
`Source::Args`. The only question
is `--key value`: is `value` the key's value, or a positional after a switch?
`ArgsOptions` answers it.

| `ArgsOptions` method | Effect |
|---|---|
| `ArgsOptions::new()` / `default()` | no extras: the layers merged earlier decide — identical to `merge_args` |
| `types(&schema)` | take each key's type (bool, list or other) from `schema` instead of the earlier layers |
| `value_follows()` | a key of unknown type takes the next token as its value unless it starts with `-`; pair it with `types` (an unset `Option` is `null` in the schema) |

`Debug` on `ArgsOptions` never prints the schema.

**With `ArgsOptions::default().types(&schema).value_follows()`** (the recommended
recipe) the key types come from `schema` (typically `AppConfig::default().encode()`),
so the flags layer needs no earlier layer of the same builder: it can be built,
checked (unknown keys per layer) or shown on its own, and merged anywhere. Add
`value_follows()` because an encoded default holds `null` for every unset `Option`
field, whose type is therefore unknown: with `types` alone, `--engine.limit 5` on an
`Option<u32>` would be a switch (`limit = true`, and `5` dropped).

```rust
use ferroly::codec::Encode;
use ferroly::config::{ArgsOptions, Config};

let schema = AppConfig::default().encode();
let flags = Config::builder()
    .merge_args_with(
        ["--engine.threads", "16", "--engine.deterministic", "--model.context", "-1"],
        ArgsOptions::default().types(&schema).value_follows(),
    )
    .build();
assert_eq!(flags.get::<u32>("engine.threads")?, 16);
assert!(flags.get::<bool>("engine.deterministic")?);
assert_eq!(flags.get::<i64>("model.context")?, -1);
```

| Schema value at the key | `--key value` |
|---|---|
| a boolean | a switch: `--key` → `true`; `--key true` / `--key false` read the word (those two only, so `--key 1 file` keeps `1` a positional); any other next token is left alone. `--key=value` reads `true`/`false`, `1`/`0`, `yes`/`no`, `on`/`off` (as a declared `cli` switch does) |
| any other scalar | the next token is the value, **even one starting with a single `-`** (`--model.context -1`), but never one starting with `--` (another flag, or a bare `--`): write `--key=--x` for that. With no value the key merges as `true`, which then fails to convert |
| a list | as a scalar, and **every occurrence is collected**: `--tags a --tags=b` merges `["a", "b"]`; one element per occurrence (a comma is part of the value) |
| absent, `null` (an unset `Option`) or a section | unknown: a switch, unless `value_follows()` |

Switches merge as booleans, values as strings (converted by `get` / `extract` as
usual). `--key=value` always sets the value.

**`ArgsOptions::value_follows()`** (opt-in, the rule before 0.4.0): a key whose type
is unknown takes the next token as its value when it does not start with `-`;
otherwise it is a switch. It combines with `types` or works on its own.

**With `ArgsOptions::default()`** the layers merged *before* this call decide, exactly
as `merge_args` does (below); `merge_args(args)` and `Layer::args(args)` are the
shorthand for `merge_args_with(args, ArgsOptions::default())` and
`Layer::args_with(args, ArgsOptions::default())`. Nothing changes for code that does
not pass the new options. When the program declares its interface with
[`ferroly::cli`](cli.md), prefer [`merge_cli`](#merge_cli--parsed-command-lines):
values are validated and `--help` works, and [dynamic flags](cli.md#dynamic-flags)
cover the `--<section>.<key>` family without pulling them out of `argv` first.

#### The earlier-layers rule (`ArgsOptions::default()`, `merge_args`)

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

So merge your defaults first, write `--key=value` (always unambiguous), or pass
`types(&schema)`. Merged last, flags take the highest precedence.

### `merge_cli` — parsed command lines

*Requires the `cli` feature in addition to `config`.* Raw `argv` needs typing;
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
- The [dynamic flags](cli.md#dynamic-flags) (`Matches::dynamic()`, which already
  spans every subcommand level) are merged too, after the declared arguments and in
  command-line order: the flag name is the key, a value merges as a string, a switch
  as `true`. Source: `Source::Cli`.
- `merge_cli_with(&m, ArgsOptions::default().types(&schema))` types the dynamic flags:
  a dynamic flag naming a boolean key merges its value as a boolean (`true`/`false`,
  `1`/`0`, `yes`/`no`, `on`/`off`), and the occurrences of a list key collect into one
  list (`--tags a --tags b` → `["a", "b"]`). The parser has already decided which
  flags carry a value, so `value_follows` has no effect here. `merge_cli(&m)` is
  `merge_cli_with(&m, ArgsOptions::default())`.
- To have the **parser** read each dynamic flag by its type, build the command with
  `dynamic_flags_with(ferroly::config::dynamic_arity(&schema))`: a boolean key is a
  `Switch` (it never takes the next positional: `app --engine.deterministic file.txt`
  keeps `file.txt`), any other scalar or list a `Value` (always takes the next token,
  `--model.context -1` included), a `null` key `Optional`, and a section or a key the
  schema does not have is refused (`UnknownFlag`), so a typo fails at parse time. With
  plain `dynamic_flags` a switch directly before a positional takes it as its value
  (see [the limitation](cli.md#dynamic-flags)). To accept the keys of a map section
  (whose default is empty) too, chain a fallback:
  `move |n| arity(n).or_else(|| n.starts_with("labels.").then_some(DynamicArity::Value))`.

```rust
use ferroly::cli::{Arg, Command};
use ferroly::codec::Encode;
use ferroly::config::{dynamic_arity, ArgsOptions, Config};

let schema = AppConfig::default().encode();
let cmd = Command::new("app")
    .flag(Arg::new("config").long("config"))
    .dynamic_flags_with(dynamic_arity(&schema))
    .subcommand(Command::new("serve"));

// app --engine.threads 4 serve --server.port=9000 --engine.deterministic
let matches = cmd.parse_env_or_exit();
let config = Config::builder()
    .merge_value(schema.clone())
    .merge_file_optional("app.yaml")?
    .merge_cli_with(&matches, ArgsOptions::default().types(&schema))
    .build();
```

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

Two prefixed forms skip the config-key lookup:

| Syntax | Meaning |
|---|---|
| `${env:NAME}` | only the environment variable `NAME` |
| `${file:/path}` | the content of a file: UTF-8, at most 1 MiB, **one** trailing `\n` or `\r\n` removed; a missing file takes the `:-default` if there is one |

`${file:…}` is for a secret embedded in a larger value, such as a DSN:

```yaml
db:
  conn: "host=db user=app password=${file:/run/secrets/db}"
```

A value built from a `${file:…}` or `${env:…}` placeholder — or one that copies
such a value, or a value from a secret file, through `${key}` (however the key is
spelled, and in this `interpolate` call or a later one) — is recorded as
**secret-derived**, and `to_redacted_string` always masks it, whatever its key is
called. (A plain `${NAME}` that falls through to an environment variable is not
masked: `${HOME}` is rarely a secret.)

#### Trusted layers: `InterpolateOptions`

`${env:…}` and `${file:…}` read the process environment and the file system, so they
are expanded **only in values from trusted layers**: config files, value layers
(code), `*_FILE` secret files and secrets directories. In a value that came from an
environment variable or a command-line argument they are a `ConfigError::Interpolation`
naming the key and the layer — never read, so whoever can set a variable or pass an
argument cannot pull `/etc/passwd` into the configuration:

```text
APP_SERVER__HOST='${file:/etc/passwd}'
→ cannot interpolate 'server.host': '${file:…}' is not expanded in a value from env APP_SERVER__HOST; opt that layer in with InterpolateOptions
```

A `${key}` reference is expanded as the **referenced** key's layer allows: a config
file value may not launder an environment value holding `${file:…}`, and an
environment value may copy a file value that read a secret (the copy is masked). A
`:-default` is expanded with the layer of the text it is in. Plain `${NAME}`
placeholders work in every layer, as before these forms existed.

To allow the forms in more layers — say a deployment that sets
`APP_DB__URL='postgres://app:${file:/run/secrets/pw}@db/app'` — opt them in:

```rust
use ferroly::config::{Config, InterpolateOptions};

let config = Config::builder()
    .merge_file_optional("app.yaml")?
    .merge_env("APP_")
    .interpolate_with(&InterpolateOptions::new().trust_env())?   // .trust_args() for Args / Cli
    .build();
```

`trust_args()` covers raw arguments and `cli` matches, `cli` argument defaults
included.

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

`extract` reports a value that does not convert the same way, with the path the
codec recorded ([`CodecError::path_string`](codec.md)): `key` is the full path
(`server.api_keys[0]`, `pools[1]["my pool"].threads`) and `source` is the cause
without it (`CodecError::root_cause`), so the path is printed once. A section bound
with `get::<Section>("db")` names the path below it joined with `db`. Only an error
about the root itself — it is not an object, or a top-level field is missing — has
no path and stays `ConfigError::Codec`.

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

## Unknown keys: `schema` and `build_checked`

A misspelled key is otherwise silently ignored: `engine.thread: 8` merges, the real
`engine.threads` keeps its default, and the process starts wrong. Give the builder
a **schema** and every layer merged after it is checked *as it is merged*, so each
unknown key is reported with the layer it came from and that layer's own spelling:

```rust
use ferroly::codec::Encode;
use ferroly::config::{Config, ConfigError};

let schema = AppConfig::default().encode();   // valid keys come from the struct

let config = Config::builder()
    .schema(schema)                           // turns on per-layer checking
    .merge_value(AppConfig::default().encode())
    .merge_file_optional("app.yaml")?
    .merge_env("APP_")
    .build_checked();

match config {
    Ok(config) => { /* … */ }
    Err(ConfigError::UnknownKeys(keys)) => {
        for k in &keys {
            eprintln!("{} from {}", k.spelled, k.source);
        }
        std::process::exit(2);
    }
    Err(other) => return Err(other.into()),
}
```

```text
unknown config keys: `serve.port` in /etc/app.yaml (did you mean `server.port`?), `APP_ENGINE__THREAD` (did you mean `APP_ENGINE__THREADS`?), `--engine.thread` (did you mean `--engine.threads`?)
```

**The schema** is any `Value` shaped like the configuration; `T::default().encode()`
is the convenient source, so the keys cannot drift from the struct.

- An object is a section; its entries are the valid keys (matched exactly).
- A scalar or a list is a leaf: it accepts any scalar or list value, but **no keys
  below it** — `--server.port.x=1` or `APP_PORT__X=4` against `port: 8080` is
  unknown, so it cannot replace the valid key.
- `null` (an `Option` defaulting to `None`) and an object with **no entries**
  (free-form maps) accept any keys below them.
- A value that is not an object where the schema has a section (`APP_ENGINE=3`
  against an `engine` section) is not an unknown key: it is merged and left for
  decoding to accept (an enum variant, `null` for an `Option`) or to report. A field
  whose default encodes as a scalar but which can also hold keys (an enum whose
  default is a unit variant) needs `null` or `{}` in the schema.
- A field that only decodes through an `alias`, or that is left out of the encoding
  (`skip`, a `None` under `skip_none`), must be added to the schema by hand.

**Each unknown key** is removed from its layer before the layer is merged — so it
can never shadow or partly overwrite a valid key — and recorded as an `UnknownKey`:

| Field | Meaning |
|---|---|
| `path` | full dotted path, `engine.thread` (each leaf of an unknown section is listed) |
| `spelled` | as the source writes it: `engine.thread` (file, value), `APP_ENGINE__THREAD` (env), `--engine.thread` (raw args, dynamic flags), the argument name (a declared `cli` argument, whose flag may be spelled differently), the file path (secrets directory) |
| `source` | the layer's [`Source`](#provenance) |
| `suggestion` | the nearest valid key or section, spelled the same way, or `None` |
| `dotted` | the key is one literal name with dots (`"engine.threads": 9`) that spells a valid nested key; the message says to nest it |

The suggestion is the schema path (sections included — a misspelled section is as
likely as a misspelled key) with the smallest Levenshtein distance from the whole
dotted path, accepted within **one edit per four characters**, at least 1 and at
most 3: `engine.thread` → `engine.threads`, `serve.port` → `server.port`, `zzz` →
none. Its cost is bounded, so a huge or hostile layer cannot stall startup: no
suggestion for a path over 256 characters, only the first 4096 schema paths are
offered, candidates whose length is out of reach are skipped, and once a builder has
spent its budget (a few million edit-distance steps) later keys are still reported
but get no suggestion. The error message lists the first 50 keys and counts the
rest (`…, and 950 more`), and cuts a key longer than 200 characters; the
`UnknownKeys` vector holds them all.

**Environment variables** under the prefix whose first segment is not a top-level
key of the schema (`APP_LOG`, the variables secret references point at) are not
configuration: they are dropped without being reported, and with `secret_files()`
their file is never read. Unknown keys inside a known section are reported (the file
of an unknown `*_FILE` variable is not read either).

**Declared `cli` arguments** (`merge_cli`, `merge_cli_with`, `merge_cli_defaults`)
follow the same rule: one whose first segment is not a top-level schema key
(`--config`, `--verbose`, a `--log-level` default) is part of the interface, not
configuration, and is left out without being reported — read it from the `Matches`.
One inside a schema section is checked. Raw arguments (`merge_args`) and dynamic
flags are all configuration and always checked.

**Outcome.** `build_checked()` fails with `ConfigError::UnknownKeys` listing every
unknown key of every layer, in merge order. After `lenient()` it succeeds and
`Config::ignored_keys()` lists them for the caller to log:

```rust
let config = Config::builder().schema(schema).lenient()
    .merge_file_optional("app.yaml")?.build_checked()?;
for k in config.ignored_keys() {
    log::warn!("ignoring unknown config key {}", k.spelled);
}
```

`build()` never fails: with a schema it drops unknown keys exactly as lenient mode
does. **Without a schema nothing is checked and `build()` is unchanged.** Layers
merged before `schema(..)` are not checked.

## Config file discovery: `search`

When no config file is named, most tools look in the current directory and then in
the user's config directory. `ferroly::config::search` does that search:

```rust
use ferroly::config::{search, Config};

let names = ["app.yaml", "app.toml", "app.json"];
let found = search::find_first(&search::default_dirs("app"), &names);

let config = Config::builder()
    .merge_value(AppConfig::default().encode())
    .merge_found(&search::default_dirs("app"), &names)?   // or nothing, if none exists
    .merge_env("APP_")
    .build();
println!("loaded from {:?}", config.sources().find(|(_, s)| s.is_file()));
```

| Function | Result |
|---|---|
| `user_config_dir(app) -> Option<PathBuf>` | the per-user config directory (below) |
| `default_dirs(app) -> Vec<PathBuf>` | the current directory, then `user_config_dir(app)`; undeterminable ones left out |
| `find_first(dirs, names) -> Option<PathBuf>` | the first existing **regular file** among `dirs × names` |
| `macos_native(app) -> Option<PathBuf>` | `~/Library/Application Support/<app>`, for macOS apps that prefer it |
| `user_config_dir_from(app, xdg, home, appdata)` | `user_config_dir` with the variables passed in |
| `user_config_dir_for(platform, app, xdg, home, appdata)` | the same for an explicit `Platform` (`Xdg`, `Windows`) |
| `default_dirs_from(cwd, user)`, `macos_native_from(app, home)` | the other injectable cores |

`user_config_dir` follows the platform:

- **Linux, BSD, macOS** (`Platform::Xdg`): `$XDG_CONFIG_HOME/<app>` if that is set,
  non-empty and absolute — as the XDG Base Directory spec requires — else
  `$HOME/.config/<app>`. An empty or relative `XDG_CONFIG_HOME` is ignored; with
  `HOME` unset, empty or relative there is no user directory.
- **Windows** (`Platform::Windows`): `%APPDATA%\<app>`; none when `APPDATA` is unset,
  empty or not an absolute path (`C:\…`, `\\server\share\…`).

`app` must be one plain file name: an empty name, `.`, `..`, an absolute path or one
containing `/` or `\` (`"../../etc"`) gives no directory at all.

The `_from` / `_for` variants take the environment as arguments, so every platform's
rule is testable anywhere without touching the process environment.

`find_first` searches **directories in order, then names in order within each
directory**, so `./app.toml` outranks `~/.config/app/app.yaml`. A directory named
like a config file is skipped (symlinks are followed), and so is a directory that
does not exist. The file is not opened: a file that exists but is unreadable or
malformed is returned and fails when it is loaded, never falling through to a later
candidate. The same goes for a candidate that is there but cannot be checked — a
dangling symlink, or one inside a directory that may not be searched. `names` must be
relative (`app.yaml`, `conf/app.yaml`); an absolute name would replace the directory
and is skipped.

`merge_found(&dirs, &names)` merges the file `find_first` returns (or nothing) with
`merge_file`, so it is recorded in provenance as `Source::File(path)` and shows in
`source`, `sources` and `to_redacted_string`. `Source::is_file()` tells file layers
apart.

## Secret references: `SecretRef`

*Features `config` + `hash`.* A configuration file should never hold a secret, but
often has to say **where** one is. Declare such a field as `SecretRef`:

```rust
use ferroly::codec::{Decode, Encode};
use ferroly::config::{Config, SecretRef};
use ferroly::hash::Secret;

#[derive(Decode, Encode, Default)]
struct Server { api_keys: Vec<SecretRef>, host: String }

// app.yaml:
// server:
//   api_keys: ["env:APP_KEY", "file:/run/secrets/key2"]
let cfg: AppConfig = Config::builder().merge_file("app.yaml")?.build().extract()?;

let keys: Vec<Secret> = cfg.server.api_keys.iter().map(SecretRef::resolve).collect::<Result<_, _>>()?;
assert!(keys[0].ct_eq(presented_token.as_bytes()));
```

- **Decoding** accepts `env:NAME` (a non-empty variable name without `=`) and
  `file:/path` (absolute, or relative to the process's current directory); both
  prefixes are case-sensitive. `Vec<SecretRef>` and `Option<SecretRef>` work as
  usual, and `SecretRef` encodes back to the same string. Anything else is refused
  without printing it — at most its first two characters (none below eight
  characters) and its length:

  ```text
  secret must be an env:NAME or file:/path reference, not a literal (got "sk…", 14 chars)
  ```

  Through `extract` (or `get` on a section) the error is `ConfigError::InvalidValue`
  naming the field and the index:

  ```text
  invalid value for 'server.api_keys[0]': secret must be an env:NAME or file:/path reference, not a literal (got "sk…", 14 chars)
  ```

- **`resolve()`** returns a `hash::Secret` (redacting `Debug`/`Display`, wiped on
  drop): the variable's value, or the whole file — UTF-8, at most 1 MiB, **one**
  trailing `\n` or `\r\n` removed. An empty secret is allowed; check
  `Secret::is_empty`. Errors are `ConfigError::Secret` and name the reference only:
  `secret env:APP_KEY: variable is not set`,
  `secret file:/run/secrets/k: permission denied`.
- **Constructors and parsing:** `SecretRef::env(name)`, `SecretRef::file(path)`,
  `SecretRef::parse(text)` / `text.parse::<SecretRef>()`. A reference is not
  secret: `Debug` and `Display` show it.
- **Redaction.** `to_redacted_string` shows a value that is a well-formed reference
  **verbatim**, even under a sensitive key, and masks everything else by the usual
  rules. "Well-formed" is stricter than decoding, so a secret that merely starts with
  a prefix is not shown: `env:NAME` needs `NAME` to be ASCII letters, digits and `_`
  (not starting with a digit), and `file:PATH` needs `PATH` free of whitespace and
  control characters (`password = "env:x y z real secret"` is masked; so is a
  `file:` reference whose path has a space).
  `redact_literals(["server.api_keys"])` masks every entry under those keys that is
  *not* a reference, whatever the key is called:

  ```text
  server.api_keys[0] = "env:APP_KEY"  # file app.yaml
  server.api_keys[1] = "file:/run/secrets/key2"  # file app.yaml
  server.api_keys[2] = <redacted>  # file app.yaml
  ```

For a secret embedded in a larger string (a DSN), use
[`${file:/path}` interpolation](#interpolate--var-placeholders); the result is masked.

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
| `SecretDir(path)` | `merge_secret_dir` / `merge_secret_dir_optional` | `secret file <path>` |
| `Args` | `merge_args_with`, `merge_args` | `args` |
| `Cli` | `merge_cli`, `merge_cli_with`, `merge_cli_defaults` | `cli` |

`merge_found` records `File(path)` like `merge_file`; `Source::is_file()` is true
for exactly that variant.

Keys are full dotted paths to leaf values; a list counts as one leaf, and a key
naming a whole section has no source. A [`Layer`](#layer--the-stack-as-a-list)
records the same source as its builder call (`Layer::File` → `File(path)`, …).

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
  secret file or embeds a `${file:…}` or `${env:…}` placeholder's content, (c) it
  is a URL carrying credentials (`scheme://user:pass@host`), or (d) it sits under a
  key given to `redact_literals` and is not a secret reference. The heuristic
  deliberately masks too much rather than too little. A value that **is** a
  well-formed secret reference (`env:NAME`, `file:/path`, see
  [`SecretRef`](#secret-references-secretref)) names a source rather than a secret
  and is shown even under a key rule (a) flags — unless it was itself read from a
  secret file.

  `to_redacted_string_with(|path| …)` replaces rule (a) with your own predicate
  (a masked section masks everything below it), and that predicate is final:
  everything under a key it flags is masked, references included. The other rules
  always apply, and references are still shown under a `redact_literals` key the
  predicate does not flag. Combine it with the default via
  `ferroly::config::is_sensitive_key`.
- **Secrets as values**: [`SecretRef`](#secret-references-secretref) resolves to a
  `hash::Secret`, whose `Debug` and `Display` print `[REDACTED]`.

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
| `watch_files(&self, paths, interval, on_reload) -> Result<ConfigWatcher, ConfigError>` | poll files and reload when one changes ([change detection](#change-detection)) |
| `reload_on(&self, signal, on_reload) -> JoinHandle<()>` | reload on the lifecycle reload handle (*`lifecycle` feature*) |

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
- **Reloads never overlap.** `reload()` from a watcher, the lifecycle signal and your
  own code take turns: one that starts while another runs waits for it, so the
  configuration installed last is always the one read last.
- Without a loader (`SharedConfig::new`), `reload()` returns the current snapshot
  unchanged; use `replace`.

### Change detection

Two opt-in triggers call `reload()` for you. Both hand every outcome to a callback —
the new snapshot, or the loader's error while the previous configuration stays.

**Polling files: `watch_files`.** A background thread checks the listed paths every
`interval` (at least 10 ms) and reloads when one appears, disappears, or changes size,
modification time or content (the content of a file up to 1 MiB is hashed as well, so
an edit inside the filesystem's timestamp granularity, or a Kubernetes ConfigMap's
symlink swap, is still seen; a larger file is compared by size and modification time
only). List the files your loader reads; the watcher stops when the returned
`ConfigWatcher` is dropped, and `ConfigWatcher::stop()` also waits for an in-progress
reload. `watch_files` returns `ConfigError::Io` if its thread cannot be started.

- **A change must settle first.** The watcher reloads only once the files look the
  same on two polls in a row, so a file caught half written (an editor or `cp` that
  truncates, then writes) is not loaded while it is still being written. A writer
  slower than one interval can still be caught mid-write, so **replace files
  atomically** (write a temporary file, then rename it over the old one) where you can.
  Expect a reload one to two intervals after the change.
- **Changes before the call count.** If a watched file was modified at or after the
  last load started (between `with_loader` and `watch_files`, say), the first poll
  reloads. A file deleted in that window is only noticed with the next change.

```rust,no_run
use std::time::Duration;
use ferroly::config::{Config, SharedConfig};

let shared = SharedConfig::with_loader(|| {
    Ok(Config::builder().merge_file("config/app.yaml")?.build())
})?;
let _watcher = shared.watch_files(["config/app.yaml"], Duration::from_secs(2), |result| {
    if let Err(e) = result {
        eprintln!("config reload failed, keeping the old one: {e}");
    }
})?;
# Ok::<(), ferroly::config::ConfigError>(())
```

**The lifecycle reload handle: `reload_on`** (*`lifecycle` feature*). Pass
`ComponentManager::subscribe_reload()`: every `trigger_reload()` — and `SIGHUP` with
`ManagerOptions::reload_on_sighup` — reruns the loader on Tokio's blocking pool. The
task ends when the manager is dropped.

```rust,no_run
use ferroly::config::{Config, SharedConfig};
use ferroly::lifecycle::{ComponentManager, ManagerOptions};

# async fn ex() -> Result<(), ferroly::config::ConfigError> {
let mgr = ComponentManager::with_options(ManagerOptions::new().reload_on_sighup(true));
let shared = SharedConfig::with_loader(|| {
    Ok(Config::builder().merge_file("config/app.yaml")?.build())
})?;
shared.reload_on(mgr.subscribe_reload(), |result| {
    if let Err(e) = result {
        eprintln!("config reload failed, keeping the old one: {e}");
    }
});
# Ok(())
# }
```

Nothing pushes the new configuration into running code: readers see it on their next
`snapshot()`, and code that needs to react can compare `generation()`.

## Worked example: a full layered stack bound to a struct

*Uses the `cli` feature for the command line.*

```rust
use ferroly::cli::{Arg, Command};
use ferroly::config::Config;
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

fn load() -> Result<Cfg, ferroly::Error> {
    let matches = Command::new("app")
        .flag(Arg::new("db.port").long("db-port"))
        .flag(Arg::new("verbose").long("verbose").boolean())
        .parse(std::env::args().skip(1))?;

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
        // mounted secrets, if any: /run/secrets/db.password
        .merge_secret_dir_optional("/run/secrets")?
        // final overrides via flags: --db-port 6000 --verbose
        .merge_cli(&matches)
        .build();

    cfg.require(["db.host", "db.port"])?;
    Ok(cfg.extract()?)
}
```

With `APP_DB__HOST=prod-db` set and `--db-port 6000 --verbose` on the command line,
the result is `Cfg { db: Db { host: "prod-db", port: 6000 }, verbose: true }` — the
default `host` was replaced by the env layer and `port` by the command line, while
deep-merge keeps each sibling from the layer that last set it. Flags that were not
given are left out by `merge_cli`, so they never override the layers below.

## Error handling

- `from_file` / `merge_file` / `merge_file_optional` / `merge_profile` return `Err`
  eagerly: `UnsupportedFormat` for an unknown extension (checked before any disk
  access), `Io` for a read failure, and `Parse` for a malformed document.
- `merge_env`, `merge_args_with`, `merge_args`, `merge_cli`, `merge_cli_with` and
  `merge_value` are infallible — they
  return `Self` directly. `from_layers` / `layers` return the error of the first
  failing layer. `merge_env_with` fails only for an unreadable secret
  file; `merge_secret_dir` with `Io` for a missing (unless optional) or unreadable
  directory or an unreadable, oversized or non-UTF-8 file; `interpolate` fails with
  `Interpolation`.
- `get` returns `MissingKey` when the path does not resolve and `InvalidValue`
  when the value cannot be coerced to the target type; `extract` returns
  `InvalidValue` naming the full path of the value that does not convert, or
  `Codec` for an error about the root itself.
- `require` returns `MissingKeys`; `validate_schema` returns `Schema`.

## Limitations

- **No INI support**, and no remote or secret-store sources — files, mounted secret
  directories, environment and arguments only.
- **Change detection polls.** `watch_files` compares size, modification time and a
  content hash (files up to 1 MiB) every interval (no inotify / kqueue), waits one
  interval for a change to settle, and watches only the paths you list; `reload_on`
  follows the lifecycle reload signal.
- **Arrays replace, never merge.** A later layer's array wholly replaces an earlier
  one; there is no append or index-merge.
- **Env / properties / flag values are strings.** Rely on codec coercion during
  `extract`/`get`; there is no per-source typing.
- **`.env` files are read as properties**, not with dotenv semantics: no `export`
  prefix, no quote removal, no inline comments.
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

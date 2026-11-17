//! Layered configuration loading.
//!
//! Loads configuration from environment variables, config files and
//! command-line flags, with later layers overriding earlier ones.
//! Built entirely on `ferroly::codec` — no external configuration crate.
//!
//! - File formats are inferred from the extension: `.json`, `.yaml`/`.yml`,
//!   `.toml`, `.xml`, `.properties`/`.props`/`.env`.
//!   [`ConfigBuilder::formats`] restricts a builder to the [`FileFormat`]s an
//!   application supports.
//! - Environment variables are matched by prefix; `__` denotes nesting
//!   (`APP_DB__HOST` → `db.host`).
//! - Values deep-merge; later layers win.
//!
//! ```no_run
//! use ferroly::config::Config;
//! use ferroly::codec::Decode;
//!
//! #[derive(Decode)]
//! struct Settings { port: u16, host: String }
//!
//! let cfg = Config::builder()
//!     .merge_file("config.json").unwrap()
//!     .merge_env("APP_")
//!     .build();
//!
//! let settings: Settings = cfg.extract().unwrap();
//! ```
//!
//! The same stack can be written as an ordered list of [`Layer`]s with
//! [`Config::from_layers`] or [`ConfigBuilder::layers`], which keeps the
//! priority order in one place.
//!
//! Beyond the basic layers the builder offers optional files
//! ([`merge_file_optional`](ConfigBuilder::merge_file_optional)), profile
//! overlays ([`merge_profile`](ConfigBuilder::merge_profile)), `${VAR}`
//! interpolation ([`interpolate`](ConfigBuilder::interpolate)) and tunable
//! environment handling ([`EnvOptions`]: secrets from files, lists, key-case
//! matching). A built [`Config`] can check required keys
//! ([`require`](Config::require)), validate against a schema
//! ([`validate_schema`](Config::validate_schema)), report where each value
//! came from ([`source`](Config::source)) and print itself with secrets masked
//! ([`to_redacted_string`](Config::to_redacted_string)). [`SharedConfig`] shares
//! one configuration between threads and reloads it on demand, when watched
//! files change, or on the lifecycle reload signal. Mounted secret volumes
//! load with [`merge_secret_dir`](ConfigBuilder::merge_secret_dir).
//!
//! Three further pieces cover what most services re-implement:
//!
//! - **Unknown keys.** Give the builder a [`schema`](ConfigBuilder::schema)
//!   (usually `AppConfig::default().encode()`) and every layer is checked as it
//!   is merged; [`build_checked`](ConfigBuilder::build_checked) reports each
//!   misspelled key with its source, its own spelling and a "did you mean"
//!   suggestion ([`UnknownKey`]).
//! - **File discovery.** [`search`] finds the first config file in the current
//!   directory or the user's config directory, and
//!   [`merge_found`](ConfigBuilder::merge_found) merges it.
//! - **Secret references.** A config field of type `SecretRef` (feature
//!   `hash`) holds `env:NAME` or `file:/path`, refuses literals, and resolves to
//!   a redacting `hash::Secret`; the redacted dump shows references verbatim.

#![deny(missing_docs)]

mod args;
mod interpolate;
mod layer;
pub mod search;
mod secret;
mod shared;
mod source;
mod unknown;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use ferroly::codec::{find, schema, CodecError, Decode, Value};

#[cfg(feature = "cli")]
pub use args::dynamic_arity;
pub use args::ArgsOptions;
pub use interpolate::InterpolateOptions;
pub use layer::Layer;
#[cfg(feature = "hash")]
pub use secret::SecretRef;
pub use shared::{ConfigWatcher, SharedConfig};
pub use source::FileFormat;
pub use unknown::UnknownKey;

use source::{Setting, Tree};

/// Errors raised while loading or extracting configuration.
///
/// No variant ever carries a configuration *value*: messages name files, keys
/// and variables only, so they are safe to log.
#[derive(Debug)]
#[non_exhaustive]
pub enum ConfigError {
    /// A file's extension is not a recognized config format. The payload is
    /// the path; the message also lists the supported extensions.
    UnsupportedFormat(String),

    /// A file's extension is a format ferroly reads, but not one this builder
    /// accepts ([`ConfigBuilder::formats`]).
    FormatNotAllowed {
        /// The file, as given.
        path: String,
        /// The accepted extensions, with the dot, in probing order
        /// (`[".json", ".yaml", ".yml", ".toml"]`); empty when no format is
        /// accepted.
        allowed: Vec<String>,
    },

    /// An I/O error reading a config or secret file. The message starts with
    /// the path.
    Io(String),

    /// The requested key was not present.
    MissingKey(String),

    /// One or more keys demanded by [`Config::require`] are absent; every
    /// missing key is listed by its full dotted path.
    MissingKeys(Vec<String>),

    /// A codec (decode) error while binding the whole configuration with
    /// [`Config::extract`] that is about the root itself (it is not an
    /// object, or a top-level field is missing); an error further down the
    /// tree is [`InvalidValue`](ConfigError::InvalidValue). Also what
    /// `From<CodecError>` builds.
    Codec(CodecError),

    /// A config file could not be parsed.
    Parse {
        /// The file that failed to parse.
        path: String,
        /// The parser's message, with any text it copied from the file
        /// replaced by `<redacted>`.
        message: String,
    },

    /// The value at `key` could not be converted to the requested type, by
    /// [`Config::get`] or [`Config::extract`].
    InvalidValue {
        /// Full path of the offending value, dotted with `[n]` for a list
        /// index (`server.api_keys[0]`).
        key: String,
        /// Why the conversion failed. Its path, if any, is already in `key`.
        source: CodecError,
    },

    /// A `${…}` placeholder could not be expanded.
    Interpolation {
        /// Full dotted path of the value containing the placeholder.
        key: String,
        /// What went wrong (an unset name, a cycle, …).
        reason: String,
    },

    /// The configuration violates the schema given to
    /// [`Config::validate_schema`]; one entry per violation.
    Schema(Vec<String>),

    /// A profile name given to [`ConfigBuilder::merge_profile`] is not a plain
    /// name (it may only contain letters, digits, `_`, `-` and `.`).
    InvalidProfile(String),

    /// Keys that the [schema](ConfigBuilder::schema) does not know, from every
    /// layer, reported by [`ConfigBuilder::build_checked`]. The message lists
    /// the first 50 and counts the rest; the vector holds them all. It reads
    /// `unknown config key: …` for one key and `unknown config keys: …`
    /// otherwise.
    UnknownKeys(Vec<UnknownKey>),

    /// A secret reference (`env:NAME` / `file:/path`) could not be resolved.
    /// Neither field ever holds the secret itself.
    Secret {
        /// The reference as written (`env:APP_KEY`, `file:/run/secrets/k`).
        reference: String,
        /// Why it could not be resolved (`variable is not set`, …).
        reason: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::UnsupportedFormat(path) => {
                let known: Vec<String> =
                    source::EXTENSIONS.iter().map(|e| format!(".{e}")).collect();
                write!(
                    f,
                    "unsupported config file format for '{path}' (expected one of {})",
                    known.join(", ")
                )
            }
            ConfigError::FormatNotAllowed { path, allowed } => {
                if allowed.is_empty() {
                    write!(
                        f,
                        "config file format of '{path}' is not accepted (no file formats are accepted)"
                    )
                } else {
                    write!(
                        f,
                        "config file format of '{path}' is not accepted (expected one of {})",
                        allowed.join(", ")
                    )
                }
            }
            ConfigError::Io(m) => write!(f, "io error: {m}"),
            ConfigError::MissingKey(k) => write!(f, "missing key: {k}"),
            ConfigError::MissingKeys(keys) => {
                write!(f, "missing required keys: {}", keys.join(", "))
            }
            ConfigError::Codec(e) => fmt::Display::fmt(e, f),
            ConfigError::Parse { path, message } => {
                write!(f, "invalid config file '{path}': {message}")
            }
            ConfigError::InvalidValue { key, source } => {
                write!(f, "invalid value for '{key}': {source}")
            }
            ConfigError::Interpolation { key, reason } => {
                write!(f, "cannot interpolate '{key}': {reason}")
            }
            ConfigError::Schema(violations) => {
                write!(f, "configuration does not match the schema: ")?;
                f.write_str(&violations.join("; "))
            }
            ConfigError::InvalidProfile(p) => write!(f, "invalid profile name '{p}'"),
            ConfigError::UnknownKeys(keys) => {
                // Enough to act on; the variant holds every key.
                const SHOWN: usize = 50;
                f.write_str(if keys.len() == 1 {
                    "unknown config key: "
                } else {
                    "unknown config keys: "
                })?;
                for (i, key) in keys.iter().take(SHOWN).enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    fmt::Display::fmt(key, f)?;
                }
                if keys.len() > SHOWN {
                    write!(f, ", and {} more", keys.len() - SHOWN)?;
                }
                Ok(())
            }
            ConfigError::Secret { reference, reason } => {
                write!(f, "secret {reference}: {reason}")
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Codec(e) => Some(e),
            ConfigError::InvalidValue { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<CodecError> for ConfigError {
    fn from(e: CodecError) -> Self {
        ConfigError::Codec(e)
    }
}

#[cfg(feature = "errutils")]
impl ferroly::errutils::Classify for ConfigError {
    /// A configuration problem is the operator's input
    /// ([`InvalidInput`](ferroly::errutils::ErrorKind::InvalidInput)), except an
    /// unknown or not accepted file format
    /// ([`Unsupported`](ferroly::errutils::ErrorKind::Unsupported))
    /// and I/O failures, whose cause is not known here
    /// ([`Unknown`](ferroly::errutils::ErrorKind::Unknown)).
    fn kind(&self) -> ferroly::errutils::ErrorKind {
        use ferroly::errutils::ErrorKind;
        match self {
            ConfigError::UnsupportedFormat(_) | ConfigError::FormatNotAllowed { .. } => {
                ErrorKind::Unsupported
            }
            ConfigError::Io(_) => ErrorKind::Unknown,
            _ => ErrorKind::InvalidInput,
        }
    }
}

/// Where a configuration value came from; see [`Config::source`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Source {
    /// An explicit [`ConfigBuilder::merge_value`] layer.
    Value,
    /// A config file (the path as given).
    File(String),
    /// An environment variable (its name).
    Env(String),
    /// A file named by a `*_FILE` environment variable
    /// ([`EnvOptions::secret_files`]).
    SecretFile {
        /// The environment variable that pointed at the file.
        var: String,
        /// The file the value was read from.
        path: String,
    },
    /// A file of a mounted secrets directory
    /// ([`ConfigBuilder::merge_secret_dir`]); the payload is the file's path.
    SecretDir(String),
    /// A raw `--key=value` argument (`ConfigBuilder::merge_args_with`, or
    /// its shorthand `ConfigBuilder::merge_args`).
    Args,
    /// A parsed command-line argument or dynamic flag
    /// (`ConfigBuilder::merge_cli`, `ConfigBuilder::merge_cli_with`).
    Cli,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::Value => f.write_str("value"),
            Source::File(path) => write!(f, "file {path}"),
            Source::Env(var) => write!(f, "env {var}"),
            Source::SecretFile { var, path } => write!(f, "secret file {path} (env {var})"),
            Source::SecretDir(path) => write!(f, "secret file {path}"),
            Source::Args => f.write_str("args"),
            Source::Cli => f.write_str("cli"),
        }
    }
}

impl Source {
    /// Whether the value came from a config file ([`Source::File`]).
    pub fn is_file(&self) -> bool {
        matches!(self, Source::File(_))
    }
}

/// Opt-in behaviours for [`ConfigBuilder::merge_env_with`].
///
/// ```
/// use ferroly::config::EnvOptions;
///
/// let opts = EnvOptions::new()
///     .secret_files()                    // APP_DB__PASSWORD_FILE=/run/secrets/pw
///     .list_keys(',', ["allowed_hosts"]) // APP_ALLOWED_HOSTS=a,b → ["a", "b"]
///     .case_insensitive();               // APP_MAX_CONNECTIONS → maxConnections
/// # let _ = opts;
/// ```
#[derive(Debug, Clone, Default)]
pub struct EnvOptions {
    secret_files: bool,
    split_all: Option<char>,
    list: Option<(char, Vec<String>)>,
    preserve_case: bool,
    match_keys: bool,
}

impl EnvOptions {
    /// The default behaviour — identical to [`ConfigBuilder::merge_env`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Enables the `*_FILE` convention used for container secrets: a variable
    /// `APP_DB__PASSWORD_FILE=/run/secrets/pw` sets `db.password` to the
    /// content of that file (minus its trailing newline). If both
    /// `APP_DB__PASSWORD` and `APP_DB__PASSWORD_FILE` are set, the file wins.
    /// An unreadable file is an error. Values loaded this way are always
    /// masked by [`Config::to_redacted_string`].
    pub fn secret_files(mut self) -> Self {
        self.secret_files = true;
        self
    }

    /// Turns the listed (dotted) keys into lists by splitting their value on
    /// `separator`: `APP_TAGS=a, b` → `["a", "b"]`. Items are trimmed and empty
    /// items dropped, so a single item gives a one-element list and an empty
    /// value an empty list.
    pub fn list_keys<I, S>(mut self, separator: char, keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.list = Some((separator, keys.into_iter().map(Into::into).collect()));
        self
    }

    /// Splits **every** value that contains `separator` into a list. Simpler
    /// than [`list_keys`](EnvOptions::list_keys), but any value that merely
    /// happens to contain the separator (a sentence, a connection string)
    /// becomes a list too — prefer `list_keys` when such values exist.
    pub fn split_all(mut self, separator: char) -> Self {
        self.split_all = Some(separator);
        self
    }

    /// Keeps the case of variable names instead of lowercasing them, so
    /// `APP_maxConnections` maps to `maxConnections`.
    pub fn preserve_case(mut self) -> Self {
        self.preserve_case = true;
        self
    }

    /// Matches each key segment against the keys already present in the
    /// earlier layers, ignoring case and `_`/`-`, and adopts the existing
    /// spelling: with a file defining `maxConnections`, the variable
    /// `APP_MAX_CONNECTIONS` overrides it instead of creating a separate
    /// `max_connections` key. Segments with no existing counterpart are
    /// lowercased as usual.
    pub fn case_insensitive(mut self) -> Self {
        self.match_keys = true;
        self
    }
}

/// A resolved, layered configuration backed by a merged [`Value`].
#[derive(Clone)]
pub struct Config {
    value: Value,
    /// Leaf path → the layer that last set it.
    origins: BTreeMap<String, Source>,
    /// Keys the schema did not know, left out of the merge.
    ignored: Vec<UnknownKey>,
    /// Leaf paths (in the dump's spelling) whose value embeds a secret
    /// through interpolation; always masked.
    secret_derived: BTreeSet<String>,
    /// Keys under which anything but a secret reference is masked.
    literal_keys: Vec<String>,
}

impl Default for Config {
    /// Same as [`Config::new`] — an empty object (not `Value::Null`).
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Config {
    /// Redacts values so configuration secrets (tokens, passwords, connection
    /// strings) can't leak into logs or panic messages via `{:?}`. Only the
    /// top-level key names are shown.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_redacted(f, "Config", &self.value)
    }
}

/// The shared `Debug` body of [`Config`] and [`ConfigBuilder`].
fn fmt_redacted(f: &mut fmt::Formatter<'_>, name: &str, value: &Value) -> fmt::Result {
    let keys: Vec<&str> = match value {
        Value::Object(o) => o.iter().map(|(k, _)| k.as_str()).collect(),
        _ => Vec::new(),
    };
    f.debug_struct(name)
        .field("keys", &keys)
        .field("values", &"<redacted>")
        .finish()
}

impl Config {
    /// Creates an empty configuration.
    pub fn new() -> Self {
        Self {
            value: Value::Object(Vec::new()),
            origins: BTreeMap::new(),
            ignored: Vec::new(),
            secret_derived: BTreeSet::new(),
            literal_keys: Vec::new(),
        }
    }

    /// Starts a [`ConfigBuilder`] for composing multiple layers.
    pub fn builder() -> ConfigBuilder {
        ConfigBuilder::default()
    }

    /// Loads configuration solely from environment variables with the given
    /// prefix (e.g. `"APP_"` maps `APP_PORT` to `port`, `APP_DB__HOST` to
    /// `db.host`).
    pub fn from_env(prefix: &str) -> Self {
        Self::builder().merge_env(prefix).build()
    }

    /// Loads configuration solely from a file, format inferred from extension.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        Ok(Self::builder().merge_file(path)?.build())
    }

    /// Extracts the whole configuration into a typed value.
    ///
    /// A value inside the tree that does not convert is
    /// [`ConfigError::InvalidValue`] naming its full path (`server.api_keys[0]`)
    /// with the codec's error, without the path, as `source`. An error about
    /// the root itself (it is not an object, or a top-level field is missing)
    /// is [`ConfigError::Codec`].
    ///
    /// ```
    /// use ferroly::codec::{json, Decode};
    /// use ferroly::config::{Config, ConfigError};
    ///
    /// #[derive(Decode, Debug)]
    /// struct Pool { threads: u32 }
    /// #[derive(Decode, Debug)]
    /// struct App { pool: Pool }
    ///
    /// let cfg = Config::builder()
    ///     .merge_value(json::from_str(r#"{"pool":{"threads":"many"}}"#).unwrap())
    ///     .build();
    /// let err = cfg.extract::<App>().unwrap_err();
    /// assert!(matches!(&err, ConfigError::InvalidValue { key, .. } if key == "pool.threads"));
    /// ```
    pub fn extract<T: Decode>(&self) -> Result<T, ConfigError> {
        T::decode(&self.value).map_err(|e| invalid_value("", e))
    }

    /// Extracts a single (dot-pathed) key into a typed value.
    ///
    /// A missing key is [`ConfigError::MissingKey`]; a value that does not
    /// convert is [`ConfigError::InvalidValue`] naming the full path of the
    /// offending value: `key` itself, or a path below it when `key` is a
    /// section (`get::<Server>("server")` reports `server.api_keys[0]`).
    pub fn get<T: Decode>(&self, key: &str) -> Result<T, ConfigError> {
        let v =
            navigate(&self.value, key).ok_or_else(|| ConfigError::MissingKey(key.to_string()))?;
        T::decode(v).map_err(|e| match invalid_value(key, e) {
            // Raised at `key` itself.
            ConfigError::Codec(source) => ConfigError::InvalidValue {
                key: key.to_string(),
                source,
            },
            other => other,
        })
    }

    /// Whether a (dot-pathed) key is present with a non-null value.
    pub fn contains(&self, key: &str) -> bool {
        navigate(&self.value, key).is_some_and(|v| !v.is_null())
    }

    /// Checks that every listed (dot-pathed) key is present and non-null.
    ///
    /// Unlike a chain of [`get`](Config::get) calls this reports **all**
    /// missing keys at once, in [`ConfigError::MissingKeys`], so an operator
    /// fixes a deployment in one round instead of one key per restart.
    ///
    /// ```
    /// use ferroly::codec::Value;
    /// use ferroly::config::{Config, ConfigError};
    ///
    /// let cfg = Config::builder()
    ///     .merge_value(Value::Object(vec![("port".into(), Value::Int(80))]))
    ///     .build();
    ///
    /// match cfg.require(["port", "db.host", "db.password"]) {
    ///     Err(ConfigError::MissingKeys(keys)) => assert_eq!(keys, ["db.host", "db.password"]),
    ///     other => panic!("unexpected: {other:?}"),
    /// }
    /// ```
    pub fn require<I, S>(&self, keys: I) -> Result<(), ConfigError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let missing: Vec<String> = keys
            .into_iter()
            .filter(|k| !self.contains(k.as_ref()))
            .map(|k| k.as_ref().to_string())
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::MissingKeys(missing))
        }
    }

    /// Validates the configuration against a JSON-Schema subset (see
    /// [`ferroly::codec::schema`]), reporting every violation in
    /// [`ConfigError::Schema`].
    ///
    /// Values from the environment, `.properties` files and command-line flags
    /// are strings; before checking, a string is read as the number or boolean
    /// the schema asks for at that position (`"8080"` satisfies
    /// `"type": "integer"`), mirroring what [`extract`](Config::extract) does.
    pub fn validate_schema(&self, schema: &Value) -> Result<(), ConfigError> {
        let data = coerce(schema, &self.value);
        schema::validate(schema, &data).map_err(|violations| {
            ConfigError::Schema(violations.iter().map(|v| v.to_string()).collect())
        })
    }

    /// Returns the merged underlying [`Value`].
    pub fn value(&self) -> &Value {
        &self.value
    }

    /// The layer that supplied the value at `key` (a full dotted path to a
    /// leaf value; a list counts as one leaf). `None` for an unknown key or a
    /// key naming a whole section.
    pub fn source(&self, key: &str) -> Option<&Source> {
        self.origins.get(key)
    }

    /// Every leaf key with the layer that supplied it, sorted by key.
    pub fn sources(&self) -> impl Iterator<Item = (&str, &Source)> {
        self.origins.iter().map(|(k, s)| (k.as_str(), s))
    }

    /// Renders the effective configuration, one `key = value  # source` line
    /// per leaf, with sensitive values replaced by `<redacted>`.
    ///
    /// A value is masked when its key path looks sensitive
    /// ([`is_sensitive_key`]), when it was read from a secret file, when it
    /// embeds the content of a `${file:…}` or `${env:…}` placeholder, or
    /// when it is a URL carrying credentials (`scheme://user:pass@host`).
    ///
    /// A string that is a well-formed secret *reference* names where a
    /// secret lives, not the secret, so it is shown verbatim even under a
    /// sensitive key: `env:NAME` where `NAME` is ASCII letters, digits and
    /// `_` (not starting with a digit), or `file:PATH` where `PATH` has no
    /// whitespace or control characters. Anything else that merely starts
    /// with `env:` or `file:` is masked like any other value. Under a key
    /// passed to [`redact_literals`](Config::redact_literals) everything that
    /// is not a reference is masked.
    ///
    /// Use [`to_redacted_string_with`](Config::to_redacted_string_with) to
    /// supply your own notion of a sensitive key.
    pub fn to_redacted_string(&self) -> String {
        self.dump(&is_sensitive_key, true)
    }

    /// Like [`to_redacted_string`](Config::to_redacted_string), but `sensitive`
    /// decides which dotted key paths are masked (a masked section masks
    /// everything below it), and its decision is final: every value under a
    /// key it flags is masked, secret references included. Secret-file
    /// values, interpolated `${file:…}` / `${env:…}` content and
    /// credential-bearing URLs are masked regardless. References are still
    /// shown under a [`redact_literals`](Config::redact_literals) key that
    /// `sensitive` does not flag.
    pub fn to_redacted_string_with<F>(&self, sensitive: F) -> String
    where
        F: Fn(&str) -> bool,
    {
        self.dump(&sensitive, false)
    }

    /// The redacted dump; `references` shows secret references under a
    /// sensitive key (the built-in rule only).
    fn dump(&self, sensitive: &dyn Fn(&str) -> bool, references: bool) -> String {
        let mut out = String::new();
        let cx = Dump {
            sensitive,
            references,
            origins: &self.origins,
            secret_derived: &self.secret_derived,
            literal_keys: &self.literal_keys,
        };
        cx.dump(&self.value, "", Mask::default(), None, &mut out);
        out
    }

    /// Wraps this configuration in a [`SharedConfig`] handle.
    pub fn into_shared(self) -> SharedConfig {
        SharedConfig::new(self)
    }
}

/// The default notion of a sensitive key for
/// [`Config::to_redacted_string`]: the (case-insensitive) dotted path contains
/// one of `password`, `passwd`, `passphrase`, `pwd`, `secret`, `token`, `key`,
/// `credential`, `auth`, `private`, `cert`, `signature`, `salt`, `cookie`,
/// `session` or `dsn`. It errs on the side of masking too much.
pub fn is_sensitive_key(path: &str) -> bool {
    const MARKERS: [&str; 16] = [
        "password",
        "passwd",
        "passphrase",
        "pwd",
        "secret",
        "token",
        "key",
        "credential",
        "auth",
        "private",
        "cert",
        "signature",
        "salt",
        "cookie",
        "session",
        "dsn",
    ];
    let lower = path.to_ascii_lowercase();
    MARKERS.iter().any(|m| lower.contains(m))
}

/// `scheme://user:pass@host…` — a URL with a userinfo part.
fn has_url_credentials(text: &str) -> bool {
    text.find("://").is_some_and(|i| {
        let authority = text[i + 3..].split(['/', '?', '#']).next().unwrap_or("");
        authority.contains('@')
    })
}

/// What [`Config::to_redacted_string_with`] consults for each leaf.
struct Dump<'a> {
    sensitive: &'a dyn Fn(&str) -> bool,
    /// Whether a secret reference is shown under a sensitive key.
    references: bool,
    origins: &'a BTreeMap<String, Source>,
    secret_derived: &'a BTreeSet<String>,
    literal_keys: &'a [String],
}

/// The masking rules inherited from the enclosing keys.
#[derive(Clone, Copy, Default)]
struct Mask {
    /// Under a sensitive key.
    sensitive: bool,
    /// Under a [`Config::redact_literals`] key.
    literals: bool,
}

impl Dump<'_> {
    fn dump(
        &self,
        value: &Value,
        path: &str,
        mask: Mask,
        origin: Option<&Source>,
        out: &mut String,
    ) {
        match value {
            Value::Object(entries) if !entries.is_empty() => {
                for (k, v) in entries {
                    let child = join(path, k);
                    let mask = Mask {
                        sensitive: mask.sensitive || (self.sensitive)(&child),
                        literals: mask.literals || self.literal_keys.contains(&child),
                    };
                    let origin = self.origins.get(&child).or(origin);
                    self.dump(v, &child, mask, origin, out);
                }
            }
            Value::Array(items) if !items.is_empty() => {
                for (i, v) in items.iter().enumerate() {
                    let child = format!("{path}[{i}]");
                    self.dump(v, &child, mask, origin, out);
                }
            }
            leaf => {
                let from_secret = matches!(
                    origin,
                    Some(Source::SecretFile { .. } | Source::SecretDir(_))
                ) || self.secret_derived.contains(path);
                // A reference names where a secret lives, not the secret, so
                // it is shown under a sensitive key too — unless the text was
                // itself read from a secret source.
                let reference = leaf.as_str().is_some_and(secret::is_reference);
                let masked = from_secret
                    || (mask.sensitive && !(reference && self.references))
                    || (mask.literals && !reference)
                    || leaf.as_str().is_some_and(has_url_credentials);
                let shown = if masked {
                    "<redacted>".to_string()
                } else {
                    match leaf {
                        Value::Null => "null".to_string(),
                        Value::Bool(b) => b.to_string(),
                        Value::Int(i) => i.to_string(),
                        Value::UInt(u) => u.to_string(),
                        Value::Float(x) => x.to_string(),
                        Value::Str(s) => format!("{s:?}"),
                        Value::Bytes(b) => format!("<{} bytes>", b.len()),
                        Value::Array(_) => "[]".to_string(),
                        Value::Object(_) => "{}".to_string(),
                    }
                };
                out.push_str(path);
                out.push_str(" = ");
                out.push_str(&shown);
                if let Some(origin) = origin {
                    out.push_str(&format!("  # {origin}"));
                }
                out.push('\n');
            }
        }
    }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

/// Calls `f` with the dotted path of every leaf of `value` (anything that is
/// not a non-empty object).
fn leaves(value: &Value, path: &str, f: &mut dyn FnMut(&str)) {
    match value {
        Value::Object(entries) if !entries.is_empty() => {
            for (k, v) in entries {
                leaves(v, &join(path, k), f);
            }
        }
        _ => f(path),
    }
}

/// Reads string scalars as the type `schema` expects at each position, so
/// string-typed sources can be validated against a typed schema.
fn coerce(schema: &Value, data: &Value) -> Value {
    let ty = schema.get("type").and_then(Value::as_str);
    match (ty, data) {
        (Some("integer"), Value::Str(s)) => parse_integer(s).unwrap_or_else(|| data.clone()),
        (Some("number"), Value::Str(s)) => parse_integer(s)
            .or_else(|| {
                s.trim()
                    .parse::<f64>()
                    .ok()
                    .filter(|x| x.is_finite())
                    .map(Value::Float)
            })
            .unwrap_or_else(|| data.clone()),
        (Some("boolean"), Value::Str(s)) => match s.trim() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => data.clone(),
        },
        (_, Value::Object(entries)) => match schema.get("properties") {
            Some(Value::Object(props)) => Value::Object(
                entries
                    .iter()
                    .map(|(k, v)| {
                        let v = find(props, k).map_or_else(|| v.clone(), |sub| coerce(sub, v));
                        (k.clone(), v)
                    })
                    .collect(),
            ),
            _ => data.clone(),
        },
        (_, Value::Array(items)) => match schema.get("items") {
            Some(sub) => Value::Array(items.iter().map(|v| coerce(sub, v)).collect()),
            None => data.clone(),
        },
        _ => data.clone(),
    }
}

fn parse_integer(s: &str) -> Option<Value> {
    let s = s.trim();
    s.parse::<i64>()
        .map(Value::Int)
        .or_else(|_| s.parse::<u64>().map(Value::UInt))
        .ok()
}

/// Builder for composing configuration layers in priority order.
#[must_use]
pub struct ConfigBuilder {
    value: Value,
    origins: BTreeMap<String, Source>,
    /// Unknown-key checking ([`ConfigBuilder::schema`]).
    check: unknown::Check,
    /// See [`Config::secret_derived`].
    secret_derived: BTreeSet<String>,
    /// The file formats accepted ([`ConfigBuilder::formats`]); `None` accepts
    /// every format.
    formats: Option<Vec<FileFormat>>,
}

impl Default for ConfigBuilder {
    /// An empty builder — the same as [`Config::builder`].
    fn default() -> Self {
        Self {
            value: Value::Object(Vec::new()),
            origins: BTreeMap::new(),
            check: unknown::Check::default(),
            secret_derived: BTreeSet::new(),
            formats: None,
        }
    }
}

impl fmt::Debug for ConfigBuilder {
    /// Redacted exactly like [`Config`]'s `Debug`: the layers merged so far may
    /// already hold secrets, so only the top-level key names are shown.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_redacted(f, "ConfigBuilder", &self.value)
    }
}

impl ConfigBuilder {
    /// Merges a whole layer that came from one place.
    fn apply(mut self, layer: Value, source: Source) -> Self {
        let layer = self.check.screen_value(layer, &source);
        // An empty layer (an empty file, a layer that screened to nothing)
        // sets nothing, so it is the origin of nothing.
        if !matches!(&layer, Value::Object(entries) if entries.is_empty()) {
            let origins = &mut self.origins;
            leaves(&layer, "", &mut |path| {
                origins.insert(path.to_string(), source.clone());
            });
        }
        self.value = merge(std::mem::take(&mut self.value), layer);
        self
    }

    /// Merges individual settings, each with its own origin.
    fn apply_settings(self, settings: Vec<Setting>) -> Self {
        self.apply_screened(settings, &unknown::Screen::All)
    }

    /// [`apply_settings`](ConfigBuilder::apply_settings), screening the
    /// settings as `screen` says.
    fn apply_screened(mut self, settings: Vec<Setting>, screen: &unknown::Screen<'_>) -> Self {
        let settings = self.check.screen_settings(settings, screen);
        self.merge_settings(settings)
    }

    /// [`apply_settings`](ConfigBuilder::apply_settings) for settings already
    /// screened against the schema.
    fn merge_settings(mut self, settings: Vec<Setting>) -> Self {
        let mut tree = Tree::default();
        for s in settings {
            self.origins.insert(s.segs.join("."), s.source);
            tree.insert(&s.segs, s.value);
        }
        self.value = merge(std::mem::take(&mut self.value), tree.into_value());
        self
    }

    /// Merges environment variables with the given prefix.
    ///
    /// The prefix is stripped, the rest lowercased and split on `__` into a
    /// key path. Variables that are not valid UTF-8, and variables whose path
    /// would contain an empty segment (`APP_A____B`) or more than 128
    /// segments (the codec's nesting limit), are ignored.
    ///
    /// With a [schema](ConfigBuilder::schema), a variable whose path names a
    /// section rather than a key (`APP_MODEL` against a `model` section) is
    /// not configuration: it is skipped without being reported, in strict
    /// and lenient mode alike, so it can never replace the section.
    /// `APP_MODEL__PATH` still sets `model.path`. A field whose default
    /// encodes as a section (an enum whose default is a data-carrying
    /// variant) therefore cannot be set to a unit variant from the
    /// environment; set it from a file, a value layer or arguments.
    ///
    /// To test environment handling without touching the process
    /// environment, use [`merge_env_from`](ConfigBuilder::merge_env_from).
    pub fn merge_env(self, prefix: &str) -> Self {
        self.merge_env_from(prefix, std::env::vars_os())
    }

    /// Merges environment variables with the given prefix, with the extra
    /// behaviours selected in `options` (secrets from files, lists, key-case
    /// matching). Fails only when a secret file cannot be read.
    ///
    /// As with [`merge_env`](ConfigBuilder::merge_env), a variable naming a
    /// schema section rather than a key is skipped without being reported;
    /// with [`EnvOptions::secret_files`] the file an `APP_MODEL_FILE`
    /// variable points at is then never read.
    ///
    /// To test environment handling without touching the process
    /// environment, use
    /// [`merge_env_from_with`](ConfigBuilder::merge_env_from_with).
    pub fn merge_env_with(self, prefix: &str, options: &EnvOptions) -> Result<Self, ConfigError> {
        self.merge_env_from_with(prefix, std::env::vars_os(), options)
    }

    /// Like [`merge_env`](ConfigBuilder::merge_env), but reads the variables
    /// from `vars` instead of the process environment, so a test needs no
    /// `std::env::set_var` and cannot race other tests.
    ///
    /// Everything else is the same: prefix stripping, lowercasing, `__`
    /// nesting, ignored keys, application in sorted name order (`APP_DB__HOST`
    /// beats `APP_DB`), schema screening and provenance ([`Source::Env`]).
    /// The process environment is not consulted at all.
    ///
    /// - A name or value that is not valid UTF-8 is skipped.
    /// - A name given more than once keeps its **last** value in input order,
    ///   as if the variables had been set one after another. If that last
    ///   value is not valid UTF-8, the variable is skipped, even when an
    ///   earlier value was valid: the process environment would hold only the
    ///   last value, which `merge_env` skips.
    ///
    /// `vars` takes anything yielding pairs of `&str`, `String`, `&OsStr` or
    /// `OsString`: an array, a `Vec`, a `HashMap`, or `std::env::vars_os()`
    /// (which is what `merge_env` passes).
    ///
    /// ```
    /// use ferroly::config::{Config, Source};
    ///
    /// let cfg = Config::builder()
    ///     .merge_env_from("APP_", [("APP_PORT", "8080"), ("APP_DB__HOST", "db"), ("HOME", "/x")])
    ///     .build();
    /// assert_eq!(cfg.get::<u16>("port")?, 8080);
    /// assert_eq!(cfg.get::<String>("db.host")?, "db");
    /// assert!(!cfg.contains("home"));
    /// assert_eq!(cfg.source("port"), Some(&Source::Env("APP_PORT".into())));
    /// # Ok::<(), ferroly::config::ConfigError>(())
    /// ```
    pub fn merge_env_from<I, K, V>(self, prefix: &str, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        // Without secret files there is nothing fallible to do.
        let options = EnvOptions::default();
        let vars = source::collect_vars(vars);
        let classify = |segs: &[String]| self.check.classify_env(segs);
        let settings =
            source::read_env(prefix, vars, &options, &self.value, &classify).unwrap_or_default();
        self.apply_env(prefix, &options, settings)
    }

    /// Like [`merge_env_with`](ConfigBuilder::merge_env_with), but reads the
    /// variables from `vars` instead of the process environment; see
    /// [`merge_env_from`](ConfigBuilder::merge_env_from) for how `vars` is
    /// read (non-UTF-8 pairs skipped, the last of a repeated name wins, and a
    /// repeated name whose last value is not valid UTF-8 is skipped even when
    /// an earlier value was valid).
    ///
    /// Only the variables are injected: with
    /// [`EnvOptions::secret_files`], the value of a `*_FILE` variable is
    /// still a path read from the real filesystem, with the same precedence,
    /// errors, masking and [`Source::SecretFile`] provenance.
    ///
    /// ```
    /// use ferroly::config::{Config, EnvOptions, Source};
    ///
    /// let path = std::env::temp_dir().join(format!("ferroly-doc-env-from-{}", std::process::id()));
    /// std::fs::write(&path, "s3cret\n").unwrap();
    /// let file = path.display().to_string();
    ///
    /// let cfg = Config::builder()
    ///     .merge_env_from_with(
    ///         "APP_",
    ///         [("APP_DB__PASSWORD", "from-env"), ("APP_DB__PASSWORD_FILE", file.as_str())],
    ///         &EnvOptions::new().secret_files(),
    ///     )?
    ///     .build();
    /// assert_eq!(cfg.get::<String>("db.password")?, "s3cret");
    /// assert!(matches!(cfg.source("db.password"), Some(Source::SecretFile { .. })));
    /// # std::fs::remove_file(&path).ok();
    /// # Ok::<(), ferroly::config::ConfigError>(())
    /// ```
    pub fn merge_env_from_with<I, K, V>(
        self,
        prefix: &str,
        vars: I,
        options: &EnvOptions,
    ) -> Result<Self, ConfigError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        let vars = source::collect_vars(vars);
        let classify = |segs: &[String]| self.check.classify_env(segs);
        let settings = source::read_env(prefix, vars, options, &self.value, &classify)?;
        Ok(self.apply_env(prefix, options, settings))
    }

    /// Merges settings read from the environment with `prefix`, screening
    /// them against the schema with environment spelling.
    fn apply_env(self, prefix: &str, options: &EnvOptions, settings: Vec<Setting>) -> Self {
        let env = unknown::Screen::Env(unknown::EnvSpelling {
            prefix,
            upper: !options.preserve_case,
        });
        self.apply_screened(settings, &env)
    }

    /// Restricts the file formats this builder loads to `formats`; by default
    /// every [`FileFormat`] is accepted.
    ///
    /// Every file-loading call merged **after** this one honours the list:
    /// [`merge_file`](ConfigBuilder::merge_file),
    /// [`merge_file_optional`](ConfigBuilder::merge_file_optional),
    /// [`merge_profile`](ConfigBuilder::merge_profile),
    /// [`merge_found`](ConfigBuilder::merge_found), and the file and profile
    /// [`Layer`]s of [`layers`](ConfigBuilder::layers). Call it first; layers
    /// merged earlier are kept as they are. [`Config::from_file`] and
    /// [`Config::from_layers`] stay unrestricted — write
    /// `Config::builder().formats(..).layers(..)?.build()` instead.
    ///
    /// - A file whose extension is a known format outside the list is
    ///   [`ConfigError::FormatNotAllowed`], decided from the name alone,
    ///   before the filesystem is touched: `merge_file_optional("app.env")`
    ///   fails even when `app.env` does not exist.
    /// - An extension ferroly does not know at all (`app.ini`) is still
    ///   [`ConfigError::UnsupportedFormat`].
    /// - Calling it again replaces the list; order and duplicates do not
    ///   matter. An empty slice accepts no format, so every file-loading call
    ///   then fails with `FormatNotAllowed`.
    ///
    /// ```
    /// use ferroly::config::{Config, ConfigError, FileFormat};
    ///
    /// let dir = std::env::temp_dir().join(format!("ferroly-doc-formats-{}", std::process::id()));
    /// std::fs::create_dir_all(&dir).unwrap();
    /// std::fs::write(dir.join("app.env"), "port=1\n").unwrap();
    /// std::fs::write(dir.join("app.yaml"), "port: 2\n").unwrap();
    ///
    /// let documented = [FileFormat::Yaml, FileFormat::Toml, FileFormat::Json];
    /// let err = Config::builder()
    ///     .formats(&documented)
    ///     .merge_file(dir.join("app.env"))
    ///     .unwrap_err();
    /// assert!(matches!(err, ConfigError::FormatNotAllowed { .. }));
    ///
    /// let cfg = Config::builder()
    ///     .formats(&documented)
    ///     .merge_file(dir.join("app.yaml"))?
    ///     .build();
    /// assert_eq!(cfg.get::<u16>("port")?, 2);
    /// # std::fs::remove_dir_all(&dir).ok();
    /// # Ok::<(), ferroly::config::ConfigError>(())
    /// ```
    pub fn formats(mut self, formats: &[FileFormat]) -> Self {
        let mut list: Vec<FileFormat> = Vec::with_capacity(formats.len());
        for f in formats {
            if !list.contains(f) {
                list.push(*f);
            }
        }
        self.formats = Some(list);
        self
    }

    /// Merges a config file, format inferred from extension. A missing file is
    /// an error; see [`merge_file_optional`](ConfigBuilder::merge_file_optional).
    ///
    /// An unknown extension is [`ConfigError::UnsupportedFormat`]; a known one
    /// that [`formats`](ConfigBuilder::formats) does not accept is
    /// [`ConfigError::FormatNotAllowed`].
    ///
    /// An empty layer, which changes nothing with or without a schema, is
    /// read from a file of any format that is empty or holds only whitespace
    /// (after an optional UTF-8 byte-order mark), a YAML, TOML or properties
    /// file holding only comments, or a JSON file holding `null`.
    pub fn merge_file(self, path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        self.merge_file_impl(path.as_ref(), false).map(|(b, _)| b)
    }

    /// Merges a config file if it exists. A missing file is skipped silently;
    /// an unsupported or not accepted extension
    /// ([`formats`](ConfigBuilder::formats)), an unreadable file or malformed
    /// content is still an error. The extension is checked first, so a
    /// missing file with a not accepted extension is an error too.
    pub fn merge_file_optional(self, path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        self.merge_file_impl(path.as_ref(), true).map(|(b, _)| b)
    }

    /// The extensions [`formats`](ConfigBuilder::formats) accepts, without the
    /// dot, in probing order.
    fn allowed_extensions(&self) -> Vec<&'static str> {
        source::allowed_extensions(self.formats.as_deref())
    }

    /// Returns the builder and whether the file was found.
    fn merge_file_impl(self, path: &Path, optional: bool) -> Result<(Self, bool), ConfigError> {
        match source::load_file(path, optional, self.formats.as_deref())? {
            Some(layer) => Ok((
                self.apply(layer, Source::File(path.display().to_string())),
                true,
            )),
            None => Ok((self, false)),
        }
    }

    /// Merges a base config file and then, if present, its profile overlay:
    /// `merge_profile("config/app", "prod")` loads `config/app.<ext>` and then
    /// `config/app-prod.<ext>`.
    ///
    /// - `base` may carry an extension (`config/app.yaml` — exactly that file
    ///   and `config/app-prod.yaml`) or omit it, in which case every supported
    ///   extension is tried, in the order `.json`, `.yaml`, `.yml`, `.toml`,
    ///   `.xml`, `.properties`, `.props`, `.env`, and every file found is
    ///   merged. With [`formats`](ConfigBuilder::formats), only the accepted
    ///   formats' extensions are tried, so a stray `config/app.env` is never
    ///   read; an explicit extension that is not accepted is
    ///   [`ConfigError::FormatNotAllowed`] before anything is read, and so is
    ///   an extension-less `base` when no format is accepted.
    /// - At least one base file must exist; the overlay is optional.
    /// - An empty `profile` loads only the base. The profile is used as part
    ///   of a file name, so it may only contain letters, digits, `_`, `-` and
    ///   `.` — anything else is [`ConfigError::InvalidProfile`].
    pub fn merge_profile(
        mut self,
        base: impl AsRef<Path>,
        profile: &str,
    ) -> Result<Self, ConfigError> {
        let plain = profile
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
        if !plain || profile.starts_with('.') {
            return Err(ConfigError::InvalidProfile(profile.to_string()));
        }

        let base = base.as_ref();
        let explicit = base
            .extension()
            .and_then(|e| e.to_str())
            .filter(|_| source::has_known_extension(base));
        // The explicit extension is detected against every known format (so
        // `config/app.env` is never probed as `config/app.env.json`) and then
        // checked against the accepted ones.
        let (stem, extensions): (PathBuf, Vec<&str>) = match explicit {
            Some(ext) => {
                source::check_allowed(base, self.formats.as_deref())?;
                (base.with_extension(""), vec![ext])
            }
            None => {
                let extensions = self.allowed_extensions();
                if extensions.is_empty() {
                    return Err(ConfigError::FormatNotAllowed {
                        path: base.display().to_string(),
                        allowed: Vec::new(),
                    });
                }
                (base.to_path_buf(), extensions)
            }
        };

        let mut found = false;
        for ext in &extensions {
            let (next, hit) = self.merge_file_impl(&source::with_extension(&stem, ext), true)?;
            self = next;
            found |= hit;
        }
        if !found {
            let tried: Vec<String> = extensions.iter().map(|e| format!(".{e}")).collect();
            return Err(ConfigError::Io(format!(
                "{}: no config file found (tried {})",
                stem.display(),
                tried.join(", ")
            )));
        }

        if !profile.is_empty() {
            let mut overlay = stem.into_os_string();
            overlay.push("-");
            overlay.push(profile);
            let overlay = PathBuf::from(overlay);
            for ext in &extensions {
                let (next, _) =
                    self.merge_file_impl(&source::with_extension(&overlay, ext), true)?;
                self = next;
            }
        }
        Ok(self)
    }

    /// Merges a mounted secrets directory — one file per key, the layout of
    /// Kubernetes secret volumes and Docker (Swarm/Compose) secrets
    /// (`/run/secrets`):
    ///
    /// ```text
    /// /etc/secrets/db.password      → db.password
    /// /etc/secrets/api__token       → api.token
    /// /etc/secrets/..data/…         (skipped: hidden)
    /// ```
    ///
    /// - The file name is the key, with its case kept; `.` and `__` both nest.
    ///   Names starting with `.` and names with an empty segment (or more
    ///   than 128 segments) are skipped,
    ///   as is anything that is not a regular file after following symlinks
    ///   (Kubernetes exposes each key as a symlink into `..data`).
    /// - The value is the file's content as a string with **one** trailing
    ///   newline (`\n` or `\r\n`) removed — an intentional blank line before
    ///   it survives. It must be UTF-8 and at most 1 MiB.
    /// - Values loaded this way are always masked by
    ///   [`Config::to_redacted_string`]; their [`Source`] is
    ///   [`Source::SecretDir`].
    ///
    /// A missing or unreadable directory, or an unreadable, oversized or
    /// non-UTF-8 file, is [`ConfigError::Io`]; see
    /// [`merge_secret_dir_optional`](ConfigBuilder::merge_secret_dir_optional)
    /// for a directory that may be absent.
    ///
    /// ```
    /// use ferroly::config::{Config, Source};
    ///
    /// let dir = std::env::temp_dir().join(format!("ferroly-doc-secrets-{}", std::process::id()));
    /// std::fs::create_dir_all(&dir).unwrap();
    /// std::fs::write(dir.join("db.password"), "s3cret\n").unwrap();
    ///
    /// let cfg = Config::builder().merge_secret_dir(&dir)?.build();
    /// assert_eq!(cfg.get::<String>("db.password")?, "s3cret");
    /// assert!(matches!(cfg.source("db.password"), Some(Source::SecretDir(_))));
    /// # std::fs::remove_dir_all(&dir).ok();
    /// # Ok::<(), ferroly::config::ConfigError>(())
    /// ```
    pub fn merge_secret_dir(self, dir: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let settings = source::read_secret_dir(dir.as_ref(), false)?.unwrap_or_default();
        Ok(self.apply_settings(settings))
    }

    /// Like [`merge_secret_dir`](ConfigBuilder::merge_secret_dir), but a
    /// missing directory is skipped silently (an unreadable one is still an
    /// error).
    pub fn merge_secret_dir_optional(self, dir: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let settings = source::read_secret_dir(dir.as_ref(), true)?.unwrap_or_default();
        Ok(self.apply_settings(settings))
    }

    /// Merges an explicit [`Value`] layer.
    pub fn merge_value(self, value: Value) -> Self {
        self.apply(value, Source::Value)
    }

    /// Merges command-line flags, nesting keys on `.` (typically pass
    /// `std::env::args().skip(1)`). Highest priority when merged last.
    ///
    /// Only `--`-prefixed tokens are read; everything else is skipped, and a
    /// bare `--` ends processing (the tokens after it are positionals).
    ///
    /// - `--db.port=5432` always sets `db.port` to `"5432"`.
    /// - `--flag value` is ambiguous without knowing whether `flag` takes a
    ///   value, so the layers merged **before** this call decide: if the key
    ///   already holds a non-boolean scalar or a list, the next token is its
    ///   new value; if it holds a boolean (`true`/`false`) or does not exist
    ///   yet, the flag is a switch set to `"true"` and the next token is left
    ///   alone. Merge your defaults first, or write `--key=value`.
    /// - A token starting with `--` is never taken as a value.
    ///
    /// This is shorthand for
    /// [`merge_args_with(args, ArgsOptions::default())`](ConfigBuilder::merge_args_with).
    /// To build the flags layer on its own, declare the key types with
    /// `merge_args_with(args, ArgsOptions::default().types(&schema).value_follows())`;
    /// or declare the interface with `ferroly::cli` (feature `cli`) and merge
    /// the parsed result with `merge_cli`: values are validated and `--help`
    /// works.
    pub fn merge_args<I, S>(self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.merge_args_with(args, ArgsOptions::default())
    }

    /// Merges the arguments of a parsed command line that were given
    /// explicitly — on the command line or through an argument's environment
    /// variable. Values that are only an argument's *default* are left out, so
    /// they cannot override a config file; merge those first with
    /// [`merge_cli_defaults`](ConfigBuilder::merge_cli_defaults).
    ///
    /// The argument name is the config key (dots nest: an argument named
    /// `db.host` sets `db.host`). Switches become booleans, counters integers,
    /// repeatable arguments lists. Only this level of `matches` is read; pass
    /// [`Matches::subcommand`](ferroly::cli::Matches::subcommand)'s matches to
    /// merge a subcommand's arguments.
    ///
    /// The flags accepted by
    /// [`Command::dynamic_flags`](ferroly::cli::Command::dynamic_flags)
    /// ([`Matches::dynamic`](ferroly::cli::Matches::dynamic), which covers
    /// every subcommand level) are merged too, after the declared arguments
    /// and in command-line order: the flag name is the key, a value merges as
    /// a string and a switch as `true`. To type them from a schema use
    /// [`merge_cli_with`](ConfigBuilder::merge_cli_with); this call is
    /// `merge_cli_with(matches, ArgsOptions::default())`.
    ///
    /// With a [schema](ConfigBuilder::schema), a declared argument whose
    /// first segment is not a top-level schema key (`--config`, `--verbose`)
    /// is part of the interface, not configuration: it is left out without
    /// being reported. One inside a schema section is checked, and reported
    /// by its argument name. Dynamic flags are always checked.
    #[cfg(feature = "cli")]
    pub fn merge_cli(self, matches: &ferroly::cli::Matches) -> Self {
        self.merge_cli_with(matches, ArgsOptions::default())
    }

    /// Merges only the *default* values of a parsed command line — the
    /// counterpart of [`merge_cli`](ConfigBuilder::merge_cli), meant to be the
    /// lowest layer.
    #[cfg(feature = "cli")]
    pub fn merge_cli_defaults(self, matches: &ferroly::cli::Matches) -> Self {
        self.merge_cli_where(matches, |s| s == ferroly::cli::ValueSource::Default)
    }

    #[cfg(feature = "cli")]
    fn merge_cli_where(
        self,
        matches: &ferroly::cli::Matches,
        wanted: impl Fn(ferroly::cli::ValueSource) -> bool,
    ) -> Self {
        use ferroly::cli::ArgValue;

        let mut settings = Vec::new();
        for name in matches.names() {
            if !matches.source(name).is_some_and(&wanted) {
                continue;
            }
            let segs: Vec<String> = name.split('.').map(str::to_string).collect();
            if !source::usable_key(&segs) {
                continue;
            }
            let value = match matches.value(name) {
                Some(ArgValue::Bool(b)) => Value::Bool(b),
                Some(ArgValue::Count(n)) => Value::UInt(n as u64),
                Some(ArgValue::One(v)) => Value::Str(v.to_string()),
                Some(ArgValue::Many(vs)) => {
                    Value::Array(vs.iter().map(|v| Value::Str(v.clone())).collect())
                }
                _ => continue,
            };
            settings.push(Setting {
                segs,
                value,
                source: Source::Cli,
            });
        }
        // A declared argument outside the schema's sections (`--config`,
        // `--verbose`) is part of the interface, not configuration.
        self.apply_screened(settings, &unknown::Screen::Declared)
    }

    /// Expands `${NAME}` and `${NAME:-default}` placeholders in every string
    /// value merged so far. Call it after the last `merge_*`.
    ///
    /// - `NAME` is looked up as a (dot-pathed) key of this configuration first,
    ///   then as an environment variable of exactly that name — so
    ///   `${db.host}` and `${HOME}` both work. A key that refers to its own
    ///   name (`PORT: "${PORT}"`) reads the environment variable.
    /// - `${NAME:-default}` uses `default` when `NAME` is unset or empty; the
    ///   default may itself contain placeholders. Without a default an unset
    ///   name is an error.
    /// - `$${` yields a literal `${`.
    /// - Keys referring to each other in a circle are reported as a cycle
    ///   rather than looping.
    ///
    /// - `${env:NAME}` reads only the environment variable `NAME`.
    /// - `${file:/path}` is the content of a file (UTF-8, at most 1 MiB, one
    ///   trailing `\n` or `\r\n` removed), for secrets embedded in a larger
    ///   value such as a DSN; a missing file takes the default if one is
    ///   given.
    /// - A value built with either form — and one that copies such a value,
    ///   or a secret-file value, through `${key}`, in this call or a later
    ///   one — is recorded as secret-derived and always masked by
    ///   [`Config::to_redacted_string`].
    ///
    /// **Trusted layers only.** `${env:…}` and `${file:…}` are expanded only
    /// in values from config files, value layers, `*_FILE` secret files and
    /// secrets directories. In a value set by an environment variable or a
    /// command-line argument they are an error — whoever can set a variable
    /// or pass an argument must not be able to read files into the
    /// configuration. A `${key}` reference is expanded as the referenced
    /// key's own layer allows. Use
    /// [`interpolate_with`](ConfigBuilder::interpolate_with) to trust more
    /// layers.
    ///
    /// Errors are [`ConfigError::Interpolation`] and name the key and the
    /// placeholder, never a value.
    pub fn interpolate(self) -> Result<Self, ConfigError> {
        self.interpolate_with(&InterpolateOptions::new())
    }

    /// [`interpolate`](ConfigBuilder::interpolate), with `options` choosing
    /// which layers may use `${env:…}` and `${file:…}`.
    ///
    /// ```
    /// use ferroly::codec::Value;
    /// use ferroly::config::{Config, InterpolateOptions};
    ///
    /// let args = ["--greeting=${env:CFG_DOC_GREETING}"];
    /// std::env::set_var("CFG_DOC_GREETING", "hello");
    ///
    /// // By default a command-line value may not read the environment …
    /// let err = Config::builder().merge_args(args).interpolate().unwrap_err();
    /// assert!(err.to_string().contains("not expanded in a value from args"));
    ///
    /// // … unless the caller trusts command-line values.
    /// let cfg = Config::builder()
    ///     .merge_args(args)
    ///     .interpolate_with(&InterpolateOptions::new().trust_args())?
    ///     .build();
    /// assert_eq!(cfg.get::<String>("greeting")?, "hello");
    /// # std::env::remove_var("CFG_DOC_GREETING");
    /// # Ok::<(), ferroly::config::ConfigError>(())
    /// ```
    pub fn interpolate_with(mut self, options: &InterpolateOptions) -> Result<Self, ConfigError> {
        let (value, derived) =
            interpolate::interpolate(&self.value, &self.origins, &self.secret_derived, options)?;
        self.value = value;
        self.secret_derived.extend(derived);
        Ok(self)
    }

    /// Finalizes the builder into a [`Config`].
    pub fn build(mut self) -> Config {
        // Keep provenance only for what survived as a leaf: a later layer may
        // have replaced a section by a scalar or the other way round.
        let mut live = BTreeSet::new();
        leaves(&self.value, "", &mut |path| {
            live.insert(path.to_string());
        });
        self.origins.retain(|k, _| live.contains(k));
        Config {
            value: self.value,
            origins: self.origins,
            ignored: self.check.take_found(),
            secret_derived: self.secret_derived,
            literal_keys: Vec::new(),
        }
    }
}

/// Deep-merges `over` onto `base`; objects merge recursively, everything else
/// is replaced.
fn merge(base: Value, over: Value) -> Value {
    // Up to this many keys in `over` are looked up by scanning `base`; more
    // are looked up in an index, so merging many keys stays linear.
    const SCAN: usize = 16;
    match (base, over) {
        (Value::Object(mut b), Value::Object(o)) => {
            let mut index: Option<HashMap<String, usize>> = (o.len() > SCAN).then(|| {
                let mut index = HashMap::with_capacity(b.len() + o.len());
                for (at, (k, _)) in b.iter().enumerate() {
                    index.entry(k.clone()).or_insert(at);
                }
                index
            });
            for (k, ov) in o {
                let pos = match &index {
                    Some(index) => index.get(&k).copied(),
                    None => b.iter().position(|(bk, _)| *bk == k),
                };
                if let Some(pos) = pos {
                    let bv = std::mem::replace(&mut b[pos].1, Value::Null);
                    b[pos].1 = merge(bv, ov);
                } else {
                    if let Some(index) = &mut index {
                        index.insert(k.clone(), b.len());
                    }
                    b.push((k, ov));
                }
            }
            Value::Object(b)
        }
        (_, over) => over,
    }
}

/// The error for a value under `prefix` (a dotted key, or `""` for the root)
/// that failed to decode: [`ConfigError::InvalidValue`] naming `prefix` joined
/// with the error's path, with the path-free cause as its source, when the
/// error carries a path; [`ConfigError::Codec`] otherwise.
fn invalid_value(prefix: &str, e: CodecError) -> ConfigError {
    let path = e.path_string();
    if path.is_empty() {
        return ConfigError::Codec(e);
    }
    let key = if prefix.is_empty() || path.starts_with('[') {
        format!("{prefix}{path}")
    } else {
        format!("{prefix}.{path}")
    };
    ConfigError::InvalidValue {
        key,
        source: e.root_cause().clone(),
    }
}

fn navigate<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    let mut cur = value;
    for seg in key.split('.') {
        if seg.is_empty() {
            continue;
        }
        cur = cur.get(seg)?;
    }
    Some(cur)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroly::codec::Decode;

    #[derive(Decode, Debug, PartialEq)]
    struct Settings {
        port: u16,
        host: String,
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("ferroly-config-{name}"))
    }

    #[test]
    fn loads_from_env() {
        std::env::set_var("CFGTEST1_PORT", "9000");
        std::env::set_var("CFGTEST1_HOST", "localhost");
        let cfg = Config::from_env("CFGTEST1_");
        let settings: Settings = cfg.extract().unwrap();
        assert_eq!(
            settings,
            Settings {
                port: 9000,
                host: "localhost".into()
            }
        );
        std::env::remove_var("CFGTEST1_PORT");
        std::env::remove_var("CFGTEST1_HOST");
    }

    #[test]
    fn nested_env_keys() {
        std::env::set_var("CFGTEST2_DB__HOST", "db1");
        let cfg = Config::from_env("CFGTEST2_");
        let host: String = cfg.get("db.host").unwrap();
        assert_eq!(host, "db1");
        std::env::remove_var("CFGTEST2_DB__HOST");
    }

    #[test]
    fn env_overrides_file() {
        let path = temp_path("override.json");
        std::fs::write(&path, r#"{"port":8080,"host":"file-host"}"#).unwrap();
        std::env::set_var("CFGTEST3_HOST", "env-host");

        let cfg = Config::builder()
            .merge_file(&path)
            .unwrap()
            .merge_env("CFGTEST3_")
            .build();
        let settings: Settings = cfg.extract().unwrap();
        assert_eq!(
            settings,
            Settings {
                port: 8080,
                host: "env-host".into()
            }
        );

        std::env::remove_var("CFGTEST3_HOST");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn loads_yaml_file() {
        let path = temp_path("cfg.yaml");
        std::fs::write(&path, "port: 7000\nhost: yaml-host\n").unwrap();
        let settings: Settings = Config::from_file(&path).unwrap().extract().unwrap();
        assert_eq!(
            settings,
            Settings {
                port: 7000,
                host: "yaml-host".into()
            }
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unsupported_extension_errors() {
        let err = Config::from_file("config.ini").unwrap_err();
        assert!(matches!(&err, ConfigError::UnsupportedFormat(p) if p == "config.ini"));
    }

    #[test]
    fn args_override_in_precedence_order() {
        #[derive(ferroly::codec::Decode, PartialEq, Debug)]
        struct Db {
            host: String,
            port: u16,
        }
        #[derive(ferroly::codec::Decode, PartialEq, Debug)]
        struct Cfg {
            db: Db,
            verbose: bool,
        }

        // defaults, then CLI flags override (later merge wins).
        let cfg: Cfg = Config::builder()
            .merge_value(source::parse_properties(
                "db.host=default\ndb.port=1\nverbose=false",
            ))
            .merge_args([
                "--db.host=cli-host".to_string(),
                "--db.port".to_string(),
                "5432".to_string(),
                "--verbose".to_string(),
            ])
            .build()
            .extract()
            .unwrap();

        assert_eq!(
            cfg,
            Cfg {
                db: Db {
                    host: "cli-host".into(),
                    port: 5432
                },
                verbose: true,
            }
        );
    }

    #[test]
    fn credential_urls_are_recognised() {
        assert!(has_url_credentials("postgres://app:pw@db:5432/x"));
        assert!(!has_url_credentials("https://example.org/a@b"));
        assert!(!has_url_credentials("plain text"));
    }
}

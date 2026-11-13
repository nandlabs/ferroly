//! Layered configuration loading.
//!
//! Loads configuration from environment variables, config files and
//! command-line flags, with later layers overriding earlier ones.
//! Built entirely on `ferroly::codec` — no external configuration crate.
//!
//! - File formats are inferred from the extension: `.json`, `.yaml`/`.yml`,
//!   `.toml`, `.xml`, `.properties`/`.props`/`.env`.
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
//! one configuration between threads and reloads it on demand.

#![deny(missing_docs)]

mod interpolate;
mod shared;
mod source;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use ferroly::codec::{find, schema, CodecError, Decode, Value};

pub use shared::SharedConfig;

use source::{insert_nested, Setting};

/// Errors raised while loading or extracting configuration.
///
/// No variant ever carries a configuration *value*: messages name files, keys
/// and variables only, so they are safe to log.
#[derive(Debug)]
#[non_exhaustive]
pub enum ConfigError {
    /// A file's extension is not a recognized config format. The message
    /// names the file and lists the supported extensions.
    UnsupportedFormat(String),

    /// An I/O error reading a config or secret file. The message starts with
    /// the path.
    Io(String),

    /// The requested key was not present.
    MissingKey(String),

    /// One or more keys demanded by [`Config::require`] are absent; every
    /// missing key is listed by its full dotted path.
    MissingKeys(Vec<String>),

    /// A codec (decode) error while binding the whole configuration with
    /// [`Config::extract`].
    Codec(CodecError),

    /// A config file could not be parsed.
    Parse {
        /// The file that failed to parse.
        path: String,
        /// The parser's message, with any text it copied from the file
        /// replaced by `<redacted>`.
        message: String,
    },

    /// The value at `key` could not be converted to the requested type.
    InvalidValue {
        /// Full dotted path of the offending key.
        key: String,
        /// Why the conversion failed.
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
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::UnsupportedFormat(m) => {
                write!(f, "unsupported config file format for {m}")
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
    /// unknown file format ([`Unsupported`](ferroly::errutils::ErrorKind::Unsupported))
    /// and I/O failures, whose cause is not known here
    /// ([`Unknown`](ferroly::errutils::ErrorKind::Unknown)).
    fn kind(&self) -> ferroly::errutils::ErrorKind {
        use ferroly::errutils::ErrorKind;
        match self {
            ConfigError::UnsupportedFormat(_) => ErrorKind::Unsupported,
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
    /// A raw `--key=value` argument ([`ConfigBuilder::merge_args`]).
    Args,
    /// A parsed command-line argument (`ConfigBuilder::merge_cli`).
    Cli,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::Value => f.write_str("value"),
            Source::File(path) => write!(f, "file {path}"),
            Source::Env(var) => write!(f, "env {var}"),
            Source::SecretFile { var, path } => write!(f, "secret file {path} (env {var})"),
            Source::Args => f.write_str("args"),
            Source::Cli => f.write_str("cli"),
        }
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
    pub fn extract<T: Decode>(&self) -> Result<T, ConfigError> {
        Ok(T::decode(&self.value)?)
    }

    /// Extracts a single (dot-pathed) key into a typed value.
    ///
    /// A missing key is [`ConfigError::MissingKey`]; a value that does not
    /// convert is [`ConfigError::InvalidValue`] naming the key's full path.
    pub fn get<T: Decode>(&self, key: &str) -> Result<T, ConfigError> {
        let v =
            navigate(&self.value, key).ok_or_else(|| ConfigError::MissingKey(key.to_string()))?;
        T::decode(v).map_err(|source| ConfigError::InvalidValue {
            key: key.to_string(),
            source,
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
    /// ([`is_sensitive_key`]), when it was read from a secret file, or when it
    /// is a URL carrying credentials (`scheme://user:pass@host`). Use
    /// [`to_redacted_string_with`](Config::to_redacted_string_with) to supply
    /// your own notion of a sensitive key.
    pub fn to_redacted_string(&self) -> String {
        self.to_redacted_string_with(is_sensitive_key)
    }

    /// Like [`to_redacted_string`](Config::to_redacted_string), but `sensitive`
    /// decides which dotted key paths are masked (a masked section masks
    /// everything below it). Secret-file values and credential-bearing URLs
    /// are masked regardless.
    pub fn to_redacted_string_with<F>(&self, sensitive: F) -> String
    where
        F: Fn(&str) -> bool,
    {
        let mut out = String::new();
        dump(
            &self.value,
            "",
            false,
            None,
            &sensitive,
            &self.origins,
            &mut out,
        );
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

fn dump(
    value: &Value,
    path: &str,
    hidden: bool,
    origin: Option<&Source>,
    sensitive: &dyn Fn(&str) -> bool,
    origins: &BTreeMap<String, Source>,
    out: &mut String,
) {
    match value {
        Value::Object(entries) if !entries.is_empty() => {
            for (k, v) in entries {
                let child = join(path, k);
                let hidden = hidden || sensitive(&child);
                let origin = origins.get(&child).or(origin);
                dump(v, &child, hidden, origin, sensitive, origins, out);
            }
        }
        Value::Array(items) if !items.is_empty() => {
            for (i, v) in items.iter().enumerate() {
                let child = format!("{path}[{i}]");
                dump(v, &child, hidden, origin, sensitive, origins, out);
            }
        }
        leaf => {
            let masked = hidden
                || matches!(origin, Some(Source::SecretFile { .. }))
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
}

impl Default for ConfigBuilder {
    /// An empty builder — the same as [`Config::builder`].
    fn default() -> Self {
        Self {
            value: Value::Object(Vec::new()),
            origins: BTreeMap::new(),
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
        let origins = &mut self.origins;
        leaves(&layer, "", &mut |path| {
            origins.insert(path.to_string(), source.clone());
        });
        self.value = merge(std::mem::take(&mut self.value), layer);
        self
    }

    /// Merges individual settings, each with its own origin.
    fn apply_settings(mut self, settings: Vec<Setting>) -> Self {
        let mut entries: Vec<(String, Value)> = Vec::new();
        for s in settings {
            self.origins.insert(s.segs.join("."), s.source);
            insert_nested(&mut entries, &s.segs, s.value);
        }
        self.value = merge(std::mem::take(&mut self.value), Value::Object(entries));
        self
    }

    /// Merges environment variables with the given prefix.
    ///
    /// The prefix is stripped, the rest lowercased and split on `__` into a
    /// key path. Variables that are not valid UTF-8, and variables whose path
    /// would contain an empty segment (`APP_A____B`), are ignored.
    pub fn merge_env(self, prefix: &str) -> Self {
        // Without secret files there is nothing fallible to do.
        let settings =
            source::read_env(prefix, &EnvOptions::default(), &self.value).unwrap_or_default();
        self.apply_settings(settings)
    }

    /// Merges environment variables with the given prefix, with the extra
    /// behaviours selected in `options` (secrets from files, lists, key-case
    /// matching). Fails only when a secret file cannot be read.
    pub fn merge_env_with(self, prefix: &str, options: &EnvOptions) -> Result<Self, ConfigError> {
        let settings = source::read_env(prefix, options, &self.value)?;
        Ok(self.apply_settings(settings))
    }

    /// Merges a config file, format inferred from extension. A missing file is
    /// an error; see [`merge_file_optional`](ConfigBuilder::merge_file_optional).
    pub fn merge_file(self, path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        self.merge_file_impl(path.as_ref(), false).map(|(b, _)| b)
    }

    /// Merges a config file if it exists. A missing file is skipped silently;
    /// an unsupported extension, an unreadable file or malformed content is
    /// still an error.
    pub fn merge_file_optional(self, path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        self.merge_file_impl(path.as_ref(), true).map(|(b, _)| b)
    }

    /// Returns the builder and whether the file was found.
    fn merge_file_impl(self, path: &Path, optional: bool) -> Result<(Self, bool), ConfigError> {
        match source::load_file(path, optional)? {
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
    ///   merged.
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
        let (stem, extensions): (PathBuf, Vec<&str>) = match explicit {
            Some(ext) => (base.with_extension(""), vec![ext]),
            None => (base.to_path_buf(), source::EXTENSIONS.to_vec()),
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
    /// For a declared interface with validation and help, parse with
    /// `ferroly::cli` and feed the result to `merge_cli` (available when the
    /// `cli` feature is enabled).
    pub fn merge_args<I: IntoIterator<Item = String>>(self, args: I) -> Self {
        let settings = source::parse_args(args, &self.value)
            .into_iter()
            .map(|(segs, value)| Setting {
                segs,
                value,
                source: Source::Args,
            })
            .collect();
        self.apply_settings(settings)
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
    #[cfg(feature = "cli")]
    pub fn merge_cli(self, matches: &ferroly::cli::Matches) -> Self {
        self.merge_cli_where(matches, |s| s != ferroly::cli::ValueSource::Default)
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
            if segs.iter().any(|s| s.is_empty()) {
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
        self.apply_settings(settings)
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
    /// Errors are [`ConfigError::Interpolation`] and name the key and the
    /// placeholder, never a value.
    pub fn interpolate(mut self) -> Result<Self, ConfigError> {
        self.value = interpolate::interpolate(&self.value)?;
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
        }
    }
}

/// Deep-merges `over` onto `base`; objects merge recursively, everything else
/// is replaced.
fn merge(base: Value, over: Value) -> Value {
    match (base, over) {
        (Value::Object(mut b), Value::Object(o)) => {
            for (k, ov) in o {
                if let Some(pos) = b.iter().position(|(bk, _)| *bk == k) {
                    let bv = std::mem::replace(&mut b[pos].1, Value::Null);
                    b[pos].1 = merge(bv, ov);
                } else {
                    b.push((k, ov));
                }
            }
            Value::Object(b)
        }
        (_, over) => over,
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
        assert!(matches!(err, ConfigError::UnsupportedFormat(_)));
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

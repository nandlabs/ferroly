//! Layered configuration loading.
//!
//! Loads configuration from environment variables and config files, with later
//! layers overriding earlier ones.
//! Built entirely on `ferroly-codec` — no external configuration crate.
//!
//! - File formats are inferred from the extension: `.json`, `.yaml`/`.yml`, `.xml`.
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

#![deny(missing_docs)]

use std::path::Path;

use ferroly::codec::{json, xml, yaml, CodecError, Decode, Value};
use ferroly_derive::FerrolyError;

/// Errors raised while loading or extracting configuration.
#[derive(Debug, FerrolyError)]
#[non_exhaustive]
pub enum ConfigError {
    /// A file's extension is not a recognized config format.
    #[error("unsupported config file format for '{0}' (expected .json, .yaml, .yml, or .xml)")]
    UnsupportedFormat(String),

    /// An I/O error reading a config file.
    #[error("io error: {0}")]
    Io(String),

    /// The requested key was not present.
    #[error("missing key: {0}")]
    MissingKey(String),

    /// A codec (parse/decode) error.
    #[error(transparent)]
    Codec(#[from] CodecError),
}

/// A resolved, layered configuration backed by a merged [`Value`].
#[derive(Clone)]
pub struct Config {
    value: Value,
}

impl Default for Config {
    /// Same as [`Config::new`] — an empty object (not `Value::Null`).
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Config {
    /// Redacts values so configuration secrets (tokens, passwords, connection
    /// strings) can't leak into logs or panic messages via `{:?}`. Only the
    /// top-level key names are shown.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keys: Vec<&str> = match &self.value {
            Value::Object(o) => o.iter().map(|(k, _)| k.as_str()).collect(),
            _ => Vec::new(),
        };
        f.debug_struct("Config")
            .field("keys", &keys)
            .field("values", &"<redacted>")
            .finish()
    }
}

impl Config {
    /// Creates an empty configuration.
    pub fn new() -> Self {
        Self {
            value: Value::Object(Vec::new()),
        }
    }

    /// Starts a [`ConfigBuilder`] for composing multiple layers.
    pub fn builder() -> ConfigBuilder {
        ConfigBuilder {
            value: Value::Object(Vec::new()),
        }
    }

    /// Loads configuration solely from environment variables with the given
    /// prefix (e.g. `"APP_"` maps `APP_PORT` to `port`, `APP_DB__HOST` to
    /// `db.host`).
    pub fn from_env(prefix: &str) -> Self {
        Self {
            value: env_to_value(prefix),
        }
    }

    /// Loads configuration solely from a file, format inferred from extension.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        Ok(Self {
            value: load_file(path.as_ref())?,
        })
    }

    /// Extracts the whole configuration into a typed value.
    pub fn extract<T: Decode>(&self) -> Result<T, ConfigError> {
        Ok(T::decode(&self.value)?)
    }

    /// Extracts a single (dot-pathed) key into a typed value.
    pub fn get<T: Decode>(&self, key: &str) -> Result<T, ConfigError> {
        let v =
            navigate(&self.value, key).ok_or_else(|| ConfigError::MissingKey(key.to_string()))?;
        Ok(T::decode(v)?)
    }

    /// Returns the merged underlying [`Value`].
    pub fn value(&self) -> &Value {
        &self.value
    }
}

/// Builder for composing configuration layers in priority order.
#[derive(Debug, Default)]
#[must_use]
pub struct ConfigBuilder {
    value: Value,
}

impl ConfigBuilder {
    /// Merges environment variables with the given prefix.
    pub fn merge_env(mut self, prefix: &str) -> Self {
        self.value = merge(self.value, env_to_value(prefix));
        self
    }

    /// Merges a config file, format inferred from extension.
    pub fn merge_file(mut self, path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let v = load_file(path.as_ref())?;
        self.value = merge(self.value, v);
        Ok(self)
    }

    /// Merges an explicit [`Value`] layer.
    pub fn merge_value(mut self, value: Value) -> Self {
        self.value = merge(self.value, value);
        self
    }

    /// Merges command-line flags — `--db.host localhost`, `--db.port=5432`, and
    /// bare `--verbose` (→ `"true"`) — nesting keys on `.` (typically pass
    /// `std::env::args().skip(1)`). Highest priority when merged last.
    pub fn merge_args<I: IntoIterator<Item = String>>(mut self, args: I) -> Self {
        let mut entries: Vec<(String, Value)> = Vec::new();
        let mut it = args.into_iter().peekable();
        while let Some(arg) = it.next() {
            let Some(flag) = arg.strip_prefix("--") else {
                continue;
            };
            let (key, val) = match flag.split_once('=') {
                Some((k, v)) => (k.to_string(), v.to_string()),
                None => {
                    let takes_value = it.peek().is_some_and(|n| !n.starts_with("--"));
                    let v = if takes_value {
                        it.next().unwrap()
                    } else {
                        "true".to_string()
                    };
                    (flag.to_string(), v)
                }
            };
            let segs: Vec<String> = key.split('.').map(str::to_string).collect();
            if !segs.iter().any(|s| s.is_empty()) {
                insert_nested(&mut entries, &segs, Value::Str(val));
            }
        }
        self.value = merge(self.value, Value::Object(entries));
        self
    }

    /// Finalizes the builder into a [`Config`].
    pub fn build(self) -> Config {
        Config { value: self.value }
    }
}

enum FileFormat {
    Json,
    Yaml,
    Xml,
    Properties,
}

fn load_file(path: &Path) -> Result<Value, ConfigError> {
    // Validate the extension before touching the filesystem.
    let format = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("json") => FileFormat::Json,
        Some("yaml") | Some("yml") => FileFormat::Yaml,
        Some("xml") => FileFormat::Xml,
        Some("properties") | Some("props") | Some("env") => FileFormat::Properties,
        _ => return Err(ConfigError::UnsupportedFormat(path.display().to_string())),
    };
    let bytes = std::fs::read(path).map_err(|e| ConfigError::Io(e.to_string()))?;
    let value = match format {
        FileFormat::Json => json::from_slice(&bytes)?,
        FileFormat::Yaml => yaml::from_str(std::str::from_utf8(&bytes).map_err(io)?)?,
        FileFormat::Xml => xml::from_str(std::str::from_utf8(&bytes).map_err(io)?)?,
        FileFormat::Properties => parse_properties(std::str::from_utf8(&bytes).map_err(io)?),
    };
    Ok(value)
}

/// Parses `.properties` lines (`key.path = value`, `#`/`!` comments), nesting on
/// `.` — so `db.host = localhost` becomes `{ db: { host: "localhost" } }`.
fn parse_properties(input: &str) -> Value {
    let mut entries: Vec<(String, Value)> = Vec::new();
    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        if let Some((key, val)) = line.split_once('=') {
            let segs: Vec<String> = key
                .trim()
                .split('.')
                .map(|s| s.trim().to_string())
                .collect();
            if !segs.iter().any(|s| s.is_empty()) {
                insert_nested(&mut entries, &segs, Value::Str(val.trim().to_string()));
            }
        }
    }
    Value::Object(entries)
}

fn io<E: std::fmt::Display>(e: E) -> ConfigError {
    ConfigError::Io(e.to_string())
}

fn env_to_value(prefix: &str) -> Value {
    // Process variables in sorted key order so the result is deterministic
    // regardless of `env::vars()`' arbitrary iteration order. On a scalar-vs-
    // nested collision (`APP_DB` and `APP_DB__HOST`) the nested form wins, since
    // its longer key sorts last and is applied last.
    let mut vars: Vec<(String, String)> = std::env::vars().collect();
    vars.sort();
    let mut entries: Vec<(String, Value)> = Vec::new();
    for (k, v) in vars {
        if let Some(rest) = k.strip_prefix(prefix) {
            if rest.is_empty() {
                continue;
            }
            let segs: Vec<String> = rest.split("__").map(|s| s.to_lowercase()).collect();
            insert_nested(&mut entries, &segs, Value::Str(v));
        }
    }
    Value::Object(entries)
}

fn insert_nested(entries: &mut Vec<(String, Value)>, segs: &[String], value: Value) {
    let head = &segs[0];
    if segs.len() == 1 {
        match entries.iter_mut().find(|(k, _)| k == head) {
            Some((_, v)) => *v = value,
            None => entries.push((head.clone(), value)),
        }
        return;
    }
    let idx = match entries.iter().position(|(k, _)| k == head) {
        Some(i) => i,
        None => {
            entries.push((head.clone(), Value::Object(Vec::new())));
            entries.len() - 1
        }
    };
    if !matches!(entries[idx].1, Value::Object(_)) {
        entries[idx].1 = Value::Object(Vec::new());
    }
    if let Value::Object(child) = &mut entries[idx].1 {
        insert_nested(child, &segs[1..], value);
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
    fn parses_properties_nested() {
        let v = parse_properties("# comment\ndb.host = localhost\ndb.port = 5432\nname=svc\n");
        assert_eq!(v.get("name").and_then(Value::as_str), Some("svc"));
        let db = v.get("db").unwrap();
        assert_eq!(db.get("host").and_then(Value::as_str), Some("localhost"));
        assert_eq!(db.get("port").and_then(Value::as_str), Some("5432"));
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
            .merge_value(parse_properties(
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
}

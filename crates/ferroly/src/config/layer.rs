//! A declarative, ordered list of configuration layers.

use std::fmt;
use std::path::PathBuf;

use ferroly::codec::Value;

use super::{ArgsOptions, Config, ConfigBuilder, ConfigError, EnvOptions};

/// One configuration layer, applied by [`ConfigBuilder::layers`] or
/// [`Config::from_layers`].
///
/// Every variant is exactly one [`ConfigBuilder`] `merge_*` call, with the same
/// behaviour, errors and provenance ([`Source`](super::Source)); a list of
/// layers is applied front to back, so **later layers override earlier ones**.
/// Writing the stack as a list keeps its priority order in one visible place:
///
/// ```no_run
/// use ferroly::codec::Value;
/// use ferroly::config::{Config, Layer};
///
/// let defaults = Value::Object(vec![("port".into(), Value::Int(8080))]);
/// let cfg = Config::from_layers([
///     Layer::Value(defaults),                     // lowest priority
///     Layer::file("config/app.toml"),
///     Layer::optional_file("config/local.toml"),
///     Layer::optional_secret_dir("/run/secrets"),
///     Layer::env("APP_"),                         // highest priority
/// ])?;
/// # Ok::<(), ferroly::config::ConfigError>(())
/// ```
///
/// `Debug` never prints configuration values: a `Value` layer, the argument
/// list of an `Args` layer and parsed command lines are shown as
/// `<redacted>`.
#[derive(Clone)]
#[non_exhaustive]
pub enum Layer {
    /// An explicit value, typically compiled-in defaults —
    /// [`merge_value`](ConfigBuilder::merge_value).
    Value(Value),
    /// A config file that must exist, format inferred from the extension —
    /// [`merge_file`](ConfigBuilder::merge_file).
    File(PathBuf),
    /// A config file that is skipped when missing —
    /// [`merge_file_optional`](ConfigBuilder::merge_file_optional).
    OptionalFile(PathBuf),
    /// A base file and its per-profile overlay —
    /// [`merge_profile`](ConfigBuilder::merge_profile).
    Profile {
        /// The base path, with or without an extension.
        base: PathBuf,
        /// The profile name; empty loads only the base.
        profile: String,
    },
    /// Environment variables with a prefix —
    /// [`merge_env`](ConfigBuilder::merge_env).
    Env(String),
    /// Environment variables with a prefix and opt-in behaviours —
    /// [`merge_env_with`](ConfigBuilder::merge_env_with).
    EnvWith {
        /// The variable-name prefix (`"APP_"`).
        prefix: String,
        /// Secret files, lists and key-case handling.
        options: EnvOptions,
    },
    /// A mounted secrets directory, one file per key —
    /// [`merge_secret_dir`](ConfigBuilder::merge_secret_dir).
    SecretDir(PathBuf),
    /// A secrets directory that is skipped when missing —
    /// [`merge_secret_dir_optional`](ConfigBuilder::merge_secret_dir_optional).
    OptionalSecretDir(PathBuf),
    /// Raw `--key=value` command-line flags —
    /// [`merge_args`](ConfigBuilder::merge_args), the shorthand for
    /// [`Layer::ArgsWith`] with the default options.
    Args(Vec<String>),
    /// Raw `--key value` command-line flags typed by `options` —
    /// [`merge_args_with`](ConfigBuilder::merge_args_with).
    ArgsWith {
        /// The arguments (typically `std::env::args().skip(1)`).
        args: Vec<String>,
        /// How each flag's arity is decided.
        options: ArgsOptions,
    },
    /// The explicitly given arguments of a parsed command line, and its
    /// dynamic flags — [`merge_cli`](ConfigBuilder::merge_cli).
    #[cfg(feature = "cli")]
    Cli(ferroly::cli::Matches),
    /// Like [`Layer::Cli`], with the dynamic flags typed by `options` —
    /// [`merge_cli_with`](ConfigBuilder::merge_cli_with).
    #[cfg(feature = "cli")]
    CliWith {
        /// The parsed command line.
        matches: ferroly::cli::Matches,
        /// How the dynamic flags are typed.
        options: ArgsOptions,
    },
    /// Only the default values of a parsed command line —
    /// [`merge_cli_defaults`](ConfigBuilder::merge_cli_defaults).
    #[cfg(feature = "cli")]
    CliDefaults(ferroly::cli::Matches),
}

impl Layer {
    /// A [`Layer::Value`] layer.
    pub fn value(value: Value) -> Self {
        Layer::Value(value)
    }

    /// A [`Layer::File`] layer.
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Layer::File(path.into())
    }

    /// A [`Layer::OptionalFile`] layer.
    pub fn optional_file(path: impl Into<PathBuf>) -> Self {
        Layer::OptionalFile(path.into())
    }

    /// A [`Layer::Profile`] layer.
    pub fn profile(base: impl Into<PathBuf>, profile: impl Into<String>) -> Self {
        Layer::Profile {
            base: base.into(),
            profile: profile.into(),
        }
    }

    /// A [`Layer::Env`] layer.
    pub fn env(prefix: impl Into<String>) -> Self {
        Layer::Env(prefix.into())
    }

    /// A [`Layer::EnvWith`] layer.
    pub fn env_with(prefix: impl Into<String>, options: EnvOptions) -> Self {
        Layer::EnvWith {
            prefix: prefix.into(),
            options,
        }
    }

    /// A [`Layer::SecretDir`] layer.
    pub fn secret_dir(dir: impl Into<PathBuf>) -> Self {
        Layer::SecretDir(dir.into())
    }

    /// A [`Layer::OptionalSecretDir`] layer.
    pub fn optional_secret_dir(dir: impl Into<PathBuf>) -> Self {
        Layer::OptionalSecretDir(dir.into())
    }

    /// A [`Layer::Args`] layer (typically `std::env::args().skip(1)`): the
    /// shorthand for [`Layer::args_with`] with the default options.
    pub fn args<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Layer::Args(args.into_iter().map(Into::into).collect())
    }

    /// A [`Layer::ArgsWith`] layer.
    pub fn args_with<I, S>(args: I, options: ArgsOptions) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Layer::ArgsWith {
            args: args.into_iter().map(Into::into).collect(),
            options,
        }
    }

    /// Applies this layer with the matching `merge_*` call.
    fn apply(self, builder: ConfigBuilder) -> Result<ConfigBuilder, ConfigError> {
        match self {
            Layer::Value(value) => Ok(builder.merge_value(value)),
            Layer::File(path) => builder.merge_file(path),
            Layer::OptionalFile(path) => builder.merge_file_optional(path),
            Layer::Profile { base, profile } => builder.merge_profile(base, &profile),
            Layer::Env(prefix) => Ok(builder.merge_env(&prefix)),
            Layer::EnvWith { prefix, options } => builder.merge_env_with(&prefix, &options),
            Layer::SecretDir(dir) => builder.merge_secret_dir(dir),
            Layer::OptionalSecretDir(dir) => builder.merge_secret_dir_optional(dir),
            Layer::Args(args) => Ok(builder.merge_args(args)),
            Layer::ArgsWith { args, options } => Ok(builder.merge_args_with(args, options)),
            #[cfg(feature = "cli")]
            Layer::Cli(matches) => Ok(builder.merge_cli(&matches)),
            #[cfg(feature = "cli")]
            Layer::CliWith { matches, options } => Ok(builder.merge_cli_with(&matches, options)),
            #[cfg(feature = "cli")]
            Layer::CliDefaults(matches) => Ok(builder.merge_cli_defaults(&matches)),
        }
    }
}

impl fmt::Debug for Layer {
    /// Shows the layer kind and where it reads from, never a value.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const HIDDEN: &str = "<redacted>";
        match self {
            Layer::Value(_) => f.debug_tuple("Value").field(&HIDDEN).finish(),
            Layer::File(path) => f.debug_tuple("File").field(path).finish(),
            Layer::OptionalFile(path) => f.debug_tuple("OptionalFile").field(path).finish(),
            Layer::Profile { base, profile } => f
                .debug_struct("Profile")
                .field("base", base)
                .field("profile", profile)
                .finish(),
            Layer::Env(prefix) => f.debug_tuple("Env").field(prefix).finish(),
            Layer::EnvWith { prefix, options } => f
                .debug_struct("EnvWith")
                .field("prefix", prefix)
                .field("options", options)
                .finish(),
            Layer::SecretDir(dir) => f.debug_tuple("SecretDir").field(dir).finish(),
            Layer::OptionalSecretDir(dir) => f.debug_tuple("OptionalSecretDir").field(dir).finish(),
            Layer::Args(_) => f.debug_tuple("Args").field(&HIDDEN).finish(),
            Layer::ArgsWith { options, .. } => f
                .debug_struct("ArgsWith")
                .field("args", &HIDDEN)
                .field("options", options)
                .finish(),
            #[cfg(feature = "cli")]
            Layer::Cli(_) => f.debug_tuple("Cli").field(&HIDDEN).finish(),
            #[cfg(feature = "cli")]
            Layer::CliWith { options, .. } => f
                .debug_struct("CliWith")
                .field("matches", &HIDDEN)
                .field("options", options)
                .finish(),
            #[cfg(feature = "cli")]
            Layer::CliDefaults(_) => f.debug_tuple("CliDefaults").field(&HIDDEN).finish(),
        }
    }
}

impl ConfigBuilder {
    /// Applies `layers` in order, each with its matching `merge_*` call, so
    /// later layers override earlier ones (and everything merged before this
    /// call). Provenance is recorded exactly as by the individual calls.
    ///
    /// Stops at the first layer that fails and returns its error unchanged
    /// (later layers are not read); like the fallible `merge_*` calls, the
    /// builder is consumed. Chain
    /// [`interpolate`](ConfigBuilder::interpolate) afterwards to expand
    /// `${…}` placeholders.
    ///
    /// ```
    /// use ferroly::codec::Value;
    /// use ferroly::config::{Config, Layer, Source};
    ///
    /// let cfg = Config::builder()
    ///     .layers([
    ///         Layer::Value(Value::Object(vec![
    ///             ("host".into(), Value::Str("localhost".into())),
    ///             ("port".into(), Value::Str("8080".into())),
    ///         ])),
    ///         Layer::Value(Value::Object(vec![("port".into(), Value::Str("9000".into()))])),
    ///     ])?
    ///     .build();
    ///
    /// assert_eq!(cfg.get::<u16>("port")?, 9000);
    /// assert_eq!(cfg.source("port"), Some(&Source::Value));
    /// assert_eq!(cfg.source("host"), Some(&Source::Value));
    /// # Ok::<(), ferroly::config::ConfigError>(())
    /// ```
    pub fn layers<I>(self, layers: I) -> Result<Self, ConfigError>
    where
        I: IntoIterator<Item = Layer>,
    {
        layers
            .into_iter()
            .try_fold(self, |builder, layer| layer.apply(builder))
    }
}

impl Config {
    /// Builds a configuration from `layers`, applied in order (later layers
    /// override earlier ones) — shorthand for
    /// `Config::builder().layers(layers)?.build()`.
    pub fn from_layers<I>(layers: I) -> Result<Config, ConfigError>
    where
        I: IntoIterator<Item = Layer>,
    {
        Ok(Config::builder().layers(layers)?.build())
    }
}

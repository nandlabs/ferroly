//! Typed command-line argument merging: [`ArgsOptions`],
//! [`ConfigBuilder::merge_args_with`], [`ConfigBuilder::merge_cli_with`] and
//! `dynamic_arity`.

use std::collections::HashMap;
use std::fmt;

use ferroly::codec::Value;

use super::source::{self, Setting};
use super::{navigate, ConfigBuilder, Source};

/// How [`ConfigBuilder::merge_args_with`] (and the dynamic flags of
/// `ConfigBuilder::merge_cli_with`, feature `cli`) decide whether `--key`
/// takes a value.
///
/// The default options keep the rule of
/// [`merge_args`](ConfigBuilder::merge_args): the layers merged earlier into
/// the same builder decide. [`types`](ArgsOptions::types) declares the key
/// types explicitly, so a flags layer can be built on its own. The usual
/// recipe is `types(&schema).value_follows()`: the schema is typically
/// `AppConfig::default().encode()`, where an unset `Option` field is `null`
/// (type unknown), and `value_follows` lets such a key still take
/// `--key value`:
///
/// ```
/// use ferroly::codec::Value;
/// use ferroly::config::{ArgsOptions, Config};
///
/// // Usually `AppConfig::default().encode()`.
/// let schema = Value::Object(vec![(
///     "engine".into(),
///     Value::Object(vec![
///         ("threads".into(), Value::UInt(1)),
///         ("deterministic".into(), Value::Bool(false)),
///         ("limit".into(), Value::Null), // an unset `Option<u32>`
///     ]),
/// )]);
///
/// let flags = Config::builder()
///     .merge_args_with(
///         ["--engine.threads", "16", "--engine.deterministic", "--engine.limit", "5"],
///         ArgsOptions::default().types(&schema).value_follows(),
///     )
///     .build();
/// assert_eq!(flags.get::<u32>("engine.threads")?, 16);
/// assert!(flags.get::<bool>("engine.deterministic")?);
/// assert_eq!(flags.get::<u32>("engine.limit")?, 5);
/// # Ok::<(), ferroly::config::ConfigError>(())
/// ```
///
/// `Debug` never prints the schema, which may hold default values.
#[derive(Clone, Default)]
pub struct ArgsOptions {
    types: Option<Value>,
    value_follows: bool,
}

impl ArgsOptions {
    /// The default options (the same as [`ArgsOptions::default`]).
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes the key types from `schema` (typically the application's default
    /// configuration, encoded) instead of from the layers merged earlier:
    ///
    /// - a key whose schema value is a boolean is a switch: `--key` sets
    ///   `true`; `--key=value` reads `true`/`false`, `1`/`0`, `yes`/`no` and
    ///   `on`/`off` (as a declared `cli` switch does); `--key true|false`
    ///   (those two words only, so `--key 1 file` stays unambiguous) sets that
    ///   value, and any other following token is left alone;
    /// - any other scalar or list key takes the next token as its value, even
    ///   one that starts with a single `-` (`--model.context -1`), but never
    ///   one that starts with `--` (another flag, or a bare `--`); a value key
    ///   with no value left merges as `true`, so reading it as its real type
    ///   fails instead of passing silently. Write `--key=--x` for such a value;
    /// - a list key collects every occurrence, in order:
    ///   `--tags a --tags=b` merges `["a", "b"]` (one element per occurrence;
    ///   a comma is part of the value);
    /// - a key the schema does not have, or where it holds `null` (an unset
    ///   `Option`) or a section, is unknown: a switch, unless
    ///   [`value_follows`](ArgsOptions::value_follows) is set, which is why
    ///   `types(&schema).value_follows()` is the usual combination.
    ///
    /// Switches are merged as booleans; values as strings, which
    /// [`Config::get`](super::Config::get) and
    /// [`Config::extract`](super::Config::extract) convert as usual.
    pub fn types(mut self, schema: &Value) -> Self {
        self.types = Some(schema.clone());
        self
    }

    /// For keys whose type is unknown, reads `--key value` as a value
    /// whenever the next token does not start with `-` (the rule before
    /// 0.4.0, opt-in); otherwise such a key is a switch. Has no effect on
    /// `merge_cli_with` (feature `cli`), whose parser has already made that
    /// decision.
    pub fn value_follows(mut self) -> Self {
        self.value_follows = true;
        self
    }
}

impl fmt::Debug for ArgsOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArgsOptions")
            .field("types", &self.types.as_ref().map(|_| "<redacted>"))
            .field("value_follows", &self.value_follows)
            .finish()
    }
}

/// What a type source says about one key.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Switch,
    Value,
    /// A value key whose occurrences accumulate (schema only).
    List,
    Unknown,
}

/// The kind of `key` in an explicit schema.
fn schema_kind(schema: &Value, key: &str) -> Kind {
    match navigate(schema, key) {
        Some(Value::Bool(_)) => Kind::Switch,
        Some(Value::Array(_)) => Kind::List,
        None | Some(Value::Null) | Some(Value::Object(_)) => Kind::Unknown,
        Some(_) => Kind::Value,
    }
}

/// How a [dynamic flag](ferroly::cli::Command::dynamic_flags_with) reads its
/// value, taken from `schema` (typically `AppConfig::default().encode()`), for
/// [`Command::dynamic_flags_with`](ferroly::cli::Command::dynamic_flags_with):
///
/// | Schema value at the flag's name | Result |
/// |---|---|
/// | a boolean | [`BoolSwitch`](ferroly::cli::DynamicArity::BoolSwitch): takes the next token only if it is `true`/`false` |
/// | any other scalar, or a list | [`Value`](ferroly::cli::DynamicArity::Value): always takes the next token (even `-1` or `-x`) |
/// | `null` (an unset `Option`) | [`Optional`](ferroly::cli::DynamicArity::Optional): the value heuristic of `dynamic_flags` |
/// | a section, or absent | `None`: not a dynamic flag ([`CliError::UnknownFlag`](ferroly::cli::CliError::UnknownFlag)) |
///
/// So only keys the configuration really has are accepted, and a mistyped key
/// is refused at parse time. To also accept the keys of a map section (whose
/// default holds no keys), chain a fallback:
/// `move |n| arity(n).or_else(|| n.starts_with("labels.").then_some(DynamicArity::Value))`.
/// Merge the result with
/// [`merge_cli_with`](ConfigBuilder::merge_cli_with) and the same schema.
///
/// A boolean key reads `--key`, `--key=false` and `--key false` the way
/// [`ArgsOptions::types`] does, so `parse` + `merge_cli_with` and
/// [`merge_args_with`](ConfigBuilder::merge_args_with) merge the same value.
/// A positional whose value is literally `true` or `false`, written directly
/// after a boolean flag, is therefore taken as the flag's value. To keep it
/// a positional (the [`Switch`](ferroly::cli::DynamicArity::Switch) arity,
/// which never takes the next token), wrap the function:
/// `move |n| arity(n).map(|a| if a == DynamicArity::BoolSwitch { DynamicArity::Switch } else { a })`.
///
/// ```
/// use ferroly::cli::{Arg, Command};
/// use ferroly::codec::Value;
/// use ferroly::config::{dynamic_arity, ArgsOptions, Config};
///
/// let schema = Value::Object(vec![(
///     "engine".into(),
///     Value::Object(vec![
///         ("threads".into(), Value::UInt(1)),
///         ("deterministic".into(), Value::Bool(false)),
///     ]),
/// )]);
/// let cmd = Command::new("app")
///     .dynamic_flags_with(dynamic_arity(&schema))
///     .subcommand(Command::new("serve").positional(Arg::new("file")));
///
/// // The switch does not take `model.bin`.
/// let m = cmd
///     .parse(["serve", "--engine.deterministic", "model.bin", "--engine.threads", "4"].map(String::from))
///     .unwrap();
/// assert_eq!(m.subcommand().unwrap().1.get("file"), Some("model.bin"));
/// assert!(cmd.parse(["--engine.thread=4".to_string()]).is_err());
///
/// let cfg = Config::builder()
///     .merge_cli_with(&m, ArgsOptions::default().types(&schema))
///     .build();
/// assert!(cfg.get::<bool>("engine.deterministic")?);
/// assert_eq!(cfg.get::<u32>("engine.threads")?, 4);
///
/// // But it does take a following `true` / `false`.
/// let m = cmd
///     .parse(["--engine.deterministic", "false", "serve"].map(String::from))
///     .unwrap();
/// assert_eq!(m.subcommand().unwrap().1.get("file"), None);
/// let cfg = Config::builder()
///     .merge_cli_with(&m, ArgsOptions::default().types(&schema))
///     .build();
/// assert!(!cfg.get::<bool>("engine.deterministic")?);
/// # Ok::<(), ferroly::config::ConfigError>(())
/// ```
#[cfg(feature = "cli")]
pub fn dynamic_arity(
    schema: &Value,
) -> impl Fn(&str) -> Option<ferroly::cli::DynamicArity> + Send + Sync + 'static {
    use ferroly::cli::DynamicArity;
    let schema = schema.clone();
    move |name| match navigate(&schema, name)? {
        Value::Bool(_) => Some(DynamicArity::BoolSwitch),
        Value::Null => Some(DynamicArity::Optional),
        Value::Object(_) => None,
        _ => Some(DynamicArity::Value),
    }
}

/// The kind of `key` in the layers merged so far (the `merge_args` rule:
/// strings spelled `true`/`false` count as booleans).
fn layer_kind(base: &Value, key: &str) -> Kind {
    match navigate(base, key) {
        None | Some(Value::Null) | Some(Value::Object(_)) => Kind::Unknown,
        Some(Value::Bool(_)) => Kind::Switch,
        Some(Value::Str(s))
            if matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "false") =>
        {
            Kind::Switch
        }
        Some(_) => Kind::Value,
    }
}

/// `true` / `false`, case-insensitively: the only words a switch takes from
/// the *next* token.
fn bool_word(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// The boolean spellings of a declared `cli` switch (`true`/`1`/`yes`/`on`,
/// `false`/`0`/`no`/`off`), for a value written with the flag (`--key=0`).
fn flag_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Settings in order, where repeated values of a list key collect into one
/// array (at the position of the first).
#[derive(Default)]
struct Collected {
    out: Vec<(Vec<String>, Value)>,
    lists: HashMap<String, usize>,
}

impl Collected {
    fn push(&mut self, key: &str, value: Value, list: bool) {
        let segs: Vec<String> = key.split('.').map(str::to_string).collect();
        if !source::usable_key(&segs) {
            return;
        }
        if !list {
            self.out.push((segs, value));
            return;
        }
        match value {
            Value::Str(s) => {
                if let Some(&at) = self.lists.get(key) {
                    if let Value::Array(items) = &mut self.out[at].1 {
                        items.push(Value::Str(s));
                        return;
                    }
                }
                self.lists.insert(key.to_string(), self.out.len());
                self.out.push((segs, Value::Array(vec![Value::Str(s)])));
            }
            // A list key with no value: merged as is (and fails to convert);
            // a later value starts a new list.
            other => {
                self.lists.remove(key);
                self.out.push((segs, other));
            }
        }
    }
}

/// A switch's value: a boolean in typed mode, the string `"true"` otherwise.
fn switch_value(typed: bool, on: bool) -> Value {
    if typed {
        Value::Bool(on)
    } else {
        Value::Str(on.to_string())
    }
}

/// Parses `--key[=value]` arguments with `options`. Without any option this
/// is exactly [`source::parse_args`].
fn parse_args_with<I>(args: I, base: &Value, options: &ArgsOptions) -> Vec<(Vec<String>, Value)>
where
    I: IntoIterator<Item = String>,
{
    if options.types.is_none() && !options.value_follows {
        return source::parse_args(args, base);
    }
    let typed = options.types.is_some();
    let kind_of = |key: &str| match &options.types {
        Some(schema) => schema_kind(schema, key),
        None => layer_kind(base, key),
    };

    let mut out = Collected::default();
    let mut it = args.into_iter().peekable();
    while let Some(arg) = it.next() {
        if arg == "--" {
            break;
        }
        let Some(flag) = arg.strip_prefix("--") else {
            continue;
        };
        let (key, value) = match flag.split_once('=') {
            Some((k, v)) => {
                let value = match (typed && kind_of(k) == Kind::Switch, flag_bool(v)) {
                    (true, Some(b)) => Value::Bool(b),
                    _ => Value::Str(v.to_string()),
                };
                (k, value)
            }
            None => {
                let next = it.peek().map(String::as_str);
                let value = match kind_of(flag) {
                    Kind::Switch => match next.and_then(bool_word) {
                        // Only an explicit schema lets `--key false` read the
                        // word; the layer rule keeps a switch from consuming.
                        Some(b) if typed => {
                            it.next();
                            Value::Bool(b)
                        }
                        _ => switch_value(typed, true),
                    },
                    // A value never starts with `--`: that is another flag
                    // (or the bare `--`), which must not be swallowed.
                    Kind::Value | Kind::List => match next {
                        Some(n) if !n.starts_with("--") => {
                            Value::Str(it.next().unwrap_or_default())
                        }
                        _ => switch_value(typed, true),
                    },
                    Kind::Unknown => match next {
                        Some(n) if options.value_follows && !n.starts_with('-') => {
                            Value::Str(it.next().unwrap_or_default())
                        }
                        _ => switch_value(typed, true),
                    },
                };
                (flag, value)
            }
        };
        out.push(key, value, kind_of(key) == Kind::List);
    }
    out.out
}

impl ConfigBuilder {
    /// Merges `--key value` / `--key=value` command-line flags, nesting keys
    /// on `.`, deciding each flag's arity as `options` says. Typically pass
    /// `std::env::args().skip(1)`; highest priority when merged last.
    ///
    /// With [`ArgsOptions::types`] the flags layer needs no earlier layer of
    /// the same builder, so it can be built, checked and inspected on its
    /// own. With the default options this is exactly
    /// [`merge_args`](ConfigBuilder::merge_args).
    ///
    /// In every mode only `--`-prefixed tokens are read, everything else is
    /// skipped, a bare `--` ends processing, `--key=value` always sets `key`,
    /// and a key with an empty segment or more than 128 segments is ignored.
    /// The [`Source`] is
    /// [`Source::Args`].
    ///
    /// ```
    /// use ferroly::codec::Value;
    /// use ferroly::config::{ArgsOptions, Config};
    ///
    /// let schema = Value::Object(vec![
    ///     ("context".into(), Value::Int(0)),
    ///     ("verbose".into(), Value::Bool(false)),
    /// ]);
    /// let cfg = Config::builder()
    ///     .merge_args_with(
    ///         ["--context", "-1", "--verbose", "input.txt"],
    ///         ArgsOptions::default().types(&schema).value_follows(),
    ///     )
    ///     .build();
    /// assert_eq!(cfg.get::<i32>("context")?, -1);
    /// assert!(cfg.get::<bool>("verbose")?);
    /// # Ok::<(), ferroly::config::ConfigError>(())
    /// ```
    pub fn merge_args_with<I, S>(self, args: I, options: ArgsOptions) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let settings = parse_args_with(args.into_iter().map(Into::into), &self.value, &options)
            .into_iter()
            .map(|(segs, value)| Setting {
                segs,
                value,
                source: Source::Args,
            })
            .collect();
        self.apply_settings(settings)
    }

    /// [`merge_cli`](ConfigBuilder::merge_cli), typing the dynamic flags
    /// ([`Command::dynamic_flags`](ferroly::cli::Command::dynamic_flags)) with
    /// `options`.
    ///
    /// The parser has already decided which dynamic flags carry a value, so
    /// only [`ArgsOptions::types`] matters here: a dynamic flag naming a
    /// boolean key merges its value as a boolean (`true`/`false`,
    /// `1`/`0`, `yes`/`no`, `on`/`off`), and the occurrences of a list key
    /// collect into one list (`--tags a --tags b` merges `["a", "b"]`). Every
    /// other dynamic value merges as a string, and a dynamic switch as `true`.
    /// Declared arguments merge exactly as with `merge_cli`.
    ///
    /// To have the parser read each flag by its type too (so a boolean key
    /// never takes the next positional), build the command with
    /// [`dynamic_flags_with(dynamic_arity(&schema))`](dynamic_arity).
    ///
    /// ```
    /// use ferroly::cli::{Arg, Command};
    /// use ferroly::codec::Value;
    /// use ferroly::config::{ArgsOptions, Config};
    ///
    /// let schema = Value::Object(vec![(
    ///     "engine".into(),
    ///     Value::Object(vec![
    ///         ("threads".into(), Value::UInt(1)),
    ///         ("deterministic".into(), Value::Bool(false)),
    ///     ]),
    /// )]);
    /// let cmd = Command::new("app")
    ///     .dynamic_flags(|name| name.starts_with("engine."))
    ///     .subcommand(Command::new("serve"));
    /// let m = cmd
    ///     .parse(["--engine.threads", "4", "serve", "--engine.deterministic=false"].map(String::from))
    ///     .unwrap();
    ///
    /// let cfg = Config::builder()
    ///     .merge_value(schema.clone())
    ///     .merge_cli_with(&m, ArgsOptions::default().types(&schema))
    ///     .build();
    /// assert_eq!(cfg.get::<u32>("engine.threads")?, 4);
    /// assert!(!cfg.get::<bool>("engine.deterministic")?);
    /// # Ok::<(), ferroly::config::ConfigError>(())
    /// ```
    #[cfg(feature = "cli")]
    pub fn merge_cli_with(self, matches: &ferroly::cli::Matches, options: ArgsOptions) -> Self {
        let builder = self.merge_cli_where(matches, |s| s != ferroly::cli::ValueSource::Default);
        let mut collected = Collected::default();
        for (name, value) in matches.dynamic() {
            let kind = options
                .types
                .as_ref()
                .map_or(Kind::Unknown, |schema| schema_kind(schema, name));
            let value = match value {
                None => Value::Bool(true),
                Some(v) => match flag_bool(v) {
                    Some(b) if kind == Kind::Switch => Value::Bool(b),
                    _ => Value::Str(v.clone()),
                },
            };
            collected.push(name, value, kind == Kind::List);
        }
        let settings = collected
            .out
            .into_iter()
            .map(|(segs, value)| Setting {
                segs,
                value,
                source: Source::Cli,
            })
            .collect();
        builder.apply_settings(settings)
    }
}

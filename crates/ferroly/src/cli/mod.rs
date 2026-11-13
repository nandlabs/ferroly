//! Builder-based command-line argument parsing.
//!
//! Describe a program's interface with a [`Command`] and a set of [`Arg`]
//! builders, then hand the raw argument list to [`Command::parse`] to obtain a
//! typed [`Matches`] result. Options may be spelled long (`--port 80`,
//! `--port=80`), short (`-p 80`, `-p80`, `-p=80`), or as clustered switches
//! (`-abc`, `-vvv`); a bare `--` stops flag processing so every following token
//! is treated as positional.
//!
//! Every argument resolves by a fixed precedence: an explicit argument on the
//! command line wins, otherwise an environment variable named via
//! [`Arg::env`] is consulted, and finally a [`Arg::default`] is applied. This
//! holds for value options, boolean switches, counters and positionals alike.
//! An argument marked [`Arg::required`] with none of those present yields a
//! [`CliError`].
//!
//! Requesting `--help` (or `-h`) returns [`CliError::HelpRequested`] carrying
//! the rendered help text, and `--version` (or `-V`, when a version is set)
//! returns [`CliError::VersionRequested`]. A flag the program declares itself
//! always wins over these built-ins, so a tool may use `-h` for `--host`.
//! [`CliError::exit_code`] maps those two requests to `0` and every real parse
//! error to `2`; [`Command::parse_or_exit`] and [`Command::run`] apply that
//! convention for you.
//!
//! Commands nest: attach a [`Command::subcommand`] and the first non-flag token
//! matching its name dispatches the remaining arguments to that child, whose
//! result is reachable through [`Matches::subcommand`]. A flag marked
//! [`Arg::global`] is accepted before or after the subcommand name.
//!
//! ```
//! use ferroly::cli::{Arg, Command};
//!
//! let cmd = Command::new("greet")
//!     .about("Print a friendly greeting")
//!     .version("1.0")
//!     .flag(Arg::new("name").short('n').long("name").default("world").help("who to greet"))
//!     .flag(Arg::new("loud").short('l').long("loud").boolean().help("shout it"));
//!
//! let raw = ["--name", "ferris", "-l"].map(String::from);
//! let m = cmd.parse(raw).unwrap();
//! assert_eq!(m.get("name"), Some("ferris"));
//! assert!(m.get_bool("loud"));
//! ```

#![deny(missing_docs)]

mod completion;
mod help;

use std::collections::HashMap;
use std::env;
use std::fmt;
use std::process::ExitCode;
use std::str::FromStr;
use std::sync::Arc;

pub use completion::Shell;

/// Errors produced while parsing arguments or converting values.
///
/// [`HelpRequested`](CliError::HelpRequested) and
/// [`VersionRequested`](CliError::VersionRequested) are not failures: they
/// carry text to print before exiting successfully. See
/// [`CliError::exit_code`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CliError {
    /// A flag was supplied that the [`Command`] does not define.
    UnknownFlag {
        /// The offending flag, spelled as it appeared (e.g. `--verbose`).
        flag: String,
        /// The closest declared flag, when one is similar enough.
        suggestion: Option<String>,
    },

    /// A word appeared where a subcommand was expected, and no subcommand has
    /// that name.
    UnknownCommand {
        /// The word that was given.
        name: String,
        /// The closest declared subcommand, when one is similar enough.
        suggestion: Option<String>,
    },

    /// A positional argument was supplied that the command has no slot for.
    UnexpectedArgument {
        /// The surplus argument.
        arg: String,
    },

    /// A value option appeared with no value following it.
    MissingValue {
        /// The flag that expected a value.
        flag: String,
    },

    /// A required argument had no command-line value, environment fallback, or
    /// default.
    MissingRequired {
        /// The declared argument name.
        name: String,
    },

    /// The command requires a subcommand and none was given.
    MissingSubcommand {
        /// The command (full path) that needs a subcommand.
        command: String,
    },

    /// A raw value could not be converted into the requested type.
    InvalidValue {
        /// The declared argument name.
        name: String,
        /// The raw value that failed to convert.
        value: String,
    },

    /// A value is not one of the argument's declared [`Arg::choices`].
    InvalidChoice {
        /// The declared argument name.
        name: String,
        /// The rejected value.
        value: String,
        /// The values that would have been accepted.
        choices: Vec<String>,
    },

    /// A value was rejected by the argument's [`Arg::validator`] (or
    /// [`Arg::typed`]).
    ValidationFailed {
        /// The declared argument name.
        name: String,
        /// The rejected value.
        value: String,
        /// Why the validator rejected it.
        reason: String,
    },

    /// Two arguments of a mutually exclusive [`Group`] were both supplied.
    Conflict {
        /// The first of the two, spelled as a flag (e.g. `--json`).
        first: String,
        /// The second of the two.
        second: String,
    },

    /// One argument of a required-together [`Group`] was supplied without
    /// another.
    RequiresTogether {
        /// The argument that was supplied.
        present: String,
        /// The argument that must accompany it.
        missing: String,
    },

    /// None of the arguments of a required [`Group`] was supplied.
    MissingOneOf {
        /// The group's arguments, spelled as flags.
        args: Vec<String>,
    },

    /// `--help`/`-h` was requested; the payload is the rendered help text.
    HelpRequested(String),

    /// `--version`/`-V` was requested; the payload is the version line.
    VersionRequested(String),
}

impl CliError {
    /// The conventional process exit status for this outcome: `0` for
    /// [`HelpRequested`](CliError::HelpRequested) and
    /// [`VersionRequested`](CliError::VersionRequested), `2` for every parse
    /// error.
    pub fn exit_code(&self) -> u8 {
        if self.is_informational() {
            0
        } else {
            2
        }
    }

    /// Whether this is a help or version request rather than a failure.
    pub fn is_informational(&self) -> bool {
        matches!(
            self,
            CliError::HelpRequested(_) | CliError::VersionRequested(_)
        )
    }

    /// Prints the outcome and terminates the process with
    /// [`exit_code`](CliError::exit_code): help and version text go to standard
    /// output, parse errors to standard error.
    pub fn exit(&self) -> ! {
        let code = self.report(&mut std::io::stdout(), &mut std::io::stderr());
        std::process::exit(i32::from(code))
    }

    /// Writes the outcome to the fitting stream and returns the exit status.
    fn report(&self, out: &mut dyn std::io::Write, err: &mut dyn std::io::Write) -> u8 {
        // A closed pipe must not turn `--help | head` into a panic.
        if self.is_informational() {
            let _ = writeln!(out, "{}", self.to_string().trim_end());
            let _ = out.flush();
        } else {
            let _ = writeln!(err, "error: {self}");
            let _ = writeln!(err, "\nFor more information, try '--help'.");
            let _ = err.flush();
        }
        self.exit_code()
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::UnknownFlag { flag, suggestion } => {
                write!(f, "unknown flag: {flag}")?;
                if let Some(s) = suggestion {
                    write!(f, " (did you mean '{s}'?)")?;
                }
                Ok(())
            }
            CliError::UnknownCommand { name, suggestion } => {
                write!(f, "unknown command: {name}")?;
                if let Some(s) = suggestion {
                    write!(f, " (did you mean '{s}'?)")?;
                }
                Ok(())
            }
            CliError::UnexpectedArgument { arg } => write!(f, "unexpected argument: {arg}"),
            CliError::MissingValue { flag } => write!(f, "missing value for flag: {flag}"),
            CliError::MissingRequired { name } => write!(f, "missing required argument: {name}"),
            CliError::MissingSubcommand { command } => {
                write!(f, "'{command}' requires a subcommand")
            }
            CliError::InvalidValue { name, value } => {
                write!(f, "invalid value '{value}' for argument '{name}'")
            }
            CliError::InvalidChoice {
                name,
                value,
                choices,
            } => write!(
                f,
                "invalid value '{value}' for argument '{name}' (possible values: {})",
                choices.join(", ")
            ),
            CliError::ValidationFailed {
                name,
                value,
                reason,
            } => write!(f, "invalid value '{value}' for argument '{name}': {reason}"),
            CliError::Conflict { first, second } => {
                write!(f, "{first} cannot be used with {second}")
            }
            CliError::RequiresTogether { present, missing } => {
                write!(f, "{present} requires {missing}")
            }
            CliError::MissingOneOf { args } => {
                write!(f, "one of {} is required", args.join(", "))
            }
            CliError::HelpRequested(text) | CliError::VersionRequested(text) => f.write_str(text),
        }
    }
}

impl std::error::Error for CliError {}

#[cfg(feature = "errutils")]
impl ferroly::errutils::Classify for CliError {
    /// Every parse error is the caller's input
    /// ([`InvalidInput`](ferroly::errutils::ErrorKind::InvalidInput)); help and
    /// version requests are not failures and report
    /// [`Unknown`](ferroly::errutils::ErrorKind::Unknown).
    fn kind(&self) -> ferroly::errutils::ErrorKind {
        if self.is_informational() {
            ferroly::errutils::ErrorKind::Unknown
        } else {
            ferroly::errutils::ErrorKind::InvalidInput
        }
    }
}

/// Where a resolved argument value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValueSource {
    /// Given on the command line.
    CommandLine,
    /// Read from the environment variable named by [`Arg::env`].
    Env,
    /// The [`Arg::default`] value.
    Default,
}

/// A resolved argument value, as returned by [`Matches::value`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ArgValue<'a> {
    /// A boolean switch.
    Bool(bool),
    /// A counted flag ([`Arg::count`]).
    Count(usize),
    /// A single value.
    One(&'a str),
    /// The values of a repeatable argument ([`Arg::multiple`] or
    /// [`Arg::variadic`]).
    Many(&'a [String]),
}

type ValidatorFn = dyn Fn(&str) -> Result<(), String> + Send + Sync;

/// A shared parse-time validation callback (opaque in `Debug` output).
#[derive(Clone)]
struct Validator(Arc<ValidatorFn>);

impl fmt::Debug for Validator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Validator(..)")
    }
}

/// A single argument declaration: an option, a boolean switch, a counter, or a
/// positional.
///
/// Build one with [`Arg::new`] and refine it through the chained setters.
#[derive(Debug, Clone)]
pub struct Arg {
    name: String,
    short: Option<char>,
    long: Option<String>,
    aliases: Vec<String>,
    short_aliases: Vec<char>,
    help: Option<String>,
    required: bool,
    default: Option<String>,
    env: Option<String>,
    boolean: bool,
    count: bool,
    multiple: bool,
    delimiter: Option<char>,
    variadic: bool,
    global: bool,
    choices: Vec<String>,
    validator: Option<Validator>,
}

impl Arg {
    /// Starts a new argument identified by `name`; the name is the key used to
    /// look the value up in [`Matches`].
    pub fn new(name: impl Into<String>) -> Self {
        Arg {
            name: name.into(),
            short: None,
            long: None,
            aliases: Vec::new(),
            short_aliases: Vec::new(),
            help: None,
            required: false,
            default: None,
            env: None,
            boolean: false,
            count: false,
            multiple: false,
            delimiter: None,
            variadic: false,
            global: false,
            choices: Vec::new(),
            validator: None,
        }
    }

    /// Sets the single-character short form (matched as `-c`).
    pub fn short(mut self, c: char) -> Self {
        self.short = Some(c);
        self
    }

    /// Sets the long form (matched as `--long`).
    pub fn long(mut self, long: impl Into<String>) -> Self {
        self.long = Some(long.into());
        self
    }

    /// Adds an alternative long spelling (matched as `--alias`). May be called
    /// repeatedly.
    pub fn alias(mut self, alias: impl Into<String>) -> Self {
        self.aliases.push(alias.into());
        self
    }

    /// Adds an alternative short spelling (matched as `-c`). May be called
    /// repeatedly.
    pub fn short_alias(mut self, c: char) -> Self {
        self.short_aliases.push(c);
        self
    }

    /// Sets the one-line description shown in generated help.
    pub fn help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    /// Marks the argument as required; parsing fails if it resolves to nothing.
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// Sets the fallback value used when nothing else supplies one. On a
    /// boolean switch the value is read as a boolean (`"true"`, `"false"`, …),
    /// on a counter as a number, and on a repeatable argument with a
    /// [`delimiter`](Arg::delimiter) it is split.
    pub fn default(mut self, value: impl Into<String>) -> Self {
        self.default = Some(value.into());
        self
    }

    /// Names an environment variable consulted before the default. Its value is
    /// interpreted exactly like a [`default`](Arg::default); for a boolean
    /// switch an empty value reads as `false`.
    pub fn env(mut self, var: impl Into<String>) -> Self {
        self.env = Some(var.into());
        self
    }

    /// Turns the argument into a boolean switch: its presence alone means true
    /// and it never consumes a following value. An explicit `--flag=false` /
    /// `--flag=true` (also `1`/`0`, `yes`/`no`, `on`/`off`) is honoured.
    pub fn boolean(mut self) -> Self {
        self.boolean = true;
        self
    }

    /// Turns the argument into a counter: every occurrence adds one
    /// (`-vvv` → 3, `--verbose --verbose` → 2). Read it with
    /// [`Matches::get_count`].
    pub fn count(mut self) -> Self {
        self.count = true;
        self
    }

    /// Lets a value option be given more than once, keeping every value
    /// (`--tag a --tag b`). Read them with [`Matches::get_all`]. Without this a
    /// repeated option keeps only its last value.
    pub fn multiple(mut self) -> Self {
        self.multiple = true;
        self
    }

    /// Splits each supplied value on `delimiter` (`--tag a,b` → `a`, `b`).
    /// Implies [`multiple`](Arg::multiple).
    pub fn delimiter(mut self, delimiter: char) -> Self {
        self.delimiter = Some(delimiter);
        self.multiple = true;
        self
    }

    /// For the **last** positional of a command: collect every remaining
    /// positional token (`cp <src>... `). Read them with [`Matches::get_all`].
    pub fn variadic(mut self) -> Self {
        self.variadic = true;
        self
    }

    /// Makes a flag available to every nested subcommand as well, so it may be
    /// written before or after the subcommand name (`app -v serve` and
    /// `app serve -v`). The value is visible in the [`Matches`] of the command
    /// that declares it and of every subcommand below it.
    pub fn global(mut self) -> Self {
        self.global = true;
        self
    }

    /// Restricts the accepted values to `choices`; anything else fails parsing
    /// with [`CliError::InvalidChoice`].
    pub fn choices<I, S>(mut self, choices: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.choices = choices.into_iter().map(Into::into).collect();
        self
    }

    /// Validates every value at parse time; an `Err(reason)` fails parsing with
    /// [`CliError::ValidationFailed`]. Applies to command-line, environment and
    /// default values alike.
    pub fn validator<F>(mut self, f: F) -> Self
    where
        F: Fn(&str) -> Result<(), String> + Send + Sync + 'static,
    {
        self.validator = Some(Validator(Arc::new(f)));
        self
    }

    /// Requires every value to parse as `T` at parse time, so a bad value is
    /// reported by [`Command::parse`] instead of surfacing later from
    /// [`Matches::get_as`].
    pub fn typed<T>(self) -> Self
    where
        T: FromStr + 'static,
        T::Err: fmt::Display,
    {
        self.validator(|raw| raw.parse::<T>().map(|_| ()).map_err(|e| e.to_string()))
    }

    /// Whether this argument consumes a value (neither a switch nor a counter).
    fn takes_value(&self) -> bool {
        !self.boolean && !self.count
    }

    fn matches_long(&self, key: &str) -> bool {
        self.long.as_deref() == Some(key) || self.aliases.iter().any(|a| a == key)
    }

    fn matches_short(&self, c: char) -> bool {
        self.short == Some(c) || self.short_aliases.contains(&c)
    }

    /// How the argument is written on the command line, for messages.
    fn spelled(&self) -> String {
        match (&self.long, self.short) {
            (Some(l), _) => format!("--{l}"),
            (None, Some(c)) => format!("-{c}"),
            (None, None) => format!("<{}>", self.name),
        }
    }

    fn split(&self, raw: &str, out: &mut Vec<String>) {
        match self.delimiter {
            Some(d) => out.extend(
                raw.split(d)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
            ),
            None => out.push(raw.to_string()),
        }
    }

    /// Interprets a fallback (environment or default) string for this argument.
    fn data_from_str(&self, raw: &str) -> Result<Data, CliError> {
        let invalid = || CliError::InvalidValue {
            name: self.name.clone(),
            value: raw.to_string(),
        };
        if self.count {
            return raw
                .trim()
                .parse::<usize>()
                .map(Data::Count)
                .map_err(|_| invalid());
        }
        if self.boolean {
            if raw.trim().is_empty() {
                return Ok(Data::Bool(false));
            }
            return parse_bool(raw).map(Data::Bool).ok_or_else(invalid);
        }
        if self.multiple || self.variadic {
            let mut values = Vec::new();
            self.split(raw, &mut values);
            return Ok(Data::Many(values));
        }
        Ok(Data::One(raw.to_string()))
    }

    /// Applies the choices and validator checks to one value.
    fn check(&self, value: &str) -> Result<(), CliError> {
        if !self.choices.is_empty() && !self.choices.iter().any(|c| c == value) {
            return Err(CliError::InvalidChoice {
                name: self.name.clone(),
                value: value.to_string(),
                choices: self.choices.clone(),
            });
        }
        if let Some(v) = &self.validator {
            (v.0)(value).map_err(|reason| CliError::ValidationFailed {
                name: self.name.clone(),
                value: value.to_string(),
                reason,
            })?;
        }
        Ok(())
    }

    fn check_data(&self, data: &Data) -> Result<(), CliError> {
        match data {
            Data::One(v) => self.check(v),
            Data::Many(vs) => vs.iter().try_for_each(|v| self.check(v)),
            Data::Bool(_) | Data::Count(_) => Ok(()),
        }
    }

    /// Environment value first, then the default.
    fn fallback(&self) -> Result<Option<Entry>, CliError> {
        if let Some(var) = &self.env {
            // A variable that is unset (or not valid UTF-8) is simply absent.
            if let Ok(raw) = env::var(var) {
                return Ok(Some(Entry {
                    data: self.data_from_str(&raw)?,
                    source: ValueSource::Env,
                }));
            }
        }
        match &self.default {
            Some(raw) => Ok(Some(Entry {
                data: self.data_from_str(raw)?,
                source: ValueSource::Default,
            })),
            None => Ok(None),
        }
    }
}

/// A constraint across several arguments of one [`Command`].
///
/// Members are referred to by their [`Arg::new`] names. An argument counts as
/// *supplied* when it came from the command line or the environment — a
/// default alone never triggers a group rule.
///
/// ```
/// use ferroly::cli::{Arg, CliError, Command, Group};
///
/// let cmd = Command::new("export")
///     .flag(Arg::new("json").long("json").boolean())
///     .flag(Arg::new("yaml").long("yaml").boolean())
///     .group(Group::exclusive(["json", "yaml"]));
///
/// let err = cmd.parse(["--json", "--yaml"].map(String::from)).unwrap_err();
/// assert!(matches!(err, CliError::Conflict { .. }));
/// ```
#[derive(Debug, Clone)]
pub struct Group {
    members: Vec<String>,
    kind: GroupKind,
    required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupKind {
    Exclusive,
    Together,
}

impl Group {
    /// A mutually exclusive group: at most one member may be supplied.
    pub fn exclusive<I, S>(members: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Group {
            members: members.into_iter().map(Into::into).collect(),
            kind: GroupKind::Exclusive,
            required: false,
        }
    }

    /// A required-together group: supplying any member requires all of them.
    pub fn together<I, S>(members: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Group {
            members: members.into_iter().map(Into::into).collect(),
            kind: GroupKind::Together,
            required: false,
        }
    }

    /// Makes the group mandatory: an exclusive group then needs exactly one
    /// member, a together group needs all of them.
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }
}

/// A description of a command: its options, positionals, and subcommands.
///
/// Assemble one through the chained builders, then call [`Command::parse`] or
/// [`Command::parse_env`].
#[derive(Debug, Clone)]
pub struct Command {
    name: String,
    aliases: Vec<String>,
    about: Option<String>,
    version: Option<String>,
    flags: Vec<Arg>,
    positionals: Vec<Arg>,
    subcommands: Vec<Command>,
    groups: Vec<Group>,
    subcommand_required: bool,
}

impl Command {
    /// Starts a new command named `name`.
    pub fn new(name: impl Into<String>) -> Self {
        Command {
            name: name.into(),
            aliases: Vec::new(),
            about: None,
            version: None,
            flags: Vec::new(),
            positionals: Vec::new(),
            subcommands: Vec::new(),
            groups: Vec::new(),
            subcommand_required: false,
        }
    }

    /// Adds an alternative name under which this command can be invoked as a
    /// subcommand. May be called repeatedly.
    pub fn alias(mut self, alias: impl Into<String>) -> Self {
        self.aliases.push(alias.into());
        self
    }

    /// Sets the short description shown in help.
    pub fn about(mut self, about: impl Into<String>) -> Self {
        self.about = Some(about.into());
        self
    }

    /// Sets the version string shown in help and printed for `--version` /
    /// `-V`. Without a version those two flags are not recognised.
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// Adds a flag, counter or value option.
    pub fn flag(mut self, arg: Arg) -> Self {
        self.flags.push(arg);
        self
    }

    /// Adds a positional argument; positionals bind in the order declared.
    /// Only the last one may be [`variadic`](Arg::variadic).
    pub fn positional(mut self, arg: Arg) -> Self {
        self.positionals.push(arg);
        self
    }

    /// Adds a nested subcommand.
    pub fn subcommand(mut self, cmd: Command) -> Self {
        self.subcommands.push(cmd);
        self
    }

    /// Adds a cross-argument constraint; see [`Group`].
    pub fn group(mut self, group: Group) -> Self {
        self.groups.push(group);
        self
    }

    /// Makes parsing fail with [`CliError::MissingSubcommand`] when no
    /// subcommand is given.
    pub fn subcommand_required(mut self) -> Self {
        self.subcommand_required = true;
        self
    }

    /// Parses `args` (the argument list, excluding the program name) into a
    /// [`Matches`].
    pub fn parse<I>(&self, args: I) -> Result<Matches, CliError>
    where
        I: IntoIterator<Item = String>,
    {
        let argv: Vec<String> = args.into_iter().collect();
        self.parse_slice(&argv, &[], &[])
    }

    /// Parses the current process arguments, skipping the program name.
    /// Arguments that are not valid UTF-8 are converted lossily.
    pub fn parse_env(&self) -> Result<Matches, CliError> {
        self.parse(
            env::args_os()
                .skip(1)
                .map(|a| a.to_string_lossy().into_owned()),
        )
    }

    /// Like [`parse`](Command::parse), but handles the non-`Ok` outcomes the
    /// conventional way instead of returning them: help and version text are
    /// printed to standard output and the process exits with status `0`; a
    /// parse error is printed to standard error and the process exits with
    /// status `2`.
    pub fn parse_or_exit<I>(&self, args: I) -> Matches
    where
        I: IntoIterator<Item = String>,
    {
        match self.parse(args) {
            Ok(m) => m,
            Err(e) => e.exit(),
        }
    }

    /// [`parse_or_exit`](Command::parse_or_exit) over the current process
    /// arguments.
    pub fn parse_env_or_exit(&self) -> Matches {
        match self.parse_env() {
            Ok(m) => m,
            Err(e) => e.exit(),
        }
    }

    /// Parses `args`, runs `handler` on the result and turns the outcome into a
    /// process exit status — a complete `main` body:
    ///
    /// | Outcome | Output | Status |
    /// |---|---|---|
    /// | `--help` / `--version` | text on standard output | `0` |
    /// | parse error | message on standard error | `2` |
    /// | `handler` returns `Err(e)` | `error: {e}` on standard error | `1` |
    /// | `handler` returns `Ok(())` | — | `0` |
    ///
    /// ```no_run
    /// use ferroly::cli::{Arg, Command};
    /// use std::process::ExitCode;
    ///
    /// fn main() -> ExitCode {
    ///     Command::new("tool")
    ///         .flag(Arg::new("name").long("name").default("world"))
    ///         .run(std::env::args().skip(1), |m| -> Result<(), String> {
    ///             println!("hello {}", m.get("name").unwrap_or_default());
    ///             Ok(())
    ///         })
    /// }
    /// ```
    pub fn run<I, F, E>(&self, args: I, handler: F) -> ExitCode
    where
        I: IntoIterator<Item = String>,
        F: FnOnce(&Matches) -> Result<(), E>,
        E: fmt::Display,
    {
        ExitCode::from(self.run_with(
            args,
            handler,
            &mut std::io::stdout(),
            &mut std::io::stderr(),
        ))
    }

    fn run_with<I, F, E>(
        &self,
        args: I,
        handler: F,
        out: &mut dyn std::io::Write,
        err: &mut dyn std::io::Write,
    ) -> u8
    where
        I: IntoIterator<Item = String>,
        F: FnOnce(&Matches) -> Result<(), E>,
        E: fmt::Display,
    {
        match self.parse(args) {
            Err(e) => e.report(out, err),
            Ok(m) => match handler(&m) {
                Ok(()) => 0,
                Err(e) => {
                    let _ = writeln!(err, "error: {e}");
                    let _ = err.flush();
                    1
                }
            },
        }
    }

    fn find_subcommand(&self, name: &str) -> Option<&Command> {
        self.subcommands
            .iter()
            .find(|s| s.name == name || s.aliases.iter().any(|a| a == name))
    }

    fn defines(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f.name == name)
    }

    /// This command's own flags followed by the inherited global flags it does
    /// not shadow.
    fn effective<'a>(&'a self, inherited: &[&'a Arg]) -> Vec<&'a Arg> {
        let mut all: Vec<&Arg> = self.flags.iter().collect();
        all.extend(inherited.iter().copied().filter(|a| !self.defines(&a.name)));
        all
    }

    /// Parses one command level. `inherited` holds the global flags of the
    /// ancestors and `path` their names (for usage lines).
    fn parse_slice(
        &self,
        argv: &[String],
        inherited: &[&Arg],
        path: &[&str],
    ) -> Result<Matches, CliError> {
        let effective = self.effective(inherited);
        let own = self.flags.len();
        let find_long = |key: &str| effective.iter().copied().find(|a| a.matches_long(key));
        let find_short = |c: char| effective.iter().copied().find(|a| a.matches_short(c));
        let help = || CliError::HelpRequested(self.render_help(path, inherited));
        let version = || {
            self.version
                .as_ref()
                .map(|v| CliError::VersionRequested(format!("{} {v}", self.name)))
        };

        let last_variadic = self.positionals.last().is_some_and(|p| p.variadic);
        let mut raw: HashMap<&str, Raw> = HashMap::new();
        let mut positionals: Vec<String> = Vec::new();
        let mut subcommand: Option<(&Command, Matches)> = None;
        let mut no_more_flags = false;
        let mut i = 0;

        while i < argv.len() {
            let tok = &argv[i];

            if !no_more_flags && tok == "--" {
                no_more_flags = true;
                i += 1;
                continue;
            }

            // Long form: `--long`, `--long value`, or `--long=value`.
            if !no_more_flags && tok.starts_with("--") {
                let body = &tok[2..];
                let (key, inline) = match body.split_once('=') {
                    Some((k, v)) => (k, Some(v)),
                    None => (body, None),
                };
                let Some(arg) = find_long(key) else {
                    // Built-ins apply only when the program has not claimed
                    // the spelling for a flag of its own.
                    if key == "help" {
                        return Err(help());
                    }
                    if key == "version" {
                        if let Some(v) = version() {
                            return Err(v);
                        }
                    }
                    let known = effective
                        .iter()
                        .flat_map(|a| a.long.iter().chain(a.aliases.iter()))
                        .map(String::as_str)
                        .chain(std::iter::once("help"))
                        .chain(self.version.iter().map(|_| "version"));
                    return Err(CliError::UnknownFlag {
                        flag: format!("--{key}"),
                        suggestion: closest(key, known).map(|s| format!("--{s}")),
                    });
                };
                let slot = raw.entry(arg.name.as_str()).or_default();
                if arg.takes_value() {
                    let val = match inline {
                        Some(v) => v.to_string(),
                        None => {
                            i += 1;
                            argv.get(i).cloned().ok_or_else(|| CliError::MissingValue {
                                flag: format!("--{key}"),
                            })?
                        }
                    };
                    slot.push(arg, &val);
                } else {
                    slot.switch(arg, inline)?;
                }
                i += 1;
                continue;
            }

            // Short form: `-a`, clustered `-abc`, `-p value`, `-pvalue`, `-p=value`.
            // A token such as `-5` or `-1.5` is a (negative) number, not a flag
            // cluster, unless the program really declares a `-5`-style flag.
            if !no_more_flags && tok.starts_with('-') && tok.len() > 1 {
                let chars: Vec<char> = tok[1..].chars().collect();
                if find_short(chars[0]).is_some() || !is_negative_number(tok) {
                    let mut j = 0;
                    while j < chars.len() {
                        let c = chars[j];
                        let Some(arg) = find_short(c) else {
                            if c == 'h' {
                                return Err(help());
                            }
                            if c == 'V' {
                                if let Some(v) = version() {
                                    return Err(v);
                                }
                            }
                            return Err(CliError::UnknownFlag {
                                flag: format!("-{c}"),
                                suggestion: None,
                            });
                        };
                        let rest: String = chars[j + 1..].iter().collect();
                        let slot = raw.entry(arg.name.as_str()).or_default();
                        if arg.takes_value() {
                            let val = if rest.is_empty() {
                                i += 1;
                                argv.get(i).cloned().ok_or_else(|| CliError::MissingValue {
                                    flag: format!("-{c}"),
                                })?
                            } else {
                                // `-n=5` means the same as `-n5`.
                                rest.strip_prefix('=').unwrap_or(&rest).to_string()
                            };
                            slot.push(arg, &val);
                            break;
                        }
                        if let Some(inline) = rest.strip_prefix('=') {
                            slot.switch(arg, Some(inline))?;
                            break;
                        }
                        slot.switch(arg, None)?;
                        j += 1;
                    }
                    i += 1;
                    continue;
                }
            }

            // A non-flag token: dispatch to a subcommand or collect positionally.
            if !no_more_flags && positionals.is_empty() && !self.subcommands.is_empty() {
                if let Some(sub) = self.find_subcommand(tok) {
                    let globals: Vec<&Arg> = effective
                        .iter()
                        .enumerate()
                        .filter(|(idx, a)| *idx >= own || a.global)
                        .map(|(_, a)| *a)
                        .collect();
                    let mut child_path = path.to_vec();
                    child_path.push(self.name.as_str());
                    let child = sub.parse_slice(&argv[i + 1..], &globals, &child_path)?;
                    subcommand = Some((sub, child));
                    break;
                }
                if self.positionals.is_empty() {
                    let known = self
                        .subcommands
                        .iter()
                        .flat_map(|s| std::iter::once(&s.name).chain(s.aliases.iter()))
                        .map(String::as_str);
                    return Err(CliError::UnknownCommand {
                        name: tok.clone(),
                        suggestion: closest(tok, known).map(str::to_string),
                    });
                }
            }
            if positionals.len() >= self.positionals.len() && !last_variadic {
                return Err(CliError::UnexpectedArgument { arg: tok.clone() });
            }
            positionals.push(tok.clone());
            i += 1;
        }

        // Resolve every flag by precedence: command line > environment > default.
        let mut args: HashMap<String, Entry> = HashMap::new();
        for (idx, arg) in effective.iter().copied().enumerate() {
            let inherited_here = idx >= own;
            let mut explicit = raw.remove(arg.name.as_str());
            // A global flag written after the subcommand name was parsed by
            // the child; fold those occurrences into this level.
            if inherited_here || arg.global {
                if let Some((sub, child)) = &subcommand {
                    if !sub.defines(&arg.name) {
                        if let Some(entry) = child.args.get(&arg.name) {
                            if entry.source == ValueSource::CommandLine {
                                explicit
                                    .get_or_insert_with(Raw::default)
                                    .absorb(&entry.data);
                            }
                        }
                    }
                }
            }
            let entry = match explicit {
                Some(raw) => Some(Entry {
                    data: raw.into_data(arg),
                    source: ValueSource::CommandLine,
                }),
                None => arg.fallback()?,
            };
            match entry {
                Some(entry) => {
                    arg.check_data(&entry.data)?;
                    args.insert(arg.name.clone(), entry);
                }
                // An inherited flag is enforced by the command declaring it.
                None if arg.required && !inherited_here => {
                    return Err(CliError::MissingRequired {
                        name: arg.name.clone(),
                    });
                }
                None => {}
            }
        }

        // Positionals only apply when no subcommand took over.
        if subcommand.is_none() {
            let supplied = positionals.len();
            let mut contiguous = true;
            for (idx, p) in self.positionals.iter().enumerate() {
                let last = idx + 1 == self.positionals.len();
                let entry = if idx < supplied {
                    let data = if p.variadic && last {
                        Data::Many(positionals[idx..supplied].to_vec())
                    } else {
                        Data::One(positionals[idx].clone())
                    };
                    Some(Entry {
                        data,
                        source: ValueSource::CommandLine,
                    })
                } else {
                    let fallback = p.fallback()?;
                    // Fallback values join the positional list for as long as
                    // no slot before them is empty.
                    match &fallback {
                        Some(entry) if contiguous => match &entry.data {
                            Data::One(v) => positionals.push(v.clone()),
                            Data::Many(vs) => positionals.extend(vs.iter().cloned()),
                            Data::Bool(_) | Data::Count(_) => {}
                        },
                        _ => contiguous = false,
                    }
                    fallback
                };
                match entry {
                    Some(entry) => {
                        p.check_data(&entry.data)?;
                        args.insert(p.name.clone(), entry);
                    }
                    None if p.required => {
                        return Err(CliError::MissingRequired {
                            name: p.name.clone(),
                        });
                    }
                    None => {}
                }
            }
        }

        self.check_groups(&effective, &args)?;

        if self.subcommand_required && subcommand.is_none() {
            let mut full: Vec<&str> = path.to_vec();
            full.push(self.name.as_str());
            return Err(CliError::MissingSubcommand {
                command: full.join(" "),
            });
        }

        // Hand the final value of every global flag down, so each level of the
        // result reports the same thing.
        let subcommand = subcommand.map(|(sub, mut child)| {
            for (idx, arg) in effective.iter().copied().enumerate() {
                if idx >= own || arg.global {
                    propagate(sub, &mut child, &arg.name, args.get(&arg.name));
                }
            }
            (sub.name.clone(), Box::new(child))
        });

        Ok(Matches {
            args,
            positionals,
            subcommand,
        })
    }

    fn check_groups(
        &self,
        effective: &[&Arg],
        args: &HashMap<String, Entry>,
    ) -> Result<(), CliError> {
        let spelled = |name: &str| {
            effective
                .iter()
                .copied()
                .chain(self.positionals.iter())
                .find(|a| a.name == name)
                .map_or_else(|| name.to_string(), Arg::spelled)
        };
        for group in &self.groups {
            let supplied: Vec<&str> = group
                .members
                .iter()
                .map(String::as_str)
                .filter(|m| {
                    args.get(*m)
                        .is_some_and(|e| e.source != ValueSource::Default)
                })
                .collect();
            match group.kind {
                GroupKind::Exclusive => {
                    if supplied.len() > 1 {
                        return Err(CliError::Conflict {
                            first: spelled(supplied[0]),
                            second: spelled(supplied[1]),
                        });
                    }
                    if group.required && supplied.is_empty() {
                        return Err(CliError::MissingOneOf {
                            args: group.members.iter().map(|m| spelled(m)).collect(),
                        });
                    }
                }
                GroupKind::Together => {
                    // A member that resolved to a default still counts as
                    // being there; only a member with no value at all is
                    // missing.
                    let missing = group.members.iter().find(|m| !args.contains_key(*m));
                    match (supplied.first(), missing) {
                        (Some(present), Some(missing)) => {
                            return Err(CliError::RequiresTogether {
                                present: spelled(present),
                                missing: spelled(missing),
                            });
                        }
                        (None, Some(missing)) if group.required => {
                            return Err(CliError::MissingRequired {
                                name: missing.clone(),
                            });
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    /// Renders the help text: the usage line, the description, positionals,
    /// every option with its spellings and any default, environment or
    /// possible-value annotation, and the list of subcommands. Long
    /// descriptions are wrapped.
    ///
    /// Help requested through `--help` on a subcommand additionally shows the
    /// parent commands in the usage line and the inherited global options.
    pub fn help(&self) -> String {
        self.render_help(&[], &[])
    }

    /// Generates a shell completion script for this command tree. Source the
    /// output from the shell's start-up file, or install it where the shell
    /// looks for completions.
    pub fn completions(&self, shell: Shell) -> String {
        completion::generate(self, shell)
    }
}

/// Writes the final value of the global flag `name` into `matches` and every
/// nested subcommand result, stopping at a command that declares its own flag
/// of that name.
fn propagate(cmd: &Command, matches: &mut Matches, name: &str, entry: Option<&Entry>) {
    if cmd.defines(name) {
        return;
    }
    match entry {
        Some(e) => {
            matches.args.insert(name.to_string(), e.clone());
        }
        None => {
            matches.args.remove(name);
        }
    }
    if let Some((sub_name, child)) = &mut matches.subcommand {
        if let Some(sub) = cmd.find_subcommand(sub_name) {
            propagate(sub, child, name, entry);
        }
    }
}

/// Reads a boolean the way flags and environment variables usually spell it.
fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// `-5`, `-1.5`, `-.5`, `-1e3`: a dash followed by something numeric.
fn is_negative_number(tok: &str) -> bool {
    let body = &tok[1..];
    body.starts_with(|c: char| c.is_ascii_digit() || c == '.') && body.parse::<f64>().is_ok()
}

/// The candidate closest to `input` by edit distance, if it is close enough to
/// be a plausible typo.
fn closest<'a>(input: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let len = input.chars().count();
    if len == 0 {
        return None;
    }
    let limit = len.div_ceil(3);
    let mut best: Option<(usize, &str)> = None;
    for cand in candidates {
        let d = if len >= 2 && cand.starts_with(input) {
            // An unfinished word is the likeliest typo of all.
            0
        } else {
            edit_distance(input, cand)
        };
        if d <= limit && d < cand.chars().count() && best.map_or(true, |(b, _)| d < b) {
            best = Some((d, cand));
        }
    }
    best.map(|(_, cand)| cand)
}

/// Levenshtein distance with adjacent transpositions counted as one edit.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev2: Vec<usize> = vec![0; b.len() + 1];
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur: Vec<usize> = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut d = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d = d.min(prev2[j - 2] + 1);
            }
            cur[j] = d;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// What the command line said about one flag, before resolution.
#[derive(Debug, Default)]
struct Raw {
    values: Vec<String>,
    count: usize,
    flag: Option<bool>,
}

impl Raw {
    /// Records a value for a value option.
    fn push(&mut self, arg: &Arg, value: &str) {
        if arg.multiple {
            arg.split(value, &mut self.values);
        } else {
            self.values = vec![value.to_string()];
        }
    }

    /// Records an occurrence of a switch or counter, with its optional
    /// `=value`.
    fn switch(&mut self, arg: &Arg, inline: Option<&str>) -> Result<(), CliError> {
        let invalid = |value: &str| CliError::InvalidValue {
            name: arg.name.clone(),
            value: value.to_string(),
        };
        if arg.count {
            self.count += match inline {
                None => 1,
                Some(v) => v.parse::<usize>().map_err(|_| invalid(v))?,
            };
        } else {
            self.flag = Some(match inline {
                None => true,
                Some(v) => parse_bool(v).ok_or_else(|| invalid(v))?,
            });
        }
        Ok(())
    }

    /// Folds in occurrences that a subcommand level already resolved.
    fn absorb(&mut self, data: &Data) {
        match data {
            Data::Bool(b) => self.flag = Some(*b),
            Data::Count(n) => self.count += n,
            Data::One(v) => self.values = vec![v.clone()],
            Data::Many(vs) => self.values.extend(vs.iter().cloned()),
        }
    }

    fn into_data(mut self, arg: &Arg) -> Data {
        if arg.count {
            Data::Count(self.count)
        } else if arg.boolean {
            Data::Bool(self.flag.unwrap_or(true))
        } else if arg.multiple {
            Data::Many(self.values)
        } else {
            Data::One(self.values.pop().unwrap_or_default())
        }
    }
}

#[derive(Debug, Clone)]
enum Data {
    Bool(bool),
    Count(usize),
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Clone)]
struct Entry {
    data: Data,
    source: ValueSource,
}

/// The outcome of a successful parse: resolved options, switches, counters,
/// positionals, and an optional dispatched subcommand.
#[derive(Debug, Clone)]
pub struct Matches {
    args: HashMap<String, Entry>,
    positionals: Vec<String>,
    subcommand: Option<(String, Box<Matches>)>,
}

impl Matches {
    /// The raw string value of a resolved option or named positional, if
    /// present. For a repeatable argument this is its last value.
    pub fn get(&self, name: &str) -> Option<&str> {
        match &self.args.get(name)?.data {
            Data::One(v) => Some(v.as_str()),
            Data::Many(vs) => vs.last().map(String::as_str),
            Data::Bool(_) | Data::Count(_) => None,
        }
    }

    /// Whether a boolean switch is on (absent switches read as `false`). A
    /// counter reads as `true` once it was given at least once.
    pub fn get_bool(&self, name: &str) -> bool {
        match self.args.get(name).map(|e| &e.data) {
            Some(Data::Bool(b)) => *b,
            Some(Data::Count(n)) => *n > 0,
            _ => false,
        }
    }

    /// How many times a [counter](Arg::count) was given (`0` when absent).
    pub fn get_count(&self, name: &str) -> usize {
        match self.args.get(name).map(|e| &e.data) {
            Some(Data::Count(n)) => *n,
            Some(Data::Bool(true)) => 1,
            _ => 0,
        }
    }

    /// Every value of a repeatable option or variadic positional, in order
    /// (empty when absent). A single-valued argument yields a one-element
    /// slice.
    pub fn get_all(&self, name: &str) -> &[String] {
        match self.args.get(name).map(|e| &e.data) {
            Some(Data::Many(vs)) => vs.as_slice(),
            Some(Data::One(v)) => std::slice::from_ref(v),
            _ => &[],
        }
    }

    /// A resolved option converted into `T`.
    ///
    /// Returns `Ok(None)` when the option is absent, or
    /// [`CliError::InvalidValue`] when the raw value cannot be converted.
    pub fn get_as<T>(&self, name: &str) -> Result<Option<T>, CliError>
    where
        T: FromStr,
    {
        match self.get(name) {
            None => Ok(None),
            Some(raw) => raw
                .parse::<T>()
                .map(Some)
                .map_err(|_| CliError::InvalidValue {
                    name: name.to_string(),
                    value: raw.to_string(),
                }),
        }
    }

    /// Every value of a repeatable argument converted into `T`; fails with
    /// [`CliError::InvalidValue`] on the first value that does not convert.
    pub fn get_all_as<T>(&self, name: &str) -> Result<Vec<T>, CliError>
    where
        T: FromStr,
    {
        self.get_all(name)
            .iter()
            .map(|raw| {
                raw.parse::<T>().map_err(|_| CliError::InvalidValue {
                    name: name.to_string(),
                    value: raw.clone(),
                })
            })
            .collect()
    }

    /// Whether the argument resolved to a value at all (from any source).
    pub fn contains(&self, name: &str) -> bool {
        self.args.contains_key(name)
    }

    /// Where the argument's value came from, or `None` when it has no value.
    pub fn source(&self, name: &str) -> Option<ValueSource> {
        self.args.get(name).map(|e| e.source)
    }

    /// The resolved value in its natural shape, or `None` when absent.
    pub fn value(&self, name: &str) -> Option<ArgValue<'_>> {
        Some(match &self.args.get(name)?.data {
            Data::Bool(b) => ArgValue::Bool(*b),
            Data::Count(n) => ArgValue::Count(*n),
            Data::One(v) => ArgValue::One(v.as_str()),
            Data::Many(vs) => ArgValue::Many(vs.as_slice()),
        })
    }

    /// The names of every argument that resolved to a value, sorted.
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.args.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    /// The positional arguments in order: the tokens given on the command
    /// line, followed by the environment/default values of the declared
    /// positionals that were not given.
    pub fn positionals(&self) -> &[String] {
        &self.positionals
    }

    /// The dispatched subcommand's name and its own [`Matches`], if any. The
    /// name is the subcommand's declared name even when an alias was typed.
    pub fn subcommand(&self) -> Option<(&str, &Matches)> {
        self.subcommand
            .as_ref()
            .map(|(name, m)| (name.as_str(), m.as_ref()))
    }

    /// The dispatched subcommand's name, if any.
    pub fn subcommand_name(&self) -> Option<&str> {
        self.subcommand.as_ref().map(|(name, _)| name.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn sample() -> Command {
        Command::new("tool")
            .about("A sample tool")
            .version("0.1")
            .flag(Arg::new("name").short('n').long("name").help("a name"))
            .flag(
                Arg::new("port")
                    .short('p')
                    .long("port")
                    .help("a port")
                    .default("8080"),
            )
            .flag(Arg::new("verbose").short('v').long("verbose").boolean())
    }

    #[test]
    fn parses_long_and_short() {
        let m = sample()
            .parse(args(&["--name", "alice", "-p", "9090"]))
            .unwrap();
        assert_eq!(m.get("name"), Some("alice"));
        assert_eq!(m.get("port"), Some("9090"));
    }

    #[test]
    fn equals_and_attached_forms() {
        let m = sample().parse(args(&["--name=bob", "-p1234"])).unwrap();
        assert_eq!(m.get("name"), Some("bob"));
        assert_eq!(m.get("port"), Some("1234"));
    }

    #[test]
    fn typed_conversion() {
        let m = sample().parse(args(&["-p", "443"])).unwrap();
        assert_eq!(m.get_as::<u16>("port").unwrap(), Some(443));
        assert_eq!(m.get_as::<i64>("missing").unwrap(), None);
    }

    #[test]
    fn bad_typed_conversion() {
        let m = sample().parse(args(&["-p", "not-a-number"])).unwrap();
        match m.get_as::<u16>("port") {
            Err(CliError::InvalidValue { name, value }) => {
                assert_eq!(name, "port");
                assert_eq!(value, "not-a-number");
            }
            other => panic!("expected InvalidValue, got {other:?}"),
        }
    }

    #[test]
    fn clustered_boolean_flags() {
        let cmd = Command::new("c")
            .flag(Arg::new("a").short('a').boolean())
            .flag(Arg::new("b").short('b').boolean())
            .flag(Arg::new("c").short('c').boolean());
        let m = cmd.parse(args(&["-abc"])).unwrap();
        assert!(m.get_bool("a"));
        assert!(m.get_bool("b"));
        assert!(m.get_bool("c"));
    }

    #[test]
    fn double_dash_stops_flag_parsing() {
        let cmd = Command::new("c").positional(Arg::new("input").variadic());
        let m = cmd.parse(args(&["--", "--name", "raw"])).unwrap();
        assert_eq!(m.positionals(), &["--name".to_string(), "raw".to_string()]);
    }

    #[test]
    fn default_applies_when_absent() {
        let m = sample().parse(args(&[])).unwrap();
        assert_eq!(m.get("port"), Some("8080"));
        assert!(!m.get_bool("verbose"));
    }

    #[test]
    fn env_beats_default_and_cli_beats_env() {
        let var = "FERROLY_CLI_TEST_PORT";
        let cmd = Command::new("c").flag(Arg::new("port").long("port").env(var).default("8080"));

        std::env::set_var(var, "5555");
        let m = cmd.parse(args(&[])).unwrap();
        assert_eq!(m.get("port"), Some("5555"));
        assert_eq!(m.source("port"), Some(ValueSource::Env));

        let m = cmd.parse(args(&["--port", "6666"])).unwrap();
        assert_eq!(m.get("port"), Some("6666"));
        assert_eq!(m.source("port"), Some(ValueSource::CommandLine));

        std::env::remove_var(var);
        let m = cmd.parse(args(&[])).unwrap();
        assert_eq!(m.get("port"), Some("8080"));
        assert_eq!(m.source("port"), Some(ValueSource::Default));
    }

    #[test]
    fn missing_required_errors() {
        let cmd = Command::new("c").flag(Arg::new("token").long("token").required());
        match cmd.parse(args(&[])) {
            Err(CliError::MissingRequired { name }) => assert_eq!(name, "token"),
            other => panic!("expected MissingRequired, got {other:?}"),
        }
    }

    #[test]
    fn unknown_flag_errors() {
        match sample().parse(args(&["--nope"])) {
            Err(CliError::UnknownFlag { flag, .. }) => assert_eq!(flag, "--nope"),
            other => panic!("expected UnknownFlag, got {other:?}"),
        }
    }

    #[test]
    fn missing_value_errors() {
        match sample().parse(args(&["--name"])) {
            Err(CliError::MissingValue { flag }) => assert_eq!(flag, "--name"),
            other => panic!("expected MissingValue, got {other:?}"),
        }
    }

    #[test]
    fn subcommand_dispatch() {
        let cmd = Command::new("app")
            .flag(Arg::new("verbose").short('v').boolean())
            .subcommand(Command::new("serve").flag(Arg::new("port").long("port").default("80")));
        let m = cmd.parse(args(&["-v", "serve", "--port", "8000"])).unwrap();
        assert!(m.get_bool("verbose"));
        let (name, sub) = m.subcommand().expect("subcommand present");
        assert_eq!(name, "serve");
        assert_eq!(sub.get("port"), Some("8000"));
    }

    #[test]
    fn help_generation_and_request() {
        let text = sample().help();
        assert!(text.contains("Usage: tool"));
        assert!(text.contains("A sample tool"));
        assert!(text.contains("--port"));
        assert!(text.contains("[default: 8080]"));
        assert!(text.contains("-h, --help"));

        match sample().parse(args(&["--help"])) {
            Err(CliError::HelpRequested(body)) => assert!(body.contains("Usage: tool")),
            other => panic!("expected HelpRequested, got {other:?}"),
        }
    }

    #[test]
    fn positionals_collected_in_order() {
        let cmd = Command::new("c")
            .positional(Arg::new("first"))
            .positional(Arg::new("second"));
        let m = cmd.parse(args(&["one", "two"])).unwrap();
        assert_eq!(m.positionals(), &["one".to_string(), "two".to_string()]);
    }

    #[test]
    fn edit_distance_counts_transposition_once() {
        assert_eq!(edit_distance("prot", "port"), 1);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(closest("prot", ["port", "name"].into_iter()), Some("port"));
        assert_eq!(closest("zzz", ["port", "name"].into_iter()), None);
    }

    #[test]
    fn run_maps_outcomes_to_exit_statuses() {
        let cmd = sample();
        let run = |list: &[&str], result: Result<(), String>| {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = cmd.run_with(args(list), |_| result, &mut out, &mut err);
            (
                code,
                String::from_utf8(out).unwrap(),
                String::from_utf8(err).unwrap(),
            )
        };

        let (code, out, err) = run(&["--help"], Ok(()));
        assert_eq!(code, 0);
        assert!(out.contains("Usage: tool") && err.is_empty());

        let (code, out, _) = run(&["--version"], Ok(()));
        assert_eq!((code, out.as_str()), (0, "tool 0.1\n"));

        let (code, out, err) = run(&["--nope"], Ok(()));
        assert_eq!(code, 2);
        assert!(out.is_empty() && err.contains("unknown flag: --nope"));

        let (code, _, err) = run(&[], Err("boom".to_string()));
        assert_eq!(code, 1);
        assert!(err.contains("error: boom"));

        assert_eq!(run(&[], Ok(())).0, 0);
    }
}

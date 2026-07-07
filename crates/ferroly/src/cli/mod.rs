//! Builder-based command-line argument parsing.
//!
//! Describe a program's interface with a [`Command`] and a set of [`Arg`]
//! builders, then hand the raw argument list to [`Command::parse`] to obtain a
//! typed [`Matches`] result. Options may be spelled long (`--port 80`,
//! `--port=80`), short (`-p 80`, `-p80`), or as clustered boolean switches
//! (`-abc`); a bare `--` stops flag processing so every following token is
//! treated as positional.
//!
//! A value option resolves by a fixed precedence: an explicit argument on the
//! command line wins, otherwise an environment variable named via
//! [`Arg::env`] is consulted, and finally a [`Arg::default`] is applied. An
//! option marked [`Arg::required`] with none of those present yields a
//! [`CliError`]. Requesting `--help` (or `-h`) during parsing returns
//! [`CliError::HelpRequested`] carrying the rendered [`Command::help`] text, so
//! the caller can print it and exit cleanly.
//!
//! Commands nest: attach a [`Command::subcommand`] and the first non-flag token
//! matching its name dispatches the remaining arguments to that child, whose
//! result is reachable through [`Matches::subcommand`].
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

use std::collections::HashMap;
use std::env;
use std::str::FromStr;

/// Errors produced while parsing arguments or converting values.
#[derive(Debug, ferroly::FerrolyError)]
#[non_exhaustive]
pub enum CliError {
    /// A flag was supplied that the [`Command`] does not define.
    #[error("unknown flag: {flag}")]
    UnknownFlag {
        /// The offending flag, spelled as it appeared (e.g. `--verbose`).
        flag: String,
    },

    /// A value option appeared with no value following it.
    #[error("missing value for flag: {flag}")]
    MissingValue {
        /// The flag that expected a value.
        flag: String,
    },

    /// A required option had no command-line value, environment fallback, or
    /// default.
    #[error("missing required argument: {name}")]
    MissingRequired {
        /// The declared argument name.
        name: String,
    },

    /// A raw value could not be converted into the requested type.
    #[error("invalid value '{value}' for argument '{name}'")]
    InvalidValue {
        /// The declared argument name.
        name: String,
        /// The raw value that failed to convert.
        value: String,
    },

    /// `--help`/`-h` was requested; the payload is the rendered help text.
    #[error("{0}")]
    HelpRequested(String),
}

/// A single argument declaration: an option, a boolean switch, or a positional.
///
/// Build one with [`Arg::new`] and refine it through the chained setters.
#[derive(Debug, Clone)]
pub struct Arg {
    name: String,
    short: Option<char>,
    long: Option<String>,
    help: Option<String>,
    required: bool,
    default: Option<String>,
    env: Option<String>,
    boolean: bool,
}

impl Arg {
    /// Starts a new argument identified by `name`; the name is the key used to
    /// look the value up in [`Matches`].
    pub fn new(name: impl Into<String>) -> Self {
        Arg {
            name: name.into(),
            short: None,
            long: None,
            help: None,
            required: false,
            default: None,
            env: None,
            boolean: false,
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

    /// Sets the fallback value used when nothing else supplies one.
    pub fn default(mut self, value: impl Into<String>) -> Self {
        self.default = Some(value.into());
        self
    }

    /// Names an environment variable consulted before the default.
    pub fn env(mut self, var: impl Into<String>) -> Self {
        self.env = Some(var.into());
        self
    }

    /// Turns the argument into a boolean switch: its presence alone means true
    /// and it never consumes a following value.
    pub fn boolean(mut self) -> Self {
        self.boolean = true;
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
    about: Option<String>,
    version: Option<String>,
    flags: Vec<Arg>,
    positionals: Vec<Arg>,
    subcommands: Vec<Command>,
}

impl Command {
    /// Starts a new command named `name`.
    pub fn new(name: impl Into<String>) -> Self {
        Command {
            name: name.into(),
            about: None,
            version: None,
            flags: Vec::new(),
            positionals: Vec::new(),
            subcommands: Vec::new(),
        }
    }

    /// Sets the short description shown in help.
    pub fn about(mut self, about: impl Into<String>) -> Self {
        self.about = Some(about.into());
        self
    }

    /// Sets the version string shown in help.
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// Adds a flag or value option.
    pub fn flag(mut self, arg: Arg) -> Self {
        self.flags.push(arg);
        self
    }

    /// Adds a positional argument; positionals bind in the order declared.
    pub fn positional(mut self, arg: Arg) -> Self {
        self.positionals.push(arg);
        self
    }

    /// Adds a nested subcommand.
    pub fn subcommand(mut self, cmd: Command) -> Self {
        self.subcommands.push(cmd);
        self
    }

    /// Parses `args` (the argument list, excluding the program name) into a
    /// [`Matches`].
    pub fn parse<I>(&self, args: I) -> Result<Matches, CliError>
    where
        I: IntoIterator<Item = String>,
    {
        let argv: Vec<String> = args.into_iter().collect();
        self.parse_slice(&argv)
    }

    /// Parses the current process arguments, skipping the program name.
    pub fn parse_env(&self) -> Result<Matches, CliError> {
        self.parse(env::args().skip(1))
    }

    fn find_long(&self, key: &str) -> Option<&Arg> {
        self.flags.iter().find(|a| a.long.as_deref() == Some(key))
    }

    fn find_short(&self, c: char) -> Option<&Arg> {
        self.flags.iter().find(|a| a.short == Some(c))
    }

    fn find_subcommand(&self, name: &str) -> Option<&Command> {
        self.subcommands.iter().find(|s| s.name == name)
    }

    fn parse_slice(&self, argv: &[String]) -> Result<Matches, CliError> {
        let mut values: HashMap<String, String> = HashMap::new();
        let mut bools: HashMap<String, bool> = HashMap::new();
        let mut positionals: Vec<String> = Vec::new();
        let mut subcommand: Option<(String, Box<Matches>)> = None;
        let mut no_more_flags = false;
        let mut i = 0;

        while i < argv.len() {
            let tok = &argv[i];

            if !no_more_flags && tok == "--" {
                no_more_flags = true;
                i += 1;
                continue;
            }

            if !no_more_flags && (tok == "--help" || tok == "-h") {
                return Err(CliError::HelpRequested(self.help()));
            }

            // Long form: `--long`, `--long value`, or `--long=value`.
            if !no_more_flags && tok.starts_with("--") {
                let body = &tok[2..];
                let (key, inline) = match body.split_once('=') {
                    Some((k, v)) => (k, Some(v.to_string())),
                    None => (body, None),
                };
                let arg = self.find_long(key).ok_or_else(|| CliError::UnknownFlag {
                    flag: format!("--{key}"),
                })?;
                if arg.boolean {
                    bools.insert(arg.name.clone(), true);
                } else {
                    let val = match inline {
                        Some(v) => v,
                        None => {
                            i += 1;
                            if i >= argv.len() {
                                return Err(CliError::MissingValue {
                                    flag: format!("--{key}"),
                                });
                            }
                            argv[i].clone()
                        }
                    };
                    values.insert(arg.name.clone(), val);
                }
                i += 1;
                continue;
            }

            // Short form: `-a`, clustered `-abc`, `-p value`, or `-pvalue`.
            if !no_more_flags && tok.starts_with('-') && tok.len() > 1 {
                let chars: Vec<char> = tok[1..].chars().collect();
                let mut j = 0;
                while j < chars.len() {
                    let c = chars[j];
                    let arg = self.find_short(c).ok_or_else(|| CliError::UnknownFlag {
                        flag: format!("-{c}"),
                    })?;
                    if arg.boolean {
                        bools.insert(arg.name.clone(), true);
                        j += 1;
                    } else {
                        let rest: String = chars[j + 1..].iter().collect();
                        let val = if !rest.is_empty() {
                            rest
                        } else {
                            i += 1;
                            if i >= argv.len() {
                                return Err(CliError::MissingValue {
                                    flag: format!("-{c}"),
                                });
                            }
                            argv[i].clone()
                        };
                        values.insert(arg.name.clone(), val);
                        break;
                    }
                }
                i += 1;
                continue;
            }

            // A non-flag token: dispatch to a subcommand or collect positionally.
            if !no_more_flags && subcommand.is_none() && positionals.is_empty() {
                if let Some(sub) = self.find_subcommand(tok) {
                    let child = sub.parse_slice(&argv[i + 1..])?;
                    subcommand = Some((sub.name.clone(), Box::new(child)));
                    break;
                }
            }
            positionals.push(tok.clone());
            i += 1;
        }

        // Resolve value options by precedence: CLI > environment > default.
        for arg in &self.flags {
            if arg.boolean || values.contains_key(&arg.name) {
                continue;
            }
            if let Some(var) = &arg.env {
                if let Ok(v) = env::var(var) {
                    values.insert(arg.name.clone(), v);
                    continue;
                }
            }
            if let Some(d) = &arg.default {
                values.insert(arg.name.clone(), d.clone());
                continue;
            }
            if arg.required {
                return Err(CliError::MissingRequired {
                    name: arg.name.clone(),
                });
            }
        }

        // Required positionals only apply when no subcommand took over.
        if subcommand.is_none() {
            for (idx, p) in self.positionals.iter().enumerate() {
                if p.required && positionals.get(idx).is_none() {
                    return Err(CliError::MissingRequired {
                        name: p.name.clone(),
                    });
                }
            }
        }

        Ok(Matches {
            values,
            bools,
            positionals,
            subcommand,
        })
    }

    /// Renders a usage block: the usage line, the description, every option
    /// with its short/long spelling and any default or environment annotation,
    /// and the list of subcommands.
    pub fn help(&self) -> String {
        let mut out = String::new();

        match &self.version {
            Some(v) => out.push_str(&format!("{} {v}\n", self.name)),
            None => out.push_str(&format!("{}\n", self.name)),
        }
        if let Some(about) = &self.about {
            out.push_str(about);
            out.push('\n');
        }

        out.push_str("\nUsage: ");
        out.push_str(&self.name);
        out.push_str(" [OPTIONS]");
        for p in &self.positionals {
            if p.required {
                out.push_str(&format!(" <{}>", p.name));
            } else {
                out.push_str(&format!(" [{}]", p.name));
            }
        }
        if !self.subcommands.is_empty() {
            out.push_str(" <COMMAND>");
        }
        out.push('\n');

        out.push_str("\nOptions:\n");
        for a in &self.flags {
            let mut col = String::new();
            match a.short {
                Some(c) => col.push_str(&format!("-{c}, ")),
                None => col.push_str("    "),
            }
            match &a.long {
                Some(l) => col.push_str(&format!("--{l}")),
                None => col.push_str(&a.name),
            }
            if !a.boolean {
                col.push_str(&format!(" <{}>", a.name.to_uppercase()));
            }

            let mut desc = a.help.clone().unwrap_or_default();
            if let Some(d) = &a.default {
                desc.push_str(&format!(" [default: {d}]"));
            }
            if let Some(e) = &a.env {
                desc.push_str(&format!(" [env: {e}]"));
            }
            out.push_str(&format!("  {col:<28}{desc}\n"));
        }
        out.push_str(&format!("  {:<28}{}\n", "-h, --help", "Print help"));

        if !self.subcommands.is_empty() {
            out.push_str("\nCommands:\n");
            for s in &self.subcommands {
                out.push_str(&format!(
                    "  {:<14}{}\n",
                    s.name,
                    s.about.as_deref().unwrap_or("")
                ));
            }
        }

        out
    }
}

/// The outcome of a successful parse: resolved options, boolean switches,
/// positionals, and an optional dispatched subcommand.
#[derive(Debug, Clone)]
pub struct Matches {
    values: HashMap<String, String>,
    bools: HashMap<String, bool>,
    positionals: Vec<String>,
    subcommand: Option<(String, Box<Matches>)>,
}

impl Matches {
    /// The raw string value of a resolved option, if present.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// Whether a boolean switch was set (absent switches read as `false`).
    pub fn get_bool(&self, name: &str) -> bool {
        self.bools.get(name).copied().unwrap_or(false)
    }

    /// A resolved option converted into `T`.
    ///
    /// Returns `Ok(None)` when the option is absent, or
    /// [`CliError::InvalidValue`] when the raw value cannot be converted.
    pub fn get_as<T>(&self, name: &str) -> Result<Option<T>, CliError>
    where
        T: FromStr,
    {
        match self.values.get(name) {
            None => Ok(None),
            Some(raw) => raw
                .parse::<T>()
                .map(Some)
                .map_err(|_| CliError::InvalidValue {
                    name: name.to_string(),
                    value: raw.clone(),
                }),
        }
    }

    /// The collected positional arguments, in order.
    pub fn positionals(&self) -> &[String] {
        &self.positionals
    }

    /// The dispatched subcommand's name and its own [`Matches`], if any.
    pub fn subcommand(&self) -> Option<(&str, &Matches)> {
        self.subcommand
            .as_ref()
            .map(|(name, m)| (name.as_str(), m.as_ref()))
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
        let cmd = Command::new("c").positional(Arg::new("input"));
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

        let m = cmd.parse(args(&["--port", "6666"])).unwrap();
        assert_eq!(m.get("port"), Some("6666"));

        std::env::remove_var(var);
        let m = cmd.parse(args(&[])).unwrap();
        assert_eq!(m.get("port"), Some("8080"));
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
            Err(CliError::UnknownFlag { flag }) => assert_eq!(flag, "--nope"),
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
}

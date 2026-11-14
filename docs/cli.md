# `ferroly::cli` — command-line argument parsing

A dependency-free, builder-based CLI parser: subcommands, value options, boolean
switches, counters, repeatable options, positional arguments, environment-variable
fallback, validation, argument groups, generated `--help` / `--version`, "did you
mean" suggestions, and shell completion.

Enable with the `cli` feature:

```toml
ferroly = { version = "0.4", features = ["cli"] }
```

```rust
use ferroly::cli::{Arg, Command};

let cmd = Command::new("svc")
    .about("A demo service")
    .version("1.0")
    .flag(Arg::new("verbose").short('v').long("verbose").boolean().help("chatty output"))
    .flag(Arg::new("port").short('p').long("port").env("PORT").default("8080").help("listen port"));

let m = cmd.parse(["--verbose", "--port", "9090"].map(String::from)).unwrap();
assert!(m.get_bool("verbose"));
assert_eq!(m.get_as::<u16>("port").unwrap(), Some(9090));
```

## Building a command

| Builder | Method | Purpose |
|---|---|---|
| `Command` | `new(name)` | start a command |
| | `about(text)` / `version(v)` | help metadata; a version also enables `--version` / `-V` |
| | `alias(name)` | another name the command answers to as a subcommand |
| | `flag(Arg)` | add an option, switch or counter |
| | `positional(Arg)` | add an ordered positional argument |
| | `subcommand(Command)` | nest a subcommand |
| | `subcommand_required()` | fail when no subcommand is given |
| | `group(Group)` | add a cross-argument rule (see [Groups](#groups)) |
| | `parse(args)` / `parse_env()` | parse; returns [`Matches`](#reading-results) |
| | `parse_or_exit(args)` / `parse_env_or_exit()` / `run(args, handler)` | parse and apply the [exit-code convention](#exit-codes) |
| | `help()` | render the usage/help text |
| | `completions(Shell)` | generate a [shell completion](#shell-completion) script |

`Arg` is built the same way:

```rust
use ferroly::cli::Arg;

// A value option: -p / --port, falling back to $PORT, then "8080".
let port = Arg::new("port").short('p').long("port").env("PORT").default("8080").required();

// A boolean switch (presence = true, never consumes a value).
let verbose = Arg::new("verbose").long("verbose").boolean();

// A counter: -vvv → 3.
let level = Arg::new("level").short('v').count();

// A repeatable option: --tag a --tag b, or --tag a,b with a delimiter.
let tags = Arg::new("tags").long("tag").delimiter(',');
```

| `Arg` method | Effect |
|---|---|
| `new(name)` | the key used to look the value up in `Matches` |
| `short(char)` / `long(&str)` | the `-c` / `--long` spellings |
| `alias(&str)` / `short_alias(char)` | additional spellings (repeatable) |
| `help(&str)` | description shown in help |
| `required()` | fail when the argument resolves to nothing |
| `default(&str)` | fallback value |
| `env(&str)` | environment variable consulted before the default |
| `boolean()` | a switch: presence means true |
| `count()` | a counter: each occurrence adds one |
| `multiple()` | keep every value of a repeated option |
| `delimiter(char)` | split each value on a delimiter (implies `multiple`) |
| `variadic()` | last positional only: collect all remaining positionals |
| `global()` | flag is also accepted by every nested subcommand |
| `choices([..])` | restrict the accepted values |
| `validator(fn)` / `typed::<T>()` | validate values at parse time |

## Parsing

`parse(args)` takes the arguments **without** the program name (use `parse_env()`
to read the process arguments for you; arguments that are not valid UTF-8 are
converted lossily). Supported token forms:

- `--long value`, `--long=value`
- `-s value`, `-svalue`, `-s=value`
- clustered short switches and counters: `-abc`, `-vvv`
- `--` stops flag parsing; everything after is positional

A value option always takes the next token as its value, whatever it looks like
(`--offset -5` works).

### Precedence

Every argument — value option, switch, counter and positional alike — resolves by
**command line → environment variable (`.env`) → default (`.default`)**. A
`required()` argument with none of those produces `MissingRequired`.
`Matches::source(name)` tells you which one supplied the value.

### Boolean switches

A switch is `true` when present. An explicit value is honoured:
`--verbose=false`, `--verbose=true`, and likewise `1`/`0`, `yes`/`no`, `on`/`off`
(case-insensitive); `-v=false` works for the short form. When a switch is repeated
the last occurrence decides. Anything else (`--verbose=maybe`) is `InvalidValue`.

`.env()` and `.default()` apply to switches too: the value is read as a boolean,
and an **empty** environment value means `false`.

### Counters

`Arg::count()` counts occurrences: `-vvv` and `--verbose --verbose --verbose` both
give `3`. Read it with `get_count`; `get_bool` is `true` once the count is above
zero. `.env()` / `.default()` supply a number.

### Repeated and multi-value options

By default a repeated option keeps its **last** value. With `multiple()` every
value is kept, and with `delimiter(',')` each value is additionally split:

```rust
use ferroly::cli::{Arg, Command};

let cmd = Command::new("t")
    .flag(Arg::new("tag").long("tag").multiple())
    .flag(Arg::new("ids").long("ids").delimiter(','));

let m = cmd.parse(["--tag", "a", "--tag", "b", "--ids", "1,2", "--ids=3"].map(String::from)).unwrap();
assert_eq!(m.get_all("tag"), ["a", "b"]);
assert_eq!(m.get_all_as::<u32>("ids").unwrap(), vec![1, 2, 3]);
```

Space-separated multi-values (`--files a b c`) are not supported — they are
ambiguous with positionals; repeat the flag or use a delimiter.

### Negative numbers

A token such as `-5`, `-1.5` or `-.25` is treated as a (negative) number — a
positional or a value — unless the command really declares a matching short flag
(`Arg::short('5')`).

### Positionals

Positionals bind in declaration order and are also reachable by name
(`m.get("input")`). They honour `.default()`, `.env()`, `.choices()` and
validators like any other argument.

- A positional the command has no slot for is rejected with `UnexpectedArgument`.
  To accept any number of trailing arguments, mark the **last** positional
  `variadic()` and read it with `get_all`.
- `Matches::positionals()` returns the tokens given on the command line followed
  by the env/default values of the declared positionals that were not given (for
  as long as no slot before them is empty).

```rust
use ferroly::cli::{Arg, Command};

let cp = Command::new("cp")
    .positional(Arg::new("dest").required())
    .positional(Arg::new("sources").variadic());

let m = cp.parse(["out", "a", "b"].map(String::from)).unwrap();
assert_eq!(m.get("dest"), Some("out"));
assert_eq!(m.get_all("sources"), ["a", "b"]);
```

### Subcommands and global flags

The first non-flag token matching a subcommand's name (or `alias`) dispatches the
rest of the arguments to it. A word that matches no subcommand (on a command that
takes no positionals) is `UnknownCommand`, with a suggestion when one is close.

Flags belong to the command that declares them: `app --verbose serve` sets the
parent's `verbose`. Mark a flag `global()` to make it valid on every nested
subcommand as well, so it may be written before **or** after the subcommand name.
Its value is visible in the `Matches` of the declaring command and of every
subcommand below it; occurrences on both sides combine (counters add up,
repeatable options concatenate, single values take the later one). A subcommand
that declares its own flag of the same name shadows the global one.

```rust
use ferroly::cli::{Arg, Command};

let app = Command::new("app")
    .flag(Arg::new("verbose").short('v').count().global())
    .subcommand(Command::new("serve").flag(Arg::new("port").long("port").default("80")));

let m = app.parse(["serve", "-vv", "--port", "8000"].map(String::from)).unwrap();
assert_eq!(m.get_count("verbose"), 2);
let (name, serve) = m.subcommand().unwrap();
assert_eq!(name, "serve");
assert_eq!(serve.get_count("verbose"), 2);
assert_eq!(serve.get("port"), Some("8000"));
```

Required positionals of a command are not enforced once one of its subcommands
has been dispatched.

## Validation

Values are checked at parse time, whichever source they came from:

```rust
use ferroly::cli::{Arg, CliError, Command};

let cmd = Command::new("t")
    .flag(Arg::new("format").long("format").choices(["json", "yaml"]).default("json"))
    .flag(Arg::new("port").long("port").typed::<u16>())
    .flag(Arg::new("name").long("name").validator(|v| {
        if v.is_empty() { Err("must not be empty".to_string()) } else { Ok(()) }
    }));

assert!(matches!(
    cmd.parse(["--format", "xml"].map(String::from)),
    Err(CliError::InvalidChoice { .. })
));
assert!(matches!(
    cmd.parse(["--port", "70000"].map(String::from)),
    Err(CliError::ValidationFailed { .. })
));
```

- `choices([..])` → `InvalidChoice { name, value, choices }`.
- `validator(|v| …)` → `ValidationFailed { name, value, reason }` with your reason.
- `typed::<T>()` is a validator requiring `value.parse::<T>()` to succeed, so a bad
  value is reported by `parse` rather than later by `get_as`.

## Groups

A `Group` ties several arguments of one command together, by name:

| Constructor | Rule |
|---|---|
| `Group::exclusive([..])` | at most one member may be supplied → `Conflict { first, second }` |
| `Group::together([..])` | supplying any member requires all of them → `RequiresTogether { present, missing }` |
| `.required()` | exclusive: exactly one must be supplied (`MissingOneOf { args }`); together: all are required (`MissingRequired`) |

An argument counts as *supplied* when it came from the command line or the
environment. A default alone never triggers a rule, and in a `together` group a
member that has a default counts as being there.

```rust
use ferroly::cli::{Arg, CliError, Command, Group};

let cmd = Command::new("export")
    .flag(Arg::new("json").long("json").boolean())
    .flag(Arg::new("yaml").long("yaml").boolean())
    .flag(Arg::new("user").long("user"))
    .flag(Arg::new("password").long("password"))
    .group(Group::exclusive(["json", "yaml"]))
    .group(Group::together(["user", "password"]));

let err = cmd.parse(["--json", "--yaml"].map(String::from)).unwrap_err();
assert_eq!(err.to_string(), "--json cannot be used with --yaml");
let err = cmd.parse(["--user", "u"].map(String::from)).unwrap_err();
assert_eq!(err.to_string(), "--user requires --password");
```

## Reading results

`Matches` accessors:

| Method | Returns |
|---|---|
| `get(name)` | `Option<&str>` — raw value of an option or named positional (the last one if repeatable) |
| `get_bool(name)` | `bool` — switch state (`false` when absent); `true` for a counter above zero |
| `get_count(name)` | `usize` — counter value (`0` when absent) |
| `get_all(name)` | `&[String]` — every value of a repeatable option or variadic positional |
| `get_as::<T: FromStr>(name)` | `Result<Option<T>, CliError>` — typed |
| `get_all_as::<T: FromStr>(name)` | `Result<Vec<T>, CliError>` — typed, all values |
| `contains(name)` | `bool` — whether the argument resolved to a value at all |
| `source(name)` | `Option<ValueSource>` — `CommandLine`, `Env` or `Default` |
| `value(name)` | `Option<ArgValue>` — `Bool`, `Count`, `One` or `Many` |
| `names()` | `Vec<&str>` — every argument that has a value, sorted |
| `positionals()` | `&[String]` |
| `subcommand()` / `subcommand_name()` | `Option<(&str, &Matches)>` / `Option<&str>` (the declared name, even when an alias was typed) |

```rust
use ferroly::cli::{Arg, Command};

let app = Command::new("git")
    .subcommand(
        Command::new("clone").positional(Arg::new("url").required().help("repo url")),
    );

let m = app.parse(["clone", "https://example.com/x.git"].map(String::from)).unwrap();
let (name, sub) = m.subcommand().unwrap();
assert_eq!(name, "clone");
assert_eq!(sub.get("url"), Some("https://example.com/x.git"));
```

## Help and version

`--help` / `-h` during parsing surfaces as `CliError::HelpRequested(String)`
carrying the rendered help text, and — when the command has a `version` —
`--version` / `-V` surfaces as `CliError::VersionRequested(String)` carrying
`"<name> <version>"`. Neither is a failure; see [Exit codes](#exit-codes).

**A flag you declare always wins over a built-in.** If the command defines
`Arg::short('h')` (say, for `--host`), `-h` is that flag and only `--help`
requests help; the same holds for `--help`, `-V` and `--version`. Without a
`version`, `--version` and `-V` are ordinary (unknown) flags.

```rust
use ferroly::cli::{Command, CliError};

let cmd = Command::new("tool").about("does things").version("1.2.3");
match cmd.parse(["--help"].map(String::from)) {
    Err(CliError::HelpRequested(text)) => assert!(text.contains("does things")),
    other => panic!("expected help, got {other:?}"),
}
match cmd.parse(["--version"].map(String::from)) {
    Err(e @ CliError::VersionRequested(_)) => assert_eq!(e.to_string(), "tool 1.2.3"),
    other => panic!("expected version, got {other:?}"),
}
```

The help text lists the usage line (required options and positionals included),
an `Arguments:` section for positionals, `Options:`, and `Commands:`. Annotations
show `[default: …]`, `[env: …]`, `[possible values: …]` and `[aliases: …]`; long
descriptions are wrapped at 80 columns. Help requested on a subcommand
(`app serve --help`) shows the full path in the usage line (`Usage: app serve …`)
and the inherited flags under `Global options:`.

```text
app serve
Run the server

Usage: app serve [OPTIONS] [dir] [COMMAND]

Arguments:
  [dir]

Options:
  -p, --port <PORT>  [default: 80]
  -h, --help         Print help

Global options:
  -v, --verbose...       more output
      --config <CONFIG>  [default: app.toml]

Commands:
  status  Show status
```

`Command::help()` returns the same text for the command on its own (no parent
path).

## Exit codes

`parse` returns help and version requests through the `Err` side so that nothing
is printed behind your back. The convention for turning the outcome into a
process status is built in:

| Outcome | `CliError::exit_code()` | Stream |
|---|---|---|
| `HelpRequested`, `VersionRequested` | `0` | standard output |
| any parse error | `2` | standard error |

`CliError::is_informational()` tells the two groups apart, and three helpers apply
the convention for you:

- `err.exit()` — print the outcome to the right stream and exit with its code.
- `cmd.parse_or_exit(args)` / `cmd.parse_env_or_exit()` — return `Matches`, or
  print and exit as above.
- `cmd.run(args, handler)` — a complete `main`: parse, call `handler(&matches)`,
  and return a `std::process::ExitCode` (`0` on success and for help/version, `2`
  for a parse error, `1` when the handler returns `Err`, whose message is printed
  to standard error).

```rust
use ferroly::cli::{Arg, Command};
use std::process::ExitCode;

fn main() -> ExitCode {
    Command::new("tool")
        .version("1.0")
        .flag(Arg::new("name").long("name").default("world"))
        .run(std::env::args().skip(1), |m| -> Result<(), String> {
            println!("hello {}", m.get("name").unwrap_or_default());
            Ok(())
        })
}
```

Doing it by hand is one `match`:

```rust
use ferroly::cli::Command;

let cmd = Command::new("tool");
let matches = match cmd.parse_env() {
    Ok(m) => m,
    Err(e) => e.exit(), // 0 for --help/--version, 2 for a parse error
};
```

## Shell completion

`Command::completions(Shell)` generates a completion script for the whole command
tree — flags (with aliases), subcommands, global flags inside subcommands, and
`choices` as value candidates:

```rust
use ferroly::cli::{Arg, Command, Shell};

let cmd = Command::new("app")
    .flag(Arg::new("format").long("format").choices(["json", "yaml"]))
    .subcommand(Command::new("serve").about("Run the server"));

let bash = cmd.completions(Shell::Bash);
let zsh = cmd.completions(Shell::Zsh);
assert!(bash.contains("complete -F"));
assert!(zsh.starts_with("#compdef app"));
```

Typical wiring is a hidden subcommand or flag that prints the script:

```text
# bash: add to ~/.bashrc
source <(app completions bash)

# zsh: save as _app somewhere on $fpath, or
source <(app completions zsh)
```

Option values without `choices` fall back to file-name completion. Other shells
(fish, PowerShell) are not generated.

## Errors

`CliError` implements `std::error::Error`, `Clone` and `PartialEq`, and converts
into `ferroly::Error` (`Error::Cli`) with `?`.

| Variant | When |
|---|---|
| `UnknownFlag { flag, suggestion }` | an unrecognized `--flag` / `-f`; `suggestion` is the closest declared long flag, shown as "did you mean '--port'?" |
| `UnknownCommand { name, suggestion }` | a word where a subcommand was expected |
| `UnexpectedArgument { arg }` | a positional the command has no slot for |
| `MissingValue { flag }` | a value option given with no value |
| `MissingRequired { name }` | a `required()` argument absent (no env/default) |
| `MissingSubcommand { command }` | `subcommand_required()` and none given |
| `InvalidValue { name, value }` | `get_as::<T>` conversion failed, or a switch/counter got a bad value |
| `InvalidChoice { name, value, choices }` | value not among `choices` |
| `ValidationFailed { name, value, reason }` | rejected by `validator` / `typed` |
| `Conflict { first, second }` | two members of an exclusive group |
| `RequiresTogether { present, missing }` | an incomplete together group |
| `MissingOneOf { args }` | a required exclusive group with no member |
| `HelpRequested(String)` | `--help` / `-h` was passed |
| `VersionRequested(String)` | `--version` / `-V` was passed |

With the `errutils` feature, `CliError` implements
[`Classify`](errutils.md#classification-classify--errorkind): parse errors are
`InvalidInput`.

## Feeding a configuration

With both `cli` and `config` enabled, parsed matches plug into the layered
[config](config.md) loader — see
[`merge_cli`](config.md#merge_cli--parsed-command-lines):

```rust
use ferroly::cli::{Arg, Command};
use ferroly::config::Config;

let cmd = Command::new("svc").flag(Arg::new("port").long("port").default("8080"));
let m = cmd.parse_env_or_exit();

let cfg = Config::builder()
    .merge_cli_defaults(&m)          // the parser's defaults: lowest priority
    .merge_file_optional("svc.toml")?
    .merge_env("SVC_")
    .merge_cli(&m)                   // what the operator typed: highest priority
    .build();
```

## Limitations

- **Builder-based, not a `#[derive]`** — describe the command with `Arg` /
  `Command`; there is no attribute-macro form.
- **No space-separated multi-values** — use repetition or a delimiter.
- **Completion for bash and zsh only**; value completion is limited to `choices`
  and file names.
- **Help width is fixed at 80 columns**; the terminal width is not probed.
- **One subcommand level dispatched per parse step** — nest `Command`s for deeper
  trees; each `Matches::subcommand()` descends one level.

## See also

- [config](config.md) — layered configuration; CLI args compose with env + file
  layers.
- [errutils](errutils.md) — error context and classification; the root
  `ferroly::Error`.

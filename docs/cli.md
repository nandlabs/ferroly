# `ferroly::cli` — command-line argument parsing

A dependency-free, builder-based CLI parser: subcommands, typed flags and
options, positional arguments, environment-variable fallback, and generated
`--help`.

Enable with the `cli` feature:

```toml
ferroly = { version = "0.2", features = ["cli"] }
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
| | `about(text)` / `version(v)` | help metadata |
| | `flag(Arg)` | add an option or boolean flag |
| | `positional(Arg)` | add an ordered positional argument |
| | `subcommand(Command)` | nest a subcommand |
| | `parse(args)` / `parse_env()` | parse; returns [`Matches`](#reading-results) |
| | `help()` | render the usage/help text |

`Arg` is built the same way:

```rust
use ferroly::cli::Arg;

// A value option: -p / --port, falling back to $PORT, then "8080".
let port = Arg::new("port").short('p').long("port").env("PORT").default("8080").required();

// A boolean flag (presence = true, never consumes a value).
let verbose = Arg::new("verbose").long("verbose").boolean();
```

`Arg` methods: `new`, `short(char)`, `long(&str)`, `help(&str)`, `required()`,
`default(&str)`, `env(&str)`, `boolean()`.

## Parsing

`parse(args)` takes the arguments **without** the program name (use `parse_env()`
to read `std::env::args().skip(1)` for you). Supported token forms:

- `--long value`, `--long=value`
- `-s value`, `-svalue`
- clustered short booleans: `-abc`
- `--` stops flag parsing; everything after is positional

Value precedence for an option is **CLI argument → environment variable
(`.env`) → default (`.default`)**. A `required()` option with none of those
produces an error.

## Reading results

`Matches` accessors:

| Method | Returns |
|---|---|
| `get(name)` | `Option<&str>` — raw value |
| `get_bool(name)` | `bool` — flag presence |
| `get_as::<T: FromStr>(name)` | `Result<Option<T>, CliError>` — typed |
| `positionals()` | `&[String]` |
| `subcommand()` | `Option<(&str, &Matches)>` |

```rust
use ferroly::cli::{Arg, Command};

let app = Command::new("git")
    .subcommand(
        Command::new("clone").positional(Arg::new("url").required().help("repo url")),
    );

let m = app.parse(["clone", "https://example.com/x.git"].map(String::from)).unwrap();
let (name, sub) = m.subcommand().unwrap();
assert_eq!(name, "clone");
assert_eq!(sub.positionals(), &["https://example.com/x.git".to_string()]);
```

## Help

`--help` / `-h` during parsing surfaces as `CliError::HelpRequested(String)`,
carrying the rendered help text — so a caller can print it and exit `0` rather
than treating it as an error:

```rust
use ferroly::cli::{Command, CliError};

let cmd = Command::new("tool").about("does things");
match cmd.parse(["--help"].map(String::from)) {
    Err(CliError::HelpRequested(text)) => assert!(text.contains("does things")),
    other => panic!("expected help, got {other:?}"),
}
```

`Command::help()` returns the same text directly.

## Errors

`CliError` derives [`FerrolyError`](derive.md) (so it implements
`std::error::Error` and works with `?`):

| Variant | When |
|---|---|
| `UnknownFlag { flag }` | an unrecognized `--flag` / `-f` |
| `MissingValue { flag }` | a value option given with no value |
| `MissingRequired { name }` | a `required()` arg absent (no env/default) |
| `InvalidValue { name, value }` | `get_as::<T>` conversion failed |
| `HelpRequested(String)` | `--help` / `-h` was passed |

## Limitations

- **Builder-based, not a `#[derive]`** — describe the command with `Arg` /
  `Command`; there is no attribute-macro form.
- **No value validation beyond `FromStr`** — typing happens at read time via
  `get_as`.
- **One subcommand level dispatched per parse** — nest `Command`s for deeper
  trees; each `Matches::subcommand()` descends one level.

## See also

- [config](config.md) — layered configuration; CLI args compose with env + file
  layers.
- [errutils](errutils.md) — the `FerrolyError` derive behind `CliError`.

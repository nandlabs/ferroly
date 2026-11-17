#![cfg(feature = "cli")]
//! Regression and behaviour tests for the `cli` parser: boolean values,
//! built-in help/version precedence, negative numbers, positionals, global
//! flags, repeated/counted flags, validation, groups, help text and shell
//! completion.

use ferroly::cli::{Arg, ArgValue, CliError, Command, DynamicArity, Group, Shell, ValueSource};

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

// ---- C14: boolean flags ----------------------------------------------------

#[test]
fn boolean_flag_honours_explicit_true_and_false() {
    let cmd = Command::new("t").flag(Arg::new("verbose").short('v').long("verbose").boolean());

    assert!(!cmd
        .parse(args(&["--verbose=false"]))
        .unwrap()
        .get_bool("verbose"));
    assert!(cmd
        .parse(args(&["--verbose=true"]))
        .unwrap()
        .get_bool("verbose"));
    assert!(!cmd
        .parse(args(&["--verbose=0"]))
        .unwrap()
        .get_bool("verbose"));
    assert!(cmd
        .parse(args(&["--verbose=YES"]))
        .unwrap()
        .get_bool("verbose"));
    assert!(!cmd.parse(args(&["-v=false"])).unwrap().get_bool("verbose"));
    assert!(cmd.parse(args(&["--verbose"])).unwrap().get_bool("verbose"));

    // The last occurrence decides.
    let m = cmd.parse(args(&["--verbose", "--verbose=false"])).unwrap();
    assert!(!m.get_bool("verbose"));

    match cmd.parse(args(&["--verbose=maybe"])) {
        Err(CliError::InvalidValue { name, value }) => {
            assert_eq!((name.as_str(), value.as_str()), ("verbose", "maybe"));
        }
        other => panic!("expected InvalidValue, got {other:?}"),
    }
}

#[test]
fn boolean_flag_honours_env_and_default() {
    let var = "FERROLY_CLI_IT_BOOL_ENV";
    let cmd = Command::new("t")
        .flag(Arg::new("color").long("color").boolean().default("true"))
        .flag(Arg::new("debug").long("debug").boolean().env(var));

    std::env::remove_var(var);
    let m = cmd.parse(args(&[])).unwrap();
    assert!(m.get_bool("color"), "default(\"true\") must apply");
    assert_eq!(m.source("color"), Some(ValueSource::Default));
    assert!(!m.get_bool("debug"));
    assert!(!m.contains("debug"));

    std::env::set_var(var, "true");
    let m = cmd.parse(args(&[])).unwrap();
    assert!(m.get_bool("debug"), "env must switch a boolean on");
    assert_eq!(m.source("debug"), Some(ValueSource::Env));

    // The command line still beats the environment.
    let m = cmd
        .parse(args(&["--debug=false", "--color=false"]))
        .unwrap();
    assert!(!m.get_bool("debug"));
    assert!(!m.get_bool("color"));

    std::env::set_var(var, "");
    assert!(!cmd.parse(args(&[])).unwrap().get_bool("debug"));

    std::env::set_var(var, "not-a-bool");
    assert!(matches!(
        cmd.parse(args(&[])),
        Err(CliError::InvalidValue { .. })
    ));
    std::env::remove_var(var);
}

// ---- C14: built-in help / version ------------------------------------------

#[test]
fn user_defined_short_h_beats_builtin_help() {
    let cmd = Command::new("t").flag(Arg::new("host").short('h').long("host"));

    let m = cmd.parse(args(&["-h", "example.org"])).unwrap();
    assert_eq!(m.get("host"), Some("example.org"));

    // `--help` is still the built-in, and the help text no longer claims `-h`.
    match cmd.parse(args(&["--help"])) {
        Err(CliError::HelpRequested(text)) => {
            assert!(text.contains("--help"));
            assert!(!text.contains("-h, --help"));
            assert!(text.contains("-h, --host"));
        }
        other => panic!("expected HelpRequested, got {other:?}"),
    }
}

#[test]
fn builtin_short_h_works_when_unclaimed() {
    let cmd = Command::new("t").flag(Arg::new("verbose").short('v').boolean());
    assert!(matches!(
        cmd.parse(args(&["-h"])),
        Err(CliError::HelpRequested(_))
    ));
    // …also inside a cluster.
    assert!(matches!(
        cmd.parse(args(&["-vh"])),
        Err(CliError::HelpRequested(_))
    ));
}

#[test]
fn version_flag_is_handled_when_a_version_is_set() {
    let cmd = Command::new("tool").version("1.2.3");
    for flag in ["--version", "-V"] {
        match cmd.parse(args(&[flag])) {
            Err(e @ CliError::VersionRequested(_)) => {
                assert_eq!(e.to_string(), "tool 1.2.3");
                assert_eq!(e.exit_code(), 0);
            }
            other => panic!("expected VersionRequested, got {other:?}"),
        }
    }
    assert!(cmd.help().contains("-V, --version"));

    // Without a version there is no such flag.
    let bare = Command::new("tool");
    assert!(matches!(
        bare.parse(args(&["--version"])),
        Err(CliError::UnknownFlag { .. })
    ));
    assert!(!bare.help().contains("--version"));

    // A program may use -V / --version itself.
    let own = Command::new("tool")
        .version("1.0")
        .flag(Arg::new("verify").short('V').long("version").boolean());
    assert!(own.parse(args(&["-V"])).unwrap().get_bool("verify"));
    assert!(own.parse(args(&["--version"])).unwrap().get_bool("verify"));
}

#[test]
fn exit_codes_follow_the_convention() {
    assert_eq!(CliError::HelpRequested("h".into()).exit_code(), 0);
    assert_eq!(CliError::VersionRequested("v".into()).exit_code(), 0);
    assert!(CliError::HelpRequested("h".into()).is_informational());
    let err = CliError::MissingRequired { name: "x".into() };
    assert_eq!(err.exit_code(), 2);
    assert!(!err.is_informational());
}

// ---- C14: numbers and attached values --------------------------------------

#[test]
fn negative_number_is_a_positional_when_no_such_flag_exists() {
    let cmd = Command::new("t")
        .flag(Arg::new("verbose").short('v').boolean())
        .positional(Arg::new("delta").variadic());
    let m = cmd.parse(args(&["-5", "-1.5", "-.25", "-v"])).unwrap();
    assert_eq!(m.positionals(), &["-5", "-1.5", "-.25"]);
    assert_eq!(
        m.get_all_as::<f64>("delta").unwrap(),
        vec![-5.0, -1.5, -0.25]
    );
    assert!(m.get_bool("verbose"));

    // A non-numeric unknown short flag is still an error.
    assert!(matches!(
        cmd.parse(args(&["-x"])),
        Err(CliError::UnknownFlag { .. })
    ));

    // A declared digit flag keeps working as a flag.
    let digit = Command::new("t").flag(Arg::new("five").short('5').boolean());
    assert!(digit.parse(args(&["-5"])).unwrap().get_bool("five"));

    // And a negative number is accepted as an option value.
    let opt = Command::new("t").flag(Arg::new("offset").long("offset").short('o'));
    let m = opt.parse(args(&["--offset", "-5"])).unwrap();
    assert_eq!(m.get_as::<i32>("offset").unwrap(), Some(-5));
}

#[test]
fn short_flag_with_equals_drops_the_equals_sign() {
    let cmd = Command::new("t").flag(Arg::new("num").short('n'));
    assert_eq!(cmd.parse(args(&["-n=5"])).unwrap().get("num"), Some("5"));
    assert_eq!(cmd.parse(args(&["-n5"])).unwrap().get("num"), Some("5"));
    assert_eq!(cmd.parse(args(&["-n", "5"])).unwrap().get("num"), Some("5"));
}

// ---- C14: positionals ------------------------------------------------------

#[test]
fn positional_default_and_env_are_honoured() {
    let var = "FERROLY_CLI_IT_POS_ENV";
    std::env::remove_var(var);
    let cmd = Command::new("t")
        .positional(Arg::new("input").required())
        .positional(Arg::new("output").default("out.txt"))
        .positional(Arg::new("mode").env(var));

    let m = cmd.parse(args(&["in.txt"])).unwrap();
    assert_eq!(m.get("input"), Some("in.txt"));
    assert_eq!(m.get("output"), Some("out.txt"));
    assert_eq!(m.source("output"), Some(ValueSource::Default));
    assert_eq!(m.get("mode"), None);
    assert_eq!(m.positionals(), &["in.txt", "out.txt"]);

    std::env::set_var(var, "fast");
    let m = cmd.parse(args(&["in.txt"])).unwrap();
    assert_eq!(m.get("mode"), Some("fast"));
    assert_eq!(m.positionals(), &["in.txt", "out.txt", "fast"]);
    std::env::remove_var(var);

    let m = cmd.parse(args(&["in.txt", "explicit.txt"])).unwrap();
    assert_eq!(m.get("output"), Some("explicit.txt"));
    assert_eq!(m.source("output"), Some(ValueSource::CommandLine));

    // A required positional with a default is satisfied by the default.
    let d = Command::new("t").positional(Arg::new("dir").required().default("."));
    assert_eq!(d.parse(args(&[])).unwrap().get("dir"), Some("."));

    assert!(matches!(
        cmd.parse(args(&[])),
        Err(CliError::MissingRequired { name }) if name == "input"
    ));
}

#[test]
fn extra_positionals_are_rejected_unless_variadic() {
    let fixed = Command::new("t").positional(Arg::new("input"));
    match fixed.parse(args(&["a", "b"])) {
        Err(CliError::UnexpectedArgument { arg }) => assert_eq!(arg, "b"),
        other => panic!("expected UnexpectedArgument, got {other:?}"),
    }

    let none = Command::new("t");
    assert!(matches!(
        none.parse(args(&["stray"])),
        Err(CliError::UnexpectedArgument { .. })
    ));
    assert!(matches!(
        none.parse(args(&["--", "stray"])),
        Err(CliError::UnexpectedArgument { .. })
    ));

    let variadic = Command::new("cp")
        .positional(Arg::new("dest").required())
        .positional(Arg::new("sources").variadic());
    let m = variadic.parse(args(&["out", "a", "b", "c"])).unwrap();
    assert_eq!(m.get("dest"), Some("out"));
    assert_eq!(m.get_all("sources"), &["a", "b", "c"]);
    assert_eq!(m.positionals().len(), 4);
    assert!(variadic
        .parse(args(&["out"]))
        .unwrap()
        .get_all("sources")
        .is_empty());
}

// ---- C14: global flags -----------------------------------------------------

fn app() -> Command {
    Command::new("app")
        .version("2.0")
        .flag(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .count()
                .global(),
        )
        .flag(
            Arg::new("config")
                .long("config")
                .global()
                .default("app.toml"),
        )
        .flag(Arg::new("local").long("local").boolean())
        .subcommand(
            Command::new("serve")
                .alias("run")
                .about("Run the server")
                .flag(Arg::new("port").long("port").short('p').default("80"))
                .subcommand(Command::new("status").about("Show status")),
        )
}

#[test]
fn global_flags_are_accepted_after_a_subcommand() {
    let m = app()
        .parse(args(&["serve", "--config", "prod.toml", "-vv"]))
        .unwrap();
    // Visible on the declaring command…
    assert_eq!(m.get("config"), Some("prod.toml"));
    assert_eq!(m.get_count("verbose"), 2);
    // …and on the subcommand.
    let (name, sub) = m.subcommand().unwrap();
    assert_eq!(name, "serve");
    assert_eq!(sub.get("config"), Some("prod.toml"));
    assert_eq!(sub.get_count("verbose"), 2);
    assert_eq!(sub.source("config"), Some(ValueSource::CommandLine));

    // Before the subcommand works as it always did, and reaches the child too.
    let m = app().parse(args(&["--config", "x.toml", "serve"])).unwrap();
    assert_eq!(m.get("config"), Some("x.toml"));
    assert_eq!(m.subcommand().unwrap().1.get("config"), Some("x.toml"));

    // Occurrences on both sides combine for counters; the later value wins
    // for single-valued options.
    let m = app()
        .parse(args(&[
            "-v", "--config", "a", "serve", "-v", "--config", "b", "status", "-v",
        ]))
        .unwrap();
    assert_eq!(m.get_count("verbose"), 3);
    assert_eq!(m.get("config"), Some("b"));
    let serve = m.subcommand().unwrap().1;
    let status = serve.subcommand().unwrap().1;
    assert_eq!(status.get_count("verbose"), 3);
    assert_eq!(status.get("config"), Some("b"));

    // Defaults are visible everywhere when nothing was given.
    let m = app().parse(args(&["serve"])).unwrap();
    assert_eq!(m.subcommand().unwrap().1.get("config"), Some("app.toml"));

    // A non-global parent flag is not accepted after the subcommand.
    assert!(matches!(
        app().parse(args(&["serve", "--local"])),
        Err(CliError::UnknownFlag { .. })
    ));
}

#[test]
fn required_global_flag_may_follow_the_subcommand() {
    let cmd = Command::new("app")
        .flag(Arg::new("token").long("token").required().global())
        .subcommand(Command::new("push"));
    let m = cmd.parse(args(&["push", "--token", "t"])).unwrap();
    assert_eq!(m.get("token"), Some("t"));
    assert!(matches!(
        cmd.parse(args(&["push"])),
        Err(CliError::MissingRequired { name }) if name == "token"
    ));
}

#[test]
fn subcommand_aliases_and_unknown_commands() {
    let m = app().parse(args(&["run", "-p", "9"])).unwrap();
    let (name, sub) = m.subcommand().unwrap();
    assert_eq!(name, "serve");
    assert_eq!(m.subcommand_name(), Some("serve"));
    assert_eq!(sub.get("port"), Some("9"));

    match app().parse(args(&["serv"])) {
        Err(CliError::UnknownCommand { name, suggestion }) => {
            assert_eq!(name, "serv");
            assert_eq!(suggestion.as_deref(), Some("serve"));
        }
        other => panic!("expected UnknownCommand, got {other:?}"),
    }

    let strict = Command::new("git")
        .subcommand(Command::new("clone"))
        .subcommand_required();
    match strict.parse(args(&[])) {
        Err(CliError::MissingSubcommand { command }) => assert_eq!(command, "git"),
        other => panic!("expected MissingSubcommand, got {other:?}"),
    }
}

// ---- repeated, counted, validated ------------------------------------------

#[test]
fn repeated_and_delimited_values_accumulate() {
    let cmd = Command::new("t")
        .flag(Arg::new("tag").long("tag").short('t').multiple())
        .flag(Arg::new("ids").long("ids").delimiter(','))
        .flag(Arg::new("single").long("single"));

    let m = cmd
        .parse(args(&[
            "--tag", "a", "-t", "b", "--tag=c", "--ids", "1,2", "--ids=3", "--single", "x",
            "--single", "y",
        ]))
        .unwrap();
    assert_eq!(m.get_all("tag"), &["a", "b", "c"]);
    assert_eq!(m.get("tag"), Some("c"));
    assert_eq!(m.get_all_as::<u32>("ids").unwrap(), vec![1, 2, 3]);
    assert_eq!(m.value("ids"), Some(ArgValue::Many(m.get_all("ids"))));
    // Without `multiple` the last occurrence wins.
    assert_eq!(m.get("single"), Some("y"));
    assert_eq!(m.get_all("single"), &["y"]);
    assert!(m.get_all("absent").is_empty());

    assert!(matches!(
        cmd.parse(args(&["--ids", "1,x"]))
            .unwrap()
            .get_all_as::<u32>("ids"),
        Err(CliError::InvalidValue { .. })
    ));
}

#[test]
fn counted_flags_count_every_occurrence() {
    let cmd = Command::new("t")
        .flag(Arg::new("verbose").short('v').long("verbose").count())
        .flag(Arg::new("quiet").short('q').boolean());

    let m = cmd.parse(args(&["-vvv"])).unwrap();
    assert_eq!(m.get_count("verbose"), 3);
    assert!(m.get_bool("verbose"));
    assert_eq!(m.value("verbose"), Some(ArgValue::Count(3)));

    let m = cmd.parse(args(&["-vqv", "--verbose"])).unwrap();
    assert_eq!(m.get_count("verbose"), 3);
    assert!(m.get_bool("quiet"));

    let m = cmd.parse(args(&[])).unwrap();
    assert_eq!(m.get_count("verbose"), 0);
    assert!(!m.get_bool("verbose"));
}

#[test]
fn choices_are_validated_at_parse_time() {
    let cmd = Command::new("t").flag(
        Arg::new("format")
            .long("format")
            .choices(["json", "yaml"])
            .default("json"),
    );
    assert_eq!(
        cmd.parse(args(&["--format", "yaml"]))
            .unwrap()
            .get("format"),
        Some("yaml")
    );
    match cmd.parse(args(&["--format", "xml"])) {
        Err(e @ CliError::InvalidChoice { .. }) => {
            let text = e.to_string();
            assert!(
                text.contains("'xml'") && text.contains("json, yaml"),
                "{text}"
            );
        }
        other => panic!("expected InvalidChoice, got {other:?}"),
    }
}

#[test]
fn typed_and_custom_validators_run_at_parse_time() {
    let cmd = Command::new("t")
        .flag(Arg::new("port").long("port").typed::<u16>())
        .flag(Arg::new("name").long("name").validator(|v| {
            if v.chars().all(|c| c.is_ascii_lowercase()) {
                Ok(())
            } else {
                Err("must be lowercase".to_string())
            }
        }));

    assert!(cmd.parse(args(&["--port", "8080", "--name", "ok"])).is_ok());
    match cmd.parse(args(&["--port", "70000"])) {
        Err(CliError::ValidationFailed { name, value, .. }) => {
            assert_eq!((name.as_str(), value.as_str()), ("port", "70000"));
        }
        other => panic!("expected ValidationFailed, got {other:?}"),
    }
    match cmd.parse(args(&["--name", "Nope"])) {
        Err(CliError::ValidationFailed { reason, .. }) => assert_eq!(reason, "must be lowercase"),
        other => panic!("expected ValidationFailed, got {other:?}"),
    }
}

#[test]
fn flag_aliases_resolve_to_the_same_argument() {
    let cmd = Command::new("t").flag(
        Arg::new("color")
            .long("color")
            .alias("colour")
            .short('c')
            .short_alias('k'),
    );
    for spelling in [
        ["--color", "red"],
        ["--colour", "red"],
        ["-c", "red"],
        ["-k", "red"],
    ] {
        assert_eq!(
            cmd.parse(args(&spelling)).unwrap().get("color"),
            Some("red")
        );
    }
    assert!(cmd.help().contains("[aliases: --colour, -k]"));
}

#[test]
fn unknown_flags_suggest_the_closest_match() {
    let cmd = Command::new("t")
        .flag(Arg::new("port").long("port"))
        .flag(Arg::new("verbose").long("verbose").boolean());

    match cmd.parse(args(&["--prot", "1"])) {
        Err(e @ CliError::UnknownFlag { .. }) => {
            assert_eq!(
                e.to_string(),
                "unknown flag: --prot (did you mean '--port'?)"
            );
        }
        other => panic!("expected UnknownFlag, got {other:?}"),
    }
    match cmd.parse(args(&["--verb"])) {
        Err(CliError::UnknownFlag { suggestion, .. }) => {
            assert_eq!(suggestion.as_deref(), Some("--verbose"));
        }
        other => panic!("expected UnknownFlag, got {other:?}"),
    }
    match cmd.parse(args(&["--zzzzzz"])) {
        Err(CliError::UnknownFlag { suggestion, .. }) => assert_eq!(suggestion, None),
        other => panic!("expected UnknownFlag, got {other:?}"),
    }
    match cmd.parse(args(&["--hlep"])) {
        Err(CliError::UnknownFlag { suggestion, .. }) => {
            assert_eq!(suggestion.as_deref(), Some("--help"));
        }
        other => panic!("expected UnknownFlag, got {other:?}"),
    }
}

// ---- groups ----------------------------------------------------------------

#[test]
fn exclusive_group_rejects_two_members() {
    let cmd = Command::new("t")
        .flag(Arg::new("json").long("json").boolean())
        .flag(Arg::new("yaml").long("yaml").boolean())
        .flag(Arg::new("format").long("format").default("text"))
        .group(Group::exclusive(["json", "yaml", "format"]));

    assert!(cmd.parse(args(&["--json"])).is_ok());
    // A default alone never conflicts.
    assert!(cmd.parse(args(&["--yaml"])).is_ok());
    assert!(cmd.parse(args(&[])).is_ok());
    match cmd.parse(args(&["--json", "--yaml"])) {
        Err(e @ CliError::Conflict { .. }) => {
            assert_eq!(e.to_string(), "--json cannot be used with --yaml");
        }
        other => panic!("expected Conflict, got {other:?}"),
    }

    let required = Command::new("t")
        .flag(Arg::new("json").long("json").boolean())
        .flag(Arg::new("yaml").long("yaml").boolean())
        .group(Group::exclusive(["json", "yaml"]).required());
    match required.parse(args(&[])) {
        Err(CliError::MissingOneOf { args }) => assert_eq!(args, ["--json", "--yaml"]),
        other => panic!("expected MissingOneOf, got {other:?}"),
    }
    assert!(required.parse(args(&["--yaml"])).is_ok());
}

#[test]
fn together_group_requires_all_members() {
    let cmd = Command::new("t")
        .flag(Arg::new("user").long("user"))
        .flag(Arg::new("password").long("password"))
        .group(Group::together(["user", "password"]));

    assert!(cmd.parse(args(&[])).is_ok());
    assert!(cmd.parse(args(&["--user", "u", "--password", "p"])).is_ok());
    match cmd.parse(args(&["--user", "u"])) {
        Err(e @ CliError::RequiresTogether { .. }) => {
            assert_eq!(e.to_string(), "--user requires --password");
        }
        other => panic!("expected RequiresTogether, got {other:?}"),
    }

    let required = cmd
        .clone()
        .group(Group::together(["user", "password"]).required());
    assert!(matches!(
        required.parse(args(&[])),
        Err(CliError::MissingRequired { .. })
    ));
}

// ---- help ------------------------------------------------------------------

#[test]
fn help_lists_positionals_and_wraps_long_descriptions() {
    let long = "This description is deliberately long so that it cannot possibly fit on a \
                single eighty column line and therefore has to be wrapped by the renderer";
    let cmd = Command::new("tool")
        .about("Does things")
        .flag(Arg::new("output").short('o').long("output").help(long))
        .flag(Arg::new("format").long("format").choices(["json", "yaml"]))
        .flag(Arg::new("token").long("token").required())
        .positional(Arg::new("input").required().help("file to read"))
        .positional(Arg::new("extra").variadic().help("more files"));
    let text = cmd.help();

    assert!(
        text.contains("Usage: tool [OPTIONS] --token <TOKEN> <input> [extra]..."),
        "{text}"
    );
    assert!(text.contains("\nArguments:\n"), "{text}");
    assert!(text.contains("<input>") && text.contains("file to read"));
    assert!(text.contains("[extra]...") && text.contains("more files"));
    assert!(text.contains("[possible values: json, yaml]"));
    for line in text.lines() {
        assert!(line.chars().count() <= 80, "line too long: {line:?}");
    }
    // The long description continues on an indented line.
    let wrapped = text
        .lines()
        .skip_while(|l| !l.contains("--output"))
        .nth(1)
        .unwrap();
    assert!(wrapped.starts_with("      ") && !wrapped.trim().is_empty());
    // Nothing was lost in wrapping.
    let squashed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(squashed.contains(long));
}

#[test]
fn subcommand_help_shows_the_parent_path_and_global_options() {
    match app().parse(args(&["serve", "--help"])) {
        Err(CliError::HelpRequested(text)) => {
            assert!(
                text.contains("Usage: app serve [OPTIONS] [COMMAND]"),
                "{text}"
            );
            assert!(text.contains("Global options:"), "{text}");
            assert!(text.contains("--config"), "{text}");
            assert!(!text.contains("--local"), "{text}");
            assert!(text.contains("status"), "{text}");
        }
        other => panic!("expected HelpRequested, got {other:?}"),
    }
    match app().parse(args(&["serve", "status", "-h"])) {
        Err(CliError::HelpRequested(text)) => {
            assert!(text.contains("Usage: app serve status [OPTIONS]"), "{text}");
        }
        other => panic!("expected HelpRequested, got {other:?}"),
    }
    let top = app().help();
    assert!(top.contains("Usage: app [OPTIONS] [COMMAND]"));
    assert!(top.contains("[aliases: run]"));
}

// ---- completion ------------------------------------------------------------

#[test]
fn bash_completion_covers_flags_subcommands_and_choices() {
    let cmd = app().flag(Arg::new("format").long("format").choices(["json", "yaml"]));
    let script = cmd.completions(Shell::Bash);
    assert!(script.contains("_app_completions()"), "{script}");
    assert!(script.contains("complete -F _app_completions"), "{script}");
    assert!(script.contains("app,serve) cmd=\"app__serve\""), "{script}");
    assert!(script.contains("app,run) cmd=\"app__serve\""), "{script}");
    assert!(script.contains("app__serve,status) cmd=\"app__serve__status\""));
    assert!(script.contains("opts=('json' 'yaml')"), "{script}");
    // Global flags are offered inside subcommands; local ones are not.
    let serve = script.split("        app__serve)\n").nth(1).unwrap();
    let serve = serve
        .split(";;\n        app__serve__status)")
        .next()
        .unwrap();
    assert!(
        serve.contains("--config") && serve.contains("--port"),
        "{serve}"
    );
    assert!(!serve.contains("--local"), "{serve}");
}

#[test]
fn zsh_completion_describes_flags_and_subcommands() {
    let script = app().completions(Shell::Zsh);
    assert!(script.starts_with("#compdef app\n"), "{script}");
    assert!(script.contains("compdef _app app"), "{script}");
    assert!(script.contains("'serve:Run the server'"), "{script}");
    assert!(script.contains("'--port'"), "{script}");
    assert!(script.contains("app,serve) cmd=\"app__serve\""), "{script}");
}

#[test]
fn fish_completion_tracks_subcommands_and_offers_choices() {
    let cmd = app().flag(
        Arg::new("format")
            .long("format")
            .help("Output format")
            .choices(["json", "yaml"]),
    );
    let script = cmd.completions(Shell::Fish);
    assert!(
        script.starts_with("# fish completion for app\n"),
        "{script}"
    );
    assert!(script.contains("function __app_complete_cmd\n"), "{script}");
    assert!(
        script.contains("set -l words (commandline -opc)"),
        "{script}"
    );
    // Subcommand transitions, aliases included.
    assert!(
        script.contains("if test \"$cmd,$w\" = 'app,serve'\n            set cmd app__serve"),
        "{script}"
    );
    assert!(
        script.contains("else if test \"$cmd,$w\" = 'app,run'\n            set cmd app__serve"),
        "{script}"
    );
    assert!(script.contains("'app__serve,status'"), "{script}");
    let root = "complete -c 'app' -n 'test (__app_complete_cmd) = app'";
    assert!(
        script.contains(&format!(
            "{root} -l 'format' -x -a '\\'json\\' \\'yaml\\'' -d 'Output format'\n"
        )),
        "{script}"
    );
    assert!(
        script.contains(&format!("{root} -l 'verbose' -s 'v'\n")),
        "{script}"
    );
    assert!(
        script.contains(&format!("{root} -f -a '\\'serve\\'' -d 'Run the server'\n")),
        "{script}"
    );
    assert!(
        script.contains(&format!("{root} -l 'version' -d 'Print version'\n")),
        "{script}"
    );
    // Global flags reach the subcommand, local ones do not.
    let serve = "complete -c 'app' -n 'test (__app_complete_cmd) = app__serve'";
    assert!(
        script.contains(&format!("{serve} -l 'config' -r -F\n")),
        "{script}"
    );
    assert!(
        script.contains(&format!("{serve} -l 'port' -s 'p' -r -F\n")),
        "{script}"
    );
    assert!(!script.contains(&format!("{serve} -l 'local'")), "{script}");
}

#[test]
fn powershell_completion_tracks_subcommands_and_offers_choices() {
    let cmd = app().flag(
        Arg::new("format")
            .long("format")
            .help("Owner's format")
            .choices(["json", "yaml"]),
    );
    let script = cmd.completions(Shell::PowerShell);
    assert!(
        script.contains(
            "Register-ArgumentCompleter -Native -CommandName 'app', 'app.exe' -ScriptBlock {\n"
        ),
        "{script}"
    );
    assert!(
        script.contains("param($wordToComplete, $commandAst, $cursorPosition)"),
        "{script}"
    );
    assert!(
        script.contains("'app,serve' { $cmd = 'app__serve' }"),
        "{script}"
    );
    assert!(
        script.contains("'app,run' { $cmd = 'app__serve' }"),
        "{script}"
    );
    assert!(
        script.contains("'app__serve,status' { $cmd = 'app__serve__status' }"),
        "{script}"
    );
    assert!(
        script
            .contains("if (@('--format') -ccontains $prev) { $values = @('json', 'yaml'); break }"),
        "{script}"
    );
    // A value without choices falls back to path completion.
    assert!(
        script.contains("if (@('--config') -ccontains $prev) { return }"),
        "{script}"
    );
    // Single quotes in help text are doubled.
    assert!(
        script.contains("$words += ,@('--format', 'ParameterName', 'Owner''s format')"),
        "{script}"
    );
    assert!(
        script.contains("$words += ,@('serve', 'ParameterValue', 'Run the server')"),
        "{script}"
    );
    // Braces balance (a cheap syntax sanity check without a PowerShell).
    let open = script.matches('{').count();
    let close = script.matches('}').count();
    assert_eq!(open, close, "{script}");
}

// ---- #98: command-line order (occurrences, indices_of, subcommand_index) ---

fn embed_tool() -> Command {
    Command::new("tool")
        .flag(Arg::new("config").long("config").global())
        .subcommand(
            Command::new("embed")
                .flag(Arg::new("input").short('i').long("input").multiple())
                .flag(Arg::new("tokens").short('t').long("tokens").multiple())
                .flag(Arg::new("verbose").short('v').count())
                .flag(Arg::new("quiet").short('q').boolean())
                .positional(Arg::new("rest").variadic()),
        )
}

#[test]
fn occurrences_keep_the_order_across_repeatable_flags() {
    // The example of the issue, verbatim.
    let argv = args(&[
        "embed",
        "--input",
        "first",
        "--tokens=1,2,3",
        "--input",
        "second",
        "--tokens",
        "7",
    ]);
    let m = embed_tool().parse(argv).unwrap();
    let (_, sub) = m.subcommand().unwrap();

    let ordered: Vec<(&str, &str)> = sub.occurrences().collect();
    assert_eq!(
        ordered,
        [
            ("input", "first"),
            ("tokens", "1,2,3"),
            ("input", "second"),
            ("tokens", "7")
        ]
    );
    assert_eq!(sub.indices_of("input"), &[2, 5]);
    assert_eq!(sub.indices_of("tokens"), &[3, 7]);
    assert_eq!(m.subcommand_index(), Some(0));
    // The parent level parsed no flag of its own.
    assert_eq!(m.occurrences().count(), 0);
}

#[test]
fn occurrences_cover_every_spelling_and_stop_at_double_dash() {
    let argv = args(&[
        "embed",
        "-i",
        "a",    // short, separate value
        "-ib",  // short, attached value
        "-t=c", // short, `=` value
        "-vv",  // clustered counter: two entries
        "-vq",  // cluster of a counter and a switch
        "--input=d",
        "--input",
        "e",
        "--",
        "--input",
        "f",
        "-v",
    ]);
    let m = embed_tool().parse(argv).unwrap();
    let (_, sub) = m.subcommand().unwrap();
    let ordered: Vec<(&str, &str)> = sub.occurrences().collect();
    assert_eq!(
        ordered,
        [
            ("input", "a"),
            ("input", "b"),
            ("tokens", "c"),
            ("verbose", ""),
            ("verbose", ""),
            ("verbose", ""),
            ("quiet", ""),
            ("input", "d"),
            ("input", "e"),
        ],
        "nothing after `--` is an occurrence"
    );
    assert_eq!(sub.get_count("verbose"), 3);
    assert_eq!(sub.get_all("rest"), ["--input", "f", "-v"]);

    // A switch's or counter's explicit `=value` is its occurrence value.
    let m = embed_tool()
        .parse(args(&["embed", "-q=false", "-v=2"]))
        .unwrap();
    let (_, sub) = m.subcommand().unwrap();
    let ordered: Vec<(&str, &str)> = sub.occurrences().collect();
    assert_eq!(ordered, [("quiet", "false"), ("verbose", "2")]);
}

#[test]
fn indices_of_is_parallel_to_get_all_for_every_kind() {
    let cmd = Command::new("t")
        .flag(Arg::new("one").long("one").short('o'))
        .flag(Arg::new("many").long("many").multiple())
        .flag(Arg::new("ids").long("ids").delimiter(','))
        .flag(Arg::new("verbose").short('v').long("verbose").count())
        .flag(Arg::new("quiet").short('q').long("quiet").boolean())
        .flag(Arg::new("dflt").long("dflt").default("x"))
        .positional(Arg::new("first"))
        .positional(Arg::new("rest").variadic());
    let argv = args(&[
        "--one",
        "a",         // 0, 1
        "pos1",      // 2
        "--many=m1", // 3
        "-oz",       // 4: the single-valued option keeps its last value
        "--ids",
        "1,2",       // 5, 6
        "-vv",       // 7
        "--many",    // 8
        "m2",        // 9
        "-q",        // 10
        "--verbose", // 11
        "pos2",      // 12
        "--ids=3",   // 13
        "--",        // 14
        "pos3",      // 15
    ]);
    let m = cmd.parse(argv).unwrap();

    let parallel = |name: &str| {
        assert_eq!(
            m.indices_of(name).len(),
            m.get_all(name).len(),
            "{name}: indices_of and get_all differ in length"
        );
    };
    for name in ["one", "many", "ids", "first", "rest"] {
        parallel(name);
    }
    assert_eq!(m.get_all("one"), ["z"]);
    assert_eq!(m.indices_of("one"), &[4]);
    assert_eq!(m.get_all("many"), ["m1", "m2"]);
    assert_eq!(m.indices_of("many"), &[3, 9]);
    assert_eq!(m.get_all("ids"), ["1", "2", "3"]);
    assert_eq!(
        m.indices_of("ids"),
        &[6, 6, 13],
        "a split token repeats its index"
    );
    assert_eq!(m.get("first"), Some("pos1"));
    assert_eq!(m.indices_of("first"), &[2]);
    assert_eq!(m.get_all("rest"), ["pos2", "pos3"]);
    assert_eq!(m.indices_of("rest"), &[12, 15]);

    // Switches and counters: one index per occurrence.
    assert_eq!(m.get_count("verbose"), 3);
    assert_eq!(m.indices_of("verbose"), &[7, 7, 11]);
    assert_eq!(m.indices_of("quiet"), &[10]);

    // Defaults (and absent arguments) have no position.
    assert_eq!(m.get("dflt"), Some("x"));
    assert!(m.indices_of("dflt").is_empty());
    assert!(m.indices_of("nope").is_empty());
}

#[test]
fn indices_of_covers_global_flags_on_both_sides_of_the_subcommand() {
    let cmd = Command::new("app")
        .flag(Arg::new("tag").long("tag").multiple().global())
        .flag(Arg::new("v").short('v').count().global())
        .subcommand(Command::new("run").positional(Arg::new("file")));
    let argv = args(&["--tag", "a", "-v", "run", "--tag=b", "-v", "f.txt"]);
    let m = cmd.parse(argv.clone()).unwrap();
    assert_eq!(m.get_all("tag"), ["a", "b"]);
    assert_eq!(m.indices_of("tag"), &[1, 4]);
    assert_eq!(m.indices_of("v"), &[2, 5]);
    let (_, run) = m.subcommand().unwrap();
    assert_eq!(
        run.indices_of("tag"),
        &[1, 4],
        "the global value is handed down"
    );
    assert_eq!(run.indices_of("file"), &[6]);
    assert_eq!(argv[run.indices_of("file")[0]], "f.txt");
    // Each level lists the flags written at that level.
    assert_eq!(
        m.occurrences().collect::<Vec<_>>(),
        [("tag", "a"), ("v", "")]
    );
    assert_eq!(
        run.occurrences().collect::<Vec<_>>(),
        [("tag", "b"), ("v", "")]
    );
}

#[test]
fn a_value_that_looks_like_a_flag_has_the_right_index() {
    let argv = args(&["embed", "--input", "--tokens", "--tokens", "7", "-i", "-v"]);
    let m = embed_tool().parse(argv.clone()).unwrap();
    let (_, sub) = m.subcommand().unwrap();
    assert_eq!(sub.get_all("input"), ["--tokens", "-v"]);
    assert_eq!(sub.indices_of("input"), &[2, 6]);
    assert_eq!(sub.get_all("tokens"), ["7"]);
    assert_eq!(sub.indices_of("tokens"), &[4]);
    assert_eq!(
        sub.get_count("verbose"),
        0,
        "`-v` was a value, not the counter"
    );
    assert_eq!(
        sub.occurrences().collect::<Vec<_>>(),
        [("input", "--tokens"), ("tokens", "7"), ("input", "-v")]
    );
    for (name, idx) in [("input", 2), ("input", 6), ("tokens", 4)] {
        assert!(sub.get_all(name).contains(&argv[idx]));
    }
}

#[test]
fn subcommand_index_skips_a_flag_value_equal_to_the_subcommand_name() {
    let argv = args(&["--config", "embed", "embed", "--input", "x", "tail"]);
    let m = embed_tool().parse(argv.clone()).unwrap();
    assert_eq!(m.get("config"), Some("embed"));
    assert_eq!(m.subcommand_name(), Some("embed"));
    let i = m.subcommand_index().unwrap();
    assert_eq!(i, 2);
    assert_eq!(&argv[i + 1..], args(&["--input", "x", "tail"]).as_slice());
    let (_, sub) = m.subcommand().unwrap();
    assert_eq!(sub.indices_of("input"), &[4]);
    assert_eq!(sub.indices_of("rest"), &[5]);
    assert_eq!(sub.subcommand_index(), None);

    // `--config=embed` and an alias resolve the same way.
    let aliased = Command::new("tool")
        .flag(Arg::new("config").long("config"))
        .subcommand(Command::new("embed").alias("e"));
    let m = aliased.parse(args(&["--config=embed", "e"])).unwrap();
    assert_eq!(m.subcommand_index(), Some(1));
    assert_eq!(m.subcommand_name(), Some("embed"));

    // No subcommand, no index.
    assert_eq!(aliased.parse(args(&[])).unwrap().subcommand_index(), None);
}

// ---- #94: dynamic `--<section>.<key>` flags --------------------------------

fn dynamic_app() -> Command {
    let sections = ["engine", "server"];
    Command::new("app")
        .flag(Arg::new("config").long("config"))
        .flag(Arg::new("verbose").short('v').boolean())
        .dynamic_flags(move |name| {
            name.split_once('.')
                .is_some_and(|(s, _)| sections.contains(&s))
        })
        .subcommand(Command::new("serve").flag(Arg::new("port").long("port")))
}

fn dynamic(list: &[(&str, Option<&str>)]) -> Vec<(String, Option<String>)> {
    list.iter()
        .map(|(k, v)| (k.to_string(), v.map(str::to_string)))
        .collect()
}

#[test]
fn dynamic_flags_are_collected_in_order_around_a_subcommand() {
    let m = dynamic_app()
        .parse(args(&[
            "--engine.threads",
            "4",
            "--config",
            "app.yaml",
            "serve",
            "--server.port=9000",
            "--port",
            "1",
            "--engine.deterministic",
        ]))
        .unwrap();
    assert_eq!(
        m.dynamic(),
        dynamic(&[
            ("engine.threads", Some("4")),
            ("server.port", Some("9000")),
            ("engine.deterministic", None),
        ])
    );
    assert_eq!(m.get("config"), Some("app.yaml"));
    let (name, serve) = m.subcommand().unwrap();
    assert_eq!(name, "serve");
    assert_eq!(serve.get("port"), Some("1"));
    assert_eq!(
        serve.dynamic(),
        dynamic(&[
            ("server.port", Some("9000")),
            ("engine.deterministic", None)
        ]),
        "a subcommand's matches hold its own level and below"
    );
    // Dynamic flags are not declared arguments.
    assert!(!m.contains("engine.threads"));
    assert_eq!(
        m.occurrences().collect::<Vec<_>>(),
        [("config", "app.yaml")]
    );
}

#[test]
fn dynamic_flag_value_rules() {
    let m = dynamic_app()
        .parse(args(&[
            "--engine.a",
            "-v", // starts with `-`: `a` is a switch
            "--engine.b",
            "-1", // a negative number is a value
            "--engine.c",
            "--engine.d=--x", // `=` takes anything
            "--engine.e",
            "serve", // a subcommand name is not taken as a value
            "--engine.f",
            "--",
        ]))
        .unwrap();
    assert!(m.get_bool("verbose"));
    assert_eq!(
        m.dynamic(),
        dynamic(&[
            ("engine.a", None),
            ("engine.b", Some("-1")),
            ("engine.c", None),
            ("engine.d", Some("--x")),
            ("engine.e", None),
            ("engine.f", None),
        ])
    );
    assert_eq!(m.subcommand_name(), Some("serve"));

    // `--key=serve` forces the value.
    let m = dynamic_app().parse(args(&["--engine.mode=serve"])).unwrap();
    assert_eq!(m.dynamic(), dynamic(&[("engine.mode", Some("serve"))]));
    assert_eq!(m.subcommand_name(), None);
}

#[test]
fn dynamic_flags_still_refuse_non_matching_flags_with_a_suggestion() {
    for (argv, flag, suggestion) in [
        (args(&["--confg", "x"]), "--confg", "--config"),
        (
            args(&["--engine.threads=4", "--confg", "x"]),
            "--confg",
            "--config",
        ),
        // In a subcommand, which inherits the predicate.
        (
            args(&["serve", "--server.port=1", "--prot=x"]),
            "--prot",
            "--port",
        ),
    ] {
        let err = dynamic_app().parse(argv.clone()).unwrap_err();
        assert_eq!(
            err,
            CliError::UnknownFlag {
                flag: flag.into(),
                suggestion: Some(suggestion.into()),
            },
            "{argv:?}"
        );
    }
    let err = dynamic_app()
        .parse(args(&["--cache.size", "1"]))
        .unwrap_err();
    assert!(matches!(err, CliError::UnknownFlag { ref flag, .. } if flag == "--cache.size"));
    let err = dynamic_app().parse(args(&["-x"])).unwrap_err();
    assert!(
        matches!(err, CliError::UnknownFlag { ref flag, .. } if flag == "-x"),
        "short flags are never dynamic"
    );
}

#[test]
fn declared_flags_and_help_win_over_dynamic_flags() {
    let everything = Command::new("app")
        .version("1.0")
        .flag(Arg::new("engine.x").long("engine.x").boolean())
        .dynamic_flags(|_| true)
        .subcommand(Command::new("serve"));

    let m = everything
        .parse(args(&["--engine.x", "--engine.y", "v"]))
        .unwrap();
    assert!(m.get_bool("engine.x"));
    assert_eq!(m.dynamic(), dynamic(&[("engine.y", Some("v"))]));

    // `--help` / `--version` are unaffected, at every level, and the help
    // text is the same as without dynamic flags.
    let plain = Command::new("app")
        .version("1.0")
        .flag(Arg::new("engine.x").long("engine.x").boolean())
        .subcommand(Command::new("serve"));
    for argv in [args(&["--help"]), args(&["serve", "--help"]), args(&["-h"])] {
        let got = everything.parse(argv.clone()).unwrap_err();
        let want = plain.parse(argv.clone()).unwrap_err();
        assert!(matches!(got, CliError::HelpRequested(_)), "{argv:?}");
        assert_eq!(got, want, "{argv:?}");
    }
    assert_eq!(everything.help(), plain.help());
    assert!(matches!(
        everything.parse(args(&["--version"])).unwrap_err(),
        CliError::VersionRequested(_)
    ));
}

#[test]
fn a_subcommand_may_set_its_own_dynamic_predicate() {
    let cmd = Command::new("app")
        .dynamic_flags(|n| n.starts_with("a."))
        .subcommand(Command::new("run").dynamic_flags(|n| n.starts_with("b.")));
    let m = cmd.parse(args(&["--a.x=1", "run", "--b.y=2"])).unwrap();
    assert_eq!(
        m.dynamic(),
        dynamic(&[("a.x", Some("1")), ("b.y", Some("2"))])
    );
    assert!(cmd.parse(args(&["run", "--a.x=1"])).is_err());
}

#[test]
fn without_dynamic_flags_nothing_changes() {
    let cmd = Command::new("app").flag(Arg::new("config").long("config"));
    let err = cmd.parse(args(&["--engine.threads", "4"])).unwrap_err();
    assert!(matches!(err, CliError::UnknownFlag { ref flag, .. } if flag == "--engine.threads"));
    let m = cmd.parse(args(&["--config", "x"])).unwrap();
    assert!(m.dynamic().is_empty());
    assert!(m.dynamic_indices().is_empty());
}

// ---- dynamic-flag arity, segments and positions (review CLI-1, 5, 7, 8) ----

/// `serve` with a variadic positional, and dynamic flags whose arity is
/// declared per name.
fn typed_dynamic_app() -> Command {
    Command::new("app")
        .dynamic_flags_with(|name| match name {
            "engine.det" => Some(DynamicArity::Switch),
            "engine.flag" => Some(DynamicArity::BoolSwitch),
            "engine.name" | "engine.n" => Some(DynamicArity::Value),
            n if n.starts_with("engine.") => Some(DynamicArity::Optional),
            _ => None,
        })
        .subcommand(Command::new("serve").positional(Arg::new("rest").variadic()))
}

// CLI-1: what `dynamic_flags` does, pinned.
#[test]
fn dynamic_switch_before_a_positional_takes_it() {
    let cmd = Command::new("app")
        .dynamic_flags(|n| n.starts_with("engine."))
        .positional(Arg::new("file"));
    let m = cmd.parse(args(&["--engine.det", "file.txt"])).unwrap();
    assert_eq!(m.dynamic(), dynamic(&[("engine.det", Some("file.txt"))]));
    assert!(m.positionals().is_empty());
    // `--key=true` avoids it.
    let m = cmd.parse(args(&["--engine.det=true", "file.txt"])).unwrap();
    assert_eq!(m.get("file"), Some("file.txt"));
}

// CLI-1
#[test]
fn a_switch_arity_never_takes_the_next_token() {
    let m = typed_dynamic_app()
        .parse(args(&["serve", "--engine.det", "x", "y"]))
        .unwrap();
    assert_eq!(m.dynamic(), dynamic(&[("engine.det", None)]));
    let (_, serve) = m.subcommand().unwrap();
    assert_eq!(serve.get_all("rest"), ["x", "y"]);

    // Not even a boolean word (#108: that is `BoolSwitch`).
    let m = typed_dynamic_app()
        .parse(args(&["serve", "--engine.det", "false"]))
        .unwrap();
    assert_eq!(m.dynamic(), dynamic(&[("engine.det", None)]));
    let (_, serve) = m.subcommand().unwrap();
    assert_eq!(serve.get_all("rest"), ["false"]);

    // Before the subcommand: a mistyped subcommand is reported as one.
    let err = typed_dynamic_app()
        .parse(args(&["--engine.det", "srve"]))
        .unwrap_err();
    assert!(
        matches!(
            err,
            CliError::UnknownCommand { ref name, suggestion: Some(ref s) }
                if name == "srve" && s == "serve"
        ),
        "{err:?}"
    );

    // `=value` reads the boolean words of a declared switch, normalised.
    let m = typed_dynamic_app()
        .parse(args(&["--engine.det=0", "serve", "--engine.det=YES"]))
        .unwrap();
    assert_eq!(
        m.dynamic(),
        dynamic(&[("engine.det", Some("false")), ("engine.det", Some("true"))])
    );
    let err = typed_dynamic_app()
        .parse(args(&["--engine.det=maybe"]))
        .unwrap_err();
    assert_eq!(
        err,
        CliError::InvalidValue {
            name: "engine.det".into(),
            value: "maybe".into()
        }
    );
}

// #108
#[test]
fn a_bool_switch_takes_a_following_true_or_false() {
    let m = typed_dynamic_app()
        .parse(args(&["--engine.flag", "false", "serve"]))
        .unwrap();
    assert_eq!(m.dynamic(), dynamic(&[("engine.flag", Some("false"))]));
    assert_eq!(m.subcommand_name(), Some("serve"));
    assert_eq!(m.dynamic_indices(), [1], "the value's token");

    let m = typed_dynamic_app()
        .parse(args(&["serve", "--engine.flag", "TRUE", "x"]))
        .unwrap();
    assert_eq!(m.dynamic(), dynamic(&[("engine.flag", Some("true"))]));
    assert_eq!(m.dynamic_indices(), [2], "the value's token");
    let (_, serve) = m.subcommand().unwrap();
    assert_eq!(serve.get_all("rest"), ["x"]);
}

// #108
#[test]
fn a_bool_switch_leaves_other_tokens_alone() {
    for next in ["file.txt", "1", "on"] {
        let m = typed_dynamic_app()
            .parse(args(&["serve", "--engine.flag", next]))
            .unwrap();
        assert_eq!(m.dynamic(), dynamic(&[("engine.flag", None)]), "{next}");
        assert_eq!(m.dynamic_indices(), [1], "the flag's own token");
        let (_, serve) = m.subcommand().unwrap();
        assert_eq!(serve.get_all("rest"), [next]);
    }

    // Before the subcommand: a mistyped subcommand is reported as one.
    let err = typed_dynamic_app()
        .parse(args(&["--engine.flag", "srve"]))
        .unwrap_err();
    assert!(
        matches!(
            err,
            CliError::UnknownCommand { ref name, suggestion: Some(ref s) }
                if name == "srve" && s == "serve"
        ),
        "{err:?}"
    );
}

// #108
#[test]
fn a_bool_switch_reads_inline_values_like_a_switch() {
    let m = typed_dynamic_app()
        .parse(args(&["--engine.flag=0", "serve", "--engine.flag=YES"]))
        .unwrap();
    assert_eq!(
        m.dynamic(),
        dynamic(&[
            ("engine.flag", Some("false")),
            ("engine.flag", Some("true"))
        ])
    );
    let err = typed_dynamic_app()
        .parse(args(&["--engine.flag=maybe"]))
        .unwrap_err();
    assert_eq!(
        err,
        CliError::InvalidValue {
            name: "engine.flag".into(),
            value: "maybe".into()
        }
    );
}

// #108
#[test]
fn a_bool_switch_does_not_take_a_subcommand_or_the_terminator() {
    let m = typed_dynamic_app()
        .subcommand(Command::new("false"))
        .parse(args(&["--engine.flag", "false"]))
        .unwrap();
    assert_eq!(m.dynamic(), dynamic(&[("engine.flag", None)]));
    assert_eq!(m.subcommand_name(), Some("false"));

    let m = typed_dynamic_app()
        .parse(args(&["serve", "--engine.flag", "--", "false"]))
        .unwrap();
    assert_eq!(m.dynamic(), dynamic(&[("engine.flag", None)]));
    let (_, serve) = m.subcommand().unwrap();
    assert_eq!(serve.get_all("rest"), ["false"]);
}

// #108
#[test]
fn a_bool_switch_at_the_end_is_a_switch() {
    let m = typed_dynamic_app()
        .parse(args(&["serve", "--engine.flag"]))
        .unwrap();
    assert_eq!(m.dynamic(), dynamic(&[("engine.flag", None)]));
}

// CLI-1
#[test]
fn a_value_arity_always_takes_the_next_token() {
    let m = typed_dynamic_app()
        .parse(args(&[
            "--engine.name",
            "-x", // a single dash is a value
            "--engine.n",
            "serve", // so is a subcommand name
            "serve",
            "--engine.name=--y",
        ]))
        .unwrap();
    assert_eq!(
        m.dynamic(),
        dynamic(&[
            ("engine.name", Some("-x")),
            ("engine.n", Some("serve")),
            ("engine.name", Some("--y")),
        ])
    );
    assert_eq!(m.subcommand_name(), Some("serve"));

    // Never a `--` token, and never nothing: both are a missing value.
    for argv in [
        args(&["--engine.name"]),
        args(&["--engine.name", "--engine.det"]),
        args(&["--engine.name", "--", "x"]),
    ] {
        assert_eq!(
            typed_dynamic_app().parse(argv.clone()).unwrap_err(),
            CliError::MissingValue {
                flag: "--engine.name".into()
            },
            "{argv:?}"
        );
    }

    // `None` refuses the name; `Optional` keeps the old heuristic.
    assert!(matches!(
        typed_dynamic_app()
            .parse(args(&["--server.port=1"]))
            .unwrap_err(),
        CliError::UnknownFlag { .. }
    ));
    let m = typed_dynamic_app()
        .parse(args(&["--engine.other", "v", "--engine.more", "serve"]))
        .unwrap();
    assert_eq!(
        m.dynamic(),
        dynamic(&[("engine.other", Some("v")), ("engine.more", None)])
    );
}

// CLI-5
#[test]
fn dynamic_flag_with_an_empty_segment_is_unknown() {
    for flag in [
        "--engine.=1",
        "--engine..a=1",
        "--.engine=1",
        "--engine.",
        "--=1",
    ] {
        let err = dynamic_app().parse(args(&[flag])).unwrap_err();
        assert!(
            matches!(err, CliError::UnknownFlag { .. }),
            "{flag}: {err:?}"
        );
    }
    // Even with a predicate that accepts everything.
    let all = Command::new("app").dynamic_flags(|_| true);
    assert!(all.parse(args(&["--a..b=1"])).is_err());
    assert_eq!(
        all.parse(args(&["--a.b=1"])).unwrap().dynamic(),
        dynamic(&[("a.b", Some("1"))])
    );
}

// CLI-7
#[test]
fn a_lone_dash_is_a_dynamic_value() {
    let m = dynamic_app()
        .parse(args(&["--engine.input", "-", "--engine.n", "-v"]))
        .unwrap();
    assert_eq!(
        m.dynamic(),
        dynamic(&[("engine.input", Some("-")), ("engine.n", None)])
    );
    assert!(m.get_bool("verbose"));
}

// CLI-8
#[test]
fn dynamic_indices_are_parallel_to_dynamic() {
    let argv = args(&[
        "--engine.a",   // 0: a switch (the next token is a flag)
        "--config",     // 1
        "c",            // 2
        "--engine.b",   // 3
        "4",            // 4: the value's index
        "serve",        // 5
        "--engine.c=x", // 6
        "--port",       // 7
        "80",           // 8
        "--engine.d",   // 9: a switch at the end
    ]);
    let m = dynamic_app().parse(argv).unwrap();
    assert_eq!(
        m.dynamic(),
        dynamic(&[
            ("engine.a", None),
            ("engine.b", Some("4")),
            ("engine.c", Some("x")),
            ("engine.d", None),
        ])
    );
    assert_eq!(m.dynamic_indices(), [0, 4, 6, 9]);
    assert_eq!(m.indices_of("config"), [2]);
    assert_eq!(m.subcommand_index(), Some(5));
    // A subcommand's matches hold its own level, with the same numbering.
    let (_, serve) = m.subcommand().unwrap();
    assert_eq!(serve.dynamic_indices(), [6, 9]);
    assert_eq!(serve.indices_of("port"), [8]);
}

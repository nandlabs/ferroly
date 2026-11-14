#![cfg(feature = "cli")]
//! Regression and behaviour tests for the `cli` parser: boolean values,
//! built-in help/version precedence, negative numbers, positionals, global
//! flags, repeated/counted flags, validation, groups, help text and shell
//! completion.

use ferroly::cli::{Arg, ArgValue, CliError, Command, Group, Shell, ValueSource};

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
    assert!(script.contains("compgen -W \"json yaml\""), "{script}");
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

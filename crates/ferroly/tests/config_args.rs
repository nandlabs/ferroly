#![cfg(feature = "config")]
//! Typed command-line argument merging (`merge_args_with`, `ArgsOptions`) and
//! the dynamic `--<section>.<key>` flags of `cli` fed to `merge_cli`.

use ferroly::codec::Value;
use ferroly::config::{ArgsOptions, Config, Layer, Source};

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

fn obj(pairs: &[(&str, Value)]) -> Value {
    Value::Object(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    )
}

/// What `AppConfig::default().encode()` would give.
fn schema() -> Value {
    obj(&[
        (
            "a",
            obj(&[
                ("n", Value::Int(1)),
                ("b", Value::Bool(false)),
                ("name", Value::Str("x".into())),
                ("tags", Value::Array(vec![])),
                ("seed", Value::Null),
            ]),
        ),
        (
            "engine",
            obj(&[
                ("threads", Value::UInt(1)),
                ("deterministic", Value::Bool(false)),
            ]),
        ),
    ])
}

fn typed() -> ArgsOptions {
    ArgsOptions::default().types(&schema())
}

#[test]
fn typed_flags_need_no_earlier_layer() {
    // The flags layer on its own, with no other layer in the builder.
    let cfg = Config::builder()
        .merge_args_with(["--a.n", "16"], typed())
        .build();
    assert_eq!(cfg.get::<u32>("a.n").unwrap(), 16);
    assert_eq!(
        cfg.value(),
        &obj(&[("a", obj(&[("n", Value::Str("16".into()))]))])
    );
    assert_eq!(cfg.source("a.n"), Some(&Source::Args));

    // The same argv without types reads `--a.n` as a switch (the 0.4.0 rule).
    let untyped = Config::builder()
        .merge_args_with(["--a.n", "16"], ArgsOptions::default())
        .build();
    assert_eq!(untyped.get::<String>("a.n").unwrap(), "true");
}

#[test]
fn typed_bool_keys_stay_switches() {
    let cfg = Config::builder()
        .merge_args_with(["--a.b", "input.txt", "--engine.deterministic"], typed())
        .build();
    assert_eq!(
        cfg.value().get("a").and_then(|a| a.get("b")),
        Some(&Value::Bool(true))
    );
    assert!(cfg.get::<bool>("engine.deterministic").unwrap());
    assert!(!cfg.contains("input.txt"), "a positional is left alone");

    // `--key false`, `--key=true|false` are accepted.
    let cfg = Config::builder()
        .merge_args_with(["--a.b", "false", "--engine.deterministic=FALSE"], typed())
        .build();
    assert!(!cfg.get::<bool>("a.b").unwrap());
    assert!(!cfg.get::<bool>("engine.deterministic").unwrap());
    let cfg = Config::builder()
        .merge_args_with(["--a.b=true", "--engine.deterministic", "true"], typed())
        .build();
    assert!(cfg.get::<bool>("a.b").unwrap());
    assert!(cfg.get::<bool>("engine.deterministic").unwrap());
}

#[test]
fn typed_value_keys_take_a_dash_value() {
    let cfg = Config::builder()
        .merge_args_with(
            ["--a.n", "-1", "--a.name", "-x", "--engine.threads=8"],
            typed(),
        )
        .build();
    assert_eq!(cfg.get::<i64>("a.n").unwrap(), -1);
    assert_eq!(cfg.get::<String>("a.name").unwrap(), "-x");
    assert_eq!(cfg.get::<u32>("engine.threads").unwrap(), 8);

    // `=` takes anything, also a value starting with `--`.
    let cfg = Config::builder()
        .merge_args_with(["--a.name=--odd"], typed())
        .build();
    assert_eq!(cfg.get::<String>("a.name").unwrap(), "--odd");
}

// CLI-2
#[test]
fn typed_value_key_does_not_take_a_double_dash_flag() {
    let cfg = Config::builder()
        .merge_args_with(["--a.name", "--engine.deterministic"], typed())
        .build();
    assert!(cfg.get::<bool>("engine.deterministic").unwrap());
    // The value key had no value: it merges `true`, which is not a string.
    assert_eq!(
        cfg.value().get("a").and_then(|a| a.get("name")),
        Some(&Value::Bool(true))
    );
    assert!(cfg.get::<String>("a.name").is_err());
}

// CLI-4
#[test]
fn typed_list_key_accumulates_repeats() {
    let cfg = Config::builder()
        .merge_args_with(
            [
                "--a.tags",
                "a",
                "--a.n",
                "2",
                "--a.tags=b",
                "--a.tags",
                "c,d",
            ],
            typed(),
        )
        .build();
    assert_eq!(
        cfg.get::<Vec<String>>("a.tags").unwrap(),
        ["a", "b", "c,d"],
        "one element per occurrence, commas kept"
    );
    assert_eq!(cfg.get::<u32>("a.n").unwrap(), 2);

    // A single occurrence is a one-element list, and a later layer's list
    // replaces an earlier one as a whole.
    let cfg = Config::builder()
        .merge_value(obj(&[(
            "a",
            obj(&[("tags", Value::Array(vec![Value::Str("old".into())]))]),
        )]))
        .merge_args_with(["--a.tags", "x"], typed())
        .build();
    assert_eq!(cfg.get::<Vec<String>>("a.tags").unwrap(), ["x"]);

    // Without types a list key keeps the earlier-layers rule: last wins.
    let cfg = Config::builder()
        .merge_value(schema())
        .merge_args(["--a.tags", "a", "--a.tags", "b"])
        .build();
    assert_eq!(cfg.get::<Vec<String>>("a.tags").unwrap(), ["b"]);
}

// CLI-6
#[test]
fn typed_bool_key_accepts_the_cli_spellings() {
    for (word, want) in [
        ("1", true),
        ("0", false),
        ("yes", true),
        ("NO", false),
        ("on", true),
        ("off", false),
        ("true", true),
        ("False", false),
    ] {
        let flag = format!("--a.b={word}");
        let cfg = Config::builder()
            .merge_args_with([flag.as_str()], typed())
            .build();
        assert_eq!(cfg.get::<bool>("a.b").unwrap(), want, "{flag}");
    }
    // The next token is read only when it is `true`/`false`, so `1` stays a
    // positional after a switch.
    let cfg = Config::builder()
        .merge_args_with(["--a.b", "1", "--a.n=3"], typed())
        .build();
    assert!(cfg.get::<bool>("a.b").unwrap());
    assert_eq!(cfg.get::<u32>("a.n").unwrap(), 3);
    // A word that is not a boolean is merged as is and fails to convert.
    let cfg = Config::builder()
        .merge_args_with(["--a.b=maybe"], typed())
        .build();
    assert!(cfg.get::<bool>("a.b").is_err());
}

// CLI-10
#[test]
fn types_with_value_follows_reads_an_unset_option() {
    // `seed` is `null` in the schema (an unset `Option`).
    let cfg = Config::builder()
        .merge_args_with(["--a.seed", "5", "--a.b", "x.txt"], typed().value_follows())
        .build();
    assert_eq!(cfg.get::<u64>("a.seed").unwrap(), 5);
    assert!(cfg.get::<bool>("a.b").unwrap());
}

// CLI-3: `merge_args` and `Layer::args` are the default-options shorthand,
// not deprecated (this binary would fail clippy `-D warnings` otherwise).
#[test]
fn merge_args_and_layer_args_are_the_default_options_shorthand() {
    let earlier = obj(&[("port", Value::Int(1))]);
    let argv = ["--port", "9", "--verbose", "x"];
    let short = Config::builder()
        .merge_value(earlier.clone())
        .merge_args(argv)
        .build();
    let long = Config::builder()
        .merge_value(earlier.clone())
        .merge_args_with(argv, ArgsOptions::default())
        .build();
    assert_eq!(short.value(), long.value());
    let listed = Config::from_layers([Layer::Value(earlier), Layer::args(argv)]).unwrap();
    assert_eq!(listed.value(), long.value());
}

#[test]
fn typed_double_dash_ends_processing_and_is_never_a_value() {
    let cfg = Config::builder()
        .merge_args_with(["--a.n=2", "--", "--a.name", "y"], typed())
        .build();
    assert_eq!(cfg.get::<u32>("a.n").unwrap(), 2);
    assert!(!cfg.contains("a.name"));

    let cfg = Config::builder()
        .merge_args_with(["--a.name", "--", "--a.n=5"], typed())
        .build();
    assert!(!cfg.contains("a.n"), "`--` stops even after a value key");
    // A value key left without its value reads as `true`, so using it as the
    // string it should be fails loudly instead of silently.
    assert_eq!(
        cfg.value().get("a").and_then(|a| a.get("name")),
        Some(&Value::Bool(true))
    );
    assert!(cfg.get::<String>("a.name").is_err());
}

#[test]
fn typed_unknown_keys_are_switches_unless_value_follows() {
    // Not in the schema, `null` (an unset `Option`) and a section are unknown.
    let argv = ["--other", "v1", "--a.seed", "v2", "--engine", "v3"];
    let cfg = Config::builder().merge_args_with(argv, typed()).build();
    assert!(cfg.get::<bool>("other").unwrap());
    assert!(cfg.get::<bool>("a.seed").unwrap());
    assert!(cfg.get::<bool>("engine").unwrap());

    let cfg = Config::builder()
        .merge_args_with(argv, typed().value_follows())
        .build();
    assert_eq!(cfg.get::<String>("other").unwrap(), "v1");
    assert_eq!(cfg.get::<String>("a.seed").unwrap(), "v2");

    // `value_follows` never takes a token starting with `-`.
    let cfg = Config::builder()
        .merge_args_with(["--other", "-1"], typed().value_follows())
        .build();
    assert!(cfg.get::<bool>("other").unwrap());
}

#[test]
fn value_follows_without_types_only_changes_unknown_keys() {
    let earlier = obj(&[("debug", Value::Bool(false)), ("port", Value::Int(1))]);
    let cfg = Config::builder()
        .merge_value(earlier)
        .merge_args_with(
            ["--debug", "a.txt", "--port", "2", "--fresh", "v", "--last"],
            ArgsOptions::new().value_follows(),
        )
        .build();
    assert_eq!(cfg.get::<String>("debug").unwrap(), "true");
    assert_eq!(cfg.get::<u16>("port").unwrap(), 2);
    assert_eq!(cfg.get::<String>("fresh").unwrap(), "v");
    assert_eq!(cfg.get::<String>("last").unwrap(), "true");
    assert!(!cfg.contains("a.txt"));
}

#[test]
fn the_types_replace_the_earlier_layers() {
    // An earlier layer says `port` is a number, the schema says it is a bool:
    // the schema decides.
    let cfg = Config::builder()
        .merge_value(obj(&[("port", Value::Int(1))]))
        .merge_args_with(
            ["--port", "true"],
            ArgsOptions::default().types(&obj(&[("port", Value::Bool(false))])),
        )
        .build();
    assert_eq!(cfg.value().get("port"), Some(&Value::Bool(true)));
}

#[test]
fn default_options_behave_exactly_like_merge_args() {
    let earlier = obj(&[
        (
            "db",
            obj(&[("port", Value::Int(1)), ("host", Value::Str("h".into()))]),
        ),
        ("debug", Value::Bool(false)),
        ("quiet", Value::Str("false".into())),
    ]);
    let cases: &[&[&str]] = &[
        &["--db.port", "5432", "--db.host", "prod", "positional"],
        &["--debug", "a.txt", "--quiet", "b.txt", "--fresh", "x"],
        &["--db.host", "--other=1", "--", "--db.port=9"],
        &["--db.port", "-5", "--empty=", "--a..b=1", "--=x"],
        &[],
    ];
    for argv in cases {
        let old = Config::builder()
            .merge_value(earlier.clone())
            .merge_args(strings(argv))
            .build();
        let new = Config::builder()
            .merge_value(earlier.clone())
            .merge_args_with(argv.iter().copied(), ArgsOptions::default())
            .build();
        assert_eq!(old.value(), new.value(), "{argv:?}");
        let sources = |c: &Config| -> Vec<(String, Source)> {
            c.sources()
                .map(|(k, s)| (k.to_string(), s.clone()))
                .collect()
        };
        assert_eq!(sources(&old), sources(&new), "{argv:?}");
    }
}

#[test]
fn args_with_layer_matches_the_builder_call() {
    let argv = ["--a.n", "3", "--a.b"];
    let chained = Config::builder().merge_args_with(argv, typed()).build();
    let listed = Config::from_layers([Layer::args_with(argv, typed())]).unwrap();
    assert_eq!(listed.value(), chained.value());

    // `Debug` hides the arguments and the schema.
    let shown = format!(
        "{:?}",
        Layer::args_with(
            ["--a.name=s3cret"],
            ArgsOptions::new().types(&obj(&[(
                "a",
                obj(&[("name", Value::Str("hidden-default".into()))])
            )]))
        )
    );
    assert!(
        !shown.contains("s3cret") && !shown.contains("hidden-default"),
        "{shown}"
    );
    assert!(shown.contains("ArgsWith"), "{shown}");
}

/// Runs `f` on a thread with a small stack, so a recursion as deep as a key
/// is long crashes the test instead of passing on the main thread's stack.
fn on_small_stack(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap();
}

/// `--k.k.k…=1` with `depth` segments.
fn deep_flag(depth: usize) -> String {
    format!("--{}=1", vec!["k"; depth].join("."))
}

#[test]
fn a_very_deep_key_does_not_overflow_the_stack() {
    on_small_stack(|| {
        // ~60k segments in one argument: ignored, like an empty segment, by
        // both argument parsers.
        let cfg = Config::builder()
            .merge_args([deep_flag(60_000), "--ok=1".to_string()])
            .merge_args_with([deep_flag(60_000)], typed())
            .build();
        assert!(!cfg.contains("k"));
        assert_eq!(cfg.get::<String>("ok").unwrap(), "1");
        assert_eq!(cfg.sources().count(), 1);
    });

    // The limit is the codec's nesting depth: 128 segments merge, 129 do not.
    let at_limit = vec!["k"; 128].join(".");
    let cfg = Config::builder()
        .merge_args([deep_flag(128), deep_flag(129).replacen("--k", "--x", 1)])
        .build();
    assert_eq!(cfg.get::<String>(&at_limit).unwrap(), "1");
    assert!(!cfg.contains("x"));
}

#[test]
fn many_distinct_keys_merge_in_order() {
    // Merging used to scan every key already merged for each new one
    // (quadratic); 50k keys now take moments in a debug build.
    let n: usize = 50_000;
    let argv: Vec<String> = (0..n)
        .map(|i| format!("--s{}.k{i}={i}", i % 3))
        .chain(["--s0.k0=again".to_string()])
        .collect();
    let base = obj(&[("s1", obj(&[("k1", Value::Str("base".into()))]))]);
    let cfg = Config::builder()
        .merge_value(base)
        .merge_args(&argv)
        .build();
    let Value::Object(sections) = cfg.value() else {
        panic!("not an object");
    };
    let names: Vec<&str> = sections.iter().map(|(k, _)| k.as_str()).collect();
    // The earlier layer's section stays first; new ones follow in order.
    assert_eq!(names, ["s1", "s0", "s2"]);
    assert_eq!(cfg.get::<String>("s0.k0").unwrap(), "again");
    assert_eq!(cfg.get::<String>("s1.k1").unwrap(), "1");
    assert_eq!(cfg.get::<String>("s2.k49997").unwrap(), "49997");
    let Some(Value::Object(s0)) = cfg.value().get("s0") else {
        panic!("s0 is not an object");
    };
    assert_eq!(s0.len(), n.div_ceil(3));
    assert_eq!(s0[1].0, "k3");
    assert_eq!(cfg.sources().count(), n);
}

#[cfg(feature = "cli")]
mod cli {
    use super::*;
    use ferroly::cli::{Arg, CliError, Command};

    fn app() -> Command {
        let sections: Vec<String> = match schema() {
            Value::Object(entries) => entries.into_iter().map(|(k, _)| k).collect(),
            _ => unreachable!(),
        };
        Command::new("app")
            .flag(Arg::new("config").long("config"))
            .flag(Arg::new("port").long("port").default("80"))
            .dynamic_flags(move |name| {
                name.split_once('.')
                    .is_some_and(|(s, _)| sections.iter().any(|x| x == s))
            })
            .subcommand(Command::new("serve"))
    }

    fn parse(list: &[&str]) -> ferroly::cli::Matches {
        app().parse(strings(list)).unwrap()
    }

    #[test]
    fn merge_cli_merges_dynamic_flags() {
        // `app --engine.threads 4 serve --a.name=n --engine.deterministic`
        let m = parse(&[
            "--engine.threads",
            "4",
            "--config",
            "app.yaml",
            "serve",
            "--a.name=n",
            "--engine.deterministic",
        ]);
        let cfg = Config::builder()
            .merge_value(schema())
            .merge_cli(&m)
            .build();
        assert_eq!(cfg.get::<u32>("engine.threads").unwrap(), 4);
        assert_eq!(cfg.get::<String>("a.name").unwrap(), "n");
        assert!(cfg.get::<bool>("engine.deterministic").unwrap());
        assert_eq!(cfg.get::<String>("config").unwrap(), "app.yaml");
        assert!(!cfg.contains("port"), "a default is still left out");
        assert_eq!(cfg.source("engine.threads"), Some(&Source::Cli));
        assert_eq!(cfg.source("engine.deterministic"), Some(&Source::Cli));

        // The flags layer on its own, from the cli matches.
        let flags = Config::builder().merge_cli(&m).build();
        assert_eq!(flags.get::<u32>("engine.threads").unwrap(), 4);
        assert_eq!(
            flags
                .value()
                .get("engine")
                .and_then(|e| e.get("deterministic")),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn merge_cli_with_types_dynamic_booleans() {
        let m = parse(&[
            "--engine.deterministic=false",
            "--a.name=true",
            "--engine.threads=2",
        ]);
        let untyped = Config::builder().merge_cli(&m).build();
        assert_eq!(
            untyped
                .value()
                .get("engine")
                .and_then(|e| e.get("deterministic")),
            Some(&Value::Str("false".into()))
        );

        let cfg = Config::builder().merge_cli_with(&m, typed()).build();
        let engine = cfg.value().get("engine").unwrap();
        assert_eq!(engine.get("deterministic"), Some(&Value::Bool(false)));
        assert_eq!(engine.get("threads"), Some(&Value::Str("2".into())));
        assert_eq!(
            cfg.value().get("a").and_then(|a| a.get("name")),
            Some(&Value::Str("true".into())),
            "a string key keeps the word"
        );
    }

    // CLI-4
    #[test]
    fn merge_cli_with_accumulates_a_dynamic_list_key() {
        let m = parse(&["--a.tags", "a", "serve", "--a.tags=b", "--a.n=1"]);
        let cfg = Config::builder().merge_cli_with(&m, typed()).build();
        assert_eq!(cfg.get::<Vec<String>>("a.tags").unwrap(), ["a", "b"]);
        assert_eq!(cfg.get::<u32>("a.n").unwrap(), 1);
        assert_eq!(cfg.source("a.tags"), Some(&Source::Cli));
        // Untyped, the last one wins as before.
        let cfg = Config::builder().merge_cli(&m).build();
        assert_eq!(cfg.get::<Vec<String>>("a.tags").unwrap(), ["b"]);
    }

    // CLI-6
    #[test]
    fn merge_cli_with_reads_the_cli_boolean_spellings() {
        let m = parse(&["--engine.deterministic=0", "--a.b=yes", "--a.name=1"]);
        let cfg = Config::builder().merge_cli_with(&m, typed()).build();
        assert!(!cfg.get::<bool>("engine.deterministic").unwrap());
        assert!(cfg.get::<bool>("a.b").unwrap());
        assert_eq!(cfg.get::<String>("a.name").unwrap(), "1");
    }

    // CLI-1: the parser reads each flag by its schema type.
    #[test]
    fn dynamic_arity_from_the_schema_keeps_positionals() {
        use ferroly::cli::DynamicArity;
        use ferroly::config::dynamic_arity;

        let arity = dynamic_arity(&schema());
        assert_eq!(arity("a.b"), Some(DynamicArity::BoolSwitch));
        assert_eq!(arity("a.n"), Some(DynamicArity::Value));
        assert_eq!(arity("a.tags"), Some(DynamicArity::Value));
        assert_eq!(arity("a.seed"), Some(DynamicArity::Optional));
        assert_eq!(arity("a"), None, "a section");
        assert_eq!(arity("a.nope"), None, "not in the schema");

        let cmd = Command::new("app")
            .dynamic_flags_with(dynamic_arity(&schema()))
            .subcommand(Command::new("serve").positional(Arg::new("rest").variadic()));
        let m = cmd
            .parse(strings(&[
                "serve",
                "--engine.deterministic",
                "x",
                "--a.n",
                "-1",
                "--a.tags",
                "t1",
                "y",
                "--a.tags",
                "t2",
            ]))
            .unwrap();
        let (_, serve) = m.subcommand().unwrap();
        assert_eq!(serve.get_all("rest"), ["x", "y"]);

        let cfg = Config::builder().merge_cli_with(&m, typed()).build();
        assert!(cfg.get::<bool>("engine.deterministic").unwrap());
        assert_eq!(cfg.get::<i64>("a.n").unwrap(), -1);
        assert_eq!(cfg.get::<Vec<String>>("a.tags").unwrap(), ["t1", "t2"]);

        // A mistyped key is refused by the parser.
        let err = cmd.parse(strings(&["--engine.thread=4"])).unwrap_err();
        assert!(matches!(err, CliError::UnknownFlag { .. }), "{err:?}");
    }

    // #108: a boolean key also reads `--key false`, before or after a
    // subcommand.
    #[test]
    fn dynamic_arity_reads_a_following_boolean_word() {
        use ferroly::config::dynamic_arity;

        let cmd = Command::new("app")
            .dynamic_flags_with(dynamic_arity(&schema()))
            .subcommand(Command::new("serve").positional(Arg::new("rest").variadic()));

        for argv in [
            &["--engine.deterministic", "false", "serve"][..],
            &["serve", "--engine.deterministic", "False"][..],
        ] {
            let m = cmd.parse(strings(argv)).unwrap();
            assert_eq!(m.subcommand_name(), Some("serve"), "{argv:?}");
            let (_, serve) = m.subcommand().unwrap();
            assert!(serve.get_all("rest").is_empty(), "{argv:?}");
            let cfg = Config::builder()
                .merge_value(obj(&[(
                    "engine",
                    obj(&[("deterministic", Value::Bool(true))]),
                )]))
                .merge_cli_with(&m, typed())
                .build();
            assert!(
                !cfg.get::<bool>("engine.deterministic").unwrap(),
                "{argv:?}"
            );
        }

        let m = cmd
            .parse(strings(&["serve", "--engine.deterministic"]))
            .unwrap();
        let cfg = Config::builder().merge_cli_with(&m, typed()).build();
        assert!(cfg.get::<bool>("engine.deterministic").unwrap());
    }

    // #108: the two halves of "config keys as command-line flags" agree.
    #[test]
    fn dynamic_arity_and_merge_args_with_agree_on_booleans() {
        use ferroly::config::dynamic_arity;

        let cmd = Command::new("app")
            .dynamic_flags_with(dynamic_arity(&schema()))
            .positional(Arg::new("rest").variadic());
        let k = "--engine.deterministic";
        let table: [(Vec<String>, bool); 6] = [
            (strings(&[k]), true),
            (strings(&[&format!("{k}=false")]), false),
            (strings(&[k, "false"]), false),
            (strings(&[k, "true", "x"]), true),
            (strings(&[k, "x"]), true),
            (strings(&[k, "1", "x"]), true),
        ];
        for (argv, expected) in table {
            let m = cmd.parse(argv.clone()).unwrap();
            let via_cli = Config::builder()
                .merge_cli_with(&m, typed())
                .build()
                .get::<bool>("engine.deterministic")
                .unwrap();
            let via_args = Config::builder()
                .merge_args_with(argv.clone(), typed())
                .build()
                .get::<bool>("engine.deterministic")
                .unwrap();
            assert_eq!(via_cli, via_args, "{argv:?}");
            assert_eq!(via_cli, expected, "{argv:?}");
        }
    }

    #[test]
    fn later_dynamic_flags_win_and_override_lower_layers() {
        let m = parse(&["--engine.threads=2", "serve", "--engine.threads", "6"]);
        let cfg = Config::builder()
            .merge_value(schema())
            .merge_value(obj(&[("engine", obj(&[("threads", Value::UInt(3))]))]))
            .merge_cli_with(&m, typed())
            .build();
        assert_eq!(cfg.get::<u32>("engine.threads").unwrap(), 6);
    }

    #[test]
    fn help_is_unaffected_by_dynamic_flags() {
        let err = app()
            .parse(strings(&["--engine.threads", "4", "--help"]))
            .unwrap_err();
        assert!(matches!(err, CliError::HelpRequested(_)));
        let err = app()
            .parse(strings(&["serve", "--a.n=1", "--help"]))
            .unwrap_err();
        assert!(matches!(err, CliError::HelpRequested(_)));
    }

    #[test]
    fn merge_cli_without_dynamic_flags_is_unchanged() {
        let cmd = Command::new("svc")
            .flag(Arg::new("port").long("port").default("8080"))
            .flag(Arg::new("db.host").long("db-host"));
        let m = cmd.parse(strings(&["--db-host", "h"])).unwrap();
        assert!(m.dynamic().is_empty());
        let cfg = Config::builder().merge_cli(&m).build();
        assert_eq!(
            cfg.value(),
            &obj(&[("db", obj(&[("host", Value::Str("h".into()))]))])
        );
        let with = Config::builder().merge_cli_with(&m, typed()).build();
        assert_eq!(cfg.value(), with.value());
    }

    #[test]
    fn a_very_deep_dynamic_flag_does_not_overflow_the_stack() {
        on_small_stack(|| {
            let deep = format!("--engine.{}=1", vec!["k"; 60_000].join("."));
            let m = app()
                .parse([deep, "--engine.threads=2".to_string()])
                .unwrap();
            for cfg in [
                Config::builder().merge_cli(&m).build(),
                Config::builder().merge_cli_with(&m, typed()).build(),
            ] {
                assert!(!cfg.contains("engine.k"));
                assert_eq!(cfg.get::<u32>("engine.threads").unwrap(), 2);
            }
        });
    }

    #[test]
    fn cli_with_layer_matches_the_builder_call() {
        let m = parse(&["--engine.deterministic", "false", "--config=c"]);
        let chained = Config::builder().merge_cli_with(&m, typed()).build();
        let listed = Config::from_layers([Layer::CliWith {
            matches: m.clone(),
            options: typed(),
        }])
        .unwrap();
        assert_eq!(listed.value(), chained.value());
        assert_eq!(
            Config::from_layers([Layer::Cli(m.clone())])
                .unwrap()
                .value(),
            Config::builder().merge_cli(&m).build().value()
        );
        let shown = format!(
            "{:?}",
            Layer::CliWith {
                matches: m,
                options: typed()
            }
        );
        assert!(
            shown.contains("CliWith") && !shown.contains("engine") && !shown.contains("threads"),
            "{shown}"
        );
    }
}

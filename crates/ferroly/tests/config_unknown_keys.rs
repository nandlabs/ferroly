#![cfg(feature = "config")]
//! Unknown-key detection per layer (#93): schema from `Default::default()`,
//! per-layer strip-and-report, spelling per source, suggestions, lenient mode.
//! The environment layer is covered in `config_unknown_env.rs`, alone in its
//! binary because it sets process variables.

use std::path::PathBuf;

use ferroly::codec::{Decode, Encode, Value};
use ferroly::config::{Config, ConfigError, Source, UnknownKey};

#[derive(Encode, Decode, Debug, PartialEq)]
struct Engine {
    threads: u32,
}

#[derive(Encode, Decode, Debug, PartialEq)]
struct Server {
    port: u16,
    host: String,
}

#[derive(Encode, Decode, Debug, PartialEq)]
struct App {
    engine: Engine,
    server: Server,
    tls: Option<String>,
}

impl Default for App {
    fn default() -> Self {
        App {
            engine: Engine { threads: 4 },
            server: Server {
                port: 8080,
                host: "127.0.0.1".into(),
            },
            tls: None,
        }
    }
}

fn schema() -> Value {
    App::default().encode()
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ferroly-uk-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn obj(pairs: &[(&str, Value)]) -> Value {
    Value::Object(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    )
}

fn unknown(err: ConfigError) -> Vec<UnknownKey> {
    match err {
        ConfigError::UnknownKeys(keys) => keys,
        other => panic!("expected UnknownKeys, got {other:?}"),
    }
}

#[test]
fn file_and_argument_keys_are_reported_with_their_source_and_spelling() {
    let dir = temp_dir("file-args");
    let file = dir.join("app.yaml");
    std::fs::write(&file, "serve:\n  port: 9000\nengine:\n  threads: 2\n").unwrap();
    let shown = file.display().to_string();

    let err = Config::builder()
        .schema(schema())
        .merge_value(App::default().encode())
        .merge_file(&file)
        .unwrap()
        .merge_args(["--engine.thread=8".to_string()])
        .build_checked()
        .unwrap_err();
    let message = err.to_string();
    let keys = unknown(err);

    assert_eq!(keys.len(), 2, "{keys:?}");
    assert_eq!(keys[0].path, "serve.port");
    assert_eq!(keys[0].spelled, "serve.port");
    assert_eq!(keys[0].source, Source::File(shown.clone()));
    assert_eq!(keys[0].suggestion.as_deref(), Some("server.port"));

    assert_eq!(keys[1].path, "engine.thread");
    assert_eq!(keys[1].spelled, "--engine.thread");
    assert_eq!(keys[1].source, Source::Args);
    assert_eq!(keys[1].suggestion.as_deref(), Some("--engine.threads"));

    // One line, every key.
    assert_eq!(
        message,
        format!(
            "unknown config keys: `serve.port` in {shown} (did you mean `server.port`?), \
             `--engine.thread` (did you mean `--engine.threads`?)"
        )
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn suggestions_use_the_nearest_section_or_key_within_budget() {
    let layer = obj(&[
        ("engine", obj(&[("thread", Value::Int(1))])),
        ("serve", obj(&[("port", Value::Int(1))])),
        ("zzz", Value::Int(1)),
    ]);
    let keys = unknown(
        Config::builder()
            .schema(schema())
            .merge_value(layer)
            .build_checked()
            .unwrap_err(),
    );
    let got: Vec<(&str, Option<&str>)> = keys
        .iter()
        .map(|k| (k.path.as_str(), k.suggestion.as_deref()))
        .collect();
    assert_eq!(
        got,
        [
            ("engine.thread", Some("engine.threads")),
            ("serve.port", Some("server.port")),
            ("zzz", None),
        ]
    );
    // A misspelled section alone is matched against sections.
    let keys = unknown(
        Config::builder()
            .schema(schema())
            .merge_value(obj(&[("engin", obj(&[]))]))
            .build_checked()
            .unwrap_err(),
    );
    assert_eq!(keys[0].path, "engin");
    assert_eq!(keys[0].suggestion.as_deref(), Some("engine"));
}

#[test]
fn lenient_mode_strips_unknown_keys_before_merging_and_lists_them() {
    let layer = obj(&[(
        "engine",
        obj(&[("thread", Value::Int(1)), ("threads", Value::Int(6))]),
    )]);
    let cfg = Config::builder()
        .schema(schema())
        .lenient()
        .merge_value(App::default().encode())
        .merge_value(obj(&[("engine", obj(&[("thread", Value::Int(1))]))]))
        .merge_value(layer)
        .build_checked()
        .unwrap();

    assert_eq!(cfg.get::<u32>("engine.threads").unwrap(), 6);
    assert!(!cfg.contains("engine.thread"));
    assert_eq!(cfg.source("engine.thread"), None);
    let engine = cfg.value().get("engine").unwrap().as_object().unwrap();
    assert_eq!(engine.len(), 1, "{engine:?}");

    let ignored: Vec<&str> = cfg.ignored_keys().iter().map(|k| k.path.as_str()).collect();
    assert_eq!(ignored, ["engine.thread", "engine.thread"]);
    assert_eq!(cfg.ignored_keys()[0].source, Source::Value);

    // An unknown key on its own cannot change the default either.
    let cfg = Config::builder()
        .schema(schema())
        .lenient()
        .merge_value(App::default().encode())
        .merge_value(obj(&[("engine", obj(&[("thread", Value::Int(1))]))]))
        .build_checked()
        .unwrap();
    let app: App = cfg.extract().unwrap();
    assert_eq!(app, App::default());

    // `build` never fails; with a schema it strips the same way.
    let cfg = Config::builder()
        .schema(schema())
        .merge_value(obj(&[("engine", obj(&[("thread", Value::Int(1))]))]))
        .build();
    assert!(!cfg.contains("engine.thread"));
    assert_eq!(cfg.ignored_keys().len(), 1);
}

#[test]
fn an_empty_schema_object_accepts_any_keys_below_it() {
    let schema = obj(&[
        ("labels", obj(&[])),
        ("port", Value::Int(1)),
        ("tags", Value::Array(vec![])),
        ("tls", Value::Null),
    ]);
    let layer = obj(&[
        (
            "labels",
            obj(&[
                ("team", Value::Str("core".into())),
                ("a", obj(&[("b", Value::Int(1))])),
            ]),
        ),
        ("tags", Value::Array(vec![obj(&[("x", Value::Int(1))])])),
        ("tls", obj(&[("cert", Value::Str("c.pem".into()))])),
    ]);
    let cfg = Config::builder()
        .schema(schema)
        .merge_value(layer)
        .merge_args(["--labels.anything=1".to_string()])
        .build_checked()
        .unwrap();
    assert_eq!(cfg.get::<String>("labels.team").unwrap(), "core");
    assert_eq!(cfg.get::<String>("labels.anything").unwrap(), "1");
    assert_eq!(cfg.get::<i64>("labels.a.b").unwrap(), 1);
    assert_eq!(cfg.get::<String>("tls.cert").unwrap(), "c.pem");
    assert!(cfg.ignored_keys().is_empty());

    // An empty schema root is free-form throughout.
    let cfg = Config::builder()
        .schema(obj(&[]))
        .merge_value(obj(&[("anything", Value::Int(1))]))
        .build_checked()
        .unwrap();
    assert!(cfg.contains("anything"));
}

#[test]
fn without_a_schema_build_is_unchanged() {
    let layer = obj(&[
        ("engine", obj(&[("thread", Value::Int(1))])),
        ("zzz", Value::Int(2)),
    ]);
    let plain = Config::builder().merge_value(layer.clone()).build();
    assert_eq!(plain.value(), &layer);
    assert!(plain.ignored_keys().is_empty());

    let checked = Config::builder()
        .merge_value(layer.clone())
        .merge_args(["--yyy=1".to_string()])
        .build_checked()
        .unwrap();
    assert!(checked.contains("engine.thread") && checked.contains("zzz"));
    assert!(checked.contains("yyy"));
    assert!(checked.ignored_keys().is_empty());

    // Layers merged before `schema` are not checked.
    let cfg = Config::builder()
        .merge_value(layer)
        .schema(schema())
        .build_checked()
        .unwrap();
    assert!(cfg.contains("zzz"));
}

#[cfg(feature = "cli")]
#[test]
fn declared_cli_arguments_outside_the_schema_are_not_configuration() {
    use ferroly::cli::{Arg, Command};
    use ferroly::config::ArgsOptions;

    // CFG-2: a real command line with interface-only arguments.
    let cmd = Command::new("app")
        .flag(Arg::new("config").long("config"))
        .flag(Arg::new("verbose").short('v').count())
        .flag(Arg::new("log-level").long("log-level").default("info"))
        .flag(Arg::new("server.port").long("port").default("8080"))
        .flag(Arg::new("engine.thread").long("thr"))
        .dynamic_flags(|name| name.starts_with("engine."));
    let m = cmd
        .parse(["--config", "x.yaml", "-v", "--port", "9000"].map(String::from))
        .unwrap();
    let cfg = Config::builder()
        .schema(schema())
        .merge_value(App::default().encode())
        .merge_cli_defaults(&m)
        .merge_cli(&m)
        .build_checked()
        .unwrap();
    assert_eq!(cfg.get::<u16>("server.port").unwrap(), 9000);
    assert!(!cfg.contains("config") && !cfg.contains("verbose"));
    let app: App = cfg.extract().unwrap();
    assert_eq!(app.server.port, 9000);

    // A declared argument inside a schema section is checked and reported by
    // its name (its flag is `--thr`); a dynamic flag by its flag, both from
    // `Source::Cli`. `merge_cli_with` typing changes nothing here.
    let m = cmd
        .parse(["--thr", "8", "--engine.threadz=2", "--engine.threads=3"].map(String::from))
        .unwrap();
    for typed in [false, true] {
        let options = if typed {
            ArgsOptions::default().types(&schema())
        } else {
            ArgsOptions::default()
        };
        let err = Config::builder()
            .schema(schema())
            .merge_cli_with(&m, options)
            .build_checked()
            .unwrap_err();
        let keys = unknown(err);
        let got: Vec<(&str, &str, Option<&str>)> = keys
            .iter()
            .map(|k| (k.path.as_str(), k.spelled.as_str(), k.suggestion.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                ("engine.thread", "engine.thread", Some("engine.threads")),
                (
                    "engine.threadz",
                    "--engine.threadz",
                    Some("--engine.threads")
                ),
            ]
        );
        assert!(keys.iter().all(|k| k.source == Source::Cli));
    }
}

#[test]
fn raw_arguments_typed_from_a_schema_are_checked_too() {
    use ferroly::config::ArgsOptions;
    let err = Config::builder()
        .schema(schema())
        .merge_args_with(
            ["--engine.thread", "8", "--server.port", "1"],
            ArgsOptions::default().types(&schema()).value_follows(),
        )
        .build_checked()
        .unwrap_err();
    let keys = unknown(err);
    assert_eq!(keys.len(), 1, "{keys:?}");
    assert_eq!(keys[0].spelled, "--engine.thread");
    assert_eq!(keys[0].source, Source::Args);
}

#[test]
fn keys_below_a_scalar_leaf_are_unknown() {
    // CFG-5: `port.x` cannot be valid where the schema has `port: 8080`, and
    // must not replace it.
    let cfg = Config::builder()
        .schema(schema())
        .lenient()
        .merge_value(App::default().encode())
        .merge_args(["--server.port.x=1", "--engine.threads.y=2"])
        .merge_value(obj(&[("server", obj(&[("host", obj(&[("a", s("b"))]))]))]))
        .build_checked()
        .unwrap();
    let ignored: Vec<(&str, &str)> = cfg
        .ignored_keys()
        .iter()
        .map(|k| (k.path.as_str(), k.spelled.as_str()))
        .collect();
    assert_eq!(
        ignored,
        [
            ("server.port.x", "--server.port.x"),
            ("engine.threads.y", "--engine.threads.y"),
            ("server.host.a", "server.host.a"),
        ]
    );
    let app: App = cfg.extract().unwrap();
    assert_eq!(app, App::default());

    // A list leaf takes no keys either; `null` (an `Option`) and `{}` do.
    let leaves = obj(&[
        ("tags", Value::Array(vec![])),
        ("tls", Value::Null),
        ("labels", obj(&[])),
    ]);
    let err = Config::builder()
        .schema(leaves)
        .merge_args(["--tags.x=1", "--tls.cert=c", "--labels.a.b=1"])
        .build_checked()
        .unwrap_err();
    let paths: Vec<String> = unknown(err).into_iter().map(|k| k.path).collect();
    assert_eq!(paths, ["tags.x"]);

    // A scalar where the schema has a section is not an unknown key: it is
    // merged and left for decoding to accept or refuse.
    let cfg = Config::builder()
        .schema(schema())
        .merge_value(App::default().encode())
        .merge_args(["--engine=3"])
        .build_checked()
        .unwrap();
    assert!(cfg.extract::<App>().is_err());
}

#[test]
fn a_literal_dotted_key_is_told_to_nest() {
    // CFG-10: the suggestion would read exactly like the key.
    let err = Config::builder()
        .schema(schema())
        .merge_value(obj(&[("engine.threads", Value::Int(9))]))
        .build_checked()
        .unwrap_err();
    let message = err.to_string();
    let keys = unknown(err);
    assert_eq!(keys[0].path, "engine.threads");
    assert!(keys[0].dotted);
    assert_eq!(keys[0].suggestion, None);
    assert_eq!(
        message,
        "unknown config keys: `engine.threads` in a value layer \
         (one key name with dots in it: write it as nested keys)"
    );
}

#[test]
fn suggestions_are_bounded_in_cost_and_the_message_in_size() {
    // CFG-3: a 500-path schema against 10 000 unknown leaves, a 1 MB key and
    // 3 000 unknown arguments.
    let sections: Vec<(String, Value)> = (0..50)
        .map(|i| {
            let keys = (0..9)
                .map(|j| (format!("setting_{j}"), Value::Int(1)))
                .collect();
            (format!("section_{i}"), Value::Object(keys))
        })
        .collect();
    let schema = Value::Object(sections);
    let leaves: Vec<(String, Value)> = (0..10_000)
        .map(|i| (format!("unknown_key_{i}"), Value::Int(1)))
        .collect();
    let huge = "k".repeat(1 << 20);
    let args: Vec<String> = (0..3000)
        .map(|i| format!("--section_{}.settin_{i}=1", i % 50))
        .chain([format!("--{huge}=1")])
        .collect();

    let started = std::time::Instant::now();
    let err = Config::builder()
        .schema(schema)
        .merge_value(obj(&[("section_1", Value::Object(leaves))]))
        .merge_args(args)
        .build_checked()
        .unwrap_err();
    let elapsed = started.elapsed();
    let message = err.to_string();
    let keys = unknown(err);
    assert_eq!(keys.len(), 13_001);
    // Unoptimised test build on a shared machine: generous, but far below
    // the tens of seconds this took before.
    assert!(elapsed.as_secs_f64() < 3.0, "{elapsed:?}");
    // The first keys still get their suggestion.
    assert_eq!(
        keys[0].suggestion.as_deref(),
        None,
        "unknown_key_0 is not near anything"
    );
    let near = keys
        .iter()
        .find(|k| k.path == "section_0.settin_0")
        .unwrap();
    assert_eq!(near.suggestion.as_deref(), Some("--section_0.setting_0"));
    // The message lists 50 keys and counts the rest; the huge key is cut.
    assert!(
        message.ends_with(", and 12951 more"),
        "{}",
        &message[message.len() - 40..]
    );
    assert!(message.len() < 10_000, "{}", message.len());
    let last = keys.last().unwrap();
    assert!(last.to_string().len() < 300);
    assert_eq!(last.suggestion, None);
}

fn s(text: &str) -> Value {
    Value::Str(text.to_string())
}

#[test]
fn the_layer_api_is_checked_the_same_way() {
    use ferroly::config::Layer;
    let err = Config::builder()
        .schema(schema())
        .layers([
            Layer::value(App::default().encode()),
            Layer::args(["--engine.thread=8"]),
        ])
        .unwrap()
        .build_checked()
        .unwrap_err();
    let keys = unknown(err);
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].spelled, "--engine.thread");
}

#[test]
fn secret_directory_keys_are_spelled_as_their_file() {
    let dir = temp_dir("secret-dir");
    std::fs::write(dir.join("server.prot"), "1\n").unwrap();
    std::fs::write(dir.join("server.port"), "2\n").unwrap();
    let keys = unknown(
        Config::builder()
            .schema(schema())
            .merge_secret_dir(&dir)
            .unwrap()
            .build_checked()
            .unwrap_err(),
    );
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].path, "server.prot");
    assert_eq!(
        keys[0].spelled,
        dir.join("server.prot").display().to_string()
    );
    assert_eq!(
        keys[0].suggestion,
        Some(dir.join("server.port").display().to_string())
    );
    // The error message carries no value.
    let shown = format!("{keys:?}");
    assert!(!shown.contains("\"1\""), "{shown}");
    std::fs::remove_dir_all(&dir).ok();
}

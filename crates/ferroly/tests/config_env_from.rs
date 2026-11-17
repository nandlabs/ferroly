#![cfg(feature = "config")]
//! The environment layer with injected variables (#110):
//! `merge_env_from` / `merge_env_from_with`. No test here reads or changes
//! the process environment, so they all run in parallel.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;

use ferroly::codec::{json, Decode, Encode, Value};
use ferroly::config::{Config, ConfigError, EnvOptions, Source};

/// A fresh, empty directory unique to one test.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ferroly-env-from-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sources(cfg: &Config) -> Vec<(String, Source)> {
    cfg.sources()
        .map(|(k, s)| (k.to_string(), s.clone()))
        .collect()
}

fn env(vars: &[(&str, &str)]) -> Config {
    Config::builder()
        .merge_env_from("APP_", vars.iter().copied())
        .build()
}

#[test]
fn env_from_maps_prefix_and_nesting() {
    let cfg = env(&[
        ("APP_PORT", "8080"),
        ("APP_DB__HOST", "h"),
        ("APP_DB__POOL__MAX", "10"),
        ("OTHER", "x"),
    ]);
    assert_eq!(cfg.get::<String>("port").unwrap(), "8080");
    assert_eq!(cfg.get::<String>("db.host").unwrap(), "h");
    assert_eq!(cfg.get::<String>("db.pool.max").unwrap(), "10");
    assert!(!cfg.contains("other"));
    assert_eq!(cfg.sources().count(), 3);
    assert_eq!(
        cfg.source("db.host"),
        Some(&Source::Env("APP_DB__HOST".into()))
    );
}

#[test]
fn env_from_ignores_process_environment() {
    assert!(std::env::var_os("PATH").is_some());
    let cfg = Config::builder()
        .merge_env_from("PATH", Vec::<(String, String)>::new())
        .build();
    assert_eq!(cfg.value(), &Value::Object(Vec::new()));
    assert_eq!(cfg.sources().count(), 0);

    let cfg = Config::builder().merge_env_from("", [("A", "1")]).build();
    assert_eq!(cfg.value(), &json::from_str(r#"{"a": "1"}"#).unwrap());
}

#[test]
fn env_from_accepts_owned_borrowed_and_os_strings() {
    let pairs = [("APP_PORT", "1"), ("APP_DB__HOST", "h")];
    let borrowed = Config::builder().merge_env_from("APP_", pairs).build();

    let owned: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let map: HashMap<String, String> = owned.iter().cloned().collect();
    let os: Vec<(OsString, OsString)> = pairs
        .iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect();

    for cfg in [
        Config::builder().merge_env_from("APP_", owned).build(),
        Config::builder().merge_env_from("APP_", map).build(),
        Config::builder().merge_env_from("APP_", os).build(),
    ] {
        assert_eq!(cfg.value(), borrowed.value());
        assert_eq!(sources(&cfg), sources(&borrowed));
    }
}

#[test]
fn env_from_order_is_independent_of_input_order() {
    let mut vars = vec![
        ("APP_Z", "1"),
        ("APP_A__B", "2"),
        ("APP_M", "3"),
        ("APP_A__C", "4"),
    ];
    let forward = env(&vars);
    vars.reverse();
    let backward = env(&vars);
    assert_eq!(forward.value(), backward.value());
    assert_eq!(sources(&forward), sources(&backward));

    // Scalar vs nested: the nested form wins whichever comes first.
    for vars in [
        [("APP_DB", "x"), ("APP_DB__HOST", "h")],
        [("APP_DB__HOST", "h"), ("APP_DB", "x")],
    ] {
        assert_eq!(env(&vars).get::<String>("db.host").unwrap(), "h");
    }
}

#[test]
fn env_from_duplicate_name_last_wins() {
    let cfg = env(&[("APP_PORT", "1"), ("APP_PORT", "2")]);
    assert_eq!(cfg.get::<String>("port").unwrap(), "2");
    let cfg = env(&[("APP_PORT", "2"), ("APP_PORT", "1")]);
    assert_eq!(cfg.get::<String>("port").unwrap(), "1");
}

#[test]
fn env_from_skips_bad_segments_and_depth() {
    let at_limit = format!("APP_{}", vec!["K"; 128].join("__"));
    let too_deep = format!("APP_X__{}", vec!["K"; 128].join("__"));
    let cfg = env(&[
        ("APP_A____B", "bad"),
        ("APP___LEAD", "bad"),
        ("APP_TRAIL__", "bad"),
        ("APP_A__B", "good"),
        (at_limit.as_str(), "1"),
        (too_deep.as_str(), "1"),
    ]);
    assert_eq!(cfg.get::<String>("a.b").unwrap(), "good");
    assert_eq!(cfg.get::<String>(&vec!["k"; 128].join(".")).unwrap(), "1");
    assert!(!cfg.contains("x") && !cfg.contains("lead") && !cfg.contains("trail"));
    assert_eq!(cfg.sources().count(), 2);
}

#[cfg(unix)]
#[test]
fn env_from_skips_non_utf8() {
    use std::os::unix::ffi::OsStringExt;

    let bad = |prefix: &[u8]| {
        let mut bytes = prefix.to_vec();
        bytes.extend_from_slice(&[0xff, 0xfe]);
        OsString::from_vec(bytes)
    };
    let vars = vec![
        (bad(b"APP_NAME_"), OsString::from("v")),
        (OsString::from("APP_VALUE"), bad(b"")),
        (OsString::from("APP_PORT"), OsString::from("1")),
    ];
    let cfg = Config::builder().merge_env_from("APP_", vars).build();
    assert_eq!(cfg.get::<String>("port").unwrap(), "1");
    assert_eq!(cfg.sources().count(), 1);

    // A later non-UTF-8 value replaces the earlier one, removing the variable.
    let vars = vec![
        (OsString::from("APP_PORT"), OsString::from("1")),
        (OsString::from("APP_PORT"), bad(b"")),
    ];
    let cfg = Config::builder().merge_env_from("APP_", vars).build();
    assert_eq!(cfg.sources().count(), 0);
}

/// A value that is not valid Unicode: invalid UTF-8 bytes on Unix, an
/// unpaired surrogate on Windows.
#[cfg(any(unix, windows))]
fn not_unicode() -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(vec![0xff, 0xfe])
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        OsString::from_wide(&[0xd800])
    }
}

#[cfg(any(unix, windows))]
#[test]
fn env_from_repeated_name_with_a_last_non_utf8_value_is_skipped() {
    // As in the process environment, the last value is the only one; it is
    // not UTF-8, so the variable is skipped and the earlier "1" is not kept.
    let vars = || {
        vec![
            (OsString::from("APP_A"), OsString::from("1")),
            (OsString::from("APP_A"), not_unicode()),
        ]
    };
    let cfg = Config::builder().merge_env_from("APP_", vars()).build();
    assert_eq!(cfg.value(), &Value::Object(Vec::new()));
    assert_eq!(cfg.sources().count(), 0);
    let cfg = Config::builder()
        .merge_env_from_with("APP_", vars(), &EnvOptions::new())
        .unwrap()
        .build();
    assert_eq!(cfg.value(), &Value::Object(Vec::new()));
    assert_eq!(cfg.sources().count(), 0);

    // An earlier layer's value for the key is left in place.
    let cfg = Config::builder()
        .merge_value(json::from_str(r#"{"a":"base"}"#).unwrap())
        .merge_env_from("APP_", vars())
        .build();
    assert_eq!(cfg.get::<String>("a").unwrap(), "base");
    assert_eq!(cfg.source("a"), Some(&Source::Value));

    // The other way round, the last value is valid and wins.
    let vars = vec![
        (OsString::from("APP_A"), not_unicode()),
        (OsString::from("APP_A"), OsString::from("1")),
    ];
    let cfg = Config::builder().merge_env_from("APP_", vars).build();
    assert_eq!(cfg.get::<String>("a").unwrap(), "1");
    assert_eq!(cfg.source("a"), Some(&Source::Env("APP_A".into())));
}

#[test]
fn env_from_with_secret_file_wins() {
    let dir = temp_dir("secret");
    let secret = dir.join("pw");
    std::fs::write(&secret, "s3cret\n").unwrap();
    let path = secret.display().to_string();

    for vars in [
        [
            ("APP_DB__PASSWORD", "from-env"),
            ("APP_DB__PASSWORD_FILE", path.as_str()),
        ],
        [
            ("APP_DB__PASSWORD_FILE", path.as_str()),
            ("APP_DB__PASSWORD", "from-env"),
        ],
    ] {
        let cfg = Config::builder()
            .merge_env_from_with("APP_", vars, &EnvOptions::new().secret_files())
            .unwrap()
            .build();
        assert_eq!(cfg.get::<String>("db.password").unwrap(), "s3cret");
        assert_eq!(
            cfg.source("db.password"),
            Some(&Source::SecretFile {
                var: "APP_DB__PASSWORD_FILE".into(),
                path: path.clone(),
            })
        );
        assert!(!cfg.contains("db.password_file"));
        let dump = cfg.to_redacted_string();
        assert!(!dump.contains("s3cret"), "{dump}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn env_from_without_secret_files_keeps_file_var_literal() {
    let dir = temp_dir("literal");
    let path = dir.join("pw").display().to_string();
    // The file does not exist: reading it would fail.
    let cfg = Config::builder()
        .merge_env_from_with(
            "APP_",
            [("APP_DB__PASSWORD_FILE", path.as_str())],
            &EnvOptions::new(),
        )
        .unwrap()
        .build();
    assert_eq!(cfg.get::<String>("db.password_file").unwrap(), path);
    assert!(!cfg.contains("db.password"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn env_from_with_unreadable_secret_file_errors() {
    let dir = temp_dir("unreadable");
    let path = dir.join("missing").display().to_string();
    match Config::builder().merge_env_from_with(
        "APP_",
        [("APP_DB__PASSWORD_FILE", path.as_str())],
        &EnvOptions::new().secret_files(),
    ) {
        Err(ConfigError::Io(msg)) => {
            assert!(msg.contains("APP_DB__PASSWORD_FILE"), "{msg}");
            assert!(msg.contains("missing"), "{msg}");
        }
        other => panic!("expected Io, got {other:?}"),
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[derive(Encode, Decode, Default)]
struct Engine {
    threads: u32,
}

#[derive(Encode, Decode, Default)]
struct Server {
    port: u16,
}

#[derive(Encode, Decode, Default)]
struct App {
    engine: Engine,
    server: Server,
    port: u16,
}

#[test]
fn env_from_schema_screen() {
    let vars = [
        ("APP_ENGINE__THREAD", "8"),
        ("APP_ENGINE__THREADS", "2"),
        ("APP_LOG", "debug"),
    ];
    let err = Config::builder()
        .schema(App::default().encode())
        .merge_env_from("APP_", vars)
        .build_checked()
        .unwrap_err();
    let ConfigError::UnknownKeys(keys) = err else {
        panic!("expected UnknownKeys, got {err:?}");
    };
    assert_eq!(keys.len(), 1, "{keys:?}");
    assert_eq!(keys[0].path, "engine.thread");
    assert_eq!(keys[0].spelled, "APP_ENGINE__THREAD");
    assert_eq!(keys[0].suggestion.as_deref(), Some("APP_ENGINE__THREADS"));
    assert_eq!(keys[0].source, Source::Env("APP_ENGINE__THREAD".into()));

    // Lenient: the valid variable applies; `APP_LOG` is neither merged nor
    // reported.
    let cfg = Config::builder()
        .schema(App::default().encode())
        .lenient()
        .merge_env_from("APP_", vars)
        .build_checked()
        .unwrap();
    assert_eq!(cfg.get::<u32>("engine.threads").unwrap(), 2);
    assert!(!cfg.contains("log"));
    let ignored: Vec<&str> = cfg
        .ignored_keys()
        .iter()
        .map(|k| k.spelled.as_str())
        .collect();
    assert_eq!(ignored, ["APP_ENGINE__THREAD"]);

    // The file of an unknown `*_FILE` variable is never read.
    let builder = Config::builder()
        .schema(App::default().encode())
        .merge_env_from_with(
            "APP_",
            [("APP_ENGINE__THREAD_FILE", "/nonexistent-ferroly-env-from")],
            &EnvOptions::new().secret_files(),
        );
    assert!(builder.is_ok(), "{builder:?}");
}

#[test]
fn env_from_with_options() {
    // list_keys: only the named key is split.
    let cfg = Config::builder()
        .merge_env_from_with(
            "APP_",
            [
                ("APP_ALLOWED_HOSTS", "a.example, b.example ,,c.example"),
                ("APP_MOTTO", "ready, steady, go"),
            ],
            &EnvOptions::new().list_keys(',', ["allowed_hosts"]),
        )
        .unwrap()
        .build();
    assert_eq!(
        cfg.get::<Vec<String>>("allowed_hosts").unwrap(),
        ["a.example", "b.example", "c.example"]
    );
    assert_eq!(cfg.get::<String>("motto").unwrap(), "ready, steady, go");

    // split_all: whatever contains the separator.
    let cfg = Config::builder()
        .merge_env_from_with(
            "APP_",
            [("APP_MOTTO", "ready, steady, go"), ("APP_PORT", "80")],
            &EnvOptions::new().split_all(','),
        )
        .unwrap()
        .build();
    assert_eq!(
        cfg.get::<Vec<String>>("motto").unwrap(),
        ["ready", "steady", "go"]
    );
    assert_eq!(cfg.get::<String>("port").unwrap(), "80");

    // preserve_case keeps the variable's own spelling.
    let cfg = Config::builder()
        .merge_env_from_with(
            "APP_",
            [("APP_dbPool__maxConnections", "7")],
            &EnvOptions::new().preserve_case(),
        )
        .unwrap()
        .build();
    assert_eq!(cfg.get::<String>("dbPool.maxConnections").unwrap(), "7");

    // case_insensitive adopts the spelling of an earlier layer.
    let defaults = json::from_str(r#"{"maxConnections": 5}"#).unwrap();
    let cfg = Config::builder()
        .merge_value(defaults)
        .merge_env_from_with(
            "APP_",
            [("APP_MAX_CONNECTIONS", "50")],
            &EnvOptions::new().case_insensitive(),
        )
        .unwrap()
        .build();
    assert_eq!(cfg.get::<u32>("maxConnections").unwrap(), 50);
    assert!(!cfg.contains("max_connections"));
    assert_eq!(
        cfg.source("maxConnections"),
        Some(&Source::Env("APP_MAX_CONNECTIONS".into()))
    );
}

#[test]
fn env_from_layers_over_earlier_layers() {
    let cfg = Config::builder()
        .merge_value(json::from_str(r#"{"port": 1}"#).unwrap())
        .merge_env_from("APP_", [("APP_PORT", "2")])
        .build();
    assert_eq!(cfg.get::<String>("port").unwrap(), "2");
    assert_eq!(cfg.source("port"), Some(&Source::Env("APP_PORT".into())));
}

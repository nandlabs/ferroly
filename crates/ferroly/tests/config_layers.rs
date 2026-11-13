#![cfg(feature = "config")]
//! Regression and behaviour tests for the layered configuration loader:
//! secret hygiene, file formats, optional files and profiles, argument and
//! environment rules, interpolation, validation, provenance and sharing.

use std::path::PathBuf;

use ferroly::codec::{json, Decode, Value};
use ferroly::config::{Config, ConfigError, EnvOptions, SharedConfig, Source};

/// A fresh, empty directory unique to one test.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ferroly-cfg-{}-{name}", std::process::id()));
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

fn s(text: &str) -> Value {
    Value::Str(text.to_string())
}

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

// ---- C11: secret hygiene ---------------------------------------------------

#[test]
fn builder_debug_does_not_print_values() {
    let builder = Config::builder().merge_value(obj(&[
        ("db_password", s("s3cr3t-builder")),
        ("nested", obj(&[("api_token", s("t0ken-builder"))])),
    ]));
    let shown = format!("{builder:?}");
    assert!(shown.contains("ConfigBuilder"), "{shown}");
    assert!(shown.contains("db_password"), "{shown}");
    assert!(shown.contains("<redacted>"), "{shown}");
    assert!(!shown.contains("s3cr3t-builder"), "{shown}");
    assert!(!shown.contains("t0ken-builder"), "{shown}");

    // Pretty debug must not leak either.
    let pretty = format!("{builder:#?}");
    assert!(!pretty.contains("s3cr3t-builder") && !pretty.contains("t0ken-builder"));
}

#[test]
fn yaml_parse_error_does_not_echo_the_offending_line() {
    let dir = temp_dir("yaml-secret");
    let path = dir.join("app.yaml");
    // The second line is not `key: value`, which the YAML parser reports by
    // quoting the line back.
    std::fs::write(&path, "port: 8080\npassword hunter2-s3cr3t\n").unwrap();

    let err = Config::from_file(&path).unwrap_err();
    let text = format!("{err} / {err:?}");
    assert!(!text.contains("hunter2"), "secret leaked: {text}");
    match &err {
        ConfigError::Parse { path: p, .. } => assert!(p.ends_with("app.yaml")),
        other => panic!("expected Parse, got {other:?}"),
    }
    // The file is still named, so the operator knows where to look.
    assert!(err.to_string().contains("app.yaml"));
    // What is left of the parser's message still says what was wrong.
    if let ConfigError::Parse { message, .. } = &err {
        assert!(
            !message.is_empty() && !message.contains("hunter2"),
            "{message}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn malformed_json_and_toml_errors_do_not_leak_content() {
    let dir = temp_dir("other-secret");
    let cases = [
        ("bad.json", "{\"password\": \"json-s3cr3t\" oops}"),
        (
            "bad.toml",
            "password = \"toml-s3cr3t\"\nthis is not toml toml-s3cr3t\n",
        ),
        ("bad.xml", "<a><password>xml-s3cr3t</password>"),
    ];
    for (name, content) in cases {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        let err = Config::from_file(&path).unwrap_err();
        let text = format!("{err} / {err:?}");
        assert!(matches!(err, ConfigError::Parse { .. }), "{name}: {text}");
        assert!(!text.contains("s3cr3t"), "{name} leaked: {text}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- C13: formats ----------------------------------------------------------

#[derive(Decode, Debug, PartialEq)]
struct Db {
    host: String,
    port: u16,
}

#[derive(Decode, Debug, PartialEq)]
struct App {
    name: String,
    db: Db,
}

#[test]
fn toml_config_files_load() {
    let dir = temp_dir("toml");
    let path = dir.join("app.toml");
    std::fs::write(
        &path,
        "name = \"svc\"\n\n[db]\nhost = \"localhost\"\nport = 5432\n",
    )
    .unwrap();

    let app: App = Config::from_file(&path).unwrap().extract().unwrap();
    assert_eq!(
        app,
        App {
            name: "svc".into(),
            db: Db {
                host: "localhost".into(),
                port: 5432
            }
        }
    );

    // The extension check is case-insensitive.
    let upper = dir.join("APP.TOML");
    std::fs::write(&upper, "name = \"x\"\n").unwrap();
    assert!(Config::from_file(&upper).is_ok());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unsupported_format_error_lists_every_supported_extension() {
    let err = Config::from_file("settings.ini").unwrap_err();
    assert!(matches!(err, ConfigError::UnsupportedFormat(_)));
    let text = err.to_string();
    assert!(text.contains("settings.ini"), "{text}");
    for ext in [
        ".json",
        ".yaml",
        ".yml",
        ".toml",
        ".xml",
        ".properties",
        ".props",
        ".env",
    ] {
        assert!(text.contains(ext), "missing {ext} in: {text}");
    }
}

// ---- optional files and profiles -------------------------------------------

#[test]
fn optional_file_may_be_missing_but_not_malformed() {
    let dir = temp_dir("optional");

    let cfg = Config::builder()
        .merge_value(obj(&[("port", Value::Int(1))]))
        .merge_file_optional(dir.join("absent.json"))
        .unwrap()
        .build();
    assert_eq!(cfg.get::<u16>("port").unwrap(), 1);

    // The required variant still fails, and names the file.
    match Config::builder().merge_file(dir.join("absent.json")) {
        Err(ConfigError::Io(msg)) => assert!(msg.contains("absent.json"), "{msg}"),
        other => panic!("expected Io, got {other:?}"),
    }

    // Present files are merged…
    std::fs::write(dir.join("there.json"), r#"{"port": 2}"#).unwrap();
    let cfg = Config::builder()
        .merge_file_optional(dir.join("there.json"))
        .unwrap()
        .build();
    assert_eq!(cfg.get::<u16>("port").unwrap(), 2);

    // …and "optional" forgives absence only.
    std::fs::write(dir.join("broken.json"), "{ nope").unwrap();
    assert!(matches!(
        Config::builder().merge_file_optional(dir.join("broken.json")),
        Err(ConfigError::Parse { .. })
    ));
    assert!(matches!(
        Config::builder().merge_file_optional(dir.join("absent.ini")),
        Err(ConfigError::UnsupportedFormat(_))
    ));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn profile_overlay_is_merged_over_the_base() {
    let dir = temp_dir("profile");
    std::fs::write(
        dir.join("app.toml"),
        "name = \"svc\"\n[db]\nhost = \"localhost\"\nport = 5432\n",
    )
    .unwrap();
    std::fs::write(dir.join("app-prod.toml"), "[db]\nhost = \"prod-db\"\n").unwrap();

    // Base given without an extension: probed.
    let prod: App = Config::builder()
        .merge_profile(dir.join("app"), "prod")
        .unwrap()
        .build()
        .extract()
        .unwrap();
    assert_eq!(prod.db.host, "prod-db");
    assert_eq!(prod.db.port, 5432, "untouched base keys survive");

    // Base given with its extension.
    let cfg = Config::builder()
        .merge_profile(dir.join("app.toml"), "prod")
        .unwrap()
        .build();
    assert_eq!(cfg.get::<String>("db.host").unwrap(), "prod-db");
    match cfg.source("db.host") {
        Some(Source::File(p)) => assert!(p.ends_with("app-prod.toml"), "{p}"),
        other => panic!("unexpected source {other:?}"),
    }
    match cfg.source("db.port") {
        Some(Source::File(p)) => assert!(p.ends_with("app.toml"), "{p}"),
        other => panic!("unexpected source {other:?}"),
    }

    // A profile with no overlay file, and the empty profile, load the base.
    for profile in ["staging", ""] {
        let cfg = Config::builder()
            .merge_profile(dir.join("app"), profile)
            .unwrap()
            .build();
        assert_eq!(cfg.get::<String>("db.host").unwrap(), "localhost");
    }

    // The base itself is required.
    match Config::builder().merge_profile(dir.join("nothing"), "prod") {
        Err(ConfigError::Io(msg)) => assert!(msg.contains("no config file found"), "{msg}"),
        other => panic!("expected Io, got {other:?}"),
    }

    // A profile is a name, never a path.
    for bad in ["../etc", "a/b", "a\\b", "..", "pro d"] {
        assert!(
            matches!(
                Config::builder().merge_profile(dir.join("app"), bad),
                Err(ConfigError::InvalidProfile(_))
            ),
            "profile {bad:?} must be rejected"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- merge_args ------------------------------------------------------------

#[test]
fn merge_args_stops_at_the_double_dash_terminator() {
    let cfg = Config::builder()
        .merge_args(strings(&["--a=1", "--", "--b=2", "--c"]))
        .build();
    assert_eq!(cfg.get::<String>("a").unwrap(), "1");
    assert!(!cfg.contains("b"), "tokens after `--` are positionals");
    assert!(!cfg.contains("c"));
}

#[test]
fn merge_args_does_not_swallow_a_positional_after_an_unknown_flag() {
    // `--verbose input.txt`: nothing says `verbose` takes a value, so it is a
    // switch and `input.txt` is left alone.
    let cfg = Config::builder()
        .merge_args(strings(&["--verbose", "input.txt"]))
        .build();
    assert_eq!(cfg.get::<String>("verbose").unwrap(), "true");
    assert!(cfg.get::<bool>("verbose").unwrap());

    // A key known (from an earlier layer) to be boolean stays a switch too.
    let cfg = Config::builder()
        .merge_value(obj(&[("debug", Value::Bool(false)), ("quiet", s("false"))]))
        .merge_args(strings(&["--debug", "a.txt", "--quiet", "b.txt"]))
        .build();
    assert!(cfg.get::<bool>("debug").unwrap());
    assert!(cfg.get::<bool>("quiet").unwrap());
}

#[test]
fn merge_args_takes_a_separate_value_for_a_known_value_key() {
    let cfg = Config::builder()
        .merge_value(obj(&[
            ("db", obj(&[("port", Value::Int(1)), ("host", s("h"))])),
            ("offset", Value::Int(0)),
        ]))
        .merge_args(strings(&[
            "--db.port",
            "5432",
            "--db.host",
            "prod",
            "--offset",
            "-5",
            "positional",
        ]))
        .build();
    assert_eq!(cfg.get::<u16>("db.port").unwrap(), 5432);
    assert_eq!(cfg.get::<String>("db.host").unwrap(), "prod");
    assert_eq!(cfg.get::<i32>("offset").unwrap(), -5);
    assert_eq!(cfg.source("db.port"), Some(&Source::Args));

    // Another flag is never taken as a value.
    let cfg = Config::builder()
        .merge_value(obj(&[("name", s("x"))]))
        .merge_args(strings(&["--name", "--other=1"]))
        .build();
    assert_eq!(cfg.get::<String>("name").unwrap(), "true");
    assert_eq!(cfg.get::<String>("other").unwrap(), "1");

    // `=` is always unambiguous, known key or not.
    let cfg = Config::builder()
        .merge_args(strings(&["--fresh.key=v", "--empty="]))
        .build();
    assert_eq!(cfg.get::<String>("fresh.key").unwrap(), "v");
    assert_eq!(cfg.get::<String>("empty").unwrap(), "");
}

// ---- environment -----------------------------------------------------------

#[test]
fn env_keys_with_empty_segments_are_ignored() {
    std::env::set_var("CFGIT_SEG_A____B", "bad");
    std::env::set_var("CFGIT_SEG___LEAD", "bad");
    std::env::set_var("CFGIT_SEG_TRAIL__", "bad");
    std::env::set_var("CFGIT_SEG_A__B", "good");

    let cfg = Config::from_env("CFGIT_SEG_");
    assert_eq!(cfg.get::<String>("a.b").unwrap(), "good");

    fn has_empty_key(v: &Value) -> bool {
        match v {
            Value::Object(entries) => entries
                .iter()
                .any(|(k, child)| k.is_empty() || has_empty_key(child)),
            _ => false,
        }
    }
    assert!(!has_empty_key(cfg.value()), "{:?}", cfg.value());
    assert!(!cfg.contains("trail"));
    assert!(!cfg.contains("lead"));
    // Only the well-formed variable produced anything.
    assert_eq!(cfg.sources().count(), 1);

    for var in [
        "CFGIT_SEG_A____B",
        "CFGIT_SEG___LEAD",
        "CFGIT_SEG_TRAIL__",
        "CFGIT_SEG_A__B",
    ] {
        std::env::remove_var(var);
    }
}

#[test]
fn env_secret_files_are_read_when_enabled() {
    let dir = temp_dir("secret-file");
    let secret = dir.join("db_password");
    std::fs::write(&secret, "from-file-s3cr3t\n").unwrap();
    std::env::set_var("CFGIT_SF_DB__PASSWORD_FILE", &secret);
    std::env::set_var("CFGIT_SF_DB__PASSWORD", "from-env");
    std::env::set_var("CFGIT_SF_DB__USER", "app");

    // Off by default: the variable is an ordinary key.
    let plain = Config::from_env("CFGIT_SF_");
    assert_eq!(plain.get::<String>("db.password").unwrap(), "from-env");
    assert!(plain.contains("db.password_file"));

    let cfg = Config::builder()
        .merge_env_with("CFGIT_SF_", &EnvOptions::new().secret_files())
        .unwrap()
        .build();
    assert_eq!(
        cfg.get::<String>("db.password").unwrap(),
        "from-file-s3cr3t"
    );
    assert_eq!(cfg.get::<String>("db.user").unwrap(), "app");
    assert!(!cfg.contains("db.password_file"));
    assert!(matches!(
        cfg.source("db.password"),
        Some(Source::SecretFile { var, .. }) if var == "CFGIT_SF_DB__PASSWORD_FILE"
    ));

    // A secret file that cannot be read is an error that names the variable
    // and the path.
    std::env::set_var("CFGIT_SF_DB__PASSWORD_FILE", dir.join("missing"));
    match Config::builder().merge_env_with("CFGIT_SF_", &EnvOptions::new().secret_files()) {
        Err(ConfigError::Io(msg)) => {
            assert!(
                msg.contains("CFGIT_SF_DB__PASSWORD_FILE") && msg.contains("missing"),
                "{msg}"
            );
        }
        other => panic!("expected Io, got {other:?}"),
    }

    for var in [
        "CFGIT_SF_DB__PASSWORD_FILE",
        "CFGIT_SF_DB__PASSWORD",
        "CFGIT_SF_DB__USER",
    ] {
        std::env::remove_var(var);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn env_values_split_into_lists_when_asked() {
    #[derive(Decode, Debug, PartialEq)]
    struct Lists {
        hosts: Vec<String>,
        ports: Vec<u16>,
        motto: String,
    }

    std::env::set_var("CFGIT_LS_HOSTS", "a.example, b.example ,,c.example");
    std::env::set_var("CFGIT_LS_PORTS", "80");
    std::env::set_var("CFGIT_LS_MOTTO", "ready, steady, go");

    // Named keys only: the motto keeps its commas.
    let cfg = Config::builder()
        .merge_env_with(
            "CFGIT_LS_",
            &EnvOptions::new().list_keys(',', ["hosts", "ports"]),
        )
        .unwrap()
        .build();
    assert_eq!(
        cfg.extract::<Lists>().unwrap(),
        Lists {
            hosts: vec!["a.example".into(), "b.example".into(), "c.example".into()],
            ports: vec![80],
            motto: "ready, steady, go".into(),
        }
    );
    assert_eq!(
        cfg.value().get("ports").unwrap().as_array().unwrap().len(),
        1
    );

    // split_all splits whatever contains the separator.
    let cfg = Config::builder()
        .merge_env_with("CFGIT_LS_", &EnvOptions::new().split_all(','))
        .unwrap()
        .build();
    assert_eq!(
        cfg.get::<Vec<String>>("motto").unwrap(),
        ["ready", "steady", "go"]
    );
    assert_eq!(cfg.get::<String>("ports").unwrap(), "80");

    // Without options nothing is split.
    let cfg = Config::from_env("CFGIT_LS_");
    assert_eq!(
        cfg.get::<String>("hosts").unwrap(),
        "a.example, b.example ,,c.example"
    );

    for var in ["CFGIT_LS_HOSTS", "CFGIT_LS_PORTS", "CFGIT_LS_MOTTO"] {
        std::env::remove_var(var);
    }
}

#[test]
fn env_can_reach_camel_case_keys() {
    #[derive(Decode, Debug, PartialEq)]
    #[ferroly(rename_all = "camelCase")]
    struct Pool {
        max_connections: u32,
        idle_timeout: u32,
    }
    #[derive(Decode, Debug, PartialEq)]
    #[ferroly(rename_all = "camelCase")]
    struct Settings {
        db_pool: Pool,
    }

    let defaults =
        json::from_str(r#"{"dbPool": {"maxConnections": 5, "idleTimeout": 30}}"#).unwrap();
    std::env::set_var("CFGIT_CC_DB_POOL__MAX_CONNECTIONS", "50");

    // Default behaviour: a separate snake_case key, the field is not reached.
    let cfg = Config::builder()
        .merge_value(defaults.clone())
        .merge_env("CFGIT_CC_")
        .build();
    assert_eq!(
        cfg.extract::<Settings>().unwrap().db_pool.max_connections,
        5
    );

    // Case-insensitive matching adopts the existing spelling.
    let cfg = Config::builder()
        .merge_value(defaults.clone())
        .merge_env_with("CFGIT_CC_", &EnvOptions::new().case_insensitive())
        .unwrap()
        .build();
    assert_eq!(
        cfg.extract::<Settings>().unwrap(),
        Settings {
            db_pool: Pool {
                max_connections: 50,
                idle_timeout: 30
            }
        }
    );
    assert_eq!(
        cfg.source("dbPool.maxConnections"),
        Some(&Source::Env("CFGIT_CC_DB_POOL__MAX_CONNECTIONS".into()))
    );
    std::env::remove_var("CFGIT_CC_DB_POOL__MAX_CONNECTIONS");

    // preserve_case keeps the variable's own spelling.
    std::env::set_var("CFGIT_PC_dbPool__maxConnections", "7");
    let cfg = Config::builder()
        .merge_value(defaults)
        .merge_env_with("CFGIT_PC_", &EnvOptions::new().preserve_case())
        .unwrap()
        .build();
    assert_eq!(
        cfg.extract::<Settings>().unwrap().db_pool.max_connections,
        7
    );
    std::env::remove_var("CFGIT_PC_dbPool__maxConnections");
}

// ---- interpolation ---------------------------------------------------------

#[test]
fn interpolation_resolves_env_defaults_and_other_keys() {
    std::env::set_var("CFGIT_INT_HOST", "db.internal");
    std::env::set_var("CFGIT_INT_EMPTY", "");
    std::env::remove_var("CFGIT_INT_UNSET");

    let cfg = Config::builder()
        .merge_value(obj(&[
            ("svc_host", s("${CFGIT_INT_HOST}")),
            ("svc_port", Value::Int(5432)),
            ("svc_user", s("${CFGIT_INT_UNSET:-app}")),
            ("empty", s("[${CFGIT_INT_EMPTY}]")),
            ("empty_default", s("${CFGIT_INT_EMPTY:-fallback}")),
            (
                "svc_url",
                s("postgres://${svc_user}@${svc_host}:${svc_port}/main"),
            ),
            ("nested", s("${CFGIT_INT_UNSET:-${CFGIT_INT_HOST}}")),
            ("literal", s("$${NOT_EXPANDED} costs $5")),
            ("list", Value::Array(vec![s("${svc_host}"), s("plain")])),
            ("section", obj(&[("copy", s("${svc_url}"))])),
        ]))
        .interpolate()
        .unwrap()
        .build();

    let get = |k: &str| cfg.get::<String>(k).unwrap();
    assert_eq!(get("svc_host"), "db.internal");
    assert_eq!(get("svc_user"), "app");
    assert_eq!(get("empty"), "[]");
    assert_eq!(get("empty_default"), "fallback");
    assert_eq!(get("svc_url"), "postgres://app@db.internal:5432/main");
    assert_eq!(get("nested"), "db.internal");
    assert_eq!(get("literal"), "${NOT_EXPANDED} costs $5");
    assert_eq!(get("section.copy"), "postgres://app@db.internal:5432/main");
    assert_eq!(
        cfg.get::<Vec<String>>("list").unwrap(),
        ["db.internal", "plain"]
    );
    // Non-string values are untouched.
    assert_eq!(cfg.value().get("svc_port"), Some(&Value::Int(5432)));

    // Without `interpolate()` nothing is expanded.
    let raw = Config::builder()
        .merge_value(obj(&[("host", s("${CFGIT_INT_HOST}"))]))
        .build();
    assert_eq!(raw.get::<String>("host").unwrap(), "${CFGIT_INT_HOST}");

    std::env::remove_var("CFGIT_INT_HOST");
    std::env::remove_var("CFGIT_INT_EMPTY");
}

#[test]
fn interpolation_reports_cycles_and_unset_names() {
    let interpolate = |pairs: &[(&str, Value)]| {
        Config::builder()
            .merge_value(obj(pairs))
            .interpolate()
            .map(|b| b.build())
    };

    match interpolate(&[("cfgit_a", s("${cfgit_b}")), ("cfgit_b", s("x-${cfgit_a}"))]) {
        Err(ConfigError::Interpolation { key, reason }) => {
            assert_eq!(key, "cfgit_a");
            assert!(reason.contains("cycle"), "{reason}");
            assert!(reason.contains("cfgit_a -> cfgit_b -> cfgit_a"), "{reason}");
        }
        other => panic!("expected a cycle error, got {other:?}"),
    }

    match interpolate(&[("cfgit_self", s("${cfgit_self}"))]) {
        Err(ConfigError::Interpolation { reason, .. }) => assert!(reason.contains("cycle")),
        other => panic!("expected a cycle error, got {other:?}"),
    }

    // A key named like the variable it reads is not a cycle: the config key
    // wins for other keys, and the key itself falls through to the
    // environment.
    std::env::set_var("CFGIT_INT_SAME", "from-env");
    let cfg = interpolate(&[
        ("CFGIT_INT_SAME", s("${CFGIT_INT_SAME}-suffix")),
        ("other", s("${CFGIT_INT_SAME}")),
    ])
    .unwrap();
    assert_eq!(
        cfg.get::<String>("CFGIT_INT_SAME").unwrap(),
        "from-env-suffix"
    );
    assert_eq!(cfg.get::<String>("other").unwrap(), "from-env-suffix");
    std::env::remove_var("CFGIT_INT_SAME");

    match interpolate(&[("db", obj(&[("url", s("${CFGIT_INT_NOPE}"))]))]) {
        Err(e @ ConfigError::Interpolation { .. }) => {
            let text = e.to_string();
            assert!(
                text.contains("db.url") && text.contains("CFGIT_INT_NOPE"),
                "{text}"
            );
        }
        other => panic!("expected an unset error, got {other:?}"),
    }

    assert!(matches!(
        interpolate(&[("k", s("${unterminated"))]),
        Err(ConfigError::Interpolation { .. })
    ));
    assert!(matches!(
        interpolate(&[("k", s("${}"))]),
        Err(ConfigError::Interpolation { .. })
    ));
    // A section cannot be spliced into a string.
    assert!(matches!(
        interpolate(&[("sec", obj(&[("a", s("1"))])), ("k", s("${sec}"))]),
        Err(ConfigError::Interpolation { .. })
    ));
}

// ---- validation ------------------------------------------------------------

#[test]
fn require_reports_every_missing_key_with_its_full_path() {
    let cfg = Config::builder()
        .merge_value(obj(&[
            ("port", Value::Int(80)),
            ("db", obj(&[("host", s("h")), ("password", Value::Null)])),
        ]))
        .build();

    assert!(cfg.require(["port", "db.host"]).is_ok());
    match cfg.require(["port", "db.password", "db.pool.max", "tls.cert"]) {
        Err(e @ ConfigError::MissingKeys(_)) => {
            assert_eq!(
                e.to_string(),
                "missing required keys: db.password, db.pool.max, tls.cert"
            );
            if let ConfigError::MissingKeys(keys) = e {
                assert_eq!(keys, ["db.password", "db.pool.max", "tls.cert"]);
            }
        }
        other => panic!("expected MissingKeys, got {other:?}"),
    }
}

#[test]
fn get_errors_name_the_full_dotted_path() {
    let cfg = Config::builder()
        .merge_value(obj(&[("db", obj(&[("port", s("not-a-number"))]))]))
        .build();

    match cfg.get::<u16>("db.port") {
        Err(e @ ConfigError::InvalidValue { .. }) => {
            assert!(
                e.to_string().starts_with("invalid value for 'db.port': "),
                "{e}"
            );
            assert!(std::error::Error::source(&e).is_some());
        }
        other => panic!("expected InvalidValue, got {other:?}"),
    }
    assert!(matches!(
        cfg.get::<u16>("db.missing"),
        Err(ConfigError::MissingKey(k)) if k == "db.missing"
    ));
}

#[test]
fn schema_validation_reports_all_violations_and_coerces_strings() {
    let schema = json::from_str(
        r#"{
            "type": "object",
            "required": ["port", "name"],
            "properties": {
                "port": { "type": "integer", "minimum": 1, "maximum": 65535 },
                "debug": { "type": "boolean" },
                "ratio": { "type": "number" },
                "name": { "type": "string", "minLength": 1 },
                "db": {
                    "type": "object",
                    "required": ["host"],
                    "properties": { "port": { "type": "integer" } }
                },
                "ports": { "type": "array", "items": { "type": "integer" } }
            }
        }"#,
    )
    .unwrap();

    // String-typed sources (env, flags, properties) satisfy a typed schema.
    let ok = Config::builder()
        .merge_value(obj(&[
            ("port", s("8080")),
            ("debug", s("true")),
            ("ratio", s("0.5")),
            ("name", s("svc")),
            ("db", obj(&[("host", s("h")), ("port", s("5432"))])),
            ("ports", Value::Array(vec![s("1"), s("2")])),
        ]))
        .build();
    ok.validate_schema(&schema).unwrap();
    // Validation does not rewrite the stored values.
    assert_eq!(ok.value().get("port"), Some(&s("8080")));

    let bad = Config::builder()
        .merge_value(obj(&[
            ("port", s("70000")),
            ("debug", s("maybe")),
            ("db", obj(&[("port", s("x"))])),
        ]))
        .build();
    match bad.validate_schema(&schema) {
        Err(ConfigError::Schema(violations)) => {
            let all = violations.join("\n");
            assert!(violations.len() >= 5, "{all}");
            assert!(all.contains("name"), "{all}");
            assert!(all.contains("/port"), "{all}");
            assert!(all.contains("/debug"), "{all}");
            assert!(all.contains("/db"), "{all}");
        }
        other => panic!("expected Schema, got {other:?}"),
    }
}

// ---- provenance and the redacted dump --------------------------------------

#[test]
fn provenance_tracks_the_layer_that_set_each_key() {
    let dir = temp_dir("provenance");
    let file = dir.join("app.json");
    std::fs::write(
        &file,
        r#"{"port": 1, "db": {"host": "file", "user": "file"}}"#,
    )
    .unwrap();
    std::env::set_var("CFGIT_PV_DB__HOST", "env");

    let cfg = Config::builder()
        .merge_value(obj(&[("port", Value::Int(0)), ("name", s("default"))]))
        .merge_file(&file)
        .unwrap()
        .merge_env("CFGIT_PV_")
        .merge_args(strings(&["--port=9"]))
        .build();

    assert_eq!(cfg.source("name"), Some(&Source::Value));
    assert_eq!(cfg.source("port"), Some(&Source::Args));
    assert_eq!(
        cfg.source("db.host"),
        Some(&Source::Env("CFGIT_PV_DB__HOST".into()))
    );
    assert!(matches!(cfg.source("db.user"), Some(Source::File(p)) if p.ends_with("app.json")));
    assert_eq!(cfg.source("db"), None, "a section is not a leaf");
    assert_eq!(cfg.source("nope"), None);

    let keys: Vec<&str> = cfg.sources().map(|(k, _)| k).collect();
    assert_eq!(keys, ["db.host", "db.user", "name", "port"]);

    // A section replaced by a scalar drops the stale leaf records.
    let cfg = Config::builder()
        .merge_value(obj(&[("db", obj(&[("host", s("h"))]))]))
        .merge_args(strings(&["--db=off"]))
        .build();
    assert_eq!(cfg.sources().map(|(k, _)| k).collect::<Vec<_>>(), ["db"]);

    std::env::remove_var("CFGIT_PV_DB__HOST");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn redacted_dump_masks_secrets_and_shows_the_rest() {
    let dir = temp_dir("dump");
    let secret = dir.join("token");
    std::fs::write(&secret, "file-s3cr3t").unwrap();
    std::env::set_var("CFGIT_DP_LICENSE_FILE", &secret);

    let cfg = Config::builder()
        .merge_value(obj(&[
            ("name", s("svc")),
            ("port", Value::Int(8080)),
            (
                "db",
                obj(&[
                    ("host", s("localhost")),
                    ("password", s("pw-s3cr3t")),
                    ("url", s("postgres://app:url-s3cr3t@db/main")),
                ]),
            ),
            ("api_token", s("tok-s3cr3t")),
            ("credentials", obj(&[("user", s("cred-s3cr3t"))])),
            ("tags", Value::Array(vec![s("a"), s("b")])),
        ]))
        .merge_env_with("CFGIT_DP_", &EnvOptions::new().secret_files())
        .unwrap()
        .build();

    let dump = cfg.to_redacted_string();
    assert!(!dump.contains("s3cr3t"), "{dump}");
    assert!(dump.contains("name = \"svc\"  # value"), "{dump}");
    assert!(dump.contains("port = 8080"), "{dump}");
    assert!(dump.contains("db.host = \"localhost\""), "{dump}");
    assert!(dump.contains("db.password = <redacted>"), "{dump}");
    assert!(dump.contains("db.url = <redacted>"), "{dump}");
    assert!(dump.contains("api_token = <redacted>"), "{dump}");
    assert!(dump.contains("credentials.user = <redacted>"), "{dump}");
    assert!(dump.contains("tags[0] = \"a\""), "{dump}");
    // The secret-file value is masked although `license` is not a sensitive
    // name, and its origin is shown.
    assert!(
        dump.contains("license = <redacted>  # secret file"),
        "{dump}"
    );

    // A custom predicate replaces the key heuristic (URL credentials and
    // secret files stay masked).
    let custom = cfg.to_redacted_string_with(|path| path == "name");
    assert!(custom.contains("name = <redacted>"), "{custom}");
    assert!(custom.contains("api_token = \"tok-s3cr3t\""), "{custom}");
    assert!(custom.contains("db.url = <redacted>"), "{custom}");
    assert!(custom.contains("license = <redacted>"), "{custom}");

    // Debug stays fully redacted.
    assert!(!format!("{cfg:?}").contains("s3cr3t"));

    std::env::remove_var("CFGIT_DP_LICENSE_FILE");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- shared handle ---------------------------------------------------------

#[test]
fn shared_config_snapshots_are_stable_across_reloads() {
    let dir = temp_dir("shared");
    let file = dir.join("app.json");
    std::fs::write(&file, r#"{"port": 1, "token": "s3cr3t-shared"}"#).unwrap();

    let path = file.clone();
    let shared =
        SharedConfig::with_loader(move || Ok(Config::builder().merge_file(&path)?.build()))
            .unwrap();
    assert_eq!(shared.get::<u16>("port").unwrap(), 1);
    assert_eq!(shared.generation(), 0);

    let before = shared.snapshot();
    let other_handle = shared.clone();

    std::fs::write(&file, r#"{"port": 2}"#).unwrap();
    let after = shared.reload().unwrap();
    assert_eq!(after.get::<u16>("port").unwrap(), 2);
    assert_eq!(
        before.get::<u16>("port").unwrap(),
        1,
        "old snapshot unchanged"
    );
    assert_eq!(
        other_handle.get::<u16>("port").unwrap(),
        2,
        "clones share state"
    );
    assert_eq!(shared.generation(), 1);

    // A failed reload keeps the current configuration.
    std::fs::write(&file, "{ broken").unwrap();
    assert!(matches!(shared.reload(), Err(ConfigError::Parse { .. })));
    assert_eq!(shared.get::<u16>("port").unwrap(), 2);
    assert_eq!(shared.generation(), 1);

    // Manual replacement, and reload without a loader.
    let plain = Config::builder()
        .merge_value(obj(&[
            ("port", Value::Int(7)),
            ("token", s("s3cr3t-shared")),
        ]))
        .build()
        .into_shared();
    assert_eq!(plain.reload().unwrap().get::<u16>("port").unwrap(), 7);
    let previous = plain.replace(
        Config::builder()
            .merge_value(obj(&[("port", Value::Int(8))]))
            .build(),
    );
    assert_eq!(previous.get::<u16>("port").unwrap(), 7);
    assert_eq!(
        plain.extract::<Value>().unwrap().get("port"),
        Some(&Value::Int(8))
    );
    assert_eq!(plain.generation(), 1);

    // Usable from other threads, and never prints values.
    let handle = shared.clone();
    let port = std::thread::spawn(move || handle.get::<u16>("port").unwrap())
        .join()
        .unwrap();
    assert_eq!(port, 2);
    let shown = format!(
        "{:?}",
        Config::builder()
            .merge_value(obj(&[("token", s("s3cr3t-shared"))]))
            .build()
            .into_shared()
    );
    assert!(
        shown.contains("token") && !shown.contains("s3cr3t"),
        "{shown}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- cross-module glue -----------------------------------------------------

#[test]
fn config_error_converts_into_the_root_error() {
    fn load() -> Result<Config, ferroly::Error> {
        Ok(Config::from_file("nope.ini")?)
    }
    assert!(matches!(
        load(),
        Err(ferroly::Error::Config(ConfigError::UnsupportedFormat(_)))
    ));
}

#[cfg(feature = "cli")]
#[test]
fn parsed_cli_matches_feed_the_builder() {
    use ferroly::cli::{Arg, Command};

    #[derive(Decode, Debug, PartialEq)]
    struct Settings {
        port: u16,
        host: String,
        verbose: u8,
        debug: bool,
        tags: Vec<String>,
        db: Db,
    }

    let cmd = Command::new("svc")
        .flag(Arg::new("port").long("port").default("1000"))
        .flag(Arg::new("host").long("host").default("cli-default-host"))
        .flag(Arg::new("verbose").short('v').count())
        .flag(Arg::new("debug").long("debug").boolean())
        .flag(Arg::new("tags").long("tag").multiple())
        .flag(Arg::new("db.port").long("db-port"));
    let m = cmd
        .parse(strings(&[
            "--port",
            "9000",
            "-vv",
            "--debug",
            "--tag",
            "a",
            "--tag",
            "b",
            "--db-port=6000",
        ]))
        .unwrap();

    let file = obj(&[
        ("port", Value::Int(8080)),
        ("host", s("file-host")),
        ("verbose", Value::Int(0)),
        ("debug", Value::Bool(false)),
        (
            "db",
            obj(&[("host", s("file-db")), ("port", Value::Int(5432))]),
        ),
    ]);

    let cfg = Config::builder()
        .merge_cli_defaults(&m) // lowest: the parser's own defaults
        .merge_value(file) // then the "file"
        .merge_cli(&m) // highest: what the operator typed
        .build();

    assert_eq!(
        cfg.extract::<Settings>().unwrap(),
        Settings {
            port: 9000,               // typed on the command line
            host: "file-host".into(), // the CLI default did not override the file
            verbose: 2,
            debug: true,
            tags: vec!["a".into(), "b".into()],
            db: Db {
                host: "file-db".into(),
                port: 6000 // dotted argument name nests
            },
        }
    );
    assert_eq!(cfg.source("port"), Some(&Source::Cli));
    assert_eq!(cfg.source("host"), Some(&Source::Value));
}

#[cfg(feature = "errutils")]
#[test]
fn config_errors_classify_as_invalid_input() {
    use ferroly::errutils::{Classify, ErrorKind};

    let missing = ConfigError::MissingKeys(vec!["a".into()]);
    assert_eq!(missing.kind(), ErrorKind::InvalidInput);
    assert!(missing.is_user_error() && !missing.is_retryable());
    assert_eq!(
        ConfigError::UnsupportedFormat("x".into()).kind(),
        ErrorKind::Unsupported
    );
}

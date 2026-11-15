#![cfg(feature = "config")]
//! Unknown-key detection for the environment layer (#93), together with a
//! file and an argument layer in one error. This binary holds a single test
//! so setting process variables cannot race another test.

use ferroly::codec::{Decode, Encode};
use ferroly::config::{Config, ConfigError, EnvOptions, Source};

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
fn environment_keys_are_checked_inside_sections_and_ignored_outside() {
    let vars = [
        ("CFGUKENV_ENGINE__THREAD", "8"),   // unknown, inside a section
        ("CFGUKENV_ENGINE__THREADS", "2"),  // valid
        ("CFGUKENV_PORT", "81"),            // valid top-level key
        ("CFGUKENV_LOG", "debug"),          // not configuration: ignored
        ("CFGUKENV_KEY_1", "s3cr3t-value"), // a secret reference target
    ];
    for (k, v) in vars {
        std::env::set_var(k, v);
    }

    let dir = std::env::temp_dir().join(format!("ferroly-uk-env-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("app.json");
    std::fs::write(&file, r#"{"serve": {"port": 1}}"#).unwrap();

    let schema = App::default().encode();
    let err = Config::builder()
        .schema(schema.clone())
        .merge_file(&file)
        .unwrap()
        .merge_env("CFGUKENV_")
        .merge_args(["--engine.thread=8".to_string()])
        .build_checked()
        .unwrap_err();
    let ConfigError::UnknownKeys(keys) = err else {
        panic!("expected UnknownKeys");
    };
    let got: Vec<(&str, &str, Option<&str>)> = keys
        .iter()
        .map(|k| (k.path.as_str(), k.spelled.as_str(), k.suggestion.as_deref()))
        .collect();
    assert_eq!(
        got,
        [
            ("serve.port", "serve.port", Some("server.port")),
            (
                "engine.thread",
                "CFGUKENV_ENGINE__THREAD",
                Some("CFGUKENV_ENGINE__THREADS")
            ),
            ("engine.thread", "--engine.thread", Some("--engine.threads")),
        ]
    );
    assert_eq!(keys[0].source, Source::File(file.display().to_string()));
    assert_eq!(
        keys[1].source,
        Source::Env("CFGUKENV_ENGINE__THREAD".into())
    );
    assert_eq!(keys[2].source, Source::Args);

    // Lenient: valid variables apply, the rest are dropped; the variables
    // outside the schema are neither merged nor reported.
    let cfg = Config::builder()
        .schema(schema.clone())
        .lenient()
        .merge_env("CFGUKENV_")
        .build_checked()
        .unwrap();
    assert_eq!(cfg.get::<u32>("engine.threads").unwrap(), 2);
    assert_eq!(cfg.get::<u16>("port").unwrap(), 81);
    assert!(!cfg.contains("log") && !cfg.contains("key_1"));
    let ignored: Vec<&str> = cfg
        .ignored_keys()
        .iter()
        .map(|k| k.spelled.as_str())
        .collect();
    assert_eq!(ignored, ["CFGUKENV_ENGINE__THREAD"]);

    // `*_FILE` secret variables are spelled with their suffix, and the file
    // of an unknown one is never read (this one does not exist).
    std::env::set_var("CFGUKENV_SERVER__PRT_FILE", "/nonexistent-ferroly-file");
    let keys = match Config::builder()
        .schema(schema)
        .merge_env_with("CFGUKENV_", &EnvOptions::new().secret_files())
        .unwrap()
        .build_checked()
    {
        Err(ConfigError::UnknownKeys(keys)) => keys,
        other => panic!("unexpected {other:?}"),
    };
    let prt = keys.iter().find(|k| k.path == "server.prt").unwrap();
    assert_eq!(prt.spelled, "CFGUKENV_SERVER__PRT_FILE");
    assert_eq!(
        prt.suggestion.as_deref(),
        Some("CFGUKENV_SERVER__PORT_FILE")
    );
    assert!(matches!(prt.source, Source::SecretFile { .. }));

    // A `*_FILE` variable outside the schema is not configuration: its file
    // is not read either.
    std::env::set_var("CFGUKENV_LOG_FILE", "/nonexistent-ferroly-log");
    let cfg = Config::builder()
        .schema(App::default().encode())
        .lenient()
        .merge_env_with("CFGUKENV_", &EnvOptions::new().secret_files())
        .unwrap()
        .build_checked()
        .unwrap();
    assert!(!cfg.contains("log"));
    std::env::remove_var("CFGUKENV_LOG_FILE");
    std::env::remove_var("CFGUKENV_SERVER__PRT_FILE");

    for (k, _) in vars {
        std::env::remove_var(k);
    }
    std::fs::remove_dir_all(&dir).ok();
}

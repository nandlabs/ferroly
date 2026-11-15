#![cfg(feature = "config")]
//! `${file:…}` and `${env:…}` are expanded only in trusted layers (CFG-1),
//! and `${env:…}` content is masked (CFG-7). This binary holds a single test
//! so setting process variables cannot race another test.

use std::path::PathBuf;

use ferroly::codec::Value;
use ferroly::config::{Config, ConfigError, InterpolateOptions};

fn s(text: &str) -> Value {
    Value::Str(text.to_string())
}

fn obj(pairs: &[(&str, Value)]) -> Value {
    Value::Object(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    )
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ferroly-trust-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The reason of an interpolation error at `key`.
fn refused(result: Result<ferroly::config::ConfigBuilder, ConfigError>, key: &str) -> String {
    match result.map(|b| b.build()) {
        Err(ConfigError::Interpolation { key: at, reason }) => {
            assert_eq!(at, key, "{reason}");
            reason
        }
        other => panic!("expected an interpolation error at {key}, got {other:?}"),
    }
}

#[test]
fn file_and_env_placeholders_expand_only_in_trusted_layers() {
    let dir = temp_dir();
    let secret = dir.join("pw");
    std::fs::write(&secret, "FILE-SECRET-CFG1\n").unwrap();
    let place = format!("${{file:{}}}", secret.display());

    std::env::set_var("CFGTRUST_SERVER__HOST", "${file:/etc/passwd}");
    std::env::set_var("CFGTRUST_SERVER__NAME", &place);
    std::env::set_var("CFGTRUST_SERVER__USER", "${env:CFGTRUST_HIDDEN}");
    std::env::set_var("CFGTRUST_HIDDEN", "ENV-SECRET-CFG7");
    std::env::set_var("CFGTRUST_C_COPY", "x-${from_file}");

    // An environment variable holding `${file:/etc/passwd}` is refused, not
    // read; the error names the variable and carries no content.
    let reason = refused(
        Config::builder()
            .merge_env("CFGTRUST_SERVER__")
            .interpolate(),
        "host",
    );
    assert!(reason.contains("env CFGTRUST_SERVER__HOST"), "{reason}");
    assert!(reason.contains("InterpolateOptions"), "{reason}");
    assert!(!reason.contains("root:"), "{reason}");

    // The same file a config layer may read is refused from the environment
    // and from the command line.
    let reason = refused(
        Config::builder()
            .merge_value(obj(&[("server", obj(&[("name", s("ok"))]))]))
            .merge_env("CFGTRUST_")
            .interpolate(),
        "server.name",
    );
    assert!(!reason.contains("FILE-SECRET"), "{reason}");
    let reason = refused(
        Config::builder()
            .merge_args([format!("--engine.threads={place}")])
            .interpolate(),
        "engine.threads",
    );
    assert!(reason.contains("from args"), "{reason}");
    let reason = refused(
        Config::builder()
            .merge_args(["--engine.threads=${env:CFGTRUST_HIDDEN}"])
            .interpolate(),
        "engine.threads",
    );
    assert!(reason.contains("${env:…}"), "{reason}");
    // A default does not get around it.
    refused(
        Config::builder()
            .merge_args(["--k=${CFGTRUST_UNSET:-${file:/etc/passwd}}"])
            .interpolate(),
        "k",
    );

    // A trusted value cannot launder an untrusted one through `${key}`: the
    // referenced text is expanded as its own layer allows.
    refused(
        Config::builder()
            .merge_value(obj(&[("conn", s("pw=${server.host}"))]))
            .merge_env("CFGTRUST_")
            .interpolate(),
        "conn",
    );

    // An untrusted value may copy a trusted one, and the copy stays masked.
    let cfg = Config::builder()
        .merge_value(obj(&[("from_file", s(&place))]))
        .merge_env("CFGTRUST_C_")
        .merge_args(["--copied=${from_file}"])
        .interpolate()
        .unwrap()
        .build();
    assert_eq!(cfg.get::<String>("copied").unwrap(), "FILE-SECRET-CFG1");
    assert_eq!(cfg.get::<String>("copy").unwrap(), "x-FILE-SECRET-CFG1");
    let dump = cfg.to_redacted_string();
    assert!(dump.contains("copied = <redacted>"), "{dump}");
    assert!(dump.contains("copy = <redacted>"), "{dump}");
    assert!(!dump.contains("FILE-SECRET"), "{dump}");

    // Plain `${NAME}` placeholders still work everywhere.
    let cfg = Config::builder()
        .merge_args(["--plain=${CFGTRUST_HIDDEN}"])
        .interpolate()
        .unwrap()
        .build();
    assert_eq!(cfg.get::<String>("plain").unwrap(), "ENV-SECRET-CFG7");

    // Opting the layers in expands them; the content is masked (CFG-7 for
    // `${env:…}`).
    std::env::remove_var("CFGTRUST_SERVER__HOST");
    let cfg = Config::builder()
        .merge_env("CFGTRUST_SERVER__")
        .merge_args([format!("--engine.threads={place}")])
        .interpolate_with(&InterpolateOptions::new().trust_env().trust_args())
        .unwrap()
        .build();
    assert_eq!(cfg.get::<String>("name").unwrap(), "FILE-SECRET-CFG1");
    assert_eq!(cfg.get::<String>("user").unwrap(), "ENV-SECRET-CFG7");
    assert_eq!(
        cfg.get::<String>("engine.threads").unwrap(),
        "FILE-SECRET-CFG1"
    );
    let dump = cfg.to_redacted_string();
    for key in ["name", "user", "engine.threads"] {
        assert!(
            dump.contains(&format!("{key} = <redacted>")),
            "{key}: {dump}"
        );
    }
    assert!(!dump.contains("SECRET-CFG"), "{dump}");

    // CFG-7 in a trusted layer: `${env:…}` content is masked even under a
    // key that does not look sensitive.
    let cfg = Config::builder()
        .merge_value(obj(&[("port", s("x ${env:CFGTRUST_HIDDEN} y"))]))
        .interpolate()
        .unwrap()
        .build();
    let dump = cfg.to_redacted_string();
    assert!(dump.contains("port = <redacted>"), "{dump}");
    assert!(!dump.contains("ENV-SECRET"), "{dump}");

    for var in [
        "CFGTRUST_SERVER__HOST",
        "CFGTRUST_SERVER__NAME",
        "CFGTRUST_SERVER__USER",
        "CFGTRUST_HIDDEN",
        "CFGTRUST_C_COPY",
    ] {
        std::env::remove_var(var);
    }
    std::fs::remove_dir_all(&dir).ok();
}

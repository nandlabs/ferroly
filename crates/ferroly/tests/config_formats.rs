#![cfg(feature = "config")]
//! Restricting the file formats a builder loads (#111):
//! `ConfigBuilder::formats`, `FileFormat` and `ConfigError::FormatNotAllowed`.

use std::path::PathBuf;

use ferroly::config::FileFormat::{self, Json, Toml, Yaml};
use ferroly::config::{Config, ConfigError, Layer, Source};

/// The formats an application documents.
const DOC: [FileFormat; 3] = [Yaml, Toml, Json];

/// A fresh, empty directory unique to one test.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ferroly-formats-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// The `(path, allowed)` of a `FormatNotAllowed`, or a panic.
fn not_allowed<T: std::fmt::Debug>(result: Result<T, ConfigError>) -> (String, Vec<String>) {
    match result {
        Err(ConfigError::FormatNotAllowed { path, allowed }) => (path, allowed),
        other => panic!("expected FormatNotAllowed, got {other:?}"),
    }
}

#[test]
fn default_accepts_every_format() {
    let dir = temp_dir("default");
    let path = dir.join("app.env");
    std::fs::write(&path, "port=1\n").unwrap();
    let cfg = Config::builder().merge_file(&path).unwrap().build();
    assert_eq!(cfg.get::<u16>("port").unwrap(), 1);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn disallowed_extension_is_refused() {
    let dir = temp_dir("refused");
    let path = dir.join("app.env");
    std::fs::write(&path, "port=1\n").unwrap();
    let err = Config::builder()
        .formats(&DOC)
        .merge_file(&path)
        .unwrap_err();
    let message = err.to_string();
    let (shown, allowed) = not_allowed::<()>(Err(err));
    assert!(shown.ends_with("app.env"), "{shown}");
    assert_eq!(shown, path.display().to_string());
    assert_eq!(allowed, strings(&[".json", ".yaml", ".yml", ".toml"]));
    assert_eq!(
        message,
        format!(
            "config file format of '{}' is not accepted (expected one of .json, .yaml, .yml, .toml)",
            path.display()
        )
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn allowed_extension_loads() {
    let dir = temp_dir("allowed");
    let path = dir.join("app.yaml");
    std::fs::write(&path, "port: 2\n").unwrap();
    let cfg = Config::builder()
        .formats(&DOC)
        .merge_file(&path)
        .unwrap()
        .build();
    assert_eq!(cfg.get::<u16>("port").unwrap(), 2);
    assert_eq!(
        cfg.source("port"),
        Some(&Source::File(path.display().to_string()))
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn uppercase_extension_is_matched() {
    let dir = temp_dir("uppercase");
    let yml = dir.join("APP.YML");
    let env = dir.join("APP.ENV");
    std::fs::write(&yml, "port: 3\n").unwrap();
    std::fs::write(&env, "port=4\n").unwrap();
    let cfg = Config::builder()
        .formats(&DOC)
        .merge_file(&yml)
        .unwrap()
        .build();
    assert_eq!(cfg.get::<u16>("port").unwrap(), 3);
    not_allowed(Config::builder().formats(&DOC).merge_file(&env));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn disallowed_optional_file_fails_even_when_missing() {
    let dir = temp_dir("optional");
    let (path, _) = not_allowed(
        Config::builder()
            .formats(&DOC)
            .merge_file_optional(dir.join("missing.env")),
    );
    assert!(path.ends_with("missing.env"), "{path}");

    let cfg = Config::builder()
        .formats(&DOC)
        .merge_file_optional(dir.join("missing.yaml"))
        .unwrap()
        .build();
    assert_eq!(cfg.sources().count(), 0);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn unknown_extension_is_still_unsupported() {
    match Config::builder().formats(&DOC).merge_file("app.ini") {
        Err(ConfigError::UnsupportedFormat(p)) => assert_eq!(p, "app.ini"),
        other => panic!("expected UnsupportedFormat, got {other:?}"),
    }
    // The message still lists every known extension.
    let err = Config::builder()
        .formats(&[Json])
        .merge_file("app.ini")
        .unwrap_err();
    assert!(err.to_string().contains(".env"), "{err}");
}

#[test]
fn empty_list_refuses_every_file() {
    let dir = temp_dir("empty");
    let path = dir.join("app.yaml");
    std::fs::write(&path, "port: 1\n").unwrap();
    let err = Config::builder()
        .formats(&[])
        .merge_file(&path)
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "config file format of '{}' is not accepted (no file formats are accepted)",
            path.display()
        )
    );
    let (_, allowed) = not_allowed::<()>(Err(err));
    assert!(allowed.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn last_formats_call_wins() {
    let dir = temp_dir("last-wins");
    let toml = dir.join("app.toml");
    let json = dir.join("app.json");
    std::fs::write(&toml, "a = 1\n").unwrap();
    std::fs::write(&json, r#"{"b": 2}"#).unwrap();
    let builder = || Config::builder().formats(&[Json]).formats(&[Toml]);
    let cfg = builder().merge_file(&toml).unwrap().build();
    assert_eq!(cfg.get::<u16>("a").unwrap(), 1);
    let (_, allowed) = not_allowed(builder().merge_file(&json));
    assert_eq!(allowed, strings(&[".toml"]));

    // Order and duplicates do not matter.
    let (_, allowed) = not_allowed(
        Config::builder()
            .formats(&[Toml, Json, Toml])
            .merge_file(dir.join("x.env")),
    );
    assert_eq!(allowed, strings(&[".json", ".toml"]));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn formats_applies_only_to_later_layers() {
    let dir = temp_dir("later");
    let path = dir.join("app.env");
    std::fs::write(&path, "port=5\n").unwrap();
    let cfg = Config::builder()
        .merge_file(&path)
        .unwrap()
        .formats(&DOC)
        .build();
    assert_eq!(cfg.get::<u16>("port").unwrap(), 5);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn profile_probes_only_allowed_extensions() {
    let dir = temp_dir("profile");
    std::fs::write(dir.join("app.json"), r#"{"a": 1}"#).unwrap();
    std::fs::write(dir.join("app.env"), "b=2\n").unwrap();
    let cfg = Config::builder()
        .formats(&[Json])
        .merge_profile(dir.join("app"), "")
        .unwrap()
        .build();
    assert!(cfg.contains("a"));
    assert!(!cfg.contains("b"));

    // Unrestricted, both are merged.
    let cfg = Config::builder()
        .merge_profile(dir.join("app"), "")
        .unwrap()
        .build();
    assert!(cfg.contains("a") && cfg.contains("b"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn profile_overlay_probes_only_allowed_extensions() {
    let dir = temp_dir("profile-overlay");
    std::fs::write(dir.join("app.json"), r#"{"a": 1}"#).unwrap();
    std::fs::write(dir.join("app-prod.env"), "b=2\n").unwrap();
    let cfg = Config::builder()
        .formats(&[Json])
        .merge_profile(dir.join("app"), "prod")
        .unwrap()
        .build();
    assert!(cfg.contains("a"));
    assert!(!cfg.contains("b"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn profile_not_found_lists_only_allowed() {
    let dir = temp_dir("profile-missing");
    match Config::builder()
        .formats(&[Toml])
        .merge_profile(dir.join("app"), "")
    {
        Err(ConfigError::Io(msg)) => {
            assert!(msg.contains("tried .toml"), "{msg}");
            assert!(!msg.contains(".env"), "{msg}");
        }
        other => panic!("expected Io, got {other:?}"),
    }

    // No format at all: refused rather than "tried nothing".
    let (path, allowed) = not_allowed(
        Config::builder()
            .formats(&[])
            .merge_profile(dir.join("app"), ""),
    );
    assert_eq!(path, dir.join("app").display().to_string());
    assert!(allowed.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn profile_explicit_disallowed_extension_is_refused() {
    let dir = temp_dir("profile-explicit");
    // Were `app.env` taken as an extension-less stem, this file would load.
    std::fs::write(dir.join("app.env.json"), r#"{"a": 1}"#).unwrap();
    let (path, _) = not_allowed(
        Config::builder()
            .formats(&DOC)
            .merge_profile(dir.join("app.env"), "prod"),
    );
    assert_eq!(path, dir.join("app.env").display().to_string());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn merge_found_rejects_disallowed_names_up_front() {
    let dir = temp_dir("found");
    std::fs::write(dir.join("app.yaml"), "port: 6\n").unwrap();
    let dirs = [dir.clone()];
    let (path, _) = not_allowed(
        Config::builder()
            .formats(&DOC)
            .merge_found(&dirs, &["app.yaml", "app.env"]),
    );
    assert_eq!(path, "app.env");

    let cfg = Config::builder()
        .formats(&DOC)
        .merge_found(&dirs, &["app.yaml"])
        .unwrap()
        .build();
    assert_eq!(cfg.get::<u16>("port").unwrap(), 6);

    // An unknown extension is still only an error once it is found.
    let cfg = Config::builder()
        .formats(&DOC)
        .merge_found(&dirs, &["app.yaml", "app.ini"])
        .unwrap()
        .build();
    assert_eq!(cfg.get::<u16>("port").unwrap(), 6);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn layers_honour_formats() {
    let dir = temp_dir("layers");
    let json = dir.join("a.json");
    std::fs::write(&json, r#"{"a": 1}"#).unwrap();
    not_allowed(
        Config::builder()
            .formats(&[Toml])
            .layers([Layer::file(&json)]),
    );
    not_allowed(
        Config::builder()
            .formats(&[Toml])
            .layers([Layer::optional_file(dir.join("missing.json"))]),
    );
    not_allowed(
        Config::builder()
            .formats(&[Toml])
            .layers([Layer::profile(dir.join("a.json"), "")]),
    );

    // `from_layers` stays unrestricted.
    let cfg = Config::from_layers([Layer::file(&json)]).unwrap();
    assert_eq!(cfg.get::<u16>("a").unwrap(), 1);
    std::fs::remove_dir_all(&dir).ok();
}

#[cfg(feature = "errutils")]
#[test]
fn format_not_allowed_is_unsupported_kind() {
    use ferroly::errutils::{Classify, ErrorKind};

    let err = Config::builder()
        .formats(&DOC)
        .merge_file("app.env")
        .unwrap_err();
    assert!(matches!(err, ConfigError::FormatNotAllowed { .. }));
    assert_eq!(Classify::kind(&err), ErrorKind::Unsupported);
}

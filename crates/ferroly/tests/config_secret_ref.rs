#![cfg(all(feature = "config", feature = "hash"))]
//! Secret references as config values (#96): `env:NAME` / `file:/path`
//! decoding, resolving to `hash::Secret`, redaction and `${file:…}`
//! interpolation. No test sets a process variable: `PATH` stands in for a set
//! variable and an unlikely name for an unset one.

use std::path::{Path, PathBuf};

use ferroly::codec::{Decode, Encode, Value};
use ferroly::config::{Config, ConfigError, SecretRef};
use ferroly::hash::Secret;

const UNSET: &str = "FERROLY_TEST_SURELY_UNSET_96";

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ferroly-secret-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

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

/// `file:<path>` with forward slashes, as a config file would write it.
fn file_ref(path: &Path) -> String {
    format!("file:{}", path.display())
}

#[derive(Decode, Encode, Default, Debug)]
struct Server {
    api_keys: Vec<SecretRef>,
    admin: Option<SecretRef>,
    host: String,
}

#[derive(Decode, Encode, Default, Debug)]
struct App {
    server: Server,
}

#[test]
fn references_decode_singly_in_lists_and_as_options() {
    assert_eq!(
        SecretRef::decode(&s("env:APP_KEY")).unwrap(),
        SecretRef::env("APP_KEY")
    );
    assert_eq!(
        SecretRef::decode(&s("file:/run/secrets/k")).unwrap(),
        SecretRef::file("/run/secrets/k")
    );
    assert_eq!(
        "file:relative/k".parse::<SecretRef>().unwrap(),
        SecretRef::File(PathBuf::from("relative/k"))
    );

    let cfg = Config::builder()
        .merge_value(obj(&[(
            "server",
            obj(&[
                (
                    "api_keys",
                    Value::Array(vec![s("env:APP_KEY"), s("file:/run/secrets/key2")]),
                ),
                ("host", s("db")),
            ]),
        )]))
        .build();
    let app: App = cfg.extract().unwrap();
    assert_eq!(
        app.server.api_keys,
        [
            SecretRef::env("APP_KEY"),
            SecretRef::file("/run/secrets/key2")
        ]
    );
    assert_eq!(app.server.admin, None);

    // Encoding gives the reference back, so the struct round-trips and can
    // serve as a schema.
    assert_eq!(
        app.server.api_keys.encode(),
        Value::Array(vec![s("env:APP_KEY"), s("file:/run/secrets/key2")])
    );
    assert_eq!(
        Option::<SecretRef>::decode(&s("env:ADMIN")).unwrap(),
        Some(SecretRef::env("ADMIN"))
    );
}

#[test]
fn a_literal_is_refused_without_leaking_it() {
    let literal = "sk-live-abc123";
    let err = SecretRef::decode(&s(literal)).unwrap_err();
    let msg = err.to_string();
    assert_eq!(
        msg,
        "secret must be an env:NAME or file:/path reference, not a literal (got \"sk…\", 14 chars)"
    );
    assert!(!msg.contains(&literal[..3]) && !format!("{err:?}").contains(&literal[..3]));

    // Through a whole config, in a list.
    let cfg = Config::builder()
        .merge_value(obj(&[(
            "server",
            obj(&[("api_keys", Value::Array(vec![s(literal)]))]),
        )]))
        .build();
    let err = cfg.extract::<App>().unwrap_err();
    for text in [err.to_string(), format!("{err:?}")] {
        assert!(!text.contains("sk-"), "{text}");
        assert!(text.contains("14 chars"), "{text}");
    }

    // A short literal shows no characters at all.
    let msg = SecretRef::decode(&s("pw1")).unwrap_err().to_string();
    assert!(msg.ends_with("(got \"…\", 3 chars)"), "{msg}");
    assert!(!msg.contains("pw"), "{msg}");

    // Near misses and malformed references are refused too.
    for bad in ["ENV:APP", "env:", "file:", "env:A=B", " env:APP", ""] {
        assert!(SecretRef::decode(&s(bad)).is_err(), "{bad}");
    }
    // A non-string (a PIN written as a number) is refused without echoing it.
    let msg = SecretRef::decode(&Value::Int(73519))
        .unwrap_err()
        .to_string();
    assert!(!msg.contains("73519"), "{msg}");
}

#[test]
fn a_literal_error_names_the_field_path() {
    let literal = "sk-live-abc123";
    let cfg = Config::builder()
        .merge_value(obj(&[(
            "server",
            obj(&[
                ("api_keys", Value::Array(vec![s("env:A"), s(literal)])),
                ("host", s("h")),
            ]),
        )]))
        .build();
    let message = "secret must be an env:NAME or file:/path reference, not a literal \
                   (got \"sk…\", 14 chars)";

    // Through `extract`: the full path, with the path-free cause as source.
    match cfg.extract::<App>() {
        Err(ConfigError::InvalidValue { key, source }) => {
            assert_eq!(key, "server.api_keys[1]");
            assert_eq!(source.to_string(), message);
        }
        other => panic!("expected InvalidValue, got {other:?}"),
    }
    let err = cfg.extract::<App>().unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("invalid value for 'server.api_keys[1]': {message}")
    );

    // Through `get` on the section: the same path, joined with the key.
    match cfg.get::<Server>("server") {
        Err(ConfigError::InvalidValue { key, .. }) => assert_eq!(key, "server.api_keys[1]"),
        other => panic!("expected InvalidValue, got {other:?}"),
    }
    match cfg.get::<Vec<SecretRef>>("server.api_keys") {
        Err(ConfigError::InvalidValue { key, .. }) => assert_eq!(key, "server.api_keys[1]"),
        other => panic!("expected InvalidValue, got {other:?}"),
    }
    // An `Option<SecretRef>` holding a literal names its field too.
    let cfg = Config::builder()
        .merge_value(obj(&[(
            "server",
            obj(&[
                ("api_keys", Value::Array(vec![])),
                ("admin", s("hunter22")),
                ("host", s("h")),
            ]),
        )]))
        .build();
    match cfg.get::<Server>("server") {
        Err(ConfigError::InvalidValue { key, .. }) => assert_eq!(key, "server.admin"),
        other => panic!("expected InvalidValue, got {other:?}"),
    }
}

#[test]
fn resolve_returns_a_secret_and_errors_name_the_reference_only() {
    let dir = temp_dir("resolve");
    let lf = dir.join("lf");
    let crlf = dir.join("crlf");
    let two = dir.join("two");
    let empty = dir.join("empty");
    std::fs::write(&lf, "s3cr3t-file-value\n").unwrap();
    std::fs::write(&crlf, "s3cr3t-file-value\r\n").unwrap();
    std::fs::write(&two, "s3cr3t-file-value\n\n").unwrap();
    std::fs::write(&empty, "").unwrap();

    let secret: Secret = SecretRef::decode(&s(&file_ref(&lf)))
        .unwrap()
        .resolve()
        .unwrap();
    assert!(secret.ct_eq(b"s3cr3t-file-value"));
    let crlf_secret = SecretRef::file(&crlf).resolve().unwrap();
    assert_eq!(crlf_secret.expose_str(), Some("s3cr3t-file-value"));
    // Only one trailing newline is removed.
    assert_eq!(
        SecretRef::file(&two).resolve().unwrap().expose_str(),
        Some("s3cr3t-file-value\n")
    );
    // Empty is allowed, and visible through `is_empty`.
    assert!(SecretRef::file(&empty).resolve().unwrap().is_empty());

    // An environment variable.
    let path = std::env::var("PATH").unwrap();
    assert!(SecretRef::env("PATH")
        .resolve()
        .unwrap()
        .ct_eq(path.as_bytes()));

    // Errors: the reference and a reason, nothing else.
    let err = SecretRef::env(UNSET).resolve().unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("secret env:{UNSET}: variable is not set")
    );
    assert!(
        matches!(&err, ConfigError::Secret { reference, .. } if *reference == format!("env:{UNSET}"))
    );

    let missing = dir.join("missing");
    let err = SecretRef::file(&missing).resolve().unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("secret {}: file not found", file_ref(&missing))
    );
    let err = SecretRef::file(&dir).resolve().unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("secret {}: not a regular file", file_ref(&dir))
    );

    let bad = dir.join("binary");
    std::fs::write(&bad, [0xff, 0xfe, b's', b'3', b'c']).unwrap();
    let err = SecretRef::file(&bad).resolve().unwrap_err();
    assert!(
        err.to_string().ends_with(": content is not valid UTF-8"),
        "{err}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let locked = dir.join("locked");
        std::fs::write(&locked, "s3cr3t-file-value").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root can read anything; only check the message when access fails.
        if std::fs::read(&locked).is_err() {
            let err = SecretRef::file(&locked).resolve().unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("secret {}: permission denied", file_ref(&locked))
            );
        }
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn redacted_dump_shows_references_and_masks_literals() {
    let mut cfg = Config::builder()
        .merge_value(obj(&[
            (
                "server",
                obj(&[
                    (
                        "api_keys",
                        Value::Array(vec![
                            s("env:APP_KEY"),
                            s("file:/run/secrets/key2"),
                            s("hunter2-in-a-key"),
                        ]),
                    ),
                    ("host", s("db")),
                ]),
            ),
            (
                "upstreams",
                Value::Array(vec![s("env:UPSTREAM"), s("hunter2-upstream")]),
            ),
            ("password", s("env:DB_PASSWORD")),
        ]))
        .build();

    let dump = cfg.to_redacted_string();
    assert!(
        dump.contains("server.api_keys[0] = \"env:APP_KEY\"  # value"),
        "{dump}"
    );
    assert!(
        dump.contains("server.api_keys[1] = \"file:/run/secrets/key2\""),
        "{dump}"
    );
    // A literal under a sensitive key stays masked.
    assert!(dump.contains("server.api_keys[2] = <redacted>"), "{dump}");
    assert!(dump.contains("password = \"env:DB_PASSWORD\""), "{dump}");
    // Not a sensitive name, so a literal shows until `redact_literals`.
    assert!(dump.contains("hunter2-upstream"), "{dump}");

    cfg.redact_literals(["upstreams"]);
    let dump = cfg.to_redacted_string();
    assert!(dump.contains("upstreams[0] = \"env:UPSTREAM\""), "{dump}");
    assert!(dump.contains("upstreams[1] = <redacted>"), "{dump}");
    assert!(dump.contains("server.host = \"db\""), "{dump}");
    assert!(!dump.contains("hunter2"), "{dump}");

    // A custom sensitivity rule is final: references under the keys it
    // flags are masked too (CFG-6) …
    let dump = cfg.to_redacted_string_with(|_| true);
    assert!(dump.contains("password = <redacted>"), "{dump}");
    assert!(dump.contains("server.api_keys[0] = <redacted>"), "{dump}");
    assert!(dump.contains("server.host = <redacted>"), "{dump}");
    // … while under a `redact_literals` key it leaves alone they still show.
    let dump = cfg.to_redacted_string_with(|_| false);
    assert!(dump.contains("upstreams[0] = \"env:UPSTREAM\""), "{dump}");
    assert!(dump.contains("upstreams[1] = <redacted>"), "{dump}");
}

#[test]
fn only_well_formed_references_are_shown_under_sensitive_keys() {
    let cfg = Config::builder()
        .merge_value(obj(&[
            ("password", s("env:x y z real secret")),
            ("token", s("env:2fa-hunter2")),
            ("api_key", s("file:/run/my secret")),
            ("secret", obj(&[("note", s("env:pa$$word"))])),
            ("db_password", s("env:DB_PASSWORD")),
            ("cert_file", s("file:/run/secrets/tls.pem")),
        ]))
        .build();
    let dump = cfg.to_redacted_string();
    for masked in ["password", "token", "api_key", "secret.note"] {
        assert!(
            dump.lines()
                .any(|l| l.starts_with(&format!("{masked} = <redacted>"))),
            "{masked}: {dump}"
        );
    }
    assert!(dump.contains("db_password = \"env:DB_PASSWORD\""), "{dump}");
    assert!(
        dump.contains("cert_file = \"file:/run/secrets/tls.pem\""),
        "{dump}"
    );
    for leaked in ["real secret", "hunter2", "my secret", "pa$$word"] {
        assert!(!dump.contains(leaked), "{leaked}: {dump}");
    }
}

#[test]
fn file_interpolation_is_masked_in_the_redacted_dump() {
    let dir = temp_dir("interp");
    let pw = dir.join("pw");
    std::fs::write(&pw, "s3cr3t-dsn-pw\n").unwrap();
    std::fs::write(dir.join("db.password"), "s3cr3t-mounted\n").unwrap();
    let place = format!("${{file:{}}}", pw.display());

    let cfg = Config::builder()
        .merge_value(obj(&[
            ("conn", s(&format!("host=db user=app password={place}"))),
            (
                "hosts",
                Value::Array(vec![s(&format!("a:{place}")), s("b")]),
            ),
            ("copy", s("${conn}")),
            (
                "fallback",
                s(&format!("${{file:{}:-none}}", dir.join("nope").display())),
            ),
            ("home", s(&format!("${{env:{UNSET}:-unset}}"))),
            ("mounted", s("pw=${db.password}")),
        ]))
        .merge_secret_dir(&dir)
        .unwrap()
        .interpolate()
        .unwrap()
        .build();

    assert_eq!(
        cfg.get::<String>("conn").unwrap(),
        "host=db user=app password=s3cr3t-dsn-pw"
    );
    assert_eq!(cfg.get::<String>("fallback").unwrap(), "none");
    assert_eq!(cfg.get::<String>("home").unwrap(), "unset");
    let dump = cfg.to_redacted_string();
    for masked in ["conn", "hosts[0]", "copy", "mounted"] {
        assert!(
            dump.contains(&format!("{masked} = <redacted>")),
            "{masked}: {dump}"
        );
    }
    assert!(dump.contains("hosts[1] = \"b\""), "{dump}");
    assert!(dump.contains("fallback = \"none\""), "{dump}");
    assert!(!dump.contains("s3cr3t"), "{dump}");

    // A missing file without a default names the file, not a value.
    let err = Config::builder()
        .merge_value(obj(&[(
            "conn",
            s(&format!("${{file:{}}}", dir.join("nope").display())),
        )]))
        .interpolate()
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("does not exist"), "{msg}");
    assert!(msg.contains("conn"), "{msg}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn secret_taint_survives_odd_key_spellings_and_a_second_pass() {
    let dir = temp_dir("taint");
    std::fs::write(dir.join("db.pw"), "DIRSECRET\n").unwrap();
    let pw = dir.join("pw");
    std::fs::write(&pw, "TOPSECRET-FILE\n").unwrap();

    // CFG-4 (a): `navigate` skips empty segments, so these all copy `db.pw`;
    // each copy is masked.
    let cfg = Config::builder()
        .merge_secret_dir(&dir)
        .unwrap()
        .merge_value(obj(&[
            ("u1", s("k=${db..pw}")),
            ("u2", s("k=${.db.pw}")),
            ("u3", s("k=${x_unset_cfg4:-${db.pw.}}")),
            ("u4", s("k=${ db.pw }")),
        ]))
        .interpolate()
        .unwrap()
        .build();
    assert_eq!(cfg.get::<String>("u2").unwrap(), "k=DIRSECRET");
    let dump = cfg.to_redacted_string();
    for key in ["u1", "u2", "u3", "u4"] {
        assert!(
            dump.contains(&format!("{key} = <redacted>")),
            "{key}: {dump}"
        );
    }
    assert!(!dump.contains("DIRSECRET"), "{dump}");

    // CFG-4 (b): the taint of a first pass carries into a second one.
    let cfg = Config::builder()
        .merge_value(obj(&[("a", s(&format!("${{file:{}}}", pw.display())))]))
        .interpolate()
        .unwrap()
        .merge_value(obj(&[("b", s("x-${a}"))]))
        .interpolate()
        .unwrap()
        .build();
    assert_eq!(cfg.get::<String>("b").unwrap(), "x-TOPSECRET-FILE");
    let dump = cfg.to_redacted_string();
    assert!(dump.contains("b = <redacted>"), "{dump}");
    assert!(!dump.contains("TOPSECRET"), "{dump}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn no_secret_value_appears_in_debug_display_or_errors() {
    let dir = temp_dir("leak");
    let file = dir.join("k");
    std::fs::write(&file, "s3cr3t-leak-check\n").unwrap();
    let reference = SecretRef::file(&file);
    let secret = reference.resolve().unwrap();

    let mut outputs = vec![
        format!("{secret:?}"),
        format!("{secret}"),
        format!("{reference:?}"),
        format!("{reference}"),
    ];
    let literal = SecretRef::decode(&s("s3cr3t-leak-check")).unwrap_err();
    outputs.push(literal.to_string());
    outputs.push(format!("{literal:?}"));

    let cfg = Config::builder()
        .merge_value(obj(&[
            ("dsn", s(&format!("x=${{file:{}}}", file.display()))),
            ("key", s(&file_ref(&file))),
        ]))
        .interpolate()
        .unwrap()
        .build();
    outputs.push(format!("{cfg:?}"));
    outputs.push(cfg.to_redacted_string());
    let unreadable = ConfigError::Secret {
        reference: reference.to_string(),
        reason: "permission denied".into(),
    };
    outputs.push(unreadable.to_string());

    for text in &outputs {
        assert!(!text.contains("s3cr3t"), "{text}");
    }
    // The reference itself is visible where it should be.
    assert!(outputs[3].starts_with("file:"));
    assert!(outputs[outputs.len() - 2].contains(&format!("key = {:?}", file_ref(&file))));
    std::fs::remove_dir_all(&dir).ok();
}

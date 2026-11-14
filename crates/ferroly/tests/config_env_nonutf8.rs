#![cfg(all(feature = "config", unix))]
//! C12 regression: an environment variable that is not valid UTF-8 — even one
//! that has nothing to do with the application — must not panic the loader.
//!
//! This lives in its own test binary because the variable is process-wide:
//! any other test calling `std::env::vars()` while it is set would panic.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

use ferroly::config::{Config, EnvOptions};

#[test]
fn non_utf8_environment_variables_are_skipped() {
    let bad = || OsString::from_vec(vec![0x66, 0x6f, 0xff, 0xfe]);

    // An unrelated variable with a non-UTF-8 value…
    std::env::set_var("CFGIT_NU_UNRELATED_SYSTEM_VAR", bad());
    // …a matching variable with a non-UTF-8 value…
    std::env::set_var("CFGIT_NU_APP_BROKEN", bad());
    // …a variable whose *name* is not UTF-8…
    let mut name = OsString::from("CFGIT_NU_APP_");
    name.push(bad());
    std::env::set_var(&name, "value");
    // …and a perfectly good one.
    std::env::set_var("CFGIT_NU_APP_PORT", "8080");

    let cfg = Config::from_env("CFGIT_NU_APP_");
    assert_eq!(cfg.get::<u16>("port").unwrap(), 8080);
    assert!(!cfg.contains("broken"), "non-UTF-8 values are skipped");
    assert_eq!(cfg.sources().count(), 1);

    // Every env-reading entry point goes through the same code.
    let cfg = Config::builder()
        .merge_env("CFGIT_NU_APP_")
        .merge_env_with(
            "CFGIT_NU_APP_",
            &EnvOptions::new().secret_files().case_insensitive(),
        )
        .unwrap()
        .build();
    assert_eq!(cfg.get::<u16>("port").unwrap(), 8080);

    // Interpolation meeting a non-UTF-8 variable reports it instead of
    // panicking.
    let err = Config::builder()
        .merge_value(ferroly::codec::Value::Object(vec![(
            "k".into(),
            ferroly::codec::Value::Str("${CFGIT_NU_APP_BROKEN}".into()),
        )]))
        .interpolate()
        .unwrap_err();
    assert!(err.to_string().contains("not UTF-8"), "{err}");

    std::env::remove_var("CFGIT_NU_UNRELATED_SYSTEM_VAR");
    std::env::remove_var("CFGIT_NU_APP_BROKEN");
    std::env::remove_var(&name);
    std::env::remove_var("CFGIT_NU_APP_PORT");
}

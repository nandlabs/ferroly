#![cfg(feature = "config")]
//! An environment variable that names a schema section rather than a key is
//! not configuration (#107): it never replaces the section, its `*_FILE`
//! target is never read, and it is not reported. This binary holds a single
//! test so setting process variables cannot race another test.

use ferroly::codec::{Decode, Encode};
use ferroly::config::{Config, ConfigBuilder, EnvOptions};

#[derive(Encode, Decode, Debug, PartialEq)]
struct Model {
    path: String,
    context: u32,
}

#[derive(Encode, Decode, Debug, PartialEq)]
struct B {
    c: u32,
}

#[derive(Encode, Decode, Debug, PartialEq)]
struct A {
    b: B,
}

#[derive(Encode, Decode, Debug, PartialEq)]
struct App {
    model: Model,
    a: A,
    port: u16,
}

impl Default for App {
    fn default() -> Self {
        App {
            model: Model {
                path: "m.gguf".into(),
                context: 4096,
            },
            a: A { b: B { c: 1 } },
            port: 1,
        }
    }
}

const PREFIX: &str = "CFGENVSEC_";

fn base(lenient: bool) -> ConfigBuilder {
    let builder = Config::builder().schema(App::default().encode());
    let builder = if lenient { builder.lenient() } else { builder };
    builder.merge_value(App::default().encode())
}

fn check(cfg: &Config) {
    assert!(cfg.ignored_keys().is_empty(), "{:?}", cfg.ignored_keys());
    assert_eq!(cfg.get::<String>("model.path").unwrap(), "y");
    assert_eq!(cfg.get::<u32>("model.context").unwrap(), 4096);
    assert_eq!(cfg.get::<u32>("a.b.c").unwrap(), 1);
    let app: App = cfg.extract().unwrap();
    assert_eq!(
        app,
        App {
            model: Model {
                path: "y".into(),
                context: 4096,
            },
            ..App::default()
        }
    );
}

#[test]
fn env_variable_naming_a_section_leaves_it_untouched() {
    let missing = std::env::temp_dir().join(format!(
        "ferroly-env-section-{}-does-not-exist",
        std::process::id()
    ));
    let missing = missing.display().to_string();
    let vars = [
        ("CFGENVSEC_MODEL", "x"),            // names a section: not configuration
        ("CFGENVSEC_MODEL__PATH", "y"),      // a key inside it: applied
        ("CFGENVSEC_A__B", "z"),             // names a nested section
        ("CFGENVSEC_MODEL_FILE", &*missing), // a section's secret file: never read
    ];
    for (k, v) in vars {
        std::env::set_var(k, v);
    }

    let options = EnvOptions::new().secret_files();
    for lenient in [false, true] {
        // With secret files: the missing file is never opened.
        let cfg = base(lenient)
            .merge_env_with(PREFIX, &options)
            .unwrap()
            .build_checked()
            .unwrap();
        check(&cfg);

        // Without secret files: the same result.
        let cfg = base(lenient).merge_env(PREFIX).build_checked().unwrap();
        check(&cfg);
    }

    for (k, _) in vars {
        std::env::remove_var(k);
    }
}

#![cfg(feature = "protobuf")]
//! Golden test for the code generator: generating from the sample `.proto`
//! files must reproduce the checked-in `proto_data/sample_generated.rs`
//! (which `proto_codegen.rs` compiles and exercises).
//!
//! After an intentional change to the generator, regenerate it with:
//!
//! ```sh
//! FERROLY_BLESS=1 cargo test -p ferroly --features protobuf --test proto_codegen_golden
//! ```

use std::path::PathBuf;

use ferroly::proto::codegen::Config;
use ferroly::proto::idl::{FileSet, FsLoader};

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/proto_data")
}

fn generate() -> String {
    let loader = FsLoader::new([data_dir()]);
    let set = FileSet::load(&loader, &["sample/v1/sample.proto"]).unwrap();
    Config::new()
        .type_attribute(".sample.common.Label", "#[derive(Eq, Hash)]")
        .generate(&set)
        .unwrap()
}

#[test]
fn generated_code_matches_the_checked_in_file() {
    let code = generate();
    let path = data_dir().join("sample_generated.rs");
    if std::env::var_os("FERROLY_BLESS").is_some() {
        std::fs::write(&path, &code).unwrap();
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap()
        .replace("\r\n", "\n");
    assert!(
        code == expected,
        "generated code changed; rerun with FERROLY_BLESS=1 to update {}\n--- generated ---\n{code}",
        path.display()
    );
}

#[test]
fn generation_is_deterministic() {
    assert_eq!(generate(), generate());
}

#[test]
fn compile_to_file_writes_only_on_change() {
    let dir = std::env::temp_dir().join(format!(
        "ferroly-proto-codegen-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("protos.rs");
    let cfg = Config::new().type_attribute(".sample.common.Label", "#[derive(Eq, Hash)]");
    cfg.compile_to_file(&[data_dir()], &["sample/v1/sample.proto"], &out)
        .unwrap();
    assert_eq!(std::fs::read_to_string(&out).unwrap(), generate());
    let first = std::fs::metadata(&out).unwrap().modified().unwrap();
    cfg.compile_to_file(&[data_dir()], &["sample/v1/sample.proto"], &out)
        .unwrap();
    assert_eq!(std::fs::metadata(&out).unwrap().modified().unwrap(), first);

    let err = cfg
        .compile_to_file(&[data_dir()], &["missing.proto"], &out)
        .unwrap_err();
    assert!(
        err.message.contains("cannot load \"missing.proto\""),
        "{err}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#![cfg(feature = "grpc")]
//! Golden tests for the gRPC service generator: generating from
//! `grpc_data/echo/v1/echo.proto` and `grpc_data/tricky/v1/tricky.proto`
//! must reproduce the checked-in `grpc_data/echo_generated.rs` and
//! `grpc_data/tricky_generated.rs` (which `grpc_codegen.rs` compiles and
//! runs).
//!
//! After an intentional change to the generator, regenerate them with:
//!
//! ```sh
//! FERROLY_BLESS=1 cargo test -p ferroly --features grpc --test grpc_codegen_golden
//! ```

use std::path::PathBuf;

use ferroly::grpc::codegen::GrpcGenerator;
use ferroly::proto::codegen::Config;
use ferroly::proto::idl::{FileSet, FsLoader};

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/grpc_data")
}

fn generate_file(file: &str, generator: GrpcGenerator) -> String {
    let loader = FsLoader::new([data_dir()]);
    let set = FileSet::load(&loader, &[file]).unwrap();
    Config::new()
        .service_generator(generator)
        .generate(&set)
        .unwrap()
}

fn generate(generator: GrpcGenerator) -> String {
    generate_file("echo/v1/echo.proto", generator)
}

fn check_golden(proto: &str, golden: &str) {
    let code = generate_file(proto, GrpcGenerator::new());
    let path = data_dir().join(golden);
    if std::env::var_os("FERROLY_BLESS").is_some() {
        std::fs::write(&path, &code).unwrap();
    }
    // Windows checkouts may turn the file's line endings into CRLF.
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
fn generated_code_matches_the_checked_in_file() {
    check_golden("echo/v1/echo.proto", "echo_generated.rs");
}

#[test]
fn tricky_names_match_the_checked_in_file() {
    check_golden("tricky/v1/tricky.proto", "tricky_generated.rs");
}

#[test]
fn server_and_client_can_be_switched_off() {
    let both = generate(GrpcGenerator::new());
    assert_eq!(generate(GrpcGenerator::default()), both);
    assert!(both.contains("pub mod echo_server {"));
    assert!(both.contains("pub mod echo_client {"));
    assert!(both.contains("pub trait Echo:"));
    assert!(both.contains("pub struct EchoClient"));

    let server_only = generate(GrpcGenerator::new().client(false));
    assert!(server_only.contains("pub struct EchoServer<T: ?::core::marker::Sized>"));
    assert!(!server_only.contains("EchoClient") && !server_only.contains("echo_client"));

    let client_only = generate(GrpcGenerator::new().server(false));
    assert!(client_only.contains("pub struct EchoClient"));
    assert!(!client_only.contains("pub trait Echo:") && !client_only.contains("echo_server"));

    let neither = generate(GrpcGenerator::new().server(false).client(false));
    assert!(!neither.contains("Echo:") && !neither.contains("EchoClient"));
    assert!(neither.contains("ECHO_DESCRIPTOR"));
}

fn generate_source(source: &str, generator: GrpcGenerator) -> Result<String, String> {
    let set = FileSet::from_sources(&[("c.proto", source)]).map_err(|e| e.to_string())?;
    Config::new()
        .service_generator(generator)
        .generate(&set)
        .map_err(|e| e.to_string())
}

#[test]
fn clashing_method_names_are_renamed_on_the_client() {
    let code = generate_source(
        r#"
        syntax = "proto3";
        package c;
        message M {}
        service S {
          rpc New (M) returns (M); rpc Channel (M) returns (M); rpc Type (M) returns (M);
          rpc Clone (M) returns (M); rpc CloneFrom (M) returns (M); rpc Fmt (M) returns (M);
          rpc Into (M) returns (M);
        }
        "#,
        GrpcGenerator::new(),
    )
    .unwrap();
    for renamed in [
        "new_rpc",
        "channel_rpc",
        "clone_rpc",
        "clone_from_rpc",
        "fmt_rpc",
        "into_rpc",
    ] {
        assert!(
            code.contains(&format!("pub async fn {renamed}(&self")),
            "{renamed}: {code}"
        );
    }
    assert!(code.contains("pub async fn r#type(&self"), "{code}");
    // The server trait keeps the plain names.
    assert!(code.contains("fn new(&self, request:"), "{code}");
    assert!(code.contains("fn clone(&self, request:"), "{code}");
}

#[test]
fn a_renamed_client_method_never_takes_another_rpcs_name() {
    // `rpc New` would become `new_rpc`, which `rpc NewRpc` already is.
    let code = generate_source(
        r#"
        syntax = "proto3";
        package c;
        message M {}
        service S {
          rpc New (M) returns (M); rpc NewRpc (M) returns (M);
          rpc Fmt (M) returns (M);
        }
        "#,
        GrpcGenerator::new(),
    )
    .unwrap();
    for name in ["new_rpc", "new_rpc_", "fmt_rpc"] {
        let decl = format!("pub async fn {name}(&self");
        assert_eq!(code.matches(&decl).count(), 1, "{name}: {code}");
    }
}

#[test]
fn the_adapter_calls_the_trait_fully_qualified() {
    let code = generate_source(
        r#"
        syntax = "proto3";
        package c;
        message M {}
        service S { rpc Clone (M) returns (M); }
        "#,
        GrpcGenerator::new(),
    )
    .unwrap();
    assert!(code.contains("<T as S>::clone(&*svc, request)"), "{code}");
}

#[test]
fn messages_named_like_the_generated_items_do_not_clash() {
    // The trait, server and client live in their own submodules.
    let code = generate_source(
        r#"
        syntax = "proto3";
        package c;
        message GreeterServer {}
        message GreeterClient {}
        service Greeter { rpc Hi (GreeterServer) returns (GreeterClient); }
        "#,
        GrpcGenerator::new(),
    )
    .unwrap();
    assert!(code.contains("pub struct GreeterServer {"), "{code}");
    assert!(code.contains("pub struct GreeterServer<T: ?::core::marker::Sized>"));
    assert!(code.contains("super::GreeterClient"), "{code}");
}

#[test]
fn a_nested_type_module_named_like_a_service_module_is_an_error() {
    let clash = r#"
        syntax = "proto3";
        package c;
        message GreeterServer { message Inner {} }
        message M {}
        service Greeter { rpc Hi (M) returns (M); }
    "#;
    let err = generate_source(clash, GrpcGenerator::new()).unwrap_err();
    assert!(err.contains("greeter_server"), "{err}");
    // Without the server side there is no `greeter_server` module.
    assert!(generate_source(clash, GrpcGenerator::new().server(false)).is_ok());

    let client_clash = clash.replace("GreeterServer", "GreeterClient");
    let err = generate_source(&client_clash, GrpcGenerator::new()).unwrap_err();
    assert!(err.contains("greeter_client"), "{err}");
    assert!(generate_source(&client_clash, GrpcGenerator::new().client(false)).is_ok());

    // Two services whose submodules would coincide.
    let twins = r#"
        syntax = "proto3";
        package c;
        message M {}
        service FooBar { rpc Hi (M) returns (M); }
        service Foo_Bar { rpc Hi (M) returns (M); }
    "#;
    assert!(generate_source(twins, GrpcGenerator::new()).is_err());
}

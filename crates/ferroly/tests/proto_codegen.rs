#![cfg(feature = "protobuf")]
//! Compiles the code generated from `proto_data/sample/**.proto` (checked in
//! as `proto_data/sample_generated.rs`, kept current by
//! `proto_codegen_golden.rs`) and exercises it: round trips, wire bytes,
//! boxing of recursive types, proto2 `required`, enums and service
//! descriptors.
//!
//! The generated code is included in a *public* module of a crate that
//! denies missing docs, as a consumer's library would, so an undocumented
//! generated item fails this build (and `clippy --all-targets -D warnings`
//! lints it).
#![deny(missing_docs)]

/// The generated code.
pub mod protos {
    include!("proto_data/sample_generated.rs");
}

use std::collections::BTreeMap;

use ferroly::proto::idl::{FileSet, MemoryLoader};
use ferroly::proto::{EnumValue, Enumeration, Message, MethodKind, ProtoError};
use protos::sample::common::Label;
use protos::sample::legacy::{job, Job, Status};
use protos::sample::v1::search_request::{filter, Destination, Filter};
use protos::sample::v1::{
    tree, Corpus, Scalars, SearchRequest, SearchResponse, Tree, SEARCH_SERVICE_DESCRIPTOR,
};

fn round_trip<M: Message + PartialEq + std::fmt::Debug>(m: &M) -> Vec<u8> {
    let bytes = m.encode_to_vec();
    assert_eq!(m.encoded_len(), bytes.len());
    assert_eq!(&M::decode(&bytes).unwrap(), m);
    bytes
}

#[test]
fn generated_messages_round_trip() {
    let mut counts = BTreeMap::new();
    counts.insert("a".to_string(), 1);
    let mut labels = BTreeMap::new();
    labels.insert(
        -3,
        Label {
            key: "k".into(),
            value: "v".into(),
            ..Default::default()
        },
    );
    let req = SearchRequest {
        query: "rust".into(),
        page_number: Some(0),
        corpus: Corpus::Images.into(),
        ids: vec![1, 2, 3],
        deltas: vec![-1, 1],
        tags: vec!["x".into()],
        counts,
        labels,
        sent_at: Some(protos::google::protobuf::Timestamp {
            seconds: 1_700_000_000,
            nanos: 5,
            ..Default::default()
        }),
        r#type: "t".into(),
        fallback: Some(EnumValue::from_raw(77)),
        destination: Some(Destination::Slot(-4)),
        filters: vec![Filter {
            field: "f".into(),
            op: filter::Op::Ne.into(),
            and: vec![Filter::default()],
            ..Default::default()
        }],
        ..Default::default()
    };
    round_trip(&req);

    let scalars = Scalars {
        f_double: 1.5,
        f_float: -2.5,
        f_int32: -1,
        f_int64: i64::MIN,
        f_uint32: u32::MAX,
        f_uint64: u64::MAX,
        f_sint32: i32::MIN,
        f_sint64: -9,
        f_fixed32: 7,
        f_fixed64: 8,
        f_sfixed32: -7,
        f_sfixed64: -8,
        f_bool: true,
        f_string: "s".into(),
        f_bytes: vec![0, 1],
        ..Default::default()
    };
    round_trip(&scalars);
    assert!(Scalars::default().encode_to_vec().is_empty());
}

#[test]
fn generated_wire_bytes_follow_the_proto_definition() {
    let req = SearchRequest {
        query: "a".into(),
        ids: vec![1, 2],
        deltas: vec![-1, 1],
        destination: Some(Destination::Email("e".into())),
        ..Default::default()
    };
    // query = 1 (string), ids = 5 packed, deltas = 6 unpacked sint64,
    // email = 20 (two-byte key a2 01).
    assert_eq!(
        req.encode_to_vec(),
        [
            0x0a, 0x01, b'a', 0x2a, 0x02, 0x01, 0x02, 0x30, 0x01, 0x30, 0x02, 0xa2, 0x01, 0x01,
            b'e'
        ]
    );
}

#[test]
fn recursive_types_are_boxed() {
    let tree = Tree {
        name: "root".into(),
        left: Some(Box::new(Tree {
            name: "l".into(),
            ..Default::default()
        })),
        children: vec![Tree::default(), Tree::default()],
        extra: Some(tree::Extra::Only(Box::new(Tree {
            extra: Some(tree::Extra::Weight(3)),
            ..Default::default()
        }))),
        ..Default::default()
    };
    round_trip(&tree);

    let job = Job {
        id: 1,
        parent: Some(Box::new(Job {
            id: 2,
            ..Default::default()
        })),
        ..Default::default()
    };
    round_trip(&job);
}

#[test]
fn proto2_rules() {
    // `attempts` is unpacked (proto2 default), `packed_attempts` packed.
    let job = Job {
        id: 5,
        attempts: vec![1, 2],
        packed_attempts: vec![3, 4],
        status: Some(Status::Done.into()),
        detail: job::Detail {
            blob: Some(vec![]),
            ..Default::default()
        },
        ..Default::default()
    };
    let bytes = round_trip(&job);
    assert_eq!(
        bytes,
        [
            0x08, 0x05, 0x18, 0x01, 0x18, 0x02, 0x22, 0x02, 0x03, 0x04, 0x28, 0x02, 0x3a, 0x02,
            0x0a, 0x00
        ]
    );
    // Required fields are always written and must be present.
    assert_eq!(Job::default().encode_to_vec(), [0x08, 0x00, 0x3a, 0x00]);
    assert!(matches!(
        Job::decode(&[0x3a, 0x00]),
        Err(ProtoError::MissingRequired { field: "id", .. })
    ));
    // Aliases are associated constants.
    assert_eq!(Status::STATUS_RUNNING, Status::Started);
    assert_eq!(Status::from_proto_name("STATUS_DONE"), Some(Status::Done));
}

#[test]
fn unknown_fields_survive_generated_types() {
    // A newer SearchResponse.Result with an extra field 9.
    let newer = [0x0a, 0x05, 0x0a, 0x01, b'u', 0x48, 0x07];
    let resp = SearchResponse::decode(&newer).unwrap();
    assert_eq!(resp.results[0].url, "u");
    assert_eq!(resp.results[0].unknown_fields.len(), 1);
    assert_eq!(resp.encode_to_vec(), newer);
}

#[test]
fn enums_strip_their_prefix() {
    assert_eq!(Corpus::Images.to_i32(), 2);
    assert_eq!(Corpus::Images.proto_name(), "CORPUS_IMAGES");
    assert_eq!(filter::Op::from_i32(1), Some(filter::Op::Eq));
}

#[test]
fn service_descriptors_describe_every_method() {
    let d = SEARCH_SERVICE_DESCRIPTOR;
    assert_eq!(d.full_name, "sample.v1.SearchService");
    assert_eq!(d.package, "sample.v1");
    let kinds: Vec<(&str, MethodKind)> = d.methods.iter().map(|m| (m.path, m.kind())).collect();
    assert_eq!(
        kinds,
        [
            ("/sample.v1.SearchService/Search", MethodKind::Unary),
            (
                "/sample.v1.SearchService/Watch",
                MethodKind::ServerStreaming
            ),
            (
                "/sample.v1.SearchService/Upload",
                MethodKind::ClientStreaming
            ),
            ("/sample.v1.SearchService/Chat", MethodKind::BidiStreaming),
        ]
    );
    let watch = d.method("Watch").unwrap();
    assert_eq!(watch.output_type, "sample.v1.SearchResponse.Result");
    assert_eq!(
        d.method_by_path("/sample.v1.SearchService/Upload")
            .unwrap()
            .input_type,
        "sample.common.Label"
    );
}

#[test]
fn hash_maps_and_disabled_descriptors() {
    let loader = MemoryLoader::new().with(
        "m.proto",
        "syntax = \"proto3\"; message M { map<string, string> m = 1; } service S { rpc A (M) returns (M); }",
    );
    let set = FileSet::load(&loader, &["m.proto"]).unwrap();
    let code = ferroly::proto::codegen::Config::new()
        .map_type(ferroly::proto::codegen::MapType::HashMap)
        .service_descriptors(false)
        .generate(&set)
        .unwrap();
    assert!(
        code.contains("::std::collections::HashMap<::std::string::String, ::std::string::String>"),
        "{code}"
    );
    assert!(!code.contains("DESCRIPTOR"), "{code}");
    // A file without a package lands at the root.
    assert!(code.contains("\npub struct M {"), "{code}");
}

#[test]
fn fields_are_written_in_number_order() {
    use protos::sample::common::{ordered, Ordered};
    // `int32 z = 15; int32 a = 14;` writes 14 first, as protoc does.
    let m = Ordered {
        z: 1,
        a: 1,
        ..Default::default()
    };
    assert_eq!(round_trip(&m), [0x70, 0x01, 0x78, 0x01]);
    // A oneof is written at its lowest member's number (3, before 14).
    let m = Ordered {
        a: 1,
        pick: Some(ordered::Pick::Early("e".into())),
        ..Default::default()
    };
    assert_eq!(round_trip(&m), [0x1a, 0x01, b'e', 0x70, 0x01]);
}

fn generate(sources: &[(&str, &str)]) -> Result<String, ferroly::proto::idl::IdlError> {
    ferroly::proto::codegen::generate(&FileSet::from_sources(sources).unwrap())
}

#[test]
fn every_module_is_documented() {
    let code = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/proto_data/sample_generated.rs"
    ))
    .unwrap()
    .replace("\r\n", "\n");
    let lines: Vec<&str> = code.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with("pub mod ") {
            assert!(
                lines[i - 1].trim_start().starts_with("///"),
                "undocumented module: {line}"
            );
        }
    }
}

#[test]
fn rust_name_collisions_are_errors() {
    let cases: &[(&str, &[(&str, &str)])] = &[
        (
            "nested-type module vs package",
            &[
                (
                    "a.proto",
                    "syntax='proto3'; package a; message B { message C {} }",
                ),
                ("ab.proto", "syntax='proto3'; package a.b; message C {}"),
            ],
        ),
        (
            "PascalCase collision",
            &[(
                "x.proto",
                "syntax='proto3'; message HTTPRequest {} message HttpRequest {}",
            )],
        ),
        (
            "lower and upper case message",
            &[("x.proto", "syntax='proto3'; message foo {} message Foo {}")],
        ),
        (
            "enum vs message",
            &[(
                "x.proto",
                "syntax='proto3'; message Foo_Bar {} enum FooBar { A = 0; }",
            )],
        ),
        (
            "oneof enum vs nested message",
            &[(
                "x.proto",
                "syntax='proto3'; message M { enum Kind { K0 = 0; } oneof kind { int32 x = 1; } message KindOneof {} }",
            )],
        ),
        (
            "service descriptor constants",
            &[(
                "x.proto",
                "syntax='proto3'; message P {} service FooBar { rpc A(P) returns (P); } service Foo_Bar { rpc A(P) returns (P); }",
            )],
        ),
        (
            "package casing",
            &[
                ("a.proto", "syntax='proto3'; package fooBar; message A {}"),
                ("b.proto", "syntax='proto3'; package foo_bar; message B {}"),
            ],
        ),
        (
            "message module vs package module",
            &[
                (
                    "a.proto",
                    "syntax='proto3'; package p; message Q { message R {} }",
                ),
                ("b.proto", "syntax='proto3'; package p.q; message S {}"),
            ],
        ),
    ];
    for (name, sources) in cases {
        let err = generate(sources).expect_err(name);
        assert!(
            err.message.contains("rename one"),
            "{name}: {}",
            err.message
        );
    }

    // One package spread over two files is not a clash.
    generate(&[
        ("a.proto", "syntax='proto3'; package p.q; message A {}"),
        ("b.proto", "syntax='proto3'; package p.q; message B {}"),
    ])
    .unwrap();

    // A field named `unknown_fields` moves the unknown-field member aside.
    let code = generate(&[(
        "x.proto",
        "syntax='proto3'; message M { int32 unknown_fields = 1; }",
    )])
    .unwrap();
    assert!(code.contains("pub unknown_fields_: ::ferroly::proto::UnknownFields"));
}

#[test]
fn service_generators_reserve_their_names() {
    use ferroly::proto::codegen::{Config, ServiceContext, ServiceGenerator};
    struct Module;
    impl ServiceGenerator for Module {
        fn generate(&self, s: &ServiceContext<'_>, out: &mut String) {
            out.push_str(&format!(
                "/// Server.\npub mod {}_server {{}}\n",
                s.snake_name
            ));
        }
        fn reserved_names(&self, s: &ServiceContext<'_>) -> Vec<String> {
            vec![format!("{}_server", s.snake_name)]
        }
    }
    let set = FileSet::from_sources(&[(
        "x.proto",
        "syntax='proto3'; package p; message EchoServer { message Inner {} } service Echo { rpc A(EchoServer) returns (EchoServer); }",
    )])
    .unwrap();
    let err = Config::new()
        .service_generator(Module)
        .generate(&set)
        .unwrap_err();
    assert!(err.message.contains("`echo_server`"), "{}", err.message);
    assert!(err.message.contains("service `p.Echo`"), "{}", err.message);
}

#[test]
fn type_attributes_reach_oneof_enums() {
    let set = FileSet::from_sources(&[(
        "x.proto",
        "syntax='proto3'; package p; message M { oneof k { string a = 1; int32 b = 2; } }",
    )])
    .unwrap();
    let code = ferroly::proto::codegen::Config::new()
        .type_attribute(".", "#[derive(Eq, Hash)]")
        .type_attribute(".p.M.k", "#[allow(clippy::large_enum_variant)]")
        .type_attribute(".q", "#[should_not_appear]")
        .generate(&set)
        .unwrap();
    let oneof = &code[code.find("pub enum K").unwrap() - 200..];
    assert!(oneof.contains("#[derive(Eq, Hash)]"), "{code}");
    assert!(
        oneof.contains("#[allow(clippy::large_enum_variant)]"),
        "{code}"
    );
    assert!(!code.contains("should_not_appear"), "{code}");
}

#[test]
fn doc_comments_are_lint_clean() {
    use ferroly::proto::codegen::doc_comment;
    assert_eq!(
        doc_comment("See https://example.com/a_b*c, or http://x.y."),
        "/// See <https://example.com/a_b*c>, or <http://x.y>.\n"
    );
    assert_eq!(
        doc_comment("`https://in.code` stays"),
        "/// `https://in.code` stays\n"
    );
    assert_eq!(doc_comment("a\tb"), "/// a    b\n");
    assert_eq!(
        doc_comment("- item\nmore\n\nafter\n1. one\ntwo"),
        "/// - item\n///   more\n///\n/// after\n/// 1. one\n///    two\n"
    );
}

#![cfg(feature = "protobuf")]
//! The `.proto` parser and loader: what it accepts, how it resolves names
//! across files and scopes, and the line/column of every error it reports.

use std::io;

use ferroly::proto::idl::{
    parse, Constant, FieldType, FileSet, FsLoader, IdlError, ImportKind, Label, MemoryLoader,
    ScalarType, Syntax, TypeKind,
};

fn err(src: &str) -> IdlError {
    parse("t.proto", src).unwrap_err()
}

/// `(line, column, message)` of the error for `src`.
fn at(src: &str) -> (u32, u32, String) {
    let e = err(src);
    (e.line, e.column, e.message)
}

#[test]
fn parses_proto3_declarations() {
    let f = parse(
        "a.proto",
        r#"
syntax = "proto3";
package a.b;
import "x.proto";
import public "y.proto";
import weak "z.proto";
option java_package = "dev.x";
option (my.ext).field = -5;
option cc_enable_arenas = true;

// Doc for M.
message M {
  option deprecated = true;
  int32 a = 1 [json_name = "A", deprecated = true];
  optional string b = 2;
  repeated sint64 c = 3 [packed = false];
  map<string, Other> d = 4;
  .a.b.Other e = 5;
  oneof choice {
    option (o) = 1;
    bytes f = 6;
    Other g = 7;
  }
  reserved 10, 20 to 30, 40 to max;
  reserved "old", "older";
  message Inner { enum E { E_A = 0; } }
}
message Other {}
enum Color {
  option allow_alias = true;
  RED = 0;
  CRIMSON = 0;
  BLUE = -1 [deprecated = true];
  reserved -10 to -5;
}
service S {
  option deprecated = true;
  rpc Unary (M) returns (Other);
  rpc Both (stream M) returns (stream .a.b.Other) { option deprecated = true; };
}
"#,
    )
    .unwrap();
    assert_eq!(f.syntax, Syntax::Proto3);
    assert_eq!(f.package, "a.b");
    let kinds: Vec<_> = f
        .imports
        .iter()
        .map(|i| (i.path.as_str(), i.kind))
        .collect();
    assert_eq!(
        kinds,
        [
            ("x.proto", ImportKind::Normal),
            ("y.proto", ImportKind::Public),
            ("z.proto", ImportKind::Weak)
        ]
    );
    assert_eq!(f.options[1].name, "(my.ext).field");
    assert_eq!(f.options[1].value, Constant::Int(-5));
    assert_eq!(f.options[2].value, Constant::Bool(true));

    let m = &f.messages[0];
    assert_eq!(m.full_name, "a.b.M");
    assert_eq!(m.doc, "Doc for M.");
    assert!(m.deprecated());
    assert_eq!(m.fields.len(), 7);
    assert!(m.fields[0].deprecated());
    assert_eq!(m.fields[1].label, Label::Optional);
    assert_eq!(m.fields[2].label, Label::Repeated);
    assert_eq!(m.fields[2].packed(), Some(false));
    assert!(
        matches!(&m.fields[3].ty, FieldType::Map(ScalarType::String, v) if matches!(**v, FieldType::Named(_)))
    );
    assert!(matches!(&m.fields[4].ty, FieldType::Named(n) if n.name == ".a.b.Other"));
    assert_eq!(m.oneofs[0].name, "choice");
    assert_eq!(m.fields[5].oneof, Some(0));
    assert_eq!(m.fields[6].oneof, Some(0));
    assert!(m.reserved.contains_number(25) && m.reserved.contains_number(536_870_911));
    assert!(m.reserved.contains_name("older"));
    assert_eq!(m.messages[0].enums[0].full_name, "a.b.M.Inner.E");

    let e = &f.enums[0];
    assert!(e.allow_alias());
    assert_eq!(e.values[2].number, -1);
    assert!(e.values[2].deprecated());
    assert!(e.reserved.contains_number(-7));

    let s = &f.services[0];
    assert!(s.deprecated());
    assert!(!s.methods[0].client_streaming && !s.methods[0].server_streaming);
    assert!(s.methods[1].client_streaming && s.methods[1].server_streaming);
    assert!(s.methods[1].deprecated());
}

#[test]
fn parses_the_proto2_subset() {
    let f = parse(
        "p2.proto",
        r#"
syntax = "proto2";
message Job {
  required int64 id = 1;
  optional string name = 2 [default = "x"];
  repeated int32 tries = 3 [packed = true];
  extensions 100 to 199, 500;
}
extend Job { optional int32 extra = 100; }
"#,
    )
    .unwrap();
    assert_eq!(f.syntax, Syntax::Proto2);
    assert_eq!(f.messages[0].fields[0].label, Label::Required);
    assert_eq!(f.messages[0].extension_ranges, [(100, 199), (500, 500)]);
    assert_eq!(f.extensions[0].extendee, "Job");
    // No syntax statement means proto2.
    assert_eq!(
        parse("x.proto", "message A { optional int32 a = 1; }")
            .unwrap()
            .syntax,
        Syntax::Proto2
    );
}

#[test]
fn constants_and_aggregates() {
    let f = parse(
        "c.proto",
        r#"syntax = "proto3";
option a = inf;
option b = -inf;
option c = 1.5e3;
option d = 'single' "double";
option e = { key: "v" nested { x: [1, 2] } };
option f = 0x10;
option g = SOME.ENUM;
"#,
    )
    .unwrap();
    let values: Vec<&Constant> = f.options.iter().map(|o| &o.value).collect();
    assert_eq!(values[0], &Constant::Ident("inf".into()));
    assert_eq!(values[1], &Constant::Ident("-inf".into()));
    assert_eq!(values[2], &Constant::Float(1500.0));
    assert_eq!(values[3], &Constant::Str("singledouble".into()));
    assert!(matches!(values[4], Constant::Aggregate(s) if s.contains("nested")));
    assert_eq!(values[5], &Constant::Int(16));
    assert_eq!(values[6], &Constant::Ident("SOME.ENUM".into()));
}

#[test]
fn syntax_errors_carry_line_and_column() {
    assert_eq!(
        at("syntax = \"proto3\";\nmessage M {\n  int32 = 1;\n}"),
        (3, 9, "expected a field name, found `=`".into())
    );
    assert_eq!(
        at("syntax = \"proto4\";"),
        (
            1,
            10,
            "unknown syntax \"proto4\"; expected \"proto2\" or \"proto3\"".into()
        )
    );
    assert_eq!(
        at("message M {\n  int32 a = 1;\n"),
        (1, 1, "message `M` is not closed".into())
    );
    assert_eq!(
        at("syntax = \"proto3\";\nmessage M { required int32 a = 1; }"),
        (2, 13, "`required` is not allowed in proto3".into())
    );
    assert_eq!(
        at("syntax = \"proto2\";\nmessage M {\n  optional group G = 1 {}\n}"),
        (3, 12, "proto2 groups are not supported".into())
    );
    assert_eq!(
        at("edition = \"2023\";"),
        (
            1,
            1,
            "editions are not supported; use syntax = \"proto3\" or \"proto2\"".into()
        )
    );
    assert_eq!(
        at("syntax = \"proto3\";\nmessage M { map<float, int32> m = 1; }"),
        (
            2,
            17,
            "a map key must be an integer, bool or string type".into()
        )
    );
    assert_eq!(
        at("syntax = \"proto3\";\nmessage M { oneof o { repeated int32 a = 1; } }"),
        (2, 23, "a oneof member takes no label".into())
    );
    assert_eq!(
        at("syntax = \"proto3\";\nservice S { rpc A (int32) returns (M); }"),
        (
            2,
            20,
            "an rpc takes a message type, not the scalar `int32`".into()
        )
    );
    assert_eq!(
        at("syntax = \"proto3\";\npackage a;\npackage b;"),
        (3, 1, "a file has at most one package statement".into())
    );
    let e = err("syntax = \"proto3\";\nmessage M { string s = 1 [default = \"x\"]; }");
    assert_eq!(
        (e.line, e.message.as_str()),
        (2, "default values are not allowed in proto3")
    );
    assert_eq!(
        e.to_string(),
        "t.proto:2:13: default values are not allowed in proto3"
    );
}

#[test]
fn field_number_errors() {
    let p3 = |body: &str| at(&format!("syntax = \"proto3\";\nmessage M {{\n{body}\n}}"));
    assert_eq!(
        p3("  int32 a = 0;"),
        (
            3,
            13,
            "field number 0 is not allowed; field numbers start at 1".into()
        )
    );
    assert_eq!(
        p3("  int32 a = 19500;"),
        (3, 13, "field number 19500 is in the range 19000-19999 reserved for the protobuf implementation".into())
    );
    assert_eq!(
        p3("  int32 a = 536870912;"),
        (
            3,
            13,
            "field number 536870912 is above the maximum 536870911".into()
        )
    );
    assert_eq!(
        p3("  int32 a = -1;"),
        (3, 13, "a field number must be positive".into())
    );
    assert_eq!(
        p3("  int32 a = 1;\n  int32 b = 1;"),
        (4, 3, "field number 1 is already used by `a`".into())
    );
    assert_eq!(
        p3("  int32 a = 1;\n  string a = 2;"),
        (4, 3, "`a` is already defined on line 3".into())
    );
    assert_eq!(
        p3("  reserved 5 to 9;\n  int32 a = 7;"),
        (4, 3, "field `a` uses the reserved number 7".into())
    );
    assert_eq!(
        p3("  reserved \"a\";\n  int32 a = 7;"),
        (4, 3, "field name `a` is reserved".into())
    );
    assert_eq!(
        p3("  int32 a = 1 [packed = true];"),
        (
            3,
            3,
            "field `a`: `packed` applies only to repeated fields".into()
        )
    );
    assert_eq!(
        p3("  repeated string a = 1 [packed = true];"),
        (3, 3, "field `a`: a repeated string cannot be packed".into())
    );
    assert_eq!(
        p3("  reserved 9 to 5;"),
        (3, 12, "range 9 to 5 is empty".into())
    );
    assert_eq!(
        at("syntax = \"proto2\";\nmessage M {\n  int32 a = 1;\n}"),
        (
            3,
            3,
            "proto2 field `a` needs a label (`optional`, `required` or `repeated`)".into()
        )
    );
    assert_eq!(
        at("syntax = \"proto2\";\nmessage M {\n  extensions 10 to 20;\n  optional int32 a = 15;\n}"),
        (4, 3, "field number 15 is in an extension range".into())
    );
}

#[test]
fn enum_errors() {
    let p3 = |body: &str| at(&format!("syntax = \"proto3\";\nenum E {{\n{body}\n}}"));
    assert_eq!(
        p3("  A = 1;"),
        (3, 3, "the first value of proto3 enum `E` must be 0".into())
    );
    assert_eq!(
        p3("  A = 0;\n  B = 0;"),
        (
            4,
            3,
            "`B` reuses the number 0 of `A`; set `option allow_alias = true;` to allow aliases"
                .into()
        )
    );
    assert_eq!(
        p3("  A = 0;\n  A = 1;"),
        (4, 3, "`A` is already defined on line 3".into())
    );
    assert_eq!(
        p3("  A = 0;\n  B = 3000000000;"),
        (4, 7, "enum value 3000000000 does not fit in int32".into())
    );
    assert_eq!(
        p3("  A = 0;\n  reserved 2;\n  B = 2;"),
        (5, 3, "enum value `B` uses the reserved number 2".into())
    );
    assert_eq!(
        at("syntax = \"proto3\";\nenum E {}"),
        (2, 1, "enum `E` has no values".into())
    );
}

#[test]
fn lexical_errors_carry_positions() {
    assert_eq!(
        at("message M {\n  string s = 1; # x\n}"),
        (2, 17, "unexpected character '#'".into())
    );
    assert_eq!(
        at("option x = \"abc\n\";"),
        (1, 12, "unterminated string (newline in string)".into())
    );
    assert_eq!(
        at("/* never closed"),
        (1, 1, "unterminated block comment".into())
    );
}

#[test]
fn hostile_sources_are_bounded() {
    // Deeply nested messages.
    let deep = "message A { ".repeat(200) + &"}".repeat(200);
    assert!(err(&deep).message.starts_with("nesting deeper than 64"));
    // Deeply nested aggregate option values.
    let agg = format!("option x = {}{};", "{ a ".repeat(200), "}".repeat(200));
    assert!(err(&agg).message.starts_with("nesting deeper than 64"));
    // Truncations of a valid file never panic.
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/proto_data/sample/v1/sample.proto"
    ))
    .unwrap();
    for end in 0..src.len() {
        if src.is_char_boundary(end) {
            let _ = parse("t.proto", &src[..end]);
        }
    }
}

#[test]
fn resolves_names_across_scopes_and_files() {
    let loader = MemoryLoader::new()
        .with(
            "main.proto",
            r#"syntax = "proto3";
package app.v1;
import "lib/types.proto";
message Outer {
  message Inner { Kind kind = 1; }
  enum Kind { KIND_A = 0; }
  Inner inner = 1;
  Outer.Inner by_path = 2;
  lib.Shared shared = 3;
  .lib.Shared absolute = 4;
  map<string, Inner> map = 5;
  Top top = 6;
}
message Top {}
service Api { rpc Get (Outer) returns (lib.Shared); }
"#,
        )
        .with(
            "lib/types.proto",
            "syntax = \"proto3\";\npackage lib;\nmessage Shared {}\n",
        );
    let set = FileSet::load(&loader, &["main.proto"]).unwrap();
    // Dependencies come first.
    let names: Vec<&str> = set.files().iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["lib/types.proto", "main.proto"]);
    let outer = set.message("app.v1.Outer").unwrap();
    let target = |i: usize| match &outer.fields[i].ty {
        FieldType::Named(n) => n.target.clone().unwrap(),
        FieldType::Map(_, v) => match &**v {
            FieldType::Named(n) => n.target.clone().unwrap(),
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };
    assert_eq!(target(0).full_name, "app.v1.Outer.Inner");
    assert_eq!(target(1).full_name, "app.v1.Outer.Inner");
    assert_eq!(target(2).full_name, "lib.Shared");
    assert_eq!(target(3).full_name, "lib.Shared");
    assert_eq!(target(4).full_name, "app.v1.Outer.Inner");
    assert_eq!(target(5).full_name, "app.v1.Top");
    let inner = set.message("app.v1.Outer.Inner").unwrap();
    match &inner.fields[0].ty {
        FieldType::Named(n) => assert_eq!(n.target.as_ref().unwrap().kind, TypeKind::Enum),
        _ => unreachable!(),
    }
    assert!(set.enumeration("app.v1.Outer.Kind").is_some());
    assert!(set.message("app.v1.Missing").is_none());
    let (file, svc) = set.services().next().unwrap();
    assert_eq!(file.name, "main.proto");
    assert_eq!(
        svc.methods[0].output.target.as_ref().unwrap().full_name,
        "lib.Shared"
    );
}

#[test]
fn resolution_errors() {
    let e = FileSet::from_sources(&[(
        "m.proto",
        "syntax = \"proto3\";\nmessage M {\n  Nope n = 1;\n}",
    )])
    .unwrap_err();
    assert_eq!(e.to_string(), "m.proto:3:3: unknown type `Nope`");

    let e = FileSet::from_sources(&[(
        "m.proto",
        "syntax = \"proto3\";\nenum E { A = 0; }\nservice S {\n  rpc X (E) returns (E);\n}",
    )])
    .unwrap_err();
    assert_eq!((e.line, e.column), (4, 3));
    assert!(e.message.contains("uses the enum `E`"), "{e}");

    let e = FileSet::from_sources(&[(
        "m.proto",
        "syntax = \"proto3\";\nmessage A {}\nmessage M {\n  repeated A a = 1 [packed = true];\n}",
    )])
    .unwrap_err();
    assert_eq!(e.line, 4);
    assert!(e.message.contains("[packed = true] applies only"), "{e}");

    // The same name defined in two files.
    let e = FileSet::from_sources(&[
        (
            "a.proto",
            "syntax = \"proto3\";\npackage p;\nmessage Dup {}",
        ),
        (
            "b.proto",
            "syntax = \"proto3\";\npackage p;\n\nmessage Dup {}",
        ),
    ])
    .unwrap_err();
    assert_eq!(
        e.to_string(),
        "b.proto:4:1: `p.Dup` is already defined in a.proto"
    );
}

#[test]
fn import_errors_point_at_the_import() {
    let loader = MemoryLoader::new().with(
        "a.proto",
        "syntax = \"proto3\";\n\nimport \"missing.proto\";\n",
    );
    let e = FileSet::load(&loader, &["a.proto"]).unwrap_err();
    assert_eq!((e.file.as_str(), e.line, e.column), ("a.proto", 3, 1));
    assert!(
        e.message.starts_with("cannot load \"missing.proto\""),
        "{e}"
    );

    // A weak import may be missing.
    let loader = MemoryLoader::new().with(
        "a.proto",
        "syntax = \"proto3\";\nimport weak \"missing.proto\";\n",
    );
    assert!(FileSet::load(&loader, &["a.proto"]).is_ok());

    let loader = MemoryLoader::new()
        .with("a.proto", "import \"b.proto\";")
        .with("b.proto", "import \"a.proto\";");
    let e = FileSet::load(&loader, &["a.proto"]).unwrap_err();
    assert_eq!(e.message, "import cycle: a.proto -> b.proto -> a.proto");

    // A parse error in an imported file names that file.
    let loader = MemoryLoader::new()
        .with("a.proto", "import \"b.proto\";")
        .with("b.proto", "message {");
    assert_eq!(
        FileSet::load(&loader, &["a.proto"]).unwrap_err().file,
        "b.proto"
    );
}

#[test]
fn well_known_types_are_built_in_and_loaders_compose() {
    let src = r#"syntax = "proto3";
import "google/protobuf/timestamp.proto";
import "google/protobuf/struct.proto";
import "google/protobuf/wrappers.proto";
import "google/protobuf/any.proto";
import "google/protobuf/duration.proto";
import "google/protobuf/empty.proto";
import "google/protobuf/field_mask.proto";
message M {
  google.protobuf.Timestamp at = 1;
  google.protobuf.Struct meta = 2;
  google.protobuf.Int64Value maybe = 3;
}"#;
    let set = FileSet::from_sources(&[("m.proto", src)]).unwrap();
    assert!(set.message("google.protobuf.Value").is_some());
    assert!(set.enumeration("google.protobuf.NullValue").is_some());

    // A closure is a loader.
    let loader = |path: &str| -> io::Result<String> {
        if path == "only.proto" {
            Ok("syntax = \"proto3\"; message Only {}".into())
        } else {
            Err(io::Error::new(io::ErrorKind::NotFound, "nope"))
        }
    };
    assert!(FileSet::load(&loader, &["only.proto"])
        .unwrap()
        .message("Only")
        .is_some());
}

#[test]
fn fs_loader_stays_inside_its_include_paths() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/proto_data");
    let loader = FsLoader::new([root]);
    assert!(FileSet::load(&loader, &["sample/v1/sample.proto"]).is_ok());
    use ferroly::proto::idl::Loader;
    let escape = loader.load("../proto_idl.rs").unwrap_err();
    assert_eq!(escape.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(
        loader.load("/etc/hosts").unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        loader.load("nope.proto").unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    assert_eq!(loader.include_paths().len(), 1);
}

#[test]
fn block_comments_with_many_blank_lines_lex_in_linear_time() {
    // 200k leading blank lines in one doc comment took 11 s when each was
    // removed from the front of a vector; it is now one drain.
    let src = format!("/*{}x*/ message M {{}}", "\n".repeat(200_000));
    let start = std::time::Instant::now();
    let f = parse("q.proto", &src).unwrap();
    assert_eq!(f.messages[0].doc, "x");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "took {:?}",
        start.elapsed()
    );
}

#[test]
fn adjacent_strings_concatenate_across_comments() {
    let f = parse(
        "s.proto",
        "syntax = \"proto3\"; option x = \"a\" /* c */ \"b\" // d\n 'c';\n// doc\nmessage M {}",
    )
    .unwrap();
    assert_eq!(f.options[0].value, Constant::Str("abc".into()));
    assert_eq!(f.messages[0].doc, "doc");
}

#[test]
fn rejects_what_protoc_rejects() {
    // Octal escapes are one byte.
    assert_eq!(
        at("option y = \"\\777\";").2,
        "octal escape is above \\377 (one byte)"
    );
    assert!(parse("e.proto", "option y = \"\\377\";").is_ok());

    // `packed` takes a bool.
    assert_eq!(
        at("syntax = \"proto3\";\nmessage M { repeated int32 a = 1 [packed = 1]; }").2,
        "field `a`: `packed` takes `true` or `false`"
    );

    // Enum values are siblings of their enum: they clash with each other
    // across enums, and with messages, in the same scope.
    assert!(
        err("syntax = \"proto3\"; enum A { X = 0; } enum B { X = 0; }")
            .message
            .contains("`X` is already defined")
    );
    assert!(err("syntax = \"proto3\"; enum A { M = 0; } message M {}")
        .message
        .contains("`M` is already defined"));
    assert!(
        err("syntax = \"proto3\"; message O { enum A { X = 0; } enum B { X = 0; } }")
            .message
            .contains("`X` is already defined")
    );

    // `allow_alias` needs an alias.
    assert_eq!(
        at("syntax = \"proto3\";\nenum E { option allow_alias = true; A = 0; B = 1; }").2,
        "enum `E` sets `option allow_alias = true;` but has no aliases"
    );
    assert!(parse(
        "e.proto",
        "syntax = \"proto3\"; enum E { option allow_alias = true; A = 0; B = 0; }"
    )
    .is_ok());

    // A proto3 message cannot use a closed (proto2) enum.
    let e = FileSet::from_sources(&[
        (
            "a.proto",
            "syntax = 'proto2'; package p; enum E { ONE = 1; }",
        ),
        (
            "b.proto",
            "syntax = 'proto3'; package p; import 'a.proto'; message M { E e = 1; }",
        ),
    ])
    .unwrap_err();
    assert!(e.message.contains("proto2 (closed) enum"), "{}", e.message);
    // proto2 messages may.
    assert!(FileSet::from_sources(&[
        (
            "a.proto",
            "syntax = 'proto2'; package p; enum E { ONE = 1; }"
        ),
        (
            "b.proto",
            "syntax = 'proto2'; package p; import 'a.proto'; message M { optional E e = 1; }",
        ),
    ])
    .is_ok());
}

#[test]
fn dotted_names_bind_their_first_part_to_the_innermost_scope() {
    // `bar.Qux` inside package `p`: `bar` is `p.bar` (a message), which has
    // no `Qux`. protoc reports it rather than trying the top-level `bar.Qux`.
    let e = FileSet::from_sources(&[
        (
            "a.proto",
            "syntax='proto3'; package p; import 'b.proto'; message bar {} message M { bar.Qux x = 1; }",
        ),
        ("b.proto", "syntax='proto3'; package bar; message Qux {}"),
    ])
    .unwrap_err();
    assert!(
        e.message.contains("`bar` refers to `p.bar`"),
        "{}",
        e.message
    );

    // Without the shadowing message, `bar.Qux` reaches the package.
    let set = FileSet::from_sources(&[
        (
            "a.proto",
            "syntax='proto3'; package p; import 'b.proto'; message M { bar.Qux x = 1; }",
        ),
        ("b.proto", "syntax='proto3'; package bar; message Qux {}"),
    ])
    .unwrap();
    let FieldType::Named(n) = &set.message("p.M").unwrap().fields[0].ty else {
        panic!("named");
    };
    assert_eq!(n.target.as_ref().unwrap().full_name, "bar.Qux");

    // A package prefix binds too: inside `a.b`, `b.T` is `a.b.T`.
    let set = FileSet::from_sources(&[(
        "x.proto",
        "syntax='proto3'; package a.b; message T {} message M { b.T t = 1; }",
    )])
    .unwrap();
    let FieldType::Named(n) = &set.message("a.b.M").unwrap().fields[0].ty else {
        panic!("named");
    };
    assert_eq!(n.target.as_ref().unwrap().full_name, "a.b.T");
}

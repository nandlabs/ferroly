# `ferroly::proto` — Protocol Buffers

A hand-written Protocol Buffers implementation: the binary wire format, a
`#[derive(Message)]` that gives a Rust struct its encoding, a `.proto` parser
(proto3 and the common proto2 subset) and a code generator for a consumer's
`build.rs`. There is **no `protoc`**, no reflection and no external crate — the
generated code is plain Rust that uses the derives.

Enable with the `protobuf` feature (std-only; no other feature or crate
required):

```toml
ferroly = { version = "0.7", features = ["protobuf"] }
```

The terminology follows the rest of Ferroly: messages are **encoded** and
**decoded**.

## Contents

| Area | Items |
|---|---|
| [Quick start](#quick-start) | a derived message, encoded and decoded |
| [Wire format support](#wire-format-support) | what is implemented, per protobuf feature |
| [The derives](#the-derives) | `Message`, `Oneof`, `Enumeration` and their attributes |
| [The `Message` trait](#the-message-trait) | `encode`, `encoded_len`, `decode`, `merge`, length-delimited framing |
| [Open enums and unknown fields](#open-enums-and-unknown-fields) | `EnumValue<E>`, `UnknownFields` |
| [The `wire` module](#the-wire-module) | varints, zig-zag, keys, the bounded `Reader` |
| [`.proto` files](#proto-files) | `idl::parse`, `FileSet::load`, loaders, what is accepted |
| [Code generation](#code-generation) | `codegen::Config`, the `build.rs` recipe, the Rust mapping |
| [Services](#services) | `ServiceDescriptor`, `MethodDescriptor`, and the `ServiceGenerator` extension point |
| [Limits and untrusted input](#limits-and-untrusted-input) | `Limits`, what is bounded |
| [Errors](#errors) | `ProtoError`, `IdlError` |
| [How it is tested](#how-it-is-tested) | vectors, hostile input, fuzzing, golden codegen |
| [Not supported](#not-supported) | groups, extensions, editions, JSON mapping, … |

## Quick start

```rust
use ferroly::proto::{EnumValue, Message, UnknownFields};
use std::collections::BTreeMap;

#[derive(ferroly::proto::Enumeration, Clone, Copy, PartialEq, Debug)]
enum Role {
    #[proto(name = "ROLE_USER")]
    User = 0,
    #[proto(name = "ROLE_ADMIN")]
    Admin = 1,
}

#[derive(ferroly::proto::Oneof, Clone, PartialEq, Debug)]
enum Contact {
    #[proto(tag = 8)]
    Email(String),
    #[proto(tag = 9, fixed64)]
    Phone(u64),
}

#[derive(ferroly::proto::Message, Clone, Default, PartialEq, Debug)]
struct Account {
    #[proto(tag = 1)]
    id: u64,
    #[proto(tag = 2)]
    name: String,
    #[proto(tag = 3, sint64)]
    balance: i64,                       // zig-zag: small negatives stay small
    #[proto(tag = 4)]
    scores: Vec<i32>,                   // repeated numbers are packed
    #[proto(tag = 5)]
    role: EnumValue<Role>,              // open enum
    #[proto(tag = 6, map(key = "string", value = "int32"))]
    limits: BTreeMap<String, i32>,
    #[proto(tag = 7)]
    nickname: Option<String>,           // explicit presence (proto3 `optional`)
    #[proto(oneof)]
    contact: Option<Contact>,
    #[proto(unknown)]
    unknown: UnknownFields,             // fields from newer writers
}

let account = Account {
    id: 150,
    name: "ada".into(),
    balance: -1,
    role: Role::Admin.into(),
    contact: Some(Contact::Email("ada@example.com".into())),
    ..Account::default()
};
let bytes = account.encode_to_vec();
assert_eq!(&bytes[..3], [0x08, 0x96, 0x01]);  // field 1 = 150
assert_eq!(Account::decode(&bytes).unwrap(), account);
```

## Wire format support

| Feature | Support |
|---|---|
| varint (`int32`, `int64`, `uint32`, `uint64`, `bool`, enums) | yes; negative `int32` / enum values are written as ten-byte varints and read back from either ten or five bytes; 64-bit varints are truncated into 32-bit fields as protoc does |
| zig-zag (`sint32`, `sint64`) | yes |
| fixed width (`fixed32`, `fixed64`, `sfixed32`, `sfixed64`, `float`, `double`) | yes, little-endian |
| `string` | yes; decoding validates UTF-8 (`ProtoError::InvalidUtf8`) |
| `bytes` | yes (`Vec<u8>`) |
| embedded messages | yes; a message field seen twice is **merged**, as the spec requires |
| repeated, packed | numeric, bool and enum fields are packed by default (proto3); `packed = false` writes one key per element |
| repeated, decoding | packed **and** unpacked input are both accepted for every packable field |
| `map<K, V>` | yes, as repeated entry messages (key = 1, value = 2); both are always written; a missing key or value decodes as its default; a repeated key keeps the last value |
| `oneof` | yes; members have explicit presence (written even when default); the last member seen wins; a message member seen twice is merged |
| proto3 implicit presence | yes; defaults (0, `false`, `""`, empty bytes, enum 0) are not written; `-0.0` is written |
| proto3 `optional` / explicit presence | yes, as `Option<T>` |
| proto2 `optional` / `required` | yes: `Option<T>`; `required` fields are always written, and decoding fails when one is absent from every occurrence of its message (a sub-message split across occurrences is merged first; a replaced oneof variant no longer counts) |
| enums | open: unknown numbers are kept in `EnumValue<E>` and written back unchanged |
| unknown fields | kept in an `UnknownFields` member (varint, fixed32, fixed64, length-delimited and groups) and written back after the known fields |
| wire-type mismatch | a known field number with an unexpected wire type is kept as an unknown field, as the reference implementations do |
| groups (wire types 3/4) | skipped and preserved as unknown fields; group **fields** are not supported |
| extensions | not supported (extension fields arrive as unknown fields and are preserved) |

## The derives

The three derives are re-exported from `ferroly::proto` under the trait names,
so `#[derive(ferroly::proto::Message)]` (or `use ferroly::proto::Message;` then
`#[derive(Message)]`) brings both the trait and its derive into scope.

### `#[derive(Message)]`

For a struct with named fields (no generics). The type must also implement
`Default` (derive it). Every field carries `#[proto(...)]`:

| Attribute | Meaning |
|---|---|
| `tag = N` | the field number: 1 to 536 870 911, outside 19000–19999, unique in the message |
| a type: `int32`, `int64`, `uint32`, `uint64`, `sint32`, `sint64`, `fixed32`, `fixed64`, `sfixed32`, `sfixed64`, `float`, `double`, `bool`, `string`, `bytes`, `message`, `enumeration` | the protobuf type, when the Rust type does not imply it |
| `packed = false` (or `packed`, `packed = true`) | packing of a repeated numeric, bool or enum field (packed is the default) |
| `map(key = "…", value = "…")` | the key and value types of a `HashMap` / `BTreeMap` field (inferred from the Rust types when omitted); `value = "message"` / `"enumeration"` for message and enum values |
| `required` | proto2 `required`: always written; decoding fails with `MissingRequired` when absent (at most 64 per message) |
| `oneof` | on an `Option<E>` field where `E` derives `Oneof`; takes no tag |
| `unknown` | on one `UnknownFields` field: keep and re-emit undeclared fields (without it they are skipped) |
| `skip` | not encoded; decoding leaves it at its `Default` value |

The **shape** comes from the Rust type:

| Rust type | Shape |
|---|---|
| `T` | singular, implicit presence (not written when default). A plain message type `M` is always written. |
| `Option<T>` | singular, explicit presence (written when `Some`, even if default) |
| `Vec<T>` (but not `Vec<u8>`) | repeated |
| `Vec<u8>` | `bytes` |
| `HashMap<K, V>` / `BTreeMap<K, V>` | `map<K, V>` |
| `Box<M>` / `Option<Box<M>>` | a message, for recursive types |

The **protobuf type** is inferred when no type attribute is given: `i32` →
`int32`, `i64` → `int64`, `u32` → `uint32`, `u64` → `uint64`, `f32` → `float`,
`f64` → `double`, `bool` → `bool`, `String` → `string`, `Vec<u8>` → `bytes`,
`EnumValue<E>` → enum, anything else → message. Give a type attribute to pick
`sint*`, `fixed*` or `sfixed*`, or when a type alias hides the type.

Fields are written in declaration order, followed by the unknown fields.
Generated code declares them in field-number order (a oneof at its lowest
member's number), which is the order protoc and the other runtimes write, so
re-encoded bytes match theirs; only a oneof whose members' numbers straddle
other fields is written as one unit at that position.

The `Enumeration` derive rejects, at compile time, a discriminant outside
`i32` (the type of protobuf enum numbers), which `as i32` would otherwise
truncate into a collision.

### `#[derive(Oneof)]`

For an enum of newtype variants, each with `#[proto(tag = N)]` and an optional
type attribute. The containing message holds it as `Option<E>` marked
`#[proto(oneof)]`. A variant holds a single value (no `Option`, `Vec` or map);
use `Box<M>` for a recursive message. A oneof that reuses a field number of its
message (or of another oneof in it) fails a compile-time assertion.

### `#[derive(Enumeration)]`

For a fieldless enum whose discriminants are the enum numbers (negative values
are fine). It must also derive `Clone, Copy`. `#[proto(name = "…")]` on a
variant sets its `.proto` name (returned by `proto_name`, accepted by
`from_proto_name`); the Rust name is used otherwise.

### Compile errors

Following the rest of `ferroly-derive`, nothing is silently ignored. These are
compile errors that point at the offending tokens:

- a missing `tag` (use `skip` for fields that are not encoded), tag 0, a tag
  above 2²⁹ − 1, a tag in 19000–19999, or a tag used twice;
- unknown or repeated attribute keys, two type attributes on one field;
- `packed` on a non-repeated field or on a string, bytes or message field;
- `map(…)` on a field that is not a `HashMap` / `BTreeMap`, or a map key that
  is a float, bytes, message or enum;
- `required` on an `Option`, `Vec` or map; `oneof` on anything but `Option<E>`,
  or with a tag; `unknown` or `skip` combined with other keys; two `unknown`
  fields;
- Rust integer types protobuf does not have (`u8`, `i16`, `usize`, …) and
  reference fields;
- generics, tuple structs, enums for `Message`; non-newtype variants for
  `Oneof`; data-carrying variants, duplicate names or an empty enum for
  `Enumeration`.

A field type that does not fit its type attribute (say `#[proto(sint32)]` on a
`String`) is reported by the compiler as a missing trait implementation.

## The `Message` trait

```rust,ignore
pub trait Message: Default {
    // Required (the derive implements these):
    fn encode_raw(&self, buf: &mut Vec<u8>);
    fn encoded_len(&self) -> usize;
    fn merge_from(&mut self, r: &mut wire::Reader<'_>) -> Result<(), ProtoError>;

    // Provided:
    fn encode(&self, buf: &mut Vec<u8>);                 // append
    fn encode_to_vec(&self) -> Vec<u8>;
    fn encode_length_delimited(&self, buf: &mut Vec<u8>); // varint length + message
    fn decode(bytes: &[u8]) -> Result<Self, ProtoError>;  // default Limits
    fn decode_with(bytes: &[u8], limits: Limits) -> Result<Self, ProtoError>;
    fn decode_length_delimited(bytes: &[u8]) -> Result<Self, ProtoError>;
    fn decode_length_delimited_with(bytes: &[u8], limits: Limits) -> Result<Self, ProtoError>;
    fn merge(&mut self, bytes: &[u8]) -> Result<(), ProtoError>;
    fn merge_with(&mut self, bytes: &[u8], limits: Limits) -> Result<(), ProtoError>;
    fn clear(&mut self);
}
```

`merge` has the protobuf semantics of concatenation: scalars are replaced,
embedded messages merged, repeated fields and maps appended to. `Box<M>`
implements `Message` when `M` does. Encoding cannot fail; `encoded_len` is exact,
so `encode` reserves once.

Encoding is linear in the output at any nesting depth: the provided encoders
(and a derived `encode_raw`) run one length pass that records each embedded
message's length, then a write pass that reads them back, instead of measuring
every subtree again at every level.

proto2 `required` fields are checked when the whole input has been read:
`merge_from` reports what an occurrence lacked to the `Reader`, and
`decode`, `decode_with` and the length-delimited decoders fail with
`MissingRequired` only for a field no occurrence of its message carried (so
`0a02 0801 0a02 1002`, one sub-message in two pieces, decodes). Each element
of a repeated or map field is checked on its own. A oneof variant replaced by
another one no longer counts, together with everything it owned (boxed or
inline sub-messages): only the final message is checked, so `A{x}`, then
another variant, then `A{y}` fails for the missing `x`, and an incomplete
variant that is later replaced does not fail. The reader keeps one record per
live value with required fields, so the check is linear in the input however
the occurrences are interleaved. `merge` and `merge_with` do
not check the target message or its singular sub-messages, which may already
hold the fields; new repeated and map elements are checked. A transport that
drives `merge_from` on a `Reader` itself calls `Reader::finish_required`
afterwards.

A transport that frames messages itself (gRPC's 5-byte prefix, a file of
length-delimited records) calls `encoded_len` for the prefix, then
`encode_raw` into the same buffer, and decodes with `decode_with` and its own
`Limits`.

## Open enums and unknown fields

`EnumValue<E>` stores the raw `i32` and views it as `E` when the number is
known — so a value added to the enum by a newer writer survives a decode and
re-encode:

```rust
use ferroly::proto::{EnumValue, Enumeration};

#[derive(ferroly::proto::Enumeration, Clone, Copy, PartialEq, Debug)]
enum Color { Red = 0, Green = 1 }

let v: EnumValue<Color> = Color::Green.into();
assert_eq!(v.known(), Some(Color::Green));
assert!(v == Color::Green);
let future = EnumValue::<Color>::from_raw(7);
assert_eq!((future.known(), future.raw()), (None, 7));
assert_eq!(Color::from_i32(1), Some(Color::Green));
```

`UnknownFields` keeps what a type does not declare, in arrival order, with
`len`, `iter`, `get(tag)`, `push` and `clear`; each entry is an
`UnknownField { tag, value: UnknownValue }` where `UnknownValue` is `Varint`,
`Fixed64`, `Fixed32`, `LengthDelimited(Vec<u8>)` or `Group(Vec<u8>)`.

```rust
use ferroly::proto::{Message, UnknownFields};

#[derive(ferroly::proto::Message, Default, Debug)]
struct Old {
    #[proto(tag = 1)]
    id: u64,
    #[proto(unknown)]
    unknown: UnknownFields,
}

let newer = [0x08, 0x07, 0x12, 0x02, b'h', b'i']; // a newer writer added field 2
let old = Old::decode(&newer).unwrap();
assert_eq!(old.unknown.len(), 1);
assert_eq!(old.encode_to_vec(), newer);
```

## The `wire` module

The primitives the derive expands to, public for hand-written messages and
transports: `encode_varint`, `decode_varint`, `varint_len`, `encode_key`,
`key_len`, `zigzag_encode32/64`, `zigzag_decode32/64`, `is_valid_tag`,
`WireType`, the constants `MAX_TAG`, `RESERVED_TAGS_START/END` and
`MAX_VARINT_LEN`, and `Reader` — a bounded cursor with `read_key`,
`read_varint`, `read_fixed32/64`, `read_bytes`, `read_nested`, `read_packed`,
`skip_field` and `skip_group`, every read checked against the remaining input.

```rust
use ferroly::proto::wire::{self, Reader, WireType};

let mut buf = Vec::new();
wire::encode_key(1, WireType::Varint, &mut buf);
wire::encode_varint(150, &mut buf);
assert_eq!(buf, [0x08, 0x96, 0x01]);

let mut r = Reader::new(&buf);
assert_eq!(r.read_key().unwrap(), Some((1, WireType::Varint)));
assert_eq!(r.read_varint().unwrap(), 150);
```

## `.proto` files

`ferroly::proto::idl` parses `.proto` text into plain data (`ProtoFile`,
`MessageDef`, `FieldDef`, `EnumDef`, `ServiceDef`, `MethodDef`, …), keeping
each definition's position and the comment above it.

- `idl::parse(name, source)` parses one file and checks it on its own.
- `FileSet::load(&loader, &["a.proto"])` parses the roots and everything they
  import, checks the whole set, and resolves every message, enum and method
  type to its fully qualified name (`NamedType::target`). Files come back
  dependencies first. `FileSet::from_sources(&[(path, source)])` is the
  in-memory shortcut; `message(name)`, `enumeration(name)`, `file(name)` and
  `services()` look things up.
- A `Loader` returns a file's source for an import path. `FsLoader::new(dirs)`
  searches include directories in order like `protoc -I` (import paths must be
  relative and may not contain `..`); `MemoryLoader` holds sources in memory;
  any `Fn(&str) -> io::Result<String>` is a loader too. The well-known types
  (`google/protobuf/{any,duration,empty,field_mask,struct,timestamp,wrappers}.proto`)
  are built in and used when the loader does not have them.

```rust
use ferroly::proto::idl::{FileSet, Label, MemoryLoader};

let loader = MemoryLoader::new().with("search.proto", r#"
    syntax = "proto3";
    package search.v1;
    message SearchRequest {
      string query = 1;
      optional int32 page = 2;
      repeated string tags = 3;
    }
"#);
let set = FileSet::load(&loader, &["search.proto"]).unwrap();
let req = set.message("search.v1.SearchRequest").unwrap();
assert_eq!(req.fields[1].label, Label::Optional);
```

### What the parser accepts

| Construct | Notes |
|---|---|
| `syntax = "proto3"` / `"proto2"` | absent means proto2; `edition = …` is rejected |
| `package a.b;` | at most one |
| `import "x.proto";`, `import public`, `import weak` | resolved through the loader; a missing weak import is ignored; cycles are errors |
| `option name = value;` at file, message, enum, service and method level; `[name = value, …]` on fields and enum values | names may be `(custom.ext).field`; values are identifiers, signed numbers, `inf`/`nan`, strings (adjacent literals concatenate) or `{ … }` aggregates (kept as text) |
| `message`, nested to 64 levels | |
| fields with no label (proto3), `optional`, `required` (proto2 only), `repeated` | proto2 fields need a label |
| `map<K, V>` | key: an integer, bool or string type; not inside a oneof |
| `oneof name { … }` | members take no label; `option` statements allowed |
| `enum` with `option allow_alias = true;` | proto3 enums must start at 0; duplicate numbers need `allow_alias` |
| `reserved 2, 9 to 11, 40 to max;` and `reserved "a", "b";` | checked against the fields and enum values |
| `extensions 100 to 199;` | recorded; fields in the range are rejected |
| `extend Foo { … }` | parsed and kept in `ProtoFile::extensions`; nothing is generated |
| `service` / `rpc Name (stream Req) returns (stream Resp);` | with or without an `{ option …; }` body |
| field options `packed`, `deprecated`, `default` (proto2), `json_name` | `packed` and `deprecated` affect generated code; others are kept |
| `//` and `/* */` comments | the comment block directly above a definition becomes its `doc` |

Checks with a line and column: field numbers (0, above 2²⁹ − 1, 19000–19999,
duplicates), reserved numbers and names, duplicate names in a scope, proto3
`required` and `default`, `packed` on non-repeated or non-packable fields or
with a non-bool value, enum rules (enum values are scoped like C++
enumerators, so they may not clash with a sibling enum's values or a type in
the same scope; `allow_alias` needs an alias), unknown types, proto3 messages
using proto2 enums, enums used as rpc types, names defined twice across files,
and octal escapes above `\377`. A dotted relative name binds its first part
to the innermost scope that defines it, as protoc does: inside package `p`
with a message `p.bar`, `bar.Qux` must be `p.bar.Qux`. Comments may appear
between concatenated string literals. Errors display as
`file:line:column: message`.

## Code generation

`ferroly::proto::codegen` turns a `FileSet` into Rust source that uses the
derives. It needs no `protoc`: put `ferroly` (with `protobuf`) in
`[build-dependencies]` and call it from `build.rs`.

```toml
# Cargo.toml
[dependencies]
ferroly = { version = "0.7", features = ["protobuf"] }

[build-dependencies]
ferroly = { version = "0.7", default-features = false, features = ["protobuf"] }
```

```rust,no_run
// build.rs
fn main() {
    let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("protos.rs");
    ferroly::proto::codegen::Config::new()
        // .map_type(ferroly::proto::codegen::MapType::HashMap)
        // .type_attribute(".my.pkg.Key", "#[derive(Eq, Hash)]")
        .compile_to_file(&["proto"], &["my/pkg/service.proto"], &out)
        .unwrap_or_else(|e| panic!("{e}"));
}
```

```rust,ignore
// src/lib.rs
pub mod protos {
    include!(concat!(env!("OUT_DIR"), "/protos.rs"));
}

use protos::my::pkg::{GetRequest, SERVICE_DESCRIPTOR};
```

`compile_to_file` writes only when the output changed, so dependents are not
rebuilt needlessly, and, run from a build script, prints
`cargo:rerun-if-changed=` for every `.proto` file it read, so the build script
reruns exactly when one of them changes (`FsLoader::locate` gives a file's
path). `Config::compile(&loader, roots)` and
`Config::generate(&file_set)` return the text instead; `codegen::generate(&set)`
uses the defaults.

### The mapping

| `.proto` | Rust |
|---|---|
| `package a.b;` | `pub mod a { pub mod b { … } }` (no package: the root) |
| `message Foo` | `pub struct Foo` deriving `Clone, PartialEq, Debug, Default, Message`, with a trailing `unknown_fields: UnknownFields` |
| nested `message Bar` / `enum E` in `Foo` | `foo::Bar`, `foo::E` |
| proto3 scalar field | `T` |
| `optional` field, proto2 optional field | `Option<T>` |
| message field | `Option<M>`; `Option<Box<M>>` when `M` (transitively) contains the field's message through singular fields |
| enum field | `EnumValue<E>` |
| `repeated T` | `Vec<T>` (proto2 fields get `packed = false` unless `[packed = true]`) |
| `map<K, V>` | `BTreeMap<K, V>` by default (deterministic encoding order) or `HashMap` with `MapType::HashMap` |
| `oneof kind { … }` | a field `kind: Option<foo::Kind>` and `pub enum foo::Kind` deriving `Oneof` (`KindOneof` if a nested type is already called `Kind`); `type_attribute` paths reach it as `.pkg.Foo.kind` |
| proto2 `required` | `T` with `#[proto(required)]` |
| `enum Corpus { CORPUS_WEB = 1; }` | `#[repr(i32)] pub enum Corpus { Web = 1 }` deriving `Enumeration` plus `Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug`; the shared `ENUM_NAME_` prefix is stripped when every value has it; aliases become associated constants |
| `service Greeter` | `pub const GREETER_DESCRIPTOR: ServiceDescriptor`, then the output of each registered `ServiceGenerator` |
| comments | `///` docs (Markdown-escaped; bare URLs become `<…>` autolinks, tabs become spaces and list continuations are indented, so they produce no rustdoc or clippy warnings and no doctests); a fallback doc otherwise; `deprecated` adds a note |
| fields | struct members in field-number order, so the encoding matches protoc's |

Names: fields become `snake_case`, messages, enums and variants `PascalCase`;
Rust keywords are written raw (`r#type`) and `self`/`super`/`crate`/`Self` get
a trailing `_`. Two definitions that map to the same Rust name in one module
are an error naming both: fields and oneof variants; messages, enums and
oneof enums (`HTTPRequest` and `HttpRequest`, `foo` and `Foo`); a message's
nested-type module and a package module (package `a.b` next to `a.B`'s nested
types), packages that differ only in case (`fooBar`, `foo_bar`); service
descriptor constants; and the names a `ServiceGenerator` reserves. Every
module, including the intermediate modules of a dotted package, carries a doc,
so the output compiles under a consumer's `#![deny(missing_docs)]`. Types for
every file in the set — imports included — go
into one module tree, and cross-package references are relative
(`super::super::common::Label`), so the output works wherever it is included.
The output is deterministic.

`Config` options: `map_type(MapType)`, `service_descriptors(bool)`,
`service_generator(impl ServiceGenerator)`, `type_attribute(path, attr)` (path
`.` for everything, `.pkg` for a package, `.pkg.Msg` for a type and its nested
types and oneof enums, `.pkg.Msg.oneof_name` for one oneof enum).

## Services

### Descriptors

For every `service`, codegen emits a constant of plain data:

```rust
use ferroly::proto::{MethodDescriptor, MethodKind, ServiceDescriptor};

pub const GREETER_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor::new(
    "helloworld",            // package
    "Greeter",               // name
    "helloworld.Greeter",    // full name
    &[MethodDescriptor::new(
        "SayHello",                        // name
        "/helloworld.Greeter/SayHello",    // request path
        "helloworld.HelloRequest",         // input type
        "helloworld.HelloReply",           // output type
        false,                             // client streaming
        false,                             // server streaming
    )],
);

let m = GREETER_DESCRIPTOR.method_by_path("/helloworld.Greeter/SayHello").unwrap();
assert_eq!(m.kind(), MethodKind::Unary);
```

| Item | API |
|---|---|
| `ServiceDescriptor` | fields `package`, `name`, `full_name`, `methods: &'static [MethodDescriptor]`; `const fn new(package, name, full_name, methods)`; `method(name)`, `method_by_path(path)` |
| `MethodDescriptor` | fields `name`, `path` (`/pkg.Service/Method`, or `/Service/Method` without a package), `input_type`, `output_type` (fully qualified, no leading dot), `client_streaming`, `server_streaming`; `const fn new(…)`; `kind()` |
| `MethodKind` | `Unary`, `ServerStreaming`, `ClientStreaming`, `BidiStreaming`; `const fn from_streaming(client, server)` |

Both structs are `#[non_exhaustive]` and built through the `const fn new`
constructors, so fields can be added without breaking generated code (a new
field would get a new constructor).

### Extending code generation: `ServiceGenerator`

Transport crates and modules (such as a gRPC layer) generate client stubs and
server traits by implementing `ServiceGenerator` and registering it:

```rust,ignore
pub trait ServiceGenerator {
    /// Appends items for `service` to `out` — the body of the service's
    /// package module. Paths in `service` are relative to that module.
    /// Write items at column 0 (they are re-indented line by line, so avoid
    /// multi-line string literals).
    fn generate(&self, service: &ServiceContext<'_>, out: &mut String);

    /// The Rust item names (types, modules, consts) `generate` adds to the
    /// package module, so a clash with a generated message or another
    /// service is an `IdlError` rather than code that does not compile.
    /// Provided: none.
    fn reserved_names(&self, service: &ServiceContext<'_>) -> Vec<String> { Vec::new() }
}

// Config::new().service_generator(MyGenerator).compile_to_file(...)
```

Generators run in registration order, after the descriptor constant. The
context (`#[non_exhaustive]`, so it can grow) carries:

| `ServiceContext` field | Example |
|---|---|
| `file: &ProtoFile`, `service: &ServiceDef` | the parsed definitions (options, spans, raw methods) |
| `package: &str`, `full_name: &str` | `"greet.v1"`, `"greet.v1.Greeter"` |
| `rust_name`, `snake_name`, `const_prefix` | `"Greeter"`, `"greeter"`, `"GREETER"` |
| `descriptor: Option<String>` | `Some("GREETER_DESCRIPTOR")` (or `None` when descriptors are off) |
| `doc: &str` | the comment above the service, as plain text (format it with `codegen::doc_comment`) |
| `methods: Vec<MethodContext>` | one per `rpc`, in order |

| `MethodContext` field | Example |
|---|---|
| `method: &MethodDef`, `name: &str` | `"SayHello"` |
| `rust_name` | `"say_hello"` (keyword-safe) |
| `path` | `"/greet.v1.Greeter/SayHello"` |
| `input_proto`, `output_proto` | `"greet.v1.HelloRequest"`, `"common.Reply"` |
| `input_type`, `output_type` | Rust paths relative to the package module: `"HelloRequest"`, `"super::super::common::Reply"` |
| `client_streaming`, `server_streaming`, `kind: MethodKind` | |
| `doc: &str` | the comment above the rpc |

Helpers for generators: `codegen::doc_comment(text)` (escaped `///` lines),
`codegen::rust_ident(word)` (raw identifiers for keywords),
`codegen::snake_case(name)` and `codegen::pascal_case(name)`. Generated code
should name `::ferroly::proto` (and its own module) by absolute path.

## Limits and untrusted input

Decoding never panics, and its memory is bounded by a budget rather than by
the input size (an empty sub-message is two bytes on the wire, `0a 00`, but a
whole struct in memory, so 4 MiB of them would otherwise become a gigabyte):

- every length prefix is checked against the bytes actually left in the
  enclosing message before anything is read (`LengthOutOfBounds`), and no
  buffer is reserved from a length read off the wire;
- varints longer than ten bytes or over 64 bits are `VarintOverflow`;
  truncated values are `Truncated`;
- the `Limits` budget caps the nesting depth of messages, map entries and
  groups (default **100**; decoding recurses once per level, so this bounds the
  stack), the input size (default **64 MiB**), the number of fields plus
  packed elements (default **10 000 000**, a bound on work), and the
  estimated in-memory size of what decoding creates (`max_decoded_bytes`,
  default **256 MiB**). The last is charged before each allocation:
  `size_of::<T>()` per slot of a repeated field's vector growth, twice the
  `(key, value)` size per `BTreeMap` entry and four times it plus a control
  byte per `HashMap` entry (a hash table peaks at about three buckets per
  entry while it grows), the boxed size per `Box<M>` created
  (`Message::DEFAULT_HEAP_BYTES`), and the bytes of strings, `bytes` and
  unknown fields plus each unknown field's record. Exceeding it is
  `LimitExceeded`. Lower it to a small multiple of the largest message you
  expect: measured, scalar- and string-heavy messages are charged 3–8× their
  wire size, and repeated sparse messages of wide (~480-byte) structs about
  35×. gRPC sets it from its receive-message limit.

Raising `max_depth`: decoding, encoding and dropping a nested message each
recurse once per level. The default of 100 is safe on any thread; on a 2 MiB
thread stack (what `std::thread` and tokio give spawned threads), up to about
300 levels are safe in debug builds and about 1000 in release builds (the
measured overflow points are two to three times that). Go beyond that only
together with a larger stack for the decoding thread.
Messages built in memory are not depth-checked when encoded or dropped.

```rust
use ferroly::proto::{Limits, Message, ProtoError};

#[derive(ferroly::proto::Message, Default)]
struct Node {
    #[proto(tag = 1)]
    child: Option<Box<Node>>,
}

let mut bytes = Vec::new();
for _ in 0..10 {
    let mut outer = vec![0x0a, bytes.len() as u8];
    outer.extend_from_slice(&bytes);
    bytes = outer;
}
let limits = Limits::new()
    .with_max_depth(4)
    .with_max_input_bytes(4 << 20)
    .with_max_entries(100_000)
    .with_max_decoded_bytes(16 << 20);
assert!(matches!(Node::decode_with(&bytes, limits), Err(ProtoError::DepthExceeded(4))));
```

The `.proto` parser is bounded too: files over 16 MiB are rejected, messages
and option aggregates nest at most 64 levels, an import chain at most 64 files
deep, and one load reads at most 1024 files.

## Errors

`ProtoError` (`#[non_exhaustive]`) — decoding failures: `Truncated`,
`VarintOverflow`, `InvalidTag(n)`, `InvalidWireType { tag, bits }`,
`LengthOutOfBounds { len, remaining }`, `DepthExceeded(limit)`,
`LimitExceeded(msg)`, `InvalidUtf8`, `UnexpectedEndGroup(tag)`,
`MissingRequired { message, field }`, `Malformed(msg)`. It converts into the
crate-wide `ferroly::Error` (classified as `InvalidInput`).

`idl::IdlError` — a problem in a `.proto` file or its loading: `file`,
`line`, `column` (1-based; 0 when not tied to a position) and `message`,
displayed as `file:line:column: message`. Codegen and `compile_to_file` report
through it too.

## How it is tested

- `tests/proto_wire.rs` — byte vectors from the protobuf encoding guide
  (`08 96 01`, strings, embedded messages, packed `32 06 03 8e 02 9e a7 05`,
  ten-byte negative `int32`), every scalar type, zig-zag, explicit presence,
  maps, oneofs, open enums, merge semantics, unknown-field round trips, and
  derived output compared with bytes built by hand from `wire`.
- `tests/proto_budget.rs` — the decoded-bytes budget under a counting
  allocator: 4 MiB of empty ~480-byte sub-messages, map entries (also small
  `HashMap<u32, bool>` entries), unknown fields and packed varints are
  rejected with peak memory near the budget.
- `tests/proto_hostile.rs` — every truncation, overlong varints, depth bombs
  (through messages, map entries, oneofs and groups), huge lengths, budget
  exhaustion, required-field tracking under 1M interleaved sub-messages and
  1M oneof switches (linear time), structural garbage, and a seeded fuzz-lite loop (random and
  mutated input must decode, or fail, without panicking; whatever decodes must
  re-encode stably). `FERROLY_FUZZ_ITERS` runs a longer soak.
- `tests/proto_idl.rs` — the parser: every construct, name resolution across
  scopes and files (protoc's first-component rule), loaders, the line and
  column of each error, and a 200k-line block comment in linear time.
- `tests/proto_codegen_golden.rs` and `tests/proto_codegen.rs` — code generated
  from `tests/proto_data/sample/**.proto` must equal the checked-in
  `sample_generated.rs` (regenerate with `FERROLY_BLESS=1`), which is compiled
  — in a public module under `#![deny(missing_docs)]`, and linted by
  `clippy --all-targets -D warnings` — and exercised: round trips, wire bytes
  in field-number order, boxing, proto2 rules, descriptors; plus Rust-name
  clashes, oneof type attributes and lint-clean doc comments.
- `fuzz/fuzz_targets/proto_decode.rs` — libFuzzer over the decoder (and the
  `.proto` parser); see `fuzz/README.md`.
- The derives' compile errors are unit-tested in `ferroly-derive` and covered by
  compile-fail doctests there.

## Not supported

- **Groups** as fields (proto2 `group`) — rejected by the parser; group data on
  the wire is skipped and preserved as unknown fields.
- **Extensions** — `extend` blocks are parsed but generate nothing; extension
  fields on the wire are preserved as unknown fields.
- **Editions** (`edition = "2023"`) — rejected.
- **proto2 closed enums and `[default = …]`** — enums are always open
  (`EnumValue<E>`): an unknown number in a proto2 enum field is kept in the
  field rather than moved to the unknown fields, and fields default to the
  Rust default (an absent proto2 enum decodes as raw 0), not the declared one.
  A proto3 message may not use a proto2 enum (rejected, as protoc does).
- **Well-known-type special behaviour** — `Timestamp`, `Duration`, `Any`, the
  wrappers and `Struct` are generated as ordinary messages; there is no
  `Any` packing helper and no conversion to `std::time` types.
- **The JSON mapping**, text format, descriptors / reflection
  (`FileDescriptorProto`), and `protoc` plugins. The parsed `idl` types and
  `ServiceDescriptor` carry names and shapes, but no `FileDescriptorProto`
  encoding, so gateways and gRPC server reflection cannot be built on them.
- Aliased enum values decode to the first value with the number;
  `from_proto_name` accepts only the first name.
- Name visibility is lenient: a type defined anywhere in the loaded set
  resolves, even when the file that uses it does not import it directly.
- Derived messages cannot be generic and cannot borrow from the input (strings
  and bytes are owned).

## See also

- [grpc](grpc.md) — gRPC servers and clients over these messages; its `GrpcGenerator` is a `ServiceGenerator`.
- [derive](derive.md) — the other derives in `ferroly-derive`.
- [codec](codec.md) — `Encode` / `Decode` and the text formats.
- [fuzz/README.md](../fuzz/README.md) — running the fuzz targets.

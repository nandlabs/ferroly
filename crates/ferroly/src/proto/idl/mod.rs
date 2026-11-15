//! A `.proto` parser: proto3 and the common proto2 subset, with import
//! resolution through a caller-supplied [`Loader`].
//!
//! [`parse`] turns one file into a [`ProtoFile`]; [`FileSet::load`] parses a
//! set of root files and everything they import, checks them, and resolves
//! every type reference to its fully qualified name. The result feeds
//! [`codegen`](super::codegen), and is plain data a tool can inspect.
//!
//! ```
//! use ferroly::proto::idl::{FileSet, Label, MemoryLoader, TypeKind};
//!
//! let mut loader = MemoryLoader::new();
//! loader.insert("greet.proto", r#"
//!     syntax = "proto3";
//!     package greet.v1;
//!     import "common.proto";
//!
//!     // Says hello.
//!     service Greeter {
//!       rpc SayHello (HelloRequest) returns (common.Reply);
//!       rpc Chat (stream HelloRequest) returns (stream common.Reply);
//!     }
//!
//!     message HelloRequest {
//!       string name = 1;
//!       optional int32 times = 2;
//!       repeated string tags = 3 [packed = false];
//!     }
//! "#);
//! loader.insert("common.proto", r#"
//!     syntax = "proto3";
//!     package common;
//!     message Reply { string text = 1; }
//! "#);
//!
//! let set = FileSet::load(&loader, &["greet.proto"]).unwrap();
//! let req = set.message("greet.v1.HelloRequest").unwrap();
//! assert_eq!(req.fields[1].label, Label::Optional);
//!
//! let (_, svc) = set.services().next().unwrap();
//! assert_eq!(svc.full_name, "greet.v1.Greeter");
//! assert_eq!(svc.doc, "Says hello.");
//! let chat = &svc.methods[1];
//! assert!(chat.client_streaming && chat.server_streaming);
//! let target = chat.output.target.as_ref().unwrap();
//! assert_eq!((target.full_name.as_str(), target.kind), ("common.Reply", TypeKind::Message));
//! ```
//!
//! # Coverage
//!
//! `syntax`, `package`, `import` (plain, `public`, `weak`), `option` (file,
//! message, field, enum, value, service and method level, including
//! `(custom)` names and `{ … }` aggregate values, which are kept as text),
//! nested `message` and `enum`, `oneof`, `map<K, V>`, `optional` /
//! `required` / `repeated`, `reserved` numbers, ranges and names,
//! `extensions` ranges, and `service` / `rpc` with `stream` on either side.
//! `extend` blocks are parsed and kept, but nothing is generated for them.
//! proto2 `group` fields and `edition` files are rejected with a clear error.
//!
//! Imports of the well-known types (`google/protobuf/timestamp.proto`,
//! `duration`, `empty`, `any`, `wrappers`, `field_mask`, `struct`) are served
//! from built-in copies when the loader does not have them.

mod lexer;
mod parser;
mod resolve;
mod wkt;

use std::fmt;

pub use parser::parse;
pub use resolve::{FileSet, FsLoader, Loader, MemoryLoader};

/// A position in a `.proto` file (1-based; 0 when unknown).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Span {
    /// The line.
    pub line: u32,
    /// The column, in characters.
    pub column: u32,
}

/// An error in a `.proto` file: a syntax error, an invalid definition, an
/// unresolved type, or an import that could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct IdlError {
    /// The file the error is in, as it was named to the parser or loader.
    pub file: String,
    /// The line (1-based; 0 when the error is not tied to a position).
    pub line: u32,
    /// The column (1-based; 0 when the error is not tied to a position).
    pub column: u32,
    /// What is wrong.
    pub message: String,
}

impl IdlError {
    /// An error at `span` in `file`.
    pub fn new(file: impl Into<String>, span: Span, message: impl Into<String>) -> Self {
        IdlError {
            file: file.into(),
            line: span.line,
            column: span.column,
            message: message.into(),
        }
    }
}

impl fmt::Display for IdlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            write!(f, "{}: {}", self.file, self.message)
        } else {
            write!(
                f,
                "{}:{}:{}: {}",
                self.file, self.line, self.column, self.message
            )
        }
    }
}

impl std::error::Error for IdlError {}

/// The `syntax` of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Syntax {
    /// `syntax = "proto2";` (also the default when the statement is absent).
    Proto2,
    /// `syntax = "proto3";`
    Proto3,
}

/// One parsed `.proto` file.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ProtoFile {
    /// The name the file was loaded under (its import path).
    pub name: String,
    /// The declared syntax.
    pub syntax: Syntax,
    /// The package (empty when the file has none).
    pub package: String,
    /// The `import` statements, in order.
    pub imports: Vec<Import>,
    /// File-level options (`option java_package = "…";`).
    pub options: Vec<OptionEntry>,
    /// Top-level messages.
    pub messages: Vec<MessageDef>,
    /// Top-level enums.
    pub enums: Vec<EnumDef>,
    /// Services.
    pub services: Vec<ServiceDef>,
    /// `extend` blocks (parsed, not generated).
    pub extensions: Vec<ExtendDef>,
}

/// An `import` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Import {
    /// The imported path, as written.
    pub path: String,
    /// `public`, `weak`, or neither.
    pub kind: ImportKind,
    /// Where the statement is.
    pub span: Span,
}

/// The modifier of an `import`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImportKind {
    /// `import "x.proto";`
    Normal,
    /// `import public "x.proto";`
    Public,
    /// `import weak "x.proto";`
    Weak,
}

/// An option value.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Constant {
    /// An identifier (an enum value name, `inf`, `nan`, …).
    Ident(String),
    /// An integer, with its sign.
    Int(i128),
    /// A floating-point number.
    Float(f64),
    /// A string.
    Str(String),
    /// `true` or `false`.
    Bool(bool),
    /// A `{ … }` aggregate (text format), kept as its source text.
    Aggregate(String),
}

/// One `option name = value` (or `[name = value]`).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct OptionEntry {
    /// The option name as written, such as `packed` or `(my.ext).field`.
    pub name: String,
    /// The value.
    pub value: Constant,
    /// Where the option is.
    pub span: Span,
}

/// Finds a boolean option by name.
fn bool_option(options: &[OptionEntry], name: &str) -> Option<bool> {
    options
        .iter()
        .rev()
        .find(|o| o.name == name)
        .and_then(|o| match o.value {
            Constant::Bool(b) => Some(b),
            _ => None,
        })
}

/// A `message`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct MessageDef {
    /// The declared name.
    pub name: String,
    /// The fully qualified name, without a leading dot (`pkg.Outer.Inner`).
    pub full_name: String,
    /// Every field in declaration order, including oneof members.
    pub fields: Vec<FieldDef>,
    /// The oneofs; [`FieldDef::oneof`] indexes into this.
    pub oneofs: Vec<OneofDef>,
    /// Nested messages.
    pub messages: Vec<MessageDef>,
    /// Nested enums.
    pub enums: Vec<EnumDef>,
    /// `reserved` numbers and names.
    pub reserved: Reserved,
    /// `extensions` ranges (inclusive).
    pub extension_ranges: Vec<(u32, u32)>,
    /// `extend` blocks nested in the message (parsed, not generated).
    pub extensions: Vec<ExtendDef>,
    /// Message options.
    pub options: Vec<OptionEntry>,
    /// The comment above the declaration.
    pub doc: String,
    /// Where the declaration is.
    pub span: Span,
}

impl MessageDef {
    /// Whether `option deprecated = true;` is set.
    pub fn deprecated(&self) -> bool {
        bool_option(&self.options, "deprecated").unwrap_or(false)
    }
}

/// `reserved` declarations of a message or enum.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Reserved {
    /// Inclusive number ranges (`reserved 2, 9 to 11, 40 to max;`).
    pub ranges: Vec<(i64, i64)>,
    /// Reserved names.
    pub names: Vec<String>,
}

impl Reserved {
    /// Whether `number` is reserved.
    pub fn contains_number(&self, number: i64) -> bool {
        self.ranges.iter().any(|&(a, b)| a <= number && number <= b)
    }

    /// Whether `name` is reserved.
    pub fn contains_name(&self, name: &str) -> bool {
        self.names.iter().any(|n| n == name)
    }
}

/// The label of a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Label {
    /// No label: proto3 implicit presence (or a oneof member, or a map).
    None,
    /// `optional`: explicit presence.
    Optional,
    /// `required` (proto2).
    Required,
    /// `repeated`.
    Repeated,
}

/// The protobuf scalar types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScalarType {
    /// `double`
    Double,
    /// `float`
    Float,
    /// `int32`
    Int32,
    /// `int64`
    Int64,
    /// `uint32`
    UInt32,
    /// `uint64`
    UInt64,
    /// `sint32`
    SInt32,
    /// `sint64`
    SInt64,
    /// `fixed32`
    Fixed32,
    /// `fixed64`
    Fixed64,
    /// `sfixed32`
    SFixed32,
    /// `sfixed64`
    SFixed64,
    /// `bool`
    Bool,
    /// `string`
    String,
    /// `bytes`
    Bytes,
}

impl ScalarType {
    /// The type for a `.proto` keyword such as `sint64`.
    pub fn from_keyword(word: &str) -> Option<ScalarType> {
        Some(match word {
            "double" => ScalarType::Double,
            "float" => ScalarType::Float,
            "int32" => ScalarType::Int32,
            "int64" => ScalarType::Int64,
            "uint32" => ScalarType::UInt32,
            "uint64" => ScalarType::UInt64,
            "sint32" => ScalarType::SInt32,
            "sint64" => ScalarType::SInt64,
            "fixed32" => ScalarType::Fixed32,
            "fixed64" => ScalarType::Fixed64,
            "sfixed32" => ScalarType::SFixed32,
            "sfixed64" => ScalarType::SFixed64,
            "bool" => ScalarType::Bool,
            "string" => ScalarType::String,
            "bytes" => ScalarType::Bytes,
            _ => return None,
        })
    }

    /// The `.proto` keyword.
    pub fn keyword(self) -> &'static str {
        match self {
            ScalarType::Double => "double",
            ScalarType::Float => "float",
            ScalarType::Int32 => "int32",
            ScalarType::Int64 => "int64",
            ScalarType::UInt32 => "uint32",
            ScalarType::UInt64 => "uint64",
            ScalarType::SInt32 => "sint32",
            ScalarType::SInt64 => "sint64",
            ScalarType::Fixed32 => "fixed32",
            ScalarType::Fixed64 => "fixed64",
            ScalarType::SFixed32 => "sfixed32",
            ScalarType::SFixed64 => "sfixed64",
            ScalarType::Bool => "bool",
            ScalarType::String => "string",
            ScalarType::Bytes => "bytes",
        }
    }

    /// Whether a repeated field of this type can be packed.
    pub fn packable(self) -> bool {
        !matches!(self, ScalarType::String | ScalarType::Bytes)
    }

    /// Whether the type may be a map key.
    pub fn valid_map_key(self) -> bool {
        !matches!(
            self,
            ScalarType::Double | ScalarType::Float | ScalarType::Bytes
        )
    }
}

/// Whether a resolved name is a message or an enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TypeKind {
    /// A `message`.
    Message,
    /// An `enum`.
    Enum,
}

/// What a type reference resolved to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct TypeTarget {
    /// The fully qualified name, without a leading dot.
    pub full_name: String,
    /// Message or enum.
    pub kind: TypeKind,
}

/// A reference to a message or enum by name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct NamedType {
    /// The name as written (possibly relative, possibly with a leading dot).
    pub name: String,
    /// The definition it refers to; filled in by [`FileSet::load`], `None`
    /// straight out of [`parse`].
    pub target: Option<TypeTarget>,
}

impl NamedType {
    fn new(name: String) -> Self {
        NamedType { name, target: None }
    }
}

/// The type of a field.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FieldType {
    /// A scalar.
    Scalar(ScalarType),
    /// A message or enum.
    Named(NamedType),
    /// `map<key, value>`.
    Map(ScalarType, Box<FieldType>),
}

/// A field (including oneof members and map fields).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct FieldDef {
    /// The declared name.
    pub name: String,
    /// The field number.
    pub number: u32,
    /// The label.
    pub label: Label,
    /// The type.
    pub ty: FieldType,
    /// The index into [`MessageDef::oneofs`] for a oneof member.
    pub oneof: Option<usize>,
    /// Field options (`[packed = false, deprecated = true]`).
    pub options: Vec<OptionEntry>,
    /// The comment above the declaration.
    pub doc: String,
    /// Where the declaration is.
    pub span: Span,
}

impl FieldDef {
    /// The `packed` option, when set.
    pub fn packed(&self) -> Option<bool> {
        bool_option(&self.options, "packed")
    }

    /// Whether `[deprecated = true]` is set.
    pub fn deprecated(&self) -> bool {
        bool_option(&self.options, "deprecated").unwrap_or(false)
    }
}

/// A `oneof` group (its members are in [`MessageDef::fields`]).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct OneofDef {
    /// The declared name.
    pub name: String,
    /// Oneof options.
    pub options: Vec<OptionEntry>,
    /// The comment above the declaration.
    pub doc: String,
    /// Where the declaration is.
    pub span: Span,
}

/// An `enum`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct EnumDef {
    /// The declared name.
    pub name: String,
    /// The fully qualified name, without a leading dot.
    pub full_name: String,
    /// The values in declaration order.
    pub values: Vec<EnumValueDef>,
    /// `reserved` numbers and names.
    pub reserved: Reserved,
    /// Enum options.
    pub options: Vec<OptionEntry>,
    /// The comment above the declaration.
    pub doc: String,
    /// Where the declaration is.
    pub span: Span,
}

impl EnumDef {
    /// Whether `option allow_alias = true;` is set.
    pub fn allow_alias(&self) -> bool {
        bool_option(&self.options, "allow_alias").unwrap_or(false)
    }

    /// Whether `option deprecated = true;` is set.
    pub fn deprecated(&self) -> bool {
        bool_option(&self.options, "deprecated").unwrap_or(false)
    }
}

/// One value of an enum.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct EnumValueDef {
    /// The declared name (`CORPUS_WEB`).
    pub name: String,
    /// The number.
    pub number: i32,
    /// Value options.
    pub options: Vec<OptionEntry>,
    /// The comment above the declaration.
    pub doc: String,
    /// Where the declaration is.
    pub span: Span,
}

impl EnumValueDef {
    /// Whether `[deprecated = true]` is set.
    pub fn deprecated(&self) -> bool {
        bool_option(&self.options, "deprecated").unwrap_or(false)
    }
}

/// A `service`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ServiceDef {
    /// The declared name.
    pub name: String,
    /// The fully qualified name, without a leading dot (`pkg.Greeter`).
    pub full_name: String,
    /// The methods in declaration order.
    pub methods: Vec<MethodDef>,
    /// Service options.
    pub options: Vec<OptionEntry>,
    /// The comment above the declaration.
    pub doc: String,
    /// Where the declaration is.
    pub span: Span,
}

impl ServiceDef {
    /// Whether `option deprecated = true;` is set.
    pub fn deprecated(&self) -> bool {
        bool_option(&self.options, "deprecated").unwrap_or(false)
    }
}

/// One `rpc` of a service.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct MethodDef {
    /// The declared name.
    pub name: String,
    /// The request message.
    pub input: NamedType,
    /// The response message.
    pub output: NamedType,
    /// `rpc M(stream Req)`.
    pub client_streaming: bool,
    /// `returns (stream Resp)`.
    pub server_streaming: bool,
    /// Method options.
    pub options: Vec<OptionEntry>,
    /// The comment above the declaration.
    pub doc: String,
    /// Where the declaration is.
    pub span: Span,
}

impl MethodDef {
    /// Whether `option deprecated = true;` is set.
    pub fn deprecated(&self) -> bool {
        bool_option(&self.options, "deprecated").unwrap_or(false)
    }
}

/// An `extend Foo { … }` block.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ExtendDef {
    /// The extended message, as written.
    pub extendee: String,
    /// The extension fields.
    pub fields: Vec<FieldDef>,
    /// Where the block is.
    pub span: Span,
}

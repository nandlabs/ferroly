//! Protocol Buffers: the wire format, a `Message` derive, a `.proto` parser
//! and a code generator — with no `protoc` and no external crate.
//!
//! ```
//! use ferroly::proto::{EnumValue, Message, UnknownFields};
//! use std::collections::BTreeMap;
//!
//! #[derive(ferroly::proto::Enumeration, Clone, Copy, PartialEq, Debug)]
//! enum Role { User = 0, Admin = 1 }
//!
//! #[derive(ferroly::proto::Message, Default, PartialEq, Debug)]
//! struct Account {
//!     #[proto(tag = 1)]
//!     id: u64,
//!     #[proto(tag = 2)]
//!     name: String,
//!     #[proto(tag = 3, sint64)]
//!     balance: i64,
//!     #[proto(tag = 4)]
//!     scores: Vec<i32>,                     // packed by default
//!     #[proto(tag = 5)]
//!     role: EnumValue<Role>,
//!     #[proto(tag = 6, map(key = "string", value = "int32"))]
//!     limits: BTreeMap<String, i32>,
//!     #[proto(tag = 7)]
//!     nickname: Option<String>,             // explicit presence
//!     #[proto(oneof)]
//!     contact: Option<Contact>,
//!     #[proto(unknown)]
//!     unknown: UnknownFields,               // fields from newer writers
//! }
//!
//! #[derive(ferroly::proto::Oneof, PartialEq, Debug)]
//! enum Contact {
//!     #[proto(tag = 8)]
//!     Email(String),
//!     #[proto(tag = 9, fixed64)]
//!     Phone(u64),
//! }
//!
//! let account = Account {
//!     id: 7,
//!     name: "ada".into(),
//!     balance: -5,
//!     scores: vec![1, 2, 3],
//!     role: Role::Admin.into(),
//!     contact: Some(Contact::Email("ada@example.com".into())),
//!     ..Account::default()
//! };
//! let bytes = account.encode_to_vec();
//! assert_eq!(Account::decode(&bytes).unwrap(), account);
//! ```
//!
//! # Pieces
//!
//! - [`Message`] — the encode / decode trait, with
//!   `#[derive(ferroly::proto::Message)]`; [`Oneof`] and [`Enumeration`] with
//!   their derives; [`EnumValue`] for open enums; [`UnknownFields`] to keep
//!   fields a type does not declare.
//! - [`wire`] — varints, zig-zag, keys and the bounded [`Reader`](wire::Reader),
//!   for hand-written messages and transports.
//! - [`idl`] — a `.proto` parser (proto3 and the common proto2 subset) with
//!   import resolution through a caller-supplied loader.
//! - [`codegen`] — Rust source for the messages, enums and services of parsed
//!   `.proto` files, for a consumer's `build.rs`.
//! - [`ServiceDescriptor`] / [`MethodDescriptor`] — what codegen emits for a
//!   `service`: names, request paths, message types and streaming flags.
//!
//! # Untrusted input
//!
//! Decoding never panics, and a length prefix is checked against the bytes
//! actually left before anything is read. Wire size alone does not bound
//! memory, though: an empty sub-message is two bytes on the wire but a whole
//! struct once decoded. So a [`Limits`] budget caps the nesting depth (100),
//! the input size (64 MiB), the number of fields plus packed elements (ten
//! million) and the estimated in-memory size of the decoded value (256 MiB),
//! charged before each allocation. `Message::decode` uses the defaults;
//! `Message::decode_with` takes a budget — lower `max_decoded_bytes` to a
//! small multiple of the largest message you expect (gRPC sets it from its
//! receive limit).
//!
//! The repository's `docs/proto.md` is the full guide: the wire support table,
//! every derive attribute, `.proto` support, codegen with a `build.rs` example,
//! and what is not supported.

#![deny(missing_docs)]

pub mod codegen;
mod error;
mod field;
pub mod idl;
mod limits;
mod message;
mod service;
mod unknown;
pub mod wire;

pub use error::ProtoError;
pub use limits::Limits;
pub use message::{EnumValue, Enumeration, Message, Oneof};
pub use service::{MethodDescriptor, MethodKind, ServiceDescriptor};
pub use unknown::{UnknownField, UnknownFields, UnknownValue};

/// Derives [`Message`] for a struct with named fields.
///
/// See the [module docs](self) for an example and the repository's
/// `docs/proto.md` for every attribute.
pub use ferroly_derive::ProtoMessage as Message;

/// Derives [`Oneof`] for an enum of newtype variants.
pub use ferroly_derive::ProtoOneof as Oneof;

/// Derives [`Enumeration`] for a fieldless enum.
pub use ferroly_derive::ProtoEnumeration as Enumeration;

/// Items the derives expand to. Not a stable API.
#[doc(hidden)]
pub mod __private {
    pub use super::error::ProtoError;
    pub use super::field::*;
    pub use super::message::{Message, Oneof};
    pub use super::unknown::UnknownFields;
    pub use super::wire::{Reader, WireType};
}

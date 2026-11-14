//! Encoding core and format registry for the Ferroly toolkit.
//!
//! A dependency-free data-encoding layer with a content-type-keyed format
//! registry. Provides:
//!
//! - [`Value`] — an ordered, format-agnostic data model.
//! - [`Encode`] / [`Decode`] traits with std-library impls, plus the
//!   derives re-exported from `ferroly-derive`.
//! - [`json`] — a hand-written JSON parser and compact encoder.
//! - [`encode`] / [`decode`] — content-type-keyed dispatch over the formats.
//!
//! ```
//! use ferroly::codec::{json, Decode, Encode};
//!
//! #[derive(Encode, Decode, PartialEq, Debug)]
//! struct Point { x: i32, y: i32 }
//!
//! let s = json::encode(&Point { x: 1, y: 2 });
//! assert_eq!(s, r#"{"x":1,"y":2}"#);
//! let back: Point = json::decode(&s).unwrap();
//! assert_eq!(back, Point { x: 1, y: 2 });
//! ```
//!
//! JSON is full-fidelity; the [`xml`], [`yaml`], and [`toml`] codecs cover the
//! common struct/config subset (see their module docs for what is and isn't
//! modeled).
//!
//! # Untrusted input
//!
//! Every parser enforces one shared [`Limits`] budget — nesting depth, input
//! size, and entry count — and reports malformed input as a [`CodecError`]
//! (with line and column), never a panic. The plain entry points use
//! [`Limits::default`]; each has a `*_with` twin that takes an explicit budget:
//!
//! ```
//! use ferroly::codec::{json, CodecError, Limits};
//!
//! let limits = Limits::new().with_max_input_bytes(1 << 20).with_max_depth(32);
//! let deep = "[".repeat(100);
//! assert!(matches!(
//!     json::from_str_with(&deep, limits),
//!     Err(CodecError::LimitExceeded(_))
//! ));
//! ```
//!
//! # Strict and lenient decoding
//!
//! JSON and TOML are typed, so they decode **strictly**: the string `"42"` is
//! not a number. XML carries only text and YAML is loosely typed, so they (and
//! the `config` module's environment/properties sources) decode **leniently**
//! through [`Decode::decode`]. See [`Decode`] for the exact rules and
//! [`from_value_strict`] to decode a [`Value`] strictly.

#![deny(missing_docs)]

// Allow derive-generated `::ferroly::codec::...` paths to resolve within this crate.

mod error;
mod fmt;
pub mod json;
mod limits;
mod registry;
pub mod schema;
pub mod toml;
mod traits;
mod value;
pub mod xml;
pub mod yaml;

pub use error::CodecError;
pub use limits::Limits;
pub use registry::{decode, decode_with, encode, resolve, try_encode, Format};
pub use traits::{emit_value, from_value_strict, Decode, Decoder, Encode, Encoder, ValueDecoder};
pub use value::{find, Value};

// Re-export the derives so `use ferroly::codec::{Encode, Decode};` brings
// both the trait and the derive macro into scope with a single import.
pub use ferroly_derive::{Decode, Encode, FerrolyError};

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
//! JSON is full-fidelity; the [`xml`] and [`yaml`] codecs cover the common
//! struct/config subset (see their module docs for what is and isn't modeled).

#![deny(missing_docs)]

// Allow derive-generated `::ferroly::codec::...` paths to resolve within this crate.

mod error;
mod fmt;
pub mod json;
mod registry;
pub mod schema;
mod traits;
mod value;
pub mod xml;
pub mod yaml;

pub use error::CodecError;
pub use registry::{decode, encode, resolve, Format};
pub use traits::{emit_value, Decode, Decoder, Encode, Encoder};
pub use value::{find, Value};

// Re-export the derives so `use ferroly::codec::{Encode, Decode};` brings
// both the trait and the derive macro into scope with a single import.
pub use ferroly_derive::{Decode, Encode, FerrolyError};

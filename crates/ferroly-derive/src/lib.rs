//! Derive macros for the Ferroly toolkit.
//!
//! Provides `#[derive(Encode)]` / `#[derive(Decode)]` (targeting the `Value`
//! model and streaming traits of `ferroly::codec`) and `#[derive(FerrolyError)]`
//! (a `thiserror`-subset), so the toolkit needs no external derive crates at
//! runtime — only the standard proc-macro build tooling.
//!
//! This crate is an implementation detail of `ferroly`: use the derives through
//! the re-exports in `ferroly::codec` (or `ferroly::FerrolyError`). The code
//! they generate refers to `::ferroly::codec::…`, so it only compiles in a
//! crate that depends on `ferroly`.
//!
//! ## Encode / Decode
//!
//! Shapes:
//! - Structs with named fields (an object), tuple structs (an array), newtype
//!   structs (transparent: the inner value) and unit structs (`null`).
//! - Enums with unit, newtype, tuple and struct variants. Externally tagged by
//!   default (`"Unit"`, `{"Variant": content}`); `#[ferroly(tag = "...")]`
//!   selects the internally tagged form and `#[ferroly(untagged)]` the untagged
//!   one.
//! - Generic parameters, lifetimes and where-clauses are carried onto the impl;
//!   each type parameter used by an encoded field gets an `Encode` / `Decode`
//!   bound.
//!
//! Container attributes: `rename_all = "..."`, `tag = "..."`, `untagged`,
//! `deny_unknown_fields`, `bound = "..."` (or `bound(encode = "...", decode =
//! "...")`).
//!
//! Field attributes: `rename = "..."`, `alias = "..."` (repeatable), `default`
//! (or `default = "path"`), `skip`, `skip_none`, `flatten`.
//!
//! Variant attributes: `rename = "..."`, `alias = "..."` (repeatable),
//! `rename_all = "..."` (for the variant's own fields).
//!
//! ## FerrolyError
//! - `#[error("...")]` on a struct or on each enum variant (`{0}` positional
//!   and `{field}` named placeholders, with any format spec such as `{0:?}`);
//!   `#[error(transparent)]` forwards to the single wrapped error.
//! - `#[from]` generates a `From` impl and a `source()`; `#[source]` marks a
//!   source field. Both work on structs and on tuple or named variants.
//! - Generic error types get the `Display` / `Error` bounds their fields need.
//!
//! Every unsupported shape or attribute is a compile error pointing at the
//! offending tokens; nothing is silently ignored.

use proc_macro::TokenStream;
use syn::{parse_macro_input, DeriveInput};

mod attrs;
mod case;
mod de;
mod error;
mod fmtstr;
mod model;
mod ser;

#[cfg(doctest)]
mod doctests;

/// Derives `ferroly::codec::Encode`.
///
/// Generates both `encode` (builds a `Value`) and `encode_to` (streams to an
/// `Encoder`), which always produce the same data.
///
/// ```ignore
/// use ferroly::codec::{json, Encode};
///
/// #[derive(Encode)]
/// #[ferroly(rename_all = "camelCase")]
/// struct Account {
///     account_id: u64,
///     #[ferroly(skip_none)]
///     display_name: Option<String>,
/// }
///
/// #[derive(Encode)]
/// enum Event {
///     Ping,                         // "Ping"
///     Renamed(String),              // {"Renamed":"new"}
///     Moved { x: i32, y: i32 },     // {"Moved":{"x":1,"y":2}}
/// }
///
/// let account = Account { account_id: 7, display_name: None };
/// assert_eq!(json::encode(&account), r#"{"accountId":7}"#);
/// ```
///
/// Attributes (all inside `#[ferroly(...)]`, shared with `Decode`):
/// - on the type: `rename_all = "..."`, `tag = "..."`, `untagged`,
///   `deny_unknown_fields`, `bound = "..."`;
/// - on a field: `rename = "..."`, `alias = "..."`, `default`, `skip`,
///   `skip_none`, `flatten`;
/// - on a variant: `rename = "..."`, `alias = "..."`, `rename_all = "..."`.
///
/// The repository's `docs/derive.md` describes each one with examples.
#[proc_macro_derive(Encode, attributes(ferroly))]
pub fn derive_encode(input: TokenStream) -> TokenStream {
    let di = parse_macro_input!(input as DeriveInput);
    ser::expand(di)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Derives `ferroly::codec::Decode`.
///
/// Generates both `decode` (reads a `Value`) and `decode_from` (pulls from a
/// `Decoder`), which accept the same data. It reads exactly what
/// `#[derive(Encode)]` writes for the same type and attributes.
///
/// ```ignore
/// use ferroly::codec::{json, Decode};
///
/// #[derive(Decode)]
/// #[ferroly(deny_unknown_fields)]
/// struct Settings {
///     name: String,
///     #[ferroly(default)]
///     retries: u32,                 // 0 when absent
///     #[ferroly(alias = "ttl")]
///     ttl_seconds: u64,             // also accepts the old key "ttl"
/// }
///
/// let s: Settings = json::decode(r#"{"name":"a","ttl":30}"#).unwrap();
/// assert_eq!((s.retries, s.ttl_seconds), (0, 30));
/// ```
///
/// A field is optional when its type is spelled `Option<…>` or it carries
/// `#[ferroly(default)]`, `#[ferroly(skip)]` or `#[ferroly(skip_none)]`; any
/// other field missing from the input is a `MissingField` error.
///
/// The attributes are the ones listed for `#[derive(Encode)]`; the repository's
/// `docs/derive.md` describes each one with examples.
#[proc_macro_derive(Decode, attributes(ferroly))]
pub fn derive_decode(input: TokenStream) -> TokenStream {
    let di = parse_macro_input!(input as DeriveInput);
    de::expand(di)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Derives `std::error::Error` + `Display` (+ `From` for `#[from]` fields).
///
/// ```
/// # use ferroly_derive::FerrolyError;
/// use std::error::Error;
///
/// #[derive(Debug, FerrolyError)]
/// enum LoadError {
///     #[error("{path}: not found")]
///     NotFound { path: String },
///     #[error("line {0}: {1:?}")]
///     Syntax(usize, String),
///     #[error("read failed")]
///     Io(#[from] std::io::Error),
/// }
///
/// #[derive(Debug, FerrolyError)]
/// #[error("request {id} failed")]
/// struct RequestError {
///     id: u64,
///     #[source]
///     cause: LoadError,
/// }
///
/// let cause = LoadError::Syntax(3, "eof".into());
/// assert_eq!(cause.to_string(), "line 3: \"eof\"");
/// let err = RequestError { id: 7, cause };
/// assert_eq!(err.to_string(), "request 7 failed");
/// assert!(err.source().is_some());
///
/// let io: LoadError = std::io::Error::other("disk").into();
/// assert!(io.source().is_some());
/// ```
///
/// `#[from]` needs a variant or struct with exactly one field; anything else is
/// rejected at compile time:
///
/// ```compile_fail
/// # use ferroly_derive::FerrolyError;
/// #[derive(Debug, FerrolyError)]
/// enum Bad {
///     #[error("io failed for {1}")]
///     Io(#[from] std::io::Error, String),
/// }
/// ```
#[proc_macro_derive(FerrolyError, attributes(error, from, source))]
pub fn derive_error(input: TokenStream) -> TokenStream {
    let di = parse_macro_input!(input as DeriveInput);
    error::expand(di)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

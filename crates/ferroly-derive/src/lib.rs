//! Derive macros for the Ferroly toolkit.
//!
//! Provides `#[derive(Encode)]` / `#[derive(Decode)]` (targeting
//! `ferroly_codec`'s `Value` model) and `#[derive(FerrolyError)]` (a
//! `thiserror`-subset), so the toolkit needs no external derive crates at
//! runtime — only the standard proc-macro build tooling.
//!
//! ## Encode / Decode
//! - Structs with named fields, and enums with unit variants.
//! - Container `#[ferroly(rename_all = "lowercase" | "snake_case" | "camelCase" | ...)]`.
//! - Member `#[ferroly(rename = "...")]` and `#[ferroly(skip_none)]` (omit `None`).
//!
//! ## FerrolyError
//! - `#[error("...")]` per variant (`{0}` positional and `{field}` named placeholders).
//! - `#[from]` generates a `From` impl and a `source()`; `#[source]` marks a source field.

use proc_macro::TokenStream;
use syn::{parse_macro_input, DeriveInput};

mod attrs;
mod case;
mod de;
mod error;
mod ser;

/// Derives `ferroly_codec::Encode`.
#[proc_macro_derive(Encode, attributes(ferroly))]
pub fn derive_encode(input: TokenStream) -> TokenStream {
    let di = parse_macro_input!(input as DeriveInput);
    ser::expand(di)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Derives `ferroly_codec::Decode`.
#[proc_macro_derive(Decode, attributes(ferroly))]
pub fn derive_decode(input: TokenStream) -> TokenStream {
    let di = parse_macro_input!(input as DeriveInput);
    de::expand(di)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Derives `std::error::Error` + `Display` (+ `From` for `#[from]` fields).
#[proc_macro_derive(FerrolyError, attributes(error, from, source))]
pub fn derive_error(input: TokenStream) -> TokenStream {
    let di = parse_macro_input!(input as DeriveInput);
    error::expand(di)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

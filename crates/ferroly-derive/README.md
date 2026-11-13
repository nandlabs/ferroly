# ferroly-derive

Derive macros for the Ferroly toolkit — the in-house replacements for `serde_derive`
and `thiserror`, so the toolkit needs no external derive crates.

Use them through the re-exports in `ferroly` (`ferroly::codec::{Encode, Decode}`,
`ferroly::FerrolyError`); the generated code refers to `::ferroly::codec::…`.

- `#[derive(Encode)]` / `#[derive(Decode)]` — target the `Value` model and the
  streaming `Encoder` / `Decoder` traits of `ferroly::codec`.
  - Shapes: named-field, tuple, newtype (transparent) and unit structs; enums with
    unit, newtype, tuple and struct variants; generic parameters, lifetimes and
    where-clauses.
  - Enum representations: externally tagged (default), internally tagged
    (`#[ferroly(tag = "...")]`), untagged (`#[ferroly(untagged)]`).
  - Container attributes: `rename_all`, `tag`, `untagged`, `deny_unknown_fields`,
    `bound`.
  - Field attributes: `rename`, `alias`, `default`, `skip`, `skip_none`, `flatten`.
  - Variant attributes: `rename`, `alias`, `rename_all`.
- `#[derive(FerrolyError)]` — `#[error("...")]` / `#[error(transparent)]` for
  `Display`, `#[from]` / `#[source]` for `source()` and `From` impls, on enums and
  structs, including generic error types.

Unsupported shapes and attributes are compile errors pointing at the offending
tokens. The full reference, with examples, is in
[`docs/derive.md`](https://github.com/nandlabs/ferroly/blob/main/docs/derive.md).

Build-time only (`proc-macro2`/`syn`/`quote`); nothing added to the runtime dependency tree.

## Testing

`cargo test -p ferroly-derive` runs unit tests for the pure helpers (case
conversion, attribute parsing, format-string rewriting, bound inference) and
compile-pass / compile-fail doctests. Because this crate cannot depend on `ferroly`,
those doctests compile the generated code against a small stand-in for
`ferroly::codec` (`src/doctests/mock_codec.rs`), which must be kept in step with the
real traits. End-to-end behavior is covered by the `derive_*` integration tests in
`ferroly`.

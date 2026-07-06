# ferroly-derive

Derive macros for the Ferroly toolkit — the in-house replacements for `serde_derive`
and `thiserror`, so the toolkit needs no external derive crates.

- `#[derive(Encode)]` / `#[derive(Decode)]` — target `ferroly_serde`'s `Value`
  model; support named-field structs and unit-variant enums, with
  `#[ferroly(rename_all = ...)]`, `#[ferroly(rename = "...")]`, `#[ferroly(skip_none)]`.
- `#[derive(FerrolyError)]` — `#[error("...")]` Display, `#[from]`/`#[source]` for
  `source()` and `From` impls.

Build-time only (`proc-macro2`/`syn`/`quote`); nothing added to the runtime dependency tree.

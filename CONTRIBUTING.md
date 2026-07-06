# Contributing to Ferroly

Thanks for your interest in contributing! This document covers how to build, test, and submit
changes, plus the project's dependency policy and licensing terms.

## Getting started

```sh
git clone https://github.com/nandlabs/ferroly
cd ferroly
cargo test -p ferroly --features full
```

## Reporting issues

Open issues through the
[issue chooser](https://github.com/nandlabs/ferroly/issues/new/choose), which
provides templates for each kind:

- **Bug reports** — steps to reproduce, expected vs. actual behavior.
- **Feature requests** — the use case and the proposed API.
- **Documentation** — what is missing, wrong, or unclear.
- **Questions** — usage help.

## Before you open a pull request

Please make sure the following all pass locally — CI runs the same checks:

```sh
# formatting (must be clean)
cargo fmt --all --check

# lints (warnings are errors)
cargo clippy --workspace --all-targets --features ferroly/full -- -D warnings

# tests (default features and full)
cargo test -p ferroly
cargo test -p ferroly --features full

# license / advisory / duplicate-dependency policy (see below)
cargo deny check
```

## Dependency policy (please read)

Ferroly's defining goal is to be **self-contained**. This is enforced, not aspirational:

- **The only permitted external runtime dependencies** are `tokio` and the TLS stack
  (`rustls`, `tokio-rustls`, `rustls-pki-types`, `webpki-roots`).
- **Build-time** dependencies are limited to the proc-macro tooling (`proc-macro2`, `syn`,
  `quote`) used by `ferroly-derive`.
- **New external dependencies are not accepted.** If a feature needs functionality we don't
  have, implement it in-house (that's the whole point of the project). TLS is the one exception
  and it must stay isolated behind `ferroly::http`'s internal transport — no TLS types in any
  public API.

PRs that add a dependency to `Cargo.toml` will be asked to remove it. If you believe an
exception is genuinely warranted, open an issue to discuss it *before* writing code.

## Code style & conventions

- Format with `rustfmt` (the repo's `rustfmt.toml` settings) — CI checks this.
- Keep `clippy` clean at `-D warnings`.
- Public items are documented (`#![deny(missing_docs)]` is set on the modules).
- Errors use the in-house `#[derive(ferroly_derive::FerrolyError)]`, not `thiserror`.
- Encoding uses `ferroly::codec`'s `Encode`/`Decode`, not `serde`.
- Async trait methods return a manual `BoxFuture` (no `async-trait`).
- Prefer adding tests next to code (`#[cfg(test)] mod tests`) and end-to-end integration tests
  under `crates/ferroly/tests/` (gated with `#![cfg(feature = "...")]`).

## Documentation

Documentation is part of the source of truth and **must be kept in sync**. Each `docs/<module>.md`
page is a comprehensive developer guide, and they must stay that way:

- Update the relevant page in [`docs/`](docs/README.md) whenever you change a module's public
  API or behavior — every public type, method, feature flag, and optional item should be
  covered, with runnable examples for the main and advanced paths.
- Adding a **new module** requires a new `docs/<module>.md` page, an entry in the
  [`docs/README.md`](docs/README.md) module table and architecture diagram, a row in the
  feature table of the top-level [README](README.md), and an option in the "Affected /
  Target Module" dropdowns of the issue templates
  ([`bug_report.yml`](.github/ISSUE_TEMPLATE/bug_report.yml) and
  [`feature_request.yml`](.github/ISSUE_TEMPLATE/feature_request.yml)).
- Keep `///` doc comments accurate (CI builds docs with `-D warnings`, so broken intra-doc
  links fail the build).
- **Terminology:** the codec traits are `Encode` / `Decode` — never write "serialize" /
  "deserialize".
- **Attribution:** the port lineage is credited once in the top-level [README](README.md).
  Do **not** scatter "ports golly's X" / "(golly: …)" references through code comments or docs;
  describe what the code does on its own terms. (The Go → Rust design rationale lives in the one
  dedicated `docs/roadmap-*.md` page.)

## Commits & pull requests

- Keep PRs focused; the [pull-request template](.github/PULL_REQUEST_TEMPLATE.md)
  prompts for the motivation and a summary of what changed — please fill it in.
- Use meaningful commit messages.
- Reference any related issue.
- Ensure the crate still builds with **default features** and with **`full`** (a feature that
  compiles only under `full` but breaks a smaller feature set is a bug).

## License

Ferroly is dual-licensed under **Apache-2.0 OR MIT** (see [LICENSING.md](LICENSING.md)).

Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion
in Ferroly, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.

In other words: **inbound = outbound.** By submitting a contribution you agree that it may be
distributed under both the MIT and the Apache-2.0 licenses, at the recipient's option. Do not
submit code you cannot license this way (e.g. copied from a copyleft-licensed project).

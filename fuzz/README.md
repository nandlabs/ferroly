# ferroly-fuzz

Coverage-guided fuzz targets for ferroly's hand-rolled, untrusted-input parsers,
using [`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz) (libFuzzer).

This is a **detached workspace**: `libfuzzer-sys` is nightly-only fuzzing tooling
and is deliberately kept out of the main workspace, so it never enters ferroly's
runtime dependency budget or the `cargo-deny` check. The stable, always-on
counterpart is the generative "fuzz-lite" suite in
`crates/ferroly/tests/codec_fuzz.rs`, which runs in normal CI.

## Prerequisites

```sh
rustup toolchain install nightly
cargo install cargo-fuzz
```

## Targets

| Target | Parser |
|---|---|
| `json_parse` | `ferroly::codec::json::from_slice` |
| `xml_parse`  | `ferroly::codec::xml::from_str` |
| `yaml_parse` | `ferroly::codec::yaml::from_str` |

## Run

```sh
# from the repo root
cargo +nightly fuzz run json_parse
cargo +nightly fuzz run xml_parse
cargo +nightly fuzz run yaml_parse
```

Each target asserts the invariant that the parser must **never panic** and must
bound its memory/recursion on arbitrary input.

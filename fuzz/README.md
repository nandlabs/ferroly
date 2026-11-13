# ferroly-fuzz

Coverage-guided fuzz targets for ferroly's hand-rolled, untrusted-input parsers,
using [`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz) (libFuzzer).

This is a **detached workspace**: `libfuzzer-sys` is nightly-only fuzzing tooling
and is deliberately kept out of the main workspace, so it never enters ferroly's
runtime dependency budget or the `cargo-deny` check. The stable, always-on
counterpart is the generative "fuzz-lite" suite in
`crates/ferroly/tests/codec_fuzz.rs` (parsers) and
`crates/ferroly/tests/codec_roundtrip.rs` (typed round-trips), which run in
normal CI.

## Prerequisites

```sh
rustup toolchain install nightly
cargo install cargo-fuzz
```

## Targets

| Target | Parser | Invariants beyond "never panics" |
|---|---|---|
| `json_parse` | `ferroly::codec::json::from_slice` | the streaming decoder accepts exactly what the tree parser accepts; a parsed value is unchanged by a compact or indented re-encode |
| `xml_parse`  | `ferroly::codec::xml::from_str` | the parser reads back what `xml::encode` writes |
| `yaml_parse` | `ferroly::codec::yaml::from_str` | a parsed value is unchanged by `yaml::encode` → `from_str` |
| `toml_parse` | `ferroly::codec::toml::from_str` | the parser reads back what `toml::encode` writes |

Every target also relies on the shared `codec::Limits` budget (depth, input
size, entry count) to bound memory and recursion on arbitrary input.

## Seeds

`seeds/<target>/` holds a small committed seed corpus for each target. It
includes the inputs behind past crashes, so the fuzzer starts from them:

- XML: a truncated processing instruction (`<?`), declaration, comment
  (`<!--`), and CDATA section, plus a signed character reference.
- TOML: a `\u` / `\U` escape followed by a multibyte character, a signed
  escape, and deep dotted keys / table headers.

libFuzzer writes new inputs to the first corpus directory and only reads the
rest, so pass the seeds second (the working corpus under `corpus/` is
git-ignored):

```sh
# from the repo root
cargo +nightly fuzz run json_parse fuzz/corpus/json_parse fuzz/seeds/json_parse
cargo +nightly fuzz run xml_parse  fuzz/corpus/xml_parse  fuzz/seeds/xml_parse
cargo +nightly fuzz run yaml_parse fuzz/corpus/yaml_parse fuzz/seeds/yaml_parse
cargo +nightly fuzz run toml_parse fuzz/corpus/toml_parse fuzz/seeds/toml_parse
```

For a short, bounded burst (for example in a scheduled job), cap the time:

```sh
cargo +nightly fuzz run toml_parse fuzz/corpus/toml_parse fuzz/seeds/toml_parse \
    -- -max_total_time=60
```

## Without nightly

The fuzz-lite suites take `FERROLY_FUZZ_ITERS` to run a longer soak on stable:

```sh
FERROLY_FUZZ_ITERS=2000000 cargo test --release -p ferroly \
    --test codec_fuzz --test codec_roundtrip
```

When a fuzz target finds a crash, add the minimized input to
`seeds/<target>/` and pin it as a regression case in `codec_fuzz.rs`.

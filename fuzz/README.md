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
| `template_render` | `ferroly::genai::template::Template` | parsing and rendering arbitrary template text never panics and respects the output cap |
| `template_jinja` | `ferroly::template::Template` (the Jinja2 subset); the first byte picks a chat, tools, odd-values or empty context | parsing and rendering arbitrary text never panics or hangs (a render that takes 10 s under a two-million-step `max_steps` budget fails); output stays under the cap and rendering is deterministic |
| `regex_match` | the schema `pattern` matcher, via `ferroly::codec::schema::Schema` | input is `pattern NUL subject`; a compiled schema and the one-shot `schema::validate` agree, and a rejected pattern fails closed |
| `ed25519_verify` | `ferroly::sig` (Ed25519, Ed25519ctx, Ed25519ph) | arbitrary keys, signatures, contexts and messages never panic; the boolean and typed APIs agree; streaming verification matches one-shot for any chunking |
| `ed25519_structured` | `ferroly::sig` (Ed25519), from valid RFC 8032 triples | the fuzzer edits a valid (key, signature, message); the result verifies **iff** it is unchanged, so the accept path and full `R`/`S` decoding are exercised; streaming matches one-shot |
| `proto_decode` | `ferroly::proto::Message::decode` (and `proto::idl::parse`) | a decoded message re-encodes to bytes that decode and re-encode identically; `encoded_len` matches the encoding |
| `h2_frames` | the HTTP/2 frame decoder and HPACK decoder (`ferroly::http::__fuzz_h2_frames`, hidden) | every frame-rule violation and malformed header block is a plain error; the dynamic table and decoded header lists stay within their caps |
| `h2_conn` | the HTTP/2 connection state machine: the server engine over an in-memory pipe, the input being the client's bytes after the preface (`ferroly::http::__fuzz_h2_connection`, hidden) | every frame sequence, on streams in any state, ends in a clean close, a stream error or a GOAWAY: no panic, no hang, no task left behind |
| `grpc_frames` | gRPC message framing, `grpc-status` / `grpc-message` / details trailers, `grpc-timeout` and metadata (`ferroly::grpc::__fuzz`, hidden) | frames split at any chunk boundary never exceed the 1 KiB limit; a written status reads back with the same code and message; a formatted timeout parses back to at least the original |
| `www_authenticate` | the OCI registry `WWW-Authenticate` challenge parser (`ferroly::store::oci::__fuzz_challenges`, hidden) | at most 8 challenges of at most 16 parameters; no empty scheme or parameter name; the parsed text is never longer than the input (quoted-pairs before multi-byte characters included) |

Every target also relies on the shared `codec::Limits` budget (depth, input
size, entry count) to bound memory and recursion on arbitrary input.

## Seeds

`seeds/<target>/` holds a small committed seed corpus for each target. It
includes the inputs behind past crashes, so the fuzzer starts from them:

- XML: a truncated processing instruction (`<?`), declaration, comment
  (`<!--`), and CDATA section, plus a signed character reference.
- TOML: a `\u` / `\U` escape followed by a multibyte character, a signed
  escape, and deep dotted keys / table headers.
- Ed25519: the RFC 8032 Ed25519, Ed25519ctx and Ed25519ph vectors in the
  target's input layout, so mutations start next to valid signatures.
- Templates: one seed per construct (substitution, the leading-dot form,
  `#if` / `#unless` / `else`, `#each` over arrays and objects with the `@`
  loop variables, nested blocks, escapes and comments, a partial), plus
  unterminated and unbalanced tags and nested loops that multiply the output.
- Jinja2 templates: the chat templates from the golden tests (ChatML,
  Zephyr, Mistral v0.2 / v0.3, Phi-3, Qwen2.5 and the hand-written
  Llama-2, Llama-3 and Gemma-2 formats), one seed per construct, and inputs
  that push each limit (deep nesting, output growth, string doubling, a
  namespace that contains itself), and inputs that once hung or panicked
  (`[] * 10**17`, sorting NaN, `i128`-sized slice steps and `range` bounds,
  `map('map', ...)`, `break` over a large list, linear operations in a long
  loop, unpacking a huge string, many parameters and keyword arguments).
  `template_engine.rs` replays them on stable.
- `WWW-Authenticate`: a quoted-pair before a multi-byte character
  (`Bearer realm="\é"`, which used to panic), an unterminated quoted-pair,
  and the Docker Hub and two-challenge forms.

libFuzzer writes new inputs to the first corpus directory and only reads the
rest, so pass the seeds second (the working corpus under `corpus/` is
git-ignored):

```sh
# from the repo root
cargo +nightly fuzz run json_parse fuzz/corpus/json_parse fuzz/seeds/json_parse
cargo +nightly fuzz run xml_parse  fuzz/corpus/xml_parse  fuzz/seeds/xml_parse
cargo +nightly fuzz run yaml_parse fuzz/corpus/yaml_parse fuzz/seeds/yaml_parse
cargo +nightly fuzz run toml_parse fuzz/corpus/toml_parse fuzz/seeds/toml_parse
cargo +nightly fuzz run template_render fuzz/corpus/template_render fuzz/seeds/template_render
cargo +nightly fuzz run template_jinja fuzz/corpus/template_jinja fuzz/seeds/template_jinja
cargo +nightly fuzz run regex_match fuzz/corpus/regex_match fuzz/seeds/regex_match
cargo +nightly fuzz run ed25519_verify fuzz/corpus/ed25519_verify fuzz/seeds/ed25519_verify
cargo +nightly fuzz run ed25519_structured
cargo +nightly fuzz run proto_decode fuzz/corpus/proto_decode fuzz/seeds/proto_decode
cargo +nightly fuzz run grpc_frames fuzz/corpus/grpc_frames fuzz/seeds/grpc_frames
cargo +nightly fuzz run www_authenticate fuzz/corpus/www_authenticate fuzz/seeds/www_authenticate
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

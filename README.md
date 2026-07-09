<p align="center">
  <img src="assets/ferroly-logo.svg" alt="ferroly" width="320">
</p>

<p align="center">
  <a href="https://github.com/nandlabs/ferroly/actions/workflows/ci.yml"><img src="https://github.com/nandlabs/ferroly/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <img src="https://img.shields.io/badge/version-0.3.0-B22222" alt="version 0.3.0">
  <img src="https://img.shields.io/badge/rust-1.75%2B-B7410E" alt="MSRV 1.75+">
  <a href="#license"><img src="https://img.shields.io/badge/license-Apache--2.0%20OR%20MIT-blue" alt="license: Apache-2.0 OR MIT"></a>
  <img src="https://img.shields.io/badge/unsafe-1%20audited%20block-9cbf3b" alt="unsafe: 1 audited block">
</p>

**A self-contained, dependency-minimal Rust toolkit of enterprise utilities.**

Ferroly is a broad collection of reusable enterprise building blocks — encoding, configuration,
an LLM/GenAI abstraction, an HTTP client and server, a router, WebSockets, component lifecycle,
metrics, and more — under a single crate. It is a Rust port of
[nandlabs/golly](https://github.com/nandlabs/golly).

Its defining goal is **near-zero external dependencies**: everything is implemented in-house,
so the *entire* runtime dependency tree is just `tokio` and the TLS stack.

---

## Design philosophy

Ferroly's goal is a reusable collection of enterprise utilities that stays self-contained: it
hand-rolls the whole stack rather than pulling in an ecosystem of crates.

**The complete dependency tree is:**

- **`tokio`** — the async runtime (the one piece Rust's std library deliberately does not
  provide).
- **`rustls` / `tokio-rustls` / `rustls-pki-types` / `webpki-roots`** — TLS for HTTPS/WSS. This
  is the single crypto exception (TLS cannot be safely hand-rolled), and it is isolated behind
  an internal transport boundary so it never leaks into Ferroly's public API.
- **`ferroly-derive`** — Ferroly's own derive macros (`proc-macro2` / `syn` / `quote`,
  build-time only).

Everything else is implemented from scratch: JSON/XML/YAML/TOML encoding and a `#[derive]`-based
`Encode`/`Decode`, layered configuration, a prompt-template engine, MIME detection,
error/derive macros, an HTTP/1.1 client and server, a router, and a WebSocket implementation
(RFC 6455 framing with a from-scratch SHA-1 handshake).

## Structure

Ferroly is a single crate (`ferroly`) whose areas are **feature-gated modules**, plus the
companion `ferroly-derive` proc-macro crate. Cloud integrations live in separate crates
(`ferroly-aws`, `ferroly-gcp`, `ferroly-vault` — planned).

## Installation

```toml
[dependencies]
ferroly = { version = "0.3", features = ["genai", "openai", "codec"] }
```

Enable only what you need — unused modules and their dependencies are never compiled.

## Modules & features

| Feature | Module | Purpose |
|---|---|---|
| `errutils` | `ferroly::errutils` | `MultiError` aggregation + the `#[derive(FerrolyError)]` typed-error macro |
| `codec` | `ferroly::codec` | `Value` model, `Encode`/`Decode` (+ derives), JSON/XML/YAML/TOML, content-type registry |
| `hash` | `ferroly::hash` | Streaming SHA-256/SHA-1/HMAC-SHA256 + hex `Digest` |
| `cli` | `ferroly::cli` | Command-line parser (subcommands, typed flags, env fallback, `--help`) |
| `config` | `ferroly::config` | Layered environment + file configuration |
| `fsutils` | `ferroly::fsutils` | Content-type detection (extension table + magic-byte sniffing) + read-only memory-mapped files (`Mmap`) |
| `lifecycle` | `ferroly::lifecycle` | Component start/stop orchestration with dependency ordering |
| `rt` | `ferroly::rt` | Async runtime surface (tokio spawn/channels/sync/time/TCP re-exported) |
| `http` | `ferroly::http` | In-house HTTP/1.1 client + server (streaming, chunked, SSE, range/resumable downloads, TLS) |
| `clients` | `ferroly::clients` | Retry, circuit breaker, and auth providers |
| `genai` | `ferroly::genai` | Provider-agnostic LLM interface + prompt templates + a **model router** (capability/cost routing with fallback) |
| `openai` / `claude` / `ollama` | — | GenAI provider implementations |
| `turbo` | `ferroly::turbo` | First-class HTTP router + serving |
| `rest` | `ferroly::rest` | HTTP client + server framework (codec-aware, lifecycle-integrated) |
| `ws` | `ferroly::ws` | WebSocket client + server (RFC 6455, hand-rolled) |
| `obs` | `ferroly::obs` | Distributed span/event tracing + exporters (JSON, OTLP/HTTP) |
| `full` | — | Everything |

Default features: `codec`, `errutils`.

## Documentation

- **Per-module guides** live in [`docs/`](docs/README.md) — one detailed page per module
  (codec, hash, genai, http, turbo, rest, ws, obs, config, lifecycle, clients, cli,
  errutils, fsutils, rt, derive), with architecture notes and cross-links.
- **API reference**: `cargo doc -p ferroly --features full --open`.

## Examples

### Encoding

```rust
use ferroly::codec::{json, Encode, Decode};

#[derive(Encode, Decode, PartialEq, Debug)]
struct Config { name: String, port: u16 }

let s = json::encode(&Config { name: "svc".into(), port: 8080 });
let back: Config = json::decode(&s).unwrap();
```

### GenAI (provider-agnostic)

```rust
use ferroly::genai::{CompletionRequest, GenAiProvider, Message, OpenAiProvider};

let provider = OpenAiProvider::new("sk-...", None);

let request = CompletionRequest::builder("gpt-4o")
    .message(Message::user("Say hello in French."))
    .build();
let response = provider.complete(request).await?;
println!("{}", response.text());
```

### HTTP router (turbo)

```rust
use ferroly::turbo::Router;
use ferroly::http::{HttpResponse, StatusCode};

let router = Router::new()
    .get("/greet/:name", |ctx| async move {
        let name = ctx.param("name").unwrap_or("world").to_string();
        HttpResponse::text(StatusCode::OK, format!("hi {name}"))
    });
router.serve("127.0.0.1:8080").await?;
```

### WebSocket

```rust
use ferroly::ws::{WsClient, WsOptions, Message};

let mut client = WsClient::dial("wss://echo.example/ws", WsOptions::default()).await?;
client.send(Message::text("hello"))?;
if let Some(reply) = client.recv().await {
    println!("{reply:?}");
}
client.close().await?;
```

## Building & testing

```sh
# build/test everything
cargo build -p ferroly --features full
cargo test  -p ferroly --features full

# or just what you use
cargo build -p ferroly --features "genai,openai,codec"
```

Lints and formatting:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --features ferroly/full -- -D warnings
```

## Minimum supported Rust version

Ferroly targets a recent stable Rust toolchain (MSRV `1.75`).

## Status

The foundation, GenAI, and the full HTTP/WebSocket stack are implemented and tested, along
with hashing, an async-runtime surface, a CLI parser, memory-mapped files, and distributed
tracing. Further utilities (`scheduler`, `secrets`, `collections`, `pool`, `uuid`, `semver`,
and more), signature verification, HTTP/2 + gRPC, and the cloud extension crates are tracked
in the roadmap/issues and not yet built.

## Contributing

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md). Note the strict
**dependency policy**: new external runtime dependencies are not accepted (the only permitted
runtime deps are `tokio` and the TLS stack).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option. See [LICENSING.md](LICENSING.md) for details, including the licenses of the
(permissive) third-party dependencies.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in this work by you, as defined in the Apache-2.0 license, shall be dual licensed as above,
without any additional terms or conditions.

<p align="center">
  <img src="assets/ferroly-logo.svg" alt="ferroly" width="320">
</p>

<p align="center">
  <a href="https://github.com/nandlabs/ferroly/actions/workflows/ci.yml"><img src="https://github.com/nandlabs/ferroly/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <img src="https://img.shields.io/badge/version-0.6.0-B22222" alt="version 0.6.0">
  <img src="https://img.shields.io/badge/rust-1.75%2B-B7410E" alt="MSRV 1.75+">
  <a href="#license"><img src="https://img.shields.io/badge/license-Apache--2.0%20OR%20MIT-blue" alt="license: Apache-2.0 OR MIT"></a>
  <img src="https://img.shields.io/badge/unsafe-4%20audited%20modules-9cbf3b" alt="unsafe: 4 audited modules">
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
  an internal transport boundary so it never leaks into Ferroly's public API. The same crypto
  provider verifies RS256 / ES256 JWT signatures. Ferroly does not force a backend: it uses the
  process-default `CryptoProvider` if you installed one (e.g. `aws-lc-rs`, or `rustls::crypto::default_fips_provider()` for FIPS), else `ring`
  from the default `tls-ring` feature; with `default-features = false` no backend is linked
  ([TLS crypto backend](docs/http.md#tls-crypto-backend)).
- **`ferroly-derive`** — Ferroly's own derive macros (`proc-macro2` / `syn` / `quote`,
  build-time only).
- **`log`** — *optional, off by default.* The one exception to the list above: the
  `log-bridge` feature adds the `log` facade crate (which has no dependencies of its own) so
  records from ecosystem crates reach `ferroly::log`. Without that feature it is not compiled.

Everything else is implemented from scratch: JSON/XML/YAML/TOML encoding and a `#[derive]`-based
`Encode`/`Decode`, layered configuration, a prompt-template engine, MIME detection,
error/derive macros, SHA-2/HMAC/HKDF/PBKDF2, an HTTP/1.1 client and server, a router, a
WebSocket implementation (RFC 6455), resilience policies, and the GenAI provider clients.

The rule is enforced: CI fails if either crate gains a direct dependency outside this list.

Hand-rolling the stack moves the security surface into this repository. The parsers are
fuzzed, limits and timeouts are on by default, and the hand-written cryptography is not
FIPS-validated — see [Security posture](crates/ferroly/README.md#security-posture).

## Structure

Ferroly is a single crate (`ferroly`) whose areas are **feature-gated modules**, plus the
companion `ferroly-derive` proc-macro crate. Each feature enables only the `tokio` features
it needs. There are no separate cloud-provider crates: cloud services plug in through the
extension traits (`vfs::Vfs`, `messaging::Provider`, `vectorstore::VectorStore`).

## Installation

```toml
[dependencies]
ferroly = { version = "0.6", features = ["genai", "openai", "codec"] }
```

Enable only what you need — unused modules and their dependencies are never compiled.

## Modules & features

| Feature | Module | What it provides | Pulls in |
|---|---|---|---|
| `codec` *(default)* | `ferroly::codec` | `Value` model, `Encode` / `Decode` traits and derives, JSON, XML, YAML and TOML, content-type dispatch, shared parser limits, a JSON-Schema subset validator | — |
| `errutils` *(default)* | `ferroly::errutils` | `MultiError`, `.context()`, error classification (`Classify` / `ErrorKind`); `#[derive(FerrolyError)]` | — |
| `config` | `ferroly::config` | Layered configuration from files, environment and CLI; profiles, `${VAR}` interpolation, secrets from files, validation, unknown-key checks, config file discovery, `env:`/`file:` secret references (with `hash`) | `codec` |
| `cli` | `ferroly::cli` | Command-line parser: subcommands, typed and repeated flags, groups, help, shell completion | — |
| `hash` | `ferroly::hash` | SHA-256/384/512, SHA-1, HMAC, HKDF, PBKDF2, base64, hex, `Secret`, random bytes; MD5 and CRC-32 checksums | — |
| `sig` | `ferroly::sig` | Verify-only Ed25519 signatures (RFC 8032): Ed25519, Ed25519ctx, Ed25519ph; strict decoding, streaming verifiers | `hash` |
| `fsutils` | `ferroly::fsutils` | Content-type detection, atomic writes, temp files, lock files, memory-mapped reads (Unix and Windows), human-readable byte sizes | — (`codec` for `ByteSize` decoding and `JsonSchema`) |
| `auth` | `ferroly::auth` | JWT: HS256/384/512 mint and verify with a `Validation` policy; RS256 / ES256 verification from JWKS (with `http`); scopes | `codec`, `hash` |
| `lifecycle` | `ferroly::lifecycle` | Component start/stop ordering, rollback, supervision, health and readiness probes, graceful shutdown | tokio |
| `rt` | `ferroly::rt` | The async runtime surface: tasks, channels, time, I/O, signals, `#[ferroly::rt::main]` / `#[ferroly::rt::test]`, `CancellationToken`, `TaskTracker` | tokio (full) |
| `http` | `ferroly::http` | HTTP/1.1 client and server: strict parsing, limits and timeouts, TLS and mutual TLS, proxies, redirects, SSE, resumable downloads | tokio, rustls |
| `http2` | `ferroly::http` | HTTP/2 server and client: ALPN `h2` and h2c prior knowledge, HPACK, flow control, multiplexing, trailers, bidirectional streaming, rapid-reset and CONTINUATION-flood defences | `http` |
| `grpc` | `ferroly::grpc` | gRPC over HTTP/2: unary, server-, client- and bidirectional streaming; status and trailers, ASCII and binary metadata, `grpc-timeout` deadlines on both sides, message-size limits; a `.proto` service generator (server trait, adapter, typed client) and the standard health service; interoperates with grpc-go | `http2`, `protobuf`, `hash` |
| `tls-ring` *(default)* | — | `ring` as the rustls crypto backend, used when the application has not installed a process-default `CryptoProvider`; turn default features off to bring your own (e.g. `aws-lc-rs`, or rustls' `fips` build for FIPS). Takes effect only with `http` | ring |
| `clients` | `ferroly::clients` | Retry with jitter and budgets, circuit breaker, bulkhead, rate limiter, a composable `Policy`, auth providers including OAuth 2.0 client credentials | `http`, `hash`, `codec` |
| `turbo` | `ferroly::turbo` | Router and middleware: groups, async auth, CORS, security headers, rate limiting, JWT, cookies, forms, multipart, static files, problem+json | `http`, `codec` |
| `rest` | `ferroly::rest` | REST client (retries, breakers, pagination) and a lifecycle-managed REST server with health endpoints | `turbo`, `clients`, `lifecycle` |
| `ws` | `ferroly::ws` | WebSocket client and server (RFC 6455): async handlers, hub and rooms, origin policy, upgrade hook, keepalive, reconnect | `http`, `lifecycle`, `hash` |
| `genai` | `ferroly::genai` | Provider-agnostic LLM interface: chat, typed streaming, tools, structured output, embeddings, prompt templates, a model router with budgets and fallback, a mock provider | `codec`, `clients`, `http` |
| `openai` / `claude` / `ollama` | — | Provider implementations (`all-providers` enables all three) | `genai` |
| `vectorstore` | `ferroly::vectorstore` | `VectorStore` trait, in-memory store with metadata filters, namespaces and snapshots, text chunking | `codec` |
| `messaging` | `ferroly::messaging` | In-process queues and topics with typed attributes, subscription filters, a delivery context (deferred acks, delivery info), priority, scheduling, TTL, dead-lettering and graceful drain; a provider trait with receipts and capabilities | `codec`, `lifecycle` |
| `vfs` | `ferroly::vfs` | Virtual filesystem trait; rooted local and in-memory backends, scheme registry, atomic and streaming writes with abort, write preconditions, etags, signed-URL and typed-error hooks for object stores | tokio |
| `store` | `ferroly::store` | Content-addressed cache: SHA-256-keyed, verify-on-write, atomic commit, resumable downloads, safe concurrent fetches | `http`, `hash` |
| `store-s3` / `store-hf` / `store-oci` | `ferroly::store::{s3, hf, oci}` | Fetch adapters into the cache: S3-compatible storage with SigV4, Hugging Face Hub files, OCI registry manifests and blobs | `store` (+ `codec` for S3 / OCI) |
| `log` | `ferroly::log` | Structured logging: JSON or text, multiple sinks, file rotation, redaction, per-target levels | `codec` |
| `log-bridge` | `ferroly::log::bridge` | Routes records from the `log` facade crate (`log::info!`, …) into a ferroly `Logger`; adds the optional `log` dependency | `log` |
| `metrics` | `ferroly::metrics` | Counters, gauges, histograms; Prometheus and OpenMetrics text; cardinality guard | — |
| `obs` | `ferroly::obs` | Tracing: spans, W3C trace context, sampling, JSON and plain-text span exporters, OTLP/HTTP export | `codec` (+ `http` for OTLP) |
| `protobuf` | `ferroly::proto` | Protocol Buffers: wire codec (varint, zig-zag, fixed, length-delimited, packed, maps, oneof, open enums, unknown-field preservation), `#[derive(proto::Message)]`, a `.proto` parser with import resolution, and a `build.rs` code generator with service descriptors; no `protoc` | — |
| `rt-test-util` | `ferroly::rt::time` | The paused test clock (`pause`, `advance`, `resume`) — for `[dev-dependencies]` | `rt` |
| `full` | — | Everything above except `rt-test-util` | |

Default features: `codec`, `errutils`, `tls-ring`.

## Documentation

- **Per-module guides** live in [`docs/`](docs/README.md) — one detailed page per module,
  with architecture notes, security notes and cross-links.
- **Crate READMEs**: [`crates/ferroly`](crates/ferroly/README.md) (a tour of every module,
  security posture and known limits — the page shown on crates.io) and
  [`crates/ferroly-derive`](crates/ferroly-derive/README.md) (every derive shape and attribute).
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
client.send(Message::text("hello")).await?;
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

The full pre-PR checklist (per-feature lints, the dependency budget, fuzzing) is in
[CONTRIBUTING.md](CONTRIBUTING.md).

## Minimum supported Rust version

Ferroly's MSRV is Rust `1.75`; CI builds and tests every feature on it. The policy for
raising it (never newer than stable minus 4, raised only in a minor release unless a
held-back dependency has a security advisory) is in
[CONTRIBUTING.md](CONTRIBUTING.md#minimum-supported-rust-version-msrv).

## Status

Every module listed above is implemented, documented and tested; the changes in each
release are listed in the [changelog](CHANGELOG.md). What Ferroly deliberately does
**not** provide — HTTP/3, durable messaging and vector backends, object-storage writes,
OIDC flows, hosted-LLM platforms beyond OpenAI / Anthropic / Ollama — is listed under
"What is not here" in the [crate README](crates/ferroly/README.md#what-is-not-here).
Separate cloud crates (AWS, GCP, Vault) are an idea, not a commitment: none exist, and
there is no date for them. Until then, back the `vfs`, `messaging` and `vectorstore`
traits with your provider's SDK or HTTP API.

## Contributing

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md). Note the strict
**dependency policy**: new external runtime dependencies are not accepted (the only permitted
runtime deps are `tokio` and the TLS stack, plus the optional `log` facade crate behind the
opt-in `log-bridge` feature).

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

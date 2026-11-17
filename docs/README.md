# Ferroly — Developer Guide

**A self-contained, dependency-minimal Rust toolkit of enterprise utilities.**

Ferroly bundles data encoding, configuration, an LLM/GenAI abstraction, an HTTP
client + server, a first-class router, WebSockets, messaging, a virtual
filesystem, component lifecycle, structured logging, auth, and more — into a
**single feature-gated crate**.

Its defining constraint: **near-zero external dependencies.** The entire runtime
dependency tree is just **`tokio`** (async runtime) and the **`rustls`** TLS
stack. Everything else — JSON/XML/YAML/TOML codecs, HTTP/1.1, WebSocket framing,
SHA-1/SHA-2/HMAC/HKDF, base64, JWT, the derive macros — is implemented in-house. If a
module doesn't need async or TLS, it compiles with **no external crates at all**. The
one optional exception is the opt-in `log-bridge` feature, which adds the dependency-free
`log` facade crate (see [Bridging the `log` crate](log.md#bridging-the-log-crate)).

> New here? Start with **[Getting Started](getting-started.md)**.

---

## Module map

Everything lives under one crate; each area is a **cargo feature** you opt into.

### Foundation (mostly std-only)
| Module | Feature | What it does |
|---|---|---|
| [`codec`](codec.md) | `codec` *(default)* | `Value` model + `Encode`/`Decode` traits & derives; JSON/XML/YAML/TOML; content-type dispatch; shared parser limits; a streaming JSON fast path |
| [`codec::schema`](schema.md) | `codec` | JSON-Schema **subset** validator over `Value` |
| [derive macros](derive.md) | (with `codec`) | `#[derive(Encode, Decode)]` and `#[derive(FerrolyError)]` (and, with `protobuf`, `proto::Message` / `Oneof` / `Enumeration`) |
| [`cli`](cli.md) | `cli` | Command-line parser: subcommands, typed, repeated and counted flags, groups, global flags, help, bash/zsh completion |
| [`config`](config.md) | `config` | Layered configuration: env + files (JSON/YAML/TOML/XML/.properties) + CLI args; profiles, `${VAR}` interpolation, secrets from files, validation, provenance, unknown-key checks, config file discovery, secret references |
| [`errutils`](errutils.md) | `errutils` *(default)* | `MultiError`, `.context()`, error classification (`Classify` / `ErrorKind`); the `#[derive(FerrolyError)]` typed-error macro |
| [`hash`](hash.md) | `hash` | SHA-256/384/512, SHA-1, generic HMAC, HKDF, PBKDF2, base64, hex, `Secret`, random bytes; MD5 / CRC-32 checksums |
| [`sig`](sig.md) | `sig` | Verify-only Ed25519 (RFC 8032): Ed25519, Ed25519ctx, Ed25519ph; strict decoding; streaming verifiers that compose with `hash` |
| [`fsutils`](fsutils.md) | `fsutils` | Content-type detection; atomic writes, temp files, lock files; memory-mapped reads (`Mmap`); human-readable byte sizes (`size`, `ByteSize`) |
| [`lifecycle`](lifecycle.md) | `lifecycle` | `Component` + `ComponentManager`: dependency-ordered start/stop, rollback, supervision, async health and readiness probes, shutdown and reload handles |

### Data & AI
| Module | Feature | What it does |
|---|---|---|
| [`genai`](genai.md) | `genai` (+ `openai`/`claude`/`ollama`) | Provider-agnostic LLM interface: chat, typed streaming, tools, structured output, embeddings, prompt templates, observers, a mock provider |
| [`genai::router`](model-router.md) | `genai` | Model router: capability, cost and rank-based selection; budgets; sticky routing; breaker-guarded, error-classified fallback |
| [`proto`](proto.md) | `protobuf` | Protocol Buffers wire codec, `#[derive(proto::Message)]`, `.proto` parser and `build.rs` code generator with service descriptors — no `protoc` |
| [`template`](template.md) | `template` | Sandboxed, bounded Jinja2-subset template engine: runs LLM chat templates (`tokenizer.chat_template`) with typed, positioned errors and limits |
| [`vectorstore`](vectorstore.md) | `vectorstore` | `VectorStore` trait + in-memory backend with metadata filters, namespaces and snapshots; text chunking |

### Web & networking
| Module | Feature | What it does |
|---|---|---|
| [`http`](http.md) | `http` | Hand-rolled HTTP/1.1 client + server: strict parsing, limits and timeouts, TLS / mTLS and certificate reload, proxies, redirects, SSE, resumable downloads |
| [`http` (HTTP/2)](http.md#http2) | `http2` | Hand-rolled HTTP/2 for the same server and client: ALPN `h2`, h2c prior knowledge, HPACK, flow control, multiplexing, trailers, bidirectional streaming |
| [`grpc`](grpc.md) | `grpc` | gRPC over HTTP/2: the four call shapes, status, metadata, deadlines, a `.proto` service generator and the health service |
| [`turbo`](turbo.md) | `turbo` | Router: groups and mounts, async auth, JWT / JWKS, CORS, security headers, rate limiting, cookies, forms, multipart, static files, problem+json |
| [`rest`](rest.md) | `rest` | REST client (retries, breakers, pagination) + lifecycle-managed server with TLS and health endpoints |
| [`ws`](ws.md) | `ws` | RFC 6455 WebSocket client + server: async handlers, hub and rooms, origin policy, upgrade hook, keepalive, reconnect |
| [`clients`](clients.md) | `clients` | Resilience: retry with jitter and budgets, circuit breaker, bulkhead, rate limiter, `Policy`; auth providers incl. OAuth 2.0 client credentials |
| [`rt`](rt.md) | `rt` | Async runtime surface — tokio re-exported, `#[ferroly::rt::main]` / `test`, `CancellationToken`, `TaskTracker`, `shutdown_signal` |

### Infrastructure & ops
| Module | Feature | What it does |
|---|---|---|
| [`messaging`](messaging.md) | `messaging` | Provider-agnostic messaging + an in-process bus: queues and topics, typed attributes, subscription filters, delivery context and deferred acks, priority, scheduling, ack / retry / DLQ with reasons, request-reply, graceful drain |
| [`vfs`](vfs.md) | `vfs` | Virtual filesystem trait; rooted local and in-memory backends, scheme registry, atomic and streaming writes with abort, write preconditions, etags, glob |
| [`store`](store.md) | `store-local`, `store`, `store-s3`, `store-hf`, `store-oci` | Synchronous local content-addressed store (layouts, listing, short digests, non-destructive checks, cross-process lock; no tokio or HTTP) and an async artifact cache (verify-on-write, atomic commit, resume, concurrent-safe) with S3 (SigV4), Hugging Face Hub and OCI registry fetch adapters |
| [`log`](log.md) | `log` | Structured logger: JSON / text, multiple sinks, rotation, redaction, per-target levels, sampling, trace correlation |
| [`log::bridge`](log.md#bridging-the-log-crate) | `log-bridge` | Routes records from the `log` facade crate into a ferroly `Logger` |
| [`metrics`](metrics.md) | `metrics` | Counters, gauges, histograms (int and float); Prometheus and OpenMetrics text; cardinality guard; RED middleware for `turbo` |
| [`obs`](obs.md) | `obs` | Tracing: async-safe spans, W3C trace context, sampling, exporters (JSON, plain text, OTLP/HTTP) |
| [`auth`](auth.md) | `auth` | JWT: HS256/384/512 with a `Validation` policy; RS256 / ES256 via JWKS (with `http`); scopes and roles |

---

## Feature matrix

Enable only what you use — unused modules and their dependencies never compile.

| Feature | Default? | Pulls in | Needs `tokio`? | Needs TLS? |
|---|:--:|---|:--:|:--:|
| `errutils` | ✅ | — | — | — |
| `codec` | ✅ | — | — | — |
| `hash` | | — | — | — |
| `sig` | | `hash` | — | — |
| `cli` | | — | — | — |
| `rt` | | tokio (all of it) | ✅ | — |
| `rt-test-util` | | `rt` + tokio `test-util` (paused clock; for dev-dependencies) | ✅ | — |
| `config` | | `codec` | — | — |
| `fsutils` | | — | — | — |
| `lifecycle` | | tokio | ✅ | — |
| `http` | | tokio + rustls | ✅ | ✅ |
| `http2` | | `http` | ✅ | for ALPN `h2` |
| `tls-ring` | ✅ | `ring` as the rustls crypto backend (only with `http`; see [TLS crypto backend](http.md#tls-crypto-backend)) | — | — |
| `clients` | | `http`, `hash`, `codec` | ✅ | ✅ |
| `turbo` | | `http`, `codec` | ✅ | ✅ |
| `rest` | | `turbo`, `clients`, `lifecycle`, `codec` | ✅ | ✅ |
| `ws` | | `http`, `hash`, `lifecycle` | ✅ | ✅ |
| `genai` | | `codec`, `clients`, `http` | ✅ | ✅ |
| `openai` / `claude` / `ollama` | | `genai` | ✅ | ✅ |
| `all-providers` | | all three | ✅ | ✅ |
| `vectorstore` | | `codec` | — | — |
| `template` | | `codec` | — | — |
| `auth` | | `codec`, `hash` | — | — |
| `log` | | `codec` | — | — |
| `log-bridge` | | `log` + the `log` facade crate (no transitive deps) | — | — |
| `metrics` | | — | — | — |
| `obs` | | `codec` (+ `http` for OTLP) | for OTLP | — |
| `messaging` | | `codec`, `lifecycle` | ✅ | — |
| `vfs` | | tokio | ✅ | — |
| `store-local` | | `hash`, `fsutils` | — | — |
| `store` | | `store-local`, `http`, `hash` | ✅ | ✅ |
| `store-s3` / `store-oci` | | `store`, `codec` | ✅ | ✅ |
| `store-hf` | | `store` | ✅ | ✅ |
| `protobuf` | | — | — | — |
| `grpc` | | `http2`, `protobuf`, `hash` | ✅ | for ALPN `h2` |
| `full` | | everything above | ✅ | ✅ |

```toml
[dependencies]
# just what you need
ferroly = { version = "0.7", default-features = false, features = ["genai", "openai", "vectorstore"] }
# or everything
ferroly = { version = "0.7", features = ["full"] }
```

---

## Architecture

Modules layer cleanly — higher layers reuse lower ones, never the reverse:

```
        rest ───────────────┐
          │                 │
        turbo ── codec      │
          │       │         │
   genai ─┼── clients ──────┤
     │    │       │         │
 vectorstore  ┌── http ─────┤   ws     messaging   vfs
     │        │   │         │    │        │         │
   codec   rustls tokio ────┴────┴────────┴─────────┘
     │                              │
   (schema, derive)             lifecycle ── log ── auth ── config ── errutils ── fsutils
```

- **`codec`** is the shared data model (`Value`, `Encode`/`Decode`) used by config,
  genai, rest, messaging, vectorstore, schema.
- **`http`** is the transport; `turbo` (router) and `rest` build on it; TLS is
  isolated behind an internal `transport` boundary so it stays swappable.
- **`lifecycle`** is the start/stop + health backbone; `rest::Server`,
  `ws::WsServer` and `messaging::LocalProvider` implement its `Component` trait.
- **`hash`** holds the one implementation of SHA-2, HMAC and base64 that `auth`,
  `ws` and `clients` share.
- **`log`**, **`metrics`** and **`obs`** are wired into `turbo` (access log,
  request id, RED metrics, server spans), `rest` (trace-context injection),
  `genai` (observers) and `messaging` (observer hooks) behind their features, and
  stay optional and vendor-neutral.

---

## Design philosophy & Go → Rust translation

Ferroly is a *translation*, not a transliteration. Go-style patterns were
rewritten to idiomatic Rust rather than transliterated:

| Go idiom | ferroly (Rust) |
|---|---|
| `msg.Rsvp(true/false)` (mutate-in-place ack) | handler returns an [`Ack`](messaging.md) enum |
| `*url.URL` + a `XxxRaw(string)` twin per method | one method taking `&str` (no `Raw` twins) |
| sentinel errors (`ErrNotExist`, `ErrSkipDir`) | error enums + a [`WalkAction`](vfs.md) return enum |
| `context.Context` threaded everywhere | dropped — async cancels by dropping the future |
| `Header`/`Body`/`Message` interface trio | one `Message` struct + codec |
| string-keyed provider registries | construct the provider, or hold `Arc<dyn Trait>` |
| variadic `...Option` functional options | typed options structs / builders |

What the toolkit deliberately leaves out is listed under
[What is not here](../crates/ferroly/README.md#what-is-not-here).

---

## Conventions & guarantees

- **Apache-2.0 OR MIT** licensed.
- **No copyleft, no surprise deps** — CI fails on any new direct dependency, and
  `cargo deny` checks licences and advisories.
- Codec terminology is **encode / decode** throughout (the traits are `Encode` /
  `Decode`).
- Every module is tested; every feature lints and documents cleanly on its own
  under `clippy -D warnings` and `rustdoc -D warnings`; the untrusted-input
  parsers are fuzzed.
- **These docs are source of truth** — any API change updates the relevant page
  here alongside the code.

## Next steps

- **[Getting Started](getting-started.md)** — install, pick features, first app.
- Jump to any module page above.
- **[Crate README](../crates/ferroly/README.md)** — a tour of every module, the security posture and known limits.
- **[Changelog](../CHANGELOG.md)** — what changed in each release.

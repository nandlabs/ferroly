# Ferroly — Developer Guide

**A self-contained, dependency-minimal Rust toolkit of enterprise utilities.**

Ferroly bundles data encoding, configuration, an LLM/GenAI abstraction, an HTTP
client + server, a first-class router, WebSockets, messaging, a virtual
filesystem, component lifecycle, structured logging, auth, and more — into a
**single feature-gated crate**.

Its defining constraint: **near-zero external dependencies.** The entire runtime
dependency tree is just **`tokio`** (async runtime) and the **`rustls`** TLS
stack. Everything else — JSON/XML/YAML/TOML codecs, HTTP/1.1, WebSocket framing,
SHA-1/SHA-256/HMAC, base64, the derive macros — is implemented in-house. If a
module doesn't need async or TLS, it compiles with **no external crates at all**.

> New here? Start with **[Getting Started](getting-started.md)**.

---

## Module map

Everything lives under one crate; each area is a **cargo feature** you opt into.

### Foundation (mostly std-only)
| Module | Feature | What it does |
|---|---|---|
| [`codec`](codec.md) | `codec` *(default)* | `Value` model + `Encode`/`Decode` traits & derives; JSON/XML/YAML/TOML; content-type dispatch; a streaming JSON fast-path near serde_json speed |
| [`codec::schema`](schema.md) | `codec` | JSON-Schema **subset** validator over `Value` |
| [derive macros](derive.md) | (with `codec`) | `#[derive(Encode, Decode)]` and `#[derive(FerrolyError)]` |
| [`cli`](cli.md) | `cli` | Command-line parser: subcommands, typed flags/options, env fallback, generated `--help` |
| [`config`](config.md) | `config` | Layered configuration: env + file (JSON/YAML/XML/**.properties**) + **CLI args**, bound to a struct via `Decode` |
| [`errutils`](errutils.md) | `errutils` *(default)* | `MultiError` — aggregate multiple errors into one; the `#[derive(FerrolyError)]` typed-error macro |
| [`hash`](hash.md) | `hash` | Streaming SHA-256 / SHA-1 / HMAC-SHA256 + a hex `Digest` type |
| [`fsutils`](fsutils.md) | `fsutils` | MIME detection (extension table + magic-byte sniffing); read-only memory-mapped files (`Mmap`) |
| [`lifecycle`](lifecycle.md) | `lifecycle` | `Component` + `ComponentManager` (dependency-ordered start/stop) + **health probes** |

### Data & AI
| Module | Feature | What it does |
|---|---|---|
| [`genai`](genai.md) | `genai` (+ `openai`/`claude`/`ollama`) | Provider-agnostic LLM interface: chat, streaming, **embeddings**, **function-calling tools**, **structured output**, prompt templates |
| [`vectorstore`](vectorstore.md) | `vectorstore` | `VectorStore` trait + in-memory backend + cosine/dot/euclidean — pairs with genai embeddings |

### Web & networking
| Module | Feature | What it does |
|---|---|---|
| [`http`](http.md) | `http` | Hand-rolled HTTP/1.1 client + server (chunked, **SSE streaming**, TLS) |
| [`turbo`](turbo.md) | `turbo` | First-class router: route groups, onion middleware, rate-limit, JWT auth, access log, trace-context, content negotiation |
| [`rest`](rest.md) | `rest` | REST client + server framework (codec-aware, lifecycle-integrated, health endpoints) |
| [`ws`](ws.md) | `ws` | Hand-rolled RFC 6455 WebSocket client + server |
| [`clients`](clients.md) | `clients` | Resilience primitives: auth providers, retry/backoff, circuit breaker |
| [`rt`](rt.md) | `rt` | Async runtime surface — the `tokio` primitives (spawn, channels, sync, time, TCP) re-exported |

### Infrastructure & ops
| Module | Feature | What it does |
|---|---|---|
| [`messaging`](messaging.md) | `messaging` | Provider-agnostic messaging + a local tokio-channel bus (ack/redelivery/DLQ, routing keys, concurrency, backpressure) |
| [`vfs`](vfs.md) | `vfs` | Virtual filesystem trait + local backend on async `tokio::fs` |
| [`log`](log.md) | `log` | Enterprise structured logger (timestamps, typed fields, async appender, trace-ID correlation) |
| [`metrics`](metrics.md) | `metrics` | Dependency-free counters/gauges/histograms + Prometheus `/metrics` exposition; RED middleware for `turbo` |
| [`auth`](auth.md) | `auth` | HS256 JWT mint/verify on from-scratch SHA-256/HMAC |

---

## Feature matrix

Enable only what you use — unused modules and their dependencies never compile.

| Feature | Default? | Pulls in | Needs `tokio`? | Needs TLS? |
|---|:--:|---|:--:|:--:|
| `errutils` | ✅ | — | — | — |
| `codec` | ✅ | — | — | — |
| `hash` | | — | — | — |
| `cli` | | — | — | — |
| `rt` | | tokio | ✅ | — |
| `config` | | `codec` | — | — |
| `fsutils` | | — | — | — |
| `lifecycle` | | tokio | ✅ | — |
| `http` | | tokio + rustls | ✅ | ✅ |
| `clients` | | `http` | ✅ | ✅ |
| `turbo` | | `http`, `codec` | ✅ | ✅ |
| `rest` | | `turbo`, `clients`, `lifecycle`, `codec` | ✅ | ✅ |
| `ws` | | `http`, `hash` | ✅ | ✅ |
| `genai` | | `codec`, `clients`, `http` | ✅ | ✅ |
| `openai` / `claude` / `ollama` | | `genai` | ✅ | ✅ |
| `all-providers` | | all three | ✅ | ✅ |
| `vectorstore` | | `codec` | — | — |
| `auth` | | `codec`, `hash` | — | — |
| `log` | | `codec` | — | — |
| `metrics` | | — | — | — |
| `messaging` | | `codec`, `lifecycle` | ✅ | — |
| `vfs` | | tokio | ✅ | — |
| `full` | | everything above | ✅ | ✅ |

```toml
[dependencies]
# just what you need
ferroly = { version = "0.1", default-features = false, features = ["genai", "openai", "vectorstore"] }
# or everything
ferroly = { version = "0.1", features = ["full"] }
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
- **`lifecycle`** is the start/stop + health backbone; `rest` and `messaging`
  providers implement its `Component` trait.
- **`log`** is wired into `turbo` (access log, trace-context) and `messaging`
  (observer) via narrow hooks, but stays optional and vendor-neutral.

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

The full rationale for each decision is recorded in the
[roadmap & design decisions](roadmap.md).

---

## Conventions & guarantees

- **Apache-2.0 OR MIT** licensed.
- **No copyleft, no surprise deps** — `cargo deny` enforces the runtime budget.
- Codec terminology is **encode / decode** throughout (the traits are `Encode` /
  `Decode`).
- Every module is tested; the workspace runs clean under `clippy -D warnings`
  and `rustdoc -D warnings`.
- **These docs are source of truth** — any API change updates the relevant page
  here alongside the code.

## Next steps

- **[Getting Started](getting-started.md)** — install, pick features, first app.
- Jump to any module page above.
- **[Roadmap & design decisions](roadmap.md)** — the Go → Rust translation rationale.

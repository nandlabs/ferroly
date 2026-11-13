# ferroly

**A self-contained toolkit of enterprise building blocks for Rust** — encoding,
configuration, an HTTP client and server, a router, WebSockets, an LLM
abstraction with a model router, resilience policies, auth, logging, metrics,
tracing and more, in one feature-gated crate.

Ferroly's defining rule is **near-zero external dependencies**. The whole
runtime dependency tree is `tokio` and the `rustls` TLS stack; everything else —
the JSON/XML/YAML/TOML codecs, HTTP/1.1, WebSocket framing, SHA-2 and HMAC,
base64, JWT, the derive macros — is implemented in this repository. Modules that
need neither async nor TLS compile with no external crates at all.


- [Installation](#installation)
- [Features](#features)
- [A tour](#a-tour)
- [Security posture](#security-posture)
- [What is not here](#what-is-not-here)
- [Compatibility](#compatibility)

## Installation

```toml
[dependencies]
# pick the modules you use — nothing else is compiled
ferroly = { version = "0.4", features = ["rest", "auth", "log", "metrics"] }

# or everything
ferroly = { version = "0.4", features = ["full"] }
```

Default features are `codec` and `errutils`. Each feature turns on only the
`tokio` features it needs; enable `rt` to get the full runtime surface
(`ferroly::rt`) without depending on `tokio` yourself.

## Features

| Feature | Module | What it provides | Pulls in |
|---|---|---|---|
| `codec` *(default)* | `ferroly::codec` | `Value` model, `Encode` / `Decode` traits and derives, JSON, XML, YAML and TOML, content-type dispatch, shared parser limits, a JSON-Schema subset validator | — |
| `errutils` *(default)* | `ferroly::errutils` | `MultiError`, `.context()`, error classification (`Classify` / `ErrorKind`); `#[derive(FerrolyError)]` | — |
| `config` | `ferroly::config` | Layered configuration from files, environment and CLI; profiles, `${VAR}` interpolation, secrets from files, validation | `codec` |
| `cli` | `ferroly::cli` | Command-line parser: subcommands, typed and repeated flags, groups, help, shell completion | — |
| `hash` | `ferroly::hash` | SHA-256/384/512, SHA-1, HMAC, HKDF, PBKDF2, base64, hex, `Secret`, random bytes; MD5 and CRC-32 checksums | — |
| `fsutils` | `ferroly::fsutils` | Content-type detection, atomic writes, temp files, lock files, memory-mapped reads | — |
| `auth` | `ferroly::auth` | JWT: HS256/384/512 mint and verify with a `Validation` policy; RS256 / ES256 verification from JWKS (with `http`); scopes | `codec`, `hash` |
| `lifecycle` | `ferroly::lifecycle` | Component start/stop ordering, rollback, supervision, health and readiness probes, graceful shutdown | tokio |
| `rt` | `ferroly::rt` | The async runtime surface: tasks, channels, time, I/O, signals, `#[ferroly::rt::main]` / `#[ferroly::rt::test]`, `CancellationToken`, `TaskTracker` | tokio (full) |
| `http` | `ferroly::http` | HTTP/1.1 client and server: strict parsing, limits and timeouts, TLS and mutual TLS, proxies, redirects, SSE, resumable downloads | tokio, rustls |
| `clients` | `ferroly::clients` | Retry with jitter and budgets, circuit breaker, bulkhead, rate limiter, a composable `Policy`, auth providers including OAuth 2.0 client credentials | `http`, `hash`, `codec` |
| `turbo` | `ferroly::turbo` | Router and middleware: groups, async auth, CORS, security headers, rate limiting, JWT, cookies, forms, multipart, static files, problem+json | `http`, `codec` |
| `rest` | `ferroly::rest` | REST client (retries, breakers, pagination) and a lifecycle-managed REST server with health endpoints | `turbo`, `clients`, `lifecycle` |
| `ws` | `ferroly::ws` | WebSocket client and server (RFC 6455): async handlers, hub and rooms, origin policy, upgrade hook, keepalive, reconnect | `http`, `lifecycle`, `hash` |
| `genai` | `ferroly::genai` | Provider-agnostic LLM interface: chat, typed streaming, tools, structured output, embeddings, prompt templates, a model router with budgets and fallback, a mock provider | `codec`, `clients`, `http` |
| `openai` / `claude` / `ollama` | — | Provider implementations (`all-providers` enables all three) | `genai` |
| `vectorstore` | `ferroly::vectorstore` | `VectorStore` trait, in-memory store with metadata filters, namespaces and snapshots, text chunking | `codec` |
| `messaging` | `ferroly::messaging` | In-process queues and topics: ack / retry / dead-letter, backoff, TTL, request-reply, graceful drain | `codec`, `lifecycle` |
| `vfs` | `ferroly::vfs` | Virtual filesystem trait; rooted local backend, in-memory backend, scheme registry, atomic and streaming writes | tokio |
| `log` | `ferroly::log` | Structured logging: JSON or text, multiple sinks, file rotation, redaction, per-target levels | `codec` |
| `metrics` | `ferroly::metrics` | Counters, gauges, histograms; Prometheus and OpenMetrics text; cardinality guard | — |
| `obs` | `ferroly::obs` | Tracing: spans, W3C trace context, sampling, OTLP/HTTP export | `codec` (+ `http` for OTLP) |
| `rt-test-util` | `ferroly::rt::time` | The paused test clock (`pause`, `advance`, `resume`) — for `[dev-dependencies]` | `rt` |
| `full` | — | Everything above except `rt-test-util` | |

## A tour

Every example below is compiled as part of the crate's test suite.

### Encoding and decoding

One set of traits and derives covers JSON, XML, YAML and TOML.

```rust
use ferroly::codec::{json, toml, yaml, Decode, Encode};

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(rename_all = "camelCase")]
struct Service {
    name: String,
    listen_port: u16,
    #[ferroly(default)]
    tags: Vec<String>,
    #[ferroly(skip_none)]
    owner: Option<String>,
}

#[derive(Encode, Decode, PartialEq, Debug)]
#[ferroly(tag = "type", rename_all = "snake_case")]
enum Event {
    Started { at: u64 },
    Stopped { at: u64, reason: String },
}

fn main() {
    let svc = Service { name: "api".into(), listen_port: 8080, tags: vec![], owner: None };

    let text = json::encode(&svc);
    assert_eq!(text, r#"{"name":"api","listenPort":8080,"tags":[]}"#);
    assert_eq!(json::decode::<Service>(&text).unwrap(), svc);

    // The same type reads YAML and TOML.
    let from_yaml: Service = yaml::decode("name: api\nlistenPort: 8080 # comment\n").unwrap();
    let from_toml: Service = toml::decode("name = \"api\"\nlistenPort = 8080\n").unwrap();
    assert_eq!(from_yaml, from_toml);

    // Enums with data, internally tagged.
    let event = Event::Stopped { at: 7, reason: "deploy".into() };
    assert_eq!(json::encode(&event), r#"{"type":"stopped","at":7,"reason":"deploy"}"#);

    // JSON and TOML decode strictly: a string is not a number.
    assert!(json::decode::<Service>(r#"{"name":"api","listenPort":"8080"}"#).is_err());
}
```

Parsers enforce a shared budget (nesting depth, input size, entry count) and
report line and column on errors. See the
[codec guide](https://github.com/nandlabs/ferroly/blob/main/docs/codec.md) and
the [derive reference](https://github.com/nandlabs/ferroly/blob/main/docs/derive.md).

### Errors

```rust
use ferroly::errutils::{Classify, Context, ErrorKind};
use ferroly::FerrolyError;

#[derive(Debug, FerrolyError)]
enum StoreError {
    #[error("object {key} not found")]
    NotFound { key: String },
    #[error("storage unavailable")]
    Io(#[from] std::io::Error),
}

impl Classify for StoreError {
    fn kind(&self) -> ErrorKind {
        match self {
            StoreError::NotFound { .. } => ErrorKind::NotFound,
            StoreError::Io(_) => ErrorKind::Unavailable,
        }
    }
}

fn main() {
    let err = StoreError::NotFound { key: "a/b".into() };
    assert_eq!(err.to_string(), "object a/b not found");
    assert_eq!(err.kind().http_status(), 404);
    assert!(!err.kind().is_retryable());

    // Say what you were doing; the original error stays the source.
    let failed = std::fs::read("/no/such/file").context("loading the license").unwrap_err();
    assert_eq!(failed.to_string(), "loading the license");
    assert!(std::error::Error::source(&failed).is_some());
}
```

### Configuration

```rust,no_run
use ferroly::codec::Decode;
use ferroly::config::Config;

#[derive(Decode)]
struct Settings {
    host: String,
    port: u16,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::builder()
        .merge_file("config/app.toml")?           // base layer; must exist
        .merge_file_optional("config/local.toml")? // developer override, if present
        .merge_env("APP_")                         // APP_PORT=9090, APP_DB__URL=…
        .build();

    config.require(&["host", "port"])?;            // reports every missing key
    let settings: Settings = config.extract()?;
    println!("listening on {}:{}", settings.host, settings.port);
    println!("{}", config.to_redacted_string());   // secrets masked
    Ok(())
}
```

### An HTTP service

```rust,no_run
use std::time::Duration;

use ferroly::auth::Validation;
use ferroly::http::{HttpResponse, ServerConfig, StatusCode};
use ferroly::turbo::{require_scopes, Cors, JwtAuth, RateLimit, Router, SecurityHeaders};

#[ferroly::rt::main]
async fn main() -> Result<(), ferroly::http::HttpError> {
    let router = Router::new()
        .request_id()
        .cors(Cors::new().allow_origin("https://app.example.com"))
        .security_headers(SecurityHeaders::new())
        .rate_limit_with(RateLimit::new(50.0, 100.0)) // per client address
        .get("/health", |_| async { HttpResponse::text(StatusCode::OK, "ok") })
        .group("/api", |api| {
            api.jwt(JwtAuth::hs256(
                "a-shared-secret-of-at-least-16-bytes",
                Validation::new().issuer("https://issuer.example").audience("orders-api"),
            ))
            .filter(require_scopes(&["orders:read"]))
            .get("/orders/:id", |ctx| async move {
                let id = ctx.param("id").unwrap_or_default().to_string();
                let subject = ctx.claims().and_then(|c| c.subject().map(str::to_string));
                HttpResponse::text(StatusCode::OK, format!("order {id} for {subject:?}"))
            })
        });

    let config = ServerConfig {
        max_body_bytes: 1024 * 1024,
        handler_timeout: Some(Duration::from_secs(30)),
        ..ServerConfig::default()
    };
    router
        .serve_with_config("0.0.0.0:8080", config, async {
            // Ctrl-C or SIGTERM: stop accepting, drain in-flight requests, return.
            if let Err(e) = ferroly::rt::shutdown_signal().await {
                eprintln!("cannot install signal handlers: {e}");
            }
        })
        .await
}
```

The server parses requests strictly, caps headers, bodies and connection counts,
times out slow clients, and turns a handler panic into a `500`. Request paths
are normalised once, before authentication and routing, so a filter on `/api`
cannot be sidestepped with `//api/x`. TLS, mutual TLS and certificate rotation
are available through `serve_tls` and `serve_tls_reloadable`.

### Calling other services

```rust,no_run
use std::time::Duration;

use ferroly::clients::{Jitter, RetryPolicy};
use ferroly::codec::{Decode, Encode};
use ferroly::rest::{Client, ClientOptions};

#[derive(Encode)]
struct NewOrder {
    sku: String,
    quantity: u32,
}

#[derive(Decode)]
struct Order {
    id: u64,
}

#[ferroly::rt::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::with_options(
        ClientOptions::builder()
            .base_url("https://orders.internal.example")
            .request_timeout(Duration::from_secs(10))
            .retry_policy(RetryPolicy::default().with_jitter(Jitter::Full))
            .build(),
    );

    let order: Order = client
        .post("/customers/${customer}/orders")
        .path_param("customer", "acme/eu") // encoded as one path segment
        .body(&NewOrder { sku: "W-1".into(), quantity: 2 })
        .send()
        .await?
        .error_for_status()?               // a non-2xx status becomes an error
        .decode()?;
    println!("created order {}", order.id);
    Ok(())
}
```

Idempotent requests are retried on transport errors and on `502` / `503` /
`504` / `429`, honouring `Retry-After`. For anything that is not HTTP,
`ferroly::clients::Policy` composes a timeout, retries, a circuit breaker, a
bulkhead and a rate limiter around any async operation.

### Tokens

```rust
use std::time::{SystemTime, UNIX_EPOCH};

use ferroly::auth::{decode_hs256_with, encode_hmac, Algorithm, Scopes, Validation};
use ferroly::codec::Value;

fn main() {
    let secret = b"an-example-secret-of-32-bytes!!!";
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();

    let claims = Value::Object(vec![
        ("iss".into(), "https://issuer.example".into()),
        ("aud".into(), "orders-api".into()),
        ("sub".into(), "u123".into()),
        ("scope".into(), "orders:read".into()),
        ("exp".into(), Value::UInt(now + 3600)),
    ]);
    let token = encode_hmac(Algorithm::Hs256, &claims, secret).unwrap();

    let policy = Validation::new().issuer("https://issuer.example").audience("orders-api");
    let verified = decode_hs256_with(&token, secret, &policy).unwrap();
    assert!(Scopes::from_claims(&verified).has_scope("orders:read"));

    // A token for another audience is rejected.
    let other = Validation::new().audience("billing-api");
    assert!(decode_hs256_with(&token, secret, &other).is_err());
}
```

With the `http` feature, `Jwks` and `RemoteJwks` verify RS256 and ES256 tokens
from an identity provider and follow key rotation.

### Language models

```rust,no_run
use ferroly::genai::{ClaudeProvider, CompletionRequest, GenAiProvider, Message};

#[ferroly::rt::main]
async fn main() -> Result<(), ferroly::genai::GenAiError> {
    let provider = ClaudeProvider::new(std::env::var("ANTHROPIC_API_KEY").unwrap(), None);

    let request = CompletionRequest::builder("claude-sonnet-5-5")
        .message(Message::system("You are terse."))
        .message(Message::user("Say hello in French."))
        .build();

    let response = provider.complete(request).await?;
    println!("{}", response.text());
    Ok(())
}
```

The same request works against `OpenAiProvider` and `OllamaProvider`. Streams
are typed events (text, tool-call fragments, usage, finish); the model router
picks a provider and model by capability, cost and budget and fails over on
transient errors. `MockProvider` scripts responses for your own tests:

```rust
use ferroly::genai::{CompletionRequest, GenAiError, GenAiProvider, Message, MockProvider};

#[ferroly::rt::main]
async fn main() {
    let mock = MockProvider::new("mock");
    mock.push_error(GenAiError::Api { status: 503, message: "busy".into() });
    mock.push_text("a summary");

    let request = CompletionRequest::builder("any").message(Message::user("text")).build();
    assert!(mock.complete(request.clone()).await.is_err());
    assert_eq!(mock.complete(request).await.unwrap().text(), "a summary");
    assert_eq!(mock.call_count(), 2);
}
```

### WebSockets

```rust,no_run
use std::time::Duration;

use ferroly::ws::{Message, WsClient, WsOptions};

#[ferroly::rt::main]
async fn main() -> Result<(), ferroly::ws::WsError> {
    let options = WsOptions::default()
        .header("Authorization", "Bearer token")
        .keepalive(Duration::from_secs(20), Duration::from_secs(10));
    let mut client = WsClient::dial("wss://example.com/ws", options).await?;

    client.send(Message::text("hello")).await?;
    while let Some(message) = client.recv().await {
        match message {
            Message::Text(text) => println!("{text}"),
            Message::Binary(bytes) => println!("{} bytes", bytes.len()),
            Message::Close { code, reason } => println!("closed: {code} {reason}"),
        }
    }
    Ok(())
}
```

The server side offers per-connection async handlers, an upgrade hook for
authentication, an origin policy, a `Hub` with rooms for broadcast, and bounded
queues so a slow peer cannot grow memory.

### Running a service

```rust
use std::sync::Arc;

use ferroly::lifecycle::{ComponentManager, LifecycleError, SimpleComponent};

#[ferroly::rt::main]
async fn main() -> Result<(), LifecycleError> {
    let manager = ComponentManager::new();
    manager.register(Arc::new(SimpleComponent::new(
        "db",
        || Box::pin(async { Ok(()) }),
        || Box::pin(async { Ok(()) }),
    )))?;
    manager.register(Arc::new(SimpleComponent::new(
        "api",
        || Box::pin(async { Ok(()) }),
        || Box::pin(async { Ok(()) }),
    )))?;
    manager.add_dependency("api", "db")?; // db starts first, stops last

    manager.start_all().await?;           // a failed start rolls back what it started
    assert!(manager.is_ready());
    manager.stop_all().await?;
    Ok(())
}
```

`rest::Server`, `ws::WsServer` and `messaging::LocalProvider` are components, so
one manager starts, health-checks and drains a whole process.

### Logs, metrics and traces

```rust
use ferroly::log::Logger;
use ferroly::metrics::Registry;
use ferroly::obs::{Span, SpanKind};

fn main() {
    // Structured logs; fields named like secrets are redacted by default.
    let log = Logger::json().with("service", "api");
    log.info("request handled", &[("status", 200.into()), ("token", "abc".into())]);

    // Prometheus metrics.
    let registry = Registry::new();
    registry.counter("jobs_total", "Jobs processed", &[("queue", "email")]).inc();
    let latency = registry.histogram(
        "job_seconds",
        "Job duration",
        &[],
        ferroly::metrics::DEFAULT_BUCKETS,
    );
    latency.observe(0.042);
    assert!(registry.encode().contains("jobs_total{queue=\"email\"} 1"));

    // Spans; `traceparent` in and out via ferroly::obs::extract / inject.
    let span = Span::new("job").kind(SpanKind::Consumer).field("queue", "email").enter();
    span.record("attempt", 1);
}
```

### More

| Module | Guide |
|---|---|
| `cli` | [Command-line parsing](https://github.com/nandlabs/ferroly/blob/main/docs/cli.md) |
| `hash` | [Hashes, HMAC, KDFs, base64](https://github.com/nandlabs/ferroly/blob/main/docs/hash.md) |
| `messaging` | [Queues and topics](https://github.com/nandlabs/ferroly/blob/main/docs/messaging.md) |
| `vectorstore` | [Vector search and chunking](https://github.com/nandlabs/ferroly/blob/main/docs/vectorstore.md) |
| `vfs` / `fsutils` | [Virtual filesystem](https://github.com/nandlabs/ferroly/blob/main/docs/vfs.md), [file utilities](https://github.com/nandlabs/ferroly/blob/main/docs/fsutils.md) |
| `rt` | [Runtime surface](https://github.com/nandlabs/ferroly/blob/main/docs/rt.md) |
| everything | [Developer guide index](https://github.com/nandlabs/ferroly/blob/main/docs/README.md) |

## Security posture

Because Ferroly implements what is usually delegated to other crates, its
security surface lives here. What to know:

- **Untrusted input.** The four codecs enforce depth, size and entry limits and
  never panic on malformed input; they are covered by fuzz targets and a
  generative test suite. The HTTP server rejects ambiguous framing
  (`Transfer-Encoding` / `Content-Length` conflicts, malformed chunks, bare line
  feeds) instead of guessing. YAML is a strict subset: anything outside it is an
  error, never silently read as a string.
- **Limits and timeouts are on by default** for the HTTP client and server, the
  WebSocket client and server, and the GenAI providers. Defaults are documented
  per module and every one is configurable.
- **TLS** is `rustls`. Certificate verification cannot be disabled; add private
  CA roots or a client certificate through `ClientTlsConfig`.
- **Cryptographic primitives are hand-written and not FIPS-validated.**
  Asymmetric signature verification (RS256 / ES256) is delegated to the `rustls`
  crypto provider. If your environment requires validated cryptography, do not
  use `ferroly::hash` for it.
- **Secrets.** `Secret` redacts itself in `Debug` and `Display`; auth providers,
  provider API keys and configuration dumps do not print credentials; log fields
  with secret-like names are masked by default.
- **`unsafe`.** One audited module: the memory-map FFI in `fsutils`, reachable
  only through `unsafe fn Mmap::map`. The safe `Mmap::open` copies the file.
  Everything else is `unsafe`-free and the lint denies new uses.

Please report vulnerabilities privately — see
[SECURITY.md](https://github.com/nandlabs/ferroly/blob/main/SECURITY.md).

## What is not here

Ferroly is deliberately narrow in places. Plan around these:

- **HTTP/2 and gRPC.** The stack is HTTP/1.1. Terminate HTTP/2 at a proxy.
- **Response compression.** The client can decode `gzip` / `deflate`; the server
  does not compress.
- **Durable messaging and vector backends.** `messaging` and `vectorstore` ship
  in-process implementations behind traits (`Provider`, `VectorStore`). Nothing
  survives a restart unless you snapshot it or implement the trait over a
  broker or database.
- **Object storage.** `vfs` has local and in-memory backends; the `Vfs` trait is
  the extension point.
- **Identity flows.** JWT verification and the OAuth 2.0 client-credentials
  grant are included. OIDC discovery, authorization-code / PKCE, sessions and
  password hashing are not.
- **Hosted LLM platforms.** OpenAI (and compatible endpoints, including Azure
  OpenAI), Anthropic and Ollama are supported. Bedrock, Vertex and Gemini are
  not. Model ids and prices in the built-in catalog are defaults to override.
- **WebSocket compression** (`permessage-deflate`) and upgrading from a `turbo`
  route (the WebSocket server listens on its own port).
- **Format coverage.** XML has no namespaces or attribute mapping; TOML
  date-times are strings; the JSON-Schema validator covers a documented subset.
- **Facade bridges.** There is no bridge from the `log` or `tracing` crates.

## Compatibility

- **MSRV:** Rust 1.75.
- **Platforms:** Linux, macOS and Windows are tested in CI. `hash::random`
  returns an explicit error on platforms without `/dev/urandom`.
- **Versioning:** pre-1.0; a minor release may contain breaking changes, all
  listed in the
  [changelog](https://github.com/nandlabs/ferroly/blob/main/CHANGELOG.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

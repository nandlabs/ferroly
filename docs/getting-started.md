# Getting Started

[← Docs index](README.md)

This guide gets you from an empty project to working code with Ferroly, and
explains how to pick the right features.

## 1. Install

Ferroly is one crate with many optional modules. **Nothing but `codec` and
`errutils` is on by default**, so you turn on exactly what you use.

```toml
[dependencies]
# Everything (simplest while exploring):
ferroly = { version = "0.3", features = ["full"] }
```

Once you know what you need, trim it:

```toml
[dependencies]
# A JSON API server that talks to OpenAI and does vector search:
ferroly = { version = "0.3", default-features = false, features = ["rest", "openai", "vectorstore"] }
```

Most async modules also need a runtime; add `tokio` yourself when you write
`#[tokio::main]`:

```toml
tokio = { version = "1", features = ["full"] }
```

See the **[feature matrix](README.md#feature-matrix)** for what each feature pulls
in and whether it needs `tokio`/TLS. Rule of thumb:

- **std-only (no tokio):** `codec`, `errutils`, `fsutils`, `config`, `auth`,
  `log`, `vectorstore`.
- **needs `tokio`:** `lifecycle`, `messaging`, `vfs`.
- **needs `tokio` + TLS:** `http`, `clients`, `turbo`, `rest`, `ws`, `genai`.

## 2. Encode / decode some data

The [`codec`](codec.md) module is the shared data model. Derive `Encode`/`Decode`
(the in-house `serde` equivalent — the traits are **`Encode`/`Decode`**, and the
attribute is `#[ferroly(...)]`):

```rust
use ferroly::codec::{json, Encode, Decode};

#[derive(Encode, Decode, PartialEq, Debug)]
struct Config {
    name: String,
    port: u16,
}

let cfg = Config { name: "svc".into(), port: 8080 };
let text = json::encode(&cfg);                 // {"name":"svc","port":8080}
let back: Config = json::decode(&text).unwrap();
assert_eq!(back, cfg);
```

The same value round-trips through XML and YAML, and you can dispatch by
content-type with `codec::encode(ct, &v)` / `codec::decode(ct, bytes)`. See
[codec](codec.md).

## 3. Serve HTTP with the router

The [`turbo`](turbo.md) router is a first-class HTTP server (built on the
hand-rolled [`http`](http.md) stack):

```rust
use ferroly::turbo::Router;
use ferroly::http::{HttpResponse, StatusCode};

#[tokio::main]
async fn main() -> Result<(), ferroly::http::HttpError> {
    let router = Router::new()
        .get("/greet/:name", |ctx| async move {
            let name = ctx.param("name").unwrap_or("world").to_string();
            HttpResponse::text(StatusCode::OK, format!("hi {name}"))
        });
    router.serve("127.0.0.1:8080").await
}
```

turbo gives you route groups, onion middleware, rate limiting, JWT auth, access
logging, request-id tracing, and content negotiation — all covered in
[turbo](turbo.md). For a batteries-included REST server with lifecycle
integration and health endpoints, use [rest](rest.md).

## 4. Call an LLM

The [`genai`](genai.md) module is vendor-neutral — you depend on a trait, not an
SDK:

```rust
use ferroly::genai::{CompletionRequest, GenAiProvider, Message, OpenAiProvider};

#[tokio::main]
async fn main() -> Result<(), ferroly::genai::GenAiError> {
    let provider = OpenAiProvider::new("sk-...", None);

    let request = CompletionRequest::builder("gpt-4o")
        .message(Message::user("Say hello in French."))
        .build();

    let response = provider.complete(request).await?;
    println!("{}", response.text());
    Ok(())
}
```

genai also does streaming, embeddings (which pair with [vectorstore](vectorstore.md)
for RAG), function-calling tools, and structured output that decodes straight
into your own struct.

## 5. Compose modules

The modules are designed to snap together:

- A [rest](rest.md) `Server` implements [lifecycle](lifecycle.md)'s `Component`,
  so a `ComponentManager` starts/stops it in dependency order with graceful
  shutdown, and exposes `/health` + `/ready`.
- [turbo](turbo.md)'s `access_log` and `trace_context` write to the
  [log](log.md) module, so every request gets a structured, trace-correlated log
  line.
- A [messaging](messaging.md) `LocalProvider` is also a lifecycle `Component`, and
  can report via a [log](log.md)-backed observer.
- [config](config.md) binds layered env/file/CLI configuration straight into your
  `Decode` structs.

A typical service wires these in `main`:

```rust,ignore
use ferroly::lifecycle::ComponentManager;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    ferroly::log::set_global(ferroly::log::Logger::json());

    let server = Arc::new(build_rest_server());       // impls Component
    let bus = Arc::new(build_message_bus());          // impls Component

    let manager = ComponentManager::new();
    manager.register(server);
    manager.register(bus);
    manager.start_and_wait().await.unwrap();          // start all, block on SIGINT, stop all
}
```

## 6. Where to go next

- **[codec](codec.md)** — the data model, derives, JSON/XML/YAML, schema validation.
- **[turbo](turbo.md)** / **[rest](rest.md)** — HTTP servers and middleware.
- **[genai](genai.md)** / **[vectorstore](vectorstore.md)** — LLMs and vector search.
- **[messaging](messaging.md)** / **[vfs](vfs.md)** — async infrastructure.
- **[lifecycle](lifecycle.md)** / **[log](log.md)** / **[config](config.md)** — ops glue.
- **[Roadmap & design decisions](roadmap.md)** — the Go → Rust translation rationale.

Every page follows the same shape: overview → enabling → quick start → full API
reference → in-depth feature sections with examples → error handling →
limitations → cross-links.

# ferroly::rest

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `rest` (implies `turbo` + `clients` + `codec` + `lifecycle` + `http`).

A full HTTP client + server framework, dependency-minimal and built entirely in-house.
Nothing here talks to `hyper`, `reqwest`, `axum`, or `tower`;
everything layers on ferroly's own crates.

- The **client** wraps [`ferroly::http::Client`](http.md) with codec-aware request/response
  bodies (via [`codec`](codec.md)), pluggable authentication and retry (via
  [`clients`](clients.md)), `${name}` path templating, and query building.
- The **server** wraps the first-class [`turbo`](turbo.md) router and the
  [`ferroly::http`](http.md) server, and integrates with [`lifecycle`](lifecycle.md) so it
  starts, binds, serves, and stops gracefully as a managed `Component`.

## Overview

`rest` is a thin, opinionated convenience layer. It does not reimplement HTTP — the actual
transport, TLS, connection handling, routing, middleware, and content negotiation all live
in [`http`](http.md) and [`turbo`](turbo.md). `rest` composes them into the two shapes an
application typically wants: a fluent typed HTTP client and a lifecycle-managed HTTP server.

Because the server builder exposes the underlying `turbo::Router` directly through
[`map_router`](#map_router--the-turbo-seam), no turbo capability is walled off: onion
middleware layers, request filters, and auth interceptors are all reachable.

## Enabling

```toml
[dependencies]
ferroly = { version = "*", features = ["rest"] }
```

The `rest` feature pulls in `turbo`, `clients`, `codec`, `lifecycle`, and `http`
automatically. The re-exports below come with it:

```rust
// Handler/response types re-exported by ferroly::rest for convenience:
pub use ferroly::http::{HttpResponse, Method, StatusCode};
pub use ferroly::turbo::{Ctx, Router};
// Own public items:
pub use ferroly::rest::{Client, ClientOptions, ClientOptionsBuilder, RequestBuilder, Response};
pub use ferroly::rest::{Server, ServerBuilder, ServerOptions};
pub use ferroly::rest::ClientError;
```

---

# The Client

## Quick start

```rust
use ferroly::rest::Client;
use ferroly::codec::{Encode, Decode};

#[derive(Encode)]
struct NewItem { name: String }

#[derive(Decode)]
struct Item { id: u64, name: String }

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new();

    let resp = client
        .post("http://host/items/${parent}")
        .path_param("parent", "42")            // ${parent} substitution
        .query("verbose", "true")              // ?verbose=true (percent-encoded)
        .header("x-request-id", "abc")
        .body(&NewItem { name: "widget".into() })  // codec-encoded by content type
        .send()
        .await?;

    if resp.is_success() {                      // 200..300
        let item: Item = resp.decode()?;        // codec-decoded by Content-Type
        println!("created item {}", item.id);
    }
    Ok(())
}
```

> Note: request/response bodies are encoded and decoded through ferroly's own
> [`Encode`/`Decode`](codec.md) traits, with the codec chosen from the content type
> (JSON by default). Derive `Encode`/`Decode` on your body types with
> `#[derive(Encode, Decode)]` — no external crate is involved.

## `Client`

`Client` is a cheap, `Clone`-able handle around a shared `Arc<ClientInner>` (the HTTP
client, optional auth, optional retry, and the default content type). Cloning a `Client`
shares one underlying connection pool and configuration.

| Constructor / method | Signature | Purpose |
| --- | --- | --- |
| `Client::new()` | `fn new() -> Self` | Client with all defaults. |
| `Client::default()` | `fn default() -> Self` | Same as `new()`. |
| `Client::with_options(opts)` | `fn with_options(ClientOptions) -> Self` | Client from explicit options. |
| `request(url, method)` | `fn request(impl Into<String>, Method) -> RequestBuilder` | Start a request with an arbitrary method. |
| `get(url)` | `fn get(impl Into<String>) -> RequestBuilder` | Shorthand for `Method::Get`. |
| `post(url)` | `fn post(impl Into<String>) -> RequestBuilder` | Shorthand for `Method::Post`. |
| `put(url)` | `fn put(impl Into<String>) -> RequestBuilder` | Shorthand for `Method::Put`. |
| `delete(url)` | `fn delete(impl Into<String>) -> RequestBuilder` | Shorthand for `Method::Delete`. |

Each accessor returns a [`RequestBuilder`](#requestbuilder); nothing is sent until
[`send`](#send) is awaited.

## `ClientOptions` and `ClientOptionsBuilder`

`ClientOptions` is `Default` and constructed through a fluent builder. All fields are
optional.

```rust
use std::sync::Arc;
use std::time::Duration;
use ferroly::rest::{Client, ClientOptions};
use ferroly::clients::{BearerAuth, RetryPolicy};

let options = ClientOptions::builder()
    .request_timeout(Duration::from_secs(10))
    .auth(Arc::new(BearerAuth::new("secret-token")))   // Arc<dyn AuthProvider>
    .retry_policy(RetryPolicy::default())
    .default_content_type("application/json")
    .build();

let client = Client::with_options(options);
```

| Builder method | Type | Effect |
| --- | --- | --- |
| `request_timeout(d)` | `Duration` | Per-request timeout, forwarded to the underlying [`http::Client`](http.md). Unset ⇒ the http client's own default. |
| `auth(a)` | `Arc<dyn AuthProvider>` | An [`AuthProvider`](clients.md) applied to every outbound request just before it is sent. |
| `retry_policy(p)` | `RetryPolicy` | A [`RetryPolicy`](clients.md) applied around each `send`. Only **transport errors on idempotent methods** (GET/HEAD/PUT/DELETE/OPTIONS/TRACE) are retried — a POST/PATCH is never auto-retried (avoiding double-submit), and a 5xx is returned as `Ok(Response)` for the caller to handle. |
| `default_content_type(ct)` | `impl Into<String>` | Default request content type (and thus default body codec). Falls back to `application/json` when unset. |

`ClientOptions::builder()` returns a `ClientOptionsBuilder`; `build()` finalizes it. The
builder itself also implements `Default`.

## `RequestBuilder`

A fluent, single-use builder for one request. Path placeholders use `${name}` syntax and
are substituted from `path_param` before the request is sent.

```rust
pub fn query(self, key: impl Into<String>, value: impl Into<String>) -> Self
pub fn path_param(self, key: impl Into<String>, value: impl Into<String>) -> Self
pub fn header(self, key: impl Into<String>, value: impl Into<String>) -> Self
pub fn content_type(self, ct: impl Into<String>) -> Self
pub fn body<T: Encode>(self, value: &T) -> Self
pub fn body_bytes(self, bytes: Vec<u8>) -> Self
pub async fn send(self) -> Result<Response, ClientError>
```

### `query`

Appends a query parameter. Keys and values are percent-encoded (unreserved set
`A–Z a–z 0–9 - _ . ~` pass through; everything else becomes `%XX`). Multiple calls append
multiple parameters; the builder chooses `?` or `&` based on whether the URL already
contains a `?`.

```rust
client.get("http://host/search")
    .query("q", "hello world")   // -> ?q=hello%20world
    .query("page", "2")          // -> &page=2
```

### `path_param` — `${name}` templating

Registers a substitution for a `${name}` placeholder in the URL. Substitution happens once,
at `send` time. If any `${…}` placeholder remains unsubstituted, `send` fails with
[`ClientError::InvalidRequest`](#error-handling).

```rust
client.get("http://host/orgs/${org}/repos/${repo}")
    .path_param("org", "nandlabs")
    .path_param("repo", "ferroly")
    // -> http://host/orgs/nandlabs/repos/ferroly
```

### `header`

Adds a request header (repeatable). Headers set here are applied on top of anything the
auth provider or body handling adds.

### `content_type` and body codecs

The content type does double duty: it becomes the `Content-Type` request header **and**
selects which [codec](codec.md) encodes the body. Absent an override, it is the client's
`default_content_type` (`application/json` by default).

### `body` — codec-encoded

`body::<T: Encode>(&value)` encodes `value` with the codec for the current content type.
Encoding is eager, but a failure is **deferred**: rather than making `body` fallible, the
error is stashed and surfaced when `send` is awaited (as `ClientError::Codec`). Set
`content_type` *before* `body` if you want a non-default codec:

```rust
client.post("http://host/x")
    .content_type("application/xml")
    .body(&value)   // encoded with the XML codec
```

### `body_bytes` — raw

`body_bytes(Vec<u8>)` sets the body verbatim, bypassing the codec. The `Content-Type`
header still reflects the current content type when a body is present. Use this for
pre-encoded payloads, file uploads, or non-codec media.

### `send`

Consumes the builder and performs the request:

1. If a deferred encoding error exists, return it immediately.
2. Substitute `${…}` path params; error on any leftover placeholder.
3. If a `RetryPolicy` is configured, run the request under [`retry`](clients.md); otherwise
   run it once. Only `ClientError::Transport` failures are treated as retryable (see
   [error handling](#error-handling)).
4. For each attempt: append the query string, build the `http::Request`, set headers, set
   the body + `content-type` (if any), apply the auth provider, and send via the underlying
   [`http::Client`](http.md).
5. Read the full response body and wrap status, `Content-Type`, and bytes into a
   [`Response`](#response).

## `Response`

A fully-read response with codec-aware decoding. `Response` is `Debug + Clone`.

| Method | Signature | Returns |
| --- | --- | --- |
| `status_code()` | `fn status_code(&self) -> u16` | The numeric status code. |
| `is_success()` | `fn is_success(&self) -> bool` | `true` for `200..300`. |
| `content_type()` | `fn content_type(&self) -> Option<&str>` | The response `Content-Type`, if present. |
| `raw()` | `fn raw(&self) -> &[u8]` | The raw body bytes. |
| `text()` | `fn text(&self) -> String` | Body as a lossy UTF-8 `String`. |
| `decode::<T>()` | `fn decode<T: Decode>(&self) -> Result<T, ClientError>` | Decode via the codec named by `Content-Type` (JSON when absent). |

> `is_success()` covers the whole `2xx` range (`200..300`) — this is a deliberate change from
> the earlier port, which only accepted `200..=204`. It now matches
> [`StatusCode::is_success`](http.md).

```rust
let resp = client.get("http://host/item/1").send().await?;
match resp.status_code() {
    200..=299 => {
        let item: Item = resp.decode()?;   // codec chosen by Content-Type
        // ...
    }
    404 => println!("not found"),
    _   => eprintln!("error body: {}", resp.text()),
}
```

`decode` reads `Content-Type` to pick the codec, defaulting to `application/json` when the
header is missing. A codec failure surfaces as `ClientError::Codec`.

---

# The Server

## Quick start

```rust
use std::sync::Arc;
use ferroly::rest::{Server, ServerOptions};
use ferroly::http::{HttpResponse, StatusCode};
use ferroly::lifecycle::Component;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = Arc::new(
        Server::builder(ServerOptions {
            id: "api".into(),
            path_prefix: "/v1".into(),      // routes mount under /v1
            listen_host: "127.0.0.1".into(),
            listen_port: 0,                 // 0 => ephemeral port
            ..Default::default()
        })
        .get("/ping", |_ctx| async move { HttpResponse::text(StatusCode::OK, "pong") })
        .post("/echo", |ctx| async move {
            let body = ctx.body().to_vec();
            HttpResponse::new(StatusCode::OK).body(body)
        })
        .build(),
    );

    Component::start(server.as_ref()).await?;      // binds + serves in a background task
    let addr = server.local_addr().unwrap();       // resolved bound address
    println!("listening on {addr}");
    // ... run the app ...
    Component::stop(server.as_ref()).await?;        // graceful shutdown
    Ok(())
}
```

## `ServerOptions`

A plain configuration struct with **public fields** and a `Default`. There is deliberately
**no builder** — construct it with a struct literal, filling in the fields you care about
and defaulting the rest with `..Default::default()`.

```rust
pub struct ServerOptions {
    pub id: String,           // unique lifecycle component id
    pub path_prefix: String,  // prefix all builder routes are mounted under ("" = none)
    pub listen_host: String,  // bind host/interface
    pub listen_port: u16,     // bind port (0 = ephemeral)
}
```

| Field | Default | Notes |
| --- | --- | --- |
| `id` | `"ferroly-rest"` | The `Component::id`; must be unique within a `ComponentManager`. |
| `path_prefix` | `""` | Trailing `/` is trimmed; a non-empty prefix is prepended to every route registered through the builder's `get`/`post`/`put`/`delete`/`add_route`. |
| `listen_host` | `"0.0.0.0"` | Interface to bind. |
| `listen_port` | `8080` | `0` selects an ephemeral port — read the real one from `local_addr()` after start. |

## `ServerBuilder`

Returned by `Server::builder(options)` or `Server::default_server()` (the latter uses
`ServerOptions::default()`). It registers routes into a `turbo::Router`, then `build()`s a
`Server`.

### Route registration (path-prefixed)

```rust
pub fn get<F, Fut>(self, path: &str, handler: F) -> Self
pub fn post<F, Fut>(self, path: &str, handler: F) -> Self
pub fn put<F, Fut>(self, path: &str, handler: F) -> Self
pub fn delete<F, Fut>(self, path: &str, handler: F) -> Self
pub fn add_route<F, Fut>(self, path: &str, handler: F, methods: Vec<Method>) -> Self
where
    F: Fn(Ctx) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = HttpResponse> + Send + 'static,
```

Handlers are [`turbo::Ctx`](turbo.md) handlers: an async `Fn(Ctx) -> HttpResponse`. Every
path registered through these methods is prefixed by `path_prefix`. `add_route` binds one
handler to several methods at once:

```rust
use ferroly::http::Method;

builder.add_route("/items/${id}", handler, vec![Method::Get, Method::Head]);
```

Path parameters, wildcards, and content negotiation are turbo features — see
[turbo](turbo.md). To return a codec-negotiated body from a handler, use
`Ctx::respond` (turbo picks the codec from the request's `Accept` header).

### `map_router` — the turbo seam

```rust
pub fn map_router(self, f: impl FnOnce(Router) -> Router) -> Self
```

`map_router` hands you the underlying [`turbo::Router`](turbo.md) so you can reach any turbo
capability the prefixing wrappers don't surface — notably `layer` (onion middleware),
`filter`, and `auth` interceptors. **Routes added inside `map_router` are NOT path-prefixed**
(you address the router directly).

```rust
use ferroly::rest::{Server, ServerOptions};
use ferroly::http::{HttpResponse, StatusCode};

let server = Server::builder(ServerOptions::default())
    .map_router(|r| r.auth(|ctx| {
        if ctx.header("authorization").is_none() {
            Some(HttpResponse::text(StatusCode::UNAUTHORIZED, "no auth"))
        } else {
            None    // None = allow the request through
        }
    }))
    .get("/ping", |_ctx| async move { HttpResponse::ok() })
    .build();
```

See [turbo interceptors](turbo.md#interceptors-filters-vs-middleware) for the difference
between `layer`, `filter`, and `auth`.

### `health_endpoints`

```rust
pub fn health_endpoints(self, registry: HealthRegistry) -> Self
// Separate registries for liveness vs readiness (preferred for k8s):
pub fn health_endpoints_split(self, liveness: HealthRegistry, readiness: HealthRegistry) -> Self
```

Adds two Kubernetes-friendly probe routes, backed by a
[`lifecycle::HealthRegistry`](lifecycle.md). These are added via `map_router`, so they are
**unprefixed** regardless of `path_prefix`:

> **Prefer `health_endpoints_split` in production.** With a single registry, a `Down`
> dependency also fails *liveness*, so k8s **restarts** the pod rather than just pulling it
> from the load balancer. Give liveness a minimal "process is alive" registry and readiness
> the dependency checks.

- `GET /health` (liveness) — returns `503 SERVICE_UNAVAILABLE` when the registry's overall
  status is `Down`, otherwise `200 OK`; the body is the registry's JSON report
  (`content-type: application/json`).
- `GET /ready` (readiness) — returns `200 OK` with body `ready` only when every check is
  `Up` (`registry.is_ready()`), otherwise `503` with body `not ready`.

```rust
use ferroly::lifecycle::HealthRegistry;

let registry = HealthRegistry::new();
// register checks on `registry` ...
let server = Server::default_server()
    .health_endpoints(registry.clone())
    .get("/", |_c| async move { HttpResponse::ok() })
    .build();
// GET /health and GET /ready are now live.
```

### `build`

`build()` finalizes the router into an `http` handler and returns a runnable `Server` in
`ComponentState::Unknown` (not yet bound).

## `Server` and the lifecycle

`Server` implements [`lifecycle::Component`](lifecycle.md):

- `Component::start(&self)` — binds a `TcpListener` on `listen_host:listen_port`, records the
  resolved `local_addr`, spawns [`http::serve`](http.md) on a background task, and moves the
  component to `ComponentState::Running`. Starting a server whose handler was already taken
  (i.e. a second `start`) yields `LifecycleError::ComponentAlreadyStarted`; a bind or
  `local_addr` failure yields `LifecycleError::failure(...)`.
- `Component::stop(&self)` — signals the serve task's shutdown channel, awaits the task
  (graceful drain), and moves to `ComponentState::Stopped`.

Two inherent accessors:

| Method | Returns |
| --- | --- |
| `local_addr()` | `Option<SocketAddr>` — the bound address, available after `start` (essential when `listen_port == 0`). |
| `state()` | `ComponentState` — `Unknown` → `Running` → `Stopped`. |

Register the `Server` with a [`ComponentManager`](lifecycle.md) to get managed ordered
start/stop and OS-signal-driven shutdown, or drive `start`/`stop` directly as shown above.

---

## Error handling

The client's error type is `ClientError`:

```rust
pub enum ClientError {
    InvalidRequest(String),                  // malformed URL or leftover ${placeholder}
    Transport(#[from] ferroly::http::HttpError),   // DNS/TLS/connection/timeout
    Codec(#[from] ferroly::codec::CodecError),     // body encode/decode failure
}
```

| Variant | Raised when | Retryable? |
| --- | --- | --- |
| `InvalidRequest` | The URL cannot be parsed, or a `${name}` placeholder was never given a `path_param`. | No |
| `Transport` | The underlying [`http`](http.md) transport fails — DNS, TLS, connection, or timeout. Converts from `HttpError` via `#[from]`. | **Yes** (the only variant `send`'s retry driver retries). |
| `Codec` | A request body could not be encoded, or a response body could not be decoded. Converts from `CodecError` via `#[from]`. | No |

`ClientError` is produced through the [`FerrolyError`](derive.md) derive, so it implements
`std::error::Error` and `Display` with the messages shown in the source.

Note that only transport errors retry — a `5xx` HTTP response is a **successful** transport
exchange and is returned as an `Ok(Response)` with a `5xx` `status_code()`; inspect
`is_success()` / `status_code()` yourself. (The server-side `RetryPolicy` docs in
[clients](clients.md) describe the retry driver in general.)

On the server side, `start`/`stop` failures surface as
[`lifecycle::LifecycleError`](lifecycle.md).

## Limitations

- **Retry scope:** `send` only retries `Transport` failures, not `5xx` responses. If you
  want to retry on status codes, inspect `Response` and drive the [`retry`](clients.md)
  driver yourself.
- **No client-side circuit breaker wired in:** `ClientOptions` exposes auth and retry, but
  not a [`CircuitBreaker`](clients.md); wrap calls manually if you need one.
- **Server TLS and CORS are not (yet) re-implemented in-house.** The server binds plain TCP;
  terminate TLS upstream. CORS (previously handled by `tower-http`) has no in-house
  replacement yet.
- **Whole-body buffering:** `Response` reads the full body into memory before returning;
  there is no streaming response API on the client.

## See also

- [http](http.md) — the transport, `Client`, `Request`/`Response`, `serve`, and TLS that
  `rest` builds on.
- [turbo](turbo.md) — the router, `Ctx`, content negotiation (`Ctx::respond`), and the
  `layer`/`filter`/`auth` interceptors reached through `map_router`.
- [clients](clients.md) — `AuthProvider`, `RetryPolicy` + `retry`, and `CircuitBreaker`.
- [codec](codec.md) — the `Encode`/`Decode` traits behind `body` and `decode`.
- [lifecycle](lifecycle.md) — `Component`, `ComponentManager`, `HealthRegistry`.

---
**Related:** [http](http.md), [turbo](turbo.md), [clients](clients.md), [codec](codec.md), [lifecycle](lifecycle.md).

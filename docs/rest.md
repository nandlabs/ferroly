# ferroly::rest

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `rest` (implies `turbo` + `clients` + `codec` + `lifecycle` + `http`).

A full HTTP client + server framework, dependency-minimal and built entirely in-house.
Nothing here talks to `hyper`, `reqwest`, `axum`, or `tower`;
everything layers on ferroly's own crates.

- The **client** wraps [`ferroly::http::Client`](http.md) with codec-aware request/response
  bodies (via [`codec`](codec.md)), authentication, retry, circuit breaking and policies
  (via [`clients`](clients.md)), safe `${name}` URL templating, a base URL with default
  headers and query parameters, explicit status handling, and pagination helpers.
- The **server** wraps the first-class [`turbo`](turbo.md) router and the
  [`ferroly::http`](http.md) server — plain or TLS, with every server limit configurable —
  and integrates with [`lifecycle`](lifecycle.md) as a cancel-safe, restartable `Component`
  with health endpoints.

## Overview

`rest` is a thin, opinionated convenience layer. It does not reimplement HTTP — the actual
transport, TLS, connection handling, routing, middleware, and content negotiation all live
in [`http`](http.md) and [`turbo`](turbo.md). `rest` composes them into the two shapes an
application typically wants: a fluent typed HTTP client and a lifecycle-managed HTTP server.

Because the server builder exposes the underlying `turbo::Router` directly through
[`map_router`](#map_router--the-turbo-seam), no turbo capability is walled off.

| Concern | API |
|---|---|
| Client | [`Client`](#client), [`ClientOptions`](#clientoptions-and-clientoptionsbuilder), [`RequestBuilder`](#requestbuilder), [`Response`](#response) |
| URL templating | [`path_param`](#path_param--name-templating), [`base_url`](#base-url-default-headers-default-query) |
| Resilience | [`retry_policy`](#retry), [`circuit_breaker`](#circuit-breaker), [`policy`](#policy) |
| Status handling | [`error_for_status`](#a-non-2xx-response-is-not-an-error--by-default) |
| Pagination | [`paginate`, `paginate_links`](#pagination) |
| Server | [`Server`](#server-and-the-lifecycle), [`ServerOptions`](#serveroptions), [`ServerBuilder`](#serverbuilder) |
| TLS and limits | [`ServerOptions::tls`, `ServerOptions::server_config`](#tls-and-server-limits) |
| Health | [`health_endpoints`](#health_endpoints) |
| Errors | [`ClientError`](#error-handling) |

## Enabling

```toml
[dependencies]
ferroly = { version = "*", features = ["rest"] }
```

The `rest` feature pulls in `turbo`, `clients`, `codec`, `lifecycle`, and `http`
automatically. With the `obs` feature the client also
[propagates trace context](#trace-propagation-obs-feature).

```rust
// Handler/response types re-exported by ferroly::rest for convenience:
pub use ferroly::http::{HttpResponse, Method, StatusCode};
pub use ferroly::turbo::{Cors, Ctx, Group, IntoResponse, Route, Router, SecurityHeaders};
// Own public items:
pub use ferroly::rest::{Client, ClientOptions, ClientOptionsBuilder, RequestBuilder, Response};
pub use ferroly::rest::{Server, ServerBuilder, ServerOptions, ServerTls};
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
        .path_param("parent", "42")            // ${parent}: encoded as one path segment
        .query("verbose", "true")              // ?verbose=true (percent-encoded)
        .header("x-request-id", "abc")
        .body(&NewItem { name: "widget".into() })  // codec-encoded by content type
        .send()
        .await?
        .error_for_status()?;                  // a non-2xx status becomes an error

    let item: Item = resp.decode()?;           // codec-decoded by Content-Type
    println!("created item {}", item.id);
    Ok(())
}
```

> Note: request/response bodies are encoded and decoded through ferroly's own
> [`Encode`/`Decode`](codec.md) traits, with the codec chosen from the content type
> (JSON by default). Derive `Encode`/`Decode` on your body types with
> `#[derive(Encode, Decode)]` — no external crate is involved.

## A non-2xx response is not an error — by default

[`send`](#send) returns `Ok(Response)` for **every** response the server produced, `404` and
`500` included. `Err` means no response was obtained (a transport failure, a refused
request). This is easy to overlook, so make the choice explicit:

```rust
use ferroly::rest::{Client, ClientError, ClientOptions};

# async fn ex() -> Result<(), ClientError> {
let client = Client::new();

// Per response: turn a non-2xx status into ClientError::Status { status, body }.
match client.get("https://api.example.com/users/1").send().await?.error_for_status() {
    Ok(resp) => println!("{}", resp.text()),
    Err(ClientError::Status { status: 404, .. }) => println!("no such user"),
    Err(other) => return Err(other),
}

// Or for every request of a client:
let strict = Client::with_options(ClientOptions::builder().error_for_status(true).build());
let resp = strict.get("https://api.example.com/users/1").send().await?;   // Err on 404
# let _ = resp;
# Ok(())
# }
```

`Response::decode` decodes whatever body arrived, whatever the status — check the status (or
call `error_for_status`) first, or an error body may be decoded as if it were the result.

## `Client`

`Client` is a cheap, `Clone`-able handle around shared state (the HTTP client, auth, retry,
breakers, defaults). Cloning a `Client` shares one underlying connection pool and
configuration.

| Constructor / method | Signature | Purpose |
| --- | --- | --- |
| `Client::new()` / `Client::default()` | `fn new() -> Self` | Client with all defaults. |
| `Client::with_options(opts)` | `fn with_options(ClientOptions) -> Self` | Client from explicit options. Never fails; see below. |
| `Client::try_with_options(opts)` | `fn try_with_options(ClientOptions) -> Result<Self, ClientError>` | Reports options that cannot produce a working transport. |
| `request(url, method)` | `fn request(impl Into<String>, Method) -> RequestBuilder` | Start a request with an arbitrary method. |
| `get` / `post` / `put` / `delete` / `patch` / `head` / `options` | `fn get(impl Into<String>) -> RequestBuilder` | Shorthands. |
| `paginate` / `paginate_links` | see [Pagination](#pagination) | Walk a paginated collection. |

Each accessor returns a [`RequestBuilder`](#requestbuilder); nothing is sent until
[`send`](#send) is awaited.

`with_options` is infallible. If the options cannot be turned into a transport (an invalid
TLS configuration — for example no trust root at all), every request of the resulting client
fails with `ClientError::InvalidRequest` naming the problem; use `try_with_options` to learn
about it at construction time.

## `ClientOptions` and `ClientOptionsBuilder`

`ClientOptions` is `Default` and constructed through a fluent builder. All settings are
optional.

```rust
use std::sync::Arc;
use std::time::Duration;
use ferroly::clients::{BearerAuth, CircuitBreakerConfig, Jitter, RetryPolicy};
use ferroly::rest::{Client, ClientOptions};

let options = ClientOptions::builder()
    .base_url("https://api.example.com/v1")
    .default_header("x-tenant", "acme")
    .connect_timeout(Duration::from_secs(5))
    .request_timeout(Duration::from_secs(10))
    .total_timeout(Duration::from_secs(30))
    .auth(Arc::new(BearerAuth::new("secret-token")))   // Arc<dyn AuthProvider>
    .retry_policy(RetryPolicy::default().with_jitter(Jitter::Full))
    .circuit_breaker(CircuitBreakerConfig::default())
    .build();

let client = Client::with_options(options);
```

### Timeouts and limits

Nothing is unbounded by default: a client built without touching these inherits the
[`http::Client`](http.md#clientbuilder-timeouts-limits-pool) defaults. (Earlier versions
disabled the response timeout unless `request_timeout` was set.)

| Builder method | Default | Meaning |
| --- | --- | --- |
| `connect_timeout(d)` | 30 s | DNS + TCP connect + TLS handshake + proxy tunnel. |
| `request_timeout(d)` | 60 s | Deadline for the **response head**: connecting, sending the request, receiving status line and headers. |
| `read_timeout(d)` | 300 s | Idle time between reads of the response body. |
| `total_timeout(d)` | none | The whole exchange of one attempt, body included. |
| `max_response_body_bytes(n)` | 64 MiB | Larger responses fail with `HttpError::BodyTooLarge`. |
| `max_redirects(n)` | 0 | Redirects followed; `0` returns a `3xx` as it is. Credentials are dropped on a redirect to another origin. |

A timeout surfaces as `ClientError::Transport(HttpError::Timeout)` (or `ConnectTimeout`);
`ClientError::is_timeout()` covers both, and a [policy](#policy) deadline too.

### Transport

| Builder method | Meaning |
| --- | --- |
| `proxy(Proxy)` / `proxy_from_env()` | Route through a [proxy](http.md#proxies). The environment is never read implicitly. |
| `tls(ClientTlsConfig)` | [Client TLS](http.md#client-tls-custom-roots-client-certificates): extra or replacement roots, a client certificate, minimum version. Server certificates are always verified. |
| `interceptor(impl Interceptor)` | A transport-level [interceptor](http.md#interceptors) (sees every attempt and redirect hop). |
| `user_agent(s)` | The default `User-Agent`. |
| `decompress(bool)` | Ask for and decode `gzip` / `deflate` responses (default off). |
| `http_client(http::Client)` | Use a ready-made transport (to share a connection pool). The transport and timeout settings above then come from that client; calling them here has no effect. |

### Request defaults

| Builder method | Meaning |
| --- | --- |
| `base_url(url)` | See [below](#base-url-default-headers-default-query). |
| `default_header(name, value)` | Sent with every request; a request's own `header` of the same name replaces it. |
| `default_query(key, value)` | Appended to every request's query. |
| `default_content_type(ct)` | Default request content type and body codec (`application/json`). |
| `auth(Arc<dyn AuthProvider>)` | See [Authentication](#authentication). |
| `error_for_status(bool)` | Make `send` fail on non-2xx; see [above](#a-non-2xx-response-is-not-an-error--by-default). |

### Resilience

| Builder method | Meaning |
| --- | --- |
| `retry_policy(impl Into<Retry>)` | See [Retry](#retry). |
| `circuit_breaker(CircuitBreakerConfig)` / `circuit_breakers(Arc<CircuitBreakerRegistry>)` | See [Circuit breaker](#circuit-breaker). |
| `policy(Policy)` | See [Policy](#policy). |

`ClientOptions::builder()` returns a `ClientOptionsBuilder`; `build()` finalizes it. The
builder itself also implements `Default`.

### Base URL, default headers, default query

```rust
use ferroly::rest::{Client, ClientOptions};

let client = Client::with_options(
    ClientOptions::builder()
        .base_url("https://api.example.com/v1")
        .default_header("x-tenant", "acme")
        .default_query("api-version", "2024-01")
        .build(),
);

let _req = client.get("/users/${id}").path_param("id", "42");
// GET https://api.example.com/v1/users/42?api-version=2024-01
```

A request URL without a scheme (`/users`, `users`, `?page=2`) is appended to the base; an
absolute request URL (`https://…`) is used as given — default headers, query parameters and
**authentication still apply to it**. Do not build request URLs from untrusted input on an
authenticated client.

### Authentication

`auth(provider)` attaches an [`AuthProvider`](clients.md). Before each attempt the client
calls [`clients::authorize`](clients.md) — `prepare().await`, then `apply()` — so providers
that fetch or refresh credentials (`RefreshingAuth`, `OAuth2ClientCredentials`) work, as well
as static ones (`BearerAuth`, `ApiKeyAuth`, `BasicAuth`).

If the provider cannot supply credentials, the request is **not sent** and `send` returns
`ClientError::Auth`.

### Retry

`retry_policy` accepts a [`RetryPolicy`](clients.md) (the backoff schedule) or a full
[`Retry`](clients.md) with jitter, a time limit and a shared retry budget:

```rust
use std::sync::Arc;
use std::time::Duration;
use ferroly::clients::{Jitter, Retry, RetryBudget, RetryPolicy};
use ferroly::rest::ClientOptions;

let budget = Arc::new(RetryBudget::new(10, 1.0));
let options = ClientOptions::builder()
    .retry_policy(
        Retry::new(RetryPolicy::exponential(3, Duration::from_millis(100), Duration::from_secs(2)))
            .jitter(Jitter::Full)
            .max_elapsed(Duration::from_secs(10))
            .max_retry_after(Some(Duration::from_secs(30)))
            .budget(budget),
    )
    .build();
# let _ = options;
```

What is retried:

| Situation | Idempotent request (`GET`, `HEAD`, `PUT`, `DELETE`, `OPTIONS`, `TRACE`) | Non-idempotent (`POST`, `PATCH`) |
| --- | --- | --- |
| Transient transport error (connection failure, timeout, malformed response) | retried | **not** retried |
| Connect timeout (nothing was sent) | retried | retried |
| `502`, `503`, `504`, `429` response | retried | **not** retried |
| `500` and other statuses | not retried | not retried |
| TLS failure, invalid URL, oversized body, open circuit, auth failure | not retried | not retried |

- **`Retry-After`** on a retryable response is honoured: the client waits exactly that long
  instead of the policy's delay. If it exceeds the retry's `max_retry_after` cap (5 minutes
  by default) the client does not wait at all and returns the response.
- **Non-idempotent requests** are never retried on their own, because the server may have
  acted on the first attempt. Opt a request in with
  [`idempotency_key`](#idempotency_key-and-idempotent) when the server deduplicates.
- When the retries run out on a retryable status, the **last response** is returned (as
  `Ok`, like any other response).

The transport underneath has one retry of its own — a request on a pooled connection that
turned out to be dead is re-sent once, under the same idempotency rule — see
[`http`](http.md#retry-on-a-dead-pooled-connection).

### Circuit breaker

```rust
use std::time::Duration;
use ferroly::clients::CircuitBreakerConfig;
use ferroly::rest::ClientOptions;

let options = ClientOptions::builder()
    .circuit_breaker(CircuitBreakerConfig {
        failure_threshold: 5,
        timeout: Duration::from_secs(30),
        ..CircuitBreakerConfig::default()
    })
    .build();
# let _ = options;
```

One breaker per destination (`host:port`), created on first use from a
[`CircuitBreakerRegistry`](clients.md). Each attempt takes a permit and reports its outcome:

| Outcome of an attempt | Counts as |
| --- | --- |
| Transport failure, `5xx`, `429` | failure of the destination |
| `2xx`, `3xx` | success |
| Other `4xx`; an invalid URL; a failed credential refresh; a cancelled request | **ignored** — the caller's problem cannot open the circuit for everyone else |

While a breaker is open, requests to that destination fail at once with
`ClientError::CircuitOpen("host:port")`, are not sent, and are not retried.
`circuit_breakers(registry)` uses a registry you own — to share breakers between clients,
tune them per host, or watch their state.

### Policy

`policy(Policy)` runs every request through a [`clients::Policy`](clients.md): overall
timeout, retry, circuit breaker, bulkhead, rate limiter, per-attempt timeout.

```rust
use std::sync::Arc;
use std::time::Duration;
use ferroly::clients::{Bulkhead, Policy, RetryPolicy};
use ferroly::rest::ClientOptions;

let options = ClientOptions::builder()
    .policy(
        Policy::new()
            .timeout(Duration::from_secs(10))
            .retry(RetryPolicy::fixed(2, Duration::from_millis(50)))
            .bulkhead(Arc::new(Bulkhead::new(16))),
    )
    .build();
# let _ = options;
```

- The policy **replaces** `retry_policy` (which is then not consulted); configure retries on
  the policy. The classification is the same as in the tables above, so a non-idempotent
  request is still not retried.
- A per-host `circuit_breaker` set on the options still applies to each attempt.
- Rejections map to errors: the deadline → `ClientError::Timeout`, a full bulkhead →
  `ClientError::Rejected`, the policy's own open breaker → `ClientError::CircuitOpen("policy")`.

### Trace propagation (`obs` feature)

With the `obs` feature each attempt runs in a [client span](obs.md) (named after the method;
`url.full` without query or credentials) and the W3C `traceparent` / `tracestate` headers
are injected, so the downstream service continues the trace. Inside a server request handled
with [`turbo`'s `trace_context`](turbo.md#trace_context-log-or-obs-feature), the outbound call
joins that request's trace automatically. A `traceparent` header you set yourself is left
alone.

## `RequestBuilder`

A fluent, single-use builder for one request.

```rust
pub fn query(self, key: impl Into<String>, value: impl Into<String>) -> Self
pub fn path_param(self, key: impl Into<String>, value: impl Into<String>) -> Self
pub fn header(self, key: impl Into<String>, value: impl Into<String>) -> Self
pub fn content_type(self, ct: impl Into<String>) -> Self
pub fn accept(self, media_type: impl Into<String>) -> Self
pub fn body<T: Encode>(self, value: &T) -> Self
pub fn body_bytes(self, bytes: Vec<u8>) -> Self
pub fn idempotency_key(self, key: impl Into<String>) -> Self
pub fn idempotent(self, safe_to_retry: bool) -> Self
pub async fn send(self) -> Result<Response, ClientError>
```

### `query`

Appends a query parameter. Keys and values are percent-encoded (unreserved set
`A–Z a–z 0–9 - _ . ~` pass through; everything else becomes `%XX`). Multiple calls append
multiple parameters, after the client's `default_query`.

```rust
client.get("http://host/search")
    .query("q", "hello world")   // -> ?q=hello%20world
    .query("page", "2")          // -> &page=2
```

### `path_param` — `${name}` templating

Registers the value for a `${name}` placeholder in the URL. The value is **data, never URL
syntax**: where the placeholder stands decides how it is inserted.

| Placeholder position | Value is | Refused (`ClientError::InvalidRequest`) |
| --- | --- | --- |
| path (`/items/${id}`) | percent-encoded as **one segment** — `/`, `?`, `#`, `%`, spaces and CR/LF cannot add segments, start a query or reach the request line | an empty value, `.` and `..` (they cannot be a path segment) |
| query (`?q=${q}`) | percent-encoded as one component | — |
| scheme / authority (`http://${host}/…`) | inserted as written | anything but `A-Z a-z 0-9 . - _ ~ : [ ]` |

```rust
client.get("http://host/orgs/${org}/repos/${repo}")
    .path_param("org", "nandlabs")
    .path_param("repo", "ferroly")
    // -> http://host/orgs/nandlabs/repos/ferroly

client.get("http://host/files/${name}")
    .path_param("name", "../../etc/passwd")
    // -> http://host/files/..%2F..%2Fetc%2Fpasswd   (one segment, not a traversal)
```

A placeholder with no value — or an unterminated `${` — makes `send` fail with
`ClientError::InvalidRequest`. A value is never re-scanned for placeholders.

Because values are encoded, a placeholder cannot stand for *several* path segments or for a
whole base URL; use [`base_url`](#base-url-default-headers-default-query) for the latter.

### `header`

Adds a request header (repeatable), replacing a client `default_header` of the same name.
The auth provider is applied last.

### `content_type`, `accept` and body codecs

The content type does double duty: it becomes the `Content-Type` request header (when there
is a body) **and** selects which [codec](codec.md) encodes the body. Absent an override, it
is the client's `default_content_type` (`application/json` by default).

It also drives the default `Accept` header — the response is expected in the same format,
with anything else as a lower-priority fallback:

```text
Accept: application/json, */*;q=0.5
```

`accept(media_type)` sets `Accept` explicitly; an `Accept` set with `header` or as a client
default is left alone.

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

### `idempotency_key` and `idempotent`

By default only idempotent methods are [retried](#retry).

- `idempotency_key(key)` sends an `Idempotency-Key` header and marks the request as safe to
  retry. The same key goes out with every attempt; use it only with servers that deduplicate
  on it.
- `idempotent(true)` marks a request as safe to retry without a header (when you know the
  operation is idempotent); `idempotent(false)` keeps a `PUT` or `DELETE` from being retried.

```rust
client.post("https://api.example.com/payments")
    .idempotency_key("order-7f3a")
    .body(&payment)
```

### `send`

Consumes the builder and performs the request:

1. If a deferred encoding error exists, return it immediately.
2. Build the URL: apply the base URL, substitute `${…}` placeholders (see
   [`path_param`](#path_param--name-templating)), append the default and per-request query.
3. Run the attempts — through the [policy](#policy) if one is set, else under the
   [retry](#retry) configuration, else once.
4. For each attempt: check the destination's [circuit breaker](#circuit-breaker), build the
   request, set default then per-request headers, the body and `Content-Type`, `Accept`,
   prepare and apply the auth provider, inject trace context (`obs`), and send via the
   underlying [`http::Client`](http.md).
5. Read the full response body (bounded by the body cap) into a [`Response`](#response).
6. With `error_for_status(true)`, turn a non-2xx response into `ClientError::Status`.

## `Response`

A fully-read response with codec-aware decoding. `Response` is `Debug + Clone`.

| Method | Signature | Returns |
| --- | --- | --- |
| `status_code()` | `fn status_code(&self) -> u16` | The numeric status code. |
| `status()` | `fn status(&self) -> StatusCode` | The status as a `StatusCode`. |
| `is_success()` | `fn is_success(&self) -> bool` | `true` for `200..300`. |
| `error_for_status()` | `fn error_for_status(self) -> Result<Response, ClientError>` | `Err(ClientError::Status { status, body })` unless 2xx. |
| `headers()` | `fn headers(&self) -> &HeaderMap` | All response headers. |
| `header(name)` | `fn header(&self, name: &str) -> Option<&str>` | One header (case-insensitive). |
| `content_type()` | `fn content_type(&self) -> Option<&str>` | The response `Content-Type`, if present. |
| `url()` | `fn url(&self) -> &str` | The URL the response came from (after redirects). |
| `raw()` / `into_bytes()` | `fn raw(&self) -> &[u8]` | The raw body bytes. |
| `text()` | `fn text(&self) -> String` | Body as a lossy UTF-8 `String`. |
| `decode::<T>()` | `fn decode<T: Decode>(&self) -> Result<T, ClientError>` | Decode via the codec named by `Content-Type` (JSON when absent). |
| `link(rel)` / `next_link()` | `fn link(&self, rel: &str) -> Option<String>` | A `Link` header target as an absolute URL; see [Pagination](#pagination). |

```rust
let resp = client.get("http://host/item/1").send().await?;
match resp.status_code() {
    200..=299 => {
        let item: Item = resp.decode()?;   // codec chosen by Content-Type
        let etag = resp.header("etag");    // response headers are available
        // ...
    }
    404 => println!("not found"),
    _   => eprintln!("error body: {}", resp.text()),
}
```

## Pagination

Two helpers walk a paginated collection and return the pages as a `Vec<Response>`. Both stop
after `max_pages` (a bound on a server that never stops), and both treat a non-2xx page as an
error (`ClientError::Status`) — a failure part-way through cannot be mistaken for the end of
the collection.

**`Link` headers** (RFC 8288, `rel="next"`):

```rust
use ferroly::rest::Client;

# async fn ex() -> Result<(), ferroly::rest::ClientError> {
let client = Client::new();
let pages = client
    .paginate_links(client.get("https://api.example.com/items").query("per_page", "100"), 50)
    .await?;
for page in &pages {
    println!("{} bytes", page.raw().len());
}
# Ok(())
# }
```

Each following request repeats the method and headers of the first. A relative link is
resolved against the URL of the page it came from. A link to a **different origin** is
refused (`ClientError::InvalidRequest`) rather than followed, so a response cannot send the
client's credentials to another host.

**Cursors** (or offsets) — a callback builds the next request from the page just received:

```rust
use ferroly::codec::Value;
use ferroly::rest::Client;

# async fn ex() -> Result<(), ferroly::rest::ClientError> {
let client = Client::new();
let pages = client
    .paginate(client.get("https://api.example.com/items"), 50, |page| {
        let body: Value = page.decode().ok()?;
        let cursor = body.get("next_cursor")?.as_str()?.to_string();
        Some(client.get("https://api.example.com/items").query("cursor", cursor))
    })
    .await?;
# let _ = pages;
# Ok(())
# }
```

Returning `None` ends the walk. `Response::link(rel)` / `next_link()` are available for
custom loops.

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

```rust
pub struct ServerOptions {
    pub id: String,            // lifecycle component id        (default "ferroly-rest")
    pub path_prefix: String,   // prefix for routes             (default "")
    pub listen_host: String,   // host/interface to bind        (default "0.0.0.0")
    pub listen_port: u16,      // 0 = ephemeral                 (default 8080)
    pub config: ServerConfig,  // HTTP server limits + timeouts (default ServerConfig::default())
    pub tls: Option<ServerTls>,// TLS termination               (default None: plain HTTP)
}
```

The struct gains fields over time: always build it with struct-update syntax
(`..Default::default()`), optionally followed by the chaining methods `server_config(..)`
and `tls(..)`.

### TLS and server limits

```rust
use std::time::Duration;
use ferroly::http::{ServerConfig, TlsConfig};
use ferroly::rest::{Server, ServerOptions};

# fn ex(cert_pem: &[u8], key_pem: &[u8]) -> Result<(), ferroly::http::HttpError> {
let options = ServerOptions {
    listen_port: 8443,
    ..Default::default()
}
.tls(TlsConfig::from_pem(cert_pem, key_pem)?)
.server_config(ServerConfig {
    max_body_bytes: 1024 * 1024,
    max_connections: 4096,
    handler_timeout: Some(Duration::from_secs(30)),
    ..ServerConfig::default()
});
let server = Server::builder(options).build();
# let _ = server;
# Ok(())
# }
```

- **`config`** is the [`http::ServerConfig`](http.md#resource-limits-serverconfig--serve_with_config)
  passed to the underlying server: body and header limits, the connection cap, the
  head / body / idle / write / handler timeouts, the drain time on shutdown, keep-alive
  settings, and the `on_error` hook that receives errors the server would otherwise swallow.
- **`tls`** is a `ServerTls`, built `From` either
  - a [`TlsConfig`](http.md#https-serve_tls--tlsconfig) — a fixed certificate (use
    `TlsConfig::builder()` for mutual TLS or a minimum version), or
  - a [`ReloadableTlsConfig`](http.md#certificate-reload) — keep a clone and call `reload`
    to rotate the certificate while the server runs.

  With TLS, handlers see `ctx.is_tls() == true` and, under mutual TLS,
  `ctx.peer_certificates()`.

## `ServerBuilder`

`Server::builder(options)` (or `Server::default_server()`) returns a `ServerBuilder`.

### Route registration (path-prefixed)

| Method | Description |
| --- | --- |
| `get` / `post` / `put` / `delete` / `patch` / `head` / `options` | `fn(self, path, handler) -> Self` |
| `add_route(path, handler, methods)` | One handler for several methods. |
| `route(path, Route)` | A handler with [per-route options](turbo.md#per-route-options-route): middleware, filters, body limit, timeout. |
| `group(prefix, \|g\| …)` | A [route group](turbo.md#route-groups) with scoped filters and middleware. |

Every path given to these — including a group's prefix — is placed under
`ServerOptions::path_prefix`, consistently: with `path_prefix: "/api"`,
`.get("/thing", …)` serves `/api/thing` and `.group("/admin", …)` serves `/api/admin/…`.

Handlers return an `HttpResponse` or any [`IntoResponse`](turbo.md#handlers-and-typed-errors)
type, such as `Result<HttpResponse, MyError>`.

```rust
use ferroly::http::{HttpResponse, StatusCode};
use ferroly::rest::{Server, ServerOptions};

let server = Server::builder(ServerOptions { path_prefix: "/api".into(), ..Default::default() })
    .patch("/items/:id", |ctx| async move {
        HttpResponse::text(StatusCode::OK, format!("patched {}", ctx.param("id").unwrap_or("")))
    })
    .group("/admin", |g| {
        g.filter(|ctx| {
            ctx.header("x-admin").is_none()
                .then(|| HttpResponse::text(StatusCode::FORBIDDEN, "admins only"))
        })
        .get("/stats", |_| async { HttpResponse::text(StatusCode::OK, "stats") })
    })
    .build();
# let _ = server;
```

### Middleware

| Method | Description |
| --- | --- |
| `cors(Cors)` | A [CORS policy](turbo.md#cors). Add it before anything that authenticates. |
| `security_headers(SecurityHeaders)` | [Security response headers](turbo.md#security-headers). |
| `request_id()` | [Request ids](turbo.md#request-ids) (`X-Request-Id`). |
| `layer(mw)` | Any [onion middleware](turbo.md#onion-middleware-layer--next). |

```rust
use ferroly::http::{HttpResponse, StatusCode};
use ferroly::rest::{Cors, SecurityHeaders, Server, ServerOptions};

let server = Server::builder(ServerOptions::default())
    .request_id()
    .cors(Cors::new().allow_origin("https://app.example.com"))
    .security_headers(SecurityHeaders::new())
    .get("/items", |_| async { HttpResponse::text(StatusCode::OK, "[]") })
    .build();
# let _ = server;
```

### `map_router` — the turbo seam

`map_router(|router| …)` transforms the underlying [`turbo::Router`](turbo.md) directly —
for everything not wrapped above: `filter`, `auth`, `jwt_auth_with`, `rate_limit_with`,
`mount`, `static_files`, `access_log`, `metrics`, `trace_context`, `with_state`, custom
fallbacks. Paths added this way are **not** prefixed.

```rust
use ferroly::rest::{Server, ServerOptions};
use ferroly::turbo::RateLimit;

let server = Server::builder(ServerOptions::default())
    .map_router(|r| r.rate_limit_with(RateLimit::new(10.0, 40.0)))
    .build();
# let _ = server;
```

The router's [security model](turbo.md#security-model) — one normalised path, filter scoping,
trusted proxies — applies to the server as a whole.

### `health_endpoints`

```rust
use std::time::Duration;
use ferroly::lifecycle::{CheckOptions, HealthRegistry, HealthStatus, ProbeKind};
use ferroly::rest::{Server, ServerOptions};

let registry = HealthRegistry::new();
// Liveness: is the process alive? Cheap, no dependencies.
registry.register_with(
    "process",
    CheckOptions::new().probes(&[ProbeKind::Liveness]),
    || HealthStatus::Up,
);
// Readiness: can it take traffic? Dependencies go here.
registry.register_async(
    "db",
    CheckOptions::new().probes(&[ProbeKind::Readiness]).timeout(Duration::from_secs(2)),
    || async { HealthStatus::Up },      // e.g. `SELECT 1`
);

let server = Server::builder(ServerOptions::default())
    .health_endpoints(registry)
    .build();
# let _ = server;
```

`health_endpoints(registry)` adds two **unprefixed** routes backed by one registry;
`health_endpoints_split(liveness, readiness)` takes two registries.

| Route | Runs | `200` when | Otherwise |
| --- | --- | --- | --- |
| `GET /health` (liveness) | `check_probe(ProbeKind::Liveness).await` | no check is `Down` (`Degraded` is still alive) | `503`; the body is the JSON report either way |
| `GET /ready` (readiness) | `check_probe(ProbeKind::Readiness).await` | every check is `Up` → `ready` | `503 not ready` |

- Both use the **asynchronous** registry API, so asynchronous checks are awaited — each
  bounded by its timeout — rather than reported from a stale result.
- The two probes mean different things to an orchestrator: a failed *liveness* probe
  restarts the instance; a failed *readiness* probe takes it out of rotation. Register
  dependency checks for `Readiness` **only**, as above. A check registered without probe
  options belongs to every probe, liveness included.
- To make readiness follow component state, add the manager's components to the registry
  with [`HealthRegistry::register_components`](lifecycle.md).
- The probes pass through the router's middleware, authenticator and filters like any other
  route. If the orchestrator cannot authenticate, exempt `/health` and `/ready` there.

### `build` / `try_build`

| Method | Description |
| --- | --- |
| `build(self) -> Server` | Never fails. A route with a malformed pattern is absent (`404`); of two conflicting routes the first wins. |
| `try_build(self) -> Result<Server, RouteError>` | [Validates](turbo.md#validating-the-route-table) the routes first. |

## `Server` and the lifecycle

`Server` implements [`lifecycle::Component`](lifecycle.md):

| Member | Behaviour |
| --- | --- |
| `id()` | `ServerOptions::id`. |
| `start()` | Binds `listen_host:listen_port`, spawns the serve loop, records the bound address. `ComponentAlreadyStarted` if it is serving. A bind failure is returned as `ComponentFailure` and leaves the server startable. |
| `stop()` | Signals shutdown, waits for in-flight requests to drain (up to `ServerConfig::drain_timeout`), and returns the serve loop's error if it had failed. Stopping a stopped server is a no-op. |
| `health()` | `Some(Down)` when the serve loop has ended although the server was not stopped; otherwise `None` (healthy). |
| `local_addr()` | `Option<SocketAddr>` — `Some` after a successful start. |
| `state()` | `Unknown` → `Running` → `Stopped`; `Error` if the serve loop ended on its own. |
| `last_error()` | Why the serve loop ended, if it failed. Cleared by the next successful start. |
| `is_tls()` | Whether the server terminates TLS. |

Guarantees:

- **Cancel-safe.** If the future returned by `start` is dropped — a start timeout in the
  [`ComponentManager`](lifecycle.md) fires — nothing is left half-started: no listener, no
  task, and `start` can be called again. A dropped `stop` leaves the shutdown in progress;
  calling `stop` again finishes it.
- **Restartable.** `stop` followed by `start` serves again with the same routes. With
  `listen_port: 0` each start picks a new port; with a fixed port the listener is released
  by the time `stop` returns.
- **Errors are not discarded.** A serve loop that ends on its own is visible through
  `state()`, `health()`, `last_error()` and the result of the next `stop()`. Per-connection
  errors go to `ServerConfig::on_error`.
- Dropping the `Server` shuts the listener down.

```rust
use std::sync::Arc;
use ferroly::http::{HttpResponse, StatusCode};
use ferroly::lifecycle::ComponentManager;
use ferroly::rest::{Server, ServerOptions};

# async fn ex() -> Result<(), ferroly::lifecycle::LifecycleError> {
let server = Arc::new(
    Server::builder(ServerOptions { id: "api".into(), ..Default::default() })
        .get("/ping", |_| async { HttpResponse::text(StatusCode::OK, "pong") })
        .try_build()
        .expect("valid routes"),
);
let manager = ComponentManager::new();
manager.register(server.clone())?;
manager.start_all().await?;
// ...
manager.stop_all().await?;
# Ok(())
# }
```

---

## Error handling

```rust
#[non_exhaustive]
pub enum ClientError {
    InvalidRequest(String),              // bad URL / placeholder / parameter value / client config
    Transport(ferroly::http::HttpError), // DNS, TLS, connection, timeout, body too large
    Codec(ferroly::codec::CodecError),   // body encode / decode failure
    Auth(ferroly::clients::AuthError),   // the auth provider could not supply credentials
    Status { status: u16, body: Vec<u8> }, // non-2xx, from error_for_status
    CircuitOpen(String),                 // the destination's breaker is open; not sent
    Rejected(String),                    // the policy refused the request; not sent
    Timeout,                             // the policy's deadline passed
}
```

| Variant | When |
| --- | --- |
| `InvalidRequest` | An unparseable URL; a `${…}` placeholder without a value; a path parameter that is empty, `.` or `..`, or not valid in the host position; a pagination link to another origin; client options that cannot produce a transport. |
| `Transport` | From the HTTP client: connection, TLS, protocol, `Timeout` / `ConnectTimeout`, `BodyTooLarge`, `TooManyRedirects`. |
| `Codec` | `body` could not encode (deferred to `send`), or `decode` could not decode. |
| `Auth` | `AuthProvider::prepare` failed. The request was not sent. |
| `Status` | `error_for_status()` on a non-2xx response, or any non-2xx with `error_for_status(true)`; also a failing page during pagination. |
| `CircuitOpen` | The breaker for the destination (or the policy's) is open. |
| `Rejected` | A policy bulkhead was full. |
| `Timeout` | A policy deadline passed. |

Helpers: `status() -> Option<u16>`, `body() -> Option<&[u8]>` (for `Status`),
`is_timeout() -> bool`. With the `errutils` feature `ClientError` implements
[`Classify`](errutils.md) (`Status` maps by code, timeouts to `Timeout`, an open circuit to
`Unavailable`, …).

Server-side errors are `lifecycle::LifecycleError` from `start` / `stop`, and
`turbo::RouteError` from `try_build`.

## Upgrading to 0.4

- **`ServerOptions` has two new public fields** (`config`, `tls`). A struct literal that
  lists every field no longer compiles; add `..Default::default()`.
- **Default client timeouts.** `Client::new()` used to disable the response timeout; it now
  has the `http` client's defaults (30 s connect, 60 s response head, 300 s read idle, 64 MiB
  body cap).
- **`path_param` values are percent-encoded** by position, and empty / `.` / `..` path values
  are refused. A placeholder can no longer stand for several segments or a whole base URL —
  use `base_url`. Host-position placeholders accept host characters only.
- **Retries now cover `502` / `503` / `504` / `429`** for idempotent requests (they used to
  cover transport errors only, contrary to the documentation), honouring `Retry-After`.
- **An `Accept` header is sent by default** (`application/json, */*;q=0.5`).
- **`Server` is restartable** and `start` on a running server returns
  `ComponentAlreadyStarted` (previously a second `start` failed permanently, even after
  `stop`).
- **Health endpoints use probe groups**: `/health` runs `Liveness` checks and `/ready` runs
  `Readiness` checks of the registry (previously both ran every check synchronously).
- Server paths are normalised and routed by `turbo`'s
  [new rules](turbo.md#upgrading-to-04).

## Limitations

- **No OpenAPI generation**, client or server side.
- **Bodies are buffered** in both directions: `send` reads the whole response (bounded by
  the body cap), and the server reads the whole request before the handler runs. Use
  [`http::Client`](http.md) directly for streaming.
- **No cookie jar** on the client.
- **Circuit breakers and retry budgets are per process.**
- **One listener per `Server`**; run two servers for separate public and internal ports.
- Health endpoint paths are fixed (`/health`, `/ready`) and unprefixed; register your own
  routes with `HealthRegistry::check_probe` for other paths.

## See also

- [`ferroly::turbo`](turbo.md) — the router: groups, middleware, auth, rate limiting,
  static files, the security model.
- [`ferroly::http`](http.md) — the transport: `Client`, `ServerConfig`, TLS.
- [`ferroly::clients`](clients.md) — `AuthProvider`, `Retry`, `CircuitBreaker`, `Policy`.
- [`ferroly::lifecycle`](lifecycle.md) — `Component`, `ComponentManager`, `HealthRegistry`.
- [`ferroly::codec`](codec.md) — `Encode` / `Decode`.

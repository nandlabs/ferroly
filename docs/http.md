# ferroly::http

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `http` (pulls in `tokio` + the TLS stack `tokio-rustls`/`rustls`/`ring`/`webpki-roots`).

A **hand-rolled HTTP/1.1 stack** — client *and* server — built directly over `tokio`, with
HTTPS via `tokio-rustls`. It is the dependency-minimal replacement for the
`reqwest`/`hyper`/`http`/`axum` layer of the Go original: URL parsing, an HTTP/1.1
request/response wire codec (Content-Length **and** chunked framing), a TLS-or-plaintext
transport, a streaming client, and a small server accept loop with a handler trait.

There is **no `hyper`, `http`, or `reqwest`** anywhere in the tree. The only cryptography is
the TLS transport, which is isolated behind a boxed I/O object (see
[TLS transport](#tls-transport-the-single-crypto-exception)) so nothing else in the crate —
or in *your* code — is coupled to a `rustls` version.

Higher-level ergonomics (codec-encoded bodies, auth, retry, `${}` path templating) live one
layer up in [`ferroly::rest`](rest.md) and [`ferroly::clients`](clients.md); routing lives in
[`ferroly::turbo`](turbo.md). This page documents the raw HTTP primitives those layers are
built on — you can also use them directly.

---

## Overview

| Concern | Type / function |
|---|---|
| Client | [`Client`](#client) (`new`, `with_timeout`, `send`) — keep-alive connection pooling |
| Outgoing request | [`Request`](#request--requestbuilder) + [`RequestBuilder`](#request--requestbuilder) |
| Incoming response (streaming) | [`Response`](#response-streaming-body) (`status`, `headers`, `chunk`, `bytes`, `text`) |
| Server | [`serve`](#server-serve) + [`HttpHandler`](#httphandler-trait) trait |
| Server tuning | [`ServerConfig`](#resource-limits-serverconfig--serve_with_config) + [`serve_with_config`](#resource-limits-serverconfig--serve_with_config) |
| Server TLS | [`serve_tls`](#https-serve_tls--tlsconfig) / [`serve_tls_with_config`](#https-serve_tls--tlsconfig) + [`TlsConfig`](#https-serve_tls--tlsconfig) |
| Outgoing response | [`HttpResponse`](#httpresponse) (`new`/`ok`/`text`/`header`/`body`, plus [`stream`](#streaming-response-bodies-stream--event_stream) / [`event_stream`](#streaming-response-bodies-stream--event_stream)) |
| Value types | [`Method`](#method), [`StatusCode`](#statuscode), [`HeaderMap`](#headermap), [`Uri`](#uri) |
| Transport | [`Conn`](#tls-transport-the-single-crypto-exception), [`Io`](#tls-transport-the-single-crypto-exception) |
| Errors | [`HttpError`](#error-handling) |

Everything is re-exported from `ferroly::http`:

```rust
use ferroly::http::{
    Client, Request, RequestBuilder, Response,
    serve, serve_with_config, serve_tls, serve_tls_with_config,
    ServerConfig, TlsConfig, HttpHandler, HttpResponse, BoxFuture,
    Method, StatusCode, HeaderMap, Uri, split_target,
    Conn, Io, HttpError,
};
```

---

## Enabling

```toml
[dependencies]
ferroly = { version = "*", features = ["http"] }
tokio = { version = "1", features = ["full"] }
```

`http` is implied by the higher-level features `rest`, `clients`, and `turbo`, so if you
depend on any of those you already have it.

---

## Quick start

### Client — fetch a URL

```rust
use ferroly::http::{Client, Method, Request};

#[tokio::main]
async fn main() -> Result<(), ferroly::http::HttpError> {
    let client = Client::new();
    let req = Request::builder(Method::Get, "https://example.com/")?.build();
    let resp = client.send(req).await?;
    println!("{} {}", resp.status().as_u16(), resp.text().await?);
    Ok(())
}
```

### Server — echo handler

```rust
use std::sync::Arc;
use ferroly::http::{serve, BoxFuture, HttpHandler, HttpResponse, Request, StatusCode};
use tokio::net::TcpListener;

struct Echo;

impl HttpHandler for Echo {
    fn handle(&self, req: Request) -> BoxFuture<'_, HttpResponse> {
        Box::pin(async move { HttpResponse::new(StatusCode::OK).body(req.body) })
    }
}

#[tokio::main]
async fn main() -> Result<(), ferroly::http::HttpError> {
    let listener = TcpListener::bind("127.0.0.1:8080").await?;
    // Serve until Ctrl-C.
    serve(listener, Arc::new(Echo), async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}
```

For real routing you almost always want [`turbo::Router`](turbo.md) instead of implementing
`HttpHandler` by hand — but `HttpHandler` is the seam it plugs into.

---

## Client

```rust
pub struct Client { /* … */ }
```

A minimal HTTP/1.1 client with **keep-alive connection pooling**. Each `send` reuses an idle
connection to the same `host:port` (and TLS-ness) if one is available, otherwise opens a fresh
TCP/TLS connection. When the response body is **fully drained** (via `bytes()`/`text()`, or by
reading `chunk()` to `None`) and the response is keep-alive-able, the connection is returned to
the pool for reuse — avoiding a fresh handshake on the next request. A stale pooled connection
(closed by the server) is transparently retried on a fresh one. It is `Clone` (clones **share
the pool**) and `Send + Sync`, so create one `Client` and share it across tasks.

> **Drain to reuse.** A connection is only pooled once its body is consumed. If you drop a
> `Response` without reading the body, that connection is closed rather than reused. `Eof`-framed
> responses (no `Content-Length`, not chunked) and `Connection: close` responses are never
> pooled.

| Method | Signature | Notes |
|---|---|---|
| `Client::new` | `fn new() -> Client` | 60-second default per-request timeout; also `Default`. |
| `Client::with_timeout` | `fn with_timeout(self, timeout: Option<Duration>) -> Client` | Builder-style; `None` disables the timeout entirely. |
| `Client::send` | `async fn send(&self, req: Request) -> Result<Response, HttpError>` | Sends the request, returns the response with a **streaming** body. |

```rust
use std::time::Duration;
use ferroly::http::{Client, Method, Request};

# async fn ex() -> Result<(), ferroly::http::HttpError> {
// Ten-second timeout instead of the 60s default.
let client = Client::new().with_timeout(Some(Duration::from_secs(10)));

let req = Request::builder(Method::Post, "https://httpbin.org/post")?
    .header("content-type", "application/json")
    .body(br#"{"hello":"world"}"#.to_vec())
    .build();

let resp = client.send(req).await?;
println!("status {}", resp.status().as_u16());
# Ok(())
# }
```

The timeout wraps the **whole** `send` (connect + write + read of the response head). When it
fires you get `HttpError::Timeout`. Passing `with_timeout(None)` removes the guard — useful
for a long-lived streaming download you don't want cut off.

---

## Request / RequestBuilder

```rust
pub struct Request {
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}
```

`Request` is a plain, `Clone`-able struct used on **both** sides: the client fills it in and
sends it; the server hands you one parsed from the wire (with `uri` reduced to path + query —
see [`serve`](#server-serve)).

| Constructor | Signature |
|---|---|
| `Request::new` | `fn new(method: Method, uri: Uri) -> Request` |
| `Request::builder` | `fn builder(method: Method, url: &str) -> Result<RequestBuilder, HttpError>` |

`builder` parses `url` into a [`Uri`](#uri) up front (so a bad URL surfaces as
`HttpError::InvalidUrl` immediately). `RequestBuilder` is a small fluent builder:

| Builder method | Signature |
|---|---|
| `header` | `fn header(self, name: impl Into<String>, value: impl Into<String>) -> RequestBuilder` |
| `body` | `fn body(self, body: impl Into<Vec<u8>>) -> RequestBuilder` |
| `build` | `fn build(self) -> Request` |

```rust
use ferroly::http::{Method, Request};

# fn ex() -> Result<(), ferroly::http::HttpError> {
let req = Request::builder(Method::Put, "http://localhost:8080/items/42")?
    .header("authorization", "Bearer secret")
    .header("content-type", "text/plain")
    .body("new value")
    .build();
# let _ = req;
# Ok(())
# }
```

You never have to set `Host`, `Content-Length`, or `Connection` yourself — the wire codec
adds them if you didn't (see [Framing rules](#framing--content-length-rules)). If you *do* set
them, yours win.

---

## Response (streaming body)

```rust
pub struct Response { /* … */ }
```

Returned by [`Client::send`](#client). The status line and headers are already read; the
**body is lazy** and streamed off the connection on demand, so you can process a large or
open-ended response without buffering it all in memory.

| Method | Signature | Description |
|---|---|---|
| `status` | `fn status(&self) -> StatusCode` | The response status. |
| `headers` | `fn headers(&self) -> &HeaderMap` | Response headers. |
| `is_success` | `fn is_success(&self) -> bool` | `true` for 2xx. |
| `chunk` | `async fn chunk(&mut self) -> Result<Option<Vec<u8>>, HttpError>` | Next body chunk, or `None` at end of body. |
| `bytes` | `async fn bytes(self) -> Result<Vec<u8>, HttpError>` | Drains the whole body into a `Vec<u8>`. |
| `text` | `async fn text(self) -> Result<String, HttpError>` | Drains the body as a UTF-8 (lossy) `String`. |

`chunk` borrows `&mut self` so you can loop; `bytes`/`text` consume `self` because they read to
end of body.

### Streaming a large download chunk-by-chunk

```rust
use ferroly::http::{Client, Method, Request};

# async fn ex() -> Result<(), ferroly::http::HttpError> {
let client = Client::new();
let req = Request::builder(Method::Get, "https://example.com/big-file")?.build();
let mut resp = client.send(req).await?;

let mut total = 0usize;
while let Some(chunk) = resp.chunk().await? {
    total += chunk.len();
    // write chunk to disk, hash it, feed an SSE parser, etc.
}
println!("received {total} bytes");
# Ok(())
# }
```

Chunk sizes are an implementation detail: for a `Content-Length` or `EOF`-framed body they cap
at 16 KiB reads; for a chunked-encoded body each `chunk()` returns exactly one decoded
transfer chunk (the `chunked` framing and trailers are stripped for you).

### Body framing is decided for you

The client picks a decoding strategy from the response status and headers:

- **`204`, `304`, and `1xx`** — no body at all (empty stream).
- **`Transfer-Encoding: chunked`** — chunked decoding, trailers consumed and discarded.
- **`Content-Length: N`** — exactly `N` bytes.
- **Neither** — read until the server closes the connection (HTTP/1.0-style EOF framing).

You don't choose; `chunk`/`bytes`/`text` just do the right thing.

---

## Server: `serve`

```rust
pub async fn serve<F>(
    listener: TcpListener,
    handler: Arc<dyn HttpHandler>,
    shutdown: F,
) -> Result<(), HttpError>
where
    F: Future<Output = ()> + Send;
```

Runs an accept loop on an already-bound `tokio` `TcpListener`, dispatching every connection to
`handler`, until the `shutdown` future resolves — then returns `Ok(())`. Each accepted socket
is handled on its own `tokio::spawn`ed task with `TCP_NODELAY` set, so slow clients don't block
each other.

Per request the server reads the request line + headers, reads the body (from `Content-Length`
or a chunked `Transfer-Encoding`), builds a [`Request`](#request--requestbuilder) whose `uri`
has an **empty scheme/host** and only the path + query (the `Host` header is copied into
`uri.host`), calls your handler, and writes the `HttpResponse`.

The server supports **HTTP/1.1 keep-alive**: successive requests are served on the same
connection unless the request/response carries `Connection: close`, up to
`max_keep_alive_requests` per connection; the `head_timeout` doubles as the idle timeout
between requests. Responses to `HEAD` and `204`/`304` correctly omit the message body so
framing stays in sync on a reused connection.

### Resource limits: `ServerConfig` / `serve_with_config`

`serve` applies a set of safe default limits so a single hostile client cannot exhaust the
process. To tune them, use `serve_with_config`:

```rust
use ferroly::http::{serve_with_config, ServerConfig};
use std::time::Duration;

let config = ServerConfig {
    max_body_bytes: 8 * 1024 * 1024, // reject larger bodies with 413 (no allocation)
    max_connections: 1024,           // cap concurrent connections (backpressure)
    head_timeout: Duration::from_secs(30), // request-head deadline / keep-alive idle timeout
    body_timeout: Duration::from_secs(60), // deadline for reading the body
    drain_timeout: Duration::from_secs(30), // wait for in-flight requests on shutdown
    max_keep_alive_requests: 1000,   // requests per kept-alive connection before close
};
serve_with_config(listener, handler, shutdown, config).await
```

`ServerConfig` implements `Default` (the same limits `serve` applies), so to change just one
knob you can spread the rest:

```rust
use ferroly::http::ServerConfig;

let config = ServerConfig {
    max_body_bytes: 32 * 1024 * 1024, // allow larger uploads, keep every other default
    ..ServerConfig::default()
};
# let _ = config;
```

The guarantees these provide:

- **Body cap** — a request whose `Content-Length` exceeds `max_body_bytes` is answered with
  `413 Payload Too Large` **before** any buffer is allocated (no attacker-controlled
  `vec![0u8; content_length]`). A chunked request body is dechunked and bounded by the same cap.
- **Head/body timeouts** — a peer that trickles the request head or body one byte at a time is
  dropped once the deadline passes, closing the classic slow-loris hole.
- **Connection cap** — a permit is acquired *before* `accept()`, so the server never pulls more
  sockets off the queue than it can serve; excess connections wait instead of exhausting file
  descriptors and memory.
- **Resilient accept loop** — a transient `accept()` failure (e.g. `EMFILE`/`ENFILE` fd
  exhaustion, `ECONNABORTED`) backs off briefly and continues rather than tearing down the
  whole listener.
- **Graceful drain** — when `shutdown` resolves the server stops accepting, then waits up to
  `drain_timeout` for in-flight requests to finish before returning, so a rolling deploy does
  not cut active responses.
- **Request-smuggling defense** — a request carrying both `Content-Length` and
  `Transfer-Encoding: chunked`, or conflicting duplicate `Content-Length` values, is rejected
  with `400 Bad Request` (CL.TE / TE.CL desync). Chunked **request** bodies are now decoded
  (previously ignored).

In addition, the header reader bounds each request/header line (16 KiB), the header count
(128), and the total header block (64 KiB), independent of `ServerConfig`.

### HTTPS: `serve_tls` / `TlsConfig`

To terminate TLS in-process, build a `TlsConfig` from a certificate chain + private key and
call `serve_tls` (or `serve_tls_with_config` for custom limits):

```rust
use ferroly::http::{serve_tls, TlsConfig};

# async fn ex(listener: tokio::net::TcpListener, handler: std::sync::Arc<dyn ferroly::http::HttpHandler>) -> Result<(), ferroly::http::HttpError> {
let cert_pem = std::fs::read("server.crt")?;
let key_pem = std::fs::read("server.key")?;
let tls = TlsConfig::from_pem(&cert_pem, &key_pem)?;       // or TlsConfig::from_der(certs, key)
serve_tls(listener, handler, std::future::pending::<()>(), tls).await
# }
```

`TlsConfig` is an opaque handle (it does not leak `rustls` types into your code). `from_pem`
accepts one or more `CERTIFICATE` blocks (leaf first) and a `PRIVATE KEY` / `RSA PRIVATE KEY` /
`EC PRIVATE KEY` block; `from_der` takes the raw DER. The TLS handshake is bounded by
`head_timeout`, and the server sends a proper `close_notify` on shutdown. The same
[`ServerConfig`](#resource-limits-serverconfig--serve_with_config) limits and graceful drain
apply. Client-side TLS verification (webpki roots) is unchanged — see [Client](#client).

For graceful shutdown, pass any future — a signal, a `oneshot`, a timer:

```rust
use std::sync::Arc;
use ferroly::http::{serve, HttpHandler};
use tokio::net::TcpListener;

# async fn ex(handler: Arc<dyn HttpHandler>) -> Result<(), ferroly::http::HttpError> {
let listener = TcpListener::bind("0.0.0.0:8080").await?;
let (tx, rx) = tokio::sync::oneshot::channel::<()>();

// … hand `tx` to your shutdown logic …

serve(listener, handler, async { let _ = rx.await; }).await
# }
```

To serve forever, pass `std::future::pending::<()>()` — which is exactly what
[`turbo::Router::serve`](turbo.md) does internally.

### `HttpHandler` trait

```rust
pub trait HttpHandler: Send + Sync {
    fn handle(&self, req: Request) -> BoxFuture<'_, HttpResponse>;
}
```

The one method a server handler implements. It is object-safe by design: `handle` returns a
`BoxFuture` (the type alias below) rather than an `async fn`, so it can live behind
`Arc<dyn HttpHandler>`.

```rust
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
```

Implement it directly for a bare server, or — far more commonly — let
[`turbo::Router::into_handler`](turbo.md) produce one for you.

---

## HttpResponse

```rust
pub struct HttpResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    // plus a private streaming body, set via `stream` / `event_stream`
}
```

The owned outgoing response a handler returns. The body is **either** the fixed `body` bytes
(written with a `Content-Length`) **or** a chunked stream (written with
`Transfer-Encoding: chunked` and flushed per chunk).

| Constructor / method | Signature | Description |
|---|---|---|
| `new` | `fn new(status: StatusCode) -> HttpResponse` | Given status, empty body. |
| `ok` | `fn ok() -> HttpResponse` | `200 OK`, empty body. |
| `text` | `fn text(status: StatusCode, text: impl Into<String>) -> HttpResponse` | `text/plain; charset=utf-8` body. |
| `header` | `fn header(self, name: impl Into<String>, value: impl Into<String>) -> HttpResponse` | Sets a header (builder-style). |
| `body` | `fn body(self, body: impl Into<Vec<u8>>) -> HttpResponse` | Sets the fixed body. |
| `stream` | `fn stream(status: StatusCode, chunks: mpsc::Receiver<Vec<u8>>) -> HttpResponse` | Chunked streaming body. |
| `event_stream` | `fn event_stream(events: mpsc::Receiver<String>) -> HttpResponse` | Server-Sent Events body. |

```rust
use ferroly::http::{HttpResponse, StatusCode};

// Fixed JSON body.
let resp = HttpResponse::new(StatusCode::CREATED)
    .header("content-type", "application/json")
    .body(br#"{"id":42}"#.to_vec());

// Plain text.
let resp2 = HttpResponse::text(StatusCode::NOT_FOUND, "404 Not Found");
# let _ = (resp, resp2);
```

### Streaming response bodies: `stream` / `event_stream`

For responses whose length you don't know up front — a long computation, a proxied upstream, a
live feed — hand `HttpResponse` an `mpsc::Receiver` and send chunks into the paired `Sender`
from another task. The server writes `Transfer-Encoding: chunked` and **flushes after every
chunk**, so clients see data live rather than at the end.

```rust
use ferroly::http::{HttpResponse, StatusCode};
use tokio::sync::mpsc;

// Inside a handler:
let (tx, rx) = mpsc::channel::<Vec<u8>>(16);
tokio::spawn(async move {
    for i in 0..5 {
        let _ = tx.send(format!("line {i}\n").into_bytes()).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    // Dropping tx ends the stream (a terminating `0\r\n\r\n` is written).
});
let resp = HttpResponse::stream(StatusCode::OK, rx)
    .header("content-type", "text/plain");
# let _ = resp;
```

Empty `Vec<u8>` chunks are skipped (they would otherwise look like the chunked-encoding
terminator). When the sender is dropped and the channel drains, the server emits the final
`0\r\n\r\n`.

`event_stream` is a Server-Sent Events convenience built on top of `stream`. Give it a
`Receiver<String>`; each string is framed as `data: <event>\n\n`, and the response is
pre-populated with `content-type: text/event-stream` and `cache-control: no-cache`:

```rust
use ferroly::http::HttpResponse;
use tokio::sync::mpsc;

let (tx, rx) = mpsc::channel::<String>(16);
tokio::spawn(async move {
    let _ = tx.send("hello".to_string()).await;      // -> "data: hello\n\n"
    let _ = tx.send("world".to_string()).await;      // -> "data: world\n\n"
});
let resp = HttpResponse::event_stream(rx);
# let _ = resp;
```

Internally `event_stream` spawns a task that adapts the `String` channel to the `Vec<u8>`
channel `stream` consumes, so you don't manage the SSE framing yourself. This pairs naturally
with an LLM token stream from [`ferroly::genai`](genai.md).

---

## Core value types

### Method

```rust
pub enum Method { Get, Post, Put, Delete, Patch, Head, Options, Trace, Connect, Other(String) }
```

The nine named HTTP methods plus `Other(String)` for extensions. Two helpers:

- `Method::as_str(&self) -> &str` — the canonical uppercase token (`"GET"`, `"POST"`, …).
- `Method::parse(s: &str) -> Method` — the inverse; unknown tokens become `Other(s.to_string())`.

`Method` derives `Clone, PartialEq, Eq`, so you can match on it and compare directly.

### StatusCode

```rust
pub struct StatusCode(pub u16);
```

A transparent `u16` newtype (so `StatusCode(418)` is fine for anything not in the constant
list). Provided constants:

| Const | Code | | Const | Code |
|---|---|---|---|---|
| `OK` | 200 | | `METHOD_NOT_ALLOWED` | 405 |
| `CREATED` | 201 | | `NOT_ACCEPTABLE` | 406 |
| `NO_CONTENT` | 204 | | `TOO_MANY_REQUESTS` | 429 |
| `BAD_REQUEST` | 400 | | `INTERNAL_SERVER_ERROR` | 500 |
| `UNAUTHORIZED` | 401 | | `SERVICE_UNAVAILABLE` | 503 |
| `FORBIDDEN` | 403 | | | |
| `NOT_FOUND` | 404 | | | |

Methods:

- `as_u16(self) -> u16` — the numeric code.
- `is_success(self) -> bool` — `true` for `200..300`.
- `reason(self) -> &'static str` — a canonical reason phrase for common codes (`200 → "OK"`,
  `404 → "Not Found"`, `503 → "Service Unavailable"`, …; `""` for unrecognized codes). This is
  what the server writes into the status line.

```rust
use ferroly::http::StatusCode;

assert_eq!(StatusCode::NOT_ACCEPTABLE.as_u16(), 406);
assert!(StatusCode::CREATED.is_success());
assert_eq!(StatusCode::SERVICE_UNAVAILABLE.reason(), "Service Unavailable");
let teapot = StatusCode(418); // any code is representable
# let _ = teapot;
```

### HeaderMap

```rust
pub struct HeaderMap { /* ordered Vec<(String, String)> */ }
```

An **ordered, case-insensitive** header collection. Insertion order is preserved (it iterates
in the order values were added), and lookups compare names ASCII-case-insensitively. It allows
duplicate names via `append` (e.g. multiple `Set-Cookie`).

| Method | Signature | Description |
|---|---|---|
| `new` | `fn new() -> HeaderMap` | Empty map (also `Default`). |
| `set` | `fn set(&mut self, name: impl Into<String>, value: impl Into<String>)` | Replaces any existing same-named values. |
| `append` | `fn append(&mut self, name: impl Into<String>, value: impl Into<String>)` | Adds without removing duplicates. |
| `get` | `fn get(&self, name: &str) -> Option<&str>` | First value for a name. |
| `contains` | `fn contains(&self, name: &str) -> bool` | Presence check. |
| `content_length` | `fn content_length(&self) -> Option<u64>` | Parsed `Content-Length`, if valid. |
| `is_chunked` | `fn is_chunked(&self) -> bool` | `true` if `Transfer-Encoding` mentions `chunked`. |
| `iter` | `fn iter(&self) -> impl Iterator<Item = (&str, &str)>` | Pairs in insertion order. |

```rust
use ferroly::http::HeaderMap;

let mut h = HeaderMap::new();
h.set("Content-Type", "application/json");
assert_eq!(h.get("content-type"), Some("application/json")); // case-insensitive
h.append("Set-Cookie", "a=1");
h.append("Set-Cookie", "b=2");                                // both kept
for (name, value) in h.iter() {
    println!("{name}: {value}");
}
```

### Uri

```rust
pub struct Uri {
    pub scheme: String,        // "http" | "https" (empty for a bare request target)
    pub host: String,
    pub port: u16,             // defaulted from the scheme
    pub path: String,          // always begins with '/'
    pub query: Option<String>, // without the leading '?'
}
```

A parsed absolute URL (client side) or request target (server side). Construct one with
`Uri::parse`:

| Method | Signature | Description |
|---|---|---|
| `parse` | `fn parse(url: &str) -> Result<Uri, HttpError>` | Parses an absolute `http(s)`/`ws(s)` URL. |
| `is_tls` | `fn is_tls(&self) -> bool` | `true` for `https`/`wss`. |
| `authority` | `fn authority(&self) -> String` | The `Host` header value — `host` or `host:port` for non-default ports. |
| `request_target` | `fn request_target(&self) -> String` | Origin-form target `/path?query`. |

`parse` accepts `http`/`ws` (default port 80) and `https`/`wss` (default port 443); any other
scheme is `HttpError::InvalidUrl`. There is also a free helper:

```rust
pub fn split_target(target: &str) -> (String, Option<String>);
```

which splits a request target into `(path, query)`, defaulting an empty path to `/`. The
server uses it to parse the request line.

```rust
use ferroly::http::Uri;

let u = Uri::parse("https://api.example.com/v1/chat?x=1").unwrap();
assert_eq!(u.scheme, "https");
assert_eq!(u.host, "api.example.com");
assert_eq!(u.port, 443);
assert_eq!(u.path, "/v1/chat");
assert_eq!(u.query.as_deref(), Some("x=1"));
assert!(u.is_tls());
assert_eq!(u.authority(), "api.example.com");         // default port omitted
assert_eq!(u.request_target(), "/v1/chat?x=1");

let local = Uri::parse("http://localhost:8080/").unwrap();
assert_eq!(local.authority(), "localhost:8080");      // non-default port kept
```

---

## TLS transport: the single crypto exception

All connection setup lives in `http::transport`, behind two public items:

```rust
pub trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
pub type Conn = Box<dyn Io>;
```

`Io` is a blanket-implemented marker for any duplex byte stream; `Conn` is a **boxed** one.
This box is *the single place* where "TLS vs. plaintext" is erased: a plain `TcpStream` and a
`tokio_rustls` TLS stream both become a `Conn`, and the request/response codec, the `Client`,
and the server all speak only to `Conn` — they never see `rustls`.

This is deliberate and load-bearing for the crate's dependency policy:

- **It is the crate's only cryptography.** Everything else in `ferroly` — JWT HS256 in
  [`auth`](auth.md), hashing, etc. — is hand-rolled; TLS is the one primitive that must not be
  reimplemented, so it is quarantined here.
- **It is swappable.** Because the boundary is `Box<dyn Io>`, replacing `rustls` with another
  TLS implementation (or plugging in `native-tls`) touches only `transport.rs` — no public API
  changes, and no other module recompiles against a new crypto crate.
- **The crypto provider is named explicitly.** The client `rustls::ClientConfig` is built with
  `ring` named directly (`builder_with_provider(ring::default_provider())`) rather than relying
  on the process-default provider. If some other dependency's feature unification drags a
  second provider (`aws-lc-rs`) into the build, the ambiguous default would otherwise panic
  `ClientConfig::builder()` at runtime; naming `ring` keeps TLS setup deterministic. Roots come
  from `webpki-roots`.

The client `Client::send` path is: `transport::connect(&uri, &tls)` → for `https`, wrap the
TCP stream in a `tokio_rustls` connector using the host as the SNI server name; for `http`,
return the bare TCP stream — either way a `Conn`. Server-side TLS uses the same seam:
[`serve_tls`](#https-serve_tls--tlsconfig) wraps each accepted `TcpStream` in a `tokio_rustls`
acceptor built from your [`TlsConfig`](#https-serve_tls--tlsconfig), producing a `Conn` the
accept loop serves exactly like a plaintext one. The plain `serve` accept loop stays cleartext.

---

## Framing & Content-Length rules

The wire codec (`http::io`) applies these rules so you rarely set framing headers by hand.

**Writing a request** (`Client`): adds `Host` (from `uri.authority()`) and `Content-Length`
(when the body is non-empty and you didn't set one) — each only if you didn't already provide
it. It does **not** force `Connection: close` (HTTP/1.1 defaults to keep-alive so the pool can
reuse the connection); set `Connection: close` yourself to opt out.

**Writing a response** (server): writes the status line using `StatusCode::reason()`, then your
headers, then:

- if the response is **streaming** → adds `Transfer-Encoding: chunked` (unless you set it),
  writes each chunk as `<hex-len>\r\n<bytes>\r\n` and flushes, then a terminating `0\r\n\r\n`;
- otherwise → adds `Content-Length: <body.len()>` (unless you set it) and writes the body;
- for `HEAD` responses the body is omitted (Content-Length kept); `204`/`304` omit both;
- adds `Connection: keep-alive` (or `Connection: close` when closing) unless you set one.

**Reading a response body** (client) follows the [framing rules](#body-framing-is-decided-for-you)
above (`chunked` → `Content-Length` → EOF; empty for `204`/`304`/`1xx`).

Both sides speak **HTTP/1.1 keep-alive** by default (persistent connections, one request at a
time per connection). There is no request **pipelining** — a connection serves one
request/response exchange before the next.

---

## Error handling

Every fallible operation returns `Result<_, HttpError>`:

```rust
pub enum HttpError {
    InvalidUrl(String),   // "invalid url: {0}" — bad or unsupported-scheme URL
    Io(std::io::Error),   // "io error: {0}"   — connect/read/write failure (From<io::Error>)
    Tls(String),          // "tls error: {0}"  — TLS handshake / bad SNI name
    Protocol(String),     // "malformed http: {0}" — unparseable status/request line, bad chunk
    Timeout,              // "timed out" — the per-request timeout fired
}
```

- `InvalidUrl` surfaces at `Request::builder` / `Uri::parse` time (unsupported scheme, missing
  host, un-parseable port).
- `Io` wraps any `std::io::Error` via `#[from]`, so `?` works against raw socket errors.
- `Tls` covers an invalid SNI server name or a handshake failure.
- `Protocol` means the peer sent something malformed — an empty response, a status/request line
  that doesn't parse, or a bad chunk size.
- `Timeout` comes only from `Client::send` when the configured timeout elapses.

The type derives `ferroly_derive::FerrolyError` (see [`ferroly::derive`](derive.md)), giving it
`Display` (the messages above) and `std::error::Error`, so it composes with
[`ferroly::errutils`](errutils.md).

```rust
use ferroly::http::{Client, HttpError, Method, Request};

# async fn ex() {
let client = Client::new();
let req = Request::builder(Method::Get, "https://example.com/").unwrap().build();
match client.send(req).await {
    Ok(resp) => println!("ok: {}", resp.status().as_u16()),
    Err(HttpError::Timeout) => eprintln!("request timed out"),
    Err(HttpError::Tls(e)) => eprintln!("TLS problem: {e}"),
    Err(e) => eprintln!("failed: {e}"),
}
# }
```

---

## Security & robustness notes

- **Header injection is prevented.** `HeaderMap::set`/`append` strip CR/LF from names and
  values, so reflected user input can't split the response or inject extra headers.
- **`Expect: 100-continue`** is honored: the server sends an interim `100 Continue` before
  reading a body when the client asks, avoiding upload stalls.
- **Allocation is bounded.** The server caps request bodies/headers ([`ServerConfig`](#resource-limits-serverconfig--serve_with_config)); the client caps a server-declared chunk size (64 MiB) so a hostile peer can't force a huge allocation.

## Limitations

- **HTTP/1.1 only.** No HTTP/2 or HTTP/3.
- **Keep-alive, but no pipelining.** Both client and server reuse persistent connections; the
  client pools idle connections per host. A connection serves one request/response at a time —
  there is no request pipelining.
- **No automatic redirect following, cookie jar, decompression, or retry.** Those belong to the
  higher layers — see [`ferroly::rest`](rest.md) / [`ferroly::clients`](clients.md).
- **Server-side TLS** is available via [`serve_tls`](#https-serve_tls--tlsconfig); the plain
  `serve` accept loop is cleartext.
- **Bodies buffered where noted.** `bytes`/`text` buffer the whole body; use `chunk` for large
  or open-ended responses.

---

## See also

- [`ferroly::rest`](rest.md) — codec-aware client + server framework built on this stack.
- [`ferroly::clients`](clients.md) — higher-level HTTP client ergonomics (auth, retry).
- [`ferroly::turbo`](turbo.md) — the first-class router that produces an `HttpHandler`.
- [`ferroly::codec`](codec.md) — `Encode`/`Decode` traits for typed request/response bodies.
- [`ferroly::genai`](genai.md) — LLM client that streams tokens (pairs with `event_stream`).
- [`ferroly::ws`](ws.md) — WebSocket support layered over the same transport.
- [`ferroly::derive`](derive.md) — the `FerrolyError` derive behind `HttpError`.

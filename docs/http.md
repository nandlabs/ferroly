# ferroly::http

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `http` (pulls in `tokio` + the TLS stack `tokio-rustls`/`rustls`/`ring`/`webpki-roots`).

A **hand-rolled HTTP/1.1 stack** — client *and* server — built directly over `tokio`, with
HTTPS via `tokio-rustls`. It is the dependency-minimal replacement for the
`reqwest`/`hyper`/`http`/`axum` layer of the Go original: URL parsing, an HTTP/1.1
request/response wire codec (Content-Length **and** chunked framing), a TLS-or-plaintext
transport, a streaming client, and a server accept loop with a handler trait.

There is **no `hyper`, `http`, or `reqwest`** anywhere in the tree. The only cryptography is
the TLS transport, which is isolated behind a boxed I/O object (see
[TLS transport](#tls-transport-the-single-crypto-exception)) so nothing else in the crate —
or in *your* code — is coupled to a `rustls` version.

Both sides **parse strictly** and **bound everything**: any message two HTTP implementations
could read differently is rejected rather than repaired, and every line, header block, body
and wait has a configurable limit or timeout. The rules and defaults are collected in
[Security hardening](#security-hardening).

Higher-level ergonomics (codec-encoded bodies, auth, retry, `${}` path templating) live one
layer up in [`ferroly::rest`](rest.md) and [`ferroly::clients`](clients.md); routing lives in
[`ferroly::turbo`](turbo.md). This page documents the raw HTTP primitives those layers are
built on — you can also use them directly.

---

## Overview

| Concern | Type / function |
|---|---|
| Client | [`Client`](#client) (`new`, `builder`, `send`, `send_streaming`) — keep-alive connection pooling |
| Client configuration | [`ClientBuilder`](#clientbuilder-timeouts-limits-pool) — timeouts, body cap, pool, redirects, proxy, TLS, decompression, interceptors |
| Outgoing request | [`Request`](#request--requestbuilder) + [`RequestBuilder`](#request--requestbuilder), [`Multipart`](#form-and-multipart-bodies) |
| Incoming response (streaming) | [`Response`](#response-streaming-body) (`status`, `headers`, `uri`, `chunk`, `bytes`, `text`) |
| Downloads | [`download_to_file`](#range-requests-and-resumable-downloads) |
| Proxies | [`Proxy`](#proxies) |
| Client hooks | [`Interceptor`](#interceptors) |
| Server | [`serve`](#server-serve) + [`HttpHandler`](#httphandler-trait) trait |
| Server tuning | [`ServerConfig`](#resource-limits-serverconfig--serve_with_config), [`ErrorHook`](#errors-and-panics-errorhook) |
| TLS | [`TlsConfig`](#https-serve_tls--tlsconfig) / [`TlsConfigBuilder`](#mutual-tls-and-minimum-version), [`ReloadableTlsConfig`](#certificate-reload), [`ClientTlsConfig`](#client-tls-custom-roots-client-certificates), [`TlsVersion`](#mutual-tls-and-minimum-version) |
| Outgoing response | [`HttpResponse`](#httpresponse) (`new`/`ok`/`text`/`header`/`body`, plus [`stream`](#streaming-response-bodies-stream--event_stream) / [`event_stream`](#streaming-response-bodies-stream--event_stream) / [`sse`](#structured-server-sent-events-sse--httpsse)) |
| Value types | [`Method`](#method), [`StatusCode`](#statuscode), [`HeaderMap`](#headermap), [`Uri`](#uri) |
| URL helpers | [`percent_encode`, `percent_decode`, `normalize_path`, `form_urlencode`](#url-and-date-helpers), `format_http_date` |
| Transport | [`Conn`](#tls-transport-the-single-crypto-exception), [`Io`](#tls-transport-the-single-crypto-exception) |
| Errors | [`HttpError`](#error-handling) |

Everything is re-exported from `ferroly::http`:

```rust
use ferroly::http::{
    Client, ClientBuilder, ClientTlsConfig, Interceptor, Proxy,
    Request, RequestBuilder, Response, Multipart, download_to_file,
    serve, serve_with_config, serve_tls, serve_tls_with_config, serve_tls_reloadable,
    ServerConfig, ErrorHook, HttpHandler, HttpResponse, BoxFuture,
    TlsConfig, TlsConfigBuilder, ReloadableTlsConfig, TlsVersion,
    Method, StatusCode, HeaderMap, Uri, split_target,
    percent_encode, percent_decode, normalize_path, form_urlencode, format_http_date,
    Event, SseDecoder, Conn, Io, HttpError,
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

An HTTP/1.1 client with **keep-alive connection pooling**. Each `send` reuses an idle
connection to the same `host:port` (and TLS-ness, and proxy) if one is available, otherwise
opens a fresh TCP/TLS connection. When the response body is **fully drained** (via
`bytes()`/`text()`, or by reading `chunk()` to `None`) and the response is keep-alive-able,
the connection is returned to the pool for reuse — avoiding a fresh handshake on the next
request. It is `Clone` (clones **share the pool**) and `Send + Sync`, so create one `Client`
and share it across tasks.

> **Drain to reuse.** A connection is only pooled once its body is consumed. If you drop a
> `Response` without reading the body, that connection is closed rather than reused — except
> for responses that have no body at all (`HEAD`, `204`, `304`, `Content-Length: 0`), whose
> connection is returned immediately. `Eof`-framed responses (no `Content-Length`, not
> chunked), `Connection: close` responses and HTTP/1.0 responses without
> `Connection: keep-alive` are never pooled.

| Method | Signature | Notes |
|---|---|---|
| `Client::new` | `fn new() -> Client` | The defaults in the table below; also `Default`. Never reads the environment. |
| `Client::builder` | `fn builder() -> ClientBuilder` | Configure timeouts, limits, pool, TLS, proxy, redirects, decompression, interceptors. |
| `Client::with_timeout` | `fn with_timeout(self, timeout: Option<Duration>) -> Client` | Sets the **response-head** timeout (same as `ClientBuilder::timeout`); `None` disables it. |
| `Client::send` | `async fn send(&self, req: Request) -> Result<Response, HttpError>` | Sends the request, returns the final response with a **streaming** body. |
| `Client::send_streaming` | `async fn send_streaming<B: AsyncRead + Send + Unpin>(&self, req: Request, body: B) -> Result<Response, HttpError>` | Uploads the body from an async reader; see [Streaming uploads](#streaming-uploads). |

```rust
use std::time::Duration;
use ferroly::http::{Client, Method, Request};

# async fn ex() -> Result<(), ferroly::http::HttpError> {
// Ten seconds to get a response head instead of the 60s default.
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

`send` always returns the **final** response: interim `1xx` responses (`100 Continue`,
`103 Early Hints`, …) are read and skipped, so they can never be mistaken for the answer or
leave bytes behind on a pooled connection. (`101 Switching Protocols` is final.)

### ClientBuilder: timeouts, limits, pool

```rust
use std::time::Duration;
use ferroly::http::Client;

# fn ex() -> Result<(), ferroly::http::HttpError> {
let client = Client::builder()
    .connect_timeout(Some(Duration::from_secs(5)))
    .timeout(Some(Duration::from_secs(15)))        // until the response head
    .read_timeout(Some(Duration::from_secs(30)))   // idle time between body reads
    .total_timeout(Some(Duration::from_secs(120))) // the whole exchange, body included
    .max_response_body_bytes(Some(16 * 1024 * 1024))
    .user_agent("my-service/1.4")
    .build()?;
# let _ = client;
# Ok(())
# }
```

| Builder method | Default | Meaning |
|---|---|---|
| `timeout(Option<Duration>)` | 60 s | Deadline for the **response head**: taking or opening a connection, writing the request, reading the status line and headers. Expiry → `HttpError::Timeout`. |
| `connect_timeout(Option<Duration>)` | 30 s | DNS + TCP connect + TLS handshake + any proxy `CONNECT`. Expiry → `HttpError::ConnectTimeout` (nothing was sent, so any method is safe to retry). |
| `read_timeout(Option<Duration>)` | 300 s | **Idle** timeout while reading the body: the longest wait for the next piece. Also bounds each step of a streamed upload. Expiry → `HttpError::Timeout` from `chunk`/`bytes`/`text`. |
| `total_timeout(Option<Duration>)` | none | Overall deadline from `send` until the last body byte (redirects included). |
| `max_response_body_bytes(Option<u64>)` | 64 MiB | Cap on a response body, for every framing; see [Response-body cap](#response-body-cap). |
| `user_agent(impl Into<String>)` / `no_user_agent()` | `ferroly/<version>` | `User-Agent` sent when the request does not set one. |
| `pool_max_idle_per_host(usize)` | 32 | Idle connections kept per target; `0` disables pooling. |
| `pool_idle_timeout(Option<Duration>)` | 90 s | How long an idle connection is kept. |
| `max_redirects(usize)` | 0 | Redirects to follow; see [Redirects](#redirects). |
| `proxy(Proxy)` / `proxy_from_env()` | none | See [Proxies](#proxies). |
| `tls(ClientTlsConfig)` | web PKI roots, TLS 1.2+ | See [Client TLS](#client-tls-custom-roots-client-certificates). |
| `decompress(bool)` | off | See [Decompression](#decompression). |
| `interceptor(impl Interceptor)` | none | See [Interceptors](#interceptors). |
| `build()` | | `Result<Client, HttpError>` — fails only for invalid TLS options. |

How the four timeouts fit together for one request:

```text
send() ──► [ connect ]──►[ write request ]──►[ read head ]──►[ read body … ]
            connect_timeout
           └──────────────── timeout ───────────────────┘   read_timeout per read
           └───────────────────────── total_timeout ─────────────────────────────┘
```

**Pool hygiene.** Idle connections are evicted when they pass `pool_idle_timeout`, the oldest
is closed when a host exceeds `pool_max_idle_per_host`, and every connection is checked when
it is taken out: one the server has already closed — or that has unsolicited bytes waiting —
is discarded before anything is written to it.

### Retry on a dead pooled connection

A pooled connection can die between the liveness check and the write. When an exchange on a
**reused** connection fails, the client re-sends the request on a fresh connection only if

- **no response byte was received**, and
- the method is **idempotent** (`GET`, `HEAD`, `PUT`, `DELETE`, `OPTIONS`, `TRACE`), **or**
  nothing of the request was written (the very first write failed).

Otherwise the error is returned. A `POST` that the server may have received is therefore
never sent twice by this layer. Streamed uploads always use a fresh connection and are never
re-sent.

### Response-body cap

A hostile or broken server cannot make the client buffer without bound. With the cap at `N`
bytes (64 MiB unless configured):

- `Content-Length` larger than `N` → `HttpError::BodyTooLarge` on the first read, before
  anything is buffered;
- a chunked body → the error is raised as soon as the declared chunk sizes add up past `N`;
- a read-until-close body → the error is raised when more than `N` bytes have arrived;
- with [decompression](#decompression), the **decoded** size is capped as well.

The cap applies to streaming with `chunk()` too. Lift or change it for one response with
`Response::set_max_body_bytes` (as `download_to_file` does), or for the client with
`max_response_body_bytes(None)`.

### Redirects

Off by default: a `3xx` is returned to you as it is. `max_redirects(n)` follows up to `n`
hops of `301`, `302`, `303`, `307` and `308`:

| Status | Method on the next request | Body |
|---|---|---|
| `303` | `GET` (`HEAD` stays `HEAD`) | dropped, with the `Content-*` headers |
| `301`, `302` | `POST` → `GET`; other methods unchanged | dropped for the `POST` → `GET` case |
| `307`, `308` | unchanged | re-sent |

- A relative `Location` is resolved against the request URL (`Uri::resolve`); the fragment is
  dropped. A `Location` that is not a valid `http(s)` URL is an error (`HttpError::InvalidUrl`).
- On a redirect to a **different origin** (scheme, host or port) the `Authorization`, `Cookie`,
  `Proxy-Authorization` and `Host` headers are removed.
- One hop too many → `HttpError::TooManyRedirects`.
- `Response::uri()` reports the final URL.
- Requests sent with `send_streaming` are never redirected (the stream cannot be replayed).

### Proxies

```rust
use ferroly::http::{Client, Proxy};

# fn ex() -> Result<(), ferroly::http::HttpError> {
let proxy = Proxy::all("http://proxy.internal:3128")?
    .basic_auth("svc", "s3cret")
    .no_proxy("localhost,127.0.0.1,.internal");
let client = Client::builder().proxy(proxy).build()?;
# let _ = client;
# Ok(())
# }
```

- `http://` targets are sent **to** the proxy with an absolute-form request target
  (`GET http://host/path HTTP/1.1`).
- `https://` targets go **through** the proxy in a `CONNECT host:port` tunnel; TLS is
  negotiated end to end inside it, so the proxy sees the host and port and nothing else.
- The proxy itself may be plain (`http://proxy:3128`) or TLS (`https://proxy:3129`).
- `Proxy-Authorization: Basic …` is sent to the proxy only — never to the target, and not
  through a tunnel.

| Constructor / method | Description |
|---|---|
| `Proxy::all(url)` | Proxy for both `http://` and `https://` targets. |
| `Proxy::http(url)` / `Proxy::https(url)` | Proxy for one target scheme only. |
| `.with_http(url)` / `.with_https(url)` | Add (or replace) the proxy for one target scheme. |
| `.basic_auth(user, password)` | Credentials for every proxy configured so far. They can also be embedded in the URL as percent-encoded `user:password@`. |
| `.no_proxy(list)` | Bypass list in `NO_PROXY` syntax: comma-separated; `*` matches everything; `example.com` or `.example.com` matches the host and its subdomains; an IP literal matches exactly; `host:port` restricts to one port. No CIDR ranges. |
| `Proxy::from_env()` | **Opt-in** environment configuration; `None` when nothing is set. |
| `Proxy::from_lookup(fn)` | The same, reading variables through your function. |

`from_env` reads `http_proxy`/`HTTP_PROXY`, `https_proxy`/`HTTPS_PROXY`,
`all_proxy`/`ALL_PROXY` (fallback for both) and `no_proxy`/`NO_PROXY`; the lower-case name
wins. Upper-case `HTTP_PROXY` is ignored when `REQUEST_METHOD` is set, because in a CGI
environment that variable is controlled by the remote client ("httpoxy").
`Client::new()` never looks at the environment; use `Client::builder().proxy_from_env()`.

A refused or failed tunnel is `HttpError::Proxy`.

### Decompression

`Client::builder().decompress(true)` makes the client send `Accept-Encoding: gzip, deflate`
(unless the request sets its own) and decode a `gzip` or `deflate` response body as it is
read, with a hand-rolled, streaming INFLATE decoder. The `Content-Encoding` and
`Content-Length` headers are removed from the `Response`, since they describe bytes you no
longer see. Other encodings (`br`, `zstd`, or a list) are passed through untouched.

It is **off by default** — nothing is advertised and nothing is decoded.

The decoder is bounded and strict: output is produced in small pieces, the
[response-body cap](#response-body-cap) applies to the decoded size (a few kilobytes cannot
expand into gigabytes), and a corrupt stream, a CRC-32 / Adler-32 / length mismatch or a
truncated stream is an `HttpError::Protocol`.

### Interceptors

```rust
use ferroly::http::{Client, HeaderMap, HttpError, Interceptor, Request, StatusCode};

struct Trace;

impl Interceptor for Trace {
    fn on_request(&self, req: &mut Request) -> Result<(), HttpError> {
        req.headers.set("traceparent", "00-…");
        Ok(())
    }
    fn on_response(&self, req: &Request, status: StatusCode, _headers: &HeaderMap) {
        println!("{} {} -> {}", req.method.as_str(), req.uri, status.as_u16());
    }
    fn on_error(&self, req: &Request, error: &HttpError) {
        eprintln!("{} {} failed: {error}", req.method.as_str(), req.uri);
    }
}

# fn ex() -> Result<(), HttpError> {
let client = Client::builder().interceptor(Trace).build()?;
# let _ = client;
# Ok(())
# }
```

All three hooks have empty defaults. They run in the order the interceptors were added, once
per hop when redirects are followed. `on_request` may modify the request or veto it by
returning an error; `on_response` sees the final response head; `on_error` sees a failure
that happened before a response head arrived.

### Streaming uploads

`Client::send_streaming(req, body)` reads the request body from any
`AsyncRead + Send + Unpin` while it uploads, instead of from `Request::body`:

```rust
use ferroly::http::{Client, Method, Request};

# async fn ex() -> Result<(), ferroly::http::HttpError> {
let file = tokio::fs::File::open("backup.tar").await?;
let req = Request::builder(Method::Put, "https://example.com/backups/today")?.build();
let resp = Client::new().send_streaming(req, file).await?;
# let _ = resp;
# Ok(())
# }
```

The body is sent with `Transfer-Encoding: chunked`, or — if you set a `Content-Length`
header — as exactly that many bytes (a shorter stream is an error). The response-head timeout
starts when the upload finishes; each read and write during the upload is bounded by
`read_timeout`.

### Client TLS: custom roots, client certificates

```rust
use ferroly::http::{Client, ClientTlsConfig, TlsVersion};

# fn ex(ca_pem: &[u8], cert_pem: &[u8], key_pem: &[u8]) -> Result<(), ferroly::http::HttpError> {
let tls = ClientTlsConfig::new()
    .add_root_pem(ca_pem)?             // trust a private CA as well as the web PKI
    .identity_pem(cert_pem, key_pem)?  // present a client certificate (mutual TLS)
    .min_version(TlsVersion::Tls13);
let client = Client::builder().tls(tls).build()?;
# let _ = client;
# Ok(())
# }
```

| Method | Description |
|---|---|
| `ClientTlsConfig::new()` | Bundled web PKI roots, no client certificate, TLS 1.2 or newer. |
| `.add_root_pem(pem)?` / `.add_root_der(der)` | Trust additional CA certificate(s). |
| `.without_web_pki_roots()` | Trust **only** the roots you added (pin to a private CA). Building with no root at all is an error. |
| `.identity_pem(cert, key)?` / `.identity_der(chain, key)` | Client certificate chain (leaf first) and key (PKCS#8, PKCS#1 or SEC1). |
| `.min_version(TlsVersion)` | Oldest protocol version to negotiate (`Tls12` or `Tls13`). |

Server certificates are **always** verified — chain, validity period and host name. There is
deliberately no switch to turn verification off; to talk to a server with a private or
self-signed certificate, add that certificate (or its CA) as a root.

---

## Request / RequestBuilder

```rust
pub struct Request {
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    // plus private connection metadata, filled in by the server
}
```

`Request` is a `Clone`-able struct used on **both** sides: the client fills it in and
sends it; the server hands you one parsed from the wire (with `uri` reduced to path + query —
see [`serve`](#server-serve)). Because it has a private field, build one with `Request::new`
or `Request::builder` rather than a struct literal.

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
| `form` | `fn form(self, pairs: impl IntoIterator<Item = (K, V)>) -> RequestBuilder` — `application/x-www-form-urlencoded` body |
| `multipart` | `fn multipart(self, form: &Multipart) -> RequestBuilder` — `multipart/form-data` body |
| `range` | `fn range(self, start: u64, end: Option<u64>) -> RequestBuilder` — `Range: bytes=…` |
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

You never have to set `Host`, `Content-Length`, `User-Agent` or `Connection` yourself — the
wire codec adds them if you didn't (see [Framing rules](#framing--content-length-rules)).

### Connection metadata on the server

Every request a server handler receives knows where it came from:

| Method | Signature | Description |
|---|---|---|
| `peer_addr` | `fn peer_addr(&self) -> Option<SocketAddr>` | The remote socket address. `Some` for every served request; `None` for a hand-built one. Behind a reverse proxy this is the proxy's address. |
| `local_addr` | `fn local_addr(&self) -> Option<SocketAddr>` | The local address the connection was accepted on. |
| `is_tls` | `fn is_tls(&self) -> bool` | Whether the request arrived over TLS (`serve_tls*`). For a client-side request it reflects the URL scheme. |
| `peer_certificates` | `fn peer_certificates(&self) -> &[Vec<u8>]` | The client's verified certificate chain (DER, leaf first) under [mutual TLS](#mutual-tls-and-minimum-version); empty otherwise. |
| `set_peer_addr` / `set_tls` | `fn set_peer_addr(&mut self, addr: SocketAddr)`, `fn set_tls(&mut self, tls: bool)` | For unit-testing middleware with a hand-built request. |

```rust
use ferroly::http::{BoxFuture, HttpHandler, HttpResponse, Request, StatusCode};

struct WhoAmI;

impl HttpHandler for WhoAmI {
    fn handle(&self, req: Request) -> BoxFuture<'_, HttpResponse> {
        Box::pin(async move {
            let peer = req.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
            HttpResponse::text(StatusCode::OK, format!("{peer} tls={}", req.is_tls()))
        })
    }
}
```

### Form and multipart bodies

```rust
use ferroly::http::{Method, Multipart, Request};

# fn ex() -> Result<(), ferroly::http::HttpError> {
// application/x-www-form-urlencoded
let login = Request::builder(Method::Post, "https://example.com/login")?
    .form([("user", "ada"), ("password", "p&ss word")])
    .build();

// multipart/form-data
let form = Multipart::new()
    .text("title", "Quarterly report")
    .file("report", "q3.csv", "text/csv", b"a,b\n1,2\n".to_vec());
let upload = Request::builder(Method::Post, "https://example.com/upload")?
    .multipart(&form)
    .build();
# let _ = (login, upload);
# Ok(())
# }
```

`Multipart` generates an unpredictable 128-bit boundary, escapes field and file names
(`"` → `%22`, CR → `%0D`, LF → `%0A`) and strips control characters from part content types,
so a field can never inject a part header. `boundary()`, `content_type()` and `encode()` are
available if you need the pieces.

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
| `uri` | `fn uri(&self) -> &Uri` | The URL the response came from (the final one after redirects). |
| `is_success` | `fn is_success(&self) -> bool` | `true` for 2xx. |
| `set_max_body_bytes` | `fn set_max_body_bytes(&mut self, max: Option<u64>)` | Overrides the client's body cap for this response. |
| `chunk` | `async fn chunk(&mut self) -> Result<Option<Vec<u8>>, HttpError>` | Next body piece, or `None` at end of body. |
| `bytes` | `async fn bytes(self) -> Result<Vec<u8>, HttpError>` | Drains the whole body into a `Vec<u8>`. |
| `text` | `async fn text(self) -> Result<String, HttpError>` | Drains the body as a UTF-8 (lossy) `String`. |

`chunk` borrows `&mut self` so you can loop; `bytes`/`text` consume `self` because they read to
end of body.

Reading the body can fail, and a failure is never reported as a short success:

- the connection closing before `Content-Length` bytes arrived, or a chunked body ending
  without its terminating chunk → `HttpError::Protocol`;
- no data within `read_timeout`, or the `total_timeout` passing → `HttpError::Timeout`;
- the body exceeding the size cap → `HttpError::BodyTooLarge`.

After an error the body cannot be read further (later calls return an error too) and the
connection is closed.

### Streaming a large download chunk-by-chunk

```rust
use ferroly::http::{Client, Method, Request};

# async fn ex() -> Result<(), ferroly::http::HttpError> {
let client = Client::new();
let req = Request::builder(Method::Get, "https://example.com/big-file")?.build();
let mut resp = client.send(req).await?;
resp.set_max_body_bytes(None); // streaming to disk: the in-memory cap is not needed

let mut total = 0usize;
while let Some(chunk) = resp.chunk().await? {
    total += chunk.len();
    // write chunk to disk, hash it, feed an SSE parser, etc.
}
println!("received {total} bytes");
# Ok(())
# }
```

Piece sizes are an implementation detail: each `chunk()` returns at most 16 KiB, and a
transfer chunk larger than that arrives as several pieces (the `chunked` framing and trailers
are stripped for you).

### Range requests and resumable downloads

`RequestBuilder::range(start, end)` sets a `Range: bytes=start-end` header (`end`
inclusive, or `None` for open-ended `start-`):

```rust
use ferroly::http::{Method, Request};
# fn ex() -> Result<(), ferroly::http::HttpError> {
let req = Request::builder(Method::Get, "https://example.com/big.bin")?
    .range(1_048_576, None)      // resume from 1 MiB
    .build();
# let _ = req; Ok(())
# }
```

For the common case, `download_to_file` streams a body straight to disk and
**resumes** automatically — if the destination already holds a partial file it
issues a `Range` from that offset and appends:

```rust
use ferroly::http::{Client, download_to_file};
# async fn ex() -> Result<(), ferroly::http::HttpError> {
let client = Client::new();
let total = download_to_file(&client, "https://example.com/big.iso", "big.iso").await?;
# let _ = total; Ok(())
# }
```

It validates the server's answer rather than trusting it:

| Response | Behaviour |
|---|---|
| **206** | `Content-Range` must be present and start at exactly the requested offset; then the body is appended. Otherwise an error, and the file is left untouched. |
| **200** (any other 2xx) | The server ignored `Range`: the file is truncated and rewritten from the start. |
| **416** | The file is treated as complete. If the server states the size (`Content-Range: bytes */N`) and it differs from the size on disk, that is an error. |
| other | `HttpError::Protocol`. |

A body that ends early is an error; the bytes that did arrive stay on disk, so calling the
function again resumes from there. When the server states the total size, the size on disk is
checked against it at the end. The client's in-memory body cap does not apply (the data goes
to disk); its timeouts do. Because it streams, you can verify integrity as it downloads by
hashing each chunk with [`ferroly::hash`](hash.md).

### Body framing is decided for you

The client picks a decoding strategy from the **request method**, the response status and the
headers:

- **a `HEAD` request, `204`, `304`, `1xx`, a successful `CONNECT`** — no body at all, whatever
  the headers say.
- **`Transfer-Encoding: chunked`** — chunked decoding, trailers consumed and discarded. Any
  other `Transfer-Encoding` is an error.
- **`Content-Length: N`** — exactly `N` bytes. An invalid or conflicting `Content-Length` is
  an error.
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
has an **empty scheme** and only the path + query (the `Host` header is copied into
`uri.host`), calls your handler, and writes the `HttpResponse`.

The request target is accepted in origin form (`/path?query`), in absolute form
(`http://host/path` — reduced to its path, with the target's authority taking precedence over
`Host`), as `*` for `OPTIONS`, and in authority form for `CONNECT`.

The server supports **HTTP/1.1 keep-alive**: successive requests are served on the same
connection unless the request or the response carries `Connection: close`, up to
`max_keep_alive_requests` per connection, closing a connection that stays idle for
`idle_timeout`. An HTTP/1.0 client is kept alive only if it asks with
`Connection: keep-alive`. Responses to `HEAD` and `1xx`/`204`/`304` correctly omit the message
body so framing stays in sync on a reused connection.

### Resource limits: `ServerConfig` / `serve_with_config`

`serve` applies a set of safe default limits so a single hostile client cannot exhaust the
process. To tune them, use `serve_with_config`. `ServerConfig` implements `Default` and gains
fields over time, so always build it with struct-update syntax:

```rust
use std::time::Duration;
use ferroly::http::ServerConfig;

let config = ServerConfig {
    max_body_bytes: 32 * 1024 * 1024,         // allow larger uploads
    write_timeout: Duration::from_secs(10),   // drop clients that stop reading sooner
    handler_timeout: Some(Duration::from_secs(30)),
    ..ServerConfig::default()                 // keep every other default
};
# let _ = config;
```

| Field | Default | Meaning |
|---|---|---|
| `max_body_bytes: usize` | 8 MiB | Largest request body (`Content-Length` or decoded chunked). Larger → `413`, before the body is read. |
| `max_connections: usize` | 1024 | Connections served concurrently; further ones wait (backpressure). |
| `head_timeout: Duration` | 30 s | Time to read the request line + headers once a request has started; also the time a **new** connection has to send its first byte, and the TLS handshake limit. Expiry mid-head → `408`. |
| `body_timeout: Duration` | 60 s | Time to read the request body. Expiry → `408`. |
| `idle_timeout: Duration` | 30 s | How long a kept-alive connection may sit idle between requests. |
| `write_timeout: Duration` | 30 s | Time for the client to accept each piece of the response (up to 64 KiB at a time). A client that stops reading is disconnected and its connection slot released. |
| `handler_timeout: Option<Duration>` | `None` | Deadline for the handler to return a response. Expiry → the handler future is dropped and the client gets `503`. Does not limit how long a streaming body keeps producing chunks. |
| `drain_timeout: Duration` | 30 s | On shutdown, how long to wait for in-flight requests. |
| `keep_alive: bool` | `true` | `false` answers every request with `Connection: close`. |
| `max_keep_alive_requests: usize` | 1000 | Requests per connection before it is closed. |
| `max_header_bytes: usize` | 64 KiB | Total size of the header section (and of a chunked body's trailer section). Larger → `431`. |
| `max_headers: usize` | 128 | Number of header lines (and of trailer lines). More → `431`. |
| `max_line_bytes: usize` | 16 KiB | Length of the request line and of any header or trailer line. Longer → `414` (request line) or `431` (header line). |
| `normalize_paths: bool` | `false` | Normalise the request path before the handler sees it; see below. |
| `on_error: Option<ErrorHook>` | `None` | Receives errors the server would otherwise discard; see [Errors and panics](#errors-and-panics-errorhook). |

The guarantees these provide:

- **Body cap** — a request whose `Content-Length` exceeds `max_body_bytes` is answered with
  `413 Payload Too Large` **before** the body is read, and the buffer for an accepted body
  grows only with the bytes that actually arrive. A chunked request body is dechunked and
  bounded by the same cap, checked against each declared chunk size before the chunk is read.
- **Head, body, idle and write timeouts** — a peer that trickles the request head or body, sits
  idle, or stops reading the response is dropped once the relevant deadline passes, closing
  the slow-loris holes in both directions.
- **Connection cap** — a permit is acquired *before* `accept()`, so the server never pulls more
  sockets off the queue than it can serve; excess connections wait instead of exhausting file
  descriptors and memory. Every timeout above ends in the permit being released.
- **Resilient accept loop** — a transient `accept()` failure (e.g. `EMFILE`/`ENFILE` fd
  exhaustion, `ECONNABORTED`) is reported to `on_error`, backs off briefly and continues
  rather than tearing down the whole listener.
- **Graceful drain** — when `shutdown` resolves the server stops accepting, then waits up to
  `drain_timeout` for in-flight requests to finish before returning, so a rolling deploy does
  not cut active responses.
- **Strict parsing** — see [Security hardening](#security-hardening).

**Path normalisation (`normalize_paths`).** With this on, the path of every request goes
through [`normalize_path`](#url-and-date-helpers) before the handler runs: percent-escapes of
unreserved characters are decoded, `.` and `..` segments removed, and a path with an empty
segment (`//`), a malformed escape or an encoded control character is rejected with `400`.
Routing, authorisation and handlers then all see one canonical path. It is off by default so
existing handlers keep receiving the path exactly as sent; the query string is never touched.

### Errors and panics: `ErrorHook`

A handler that **panics** does not take the server — or even the connection — down. The panic
is caught, the client receives `500 Internal Server Error` (without the panic message), and
the connection keeps serving. The same goes for a handler that exceeds `handler_timeout`
(`503`).

Everything the server would otherwise have to swallow is offered to
`ServerConfig::on_error`:

```rust
use ferroly::http::{ErrorHook, HttpError, ServerConfig};

let config = ServerConfig {
    on_error: Some(ErrorHook::new(|peer, err| match err {
        HttpError::HandlerPanic(msg) => eprintln!("handler panic from {peer:?}: {msg}"),
        other => eprintln!("http connection error from {peer:?}: {other}"),
    })),
    ..ServerConfig::default()
};
# let _ = config;
```

The callback receives the peer address (`None` for a failed `accept`) and an `&HttpError`:

| Situation | Error |
|---|---|
| Handler panicked | `HandlerPanic(message)` |
| Handler exceeded `handler_timeout`; head, body or write timeout; TLS handshake timeout | `Timeout` |
| Malformed request, refused framing (smuggling attempt), over-limit head — the client is answered `400`/`414`/`417`/`431`/`501`/`505` | `Protocol(reason)` |
| Request body over `max_body_bytes` (`413`) | `BodyTooLarge(limit)` |
| TLS handshake failure | `Tls(reason)` |
| Socket errors, failed `accept` | `Io(error)` |

A clean close and an idle timeout are not errors and are not reported. The hook runs on the
connection task, so keep it quick; a panic inside the hook is contained too. The `http` module
does not depend on `ferroly::log` — wire the hook to whatever logger you use.

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
apply.

#### Mutual TLS and minimum version

`TlsConfig::builder()` adds client-certificate verification and a protocol floor:

```rust
use ferroly::http::{TlsConfig, TlsVersion};

# fn ex(cert: &[u8], key: &[u8], client_ca: &[u8]) -> Result<(), ferroly::http::HttpError> {
let tls = TlsConfig::builder()
    .cert_pem(cert, key)?
    .client_auth_required_pem(client_ca)?   // every client must present a certificate
    .min_version(TlsVersion::Tls13)
    .build()?;
# let _ = tls;
# Ok(())
# }
```

| Builder method | Description |
|---|---|
| `cert_pem(cert, key)?` / `cert_der(chain, key)` | The server certificate chain and key. |
| `client_auth_required_pem(ca)?` / `client_auth_required_der(cas)` | **Mutual TLS, required**: a client without a certificate that chains to one of these CAs fails the handshake. |
| `client_auth_optional_pem(ca)?` / `client_auth_optional_der(cas)` | **Mutual TLS, optional**: a certificate is requested; one that is presented must verify, but anonymous clients are served. |
| `min_version(TlsVersion)` | `TlsVersion::Tls12` (default) or `TlsVersion::Tls13`. |
| `build()?` | Validates everything (including that the key matches the certificate). |

In a handler, `Request::peer_certificates()` returns the client's verified chain (DER, leaf
first) — empty for an anonymous client in optional mode — and `Request::is_tls()` is `true`.

#### Certificate reload

To rotate a certificate without restarting, serve from a `ReloadableTlsConfig` and swap the
configuration whenever you like:

```rust
use std::sync::Arc;
use ferroly::http::{serve_tls_reloadable, HttpHandler, ServerConfig, TlsConfig};

# async fn ex(listener: tokio::net::TcpListener, handler: Arc<dyn HttpHandler>) -> Result<(), ferroly::http::HttpError> {
let load = || -> Result<TlsConfig, ferroly::http::HttpError> {
    TlsConfig::from_pem(&std::fs::read("server.crt")?, &std::fs::read("server.key")?)
};
let tls = load()?.reloadable();

let handle = tls.clone();
tokio::spawn(async move {
    // e.g. on SIGHUP, on a timer, or when the files change:
    if let Ok(next) = load() {
        handle.reload(next);
    }
});

serve_tls_reloadable(listener, handler, std::future::pending::<()>(), ServerConfig::default(), tls).await
# }
```

`reload` replaces the whole TLS configuration — certificate, key, client-auth mode and
minimum version. Every connection accepted after the call uses the new one; connections that
are already established are unaffected.

### Graceful shutdown

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
    // plus a private streaming body, set via `stream` / `event_stream` / `sse`
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
| `event_stream` | `fn event_stream(events: mpsc::Receiver<String>) -> HttpResponse` | Server-Sent Events body, one event per string. |
| `sse` | `fn sse(events: mpsc::Receiver<sse::Event>) -> HttpResponse` | Server-Sent Events body with structured [`Event`](#structured-server-sent-events-sse--httpsse)s. |

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

The server owns the framing: it always writes its own `Content-Length` (or
`Transfer-Encoding`) for the body it actually sends, and ignores one you set — except that a
response to a `HEAD` request keeps your `Content-Length`, so you can advertise the size of the
entity. It adds a `Date` header unless you set one, and it honours a `Connection: close` you
set by closing the connection after the response.

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
`0\r\n\r\n`. Waiting for your next chunk is not timed (a stream may be quiet for a long
while); delivering each chunk to the client is bounded by `write_timeout`. For an HTTP/1.0
client, which cannot receive chunked encoding, the stream is sent as is and ended by closing
the connection.

`event_stream` is a Server-Sent Events convenience built on top of `stream`. Give it a
`Receiver<String>`; each string becomes one event, and the response is pre-populated with
`content-type: text/event-stream` and `cache-control: no-cache`:

```rust
use ferroly::http::HttpResponse;
use tokio::sync::mpsc;

let (tx, rx) = mpsc::channel::<String>(16);
tokio::spawn(async move {
    let _ = tx.send("hello".to_string()).await;      // -> "data: hello\n\n"
    let _ = tx.send("two\nlines".to_string()).await; // -> "data: two\ndata: lines\n\n"
});
let resp = HttpResponse::event_stream(rx);
# let _ = resp;
```

Each string goes through the same frame encoder as `sse` (below): a line break inside it —
`\r\n`, `\r` or `\n` — becomes another `data:` line of the *same* event, so text you relay can
never forge a field or a second event. This pairs naturally with an LLM token stream from
[`ferroly::genai`](genai.md).

### Structured Server-Sent Events: `sse` + `http::sse`

When you need more than a bare `data:` line — an `event:` type, an `id:` for
resumption, a `retry:` hint, or multi-line data — use the structured `Event`
type from `ferroly::http::sse` and `HttpResponse::sse`:

```rust
use ferroly::http::{HttpResponse, sse::Event};
use tokio::sync::mpsc;

let (tx, rx) = mpsc::channel::<Event>(16);
tokio::spawn(async move {
    let _ = tx.send(Event::new("first line\nsecond line").event("update").id("1")).await;
    let _ = tx.send(Event::keep_alive("ping")).await;   // `: ping` comment
});
let resp = HttpResponse::sse(rx);
# let _ = resp;
```

`Event::to_frame()` encodes the event to its wire frame, and no field value can break out of
it:

- `data` and `comment` are split at every `\r\n`, `\r` and `\n`, each line written as its own
  `data:` / `:` line;
- `event` and `id` are single-line fields, so every `\r` and `\n` is removed from them (and
  NUL from `id`, which the format forbids);
- the frame ends with the blank line that dispatches it.

On the **client** side, decode an incoming `text/event-stream` body incrementally
with `http::sse::SseDecoder`, feeding it the chunks from
[`Response::chunk`](#response-streaming-body). It buffers across chunk boundaries
and returns each `Event` once its blank line arrives:

```rust
use ferroly::http::sse::SseDecoder;

let mut dec = SseDecoder::new();
let events = dec.push(b"event: update\ndata: hello\n\n");
assert_eq!(events[0].data, "hello");
assert_eq!(events[0].event.as_deref(), Some("update"));
```

The decoder ends a line at `\r\n`, at a lone `\r` and at a lone `\n` (including a `\r\n` split
across two pushes), skips a leading byte-order mark, ignores an `id` containing NUL and a
non-numeric `retry`, and bounds its memory: a line longer than 1 MiB is discarded, and so is
an event whose data grows past that (`SseDecoder::with_max_event_bytes` changes the limit).

| Item | Kind |
|---|---|
| `Event { data, event, id, retry, comment }` | builder (`new`, `keep_alive`, `.event`, `.id`, `.retry`, `.comment`) + `to_frame()` |
| `SseDecoder` | incremental client parser (`new`, `with_max_event_bytes`, `push(&[u8]) -> Vec<Event>`) |
| `HttpResponse::sse(Receiver<Event>)` | server response streaming structured events |

---

## Core value types

### Method

```rust
pub enum Method { Get, Post, Put, Delete, Patch, Head, Options, Trace, Connect, Other(String) }
```

The nine named HTTP methods plus `Other(String)` for extensions.

| Method | Description |
|---|---|
| `as_str(&self) -> &str` | The canonical uppercase token (`"GET"`, `"POST"`, …). |
| `parse(s: &str) -> Method` | Infallible; unknown tokens become `Other(s.to_string())`. |
| `try_parse(s: &str) -> Result<Method, HttpError>` | Rejects anything that is not an RFC 9110 `token` of at most 32 bytes — use this for untrusted input. |
| `is_valid(&self) -> bool` | Whether the method is a valid token (always `true` for the named variants). |
| `is_idempotent(&self) -> bool` | `GET`, `HEAD`, `PUT`, `DELETE`, `OPTIONS`, `TRACE`. |
| `is_safe(&self) -> bool` | `GET`, `HEAD`, `OPTIONS`, `TRACE`. |

The client refuses to send a request whose method is not a valid token, so a
`Method::Other` built from untrusted text cannot inject a second request. The server only ever
produces valid methods.

`Method` derives `Clone, PartialEq, Eq`, so you can match on it and compare directly.

### StatusCode

```rust
pub struct StatusCode(pub u16);
```

A transparent `u16` newtype (so `StatusCode(418)` is fine for anything not in the constant
list). Provided constants:

| Const | Code | | Const | Code |
|---|---|---|---|---|
| `CONTINUE` | 100 | | `NOT_FOUND` | 404 |
| `SWITCHING_PROTOCOLS` | 101 | | `METHOD_NOT_ALLOWED` | 405 |
| `OK` | 200 | | `NOT_ACCEPTABLE` | 406 |
| `CREATED` | 201 | | `REQUEST_TIMEOUT` | 408 |
| `NO_CONTENT` | 204 | | `PAYLOAD_TOO_LARGE` | 413 |
| `PARTIAL_CONTENT` | 206 | | `URI_TOO_LONG` | 414 |
| `MOVED_PERMANENTLY` | 301 | | `RANGE_NOT_SATISFIABLE` | 416 |
| `FOUND` | 302 | | `EXPECTATION_FAILED` | 417 |
| `SEE_OTHER` | 303 | | `TOO_MANY_REQUESTS` | 429 |
| `NOT_MODIFIED` | 304 | | `REQUEST_HEADER_FIELDS_TOO_LARGE` | 431 |
| `TEMPORARY_REDIRECT` | 307 | | `INTERNAL_SERVER_ERROR` | 500 |
| `PERMANENT_REDIRECT` | 308 | | `NOT_IMPLEMENTED` | 501 |
| `BAD_REQUEST` | 400 | | `SERVICE_UNAVAILABLE` | 503 |
| `UNAUTHORIZED` | 401 | | `GATEWAY_TIMEOUT` | 504 |
| `FORBIDDEN` | 403 | | `HTTP_VERSION_NOT_SUPPORTED` | 505 |

Methods:

- `as_u16(self) -> u16` — the numeric code.
- `is_success(self) -> bool` — `true` for `200..300`.
- `is_informational(self) -> bool` — `true` for `100..200`.
- `is_redirection(self) -> bool` — `true` for `301`, `302`, `303`, `307`, `308`.
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
| `set` | `fn set(&mut self, name: impl Into<String>, value: impl Into<String>)` | Replaces any existing same-named values. Sanitises (see below). |
| `append` | `fn append(&mut self, name: impl Into<String>, value: impl Into<String>)` | Adds without removing duplicates. Sanitises. |
| `try_set` / `try_append` | `fn try_set(&mut self, name, value) -> Result<(), HttpError>` | Like `set`/`append`, but **reject** an invalid name or value instead of repairing it. |
| `get` | `fn get(&self, name: &str) -> Option<&str>` | First value for a name. |
| `get_all` | `fn get_all(&self, name: &str) -> impl Iterator<Item = &str>` | Every value for a name, in order. |
| `remove` | `fn remove(&mut self, name: &str) -> bool` | Removes every value for a name. |
| `contains` | `fn contains(&self, name: &str) -> bool` | Presence check. |
| `len` / `is_empty` | | Number of header lines. |
| `content_length` | `fn content_length(&self) -> Option<u64>` | The `Content-Length` if present, valid and unambiguous. |
| `content_length_checked` | `fn content_length_checked(&self) -> Result<Option<u64>, HttpError>` | `Ok(None)` when absent; `Err` when invalid or when several values disagree. |
| `is_chunked` | `fn is_chunked(&self) -> bool` | `true` only when the `Transfer-Encoding`, taken across **all** its header lines, is exactly one `chunked` token. |
| `iter` | `fn iter(&self) -> impl Iterator<Item = (&str, &str)>` | Pairs in insertion order. |

**Sanitising.** `set` and `append` make it impossible for a header to split a message or
inject another header: every byte that is not an RFC 9110 `tchar` is removed from the **name**
(so no CR/LF, colon, space or control character), and CR, LF, NUL and the other control
characters except tab are removed from the **value**. A name that ends up empty is ignored.

**Strict `Content-Length`.** Only a run of ASCII digits is a length. `+5`, `-5`, `5, 5`, `0x5`
and `5.0` are invalid.

```rust
use ferroly::http::HeaderMap;

let mut h = HeaderMap::new();
h.set("Content-Type", "application/json");
assert_eq!(h.get("content-type"), Some("application/json")); // case-insensitive
h.append("Set-Cookie", "a=1");
h.append("Set-Cookie", "b=2");                                // both kept
assert_eq!(h.get_all("set-cookie").count(), 2);

h.set("X-Note", "one\r\nInjected: two");                      // CR/LF stripped
assert_eq!(h.get("x-note"), Some("oneInjected: two"));
assert!(h.try_set("X-Note", "one\r\ntwo").is_err());           // or refuse outright
```

### Uri

```rust
pub struct Uri {
    pub scheme: String,        // "http" | "https" (empty for a bare request target)
    pub host: String,          // IPv6 literals without brackets: "::1"
    pub port: u16,             // defaulted from the scheme
    pub path: String,          // always begins with '/'
    pub query: Option<String>, // without the leading '?'
}
```

A parsed absolute URL (client side) or request target (server side). Construct one with
`Uri::parse`:

| Method | Signature | Description |
|---|---|---|
| `parse` | `fn parse(url: &str) -> Result<Uri, HttpError>` | Parses and validates an absolute `http(s)`/`ws(s)` URL. |
| `is_tls` | `fn is_tls(&self) -> bool` | `true` for `https`/`wss`. |
| `authority` | `fn authority(&self) -> String` | The `Host` header value — `host`, or `host:port` for non-default ports; an IPv6 literal gets its brackets back (`[::1]:8080`). |
| `request_target` | `fn request_target(&self) -> String` | Origin-form target `/path?query`. |
| `absolute` | `fn absolute(&self) -> String` | `scheme://authority/path?query` (also `Display`). |
| `same_origin` | `fn same_origin(&self, other: &Uri) -> bool` | Same scheme, host and port. |
| `resolve` | `fn resolve(&self, reference: &str) -> Result<Uri, HttpError>` | Resolves a relative reference (e.g. a `Location`) per RFC 3986 §5, removing `.`/`..` segments. |

`parse` accepts `http`/`ws` (default port 80) and `https`/`wss` (default port 443); any other
scheme is `HttpError::InvalidUrl`. It is strict about anything that could change the request
on the wire:

- a **control character** (CR, LF, NUL, tab, DEL, …) or a **space** anywhere → error;
- the **`#fragment`** is dropped — it is never sent to a server;
- **`user:password@` userinfo** → error (send credentials in a header instead);
- the **host** must be a name or IPv4 address made of `A-Z a-z 0-9 - . _ ~`, or an **IPv6
  literal in brackets** (`http://[::1]:8080/`); `host` holds it without brackets, ready to
  connect to;
- the **port**, if present, must be 1–5 digits in `1..=65535`;
- **non-ASCII** characters in the path or query are percent-encoded as UTF-8.

Because the fields are public, a `Uri` can also be built or modified by hand. The client
therefore re-validates the request target and host before every send and returns
`HttpError::InvalidUrl` rather than writing a path or query that contains a space, a control
character, a non-ASCII byte or `#`.

There is also a free helper:

```rust
pub fn split_target(target: &str) -> (String, Option<String>);
```

which splits a request target into `(path, query)`, defaulting an empty path to `/`.

```rust
use ferroly::http::Uri;

let u = Uri::parse("https://api.example.com/v1/chat?x=1#section").unwrap();
assert_eq!(u.scheme, "https");
assert_eq!(u.host, "api.example.com");
assert_eq!(u.port, 443);
assert_eq!(u.path, "/v1/chat");
assert_eq!(u.query.as_deref(), Some("x=1"));
assert!(u.is_tls());
assert_eq!(u.authority(), "api.example.com");         // default port omitted
assert_eq!(u.request_target(), "/v1/chat?x=1");       // no fragment

let v6 = Uri::parse("http://[::1]:8080/").unwrap();
assert_eq!(v6.host, "::1");                           // brackets stripped for connecting
assert_eq!(v6.authority(), "[::1]:8080");             // and restored for Host

assert!(Uri::parse("http://example.com/a b").is_err());
assert!(Uri::parse("http://example.com/x\r\nHost: evil").is_err());
assert_eq!(u.resolve("../models?y=2").unwrap().absolute(), "https://api.example.com/models?y=2");
```

### URL and date helpers

| Function | Signature | Description |
|---|---|---|
| `percent_encode` | `fn percent_encode(input: &str) -> String` | Encodes a value for use as **one URL component** (path segment, query key or value): everything except `A-Z a-z 0-9 - . _ ~` becomes `%XX`, so `/`, `?`, `#`, `%`, spaces and CR/LF cannot change the URL's structure. |
| `percent_decode` | `fn percent_decode(input: &str) -> Result<Vec<u8>, HttpError>` | Decodes `%XX`; a malformed escape is an error. `+` is left alone. |
| `normalize_path` | `fn normalize_path(path: &str) -> Result<String, HttpError>` | Canonicalises a request path: decodes escapes of unreserved characters, upper-cases the rest, removes `.`/`..` segments, and rejects a path that does not start with `/`, has an empty segment (`//`), a malformed escape or an encoded control character. |
| `form_urlencode` | `fn form_urlencode(pairs) -> String` | `application/x-www-form-urlencoded` (`a=1&b=x+y`). |
| `format_http_date` | `fn format_http_date(time: SystemTime) -> String` | IMF-fixdate, e.g. `Sun, 06 Nov 1994 08:49:37 GMT`, for `Date` / `Last-Modified` / `Expires`. |

```rust
use ferroly::http::{normalize_path, percent_encode};

// Building a URL from untrusted pieces: encode each piece.
let id = "../admin?x=1";
let url = format!("https://api.example.com/items/{}", percent_encode(id));
assert_eq!(url, "https://api.example.com/items/..%2Fadmin%3Fx%3D1");

// Deciding what a request path really points at: normalise first.
assert_eq!(normalize_path("/api/%2e%2e/admin").unwrap(), "/admin");
assert!(normalize_path("//api/secret").is_err());
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
- **It is swappable.** Because the boundary is `Box<dyn Io>` and all TLS settings go through
  ferroly's own types (`TlsConfig`, `TlsConfigBuilder`, `ReloadableTlsConfig`,
  `ClientTlsConfig`, `TlsVersion`), replacing `rustls` touches only `transport.rs` — no public
  API changes, and no other module recompiles against a new crypto crate.
- **The crypto provider is named explicitly.** Every TLS configuration is built with `ring`
  named directly rather than relying on the process-default provider. If some other
  dependency's feature unification drags a second provider (`aws-lc-rs`) into the build, the
  ambiguous default would otherwise panic at runtime; naming `ring` keeps TLS setup
  deterministic. The default client roots come from `webpki-roots`, built once per process.
- **Verification cannot be disabled.** Trust can be extended or replaced
  ([Client TLS](#client-tls-custom-roots-client-certificates)), never switched off.

The client `Client::send` path is: connect TCP → for `https`, wrap the stream in a
`tokio_rustls` connector using the host as the SNI server name (through a `CONNECT` tunnel
first when a proxy is configured); for `http`, return the bare TCP stream — either way a
`Conn`. Server-side TLS uses the same seam: [`serve_tls`](#https-serve_tls--tlsconfig) wraps
each accepted `TcpStream` in a `tokio_rustls` acceptor built from your `TlsConfig`, producing a
`Conn` the accept loop serves exactly like a plaintext one. The plain `serve` accept loop
stays cleartext.

---

## Framing & Content-Length rules

The wire codec (`http::io`) applies these rules so you rarely set framing headers by hand.

**Writing a request** (`Client`): adds `Host` (from `uri.authority()`), `User-Agent` (the
client default) and `Content-Length` — each only if you didn't already provide it.
`Content-Length` is added for a non-empty body, and as `0` for a body-less `POST`, `PUT` or
`PATCH`. A streamed upload is sent chunked unless you declared a length. It does **not** force
`Connection: close` (HTTP/1.1 defaults to keep-alive so the pool can reuse the connection);
set `Connection: close` yourself to opt out.

The client refuses (`HttpError::Protocol` / `InvalidUrl`, before anything is sent) to write a
request that could be misread: a non-token method, an invalid target or host, a
`Content-Length` that disagrees with the body, both `Content-Length` and `Transfer-Encoding`,
or a `Transfer-Encoding` other than `chunked`. (Setting `Transfer-Encoding: chunked` on a
buffered request makes the client chunk-frame the body for you.)

**Writing a response** (server): writes the status line using `StatusCode::reason()`, then your
headers (minus the framing ones, which it owns), a `Date` header, then:

- if the response is **streaming** → adds `Transfer-Encoding: chunked`, writes each chunk as
  `<hex-len>\r\n<bytes>\r\n` and flushes, then a terminating `0\r\n\r\n`;
- otherwise → adds `Content-Length: <body.len()>` and writes the body;
- for `HEAD` responses the body is omitted (a `Content-Length` you set is kept, otherwise the
  body length is advertised); `1xx`/`204`/`304` omit both;
- adds `Connection: keep-alive` (or `Connection: close` when closing) unless you set one.

**Reading a response body** (client) follows the [framing rules](#body-framing-is-decided-for-you)
above (`HEAD`/`204`/`304`/`1xx` → empty; `chunked` → `Content-Length` → EOF).

Both sides speak **HTTP/1.1 keep-alive** by default (persistent connections, one request at a
time per connection). The client does not pipeline; the server answers pipelined requests in
order.

---

## Security hardening

The stack is written to be exposed to untrusted peers. This section lists what is enforced;
the tests in `crates/ferroly/tests/http_server_strict.rs`, `http_client_strict.rs`,
`http_download_strict.rs` and `http_tls.rs` exercise each rule over raw TCP.

### Strict request parsing (server)

A request that breaks any of these rules is answered with the status shown and the connection
is **closed** — its framing cannot be trusted, so nothing after it is read as a request. The
handler is never called.

| Rule | Status |
|---|---|
| The request line is exactly `method SP target SP HTTP/1.x` — single spaces, nothing extra. | `400` |
| The method is an RFC 9110 token of at most 32 bytes. | `400` |
| The version is `HTTP/1.1` or `HTTP/1.0`. Another well-formed version (`HTTP/2.0`) → `505`; a missing or malformed one → `400`. | `505` / `400` |
| The target is origin-form (`/…`), absolute-form (`http://…`), `*` (with `OPTIONS`) or authority-form (with `CONNECT`), and contains no control character, space, non-ASCII byte or `#`. | `400` |
| Every line ends in **CRLF**. A bare LF is not accepted as a line ending anywhere in a request (RFC 9112 lets a recipient choose; rejecting is the unambiguous choice), and a CR that is not part of a CRLF is an error. Up to two empty lines before the request line are skipped. | `400` |
| A header line has a colon, and its name is a non-empty token — so **whitespace before the colon** is an error. | `400` |
| **No obs-fold**: a header line may not start with a space or tab. | `400` |
| A header value has no control character other than tab (no NUL, no bare CR). | `400` |
| An HTTP/1.1 request has exactly one `Host` header (an HTTP/1.0 request at most one). | `400` |
| `Content-Length` is a run of ASCII digits — not signed, not a list, not hex. Several `Content-Length` headers must agree. | `400` |
| `Transfer-Encoding`, taken across **all** its header lines, is exactly one `chunked` token. `gzip, chunked`, `chunked, chunked`, `xchunked`, `identity`, and `gzip` on one line with `chunked` on the next are all refused. | `501` |
| `Content-Length` and `Transfer-Encoding` are never both present (CL.TE / TE.CL). | `400` |
| An HTTP/1.0 request has no `Transfer-Encoding`. | `400` |
| A chunk size is 1–16 hex digits — no sign, no `0x`, no surrounding whitespace (whitespace is allowed only before a `;extension`). | `400` |
| Chunk data is followed by exactly CRLF. | `400` |
| Trailers follow the same syntax rules as headers. | `400` |
| `Expect` is `100-continue` or absent. | `417` |

`100 Continue` is sent only after the framing checks **and** the size check have passed, so an
oversized upload is refused without being invited.

### Limits and timeouts (server)

All are fields of [`ServerConfig`](#resource-limits-serverconfig--serve_with_config).

| Limit | Default | On violation |
|---|---|---|
| Request line length (`max_line_bytes`) | 16 KiB | `414` |
| Header line length (`max_line_bytes`) | 16 KiB | `431` |
| Header count (`max_headers`) | 128 | `431` |
| Header section size (`max_header_bytes`) | 64 KiB | `431` |
| Body size (`max_body_bytes`) | 8 MiB | `413`, before the body is read |
| Chunk-size line length (size + extensions) | 1 KiB, or `max_line_bytes` if smaller | `400` |
| Trailer line length / count / total size | as for headers | `400` |
| First byte on a new connection, and the request head (`head_timeout`) | 30 s | close / `408` |
| TLS handshake (`head_timeout`) | 30 s | close |
| Request body (`body_timeout`) | 60 s | `408` |
| Idle time between requests (`idle_timeout`) | 30 s | close |
| Each response write of up to 64 KiB (`write_timeout`) | 30 s | close |
| Handler run time (`handler_timeout`) | none | `503` |
| Requests per connection (`max_keep_alive_requests`) | 1000 | `Connection: close` |
| Concurrent connections (`max_connections`) | 1024 | further connections wait |
| Shutdown drain (`drain_timeout`) | 30 s | return |

Other protections: a handler panic is contained (`500`); a handler cannot desynchronise a
response by setting its own `Content-Length` or `Transfer-Encoding`; when the server closes
after an error response it first drains unread request bytes for a short, bounded time so the
response is not lost to a TCP reset.

### Strict response parsing and limits (client)

| Rule / limit | Default | On violation |
|---|---|---|
| Status line is `HTTP/1.x SP 3 digits`. | | `Protocol` |
| Header syntax as on the server (no whitespace before the colon, no obs-fold, no control characters). A bare LF line ending **is** tolerated in a response. | | `Protocol` |
| Status/header line length, header count, header section size | 16 KiB / 128 / 64 KiB | `Protocol` |
| `Content-Length` strict and consistent; `Transfer-Encoding` exactly `chunked` | | `Protocol` |
| Chunk size, chunk-size line (1 KiB), chunk terminator and trailers as on the server | | `Protocol` |
| Interim `1xx` responses are skipped (at most 16) | | `Protocol` |
| A body shorter than its declared length, or an unterminated chunked body | | `Protocol` |
| Response body size (`max_response_body_bytes`), before and after decompression | 64 MiB | `BodyTooLarge` |
| Connect (`connect_timeout`) | 30 s | `ConnectTimeout` |
| Response head (`timeout`) | 60 s | `Timeout` |
| Idle time between body reads (`read_timeout`) | 300 s | `Timeout` |
| Whole exchange (`total_timeout`) | none | `Timeout` |
| Idle pooled connection lifetime (`pool_idle_timeout`) / per host (`pool_max_idle_per_host`) | 90 s / 32 | connection closed |

A response carrying both `Transfer-Encoding: chunked` and `Content-Length` is read as chunked
and its connection is not reused.

### What the client refuses to send

- A URL with a control character, a space, CR/LF or userinfo — rejected by `Uri::parse`. A
  `#fragment` is dropped and never sent.
- A request whose (hand-modified) path, query or host contains a space, control character,
  non-ASCII byte or `#`; a method that is not a token.
- Header names are reduced to token characters and header values lose CR, LF, NUL and other
  control characters, so neither can add a line.
- A `Content-Length` that does not match the body, or conflicting framing headers.
- A non-idempotent request a second time (see
  [Retry](#retry-on-a-dead-pooled-connection)).
- `Authorization`, `Cookie` or `Proxy-Authorization` to a different origin on a redirect;
  `Proxy-Authorization` to anything but the proxy.

### Other notes

- **Server-Sent Events** cannot be forged through event text; see
  [`event_stream`](#streaming-response-bodies-stream--event_stream).
- **TLS verification is always on**, on both sides; see
  [Client TLS](#client-tls-custom-roots-client-certificates) and
  [Mutual TLS](#mutual-tls-and-minimum-version).
- **Request paths are passed through as sent** unless `ServerConfig::normalize_paths` is on.
  A handler or router that makes decisions on the path should normalise it (with that flag or
  [`normalize_path`](#url-and-date-helpers)) *before* authorisation and routing, so both see
  the same path.
- **`peer_addr` is the directly connected peer.** Behind a reverse proxy it is the proxy;
  `X-Forwarded-For` is ordinary, client-controlled header data and is not interpreted here.

---

## Error handling

Every fallible operation returns `Result<_, HttpError>`:

```rust
#[non_exhaustive]
pub enum HttpError {
    InvalidUrl(String),    // "invalid url: {0}" — bad URL, or an unsafe request target
    Io(std::io::Error),    // "io error: {0}"   — connect/read/write failure (From<io::Error>)
    Tls(String),           // "tls error: {0}"  — TLS handshake / configuration / bad SNI name
    Protocol(String),      // "malformed http: {0}" — malformed message from the peer, or a
                           //   request the client refuses to send
    Timeout,               // "timed out" — response-head, read or total timeout (client);
                           //   head, body, write or handler timeout (server, via ErrorHook)
    ConnectTimeout,        // "connect timed out" — connect + TLS handshake + proxy CONNECT
    BodyTooLarge(u64),     // "body exceeds the configured limit of {0} bytes"
    TooManyRedirects,      // "too many redirects"
    Proxy(String),         // "proxy error: {0}" — CONNECT refused, bad proxy configuration
    HandlerPanic(String),  // "handler panicked: {0}" — reported to ErrorHook only
}
```

- `InvalidUrl` surfaces at `Request::builder` / `Uri::parse` time (unsupported scheme, missing
  host, bad port, control characters, userinfo), and from `Client::send` when a hand-built
  request target is unsafe or a redirect `Location` is invalid.
- `Io` wraps any `std::io::Error` via `#[from]`, so `?` works against raw socket errors.
- `Tls` covers a handshake failure (unknown issuer, wrong host name, version mismatch, a
  missing client certificate) and invalid TLS options.
- `Protocol` means a malformed message — or one the client will not send.
- `Timeout` and `ConnectTimeout` are distinct so a caller can tell "nothing was sent"
  (`ConnectTimeout`: safe to retry any method) from "the request may have been processed".
  `HttpError::is_timeout()` is `true` for both.
- The enum is `#[non_exhaustive]`: always keep a wildcard arm.

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
    Err(HttpError::ConnectTimeout) => eprintln!("could not connect in time"),
    Err(e) if e.is_timeout() => eprintln!("request timed out"),
    Err(HttpError::Tls(e)) => eprintln!("TLS problem: {e}"),
    Err(HttpError::BodyTooLarge(limit)) => eprintln!("response larger than {limit} bytes"),
    Err(e) => eprintln!("failed: {e}"),
}
# }
```

---

## Limitations

- **HTTP/1.1 only.** No HTTP/2 or HTTP/3, and no ALPN negotiation.
- **Keep-alive, but no client pipelining.** A client connection carries one request/response
  at a time.
- **Request bodies are buffered on the server.** A handler receives the whole body as a
  `Vec<u8>` (bounded by `max_body_bytes`); there is no streaming request-body API yet.
  Response bodies can be streamed in both directions, and the client can stream an upload.
- **No cookie jar, and no automatic retry beyond the dead-connection case.** Those belong to
  the higher layers — see [`ferroly::rest`](rest.md) / [`ferroly::clients`](clients.md).
- **Decompression covers `gzip` and `deflate` only** (opt-in), and only for responses; the
  server does not compress.
- **One certificate per listener.** No SNI-based certificate selection; use
  [certificate reload](#certificate-reload) to rotate.
- **No PROXY-protocol or forwarded-header handling**, no static-file or `Range` responses on
  the server — build those on `HttpHandler` or use [`ferroly::turbo`](turbo.md).
- **`NO_PROXY` has no CIDR matching.**
- **Bodies buffered where noted.** `bytes`/`text` buffer the whole body (up to the cap); use
  `chunk` for large or open-ended responses.

---

## See also

- [`ferroly::rest`](rest.md) — codec-aware client + server framework built on this stack.
- [`ferroly::clients`](clients.md) — higher-level HTTP client ergonomics (auth, retry).
- [`ferroly::turbo`](turbo.md) — the first-class router that produces an `HttpHandler`.
- [`ferroly::codec`](codec.md) — `Encode`/`Decode` traits for typed request/response bodies.
- [`ferroly::genai`](genai.md) — LLM client that streams tokens (pairs with `event_stream`).
- [`ferroly::ws`](ws.md) — WebSocket support layered over the same transport.
- [`ferroly::derive`](derive.md) — the `FerrolyError` derive behind `HttpError`.

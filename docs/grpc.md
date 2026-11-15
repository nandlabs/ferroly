# `ferroly::grpc` — gRPC servers and clients

**Feature:** `grpc` (implies `http2`, `protobuf` and `hash`; part of `full`).

gRPC over ferroly's own [HTTP/2](http.md#http2) stack, with [`proto`](proto.md)
messages — no `tonic`, no `hyper`, no `prost`, no `protoc`. It adds no dependency.

- [`GrpcServer`](#the-server) routes calls by path (`/package.Service/Method`) to your
  handlers. It is an `HttpHandler`, so the existing `http` server serves it over h2c
  (prior knowledge) or TLS with ALPN `h2`.
- [`Channel`](#the-client) makes calls over the `http` client's multiplexed HTTP/2
  connections.
- [`GrpcGenerator`](#code-generation) turns each `.proto` `service` into a server trait,
  a server adapter and a typed client, from your `build.rs`.
- All four call shapes — unary, server streaming, client streaming, bidirectional — with
  [status](#status-and-errors), [metadata](#metadata), [deadlines](#deadlines) and
  [cancellation](#cancellation) on both sides, and the standard
  [health service](#the-health-service).

Tested against grpc-go in both directions (see [Interoperability](#interoperability)).

## Contents

- [Quick start](#quick-start)
- [Call shapes](#call-shapes)
- [Streams: `Streaming` and `StreamSender`](#streams-streaming-and-streamsender)
- [Status and errors](#status-and-errors)
- [Metadata](#metadata)
- [Deadlines](#deadlines)
- [Cancellation](#cancellation)
- [Interceptors](#interceptors)
- [The server](#the-server)
- [The client](#the-client)
- [Transparent retry](#transparent-retry)
- [Compression](#compression)
- [Code generation](#code-generation)
- [The health service](#the-health-service)
- [Limits and untrusted input](#limits-and-untrusted-input)
- [Interoperability](#interoperability)
- [How it is tested](#how-it-is-tested)
- [Not supported](#not-supported)
- [See also](#see-also)

## Quick start

A `.proto` file:

```proto
// proto/echo/v1/echo.proto
syntax = "proto3";
package echo.v1;

message EchoRequest { string message = 1; int32 repeat = 2; }
message EchoResponse { string message = 1; int32 index = 2; }

service Echo {
  rpc Unary(EchoRequest) returns (EchoResponse);
  rpc ServerStream(EchoRequest) returns (stream EchoResponse);
  rpc ClientStream(stream EchoRequest) returns (EchoResponse);
  rpc Bidi(stream EchoRequest) returns (stream EchoResponse);
}
```

`Cargo.toml` and `build.rs`:

```toml
[dependencies]
ferroly = { version = "0.6", features = ["grpc"] }
tokio = { version = "1", features = ["full"] }

[build-dependencies]
ferroly = { version = "0.6", default-features = false, features = ["grpc"] }
```

```rust,ignore
// build.rs
use ferroly::grpc::codegen::GrpcGenerator;

fn main() {
    println!("cargo:rerun-if-changed=proto");
    let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("protos.rs");
    ferroly::proto::codegen::Config::new()
        .service_generator(GrpcGenerator::new())
        .compile_to_file(&["proto"], &["echo/v1/echo.proto"], &out)
        .unwrap_or_else(|e| panic!("{e}"));
}
```

The server — implement the generated trait, register its adapter:

```rust,ignore
pub mod protos {
    include!(concat!(env!("OUT_DIR"), "/protos.rs"));
}
use ferroly::grpc::{BoxFuture, GrpcServer, Request, Response, Status, Streaming};
use protos::echo::v1::echo_server::{Echo, EchoServer};
use protos::echo::v1::{EchoRequest, EchoResponse};

struct MyEcho;

impl Echo for MyEcho {
    fn unary(&self, req: Request<EchoRequest>)
        -> BoxFuture<'_, Result<Response<EchoResponse>, Status>>
    {
        Box::pin(async move {
            let m = req.into_inner();
            if m.message.is_empty() {
                return Err(Status::invalid_argument("message is empty"));
            }
            Ok(Response::new(EchoResponse { message: m.message, ..Default::default() }))
        })
    }

    fn server_stream(&self, req: Request<EchoRequest>)
        -> BoxFuture<'_, Result<Response<Streaming<EchoResponse>>, Status>>
    {
        Box::pin(async move {
            let m = req.into_inner();
            let replies = (0..m.repeat).map(|index| EchoResponse {
                message: m.message.clone(),
                index,
                ..Default::default()
            });
            Ok(Response::new(Streaming::from_messages(replies)))
        })
    }

    fn client_stream(&self, req: Request<Streaming<EchoRequest>>)
        -> BoxFuture<'_, Result<Response<EchoResponse>, Status>>
    {
        Box::pin(async move {
            let mut input = req.into_inner();
            let mut parts = Vec::new();
            while let Some(m) = input.message().await? {
                parts.push(m.message);
            }
            Ok(Response::new(EchoResponse { message: parts.join(" "), ..Default::default() }))
        })
    }

    fn bidi(&self, req: Request<Streaming<EchoRequest>>)
        -> BoxFuture<'_, Result<Response<Streaming<EchoResponse>>, Status>>
    {
        Box::pin(async move {
            let mut input = req.into_inner();
            let (tx, out) = Streaming::channel(8);
            tokio::spawn(async move {
                while let Some(m) = input.message().await? {
                    tx.send(EchoResponse { message: m.message, ..Default::default() }).await?;
                }
                tx.finish().await
            });
            Ok(Response::new(out))
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = GrpcServer::new().add_service(EchoServer::new(MyEcho));
    let listener = tokio::net::TcpListener::bind("0.0.0.0:50051").await?;
    server.serve(listener, async { let _ = tokio::signal::ctrl_c().await; }).await?;
    Ok(())
}
```

The client:

```rust,ignore
use std::time::Duration;
use ferroly::grpc::{Channel, Request, Streaming};
use protos::echo::v1::echo_client::EchoClient;
use protos::echo::v1::EchoRequest;

let client = EchoClient::new(Channel::new("http://127.0.0.1:50051")?);

// Unary: pass the message, or a Request with metadata and a timeout.
let reply = client.unary(EchoRequest { message: "hi".into(), ..Default::default() }).await?;
let mut req = Request::new(EchoRequest { message: "hi".into(), ..Default::default() })
    .with_timeout(Duration::from_secs(2));
req.metadata_mut().insert("x-request-id", "r-1")?;
let reply = client.unary(req).await?;

// Server streaming.
let mut stream = client
    .server_stream(EchoRequest { message: "x".into(), repeat: 3, ..Default::default() })
    .await?
    .into_inner();
while let Some(m) = stream.message().await? {
    println!("{} #{}", m.message, m.index);
}

// Bidirectional: queue the first request, then read replies as you send.
let (tx, requests) = Streaming::channel(8);
tx.send(EchoRequest { message: "ping".into(), ..Default::default() }).await?;
let mut replies = client.bidi(requests).await?.into_inner();
let first = replies.message().await?;
tx.finish().await?;
```

Without code generation, register closures and call by path:

```rust,ignore
let server = GrpcServer::new().unary("/demo.Echo/Ping", |req: Request<Ping>| async move {
    Ok(Response::new(Ping { n: req.into_inner().n + 1 }))
});
let reply = channel.unary::<Ping, Ping>("/demo.Echo/Ping", Request::new(Ping { n: 1 })).await?;
```

## Call shapes

| Shape | `.proto` | Server registration | Handler receives → returns | Client call |
|---|---|---|---|---|
| Unary | `rpc M(A) returns (B)` | `GrpcServer::unary` | `Request<A>` → `Response<B>` | `Channel::unary` → `Response<B>` |
| Server streaming | `returns (stream B)` | `server_streaming` | `Request<A>` → `Response<Streaming<B>>` | `server_streaming` → `Response<Streaming<B>>` |
| Client streaming | `rpc M(stream A)` | `client_streaming` | `Request<Streaming<A>>` → `Response<B>` | `client_streaming(Request<Streaming<A>>)` → `Response<B>` |
| Bidirectional | both | `bidi_streaming` | `Request<Streaming<A>>` → `Response<Streaming<B>>` | `bidi_streaming` → `Response<Streaming<B>>` |

Every handler returns `Result<_, Status>`; every client call returns `Result<_, Status>`.
Handlers are `Fn(Request<A>) -> impl Future + Send + 'static`; the messages are any
`proto::Message + Send + 'static` (generated, derived or hand-written).

A **client** call with a single response returns after the whole exchange. A call with a
response stream returns at the response **headers**; read the rest from the `Streaming`.
Request streams are uploaded by a background task while the response is read, so both
directions flow at once on one HTTP/2 stream.

A server may send its response headers only together with its first message (grpc-go
does), so for a bidirectional call queue the first request — or feed the request stream
from another task — **before** awaiting `bidi_streaming`.

## Streams: `Streaming` and `StreamSender`

`Streaming<M>` is both what a call receives (a request stream on the server, a response
stream on the client) and what a side sends:

| Make one | |
|---|---|
| `Streaming::channel(capacity) -> (StreamSender<M>, Streaming<M>)` | fed by another task; `send` waits while `capacity` messages are queued |
| `Streaming::from_messages(iter)` | from an iterator, pulled lazily one message per read (so it may be long or unbounded); the iterator must be `Send + 'static` |
| `Streaming::ended(status)` | already over (cleanly, or with an error) |

Read with `message().await -> Result<Option<M>, Status>`: `Ok(None)` is the clean end,
`Err` the call's error. On a client, `trailers()` then has the trailing metadata. The
same read in other shapes:

- `next().await -> Option<Result<M, Status>>`, the shape of `Stream::next`, for
  `while let Some(m) = stream.next().await`;
- `collect().await -> Result<Vec<M>, Status>` for a whole (bounded) stream;
- `poll_message(&mut self, cx) -> Poll<Result<Option<M>, Status>>`, the poll form. ferroly
  depends on no `futures` crate, so `Streaming` does not implement `futures::Stream`
  itself; a five-line adapter does (`self.0.poll_message(cx).map(Result::transpose)`,
  the example on `Streaming`'s docs). Once it returned `Pending`, keep polling it or call
  `message`, which finishes the same read.

`StreamSender` methods: `send(m)`, `finish()` (the clean end), `fail(status)` (end with an
error: the server's call ends with that status; a client's call is cancelled and fails
with it), `is_closed()`, `closed().await`.

**A stream ends cleanly only when the sender says so.** Dropping a `StreamSender`
without `finish` or `fail` — its task panicked, or returned early through `?` — ends the
stream with `INTERNAL`, so a crashed producer never looks like a complete stream. And
`send` fails with `CANCELLED` once nobody reads any more (the call ended, passed its
deadline, or the peer cancelled), so a producer stops.

## Status and errors

`Status` is the outcome of every call: a `Code`, a message, optional binary details and
trailing metadata. It implements `std::error::Error` and converts into `ferroly::Error`
(variant `Error::Grpc`, classified from its code by `errutils::Classify`).

```rust,ignore
use ferroly::grpc::{Code, Status};

let status = Status::not_found("no user 7")
    .with_details(encoded_google_rpc_status); // grpc-status-details-bin, carried as raw bytes
assert_eq!(status.code(), Code::NotFound);
```

- `Code` has the 17 standard codes (`#[non_exhaustive]`); `Code::from_i32` maps any
  unknown number to `Unknown`.
- On the wire: `grpc-status`, `grpc-message` (percent-encoded per the spec: bytes outside
  `0x20..=0x7E` and `%`; decoding never fails), `grpc-status-details-bin` (base64).
- A handler error before any response message is sent as a **trailers-only** response
  (the status in the response headers, no body); otherwise in the trailers.
- A response that is **not gRPC** maps by its HTTP status, per the spec: `400` →
  `INTERNAL`, `401` → `UNAUTHENTICATED`, `403` → `PERMISSION_DENIED`, `404` →
  `UNIMPLEMENTED`, `429`/`502`/`503`/`504` → `UNAVAILABLE`, anything else (including a
  `200` that is not `application/grpc`) → `UNKNOWN` (`Code::from_http_status`). When a
  non-`200` answer still carries a `grpc-status`, that status is used.
- Transport failures are mapped in one place, on the structured HTTP/2 cause
  (`HttpError::http2_cause`), never on message text, following the gRPC spec's HTTP/2
  table and what grpc-go reports:

  | Failure | Code |
  |---|---|
  | `RST_STREAM(CANCEL)` | `CANCELLED` |
  | `RST_STREAM(REFUSED_STREAM)`, a refused stream, GOAWAY, the connection closed (EOF) or reset, an I/O error, a connection timer (keep-alive, stalled writes), a local flood limit | `UNAVAILABLE` |
  | `RST_STREAM(ENHANCE_YOUR_CALM)` | `RESOURCE_EXHAUSTED` |
  | `RST_STREAM(INADEQUATE_SECURITY)` | `PERMISSION_DENIED` |
  | any other `RST_STREAM` code, a protocol violation by the server | `INTERNAL` |
  | a stream this side reset for its own reasons (`Http2Cause::LocalReset`): `CANCEL` or `NO_ERROR` | `CANCELLED` |
  | the same with `REFUSED_STREAM` (a buffered body over the connection's budget) / `INTERNAL_ERROR` | `UNAVAILABLE` / `INTERNAL` |
  | connect failure or timeout, TLS, proxy, a server that does not speak HTTP/2 | `UNAVAILABLE` |
  | an HTTP-level timeout of a `from_client` client | `DEADLINE_EXCEEDED` |

  Once the call's deadline has passed, any such failure is `DEADLINE_EXCEEDED`: a peer
  that resets the stream at its own copy of the deadline (grpc-go does) can win the race
  against the local timer. Status messages name the HTTP/2 cause (for example
  `HTTP/2: stream reset by the peer: CANCEL (0x8)`).
- A server stream that fails mid-way delivers its messages, then `Err(status)`.

## Metadata

`Metadata` holds the custom headers and trailers of a call:

```rust,ignore
let mut req = Request::new(msg);
req.metadata_mut().insert("x-request-id", "r-1")?;     // ASCII
req.metadata_mut().append("x-tag", "a")?;              // several values per key
req.metadata_mut().insert_bin("trace-bin", bytes)?;    // binary: key ends in -bin
```

- Keys are lower-case `0-9 a-z - _ .` (upper case is lowered); ASCII values are printable
  `0x20..=0x7E`. Keys ending in `-bin` carry bytes, base64-encoded on the wire (sent
  unpadded; padded and unpadded accepted, as are several comma-separated values).
- Reserved keys are refused by `insert`: `grpc-*`, `content-type`, `te`, `user-agent`,
  `content-length`, `host` and the connection-specific headers. Received metadata never
  contains `grpc-*`, `content-type`, `te` or `content-length`; an undecodable `-bin`
  value is dropped.
- Server: `Request::metadata()` is what the client sent; `Response::metadata_mut()` goes
  out in the response headers and `Response::trailers_mut()` in the trailers (a
  `Status`'s own metadata is sent with it).
- Client: `Response::metadata()` has the response headers, `Response::trailers()` the
  trailers of a single-response call; for a stream, `Streaming::trailers()` after the
  end, or `Status::metadata()` on failure.

## Deadlines

The client sets a timeout per call (`Request::with_timeout` / `set_timeout`), a deadline
(`Request::with_deadline` / `set_deadline`, a `std::time::Instant`; with both, the earlier
wins) or a default for the channel (`ChannelBuilder::timeout`). It is:

- sent as `grpc-timeout`, in the finest unit that fits eight digits, rounded up
  (`format_timeout` / `parse_timeout` handle every unit `H M S m u n`);
- **enforced locally**: the call — including reading a response stream — fails with
  `DEADLINE_EXCEEDED` when it passes, even if the server never answers. A deadline
  already in the past fails the call before anything is sent.

**Propagation.** A handler that calls other services passes its incoming deadline on with
`Request::inherit_deadline(&incoming)`, so the downstream calls give up when its caller
would:

```rust,ignore
async fn front(req: Request<Query>, back: BackClient) -> Result<Response<Answer>, Status> {
    let downstream = Request::new(Lookup::from(req.get_ref())).inherit_deadline(&req);
    let found = back.lookup(downstream).await?;
    Ok(Response::new(Answer::from(found.into_inner())))
}
```

The server parses `grpc-timeout` (malformed → `INTERNAL`) and, when it passes, drops the
handler future, stops forwarding a response stream (so its `StreamSender` fails) and
answers `DEADLINE_EXCEEDED`. Handlers can read `Request::timeout()` / `deadline()` to give
up early on work outside the handler future. A value beyond what an `Instant` can hold
means no deadline.

## Cancellation

- **Client:** dropping a call's future, or the returned `Streaming`, resets the HTTP/2
  stream. A failing request stream (`StreamSender::fail`) cancels the call with that
  status.
- **Server:** a reset stream aborts the handler; a response stream's forwarding task stops
  at once (even while the handler's producer is idle), so the producer's next `send`
  fails with `CANCELLED` and `StreamSender::closed()` resolves. A request `Streaming`
  read by the handler fails with `CANCELLED`.

## Interceptors

An `Interceptor` runs before every call, on a `Channel` (before the request is sent) or a
`GrpcServer` (before the handler), with the method path and the call's head as a
`Request<()>`: metadata, timeout and deadline, and on a server the peer. It may change the
metadata (attach a token, a trace id), set the timeout on a client, or fail the call with a
`Status` (the client then sends nothing; the server does not run the handler). Any
`Fn(&str, &mut Request<()>) -> Result<(), Status> + Send + Sync + 'static` is one:

```rust,ignore
let channel = Channel::builder(url)?
    .interceptor(|_path: &str, req: &mut Request<()>| {
        req.metadata_mut().insert("authorization", "Bearer t0ken")
    })
    .build()?;

let server = GrpcServer::new()
    .interceptor(|path: &str, req: &mut Request<()>| match req.metadata().get("authorization") {
        Some("Bearer t0ken") => Ok(()),
        _ => Err(Status::unauthenticated(format!("{path} needs a token"))),
    })
    .add_service(EchoServer::new(MyEcho));
```

Interceptors run in the order added and are synchronous; on a server, timeout and
deadline changes are ignored (the client's `grpc-timeout` rules). They see the head
only: for per-message or completion hooks, wrap the handler (or the `Streaming`).

## The server

```rust,ignore
let server = GrpcServer::new()
    .max_decoding_message_size(8 << 20)   // default 4 MiB
    .max_encoding_message_size(8 << 20)   // default 4 MiB
    .max_decoded_bytes(128 << 20)         // default 16 x the decoding limit
    .on_handler_panic(|path, msg| eprintln!("{path} panicked: {msg}"))
    .interceptor(auth)                    // see Interceptors
    .require_te_trailers(false)           // default: lenient, like grpc-go
    .add_service(EchoServer::new(MyEcho))
    .add_service(health.service());
```

Serve it like any `HttpHandler`, with `ServerConfig::http2` set:

```rust,ignore
use std::sync::Arc;
use ferroly::http::{serve_tls_with_config, serve_with_config, Http2Config, ServerConfig, TlsConfig};

let config = ServerConfig { http2: Some(Http2Config::default()), ..ServerConfig::default() };
// Cleartext h2c (prior knowledge):
serve_with_config(listener, Arc::new(server), shutdown, config).await?;
// TLS: ALPN selects h2.
serve_tls_with_config(listener, Arc::new(server), shutdown, config, TlsConfig::from_pem(cert, key)?).await?;
```

`GrpcServer::serve(listener, shutdown)` is the h2c shorthand with
`GrpcServer::server_config()`: the `ServerConfig` defaults with HTTP/2 on and
gRPC-appropriate HTTP/2 settings —

- `Http2Config::stream_stall_timeout: None`: a client that stops reading a long-lived
  server or bidirectional stream keeps it (the HTTP default, 30 s, resets the stream; a
  connection that stops draining entirely is still closed after `write_timeout`);
- `max_streams_per_connection: None`: no GOAWAY after a number of calls
  (`ServerConfig::max_keep_alive_requests` applies only to HTTP/1.1);
- `rapid_reset_threshold: 10 ms` (the HTTP default is 100 ms): only a stream the client
  resets within 10 ms of opening it counts towards the rapid-reset budget, so a busy
  client whose calls end by cancellation or deadline expiry, even with deadlines of a
  few tens of milliseconds (grpc-go clients often use 20 ms), keeps its connection,
  while the attack pattern (a reset right after HEADERS) is still caught. Holding each
  call just past 10 ms does not get around it: resets of calls that had not completed,
  whatever their age, count toward `max_cancels_per_window` (1000 per 10 s, or twice the
  calls completed in that time). Set the threshold to `Duration::ZERO` to count no reset
  toward the hard budget, as grpc-go does; the cancel budget still applies. A reset
  stops the handler (its future is dropped), but not work it handed off: make spawned
  tasks, blocking jobs and downstream calls end with the handler (for example by
  aborting them when a guard is dropped), or a client that cancels in a loop keeps them
  running.

  A reset stream frees its slot when its handler future is dropped, so this bounds only
  work that dropping cancels. A client that resets each call just after the threshold
  can start handlers far faster than `max_concurrent_streams` allows at once: work a
  handler hands off and does not stop on drop (`tokio::spawn`, `spawn_blocking`, a
  downstream call it does not await) keeps running for every one of them. Make such work
  cancellable — keep the `JoinHandle` and abort it on drop, or await it in the handler —
  or bound it yourself (a semaphore).

Pass `GrpcServer::server_config()` (adjusted as needed) to `serve_with_config` /
`serve_tls_with_config` to get the same with TLS. All of `ServerConfig` and `Http2Config`
apply: connection and stream limits, flood defences, `head_timeout`, graceful drain on
shutdown. Leave `handler_timeout` at `None` (deadlines are per call) — it would bound only
the time to the response headers.

When `serve`'s shutdown signal fires, every health service added to the server marks all
services `NOT_SERVING` and ends its `Watch` streams, so they do not hold the drain open
until `drain_timeout`; with `serve_with_config`, call `HealthReporter::shutdown()` first.

Per call the server:

| Situation | Answer |
|---|---|
| HTTP/1.1 request | HTTP `400`, `grpc-status: INTERNAL` ("gRPC requires HTTP/2") |
| content type not `application/grpc` / `application/grpc+proto` (incl. `+json`) | HTTP `415`, `INTERNAL` |
| method not `POST` | HTTP `405`, `INTERNAL` |
| `grpc-encoding` other than `identity`, `gzip`, `deflate` | `UNIMPLEMENTED` (with `grpc-accept-encoding: identity,gzip,deflate`) |
| compressed flag set without a `grpc-encoding`, an invalid flag, corrupt compressed data | `INTERNAL` |
| `te: trailers` missing, with `require_te_trailers(true)` | `INTERNAL` (served by default) |
| an interceptor's error | that status |
| malformed `grpc-timeout` | `INTERNAL` |
| unknown service / unknown method | `UNIMPLEMENTED` ("unknown service x" / "unknown method m for service x") |
| unary request with no message, or more than one | `INTERNAL` |
| message over the size limit (after decompression), or over the decoded-memory budget | `RESOURCE_EXHAUSTED` |
| undecodable message | `INTERNAL` |
| handler or server interceptor panics, or a panic while its response stream is encoded | `INTERNAL` ("the handler panicked"; the panic text goes only to `on_handler_panic`) |
| deadline passes | `DEADLINE_EXCEEDED` |

Responses carry `content-type: application/grpc` (or `+proto`, echoing the request) and
`grpc-accept-encoding: identity,gzip,deflate`; their messages are not compressed.

### gRPC and HTTP on one port

`GrpcServer::fallback(handler)` sends every request whose content type is not
`application/grpc…` to another `HttpHandler` — for example a [`turbo`](turbo.md) router —
so a REST API, health endpoints and gRPC share one listener (over HTTP/1.1 and HTTP/2):

```rust,ignore
let router = ferroly::turbo::Router::new().get("/healthz", |_| async { HttpResponse::ok() });
let server = GrpcServer::new().add_service(EchoServer::new(MyEcho)).fallback(router.into_handler());
serve_with_config(listener, Arc::new(server), shutdown, config).await?;
```

(Mounting a `GrpcServer` *inside* a turbo router is not supported: turbo handlers do not
stream request bodies, which client and bidirectional streaming need. Use the fallback,
or run the two on separate ports.)

## The client

```rust,ignore
let channel = Channel::builder("https://api.example.com")?
    .tls(ClientTlsConfig::new())                 // roots, client cert for mTLS
    .timeout(Duration::from_secs(10))            // default per call
    .connect_timeout(Some(Duration::from_secs(5)))
    .max_decoding_message_size(8 << 20)          // default 4 MiB
    .max_encoding_message_size(8 << 20)          // default 4 MiB
    .max_decoded_bytes(128 << 20)                // default 16 x the decoding limit
    .http2_config(Http2Config::default())
    .user_agent("my-service/1.0")
    .interceptor(auth)                           // see Interceptors
    .build()?;
```

- `http://` speaks h2c with prior knowledge, `https://` TLS with ALPN `h2`. A server that
  does not speak HTTP/2 (ALPN chose HTTP/1.1, or no h2c) fails the call with
  `UNAVAILABLE` instead of being spoken to over HTTP/1.1.
- A channel connects lazily and is cheap to clone; all clones share one HTTP/2
  connection per server, and concurrent calls are multiplexed on it.
- The URL may carry a path prefix (`https://gw.example.com/grpc`), prepended to every
  method path.
- `Channel::from_client(client, url)` returns a `ChannelBuilder` over a configured
  `http::Client` (its HTTP interceptors, a CONNECT proxy, a shared pool), for the
  message limits, default timeout and gRPC interceptors. `build` fails with
  `INVALID_ARGUMENT` unless the client has HTTP/2 enabled for the URL's scheme
  (`ClientBuilder::http2` for `https`, `http2_prior_knowledge` for `http`), so a call
  never goes out over HTTP/1.1; it fails too when a connection setting (TLS, HTTP/2
  config, connect timeout, user agent) is set, since the client fixes those. The
  client's own timeouts and body cap apply on top of the call deadlines.
- **Keep-alive.** The channel sends no HTTP/2 PINGs by default, so a peer that vanishes
  without a TCP reset hangs a call that has no deadline (a `Watch`, a long bidirectional
  stream). Set `Http2Config::keepalive_interval` (and `keepalive_timeout`) through
  `http2_config` to detect it (`UNAVAILABLE`); keep the interval at or above what the
  server allows — grpc-go closes connections that ping more often than every 5 minutes
  by default.
- No HTTP-level timeout cuts a long stream: the channel's client has none, and only the
  call's deadline (if any) limits it.

## Transparent retry

A call the server refused **without processing it** — `RST_STREAM(REFUSED_STREAM)`, or a
stream above the last stream id of a GOAWAY (a server shutting down, or at its stream
limit) — is sent again on another stream or connection, as gRPC's "transparent retry"
asks: once at once, then once more after a random pause of 25–75 ms (so refused clients
do not come back in lockstep and feed a server's reset budget). For a unary or
server-streaming call the at-once retry is the HTTP client's own, on another connection
(a fresh one while the origin is below `max_connections_per_origin`), and the paused one goes through it too: a server that refuses everything sees such a call
at most four times. A streamed upload is sent at most three times. Each retry carries
the `grpc-timeout` left at that moment, never the original one, and no retry is made
once the deadline has passed.

- unary and server-streaming requests are sent as one buffered, replayable body;
- a streamed upload (client or bidirectional streaming) keeps what it has sent until the
  response headers arrive, up to 256 KiB, and replays it; a call refused after sending
  more fails with `UNAVAILABLE`. The refused attempt is stopped before the replay starts,
  so a request stream with nothing more to send yet (a ping-pong bidi call waiting for
  its first answer) is replayed at once too.

Nothing else is retried: a call the server may have processed (a reset of a live stream,
a connection lost without GOAWAY) fails with its status. There is no retry policy
(`retryPolicy` of the service config) or hedging.

## Compression

Received messages compressed with `gzip` or `deflate` (zlib) are decoded, on both sides:
a request with `grpc-encoding: gzip`, and a response from a server that compresses (the
client advertises `grpc-accept-encoding: identity,gzip,deflate`). Decompression is
bounded: it stops with `RESOURCE_EXHAUSTED` as soon as the output passes the message
size limit, so a compression bomb costs at most the limit. This side always **sends**
`identity` — the repository has an inflater but no deflater — which every gRPC peer
accepts.

## Code generation

`ferroly::grpc::codegen::GrpcGenerator` is a
[`ServiceGenerator`](proto.md#extending-code-generation-servicegenerator). For each
`service Foo` it adds two submodules to the package module, as tonic does, so the
generated names never clash with the package's messages (a message may be called
`FooClient`):

| Item | What it is |
|---|---|
| `foo_server::Foo` (`trait Foo: Send + Sync + 'static`) | one method per `rpc`: `fn bar(&self, Request<A>) -> BoxFuture<'_, Result<Response<B>, Status>>`, with `Streaming<…>` on streamed sides. `BoxFuture` keeps the trait object-safe without an `async-trait` crate: write `Box::pin(async move { … })` |
| `foo_server::FooServer<T: Foo + ?Sized>` | `FooServer::new(impl)` / `from_arc(Arc<impl Foo>)` or `from_arc(Arc<dyn Foo>)`; implements `GrpcService`, for `GrpcServer::add_service` |
| `foo_client::FooClient` | `FooClient::new(channel)`; one `async fn bar(&self, impl IntoRequest<A>)` per `rpc` — pass the message or a `Request` of it |

```rust,ignore
use protos::echo::v1::echo_client::EchoClient;
use protos::echo::v1::echo_server::{Echo, EchoServer};
use protos::echo::v1::{EchoRequest, EchoResponse};
```

Doc comments come from the `.proto` comments, and every generated module and item is
documented, so the code compiles under a consumer's `#![deny(missing_docs)]`; clippy's
naming lints (`new_ret_no_self`, `wrong_self_convention`, `should_implement_trait`) are
allowed on the trait and the client, since their method names come from the `.proto`.
Method names are `snake_case` (a Rust keyword becomes `r#type`). The adapter calls the
trait fully qualified, so an `rpc` named `Clone`, `Deref` or `AsRef` works. On the
client, an `rpc` whose name is one of the client's own methods or would be shadowed by a
prelude trait — `New`, `Channel`, `Clone`, `CloneFrom`, `Fmt`, `Into`, `TryInto` —
becomes `new_rpc`, `channel_rpc`, `clone_rpc` and so on; when another `rpc` already has
that name (`rpc NewRpc` next to `rpc New`), further `_`s are added (`new_rpc_`). A message or nested-type
module that would also be named `foo_server` / `foo_client` is an `IdlError`.
`GrpcGenerator::new().server(false)` or `.client(false)` skips a side (and its
submodule). The `FOO_DESCRIPTOR` constant from `proto::codegen` stays in the package
module and lists the methods and paths.

`tests/grpc_data/echo_generated.rs` and `tests/grpc_data/tricky_generated.rs` are the
generator's output for `tests/grpc_data/echo/v1/echo.proto` and
`tests/grpc_data/tricky/v1/tricky.proto` (rpcs named `Clone`, `Deref`, `New`, `Into`…,
messages named like the generated items, a `dyn` service, a cross-package type). Golden
tests keep them current, and `tests/grpc_codegen.rs` compiles them publicly under
`#![deny(missing_docs)]` and clippy and runs them end to end.

## The health service

`grpc::health` implements `grpc.health.v1.Health` (`Check` and `Watch`) as used by load
balancers, Kubernetes gRPC probes and `grpc_health_probe`:

```rust,ignore
use ferroly::grpc::health::{HealthReporter, ServingStatus};

let health = HealthReporter::new();                       // "" (whole server) is SERVING
health.set_status("echo.v1.Echo", ServingStatus::Serving);
let server = GrpcServer::new().add_service(health.service());
// When draining (GrpcServer::serve does this itself on its shutdown signal):
health.shutdown();
```

`Check` of an unknown service is `NOT_FOUND`; `Watch` sends the current status, then every
change (`SERVICE_UNKNOWN` for an unknown or cleared service). `HealthReporter::shutdown`
marks every service `NOT_SERVING`, sends that to each `Watch`, and then ends the `Watch`
streams cleanly, so infinite watches do not hold a graceful drain open; later `Watch`
calls get the status and end at once. After it, `set_status` and `clear` are ignored, as
in grpc-go: a draining server keeps reporting `NOT_SERVING`. The message types
(`HealthCheckRequest`, `HealthCheckResponse`) and paths (`CHECK_PATH`, `WATCH_PATH`) are
public, for clients.

## Limits and untrusted input

| Limit | Default | Where |
|---|---|---|
| Message size, received | 4 MiB | `GrpcServer::max_decoding_message_size`, `ChannelBuilder::max_decoding_message_size` |
| Message size, sent | 4 MiB | `max_encoding_message_size` on both |
| Concurrent streams, windows, header list size, flood budgets | see [`Http2Config`](http.md#limits-and-defences) | `ServerConfig::http2`, `ChannelBuilder::http2_config` |
| Decoded memory per message (not in aggregate: see below) | 16 × the received-size limit, at least 1 MiB (64 MiB by default) | `GrpcServer::max_decoded_bytes`, `ChannelBuilder::max_decoded_bytes` |
| Protobuf decode budget | depth 100, input = the message size limit, decoded memory as above | `proto::Limits` |
| Streamed upload kept for a transparent retry | 256 KiB | fixed |

- The 5-byte prefix's length is checked against the limit **before** anything of the
  message is buffered, and the buffer only ever holds bytes that actually arrived; a
  hostile `0xFFFFFFFF` length costs nothing. Messages are reassembled across arbitrary
  DATA-frame boundaries (`FrameDecoder`, public, for custom transports).
- Request bodies are streamed, never buffered whole: a unary call reads one message and
  then expects the end.
- The **decoded-memory budget** (`proto::Limits::max_decoded_bytes`) charges the
  in-memory size of every decoded value: a 4 MiB request of empty sub-messages (`0a 00`,
  two bytes each on the wire, one whole struct each in memory) is `RESOURCE_EXHAUSTED`
  after at most the budget, instead of expanding into a gigabyte. Realistic messages
  cost 3–8× their wire size, so the default 16× leaves room; raise it for very sparse,
  wide messages near the size limit.
- The budget is **per message**, not per server: each request decoded at once may hold
  it while its handler runs, so the aggregate is about budget × `max_concurrent_streams`
  × connections — 100 streams of the default 64 MiB is 6.4 GiB per connection, and every
  further connection adds as much. A request can approach its budget from about
  1/256 of it on the wire (empty elements of a wide struct). Lower
  `max_decoded_bytes` to what your messages need and bound concurrency with
  `Http2Config::max_concurrent_streams` and `ServerConfig::max_connections` for your
  memory. The client side is the same per response, times the calls in flight.
- `grpc-timeout` accepts at most eight digits; metadata keys and values are validated;
  `grpc-message` decoding never fails; status numbers out of range are `UNKNOWN`.
- The `grpc_frames` fuzz target and the `grpc_fuzz.rs` suite cover framing, status
  trailers, timeouts and metadata decoding.

## Interoperability

Checked manually against **grpc-go** (v1.85) in both directions over h2c — a grpc-go client
calling a ferroly server, and a ferroly client calling a grpc-go server — for all four call
shapes: ASCII and binary metadata in headers and trailers, error statuses with
percent-encoded messages, an error after a streamed message, deadlines (propagated and
enforced), unknown methods, ping-pong bidirectional streaming and cancellation.

The `Interop` CI workflow repeats these checks against grpc-go v1.76 on every pull
request, together with `google.rpc.Status` details, a gzip-compressed request and a
gzip-compressed response, the health service, and 400 deadline-cancelled calls on one
connection while another call is in flight. Run it locally with `interop/grpc-go.sh` (needs Go): the
Go side is `interop/grpc-go/`, the ferroly side `crates/ferroly/examples/grpc_interop.rs`.

## How it is tested

- Unit tests: status codes, percent-encoding, HTTP mapping, metadata rules and base64
  forms, `grpc-timeout` parsing and formatting, frame decoding at every split point,
  limits, compressed and invalid flags, truncation.
- `tests/grpc_integration.rs` (h2c): the four shapes; metadata and trailers both ways;
  trailers-only errors; mid-stream errors; a producer dropped unfinished; unknown
  service / method; handler panics; oversized messages both directions; deadlines on the
  client (also against a server that never answers) and on the server; cancellation of
  unary and streaming calls reaching the handler; 40 concurrent calls on one connection;
  wrong content type, method, protocol, encoding, flags and timeouts; HTTP status mapping;
  an HTTP/1.1-only server; the health service; the non-gRPC fallback (and a turbo router
  behind it).
- `tests/grpc_transport.rs`: the client against a scripted HTTP/2 server built from raw
  frames — the code of every `RST_STREAM` code, GOAWAY, EOF and TCP reset, each reaching
  the server once; transparent retry of refused unary, server-streaming and replayed
  streamed uploads (also an idle bidi request stream), and calls straddling a GOAWAY;
  the exact number of sends to a server that refuses everything; no retry of a refusal
  after the response head or of an upload past the replay buffer; a peer resetting at its own deadline; `RST_STREAM(NO_ERROR)` after
  early trailers keeping the connection; gzip responses and a compression bomb; response
  content types. Against ferroly's own server: 400 cancellations and deadline expiries
  plus an in-flight call on one connection, 1200 calls on one connection, compressed
  requests, `te: trailers`, interceptors on both sides, a panicking server interceptor,
  deadline propagation, a panic in a response stream, health `Watch` ending on shutdown,
  lazy, pollable and cancel-safe `Streaming`.
- `tests/grpc_budget.rs`: a 4 MiB amplifying request rejected with peak memory measured
  by a counting allocator.
- `tests/grpc_tls.rs`: the four shapes over TLS with ALPN, and an HTTP/1.1-only TLS
  server failing cleanly.
- `tests/grpc_codegen.rs` / `grpc_codegen_golden.rs`: generated code, end to end and
  against the checked-in golden file.

## Not supported

- **Sending compressed messages**: received `gzip` / `deflate` messages are decoded, but
  this side always sends `identity` (no deflater in the repository).
- **Reflection** (`grpc.reflection.v1`): it serves `FileDescriptorProto`s, which the code
  generator does not embed (the generated `ServiceDescriptor`s carry names, paths and
  streaming flags only), so `grpcurl` needs the `.proto` files (`-proto`).
- **Retry policies** and **hedging** (only transparent retry of refused calls),
  **load balancing**, **name resolution** (a `Channel` talks to one URL) and the service
  config.
- **gRPC-Web** and gRPC over HTTP/1.1; **content subtypes** other than `proto`
  (`application/grpc+json` is answered `415`).
- **Keepalive pings** are off by default; turn them on with
  `Http2Config::keepalive_interval` (see [The client](#the-client)).
- A per-connection memory budget across calls: memory is bounded per message (size limit
  and decoded-memory budget) and per connection only through the stream limit.
- Mounting a `GrpcServer` inside a `turbo` router (use [`fallback`](#grpc-and-http-on-one-port)).
- The server's response headers for a streaming call are sent when the handler returns,
  not delayed until the first message.

## See also

- [proto](proto.md) — messages, `.proto` parsing and code generation.
- [http](http.md#http2) — the HTTP/2 transport, its limits and defences.
- [turbo](turbo.md) — the router to put behind `GrpcServer::fallback`.
- [fuzz/README.md](../fuzz/README.md) — running the fuzz targets.

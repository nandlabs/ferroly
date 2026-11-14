# ferroly::ws

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `ws` (implies `http`, `lifecycle`, `hash` and `tokio`).

A WebSocket client and server implementing **RFC 6455 entirely in-house**. There is no
`tungstenite`, no `sha1` crate, no `rand` crate. The frame codec, the opening handshake,
masking, fragment reassembly, the close handshake and keepalive are hand-rolled over
[`ferroly::http`](http.md)'s transport (plain TCP or TLS) and `tokio`; SHA-1, base64 and
randomness come from [`ferroly::hash`](hash.md).

## Overview

- **Client:** [`WsClient::dial`](#wsclient) opens a connection (with connect and handshake
  timeouts, custom headers, subprotocols, custom TLS, optional keepalive);
  [`ReconnectingClient`](#reconnectingclient) adds reconnection with backoff and a
  resubscribe hook.
- **Server:** [`server::serve_with_config`](#the-server) runs an accept loop and gives every
  connection to an async [`WsHandler`](#wshandler-and-wsconn) as a [`WsConn`](#wshandler-and-wsconn);
  [`WsServer`](#wsserver--a-lifecycle-component) wraps it as a lifecycle component. `wss`,
  mutual TLS, subprotocols, an [upgrade hook](#authentication-the-upgrade-hook) and an
  [origin policy](#origin-policy) are built in.
- **Fan-out:** [`Hub`](#hub-broadcast-and-rooms) broadcasts to all connections, a room, or
  one connection id.
- **Bounded everywhere:** both directions of every connection use bounded queues, every
  wait has a timeout, and every size has a cap. See [Backpressure](#backpressure-and-the-slow-consumer-policy)
  and [Limits and timeouts](#limits-and-timeouts).
- **Strict:** both sides enforce the protocol rules listed under
  [Conformance](#conformance).

Public items:

```rust
use ferroly::ws::{
    // client
    WsClient, WsOptions, ReconnectingClient, ReconnectOptions,
    // server
    server, WsServer, WsServerConfig, WsHandler, WsConn,
    // handshake security
    OriginPolicy, UpgradeHook, UpgradeRequest, UpgradeAccept, UpgradeReject,
    // shared
    Message, close_code, WsSender, WsError,
    // fan-out
    Hub, HubMember, SlowConsumerPolicy,
    // metrics
    WsObserver, WsEvent, TimeoutKind,
    DEFAULT_MAX_FRAME_SIZE, DEFAULT_MAX_MESSAGE_SIZE,
};
```

## Enabling

```toml
[dependencies]
ferroly = { version = "*", features = ["ws"] }
```

## `Message`

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Text(String),                          // a UTF-8 text message
    Binary(Vec<u8>),                       // a binary message
    Close { code: u16, reason: String },   // the connection ended
}
```

Ping and pong frames are handled internally and never appear. The close handshake is also
handled internally, but its result is surfaced: **the last message `recv` yields on every
connection is a `Message::Close`**, after which `recv` returns `None`.

| `code` in the final `Close` | Meaning |
| --- | --- |
| the peer's code (`1000`, `1001`, `3000..=4999`, …) | The peer sent a close frame with that status. |
| `1005` (`close_code::NO_STATUS`) | The peer's close frame carried no status. |
| `1002` / `1007` / `1009` | The **peer** broke the protocol, sent invalid UTF-8, or exceeded a size cap; this side failed the connection with that code. |
| `1001` | This side closed for an idle timeout or a server shutdown. |
| `1008` | This side closed the peer as a slow consumer (outgoing queue full). |
| `1011` | A keepalive pong did not arrive in time, or a server handler panicked. |
| `1006` (`close_code::ABNORMAL`) | The transport ended without a close frame (dropped connection, write timeout, close handshake timeout). The reason text says which. |

Passing a `Message::Close` to `send` is the same as calling `close(code, reason)`.

| Item | Purpose |
| --- | --- |
| `Message::text(s)` / `Message::binary(b)` / `Message::close(code, reason)` | Constructors. |
| `as_text()` / `as_binary()` | Borrow the payload of a text / binary message. |
| `is_close()` | Whether this is the final close message. |
| `len()` / `is_empty()` | Payload size in bytes. |

`close_code` holds the constants (`NORMAL`, `GOING_AWAY`, `PROTOCOL_ERROR`,
`UNSUPPORTED_DATA`, `NO_STATUS`, `ABNORMAL`, `INVALID_PAYLOAD`, `POLICY_VIOLATION`,
`MESSAGE_TOO_BIG`, `INTERNAL_ERROR`) and `close_code::is_sendable(code)`, which is true for
`1000..=1003`, `1007..=1014` and `3000..=4999` — the codes that may appear on the wire.

## `WsClient`

```rust
use std::time::Duration;
use ferroly::ws::{Message, WsClient, WsOptions};

#[tokio::main]
async fn main() -> Result<(), ferroly::ws::WsError> {
    let options = WsOptions::default()
        .header("Authorization", "Bearer token")
        .subprotocol("chat.v2")
        .keepalive(Duration::from_secs(20), Duration::from_secs(10));
    let mut client = WsClient::dial("wss://example.com/ws", options).await?;

    client.send(Message::text("hello")).await?;
    while let Some(msg) = client.recv().await {
        match msg {
            Message::Text(text) => println!("{text}"),
            Message::Binary(bytes) => println!("{} bytes", bytes.len()),
            Message::Close { code, reason } => println!("closed: {code} {reason}"),
        }
    }
    Ok(())
}
```

| Method | Signature | Notes |
| --- | --- | --- |
| `dial` | `async fn dial(url: &str, opts: WsOptions) -> Result<WsClient, WsError>` | `ws://` or `wss://`. Bounded by `connect_timeout` (TCP + TLS) and `handshake_timeout`. |
| `send` | `async fn send(&self, msg: Message) -> Result<(), WsError>` | Queues the message; waits while the queue is full (see [Backpressure](#backpressure-and-the-slow-consumer-policy)). |
| `try_send` | `fn try_send(&self, msg: Message) -> Result<(), WsError>` | Never waits; `QueueFull` if there is no room. |
| `recv` | `async fn recv(&mut self) -> Option<Message>` | The last message is a `Message::Close`. |
| `sender` | `fn sender(&self) -> WsSender` | A clonable handle for other tasks. |
| `close` | `async fn close(self) -> Result<(), WsError>` | Close with `1000` and wait for the close handshake. |
| `close_with` | `async fn close_with(self, code: u16, reason: &str) -> Result<(), WsError>` | Close with a code and reason. |
| `subprotocol` | `fn subprotocol(&self) -> Option<&str>` | The subprotocol the server selected. |
| `response_headers` | `fn response_headers(&self) -> &HeaderMap` | Headers of the server's `101`. |
| `id` / `is_closed` | | The connection id; whether it is closed or closing. |

Dropping a `WsClient` aborts its background task and closes the socket **without** a close
handshake; call `close` for a clean close.

### What `dial` checks

The handshake response must be `101` (anything else is `WsError::Rejected { status }`), must
carry `Upgrade: websocket`, a `Connection` header containing `upgrade`, and exactly the
`Sec-WebSocket-Accept` value derived from the key this client sent. A `Sec-WebSocket-Protocol`
is accepted only if it is one of the offered subprotocols (and there is exactly one), and any
`Sec-WebSocket-Extensions` is refused because the client never offers one. A violation is
`WsError::Connect`.

### `WsOptions`

All fields are public. Build a value with struct-update syntax or the chaining helpers, and
always end a struct literal with `..WsOptions::default()` — fields are added over time.

| Field | Default | Meaning |
| --- | --- | --- |
| `max_message_size: Option<usize>` | `Some(64 MiB)` | Cap on a reassembled message (`1009`). `None` removes the cap. |
| `max_frame_size: Option<usize>` | `Some(16 MiB)` | Cap on one frame's payload, enforced from the frame header (`1009`). `None` removes it. |
| `connect_timeout: Duration` | 10 s | TCP connect plus TLS handshake. |
| `handshake_timeout: Duration` | 10 s | Sending the upgrade request and reading the `101`. |
| `headers: HeaderMap` | empty | Extra request headers (`Authorization`, `Origin`, cookies). Handshake-managed headers are refused. |
| `subprotocols: Vec<String>` | empty | Offered in `Sec-WebSocket-Protocol`, in order of preference. |
| `tls: Option<ClientTlsConfig>` | `None` | Custom roots, a client certificate, a minimum TLS version. `None` uses the bundled web PKI roots. Certificate verification cannot be turned off. |
| `ping_interval: Option<Duration>` | `None` | Send a keepalive ping this often. |
| `pong_timeout: Duration` | 10 s | Deadline for the matching pong; missing it closes with `1011`. |
| `idle_timeout: Option<Duration>` | `None` | Close with `1001` when no frame of any kind arrives for this long. |
| `write_timeout: Option<Duration>` | `Some(30 s)` | Deadline for the peer to accept one frame. |
| `close_timeout: Duration` | 5 s | How long the close handshake may take. |
| `send_queue_capacity: usize` | 64 | Outgoing queue size, in messages. |
| `recv_queue_capacity: usize` | 16 | Incoming queue size, in messages. |
| `send_timeout: Duration` | 10 s | How long `send` waits for queue space before closing with `1008`. |
| `observer: Option<Arc<dyn WsObserver>>` | `None` | Receives [events](#observer-metrics). |

Helpers: `.header(name, value)`, `.subprotocol(name)`, `.tls(config)`,
`.keepalive(interval, pong_timeout)`, `.observer(observer)`. `Debug` prints header names
only, never values.

```rust
use ferroly::http::ClientTlsConfig;
use ferroly::ws::WsOptions;

# fn ex(ca_pem: &[u8], cert_pem: &[u8], key_pem: &[u8]) -> Result<(), ferroly::http::HttpError> {
let options = WsOptions {
    max_message_size: Some(1024 * 1024),
    ..WsOptions::default()
}
.tls(
    ClientTlsConfig::new()
        .add_root_pem(ca_pem)?             // trust a private CA
        .identity_pem(cert_pem, key_pem)?, // present a client certificate (mTLS)
);
# let _ = options;
# Ok(())
# }
```

## `ReconnectingClient`

A wrapper that re-dials when the connection is lost.

```rust
use ferroly::ws::{Message, ReconnectOptions, ReconnectingClient, WsOptions};

# async fn ex() -> Result<(), ferroly::ws::WsError> {
let mut client = ReconnectingClient::new("wss://example.com/feed", WsOptions::default())
    .with_reconnect(ReconnectOptions {
        max_attempts: Some(10),
        ..ReconnectOptions::default()
    })
    .on_connect(|sender, _reconnected| async move {
        // Runs after every successful dial: (re-)establish subscriptions here.
        sender.send(Message::text(r#"{"subscribe":"prices"}"#)).await
    });

loop {
    match client.recv().await? {
        Message::Close { code, .. } => eprintln!("connection lost ({code}); reconnecting"),
        msg => println!("{msg:?}"),
    }
}
# }
```

- It connects lazily, on the first `connect`, `send` or `recv`.
- When a connection ends, `recv` returns its final `Message::Close` (so the application
  knows there was a gap); the next call dials again after a backoff delay.
- The `on_connect(|sender: WsSender, reconnected: bool| …)` hook runs after every successful
  dial, before any message is delivered. If it fails, the connection is dropped and the
  attempt counts as failed.
- `recv`, `send` and `connect` return an error once `max_attempts` consecutive attempts have
  failed; calling again starts a new round.
- **Nothing is replayed.** Messages queued on a connection that drops are lost and delivery
  is at most once; a `send` that fails is not retried.

`ReconnectOptions`:

| Field | Default | Meaning |
| --- | --- | --- |
| `initial_backoff` | 500 ms | Delay before the first re-dial. |
| `max_backoff` | 30 s | Cap on the delay. |
| `jitter` | `0.5` | Fraction of each delay that is randomised (`0.0..=1.0`). |
| `max_attempts` | `None` | Consecutive failures before giving up; `None` retries forever. |
| `stable_after` | 10 s | A connection that lives this long resets the backoff. A shorter-lived one extends it, so a server that accepts and immediately closes is not hammered. |

The delay before attempt *n* (counting consecutive failures from 0) is
`initial_backoff × 2ⁿ`, capped at `max_backoff`, then reduced by a random fraction of up to
`jitter`. The backoff is local to `ws`; it does not depend on `ferroly::clients`.

Other methods: `is_connected()`, `reconnects()`, `sender()` (for the *current* connection),
`close()`.

## The server

### Quick start

```rust
use ferroly::ws::{server, Message, WsConn, WsServerConfig};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), ferroly::ws::WsError> {
    let listener = TcpListener::bind("127.0.0.1:9001").await.unwrap();
    // Any `Future<Output = ()>`: when it resolves the server shuts down gracefully.
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    server::serve_with_config(
        listener,
        |mut conn: WsConn| async move {
            println!("{} connected to {}", conn.id(), conn.path());
            while let Some(msg) = conn.recv().await {
                match msg {
                    Message::Text(text) => {
                        if conn.send(Message::text(format!("echo: {text}"))).await.is_err() {
                            break;
                        }
                    }
                    Message::Binary(_) => {
                        let _ = conn.close(1003, "text only");
                    }
                    Message::Close { code, reason } => {
                        println!("{} closed: {code} {reason}", conn.id());
                    }
                }
            }
        },
        shutdown,
        WsServerConfig::default(),
    )
    .await
}
```

| Function | Signature |
| --- | --- |
| `server::serve_with_config` | `async fn serve_with_config<H: WsHandler, S: Future<Output = ()> + Send>(listener: TcpListener, handler: H, shutdown: S, config: WsServerConfig) -> Result<(), WsError>` |
| `server::serve` | `async fn serve<F: Fn(Message) -> Option<Message> + Send + Sync + 'static>(listener: TcpListener, on_message: F) -> Result<(), WsError>` |
| `server::serve_with_options` | as `serve`, plus `options: WsOptions` (only its two size caps are used) |

`serve` and `serve_with_options` are the original reply-per-message shape, kept as thin
adapters: each text or binary message is passed to `on_message` and a returned message is
sent back. They use `WsServerConfig::default()`.

### `WsHandler` and `WsConn`

```rust
pub trait WsHandler: Send + Sync + 'static {
    fn handle(&self, conn: WsConn) -> BoxFuture<'_, ()>;
}
```

Any `Fn(WsConn) -> impl Future<Output = ()> + Send + 'static` closure or function implements
it (annotate the argument: `|mut conn: WsConn| async move { … }`). The server calls it once
per connection, on its own task.

- The connection stays open until the handler returns (closed with `1000`), the peer closes
  it, or a limit, timeout or shutdown ends it.
- A handler that **panics** closes its own connection with `1011`; nothing else is affected.
- The connection's slot (`max_connections`) is held until the handler returns, so return
  once `recv` yields `None` or a send fails.

`WsConn`:

| Method | Notes |
| --- | --- |
| `id() -> u64` | Unique in the process, stable for the connection's life. |
| `request() -> &UpgradeRequest` | The upgrade request: `path()`, `query()`, `target()`, `host()`, `headers()`, `header(name)`, `origin()`, `peer_addr()`, `local_addr()`, `is_tls()`, `peer_certificates()`, `subprotocols()`. |
| `path()`, `headers()`, `peer_addr()` | Shortcuts into `request()`. |
| `context::<T>() -> Option<&T>` | The value the [upgrade hook](#authentication-the-upgrade-hook) attached. |
| `subprotocol() -> Option<&str>` | The negotiated subprotocol. |
| `recv().await -> Option<Message>` | Incoming messages; the last one is a `Message::Close`. |
| `send(msg).await`, `try_send(msg)`, `close(code, reason)` | Outgoing; see `WsSender`. |
| `sender() -> WsSender` | A clonable handle for other tasks. |

### `WsSender`

A clonable, `Send + Sync` handle to one connection's outgoing queue, valid on both the
client and the server side. Use it to push from background tasks, timers or other
connections' handlers, or to register the connection in a [`Hub`](#hub-broadcast-and-rooms).

| Method | Notes |
| --- | --- |
| `id() -> u64` | The connection id. |
| `send(msg).await -> Result<(), WsError>` | Waits for queue space up to `send_timeout`; then closes the connection with `1008` and returns `QueueFull`. `Closed` if the connection is closed or closing. Returns when the message is **queued**, not when it is on the wire. |
| `try_send(msg) -> Result<(), WsError>` | Never waits and never closes: `QueueFull` leaves the connection open. |
| `close(code, reason) -> Result<(), WsError>` | Writes the messages already queued, then the close frame, then waits (up to `close_timeout`) for the peer's. Returns immediately. `code` must satisfy `close_code::is_sendable` (else `WsError::Protocol`); a reason over 123 bytes is truncated at a character boundary. Closing twice is a no-op. |
| `is_closed() -> bool` | Closed or closing. |

```rust
use ferroly::ws::{Message, WsConn};

async fn handler(mut conn: WsConn) {
    let sender = conn.sender();
    let ticker = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            if sender.send(Message::text("tick")).await.is_err() {
                break; // connection gone
            }
        }
    });
    while let Some(msg) = conn.recv().await {
        if msg.is_close() {
            break;
        }
    }
    ticker.abort();
}
```

### `WsServerConfig`

All fields are public; always end a struct literal with `..WsServerConfig::default()`.

| Field | Default | Meaning |
| --- | --- | --- |
| `max_connections: usize` | 1024 | Connections served at once, handshakes included. More wait in the listen backlog until a slot frees. |
| `max_frame_size: usize` | 16 MiB | Cap on one frame's payload (`1009`), enforced from the header. |
| `max_message_size: usize` | 64 MiB | Cap on a reassembled message (`1009`). |
| `handshake_timeout: Duration` | 30 s | The whole opening handshake: TLS, request head, upgrade hook, response. |
| `idle_timeout: Option<Duration>` | `Some(120 s)` | Close with `1001` when no frame of any kind (data, ping or pong) arrives for this long. |
| `write_timeout: Option<Duration>` | `Some(30 s)` | Deadline for a client to accept one frame; a client that stops reading is dropped. |
| `ping_interval: Option<Duration>` | `Some(30 s)` | Server-initiated keepalive ping. |
| `pong_timeout: Duration` | 10 s | Deadline for the matching pong (`1011`). |
| `close_timeout: Duration` | 5 s | How long a close handshake may take. |
| `drain_timeout: Duration` | 10 s | On shutdown, how long to wait for connections and handlers. |
| `send_queue_capacity: usize` | 64 | Per-connection outgoing queue, in messages. |
| `recv_queue_capacity: usize` | 16 | Per-connection incoming queue, in messages. |
| `send_timeout: Duration` | 10 s | How long `send` waits before the slow consumer is closed with `1008`. |
| `origin_policy: OriginPolicy` | `SameOrigin` | See [Origin policy](#origin-policy). |
| `subprotocols: Vec<String>` | empty | Supported subprotocols in order of preference. |
| `max_header_bytes` / `max_headers` / `max_line_bytes` | 64 KiB / 128 / 16 KiB | Limits on the upgrade request head (`431`, `431`, `414`/`431`). |
| `tls: Option<TlsConfig>` | `None` | Serve `wss` (see [TLS](#tls-wss-and-mutual-tls)). |
| `upgrade_hook: Option<Arc<dyn UpgradeHook>>` | `None` | See [Authentication](#authentication-the-upgrade-hook). |
| `observer: Option<Arc<dyn WsObserver>>` | `None` | Receives [events](#observer-metrics). |

Helpers: `.tls(config)`, `.origin_policy(policy)`, `.subprotocols([..])`,
`.upgrade_hook(hook)`, `.on_upgrade(closure)`, `.observer(observer)`.

```rust
use std::time::Duration;
use ferroly::ws::{OriginPolicy, WsServerConfig};

let config = WsServerConfig {
    max_connections: 10_000,
    max_message_size: 1024 * 1024,
    idle_timeout: Some(Duration::from_secs(60)),
    ..WsServerConfig::default()
}
.origin_policy(OriginPolicy::allow_list(["https://app.example.com"]))
.subprotocols(["chat.v2", "chat.v1"]);
# let _ = config;
```

### Subprotocols

The server selects the **first entry of its own `subprotocols` list that the client also
offered** and answers with exactly one `Sec-WebSocket-Protocol` header; with nothing in
common, none is sent (the client then decides whether to continue). An upgrade hook may
override the choice with `UpgradeAccept::subprotocol`, but only with a protocol the client
offered — otherwise the upgrade is answered with `500` rather than a `101` the client would
have to fail. The client offers `WsOptions::subprotocols` and refuses a selection it did not
offer.

### TLS (`wss`) and mutual TLS

Set `WsServerConfig::tls` to an [`http::TlsConfig`](http.md): every accepted connection then
performs a TLS handshake first, inside the handshake timeout. If the config requires or
requests client certificates, the verified chain is available to the upgrade hook and the
handler through `UpgradeRequest::peer_certificates()`.

```rust
use ferroly::http::{TlsConfig, TlsVersion};
use ferroly::ws::WsServerConfig;

# fn ex(cert: &[u8], key: &[u8], client_ca: &[u8]) -> Result<(), ferroly::http::HttpError> {
let tls = TlsConfig::builder()
    .cert_pem(cert, key)?
    .client_auth_required_pem(client_ca)? // mutual TLS (optional)
    .min_version(TlsVersion::Tls13)
    .build()?;
let config = WsServerConfig::default().tls(tls);
# let _ = config;
# Ok(())
# }
```

### `WsServer` — a lifecycle component

```rust
use ferroly::lifecycle::Component;
use ferroly::ws::{Message, WsConn, WsServer, WsServerConfig};

# async fn ex() -> Result<(), ferroly::lifecycle::LifecycleError> {
let server = WsServer::with_handler("ws", "127.0.0.1:9001", |mut conn: WsConn| async move {
    while let Some(Message::Text(text)) = conn.recv().await {
        if conn.send(Message::text(text)).await.is_err() {
            break;
        }
    }
})
.with_config(WsServerConfig {
    max_connections: 10_000,
    ..WsServerConfig::default()
});

server.start().await?;
// … later, on shutdown …
server.stop().await?;
# Ok(())
# }
```

| Item | Notes |
| --- | --- |
| `WsServer::with_handler(id, addr, handler)` | A per-connection `WsHandler`. |
| `WsServer::new(id, addr, on_message)` | The original reply-per-message callback. |
| `.with_config(WsServerConfig)` | Replaces the configuration (takes effect at the next start). |
| `.with_options(WsOptions)` | Legacy: applies only the two size caps. |
| `local_addr()` | The bound address after `start` (useful with port `0`). |
| `connections()` | Upgraded connections currently open. |

Lifecycle contract:

- `start` binds and spawns the accept loop. Starting while running returns
  `LifecycleError::ComponentAlreadyStarted` (it does not leak a second listener).
- `stop` runs the [graceful shutdown](#graceful-shutdown) and returns when it is done.
  Stopping a component that is not running (never started, already stopped, or in `Error`)
  is `Ok`.
- **Restartable:** `start` after `stop` binds again and serves.
- **Cancel-safe:** `start` has one await point (the bind) and stores nothing before it
  completes; if `stop` is abandoned while draining, the drain still finishes in the
  background.
- `health()` is `None` when not running, `Down` if the accept loop has died, `Degraded`
  while every connection slot is in use, and `Up` otherwise.

### Graceful shutdown

Every way of stopping the server — the `shutdown` future of `serve_with_config` resolving,
`WsServer::stop`, or the serve future being dropped or aborted — does the same thing:

1. stops accepting (the listener is closed);
2. drops connections that are still in their opening handshake;
3. sends a close frame with code **`1001` (going away)** and reason `server shutting down`
   to every live connection, after flushing what was already queued; handlers see it as
   their final `Message::Close`;
4. waits up to `drain_timeout` for the close handshakes to finish and the handlers to
   return;
5. aborts whatever is still running and drops its connection.

When the serve future is dropped rather than signalled, steps 3–5 run on a background task.

## Security

### Authentication: the upgrade hook

An `UpgradeHook` sees every upgrade request **after** it passed the protocol checks and the
origin policy and **before** the `101` is written. It can reject the request with an HTTP
response of its choosing, or accept it and attach per-connection context. Because it runs
before the upgrade, nothing a rejected client sends is ever parsed as a frame.

```rust
use ferroly::ws::{UpgradeAccept, UpgradeReject, UpgradeRequest, WsConn, WsServerConfig};

struct User {
    name: String,
}

let config = WsServerConfig::default().on_upgrade(|req: &UpgradeRequest| {
    // Anything in the request can drive the decision: path, query, headers,
    // peer address, TLS state, the client certificate.
    match req.header("authorization") {
        Some("Bearer let-me-in") => Ok(UpgradeAccept::new()
            .context(User { name: "ada".into() })),
        _ => Err(UpgradeReject::unauthorized()
            .header("WWW-Authenticate", "Bearer")
            .body("token required")),
    }
});

async fn handler(conn: WsConn) {
    let user = conn.context::<User>().expect("set by the hook");
    println!("{} connected", user.name);
}
# let _ = (config, handler);
```

- A closure `Fn(&UpgradeRequest) -> Result<UpgradeAccept, UpgradeReject>` is a hook
  (`.on_upgrade(closure)`). To `await` during the decision (a token introspection call, a
  database lookup), implement `UpgradeHook` on a type and pass it to `.upgrade_hook(..)`:

  ```rust
  use ferroly::lifecycle::BoxFuture;
  use ferroly::ws::{UpgradeAccept, UpgradeHook, UpgradeReject, UpgradeRequest};

  struct TicketCheck;

  impl UpgradeHook for TicketCheck {
      fn on_upgrade<'a>(
          &'a self,
          request: &'a UpgradeRequest,
      ) -> BoxFuture<'a, Result<UpgradeAccept, UpgradeReject>> {
          Box::pin(async move {
              // … await a lookup here …
              match request.query() {
                  Some("ticket=ok") => Ok(UpgradeAccept::new()),
                  _ => Err(UpgradeReject::forbidden()),
              }
          })
      }
  }
  ```

- The hook runs inside `handshake_timeout`; one that takes longer is cut off and the client
  receives `503`.
- `UpgradeAccept`: `.context(value)`, `.subprotocol(name)`, `.header(name, value)` (added to
  the `101`, e.g. `Set-Cookie`).
- `UpgradeReject`: `new(status)`, `unauthorized()`, `forbidden()`, `.header(..)`,
  `.body(..)`. A status below `300` is replaced by `403`.
- The client sees a rejection as `WsError::Rejected { status }`.
- Without a hook, **every request that passes the origin policy is accepted** — there is no
  authentication by default.

Browsers cannot set headers on a WebSocket handshake. For browser clients, authenticate with
a cookie (and keep the origin policy strict) or a short-lived ticket in the query string.

### Origin policy

Browsers send the page's origin with every WebSocket handshake and do **not** apply the
same-origin policy to WebSockets. A server that relies on cookies (or any credential the
browser attaches by itself) must therefore check `Origin`, or any web page can open an
authenticated connection on a visitor's behalf (cross-site WebSocket hijacking).

| `OriginPolicy` | Behaviour |
| --- | --- |
| `SameOrigin` (**default**) | An `Origin`, when present, must name the host and port the request was addressed to (`Host`); a scheme's default port may be spelled out on either side. Anything else, including `null`, is refused with `403`. |
| `AllowList(Vec<String>)` | An `Origin`, when present, must equal one of the entries (e.g. `https://app.example.com`), compared case-insensitively and ignoring a trailing slash. Everything else — including the server's own origin, unless listed — is refused with `403`. Build one with `OriginPolicy::allow_list([..])`. |
| `Any` | No check. Only for endpoints that do not rely on browser-held credentials. |

Things to know:

- **A request without an `Origin` header always passes this check.** It did not come from a
  browser, and a non-browser client can forge the header anyway. Origin checking protects
  browser users; it is **not** authentication. Use the upgrade hook for that (it can also
  require an `Origin`: `req.origin()`).
- A repeated `Origin` header is refused with `403`.
- Behind a reverse proxy that rewrites `Host`, same-origin requests look cross-origin under
  `SameOrigin`. Configure the proxy to pass `Host` through, or use an allow-list.

### Limits and timeouts

Every resource a peer can consume is bounded by default:

| Resource | Bound | On violation |
| --- | --- | --- |
| Concurrent connections | `max_connections` | Further connections wait in the listen backlog. |
| Opening handshake | `handshake_timeout`, `max_header_bytes`, `max_headers`, `max_line_bytes` | `408` (if a request had started), `431` or `414`; the socket is closed. |
| Frame payload | `max_frame_size` | Close `1009`, decided from the frame header: the payload is never buffered. |
| Message (all fragments) | `max_message_size` | Close `1009`. |
| Memory per frame | — | The read buffer grows with bytes actually received, never with the length a frame claims. |
| Control frames | 125 bytes, never fragmented | Close `1002`. |
| Pending pongs | 1 per connection | A newer ping replaces an unanswered one (see below). |
| Outgoing queue | `send_queue_capacity`, `send_timeout` | `send` waits, then closes `1008`. |
| Incoming queue | `recv_queue_capacity` | The connection stops reading from the socket (TCP backpressure). |
| A peer that stops reading | `write_timeout` | The connection is dropped (`1006` locally). |
| A silent peer | `idle_timeout`, `ping_interval` + `pong_timeout` | Close `1001` / `1011`. |
| Close handshake | `close_timeout` | The connection is dropped. |
| Shutdown | `drain_timeout` | Remaining connections are dropped. |

The size defaults (16 MiB per frame, 64 MiB per message) are the conventional ones and are
generous. A connection can hold up to `recv_queue_capacity` unread messages plus the one
being reassembled, so for an endpoint exposed to untrusted clients set `max_message_size`
(and `max_frame_size`) to what the application protocol actually needs.

**Ping floods.** A pong carries its ping's payload unchanged. If pings arrive faster than
pongs can be written, only the most recent unanswered ping is answered — RFC 6455 §5.5.3
allows exactly this — so a peer that pings without reading holds at most one pending pong
(≤ 125 bytes) and then runs into the write timeout.

**Keepalive and idleness.** A keepalive ping carries a sequence number; only the pong with
the same payload counts, and at most one ping is outstanding. Unsolicited pongs are ignored
(but, like any frame, they reset the idle clock). Time the connection spends waiting for the
*application* to drain its incoming queue is not counted against the peer's idle or pong
deadline.

### Masking keys and the handshake nonce

Client frames are masked with a fresh 4-byte key per frame, and the handshake carries a
fresh 16-byte nonce. Masking exists to stop a client from being tricked into writing
attacker-chosen bytes onto the wire (cache poisoning of broken intermediaries); it needs
keys an attacker who controls the payload cannot predict. It is **not** encryption — use
`wss` for confidentiality.

What this module guarantees:

- **Nothing is shared between connections.** Each connection has its own key generator with
  its own seed, so the keys one server observes say nothing about the keys used toward
  another.
- **On Unix** (where [`ferroly::hash::random`](hash.md) is supported) the generator is seeded
  with 256 bits from the operating system, and the nonce is 16 separate OS-random bytes.
- **Keys are one-way outputs.** Each block of keys is `SHA-256(tag ‖ seed ‖ counter)`, so
  observing any number of keys does not reveal the seed or other keys.
- **The nonce never exposes the key stream.** It is never taken from a key generator; in the
  fallback below it is derived from a different seed under a different tag.
- **On platforms without an OS randomness source reachable from safe, dependency-free code
  (anything that is not Unix), or if the OS source fails,** the seed is a SHA-256 digest over
  mixed process entropy: wall-clock and monotonic time, stack, heap and static addresses,
  the process id, a counter and several `RandomState` outputs (which the standard library
  keys from OS randomness). This is unpredictable to a remote peer and distinct per
  connection, but it is **not a certified cryptographic source**; do not treat mask keys or
  nonces from such a platform as secrets.

## Backpressure and the slow-consumer policy

Both directions of a connection go through bounded queues.

**Outgoing.** `send` puts the message in a queue of `send_queue_capacity` messages that a
writer drains onto the socket. If the peer reads slowly the queue fills and `send` waits —
this is the backpressure. If no space appears within `send_timeout` the peer is a *slow
consumer*: the connection is closed with code **`1008`**, queued messages are discarded,
`send` returns `WsError::QueueFull`, and the handler's `recv` ends with
`Message::Close { code: 1008, .. }`. If the peer is not reading at all the close frame cannot
be delivered either, and the connection is dropped after `close_timeout` (or `write_timeout`,
whichever comes first).

`try_send` is the non-waiting variant: it returns `QueueFull` and leaves the connection open,
so the caller can drop the message, coalesce, or close.

Memory per connection is therefore bounded by `send_queue_capacity ×` the size of the
messages *you* queue, plus `recv_queue_capacity × max_message_size` for unread input.

**Incoming.** Messages wait for `recv` in a queue of `recv_queue_capacity` messages. When it
is full the connection stops reading from the socket until the application catches up, so a
fast sender is slowed by TCP flow control instead of filling memory. (Pings are not answered
while the application is that far behind.)

## `Hub`: broadcast and rooms

```rust
use ferroly::ws::{server, Hub, Message, WsConn, WsServerConfig};
use tokio::net::TcpListener;

# async fn ex() -> Result<(), ferroly::ws::WsError> {
let hub = Hub::new();
let listener = TcpListener::bind("127.0.0.1:9001").await.unwrap();

let members = hub.clone();
tokio::spawn(server::serve_with_config(
    listener,
    move |mut conn: WsConn| {
        let hub = members.clone();
        async move {
            let _member = hub.member(conn.sender()); // unregisters when the handler returns
            hub.join(conn.id(), "lobby");
            while let Some(Message::Text(text)) = conn.recv().await {
                hub.broadcast_room("lobby", &Message::text(text));
            }
        }
    },
    std::future::pending::<()>(),
    WsServerConfig::default(),
));

// From anywhere else in the application:
hub.broadcast(&Message::text("server restarting in 5 minutes"));
# Ok(())
# }
```

A `Hub` is a cheap-to-clone registry of `WsSender`s keyed by connection id, with named
rooms. It works for server connections and for clients alike.

| Method | Notes |
| --- | --- |
| `Hub::new()` / `Hub::with_policy(policy)` | Default policy: `SlowConsumerPolicy::Close`. |
| `register(sender)` | Adds a connection under `sender.id()`. |
| `member(sender) -> HubMember` | Registers and returns a guard that unregisters on drop. |
| `unregister(id) -> bool` | Removes it from the hub and every room (does not close it). |
| `join(id, room) -> bool` / `leave(id, room) -> bool` | Room membership; empty rooms are deleted. |
| `broadcast(&msg) -> usize` | To every connection; returns how many it was queued for. |
| `broadcast_room(room, &msg) -> usize` | To the members of one room. |
| `send_to(id, msg) -> Result<(), WsError>` | To one connection id. |
| `close_all(code, reason)` | Starts the close handshake on every member and empties the hub. |
| `len()`, `is_empty()`, `contains(id)`, `room_len(room)`, `rooms()` | Introspection. |

**Slow consumers.** A broadcast never waits for one connection — that would delay everyone
else — so it uses `try_send`, and the queue capacity is the whole allowance:

- `SlowConsumerPolicy::Close` (default): a connection whose queue is full is closed with
  **`1008`** and removed from the hub, the same outcome as a `send` that times out.
- `SlowConsumerPolicy::Drop`: the message is skipped for that connection and it stays
  connected — for feeds where the newest value supersedes older ones.

Connections that have closed are pruned as broadcasts encounter them. The message is cloned
once per recipient.

## Observer (metrics)

`ws` does not depend on `ferroly::metrics`. Implement `WsObserver` and bridge the events to
whatever backend you use; set it on `WsOptions::observer` or `WsServerConfig::observer`.

```rust
use std::sync::atomic::{AtomicU64, Ordering};
use ferroly::ws::{WsEvent, WsObserver};

#[derive(Default)]
struct Counters {
    open: AtomicU64,
    bytes_in: AtomicU64,
    protocol_errors: AtomicU64,
}

impl WsObserver for Counters {
    fn on_event(&self, event: &WsEvent) {
        match event {
            WsEvent::Opened { .. } => {
                self.open.fetch_add(1, Ordering::Relaxed);
            }
            WsEvent::Closed { .. } => {
                self.open.fetch_sub(1, Ordering::Relaxed);
            }
            WsEvent::MessageReceived { bytes, .. } => {
                self.bytes_in.fetch_add(*bytes as u64, Ordering::Relaxed);
            }
            WsEvent::ProtocolError { .. } => {
                self.protocol_errors.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}
```

| `WsEvent` | When |
| --- | --- |
| `Opened { conn_id }` | The opening handshake completed. |
| `Closed { conn_id, code }` | The connection ended; `code` matches the final `Message::Close`. |
| `MessageReceived { conn_id, bytes }` | A complete message was reassembled. |
| `MessageSent { conn_id, bytes }` | A message was written to the transport. |
| `PingReceived { conn_id }` | A ping arrived. |
| `PingSent { conn_id }` / `PongReceived { conn_id }` | Keepalive ping written / its pong arrived. |
| `ProtocolError { conn_id, code, reason }` | The peer broke a rule; `code` is `1002`, `1007` or `1009`. `reason` is a fixed string, never peer input. |
| `QueueFull { conn_id }` | A `send` timed out or a `try_send` found the queue full. |
| `TimedOut { conn_id, kind }` | `TimeoutKind::Idle`, `Pong` or `Write`. |
| `HandshakeRejected { status }` | The server refused an upgrade request with this HTTP status. |

`WsEvent` and `TimeoutKind` are `#[non_exhaustive]`: always include a `_ => {}` arm. The
observer is called inline on the connection's task, never with an internal lock held; it must
be fast and must not block.

## Conformance

The RFC 6455 rules enforced. "Close *n*" means the connection is failed: a close frame with
that code is sent and the connection is dropped. Both the server and the client enforce every
frame rule on what they receive.

**Opening handshake — server** (a refused request gets an HTTP error response, never a
`101`):

| Rule | Response |
| --- | --- |
| Request head is well-formed HTTP/1.1 (strict parsing from [`http`](http.md)) and within the head limits | `400` / `414` / `431` / `505` |
| Method is `GET` (§4.2.1) | `405` with `Allow: GET` |
| Version is HTTP/1.1 | `400` |
| `Host` present, not repeated | `400` |
| `Upgrade` contains the token `websocket` (case-insensitive) | `426` with `Upgrade: websocket` |
| `Connection` contains the token `upgrade` | `400` |
| `Sec-WebSocket-Version` is exactly `13` (§4.4) | `426` with `Sec-WebSocket-Version: 13` |
| `Sec-WebSocket-Key` present once and is padded base64 of exactly 16 bytes | `400` |
| No request body (`Content-Length` > 0 or `Transfer-Encoding`) | `400` |
| `Sec-WebSocket-Protocol` values are tokens | `400` |
| `Origin` satisfies the origin policy | `403` |
| The upgrade hook accepts | the hook's response |
| Head not received within `handshake_timeout` | `408` if any byte arrived, else the socket is closed |

The `101` response carries `Upgrade`, `Connection`, the computed `Sec-WebSocket-Accept`, at
most one `Sec-WebSocket-Protocol`, and never `Sec-WebSocket-Extensions`.

**Opening handshake — client:** status `101`; `Upgrade: websocket`; `Connection: upgrade`;
the exact `Sec-WebSocket-Accept`; only an offered subprotocol; no extension (§4.1).

**Frames (§5):**

| Rule | On violation |
| --- | --- |
| Reserved bits RSV1–3 are zero (no extension is negotiated) | Close `1002` |
| Opcode is one of `0x0`, `0x1`, `0x2`, `0x8`, `0x9`, `0xA` | Close `1002` |
| Frames from a client are masked; frames from a server are not (§5.1) | Close `1002` |
| Control frames have FIN set and a payload of at most 125 bytes (§5.5) | Close `1002` |
| A 64-bit length has its top bit clear (§5.2) | Close `1002` |
| A frame is within `max_frame_size` | Close `1009` |

**Messages (§5.4, §8.1):**

| Rule | On violation |
| --- | --- |
| A continuation frame continues a message in progress | Close `1002` |
| No new text/binary frame starts while a message is in progress (control frames may interleave) | Close `1002` |
| A message is within `max_message_size` | Close `1009` |
| A text message is valid UTF-8, checked as fragments arrive and never repaired | Close `1007` |

**Control frames (§5.5):**

| Rule | Behaviour |
| --- | --- |
| A ping is answered with a pong carrying the same payload | Pending pongs are coalesced to the most recent ping. |
| An unsolicited pong is ignored | No reply. |
| A close frame's payload is empty or at least 2 bytes | Close `1002` for a 1-byte payload. |
| A close status code is `1000..=1003`, `1007..=1014` or `3000..=4999` (§7.4) | Close `1002` (so `1004`, `1005`, `1006`, `1015`, `< 1000` and unassigned codes are refused). |
| A close reason is valid UTF-8 | Close `1007` |
| A close frame is answered with a close frame echoing its status code (§5.5.1) | An empty close is answered with an empty close. |
| After the close handshake the transport is closed | The write side is half-closed right after the close frame; the socket is dropped once the peer's close arrives or `close_timeout` passes. |

**Not enforced / not implemented:**

- A length that is not encoded in the minimal number of bytes (§5.2) is accepted.
- Outgoing messages are always sent as a single frame; there is no outgoing fragmentation.
- No extensions: `permessage-deflate` (RFC 7692) is not implemented.
- The client does not follow redirects, answer `401` challenges, or use an HTTP proxy.
- After a protocol error the stream is no longer parsed; the connection lingers until the
  peer closes or `close_timeout` passes, so the close frame is not lost to a TCP reset.

## Error handling

```rust
#[derive(Debug, FerrolyError)]
#[non_exhaustive]
pub enum WsError {
    Connect(String),           // connect / handshake failed
    Rejected { status: u16 },  // the server answered the upgrade with a non-101 status
    Timeout(String),           // "connect", "handshake"
    Send(String),              // other send failure
    Closed,                    // the connection is closed or closing
    QueueFull,                 // the outgoing queue is full
    Protocol(String),          // the caller asked for something RFC 6455 forbids
    Transport(String),         // the underlying transport failed
}
```

| Variant | Raised when |
| --- | --- |
| `Connect` | `dial` failed: bad URL, refused connection, TLS failure, an invalid `101` response, a managed header or malformed subprotocol in `WsOptions`. |
| `Rejected { status }` | `dial` got an HTTP response other than `101` (an auth hook's `401`, the origin policy's `403`, a bad-handshake `4xx`). |
| `Timeout` | `dial` exceeded `connect_timeout` (`"connect"`) or `handshake_timeout` (`"handshake"`). |
| `Closed` | `send` / `try_send` on a connection that is closed or closing; `Hub::send_to` for an unknown id. |
| `QueueFull` | `try_send` found the queue full, or `send` waited `send_timeout` (the connection is then closed with `1008`). |
| `Protocol` | `close` with a status code that may not be sent. |
| `Send`, `Transport` | Kept for source compatibility; this module no longer produces them (a send on a dead connection is `Closed`, and transport failures end the stream with a `1006` close message). |

Failures *during* a connection are not returned as errors from `recv`: they end the stream,
and the final `Message::Close` says why (see the table under [`Message`](#message)).

## Migrating from 0.3

| Before | Now |
| --- | --- |
| `client.send(msg)?` (synchronous, unbounded) | `client.send(msg).await?` (bounded, waits for queue space); `client.try_send(msg)?` for the non-waiting form. |
| `match msg { Message::Text(..) => .., Message::Binary(..) => .. }` | Add an arm for `Message::Close { .. }` (or `other => ..`). `recv` now yields a final `Close` before `None`. The reply-style `serve` callback never receives `Close`. |
| `WsOptions { max_message_size, max_frame_size }` | Add `..WsOptions::default()`; the struct has more fields. |
| `WsOptions::default()` unbounded | Defaults are 16 MiB per frame and 64 MiB per message, plus 10 s connect and handshake timeouts. Set a cap to `None` to remove it. |
| Invalid UTF-8 in text replaced with U+FFFD | The connection is failed with `1007`. |
| Unmasked client frames, RSV bits, oversized or fragmented control frames accepted | The connection is failed with `1002`. |
| Any request with a `Sec-WebSocket-Key` got a `101` | The full handshake is validated; bad requests get an HTTP error. |
| No origin check | `OriginPolicy::SameOrigin` by default: cross-origin **browser** connections get `403` until allow-listed. |
| No timeouts on the server | 120 s idle timeout, 30 s write timeout, 30 s keepalive ping with a 10 s pong deadline (all configurable, all can be disabled). |
| `WsServer::stop` dropped live connections silently | Sends close `1001`, waits up to `drain_timeout`. |
| `WsServer::start` twice replaced the running server | Returns `ComponentAlreadyStarted`. |
| `WsError::Send` when the connection was gone | `WsError::Closed`. |

## Limitations

- **No `permessage-deflate`.** It needs a DEFLATE encoder, which the toolkit does not have
  yet (only an inflater).
- **No upgrade from a `turbo` / `http` route.** The WebSocket server owns its listener; it
  cannot yet share a port and routes with an HTTP server.
- **Single-frame sends.** A message is written as one frame, so a peer with a small frame cap
  will refuse large messages.
- **No message replay** in `ReconnectingClient`; delivery is at most once.
- **Broadcast clones the message** once per recipient.
- **No application-level ping API.** Keepalive pings are automatic; use a message for
  application heartbeats that carry data.

## See also

- [http](http.md) — the transport (`ClientTlsConfig`, `TlsConfig`, strict head parsing) the
  handshake runs over.
- [hash](hash.md) — SHA-1, base64 and the OS randomness source used here.
- [lifecycle](lifecycle.md) — the `Component` contract `WsServer` implements.
- [derive](derive.md) — the `FerrolyError` derive behind `WsError`.

---
**Related:** [http](http.md), [hash](hash.md), [lifecycle](lifecycle.md), [rest](rest.md), [turbo](turbo.md), [derive](derive.md).

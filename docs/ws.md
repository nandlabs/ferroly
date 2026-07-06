# ferroly::ws

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `ws` (implies `http` + `tokio`).

A WebSocket client and server implementing **RFC 6455 entirely in-house**, holding to a
zero-dependency ethos. There is no `tungstenite`, no
`tokio-tungstenite`, no `sha1` crate, no `rand` crate. The frame codec, the opening
handshake (with a from-scratch SHA-1 and base64), payload masking, fragment reassembly, and
the masking-key PRNG are all hand-rolled over [`ferroly::http`](http.md)'s transport and
`tokio`. Because the transport is `ferroly::http`'s, `wss://` reuses the same in-house TLS.

## Overview

The module surface is small and message-oriented. Control frames (ping/pong/close) are
handled internally — pings are answered with pongs automatically, and a peer close ends the
stream — so applications only ever see application-level [`Message`](#message) values.

- **Client:** [`WsClient::dial`](#wsclient) opens a connection; then `send` and `recv`
  exchange messages, decoupled by background read/write tasks so they can be used
  concurrently.
- **Server:** [`server::serve`](#the-server) runs an accept loop, upgrading each connection
  and driving a per-message handler that returns an optional reply.

Public items:

```rust
pub use ferroly::ws::WsClient;   // client connection
pub use ferroly::ws::Message;    // Text | Binary
pub use ferroly::ws::WsOptions;  // client size limits
pub use ferroly::ws::WsError;    // error type
pub mod server;                  // server::serve(...)
```

## Enabling

```toml
[dependencies]
ferroly = { version = "*", features = ["ws"] }
```

## `Message`

The transport-agnostic application message. Control frames never appear here.

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Text(String),   // a UTF-8 text frame
    Binary(Vec<u8>) // a binary frame
}
```

Constructors and accessors:

| Item | Signature | Purpose |
| --- | --- | --- |
| `Message::text(s)` | `fn text(impl Into<String>) -> Message` | Build a `Text` message. |
| `Message::binary(b)` | `fn binary(impl Into<Vec<u8>>) -> Message` | Build a `Binary` message. |
| `Message::as_text()` | `fn as_text(&self) -> Option<&str>` | Borrow the text, or `None` for a binary message. |

```rust
use ferroly::ws::Message;

let a = Message::text("hello");
let b = Message::binary(vec![1, 2, 3]);
let c = Message::Text("also fine".into());   // enum variants are public too

assert_eq!(a.as_text(), Some("hello"));
assert_eq!(b.as_text(), None);
```

Note: inbound text is decoded with `String::from_utf8_lossy`, so invalid UTF-8 in a text
frame is replaced rather than rejected.

## `WsOptions`

Client connection limits. Both fields are **public** with a `Default` of `None`
(unbounded) — construct with a struct literal or `..Default::default()`. There is no
builder.

```rust
#[derive(Debug, Clone, Default)]
pub struct WsOptions {
    pub max_message_size: Option<usize>,  // cap on a reassembled message (None = unbounded)
    pub max_frame_size: Option<usize>,    // cap on a single frame's payload (None = unbounded)
}
```

| Field | Meaning | When exceeded |
| --- | --- | --- |
| `max_message_size` | Maximum size in bytes of a **reassembled** message (all fragments summed). | The connection is closed (the read loop stops). |
| `max_frame_size` | Maximum size in bytes of a **single frame's** payload. | The frame is rejected as `InvalidData` and the connection is closed. |

```rust
use ferroly::ws::WsOptions;

let opts = WsOptions {
    max_message_size: Some(1 << 20),  // 1 MiB reassembled
    max_frame_size:   Some(64 * 1024),
    ..Default::default()
};
```

These limits are part of ws's DoS hardening — see [safety and hardening](#safety-and-hardening).

## `WsClient`

A connected client. Reads and writes are decoupled by background tasks and channels, so
`send` may be called concurrently while `recv` drains incoming messages.

```rust
pub async fn dial(url: &str, opts: WsOptions) -> Result<WsClient, WsError>
pub fn send(&self, msg: Message) -> Result<(), WsError>
pub async fn recv(&mut self) -> Option<Message>
pub async fn close(self) -> Result<(), WsError>
```

| Method | Notes |
| --- | --- |
| `dial(url, opts)` | Connects to a `ws://…` or `wss://…` URL and performs the RFC 6455 opening handshake (see [handshake](#the-opening-handshake)). |
| `send(msg)` | Queues a message for the writer task. Takes `&self` (non-async, non-blocking) — the frame is masked and written by the background writer. Errors with `WsError::Send` if the connection is gone. |
| `recv(&mut self)` | Awaits the next inbound `Message`, or `None` once the connection closes. |
| `close(self)` | Sends a close frame, waits for the writer task to drain, then aborts the reader task. Consumes the client. |

### Quick start (client)

```rust
use ferroly::ws::{WsClient, WsOptions, Message};

#[tokio::main]
async fn main() -> Result<(), ferroly::ws::WsError> {
    let mut client = WsClient::dial("ws://127.0.0.1:9001", WsOptions::default()).await?;

    client.send(Message::text("ping"))?;

    if let Some(reply) = client.recv().await {
        match reply {
            Message::Text(t)   => println!("text: {t}"),
            Message::Binary(b) => println!("binary: {} bytes", b.len()),
        }
    }

    client.close().await?;
    Ok(())
}
```

Because `send` takes `&self` and `recv` takes `&mut self`, a common pattern is to move the
client into a reader task while keeping a clone of the send handle — or simply interleave
`send`/`recv` in one task as above.

## The server

```rust
pub async fn serve<F>(listener: TcpListener, on_message: F) -> Result<(), WsError>
where
    F: Fn(Message) -> Option<Message> + Send + Sync + 'static,
// explicit size caps (a None cap = unbounded):
pub async fn serve_with_options<F>(listener: TcpListener, on_message: F, options: WsOptions)
    -> Result<(), WsError>;
```

`server::serve` takes a bound `tokio::net::TcpListener` and a per-message handler. It loops
accepting connections, sets `TCP_NODELAY`, performs the in-house upgrade handshake on each
(bounded by a 30s handshake timeout), and then for every inbound message calls `on_message(msg)`;
a returned `Some(reply)` is sent back, a `None` sends nothing. Each connection is handled on its
own spawned task (bounded by a connection semaphore), and a transient `accept()` failure backs
off and continues rather than tearing down the listener.

The handler is a **synchronous** `Fn(Message) -> Option<Message>` (not async).

### Quick start (server)

```rust
use ferroly::ws::{server, Message};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), ferroly::ws::WsError> {
    let listener = TcpListener::bind("127.0.0.1:9001").await.unwrap();

    server::serve(listener, |msg| match msg {
        Message::Text(t)   => Some(Message::text(format!("echo: {t}"))),
        Message::Binary(b) => Some(Message::Binary(b)),   // echo bytes back
    })
    .await
}
```

Because the handler is `Fn` + `'static`, keep any shared state behind an
`Arc` and `move` a clone into the closure:

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use ferroly::ws::{server, Message};

let count = Arc::new(AtomicU64::new(0));
let handler = {
    let count = count.clone();
    move |msg: Message| {
        let n = count.fetch_add(1, Ordering::Relaxed) + 1;
        Some(Message::text(format!("message #{n}: {:?}", msg)))
    }
};
// server::serve(listener, handler).await
```

> `server::serve` applies **safe default size caps** — 16 MiB per frame, 64 MiB per
> reassembled message — so a hostile client can't force unbounded reassembly. Use
> `serve_with_options` (or `WsServer::with_options`) to change them; a `None` cap is the
> explicit opt-out to unbounded.

### `WsServer` — a lifecycle component

For graceful start/stop (e.g. under a [`ComponentManager`](lifecycle.md)), use `WsServer`,
which wraps the same accept loop as a [`Component`](lifecycle.md) with a bind address and a
clean shutdown:

```rust
use ferroly::ws::{Message, WsServer};
use ferroly::lifecycle::Component;

# async fn ex() -> Result<(), ferroly::lifecycle::LifecycleError> {
let server = WsServer::new("ws", "127.0.0.1:9001", |msg| match msg {
    Message::Text(t)   => Some(Message::text(format!("echo: {t}"))),
    Message::Binary(b) => Some(Message::Binary(b)),
});
server.start().await?;               // binds and starts accepting
let addr = server.local_addr();      // the bound address (useful with port 0)
// … register it with a ComponentManager, or drive start/stop directly …
server.stop().await?;                // stops accepting, awaits the loop
# Ok(())
# }
```

`start()` binds the address and runs the accept loop; `stop()` signals it to stop accepting and
awaits it. Override the size caps with `WsServer::new(...).with_options(WsOptions { .. })`.
Enabling the `ws` feature also enables `lifecycle`.

## How it works

The implementation is a faithful, minimal RFC 6455. Understanding the moving parts helps
when debugging interop.

### Frames and the codec

Frames are encoded/decoded by hand. Each frame carries a FIN bit, a 4-bit opcode
(`Continuation`, `Text`, `Binary`, `Close`, `Ping`, `Pong`), a mask bit, and a payload
length using the standard 7-bit / 7+16-bit / 7+64-bit encoding. Client→server frames are
masked (mask bit set, 4-byte key XORed over the payload); server→client frames are unmasked.

### The opening handshake

`dial` sends an HTTP/1.1 `Upgrade: websocket` request with a random 16-byte
`Sec-WebSocket-Key` (base64-encoded) and `Sec-WebSocket-Version: 13`. It requires a `101
Switching Protocols` response and verifies the server's `Sec-WebSocket-Accept` equals
`base64(SHA1(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"))`. A mismatch, a non-101 status,
or any I/O failure yields `WsError::Connect`. The server side computes the same accept value
from the client's `Sec-WebSocket-Key` and replies `101`.

Both the SHA-1 (FIPS 180-4) and base64 are implemented from scratch in-house — validated
against the FIPS `SHA1("abc")` vector and the RFC 6455 §1.3 accept-key example.

### Masking key PRNG

Masking keys and the handshake nonce come from a tiny non-cryptographic `xorshift64*` PRNG
seeded from the system clock. Per RFC 6455 these values must be unpredictable-ish but need
not be cryptographically secure, so no `getrandom`/`rand` dependency is pulled in.

### Fragmentation, pings, and close

The read loop reassembles fragmented messages (an initial `Text`/`Binary` frame with
`FIN=0` followed by `Continuation` frames), automatically replies to `Ping` frames with a
`Pong`, ignores inbound `Pong`s, and treats a `Close` frame as end-of-stream (queuing a
close back to the peer). Only fully reassembled `Text`/`Binary` messages are surfaced to the
application.

## Safety and hardening

The read path is written to resist malicious peers, closing a class of frame-length
overflow vulnerabilities at the source:

- **64-bit length overflow guard.** A 64-bit extended payload length with the most
  significant bit set violates RFC 6455 §5.2 and is rejected as `InvalidData` before any
  allocation. This also prevents `usize` overflow on 32-bit targets and absurd allocation
  requests. (There is a regression test feeding a crafted `0x8000_0000_0000_0000` length.)
- **Bounded reads.** Memory grows with bytes *actually received*, never with the
  attacker-*claimed* frame length: reads reserve at most a 64 KiB chunk up front, so a frame
  that merely *claims* a gigantic length cannot force a huge allocation before its bytes
  arrive.
- **Configurable size caps.** [`WsOptions::max_frame_size`](#wsoptions) rejects any single
  oversized frame, and `max_message_size` closes the connection if reassembled fragments
  exceed the cap.

## Error handling

```rust
#[derive(Debug, FerrolyError)]
pub enum WsError {
    Connect(String),    // handshake / upgrade failed
    Send(String),       // sending failed (connection likely closed)
    Transport(String),  // underlying transport failed
}
```

| Variant | Raised when |
| --- | --- |
| `Connect` | `dial`/handshake failure — URL parse error, connect failure, non-101 status, or an invalid/missing `Sec-WebSocket-Accept` / `Sec-WebSocket-Key`. |
| `Send` | `WsClient::send` failed to queue a message because the writer channel is closed (the connection dropped). |
| `Transport` | The server's accept loop hit a listener error. |

`WsError` is produced by the [`FerrolyError`](derive.md) derive, so it implements
`std::error::Error` and `Display`. Note that mid-stream read failures and peer disconnects
are **not** surfaced as errors: the read loop simply ends, and `WsClient::recv` then returns
`None`. Treat `recv() == None` as "connection closed".

## Limitations

- **No async server handler.** `server::serve`'s `on_message` is synchronous; do blocking or
  async work outside the handler (e.g. via channels) if needed.
- **Server size caps are on by default** (16 MiB frame / 64 MiB message) but coarse — tune
  them via `serve_with_options` / `WsServer::with_options`, or terminate behind a proxy for
  finer control.
- **Text is lossy.** Invalid UTF-8 in a text frame is replaced, not rejected.
- **No permessage-deflate / subprotocol negotiation.** Only the core RFC 6455 framing is
  implemented; there is no compression extension or `Sec-WebSocket-Protocol` negotiation.
- **Automatic control frames only.** Pings are answered automatically; there is no API to
  send an application-initiated ping or a close with a status code/reason.

## See also

- [http](http.md) — the transport (`connect`, TLS, request/response head parsing) the ws
  handshake and framing run over.
- [rest](rest.md) / [turbo](turbo.md) — the HTTP framework; ws is a separate protocol layer
  sharing only the transport.
- [derive](derive.md) — the `FerrolyError` derive behind `WsError`.

---
**Related:** [http](http.md), [rest](rest.md), [turbo](turbo.md), [derive](derive.md).

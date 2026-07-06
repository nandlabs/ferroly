# Messaging

**Feature:** `messaging` · **Module:** `ferroly::messaging`

Provider-agnostic messaging with the full local-provider feature set: acknowledgement and redelivery, routing keys, dead-letter queues, consumer concurrency, backpressure, observability, and graceful lifecycle shutdown.

## Overview

The module is built around a small set of traits and one batteries-included backend:

- A [`Message`](#message) is a plain data struct: headers, a byte body, an optional routing key, and a redelivery counter.
- A **[`Handler`](#handler)** consumes a `Message` and **returns an [`Ack`](#ack)** telling the provider what to do with it (keep it, requeue it, or discard it).
- Three traits describe a backend: [`Producer`](#producer) (send), [`Receiver`](#receiver) (add listeners), and [`Provider`](#provider) (the two plus schemes/setup).
- [`LocalProvider`](#localprovider) is an in-process bus over tokio channels implementing all three — competing consumers, routing-key affinity, backpressure, concurrency, ack/redelivery, and a DLQ.
- An [`Observer`](#observer) receives send/receive/ack/nack/dead-letter callbacks; [`LogObserver`](#logobserver) wires those to [`ferroly::log`](log.md).

Addresses are strings of the form `chan://name`; the scheme is stripped down to the destination name (`chan://orders` → `orders`).

### Design notes

A few deliberate shape choices make the module idiomatic and hard to misuse:

| Concern | ferroly's design |
| --- | --- |
| Signalling a message's fate | the handler **returns an [`Ack`](#ack)** — explicit, no interior mutability or hidden side effects |
| Message shape | one [`Message`](#message) struct + the [codec](codec.md) `Value` for headers, not an interface split |
| Addressing | a `&str` address (`chan://name`), scheme stripped to the destination name |
| Cancellation | drop the future — there are no separate context-carrying method variants |
| Constructing messages | inherent constructors: [`Message::new`](#constructors) / `text` / `json` |

Cloud brokers (SQS, Google Pub/Sub) are planned satellite crates that will implement the same [`Provider`](#provider) trait — see the [roadmap](roadmap.md).

## Enabling

The `messaging` feature pulls in [codec](codec.md), [lifecycle](lifecycle.md), and tokio.

```toml
[dependencies]
ferroly = { version = "*", features = ["messaging"] }
```

The optional [`LogObserver`](#logobserver) additionally requires the `log` feature:

```toml
ferroly = { version = "*", features = ["messaging", "log"] }
```

## Quick start

```rust
use ferroly::messaging::{handler, Ack, LocalProvider, Message, Producer, Receiver};

#[tokio::main]
async fn main() {
    let bus = LocalProvider::new("bus");

    bus.add_listener(
        "chan://orders",
        handler(|msg| async move {
            println!("got {}", msg.body_str());
            Ack::Ok
        }),
        Default::default(),
    )
    .await
    .unwrap();

    bus.send("chan://orders", Message::text("order-1"), Default::default())
        .await
        .unwrap();
}
```

## API reference

| Item | Kind | Summary |
| --- | --- | --- |
| [`Message`](#message) | struct | Headers, body, routing key, delivery count |
| [`Ack`](#ack) | enum | `Ok` / `Retry` / `Reject` disposition returned by a handler |
| [`Handler`](#handler) | type alias | `Arc<dyn Fn(Message) -> BoxFuture<'static, Ack> + Send + Sync>` |
| [`handler`](#handler) | fn | Wraps an async closure returning `Ack` |
| [`fallible`](#fallible) | fn | Wraps an async closure returning `Result<(), E>` |
| [`SendOptions`](#sendoptions) | struct | Per-send: `routing_key`, `block_on_full` |
| [`ListenerOptions`](#listeneroptions) | struct | Per-listener: buffer, concurrency, attempts, DLQ |
| [`Producer`](#producer) | trait | `send` |
| [`Receiver`](#receiver) | trait | `add_listener` |
| [`Provider`](#provider) | trait | `Producer + Receiver` + `schemes` / `setup` |
| [`LocalProvider`](#localprovider) | struct | In-process bus over tokio channels |
| [`Observer`](#observer) | trait | `on_send` / `on_receive` / `on_ack` / `on_nack` / `on_dead_letter` |
| [`LogObserver`](#logobserver) | struct | `Observer` that logs via `ferroly::log` (feature `log`) |
| [`MessagingError`](#error-handling) | enum | `NoListeners` / `BufferFull` / `Closed` |
| [`BoxFuture`](#boxfuture) | type alias | `Pin<Box<dyn Future<Output = T> + Send + 'a>>` |

### BoxFuture

```rust
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
```

The object-safe async return type used throughout the traits. You rarely name it directly — the [`handler`](#handler) / [`fallible`](#fallible) helpers box your futures for you, and trait methods return it ready to `.await`.

## Message

```rust
pub struct Message {
    pub headers: Value,               // ferroly::codec::Value
    pub body: Vec<u8>,
    pub routing_key: Option<String>,
    pub delivery_count: u32,
}
```

A message is a plain, `Clone`-able data struct. Every field is public.

- `headers` — arbitrary metadata as a [codec](codec.md) `Value` (typically a `Value::Object`).
- `body` — the raw payload bytes.
- `routing_key` — an optional partition key. When set, every message sharing that key is delivered to the *same* listener (see [routing-key affinity](#competing-consumers--routing-key-affinity)).
- `delivery_count` — how many times this message has been (re)delivered. It starts at `0` and is incremented by the provider on each requeue; a handler can inspect it to detect a redelivered message.

### Constructors

```rust
Message::new(body: impl Into<Vec<u8>>) -> Message   // raw bytes
Message::text(text: impl Into<String>) -> Message   // UTF-8 body
Message::json<T: Encode>(value: &T) -> Message      // JSON-encoded body
```

`Message::json` uses the [codec](codec.md) `Encode` trait (not serde) to render the body as JSON bytes.

```rust
use ferroly::messaging::Message;

#[derive(ferroly::codec::Encode)]
struct Order { id: u32, total: u64 }

let raw  = Message::new(vec![0xDE, 0xAD]);
let text = Message::text("hello");
let json = Message::json(&Order { id: 1, total: 4200 });
```

### Builder methods

Both consume and return `self`, so they chain:

```rust
Message::with_header(self, key: impl Into<String>, value: impl Into<Value>) -> Message
Message::with_routing_key(self, key: impl Into<String>) -> Message
```

`with_header` appends to the `headers` object (promoting `headers` to a `Value::Object` if it was not one already). `with_routing_key` sets the partition key.

```rust
use ferroly::messaging::Message;

let msg = Message::text("ship it")
    .with_header("content-type", "text/plain")
    .with_header("priority", 5u64)
    .with_routing_key("customer-42");
```

### Reading the body

```rust
Message::body_str(&self) -> Cow<'_, str>          // lossy UTF-8 view
Message::decode<T: Decode>(&self) -> Result<T, CodecError>   // JSON body → T
```

`body_str` gives you a lossy UTF-8 view for text payloads. `decode` parses the body as JSON into any [codec](codec.md) `Decode` type.

```rust
use ferroly::messaging::Message;

#[derive(ferroly::codec::Decode)]
struct Order { id: u32, total: u64 }

fn handle(msg: &Message) {
    if let Ok(order) = msg.decode::<Order>() {
        println!("order {} = {}", order.id, order.total);
    } else {
        println!("plain text: {}", msg.body_str());
    }
}
```

## Ack

```rust
pub enum Ack {
    Ok,      // processed successfully; remove the message
    Retry,   // failed transiently; requeue (up to max_delivery_attempts, then DLQ)
    Reject,  // failed permanently; discard (no requeue)
}
```

**This is the central design choice of the module.** A handler is a pure function of its input: it **returns an `Ack`**, rather than mutating the message or flipping an ack/nack flag on it. That keeps the control flow explicit — no interior mutability, no hidden side effects — and makes a handler trivially testable in isolation.

The value you return *is* the disposition, and the provider acts on it:

- `Ack::Ok` — the provider drops the message and fires [`Observer::on_ack`](#observer).
- `Ack::Retry` — the provider increments `delivery_count` and requeues the message to the same destination. Fires `on_nack(requeue = true)` while attempts remain. Once `delivery_count` reaches `max_delivery_attempts` the message is instead routed to the [dead-letter destination](#ackredelivery--dead-lettering) (or dropped if none is configured), and that final nack fires `on_nack(requeue = false)` followed by [`Observer::on_dead_letter`](#observer). The redelivery and dead-letter sends apply backpressure (block on a full buffer) rather than silently dropping under load.
- `Ack::Reject` — the provider discards the message immediately with no requeue. Fires `on_nack(requeue = false)`.

`Ack` is `Copy`, so returning it is free.

```rust
use ferroly::messaging::{handler, Ack, Message};

let h = handler(|msg: Message| async move {
    match validate(&msg) {
        Verdict::Good      => Ack::Ok,
        Verdict::TryLater  => Ack::Retry,   // transient — will be redelivered
        Verdict::Poison    => Ack::Reject,  // permanent — drop now
    }
});
# enum Verdict { Good, TryLater, Poison }
# fn validate(_: &Message) -> Verdict { Verdict::Good }
```

## Handler

```rust
pub type Handler = Arc<dyn Fn(Message) -> BoxFuture<'static, Ack> + Send + Sync>;
```

A `Handler` is a shared, thread-safe async function from `Message` to `Ack`. You almost never construct one by hand — use one of the two adapter functions.

### `handler`

```rust
pub fn handler<F, Fut>(f: F) -> Handler
where
    F: Fn(Message) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Ack> + Send + 'static,
```

Wraps an async closure that returns an [`Ack`](#ack) directly. Use this when you want full control over the disposition.

```rust
use ferroly::messaging::{handler, Ack, Message};

let h = handler(|msg: Message| async move {
    println!("delivery #{}: {}", msg.delivery_count, msg.body_str());
    Ack::Ok
});
```

### `fallible`

```rust
pub fn fallible<F, Fut, E>(f: F) -> Handler
where
    F: Fn(Message) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), E>> + Send + 'static,
```

Wraps an async closure that returns a `Result<(), E>`. The mapping is:

- `Ok(())` → [`Ack::Ok`](#ack)
- `Err(_)` → [`Ack::Retry`](#ack)

The provider's redelivery and dead-lettering then take over. This is the ergonomic choice when your handler body is a chain of `?`-propagating fallible steps and every error should be retried.

```rust
use ferroly::messaging::{fallible, Message};

let h = fallible(|msg: Message| async move {
    let order: Order = msg.decode()?;      // CodecError → Retry
    persist(&order).await?;                // your error   → Retry
    Ok::<(), MyError>(())
});
# #[derive(ferroly::codec::Decode)] struct Order { id: u32 }
# #[derive(Debug)] struct MyError;
# impl From<ferroly::codec::CodecError> for MyError { fn from(_: ferroly::codec::CodecError) -> Self { MyError } }
# async fn persist(_: &Order) -> Result<(), MyError> { Ok(()) }
```

If you need to distinguish permanent failures (`Reject`) from transient ones, use [`handler`](#handler) and match explicitly — `fallible` only knows "succeeded" vs. "retry".

## Options

### SendOptions

```rust
pub struct SendOptions {
    pub routing_key: Option<String>,   // overrides the message's own routing_key
    pub block_on_full: bool,           // wait for buffer room instead of erroring
}
```

Per-`send` overrides. `Default` gives `None` / `false`.

- `routing_key` — takes precedence over `Message::routing_key` when picking the target listener.
- `block_on_full` — when the target listener's buffer is full, `true` awaits room (backpressure), `false` returns [`MessagingError::BufferFull`](#error-handling) immediately. See [backpressure](#backpressure).

### ListenerOptions

```rust
pub struct ListenerOptions {
    pub buffer_size: usize,            // default 256
    pub concurrency: Option<usize>,    // default None (serial, ordered)
    pub max_delivery_attempts: u32,    // default 3
    pub dead_letter: Option<String>,   // default None (drop + log on exhaustion)
}
```

Per-listener configuration passed to [`add_listener`](#receiver). `Default` yields `buffer_size: 256`, `concurrency: None`, `max_delivery_attempts: 3`, `dead_letter: None`.

- `buffer_size` — the tokio channel capacity for this listener; the [backpressure](#backpressure) bound.
- `concurrency` — bounds concurrent handler invocations via a [`Semaphore`](#consumer-concurrency). `None` runs handlers serially (preserving delivery order); `Some(n)` runs up to `n` at once.
- `max_delivery_attempts` — total delivery attempts before a [`Ack::Retry`](#ack) message is dead-lettered.
- `dead_letter` — the destination address that exhausted messages are routed to; `None` drops them (with an `on_nack` observation).

## Producer

```rust
pub trait Producer: Send + Sync {
    fn send(&self, addr: &str, msg: Message, opts: SendOptions)
        -> BoxFuture<'_, Result<(), MessagingError>>;
}
```

Sends a message to an address. The scheme is stripped to the destination name, then a listener is chosen honoring [routing-key affinity](#competing-consumers--routing-key-affinity).

```rust
use ferroly::messaging::{LocalProvider, Message, Producer, SendOptions};

# async fn ex(bus: LocalProvider) {
bus.send(
    "chan://orders",
    Message::text("order-99"),
    SendOptions { routing_key: Some("customer-7".into()), block_on_full: true },
)
.await
.unwrap();
# }
```

## Receiver

```rust
pub trait Receiver: Send + Sync {
    fn add_listener(&self, addr: &str, handler: Handler, opts: ListenerOptions)
        -> BoxFuture<'_, Result<(), MessagingError>>;
}
```

Registers a [`Handler`](#handler) as a listener on an address. Each call spawns a dedicated consumer task and channel; adding multiple listeners to the same destination makes them [competing consumers](#competing-consumers--routing-key-affinity).

## Provider

```rust
pub trait Provider: Producer + Receiver {
    fn schemes(&self) -> &[&str];                             // e.g. ["chan"]
    fn setup(&self) -> BoxFuture<'_, Result<(), MessagingError>>;
}
```

A full backend: both a [`Producer`](#producer) and a [`Receiver`](#receiver), plus the URL `schemes` it handles and a `setup` hook to connect / declare topology. For [`LocalProvider`](#localprovider), `schemes()` returns `["chan"]` and `setup()` is a no-op that returns `Ok(())`.

## LocalProvider

```rust
pub struct LocalProvider { /* … */ }   // Clone

impl LocalProvider {
    pub fn new(id: impl Into<String>) -> Self;
    pub fn set_observer(&self, observer: Arc<dyn Observer>);
}
```

An in-process bus that implements [`Provider`](#provider) over tokio `mpsc` channels using the `chan://` scheme. It is cheap to `Clone` (an `Arc` handle to shared state), so you can hand copies to producers and consumers freely.

- `new(id)` — creates the bus with a component id (used by the [lifecycle](#lifecycle-integration) integration).
- `set_observer(obs)` — installs an [`Observer`](#observer) for send/receive/ack/nack events.

### Competing consumers & routing-key affinity

Every listener on a destination gets its own channel. When a message is sent, the provider picks **one** listener to deliver it to — the listeners *compete* for messages rather than each receiving a copy (this is not fan-out / pub-sub).

Which listener is picked:

- **With a routing key** (from [`SendOptions::routing_key`](#sendoptions), falling back to [`Message::routing_key`](#message)): the key is hashed and taken modulo the listener count, so the same key always lands on the same listener — *routing-key affinity*. This gives you per-key ordering and sticky partitioning.
- **Without a routing key**: listeners are chosen round-robin.

```rust
use ferroly::messaging::{handler, Ack, LocalProvider, Message, Producer, Receiver};

# async fn ex() {
let bus = LocalProvider::new("bus");

// Two competing consumers on the same destination.
for worker in 0..2 {
    bus.add_listener("chan://work", handler(move |m: Message| async move {
        println!("worker {worker} handled {}", m.body_str());
        Ack::Ok
    }), Default::default()).await.unwrap();
}

// Same key → same worker every time.
for i in 0..4 {
    let msg = Message::text(format!("job-{i}")).with_routing_key("tenant-A");
    bus.send("chan://work", msg, Default::default()).await.unwrap();
}
# }
```

### Backpressure

Each listener's channel has capacity [`buffer_size`](#listeneroptions). When it fills, the send behaviour depends on [`SendOptions::block_on_full`](#sendoptions):

- `false` (default) — `send` returns [`MessagingError::BufferFull`](#error-handling) at once (a `try_send`). The caller decides whether to drop, retry, or shed load.
- `true` — `send` awaits until the buffer has room (a real `send`), propagating backpressure to the producer. If the channel is closed while waiting, it returns [`MessagingError::Closed`](#error-handling).

```rust
use ferroly::messaging::{ListenerOptions, Message, Producer, SendOptions, LocalProvider};

# async fn ex(bus: LocalProvider) {
// A tiny buffer to demonstrate backpressure.
# use ferroly::messaging::{handler, Ack, Receiver};
bus.add_listener("chan://slow", handler(|_m| async { Ack::Ok }),
    ListenerOptions { buffer_size: 1, ..Default::default() }).await.unwrap();

// Non-blocking: may return BufferFull.
match bus.send("chan://slow", Message::text("x"), SendOptions::default()).await {
    Err(ferroly::messaging::MessagingError::BufferFull(d)) => eprintln!("dropped, {d} full"),
    other => other.unwrap(),
}

// Blocking: waits for room.
bus.send("chan://slow", Message::text("y"),
    SendOptions { block_on_full: true, ..Default::default() }).await.unwrap();
# }
```

### Consumer concurrency

By default a listener processes messages **serially**, awaiting each handler before pulling the next — this preserves delivery order. Setting [`ListenerOptions::concurrency`](#listeneroptions) to `Some(n)` bounds in-flight handlers with a tokio `Semaphore`: up to `n` messages are processed at once, each on its own spawned task. Order is then no longer guaranteed.

```rust
use ferroly::messaging::{handler, Ack, ListenerOptions, LocalProvider, Message, Receiver};

# async fn ex() {
let bus = LocalProvider::new("bus");
bus.add_listener(
    "chan://io",
    handler(|m: Message| async move {
        do_slow_io(&m).await;
        Ack::Ok
    }),
    ListenerOptions { concurrency: Some(8), ..Default::default() },  // up to 8 at once
)
.await
.unwrap();
# }
# async fn do_slow_io(_: &ferroly::messaging::Message) {}
```

If per-key ordering matters, combine serial or bounded concurrency with a [routing key](#competing-consumers--routing-key-affinity) so related messages stick to one ordered listener.

### Dead-consumer self-healing

Each listener is a spawned task holding the receiving end of its channel. If that task finishes or panics, its channel closes and its sender becomes orphaned. On the next `send`, the provider prunes any listener whose channel is closed **before** picking a target, so a crashed or completed consumer is never selected and its slot is removed from the round-robin / affinity rotation. The bus keeps routing to the survivors with no intervention.

If *every* listener on a destination has died (or none was ever registered), `send` returns [`MessagingError::NoListeners`](#error-handling), letting the producer react rather than blocking on a dead channel.

### Ack/redelivery & dead-lettering

When a handler returns [`Ack::Retry`](#ack):

1. The provider clones the message and increments `delivery_count`.
2. If the new `delivery_count` is **below** `max_delivery_attempts`, it fires `on_nack(requeue = true)` and re-sends the message to the same destination (redelivery).
3. Once `delivery_count` **reaches** `max_delivery_attempts`, it fires `on_nack(requeue = false)`, then:
   - if a [`dead_letter`](#listeneroptions) destination is configured, moves the message there and fires `on_dead_letter(to_dlq = true)`;
   - otherwise drops the message and fires `on_dead_letter(to_dlq = false)` so the loss is observable rather than silent.

Both the redelivery send and the dead-letter move apply backpressure — they block on a full buffer instead of dropping under load — and run on their own detached task, so they can never deadlock a serial consumer that must drain to make room. If a redelivery or DLQ send ultimately fails (destination gone or closed), that too surfaces as `on_dead_letter(to_dlq = false)`.

`Ack::Reject` skips all of this and discards immediately, firing `on_nack(requeue = false)`.

```rust
use ferroly::messaging::{handler, Ack, ListenerOptions, LocalProvider, Message, Producer, Receiver};

#[tokio::main]
async fn main() {
    let bus = LocalProvider::new("bus");

    // A dead-letter sink.
    bus.add_listener("chan://orders.dlq", handler(|m: Message| async move {
        eprintln!("DLQ after {} attempts: {}", m.delivery_count, m.body_str());
        Ack::Ok
    }), Default::default()).await.unwrap();

    // The worker always asks to retry — it will exhaust its attempts and hit the DLQ.
    bus.add_listener(
        "chan://orders",
        handler(|_m: Message| async move { Ack::Retry }),
        ListenerOptions {
            max_delivery_attempts: 3,
            dead_letter: Some("chan://orders.dlq".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    bus.send("chan://orders", Message::text("bad-order"), Default::default())
        .await
        .unwrap();

    // Give the redelivery loop time to run in this example.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    // Prints: DLQ after 3 attempts: bad-order
}
```

## Observer

```rust
pub trait Observer: Send + Sync {
    fn on_send(&self, addr: &str, msg: &Message) { /* no-op */ }
    fn on_receive(&self, addr: &str, msg: &Message) { /* no-op */ }
    fn on_ack(&self, addr: &str, msg: &Message) { /* no-op */ }
    fn on_nack(&self, addr: &str, msg: &Message, requeue: bool) { /* no-op */ }
    fn on_dead_letter(&self, addr: &str, msg: &Message, to_dlq: bool) { /* no-op */ }
}
```

Lifecycle hooks for individual messages. Every method has a default no-op body, so you only override what you care about. Install one with [`LocalProvider::set_observer`](#localprovider). Use it for metrics, tracing, or logging.

- `on_send` — fired just before a message is enqueued to a listener.
- `on_receive` — fired when a consumer pulls a message, before the handler runs.
- `on_ack` — fired after an [`Ack::Ok`](#ack).
- `on_nack` — fired after [`Ack::Retry`](#ack) (`requeue = true` while attempts remain, `requeue = false` on the final attempt) or [`Ack::Reject`](#ack) (`requeue = false`).
- `on_dead_letter` — fired once a message has exhausted its redelivery attempts. `to_dlq` is `true` when it was moved to the configured [`dead_letter`](#listeneroptions) destination, `false` when it was dropped (no DLQ configured, or the requeue / DLQ send itself failed). This is the hook to alarm on: a `to_dlq = false` event means a message was lost.

The table below summarises when each fires and with what arguments:

| Hook | Fired when | Notable args |
| --- | --- | --- |
| `on_send` | a `send` picks a listener and enqueues | `msg.body`, `msg.routing_key` |
| `on_receive` | a consumer pulls a message off its channel | `msg.delivery_count` |
| `on_ack` | handler returned [`Ack::Ok`](#ack) | — |
| `on_nack` | handler returned [`Ack::Retry`](#ack) or [`Ack::Reject`](#ack) | `requeue` |
| `on_dead_letter` | retry attempts exhausted | `to_dlq` (`false` = message lost) |

A counting observer that tracks acks, nacks, and lost messages:

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use ferroly::messaging::{Message, Observer};

#[derive(Default)]
struct Counters { acked: AtomicU64, nacked: AtomicU64, lost: AtomicU64 }

impl Observer for Counters {
    fn on_ack(&self, _addr: &str, _m: &Message) { self.acked.fetch_add(1, Ordering::Relaxed); }
    fn on_nack(&self, _addr: &str, _m: &Message, _requeue: bool) {
        self.nacked.fetch_add(1, Ordering::Relaxed);
    }
    fn on_dead_letter(&self, _addr: &str, _m: &Message, to_dlq: bool) {
        if !to_dlq { self.lost.fetch_add(1, Ordering::Relaxed); } // dropped, not DLQ'd
    }
}

# async fn ex(bus: ferroly::messaging::LocalProvider) {
bus.set_observer(Arc::new(Counters::default()));
# }
```

## LogObserver

**Requires the `log` feature.**

```rust
#[cfg(feature = "log")]
pub struct LogObserver { /* … */ }

impl LogObserver {
    pub fn new(logger: ferroly::log::Logger) -> Self;
}
```

An [`Observer`](#observer) that emits structured logs through [`ferroly::log`](log.md): `debug` on send/receive/ack, and `warn` on nack and dead-letter, with fields such as `addr`, `bytes`, `delivery`, `requeue`, `dead_lettered`, and `dropped`. It is the zero-code way to get message-level visibility — just hand it a [`Logger`](log.md#logger).

```rust
#[cfg(feature = "log")]
# fn ex(bus: ferroly::messaging::LocalProvider, logger: ferroly::log::Logger) {
use std::sync::Arc;
use ferroly::messaging::LogObserver;

bus.set_observer(Arc::new(LogObserver::new(logger)));
# }
```

## Lifecycle integration

`LocalProvider` implements [`ferroly::lifecycle::Component`](lifecycle.md), so it slots into a managed startup/shutdown group:

- `id()` returns the id passed to [`new`](#localprovider).
- `start()` is a no-op (`Ok(())`).
- `stop()` clears the destination table, dropping every listener's sender. That closes each channel; the consumer tasks then drain any buffered messages and exit cleanly. This gives you a graceful shutdown — in-flight and buffered messages are processed, not abandoned.

```rust
use ferroly::lifecycle::Component;
use ferroly::messaging::LocalProvider;

# async fn ex() {
let bus = LocalProvider::new("order-bus");
// … register listeners, run …
bus.stop().await.unwrap();  // drains + closes
# }
```

## Complete example

Listener with an ack handler, a normal send, plus a retry-to-DLQ path:

```rust
use ferroly::messaging::{handler, Ack, ListenerOptions, LocalProvider, Message, Producer, Receiver};

#[derive(ferroly::codec::Decode)]
struct Order { id: u32, total: u64 }

#[tokio::main]
async fn main() {
    let bus = LocalProvider::new("orders-bus");

    // Dead-letter sink.
    bus.add_listener("chan://orders.dlq", handler(|m: Message| async move {
        eprintln!("dead-lettered: {}", m.body_str());
        Ack::Ok
    }), Default::default()).await.unwrap();

    // Main worker: decode, then ack / retry / reject.
    bus.add_listener(
        "chan://orders",
        handler(|msg: Message| async move {
            match msg.decode::<Order>() {
                Ok(order) if order.total == 0 => Ack::Reject,     // permanently bad
                Ok(order) => {
                    println!("processing order {} = {}", order.id, order.total);
                    Ack::Ok
                }
                Err(_) => Ack::Retry,                              // transient/parse — retry
            }
        }),
        ListenerOptions {
            concurrency: Some(4),
            max_delivery_attempts: 3,
            dead_letter: Some("chan://orders.dlq".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // A good order — acked.
    bus.send("chan://orders", Message::json(&Order { id: 1, total: 4200 }), Default::default())
        .await
        .unwrap();

    // A malformed body — retried up to 3 times, then dead-lettered.
    bus.send("chan://orders", Message::text("not json"), Default::default())
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
}
# mod _d { #[derive(ferroly::codec::Encode)] struct Order { id: u32, total: u64 } }
```

## Error handling

```rust
pub enum MessagingError {
    NoListeners(String),   // no listener registered for the destination
    BufferFull(String),    // the listener's buffer was full (non-blocking send)
    Closed(String),        // the destination's channel is closed
}
```

`MessagingError` derives ferroly's error machinery (via `ferroly_derive::FerrolyError`) — it implements `std::error::Error` and `Display`. The `String` payload is the destination name.

- [`send`](#producer) returns `NoListeners` if nothing is listening on the destination, `BufferFull` for a non-blocking send into a full buffer, and `Closed` if the target channel has been closed (e.g. after [`stop`](#lifecycle-integration)).
- Redelivery and dead-letter sends happen inside the provider and cannot be returned to a caller. Rather than being lost silently, a failed requeue or DLQ move is reported through [`Observer::on_dead_letter`](#observer) with `to_dlq = false`. Configure the DLQ listener before you start producing, and watch that hook to catch lost messages.

## Limitations

- **In-process only.** `LocalProvider` lives inside one process; there is no persistence, so messages do not survive a restart. Cloud brokers (SQS, Google Pub/Sub) are [planned satellite crates](roadmap.md) implementing the same [`Provider`](#provider) trait.
- **Competing consumers, not fan-out.** Each message goes to exactly one listener. There is no built-in publish/subscribe broadcast.
- **Redelivery is immediate.** `Ack::Retry` requeues right away — there is no backoff or delay between attempts.
- **DLQ moves can still fail.** If the `dead_letter` destination has no listener (or its buffer is closed), the exhausted message is lost — but the loss is reported via [`Observer::on_dead_letter`](#observer)`(to_dlq = false)` rather than passing silently.
- **Headers are metadata only.** The local provider does not route or filter on headers; only the [routing key](#competing-consumers--routing-key-affinity) affects delivery.

### Design rationale recap

- **Cancellation is by dropping the future** — there are no separate context-carrying method variants to thread through.
- **Headers and body live in one [`Message`](#message) struct**, with the [codec](codec.md) `Value` for headers, rather than a split interface.
- **Messages are built with the [`Message::new` / `text` / `json`](#constructors) constructors.**
- **A handler signals its verdict by returning an [`Ack`](#ack)**, not by mutating the message — explicit and side-effect-free.

## See also

- [codec](codec.md) — `Value`, `Encode`, `Decode` used for headers and bodies.
- [lifecycle](lifecycle.md) — the `Component` trait `LocalProvider` implements.
- [log](log.md) — backs [`LogObserver`](#logobserver).
- [roadmap](roadmap.md) — planned cloud provider satellites.

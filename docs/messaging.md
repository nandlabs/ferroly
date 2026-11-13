# Messaging

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `messaging` · **Module:** `ferroly::messaging`

Provider-agnostic messaging with a full-featured **in-process** provider: queues and publish/subscribe topics, acknowledgement and redelivery with backoff, routing keys, dead-letter queues, TTL and delayed delivery, request/reply, de-duplication, backpressure policies, observability hooks, and a draining shutdown.

## Overview

The module is built around a small set of traits and one batteries-included backend:

- A [`Message`](#message) is a plain data struct: headers, a byte body, an optional routing key, and a redelivery counter. Metadata such as the message id or trace context lives in [conventional headers](#metadata-conventions).
- A **[`Handler`](#handler)** consumes a `Message` and **returns an [`Ack`](#ack)** telling the provider what to do with it (keep it, redeliver it, or discard it).
- Three traits describe a backend: [`Producer`](#producer) (send), [`Receiver`](#receiver) (add listeners), and [`Provider`](#provider) (the two plus schemes/setup/close). They are the [extension point for external brokers](#implementing-a-provider).
- [`LocalProvider`](#localprovider) is an in-process bus implementing all three.
- An [`Observer`](#observer) receives a callback for every step of a message's life; [`LogObserver`](#logobserver) wires those to [`ferroly::log`](log.md).

Addresses are strings with a scheme:

| Address | Semantics |
| --- | --- |
| `chan://name` (or a bare `name`) | A **queue**: listeners are competing consumers — each message goes to exactly one. |
| `topic://name` | **Publish/subscribe**: every matching subscription gets its own copy; subscriptions may use the `*` and `>` wildcards. |
| `reply://…` | A private reply inbox created by [`request`](#requestreply). |

### Delivery guarantee

Read this before relying on the module for anything that matters:

- `LocalProvider` is **in-process and in-memory.** Nothing is written to disk and nothing crosses a process boundary.
- **Within the life of the process, delivery is at-least-once.** A message that `send` accepted is handed to a handler until it is acked, rejected, or dead-lettered. A handler that returns `Retry`, returns an error (via [`fallible`](#fallible)), or **panics** causes a redelivery — not a loss. Because a message can be delivered more than once, handlers should be idempotent.
- **Nothing survives a restart.** A message that has not been completed when the process exits — or crashes — is gone. A graceful [`stop`/`shutdown`](#graceful-shutdown-and-draining) drains what is buffered and hands back whatever it could not finish, but that is a courtesy, not durability.
- Messages are only ever discarded for a reason you configured or can observe: `Ack::Reject`, exhausted attempts with no dead-letter destination, TTL expiry, `OnFull::DropOldest`, de-duplication, a topic with no subscribers, or a shutdown that timed out. Every one of these is reported through the [`Observer`](#observer).

If you need durability, cross-process delivery, or consumer groups across processes, [implement `Provider`](#implementing-a-provider) over a broker. Those are out of scope for the built-in provider.

### Design notes

| Concern | ferroly's design |
| --- | --- |
| Signalling a message's fate | the handler **returns an [`Ack`](#ack)** — explicit, no interior mutability or hidden side effects |
| Message shape | one [`Message`](#message) struct + the [codec](codec.md) `Value` for headers, not an interface split |
| Addressing | a `&str` address (`chan://name`, `topic://name`) |
| Cancellation | drop the future — there are no separate context-carrying method variants |
| Constructing messages | inherent constructors: [`Message::new`](#constructors) / `text` / `json` |
| Rejected sends | the error [hands the message back](#error-handling) |

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
use std::time::Duration;
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

    // Let the consumer finish what is buffered before the process exits.
    let report = bus.shutdown(Duration::from_secs(5)).await;
    assert!(report.is_clean());
}
```

## API reference

| Item | Kind | Summary |
| --- | --- | --- |
| [`Message`](#message) | struct | Headers, body, routing key, delivery count |
| [`headers`](#metadata-conventions) | module | Conventional header keys (`MESSAGE_ID`, `TRACEPARENT`, …) |
| [`new_message_id`](#metadata-conventions) | fn | Generates a process-unique message id |
| [`Ack`](#ack) | enum | `Ok` / `Retry` / `Reject` disposition returned by a handler |
| [`Handler`](#handler) | type alias | `Arc<dyn Fn(Message) -> BoxFuture<'static, Ack> + Send + Sync>` |
| [`handler`](#handler) | fn | Wraps an async closure returning `Ack` |
| [`fallible`](#fallible) | fn | Wraps an async closure returning `Result<(), E>` |
| [`SendOptions`](#sendoptions) | struct | Per-send: routing key, backpressure policy, TTL, delay |
| [`OnFull`](#onfull) | enum | `Reject` / `Block` / `BlockFor` / `DropOldest` |
| [`ListenerOptions`](#listeneroptions) | struct | Per-listener: buffer, concurrency, attempts, DLQ, backoff, expiry, subscription |
| [`Backoff`](#backoff) | struct | Delay between redeliveries |
| [`ExpiryAction`](#listeneroptions) | enum | What happens to an expired message |
| [`LocalOptions`](#localprovider) | struct | Provider-level: drain timeout, de-duplication window |
| [`Producer`](#producer) | trait | `send`, `send_batch` |
| [`Receiver`](#receiver) | trait | `add_listener`, `remove_listeners` |
| [`Provider`](#provider) | trait | `Producer + Receiver` + `schemes` / `setup` / `close` |
| [`ProviderRegistry`](#providerregistry) | struct | Dispatches to providers by address scheme |
| [`LocalProvider`](#localprovider) | struct | In-process provider |
| [`WeakLocalProvider`](#localprovider) | struct | A handle that does not keep the provider alive |
| [`DrainReport`](#graceful-shutdown-and-draining), `Undelivered` | structs | What a shutdown could not deliver |
| [`Observer`](#observer) | trait | Per-message hooks, sufficient for metrics |
| [`DropReason`](#observer) | enum | Why a message was discarded |
| [`LogObserver`](#logobserver) | struct | `Observer` that logs via `ferroly::log` (feature `log`) |
| [`MessagingError`](#error-handling) | enum | Errors; send errors hand the message back |
| [`Rejected`](#error-handling), `BatchFailure` | structs | Error payloads |
| [`topic_matches`](#wildcards), `split_address` | fns | Address helpers |
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

- `headers` — arbitrary metadata as a [codec](codec.md) `Value` (a `Value::Object` once any header is set).
- `body` — the raw payload bytes.
- `routing_key` — an optional partition key. When set, every message sharing that key is delivered to the *same* listener (see [routing-key affinity](#queues-competing-consumers--routing-key-affinity)).
- `delivery_count` — how many times this message has been redelivered. It starts at `0` and is incremented by the provider before each redelivery; a handler can inspect it to detect a redelivered message.

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

### Builder methods and headers

```rust
Message::with_header(self, key: impl Into<String>, value: impl Into<Value>) -> Message
Message::set_header(&mut self, key: impl Into<String>, value: impl Into<Value>)
Message::with_routing_key(self, key: impl Into<String>) -> Message

Message::header(&self, key: &str) -> Option<&Value>
Message::header_str(&self, key: &str) -> Option<&str>
```

`with_header` / `set_header` **replace** an existing header with the same key (promoting `headers` to a `Value::Object` if it was not one already). `header` / `header_str` read one back.

```rust
use ferroly::messaging::Message;

let msg = Message::text("ship it")
    .with_header("content-type", "text/plain")
    .with_header("priority", 5u64)
    .with_routing_key("customer-42");
assert_eq!(msg.header_str("content-type"), Some("text/plain"));
```

### Metadata conventions

Well-known metadata is carried in headers under fixed keys, exposed as constants in `ferroly::messaging::headers` and through typed accessors on `Message`. They are conventions, not separate fields — so they travel through any provider that carries headers, and an adapter for an external broker can map them to that broker's native attributes.

| Constant | Key | Value | Setter / getter |
| --- | --- | --- | --- |
| `headers::MESSAGE_ID` | `message-id` | string | `with_id`, `with_new_id` / `id` |
| `headers::CORRELATION_ID` | `correlation-id` | string | `with_correlation_id` / `correlation_id` |
| `headers::TIMESTAMP` | `timestamp` | unsigned integer: milliseconds since the Unix epoch | `with_timestamp(SystemTime)` / `timestamp` |
| `headers::REPLY_TO` | `reply-to` | string: an address | `with_reply_to` / `reply_to` |
| `headers::TRACEPARENT` | `traceparent` | string: W3C Trace Context | `with_traceparent` / `traceparent` |
| `headers::TRACESTATE` | `tracestate` | string: W3C Trace Context | `with_tracestate` / `tracestate` |
| `headers::TOPIC` | `topic` | string: the concrete topic a copy was published to | set by the provider / `topic` |

Helpers that combine them:

- `stamped(self)` — fills in a generated id and the current timestamp where they are missing.
- `in_reply_to(self, &request)` — marks a message as a reply: the correlation id becomes the request's correlation id (or its message id), and the request's trace context is carried over.
- `with_trace_from(self, &parent)` — copies `traceparent`/`tracestate` from the message being handled, unless this one already has a `traceparent`.
- `new_message_id() -> String` — 32 hex characters, unique within the process and unlikely to collide across processes. It is an identifier, not a secret.

```rust
use ferroly::messaging::{headers, Message};

let msg = Message::text("payload")
    .stamped()                                   // message-id + timestamp
    .with_correlation_id("checkout-7781")
    .with_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01");

assert!(msg.id().is_some());
assert_eq!(msg.header_str(headers::CORRELATION_ID), Some("checkout-7781"));
```

**Trace propagation.** The module does not create spans and does not depend on `obs`. The convention is: when you send, copy the active span's W3C `traceparent` (and `tracestate`) into the message with `with_traceparent`; when you handle a message, start your consumer span from `msg.traceparent()`; and when a handler produces a follow-up message, call `with_trace_from(&msg)` so the trace continues.

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
    Retry,   // failed transiently; redeliver after a backoff (then dead-letter)
    Reject,  // failed permanently; discard (no redelivery, no dead-lettering)
}
```

**This is the central design choice of the module.** A handler is a pure function of its input: it **returns an `Ack`**, rather than mutating the message or flipping an ack/nack flag on it. That keeps the control flow explicit — no interior mutability, no hidden side effects — and makes a handler trivially testable in isolation.

The value you return *is* the disposition, and the provider acts on it:

- `Ack::Ok` — the provider drops the message and fires [`Observer::on_ack`](#observer).
- `Ack::Retry` — the provider increments `delivery_count` and, while attempts remain, redelivers the message to the same listener after the [retry backoff](#backoff), firing `on_nack(requeue = true)`. Once `delivery_count` reaches `max_delivery_attempts` the message is instead routed to the [dead-letter destination](#ackredelivery--dead-lettering) (or dropped if none is configured); that final nack fires `on_nack(requeue = false)` followed by [`Observer::on_dead_letter`](#observer).
- `Ack::Reject` — the provider discards the message immediately. Fires `on_nack(requeue = false)`.

**A handler that panics is treated as having returned `Ack::Retry`.** The panic is contained, reported through [`Observer::on_handler_panic`](#observer), and the consumer keeps running.

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

A `Handler` is a shared, thread-safe async function from `Message` to `Ack`. You almost never construct one by hand — use one of the adapter functions (or [`LocalProvider::responder`](#requestreply) for request/reply).

Each invocation runs on its own task. That is what contains a panic; it also means a handler must not rely on task-local state of the task that registered it, and that a handler still running when a shutdown times out is cancelled at its next `.await`.

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
    pub block_on_full: bool,           // shorthand for on_full = Some(OnFull::Block)
    pub on_full: Option<OnFull>,       // backpressure policy
    pub ttl: Option<Duration>,         // time to live
    pub delay: Option<Duration>,       // delayed delivery
}
```

Per-`send` options. `Default` gives no routing-key override, [`OnFull::Reject`](#onfull), no TTL, no delay. Build one with a struct literal and `..Default::default()`, or fluently:

```rust
use std::time::Duration;
use ferroly::messaging::{OnFull, SendOptions};

let opts = SendOptions::new()
    .routing_key("customer-7")
    .on_full(OnFull::BlockFor(Duration::from_secs(1)))
    .ttl(Duration::from_secs(30))
    .delay(Duration::from_secs(5));
```

- `routing_key` — takes precedence over `Message::routing_key` when picking the target listener.
- `on_full` — what to do when the target listener's buffer is full; see [`OnFull`](#onfull). `block_on_full: true` is the older spelling of `OnFull::Block` and is ignored when `on_full` is set. `full_policy()` returns the effective policy.
- `ttl` — [time to live](#ttl-and-expiry), counted from the moment the message is accepted.
- `delay` — [delayed delivery](#delayed-delivery): the message becomes visible to its listener only after this long.

### OnFull

```rust
pub enum OnFull {
    Reject,              // default: fail at once with BufferFull, message handed back
    Block,               // wait for room (unbounded backpressure)
    BlockFor(Duration),  // wait up to this long, then SendTimeout, message handed back
    DropOldest,          // evict the oldest buffered message to make room
}
```

See [Backpressure](#backpressure).

### ListenerOptions

```rust
pub struct ListenerOptions {
    pub buffer_size: usize,            // default 256; must be >= 1
    pub concurrency: Option<usize>,    // default None (serial, ordered)
    pub max_delivery_attempts: u32,    // default 3
    pub dead_letter: Option<String>,   // default None
    pub retry_backoff: Backoff,        // default 100 ms doubling up to 30 s
    pub on_expired: ExpiryAction,      // default ExpiryAction::DeadLetter
    pub subscription: Option<String>,  // topic:// only; default None
}
```

Per-listener configuration passed to [`add_listener`](#receiver). Use `..Default::default()` for the fields you do not set.

- `buffer_size` — the capacity of this listener's buffer; the [backpressure](#backpressure) bound. **`0` is rejected** with [`MessagingError::InvalidOptions`](#error-handling) (it does not panic).
- `concurrency` — bounds concurrent handler invocations. `None` runs handlers serially (preserving order); `Some(n)` runs up to `n` at once. `Some(0)` is treated as `Some(1)`.
- `max_delivery_attempts` — total delivery attempts before a [`Ack::Retry`](#ack) message is dead-lettered.
- `dead_letter` — the destination address that exhausted (and, by default, expired) messages are routed to; `None` drops them and reports the drop. It may be a `chan://` or `topic://` address, and may not point back at the listener's own queue (`InvalidOptions`).
- `retry_backoff` — the delay between redeliveries; see [`Backoff`](#backoff).
- `on_expired` — `ExpiryAction::DeadLetter` (move an expired message to `dead_letter`, or drop it if none is configured) or `ExpiryAction::Drop`.
- `subscription` — for `topic://` listeners, the name of a [shared subscription](#publishsubscribe-topics).

### Backoff

```rust
pub struct Backoff {
    pub initial: Duration,   // delay before the first redelivery
    pub max: Duration,       // upper bound
    pub multiplier: u32,     // growth factor per redelivery (0 is treated as 1)
}
```

The delay before redelivery *n* (1-based) is `initial * multiplier^(n-1)`, capped at `max`.

| Constructor | Meaning |
| --- | --- |
| `Backoff::default()` | 100 ms, doubling, capped at 30 s |
| `Backoff::exponential(initial, max)` | doubling from `initial` up to `max` |
| `Backoff::fixed(delay)` | the same delay every time |
| `Backoff::none()` | no delay (the consumer still yields to the scheduler between attempts, so it cannot spin) |

`delay(n)` returns the delay before redelivery `n`.

## Producer

```rust
pub trait Producer: Send + Sync {
    fn send(&self, addr: &str, msg: Message, opts: SendOptions)
        -> BoxFuture<'_, Result<(), MessagingError>>;

    // Defaulted: one `send` per message, stopping at the first failure.
    fn send_batch(&self, addr: &str, msgs: Vec<Message>, opts: SendOptions)
        -> BoxFuture<'_, Result<(), MessagingError>>;
}
```

`send` delivers one message to an address. If the message cannot be accepted, the error [hands it back](#error-handling).

```rust
use ferroly::messaging::{LocalProvider, Message, Producer, SendOptions};

# async fn ex(bus: LocalProvider) {
bus.send(
    "chan://orders",
    Message::text("order-99"),
    SendOptions::new().routing_key("customer-7"),
)
.await
.unwrap();
# }
```

### Batch send

`send_batch` sends several messages to one address in order, applying the same options to each. It is **not atomic**: it stops at the first failure and returns `MessagingError::Batch(BatchFailure)`:

```rust
pub struct BatchFailure {
    pub sent: usize,                 // accepted before the failure
    pub error: Box<MessagingError>,  // why it stopped
    pub unsent: Vec<Message>,        // the failed message first, then the rest
}
```

```rust
use ferroly::messaging::{LocalProvider, Message, MessagingError, Producer};

# async fn ex(bus: LocalProvider) {
let batch = vec![Message::text("a"), Message::text("b"), Message::text("c")];
match bus.send_batch("chan://orders", batch, Default::default()).await {
    Ok(()) => {}
    Err(MessagingError::Batch(failure)) => {
        eprintln!("{} sent, {} to retry: {}", failure.sent, failure.unsent.len(), failure.error);
        // `failure.unsent` is yours again — retry or park it.
    }
    Err(other) => eprintln!("{other}"),
}
# }
```

## Receiver

```rust
pub trait Receiver: Send + Sync {
    fn add_listener(&self, addr: &str, handler: Handler, opts: ListenerOptions)
        -> BoxFuture<'_, Result<(), MessagingError>>;

    // Defaulted: returns MessagingError::Unsupported.
    fn remove_listeners(&self, addr: &str)
        -> BoxFuture<'_, Result<usize, MessagingError>>;
}
```

`add_listener` registers a [`Handler`](#handler) on an address. With `LocalProvider` each call creates a dedicated buffer and consumer task; the returned future must be awaited for the listener to be registered.

`remove_listeners` removes every listener on an address and returns how many were removed. With `LocalProvider`, removed listeners take no new messages but **finish the backlog already buffered for them**. For `topic://`, the address is matched against subscription patterns literally (`topic://orders.*` removes the subscriptions registered with exactly that pattern).

## Provider

```rust
pub trait Provider: Producer + Receiver {
    fn schemes(&self) -> &[&str];                             // e.g. ["chan"]
    fn setup(&self) -> BoxFuture<'_, Result<(), MessagingError>>;

    // Defaulted: does nothing.
    fn close(&self) -> BoxFuture<'_, Result<(), MessagingError>>;
}
```

A full backend: both a [`Producer`](#producer) and a [`Receiver`](#receiver), plus the URL `schemes` it handles, a `setup` hook to connect / declare topology, and a `close` hook for a graceful shutdown.

For [`LocalProvider`](#localprovider): `schemes()` returns `["chan", "topic", "reply"]`, `setup()` is a no-op, and `close()` is a [draining shutdown](#graceful-shutdown-and-draining) that fails with `MessagingError::Undelivered(n)` if messages were left over.

### Implementing a provider

`Provider` is the extension point for external brokers — Kafka, NATS, SQS, Google Pub/Sub. No such backend ships with ferroly; **durable backends are out of scope for this crate** and belong in adapter crates that implement these traits. Application code written against `Arc<dyn Provider>` (or a [`ProviderRegistry`](#providerregistry)) then does not change when the backend does.

The traits are object-safe (every method returns a [`BoxFuture`](#boxfuture)), and only four methods are required: `send`, `add_listener`, `schemes` and `setup`. What an adapter is expected to do:

- **Addresses.** Claim one or more schemes in `schemes()` (`"kafka"`, `"nats"`, `"sqs"`); use `split_address(addr)` to separate the scheme from the destination name.
- **Sending.** Map `Message::body`, `Message::routing_key` (partition / group key) and the [header conventions](#metadata-conventions) onto the broker's native fields. Honour `SendOptions` where the broker can (`ttl`, `delay`) and ignore what it cannot. Report transport and protocol failures with `MessagingError::backend(err)`. Where it still holds the message, hand it back in the error.
- **Receiving.** Drive the `Handler` and translate its `Ack` into the broker's acknowledgement: `Ok` → ack/commit, `Retry` → nack/redeliver (bounded by `max_delivery_attempts`, then `dead_letter`), `Reject` → discard. Set `Message::delivery_count` from the broker's redelivery counter. Contain handler panics and treat them as `Retry`.
- **Lifecycle.** Connect and declare topology in `setup`; flush, drain and disconnect in `close`. Override `send_batch` if the broker has a native batch call, and `remove_listeners` if it can unsubscribe.

The semantics a backend actually provides — durability, ordering, delivery guarantee — are the backend's; the trait does not promise more than the underlying broker does.

```rust
use ferroly::messaging::{
    BoxFuture, Handler, ListenerOptions, Message, MessagingError, Producer, Provider, Receiver,
    SendOptions,
};

struct NatsProvider { /* client handle */ }

impl Producer for NatsProvider {
    fn send(&self, addr: &str, msg: Message, _opts: SendOptions)
        -> BoxFuture<'_, Result<(), MessagingError>>
    {
        let subject = ferroly::messaging::split_address(addr).1.to_string();
        Box::pin(async move {
            self.publish(&subject, &msg).await.map_err(MessagingError::backend)
        })
    }
}

impl Receiver for NatsProvider {
    fn add_listener(&self, addr: &str, handler: Handler, opts: ListenerOptions)
        -> BoxFuture<'_, Result<(), MessagingError>>
    {
        let subject = ferroly::messaging::split_address(addr).1.to_string();
        Box::pin(async move { self.subscribe(subject, handler, opts).await })
    }
}

impl Provider for NatsProvider {
    fn schemes(&self) -> &[&str] { &["nats"] }
    fn setup(&self) -> BoxFuture<'_, Result<(), MessagingError>> { Box::pin(async { Ok(()) }) }
}
# impl NatsProvider {
#     async fn publish(&self, _: &str, _: &Message) -> Result<(), std::io::Error> { Ok(()) }
#     async fn subscribe(&self, _: String, _: Handler, _: ListenerOptions) -> Result<(), MessagingError> { Ok(()) }
# }
```

### ProviderRegistry

Routes sends and listeners to the provider that owns an address's scheme, so one handle can front several backends. It implements `Producer` and `Receiver` itself.

```rust
use std::sync::Arc;
use ferroly::messaging::{
    handler, Ack, LocalProvider, Message, MessagingError, Producer, ProviderRegistry, Receiver,
};

# async fn ex() -> Result<(), MessagingError> {
let registry = ProviderRegistry::new();
registry.register(Arc::new(LocalProvider::new("local")))?;   // chan://, topic://, reply://
// registry.register(Arc::new(nats_provider))?;              // nats://

registry
    .add_listener("chan://orders", handler(|_m| async { Ack::Ok }), Default::default())
    .await?;
registry
    .send("chan://orders", Message::text("o-1"), Default::default())
    .await?;
# Ok(())
# }
```

| Method | Description |
| --- | --- |
| `register(&self, Arc<dyn Provider>) -> Result<(), MessagingError>` | Register a provider for every scheme it reports; `InvalidOptions` (registering nothing) if a scheme is already taken. |
| `provider_for(&self, addr) -> Result<Arc<dyn Provider>, MessagingError>` | The provider for an address; `InvalidAddress` if the address has no scheme or an unregistered one. |
| `schemes(&self) -> Vec<String>` | The registered schemes, sorted. |

Unlike `LocalProvider` on its own, the registry needs a scheme on every address.

## LocalProvider

```rust
pub struct LocalProvider { /* … */ }   // Clone

impl LocalProvider {
    pub fn new(id: impl Into<String>) -> Self;
    pub fn with_options(id: impl Into<String>, options: LocalOptions) -> Self;
    pub fn options(&self) -> &LocalOptions;
    pub fn set_observer(&self, observer: Arc<dyn Observer>);
    pub fn downgrade(&self) -> WeakLocalProvider;

    pub fn pending(&self) -> usize;
    pub fn queue_depth(&self, addr: &str) -> Option<usize>;

    pub async fn shutdown(&self, drain_timeout: Duration) -> DrainReport;

    pub async fn request(&self, addr: &str, msg: Message, timeout: Duration)
        -> Result<Message, MessagingError>;
    pub async fn reply(&self, request: &Message, reply: Message) -> Result<(), MessagingError>;
    pub fn responder<F, Fut>(&self, f: F) -> Handler;   // F: Fn(Message) -> Future<Output = Result<Message, Ack>>
}

pub struct LocalOptions {
    pub drain_timeout: Duration,   // default 30 s — used by `stop` and `close`
    pub dedup_window: usize,       // default 0 (de-duplication off)
}
```

An in-process bus that implements [`Provider`](#provider) over bounded in-memory buffers. It is cheap to `Clone` (an `Arc` handle to shared state), so you can hand copies to producers freely.

- `new(id)` / `with_options(id, options)` — creates the bus with a component id (used by the [lifecycle](#lifecycle-integration) integration).
- `set_observer(obs)` — installs an [`Observer`](#observer).
- `pending()` — messages accepted and not yet completed, across all destinations (buffered, delayed, or being handled).
- `queue_depth(addr)` — messages buffered for an address (not counting ones being handled), or `None` if nothing listens there.
- `downgrade()` — a `WeakLocalProvider` (with `upgrade() -> Option<LocalProvider>`) that does not keep the bus alive.

### Handle lifetime

Consumer tasks hold the provider only **weakly**. When the last `LocalProvider` handle is dropped, every consumer task ends: in-flight handlers are cancelled and whatever was still buffered is reported through [`Observer::on_dropped`](#observer) with `DropReason::Shutdown`. Dropping is abortive — call [`shutdown`](#graceful-shutdown-and-draining) (or `stop`) first to drain.

One consequence: **a handler must not capture a `LocalProvider` clone**, or the handler (owned by a consumer task) would keep the provider alive forever. A handler that needs to send captures a weak handle instead:

```rust
use ferroly::messaging::{handler, Ack, LocalProvider, Message, Producer, Receiver};

# async fn ex(bus: LocalProvider) {
let weak = bus.downgrade();
bus.add_listener("chan://orders", handler(move |msg: Message| {
    let weak = weak.clone();
    async move {
        let Some(bus) = weak.upgrade() else { return Ack::Retry };
        let receipt = Message::text("receipt").with_trace_from(&msg);
        match bus.send("chan://receipts", receipt, Default::default()).await {
            Ok(()) => Ack::Ok,
            Err(_) => Ack::Retry,
        }
    }
}), Default::default()).await.unwrap();
# }
```

### Queues: competing consumers & routing-key affinity

Every listener on a `chan://` destination gets its own buffer. When a message is sent, the provider picks **one** listener to deliver it to — the listeners *compete* for messages rather than each receiving a copy. (For a copy per listener, use a [topic](#publishsubscribe-topics).)

Which listener is picked:

- **With a routing key** (from [`SendOptions::routing_key`](#sendoptions), falling back to [`Message::routing_key`](#message)): the key is hashed and taken modulo the listener count, so the same key always lands on the same listener — *routing-key affinity*.
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

If nothing listens on a queue, `send` returns [`MessagingError::NoListeners`](#error-handling) with the message handed back.

### Publish/subscribe: topics

A `topic://` address fans out: **every subscription whose pattern matches the topic receives its own copy** of each message.

```rust
use ferroly::messaging::{handler, Ack, LocalProvider, Message, Producer, Receiver};

# async fn ex() {
let bus = LocalProvider::new("bus");

// Two independent subscriptions: both receive every order event.
bus.add_listener("topic://orders.created", handler(|m: Message| async move {
    println!("billing saw {}", m.body_str());
    Ack::Ok
}), Default::default()).await.unwrap();
bus.add_listener("topic://orders.*", handler(|m: Message| async move {
    println!("audit saw {} on {}", m.body_str(), m.topic().unwrap_or("?"));
    Ack::Ok
}), Default::default()).await.unwrap();

bus.send("topic://orders.created", Message::text("o-1"), Default::default())
    .await
    .unwrap();
# }
```

How subscriptions work:

- **Each `add_listener` on a topic is its own subscription** with its own buffer, consumer, retry counter and dead-letter routing. One subscriber failing or falling behind does not affect the others.
- **Shared subscriptions.** Listeners that set the same [`ListenerOptions::subscription`](#listeneroptions) name on the same pattern form *one* subscription and compete for its messages (with routing-key affinity, as for a queue) — an in-process consumer group. A listener with no name is always a subscription of its own.
- **The concrete topic** is stamped on every copy in the [`topic` header](#metadata-conventions) (`msg.topic()`), so a wildcard subscriber can tell which topic matched.
- **No subscribers is not an error.** Publishing to a topic nothing subscribes to returns `Ok(())`; the message is reported through `on_dropped(…, DropReason::NoSubscribers)`.
- **Backpressure is per subscription.** With `OnFull::Reject` (the default) or `BlockFor`, a subscription whose buffer is full *misses* that message — reported through `on_dropped(…, DropReason::BufferFull)` — while the others still receive it, and `send` returns `Ok(())`. Use `OnFull::Block` if every subscription must get every message.
- Subscriptions only see messages published **after** they were added; there is no replay.

#### Wildcards

Topics are dot-separated tokens (`orders.eu.created`). In a subscription pattern:

| Token | Matches | Example |
| --- | --- | --- |
| `*` | exactly one token | `orders.*` matches `orders.created`, not `orders.eu.created` |
| `>` | one or more remaining tokens; only valid as the last token | `orders.>` matches `orders.created` and `orders.eu.created`, not `orders` |
| anything else | itself, literally | `orders.created` |

Wildcards belong in subscriptions only: publishing to `topic://orders.*` is `MessagingError::InvalidAddress`, as is subscribing with an empty token or a `>` that is not last. `topic_matches(pattern, topic)` exposes the matching rule.

### Backpressure

Each listener's buffer has capacity [`buffer_size`](#listeneroptions). When it is full, the send behaviour is set by [`SendOptions::on_full`](#onfull):

| Policy | Behaviour when the buffer is full |
| --- | --- |
| `OnFull::Reject` (default) | `send` returns [`MessagingError::BufferFull`](#error-handling) at once, **with the message handed back**. The caller decides whether to retry, park or shed. |
| `OnFull::Block` | `send` waits until there is room, propagating backpressure to the producer. |
| `OnFull::BlockFor(d)` | `send` waits up to `d`, then returns `MessagingError::SendTimeout` with the message handed back. |
| `OnFull::DropOldest` | The oldest buffered message is evicted to make room and reported through `on_dropped(…, DropReason::Evicted)`. Never blocks, never fails for lack of room. |

A waiting send returns [`MessagingError::Closed`](#error-handling) (message handed back) if the listener goes away or the provider stops while it waits.

```rust
use std::time::Duration;
use ferroly::messaging::{LocalProvider, Message, MessagingError, OnFull, Producer, SendOptions};

# async fn ex(bus: LocalProvider) {
// Non-blocking: get the message back if there is no room.
match bus.send("chan://slow", Message::text("x"), SendOptions::default()).await {
    Err(MessagingError::BufferFull(rejected)) => {
        let msg = rejected.into_message().expect("handed back");
        eprintln!("no room for {}", msg.body_str());
    }
    other => other.unwrap(),
}

// Wait at most a second for room.
let opts = SendOptions::new().on_full(OnFull::BlockFor(Duration::from_secs(1)));
let _ = bus.send("chan://slow", Message::text("y"), opts).await;

// Telemetry-style: keep the newest, drop the oldest.
let opts = SendOptions::new().on_full(OnFull::DropOldest);
bus.send("chan://slow", Message::text("z"), opts).await.unwrap();
# }
```

### Consumer concurrency

By default a listener processes messages **serially**: it does not take the next message until the current one is done. Setting [`ListenerOptions::concurrency`](#listeneroptions) to `Some(n)` lets up to `n` messages be processed at once, each on its own task; order is then not guaranteed.

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

### Ordering guarantee

Stated precisely:

- **A serial listener** (`concurrency: None`) handles the messages placed in its buffer **in the order they were accepted**, one at a time.
- **A retried message keeps its place.** When a handler returns `Retry` (or panics), the listener waits out the backoff and redelivers *that same message* before it takes anything else. A later message never overtakes an earlier one on the same listener. The cost is head-of-line blocking: while a message is backing off, the listener handles nothing else.
- **Per-key order across competing listeners.** Messages with the same routing key go to the same listener, so with serial listeners they are handled in send order — retries included — **as long as the destination's set of listeners does not change**. Adding or removing a listener changes the key-to-listener mapping; messages already buffered stay where they are, so around such a change two messages with the same key can be handled by different listeners, in either order.
- **No guarantee** for: messages *without* a routing key on a destination with several listeners (they are spread round-robin); listeners with `concurrency: Some(n)` where `n > 1`; and messages sent concurrently from several tasks (their acceptance order is whatever order the sends complete in).
- **Deliberate reordering:** a [delayed](#delayed-delivery) message is ordered by when it becomes due, not when it was sent; `OnFull::DropOldest` removes the oldest message; a dead-lettered or expired message leaves its queue.

### Ack/redelivery & dead-lettering

When a handler returns [`Ack::Retry`](#ack) (or panics):

1. The provider increments `delivery_count`.
2. If the new `delivery_count` is **below** `max_delivery_attempts`, it fires `on_nack(requeue = true)`, waits for [`retry_backoff`](#backoff)`.delay(delivery_count)`, and redelivers the message to the same listener.
3. Once `delivery_count` **reaches** `max_delivery_attempts`, it fires `on_nack(requeue = false)`, then:
   - if a [`dead_letter`](#listeneroptions) destination is configured, moves the message there and fires `on_dead_letter(to_dlq = true)`;
   - otherwise drops the message and fires `on_dead_letter(to_dlq = false)` so the loss is observable rather than silent.

The dead-letter move waits for room in the destination's buffer rather than dropping under load. If the dead-letter destination has no listener, the message is lost and that is reported as `on_dead_letter(to_dlq = false)`. The message keeps its `delivery_count` when it arrives in the dead-letter queue, so the handler there can see how many attempts were made.

`Ack::Reject` skips all of this and discards immediately, firing `on_nack(requeue = false)`.

```rust
use std::time::Duration;
use ferroly::messaging::{
    handler, Ack, Backoff, ListenerOptions, LocalProvider, Message, Producer, Receiver,
};

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
            retry_backoff: Backoff::exponential(Duration::from_millis(50), Duration::from_secs(1)),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    bus.send("chan://orders", Message::text("bad-order"), Default::default())
        .await
        .unwrap();

    // Drain: returns once the retries (50 ms, then 100 ms) and the DLQ handler are done.
    bus.shutdown(Duration::from_secs(5)).await;
    // Prints: DLQ after 3 attempts: bad-order
}
```

### TTL and expiry

[`SendOptions::ttl`](#sendoptions) gives a message a time to live, counted from the moment `send` accepted it. Expiry is checked each time the message is about to be handed to a handler (including before a redelivery): if its TTL has run out, the handler is **not** called. `Observer::on_expired` fires, and then, per the listener's [`on_expired`](#listeneroptions):

- `ExpiryAction::DeadLetter` (default) — the message is moved to the listener's `dead_letter` destination (`on_dead_letter(to_dlq = true)`); if none is configured it is dropped (`on_dropped(…, DropReason::Expired)`).
- `ExpiryAction::Drop` — the message is dropped (`on_dropped(…, DropReason::Expired)`).

A message that is already inside a handler when its TTL passes is not interrupted. A dead-lettered message does not carry its TTL into the dead-letter queue.

### Delayed delivery

[`SendOptions::delay`](#sendoptions) holds a message back: it is accepted (and counts against the buffer and towards `pending()`) immediately, but its listener only sees it once the delay has passed. Delayed messages are delivered in the order they become due.

```rust
use std::time::Duration;
use ferroly::messaging::{LocalProvider, Message, Producer, SendOptions};

# async fn ex(bus: LocalProvider) {
bus.send(
    "chan://reminders",
    Message::text("follow up"),
    SendOptions::new().delay(Duration::from_secs(60)),
)
.await
.unwrap();
# }
```

The delay lives in memory only: a delayed message does not survive a restart, and a [shutdown](#graceful-shutdown-and-draining) hands back any message that would only become due after the drain deadline.

### De-duplication

With [`LocalOptions::dedup_window`](#localprovider) set to `n > 0`, the provider remembers the last `n` message ids ([`headers::MESSAGE_ID`](#metadata-conventions)) accepted **per destination**. A `send` whose message id is still in that window is treated as a duplicate: it returns `Ok(())`, the message is not delivered, and `on_dropped(…, DropReason::Duplicate)` fires.

```rust
use ferroly::messaging::{LocalOptions, LocalProvider, Message, Producer};

# async fn ex() {
let bus = LocalProvider::with_options(
    "bus",
    LocalOptions { dedup_window: 10_000, ..Default::default() },
);
# let _ = bus.send("chan://orders", Message::text("o").with_id("order-17"), Default::default()).await;
// A retry of the same logical message — same id — is dropped as a duplicate.
let again = Message::text("o").with_id("order-17");
bus.send("chan://orders", again, Default::default()).await.ok();
# }
```

- Messages **without** an id are never de-duplicated.
- The window is bounded by **count**, not time: once `n` newer ids have been accepted for the destination, an old id is forgotten.
- A send that fails (`NoListeners`, `BufferFull`, …) does not record its id, so retrying the message you were handed back works.
- This de-duplicates *sends*. It does not make handling exactly-once: a redelivery after `Retry` reaches the handler again by design.

### Request/reply

`request` sends a message and waits for an answer; `responder` builds the answering handler.

```rust
use std::time::Duration;
use ferroly::messaging::{LocalProvider, Message, MessagingError, Receiver};

# async fn ex() -> Result<(), MessagingError> {
let bus = LocalProvider::new("bus");

// The responder turns a request into Ok(reply) — or Err(ack) to retry/reject it.
let pricing = bus.responder(|request: Message| async move {
    Ok(Message::text(format!("price of {}: 42", request.body_str())))
});
bus.add_listener("chan://pricing", pricing, Default::default()).await?;

let reply = bus
    .request("chan://pricing", Message::text("sku-1"), Duration::from_secs(2))
    .await?;
assert_eq!(reply.body_str(), "price of sku-1: 42");
# Ok(())
# }
```

What `request` does:

- gives the message a [message id and correlation id](#metadata-conventions) if it has none;
- sets its `reply-to` header to a private `reply://…` inbox;
- gives it a **TTL equal to the timeout**, so a request still waiting in a buffer when the caller gives up expires instead of being handled for nobody;
- waits up to `timeout` for the reply, then returns `MessagingError::RequestTimeout(addr)`.

On the answering side, `responder(f)` sends `f`'s `Ok(reply)` to the request's reply address — marked with [`in_reply_to`](#metadata-conventions), so the correlation id and trace context carry over — and acks. `Err(ack)` retries or rejects the request without replying. To reply by hand, call `bus.reply(&request, reply)` from a handler holding a [weak handle](#handle-lifetime). A reply whose requester is no longer waiting fails with `NoListeners`; `responder` ignores that.

Requests work over `topic://` too; the first reply wins.

### Graceful shutdown and draining

`shutdown(drain_timeout)` — and the lifecycle `stop()` and `Provider::close()`, which call it with [`LocalOptions::drain_timeout`](#localprovider) — stops the provider **without silently dropping anything**:

1. **New work is refused.** `send` and `add_listener` return [`MessagingError::Closed`](#error-handling); a refused send gets its message back. (Replies to requests that are already waiting are still delivered, and the provider's own dead-letter moves are still accepted.)
2. **Consumers drain.** They keep working until every accepted message has been completed — acked, rejected, or dead-lettered, with retries and their backoff included — or `drain_timeout` has passed. Messages [scheduled](#delayed-delivery) for later than the deadline are not waited for.
3. **Consumers are stopped and awaited.** A handler still running at the deadline is cancelled at its next `.await`. When `shutdown` returns, the consumer tasks have ended.

Whatever could not be completed is reported, twice over: each message fires `on_dropped(…, DropReason::Shutdown)`, and all of them are returned to the caller:

```rust
pub struct DrainReport { pub undelivered: Vec<Undelivered> }   // + is_clean()
pub struct Undelivered { pub addr: String, pub message: Message }
```

```rust
use std::time::Duration;
use ferroly::messaging::LocalProvider;

# async fn ex(bus: LocalProvider) {
let report = bus.shutdown(Duration::from_secs(10)).await;
for lost in &report.undelivered {
    // Persist or re-route: these were accepted but not completed.
    eprintln!("undelivered on {}: {}", lost.addr, lost.message.body_str());
}
# }
```

After a shutdown the provider is stopped and has **no listeners**. The lifecycle `start()` reopens it; listeners must be added again. A second `shutdown` on a stopped provider returns an empty report.

Two things to keep in mind:

- Sends made **by handlers** are refused during the drain like any other send. A handler that forwards to another destination should return `Ack::Retry` when its send fails; if the drain ends first, the message is reported as undelivered.
- A handler that blocks its thread without ever awaiting cannot be cancelled. `shutdown` still returns at the deadline, but that handler's task runs on until it yields.

### Listener self-healing

Each listener is a task serving its own buffer. If a listener is removed, or its task ends for any reason, its buffer is closed and the provider stops selecting it on the next send, so the remaining listeners keep receiving. A handler panic does **not** end the task — it is [contained and retried](#ack). If no listener is left on a queue, `send` returns [`MessagingError::NoListeners`](#error-handling).

## Observer

```rust
pub trait Observer: Send + Sync {
    fn on_send(&self, addr: &str, msg: &Message) {}
    fn on_receive(&self, addr: &str, msg: &Message) {}
    fn on_ack(&self, addr: &str, msg: &Message) {}
    fn on_nack(&self, addr: &str, msg: &Message, requeue: bool) {}
    fn on_dead_letter(&self, addr: &str, msg: &Message, to_dlq: bool) {}
    fn on_handled(&self, addr: &str, msg: &Message, elapsed: Duration, ack: Ack) {}
    fn on_handler_panic(&self, addr: &str, msg: &Message, panic: &str) {}
    fn on_expired(&self, addr: &str, msg: &Message) {}
    fn on_dropped(&self, addr: &str, msg: &Message, reason: DropReason) {}
    fn on_queue_depth(&self, addr: &str, depth: usize) {}
}
```

Hooks for every step of a message's life. Every method has a default no-op body, so you only override what you care about. Install one with [`LocalProvider::set_observer`](#localprovider).

`addr` is the canonical address of the listener's buffer (`chan://orders`, `topic://orders.*`) — a stable value to use as a metrics label. Callbacks run synchronously on the task that caused the event and never while a provider lock is held; keep them short and non-blocking. A panic inside a callback is caught and discarded.

| Hook | Fired when | Metric |
| --- | --- | --- |
| `on_send` | a message has been **accepted** into a listener's buffer (once per copy for a topic; also for dead-letter moves) | enqueued |
| `on_receive` | a message is about to be handed to a handler; once per delivery attempt | delivered |
| `on_ack` | the handler returned [`Ack::Ok`](#ack) | acked |
| `on_nack` | the handler returned `Retry` (or panicked) or `Reject`; `requeue` says whether it will be redelivered | retried (`requeue = true`) / rejected |
| `on_dead_letter` | retry attempts were exhausted, or an expired message was routed onwards; `to_dlq = false` means the message was **lost** | dead-lettered / lost |
| `on_handled` | a handler invocation finished after `elapsed` with `ack` (a panic counts as `Retry`) | handler duration |
| `on_handler_panic` | a handler panicked; `panic` is the payload when it was a string | handler panics |
| `on_expired` | a message's TTL ran out before a handler took it | expired |
| `on_dropped` | a message was discarded without being completed; see `DropReason` | dropped |
| `on_queue_depth` | the number of messages buffered for `addr` changed; after each enqueue and dequeue | queue depth (gauge) |

```rust
#[non_exhaustive]
pub enum DropReason {
    Expired,        // TTL ran out and it was not dead-lettered
    Evicted,        // displaced by a send with OnFull::DropOldest
    Duplicate,      // same message id within the de-duplication window
    NoSubscribers,  // published to a topic with no matching subscription
    BufferFull,     // a topic subscription had no room for its copy
    Shutdown,       // the provider stopped or was dropped first
}
```

`DropReason::as_str()` gives a lowercase label (`"expired"`, `"evicted"`, …). The hooks to alarm on are `on_dead_letter` with `to_dlq = false` and `on_dropped` with `Shutdown` or `BufferFull`: each means a message was lost.

The module does not depend on `ferroly::metrics`; bridge to it (or to any other sink) inside your observer. A counting observer:

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use ferroly::messaging::{Ack, DropReason, Message, Observer};

#[derive(Default)]
struct Counters {
    enqueued: AtomicU64,
    acked: AtomicU64,
    retried: AtomicU64,
    lost: AtomicU64,
    depth: AtomicUsize,
    handler_micros: AtomicU64,
}

impl Observer for Counters {
    fn on_send(&self, _addr: &str, _m: &Message) {
        self.enqueued.fetch_add(1, Ordering::Relaxed);
    }
    fn on_ack(&self, _addr: &str, _m: &Message) {
        self.acked.fetch_add(1, Ordering::Relaxed);
    }
    fn on_nack(&self, _addr: &str, _m: &Message, requeue: bool) {
        if requeue { self.retried.fetch_add(1, Ordering::Relaxed); }
    }
    fn on_dead_letter(&self, _addr: &str, _m: &Message, to_dlq: bool) {
        if !to_dlq { self.lost.fetch_add(1, Ordering::Relaxed); } // dropped, not DLQ'd
    }
    fn on_dropped(&self, _addr: &str, _m: &Message, _reason: DropReason) {
        self.lost.fetch_add(1, Ordering::Relaxed);
    }
    fn on_handled(&self, _addr: &str, _m: &Message, elapsed: Duration, _ack: Ack) {
        self.handler_micros.fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);
    }
    fn on_queue_depth(&self, _addr: &str, depth: usize) {
        self.depth.store(depth, Ordering::Relaxed);
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

An [`Observer`](#observer) that emits structured logs through [`ferroly::log`](log.md): `debug` on send/receive/ack, `warn` on nack, dead-letter, expiry and drop, and `error` on a handler panic, with fields such as `addr`, `bytes`, `delivery`, `requeue`, `dead_lettered`, `dropped`, `reason` and `panic`. It is the zero-code way to get message-level visibility — just hand it a [`Logger`](log.md#logger).

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
- `start()` opens the provider. A new provider is already open, so calling `start` before first use is optional; after a `stop` it is what reopens it.
- `stop()` is a [draining shutdown](#graceful-shutdown-and-draining) bounded by [`LocalOptions::drain_timeout`](#localprovider). If messages could not be delivered in time it returns an error (`… N message(s) could not be delivered before shutdown`), which makes the manager mark the component `Error`; the messages themselves are reported through `Observer::on_dropped`. Call [`shutdown`](#graceful-shutdown-and-draining) directly if you want them handed back.

```rust
use std::sync::Arc;
use ferroly::lifecycle::ComponentManager;
use ferroly::messaging::LocalProvider;

# async fn ex() -> Result<(), ferroly::lifecycle::LifecycleError> {
let bus = LocalProvider::new("order-bus");
let mgr = ComponentManager::new();
mgr.register(Arc::new(bus.clone()))?;
mgr.start_all().await?;
// … add listeners, run …
mgr.stop_all().await?;   // drains the bus before returning
# Ok(())
# }
```

Register the bus so that it stops **before** the things its handlers depend on (make the bus depend on them with `add_dependency`), so handlers can still finish their work during the drain.

## Complete example

Listener with an ack handler, a normal send, a retry-to-DLQ path, and a draining shutdown:

```rust
use std::time::Duration;
use ferroly::messaging::{
    handler, Ack, ListenerOptions, LocalProvider, Message, Producer, Receiver,
};

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

    // Drain everything (including the retries and the DLQ) before exiting.
    let report = bus.shutdown(Duration::from_secs(10)).await;
    assert!(report.is_clean());
}
# mod _d { #[derive(ferroly::codec::Encode)] struct Order { id: u32, total: u64 } }
```

## Error handling

```rust
#[non_exhaustive]
pub enum MessagingError {
    NoListeners(Rejected),      // no listener registered for the destination
    BufferFull(Rejected),       // the buffer was full (OnFull::Reject)
    Closed(Rejected),           // the provider is draining/stopped, or the listener went away
    SendTimeout(Rejected),      // the buffer stayed full for the whole OnFull::BlockFor wait
    RequestTimeout(String),     // a request received no reply in time (the address)
    InvalidAddress(String),     // malformed address, unsupported scheme, or wrong kind for the operation
    InvalidOptions(String),     // e.g. buffer_size: 0
    Unsupported(String),        // the provider does not implement the operation
    Undelivered(usize),         // shutdown left this many messages undelivered
    Batch(BatchFailure),        // a batch stopped part-way
    Backend { source: Box<dyn Error + Send + Sync> },  // a failure from an external backend
}

pub struct Rejected {
    pub destination: String,
    pub message: Option<Box<Message>>,
}
```

`MessagingError` derives ferroly's error machinery (via `ferroly_derive::FerrolyError`) — it implements `std::error::Error` and `Display`.

**Rejected sends hand the message back.** The four send-side variants carry a `Rejected`: the destination plus the message that was not accepted. Recover it instead of losing it:

| Method | Description |
| --- | --- |
| `MessagingError::into_message(self) -> Option<Message>` | Take the rejected message back. |
| `MessagingError::message(&self) -> Option<&Message>` | Look at it without consuming the error. |
| `MessagingError::destination(&self) -> Option<&str>` | The destination the operation was addressed to. |
| `Rejected::into_message(self) -> Option<Message>` | The same, on the payload. |
| `MessagingError::backend(err)` | Wrap a backend's own error (for provider implementations). |

```rust
use ferroly::messaging::{LocalProvider, Message, Producer};

# async fn ex(bus: LocalProvider, parked: &mut Vec<Message>) {
if let Err(error) = bus.send("chan://orders", Message::text("o-1"), Default::default()).await {
    eprintln!("send failed: {error}");
    if let Some(msg) = error.into_message() {
        parked.push(msg); // retry later
    }
}
# }
```

`Rejected::message` is `None` only when the failing operation carried no message (for example `add_listener` on a stopped provider returns `Closed`).

- [`send`](#producer) to a queue returns `NoListeners` if nothing is listening, `BufferFull`/`SendTimeout` per the [backpressure policy](#backpressure), and `Closed` if the provider is draining or stopped.
- [`send`](#producer) to a topic returns `Ok(())` even if some or all subscriptions did not take the message; see [topics](#publishsubscribe-topics).
- [`add_listener`](#receiver) returns `InvalidOptions`, `InvalidAddress`, or `Closed`.
- Redelivery and dead-letter moves happen inside the provider and cannot be returned to a caller. A dead-letter move that fails is reported through [`Observer::on_dead_letter`](#observer) with `to_dlq = false`. Configure the DLQ listener before you start producing, and watch that hook.

## Limitations

- **In-process, in-memory only.** No persistence: nothing survives a restart or a crash, and nothing crosses a process boundary. See the [delivery guarantee](#delivery-guarantee).
- **No durable or external backends are included.** Kafka, NATS, SQS and similar belong in adapter crates [implementing `Provider`](#implementing-a-provider).
- **Deferred:** a transactional outbox (atomically committing a database change and a message), and consumer groups / shared subscriptions **across processes**. Named subscriptions give you competing consumers within one process only.
- **At-least-once, not exactly-once.** A redelivered message reaches the handler again; make handlers idempotent. [De-duplication](#de-duplication) covers repeated sends, not redeliveries.
- **Retry blocks the listener.** A message that is backing off holds its place ([ordering guarantee](#ordering-guarantee)), so a serial listener handles nothing else meanwhile. Use `concurrency`, a shorter [`Backoff`](#backoff), or a lower `max_delivery_attempts` with a dead-letter queue if that matters.
- **Topics have no replay or retention.** A subscription receives only what is published after it was added.
- **De-duplication is bounded by count**, per destination, in memory.
- **DLQ moves can still fail.** If the `dead_letter` destination has no listener, the exhausted message is lost — reported via `on_dead_letter(to_dlq = false)`.
- **Headers are metadata only.** The local provider routes on the address and the [routing key](#queues-competing-consumers--routing-key-affinity); it does not filter on headers.
- **Cancellation is cooperative.** A handler cancelled at a shutdown deadline stops at its next `.await`.

### Design rationale recap

- **Cancellation is by dropping the future** — there are no separate context-carrying method variants to thread through.
- **Headers and body live in one [`Message`](#message) struct**, with the [codec](codec.md) `Value` for headers, rather than a split interface.
- **Messages are built with the [`Message::new` / `text` / `json`](#constructors) constructors.**
- **A handler signals its verdict by returning an [`Ack`](#ack)**, not by mutating the message — explicit and side-effect-free.
- **A rejected message goes back to its sender**, and a discarded one is always reported.

## See also

- [codec](codec.md) — `Value`, `Encode`, `Decode` used for headers and bodies.
- [lifecycle](lifecycle.md) — the `Component` trait `LocalProvider` implements.
- [log](log.md) — backs [`LogObserver`](#logobserver).

---
**Related:** [codec](codec.md), [lifecycle](lifecycle.md), [log](log.md).

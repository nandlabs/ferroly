# Messaging

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `messaging` · **Module:** `ferroly::messaging`

Provider-agnostic messaging with a full-featured **in-process** provider: queues and publish/subscribe topics, acknowledgement and redelivery with backoff, routing keys, dead-letter queues, TTL and delayed delivery, request/reply, de-duplication, backpressure policies, observability hooks, and a draining shutdown.

## Overview

The module is built around a small set of traits and one batteries-included backend:

- A [`Message`](#message) is a plain data struct: typed system [properties](#properties), typed application [attributes](#attributes), a byte body, an optional routing key, and a redelivery counter.
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
- Messages are only ever discarded for a reason you configured or can observe: `Ack::Reject`, exhausted attempts with no dead-letter destination, TTL expiry, `OnFull::DropOldest`, de-duplication, a topic with no (matching) subscribers, or a shutdown that timed out. Every one of these is reported through the [`Observer`](#observer).

If you need durability, cross-process delivery, or consumer groups across processes, [implement `Provider`](#implementing-a-provider) over a broker. Those are out of scope for the built-in provider.

### Design notes

| Concern | ferroly's design |
| --- | --- |
| Signalling a message's fate | the handler **returns an [`Ack`](#ack)** — explicit, no interior mutability or hidden side effects |
| Message shape | one [`Message`](#message) struct with typed properties and attributes, not an interface split |
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

The optional [`Traced`](#tracing-spans-traced) wrapper (producer and consumer spans) requires the `obs` feature:

```toml
ferroly = { version = "*", features = ["messaging", "obs"] }
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
| [`Message`](#message) | struct | Properties, attributes, body, routing key, delivery count |
| [`Properties`](#properties) | struct | System metadata: id, correlation id, content type, priority, … |
| [`Attributes`](#attributes), `AttrValue` | struct, enum | Typed application attributes |
| [`headers`](#properties) | module | Wire names of the properties (`MESSAGE_ID`, `TRACEPARENT`, …) |
| [`new_message_id`](#properties) | fn | Generates a process-unique message id |
| [`Ack`](#ack) | enum | `Ok` / `Retry` / `Reject` / `RetryAfter` / `DeadLetter` / `Pending` disposition returned by a handler |
| [`Handler`](#handler) | type alias | `Arc<dyn Fn(Message) -> BoxFuture<'static, Ack> + Send + Sync>` |
| [`DeliveryHandler`](#delivery-context), `delivery_handler` | type alias, fn | A handler that receives the delivery context |
| [`Delivery`](#delivery), [`DeliveryInfo`](#deliveryinfo), [`Acker`](#acker) | structs | The delivery context: details, lease extension, deferred acks |
| [`DeliveryControl`](#deliverycontrol--for-provider-implementations) | trait | The provider's side of a `Delivery` |
| [`handler`](#handler) | fn | Wraps an async closure returning `Ack` |
| [`fallible`](#fallible) | fn | Wraps an async closure returning `Result<(), E>` |
| [`SendOptions`](#sendoptions) | struct | Per-send: routing key, backpressure policy, TTL, delay |
| [`OnFull`](#onfull) | enum | `Reject` / `Block` / `BlockFor` / `DropOldest` |
| [`ListenerOptions`](#listeneroptions) | struct | Per-listener: buffer, concurrency, attempts, DLQ, backoff, expiry, subscription |
| [`Filter`](#filtering-subscriptions) | enum | Attribute filter for a topic subscription |
| [`Backoff`](#backoff) | struct | Delay between redeliveries |

| [`ExpiryAction`](#listeneroptions) | enum | What happens to an expired message |
| [`LocalOptions`](#localprovider) | struct | Provider-level: drain timeout, de-duplication window (by dedup key, then message id) |

| [`Producer`](#producer) | trait | `send`, `send_with_receipt`, `send_batch` |
| [`Receiver`](#receiver) | trait | `add_listener`, `remove_listeners` (both defaulted) |
| [`Provider`](#provider) | trait | `Producer + Receiver` + `schemes` / `setup` / `close` / `capabilities` / `set_observer` |
| [`Capabilities`](#capabilities) | struct | What a provider supports |
| [`Receipt`](#receipts) | struct | What a provider returns for an accepted message |

| [`ProviderRegistry`](#providerregistry) | struct | Dispatches to providers by address scheme |
| [`LocalProvider`](#localprovider) | struct | In-process provider |
| [`WeakLocalProvider`](#localprovider) | struct | A handle that does not keep the provider alive |
| [`DrainReport`](#graceful-shutdown-and-draining), `Undelivered` | structs | What a shutdown could not deliver |
| [`Observer`](#observer) | trait | Per-message hooks, sufficient for metrics |
| [`DropReason`](#observer) | enum | Why a message was discarded |
| [`LogObserver`](#logobserver) | struct | `Observer` that logs via `ferroly::log` (feature `log`) |
| [`MessagingError`](#error-handling) | enum | Errors; send errors hand the message back |
| [`Rejected`](#error-handling), [`BatchFailure`](#batch-send), `BatchEntryFailure` | structs | Error payloads |
| [`topic_matches`](#wildcards), `split_address` | fns | Address helpers |
| [`BoxFuture`](#boxfuture) | type alias | `Pin<Box<dyn Future<Output = T> + Send + 'a>>` |

### BoxFuture

```rust
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
```

The object-safe async return type used throughout the traits. You rarely name it directly — the [`handler`](#handler) / [`fallible`](#fallible) helpers box your futures for you, and trait methods return it ready to `.await`.

## Message

```rust
#[non_exhaustive]
pub struct Message {
    pub properties: Properties,     // system metadata: id, correlation id, content type, …
    pub attributes: Attributes,     // application metadata: typed key/value pairs
    pub body: Vec<u8>,
    pub routing_key: Option<String>,
    pub delivery_count: u32,
}
```

A message is a plain, `Clone`-able data struct. Every field is public, but the struct is `#[non_exhaustive]`: outside the crate, build one with [`Message::new` / `text` / `json`](#constructors) (or `Message::default()`) and the `with_*` methods rather than a struct literal.

- `properties` — the [system properties](#properties): well-known metadata with a fixed meaning (message id, correlation id, timestamp, reply address, trace context, content type, priority, …).
- `attributes` — the application's own metadata, as typed [`Attributes`](#attributes).
- `body` — the raw payload bytes.
- `routing_key` — an optional partition key. When set, every message sharing that key is delivered to the *same* listener (see [routing-key affinity](#queues-competing-consumers--routing-key-affinity)).
- `delivery_count` — how many times this message has been redelivered. It starts at `0` and is incremented by the provider before each redelivery; a handler can inspect it to detect a redelivered message.

### Constructors

```rust
Message::new(body: impl Into<Vec<u8>>) -> Message   // raw bytes
Message::text(text: impl Into<String>) -> Message   // UTF-8 body
Message::json<T: Encode>(value: &T) -> Message      // JSON-encoded body, content type application/json
```

`Message::json` uses the [codec](codec.md) `Encode` trait (not serde) to render the body as JSON bytes, and sets the `content_type` property to `application/json`. `new` and `text` leave the content type unset.

```rust
use ferroly::messaging::Message;

#[derive(ferroly::codec::Encode)]
struct Order { id: u32, total: u64 }

let raw  = Message::new(vec![0xDE, 0xAD]);
let text = Message::text("hello");
let json = Message::json(&Order { id: 1, total: 4200 });
assert_eq!(json.content_type(), Some("application/json"));
```

### Attributes

Application metadata is a typed, ordered map:

```rust
#[non_exhaustive]
pub enum AttrValue {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Bytes(Vec<u8>),
}

pub struct Attributes { /* ordered (name, AttrValue) entries */ }
```

There is no timestamp variant — carry a time as an `Int` of milliseconds. Nested or array values are not supported; put structured data in the body or encode it into a string.

`AttrValue` converts `From` `&str`, `String`, `&String`, `i64`, `i32`, `u32`, `u16`, `u8`, `f64`, `f32`, `bool`, `Vec<u8>` and `&[u8]`. There is deliberately no `From<u64>` / `From<usize>` (they do not fit an `i64`): cast, as in `with_attribute("n", n as i64)`.

| `AttrValue` method | Returns |
| --- | --- |
| `as_str()` / `as_i64()` / `as_bool()` / `as_bytes()` | `Some` only for the matching variant |
| `as_f64()` | `Float(f)` → `f`, `Int(i)` → `i as f64`, otherwise `None` |
| `type_name()` | `"string"`, `"int"`, `"float"`, `"bool"`, `"bytes"` |
| `to_text()` | a string as is; numbers and booleans in their usual form; bytes as lowercase hex |

`Attributes` keeps entries in insertion order and allows a name more than once. Names are compared exactly (case-sensitively).

| `Attributes` method | Behaviour |
| --- | --- |
| `new()`, `len()`, `is_empty()`, `clear()` | the usual |
| `insert(name, value) -> Option<AttrValue>` | sets `name` to exactly one value: replaces the first entry in place, removes every other entry with that name, returns the first old value; appends a new name at the end |
| `append(name, value)` | always adds a new entry at the end, keeping existing ones |
| `get(name)` / `get_all(name)` | the first value / every value with that name, in order |
| `get_str` / `get_i64` / `get_f64` / `get_bool` / `get_bytes` | `get(name)` with the typed accessor applied |
| `contains_key(name)` | whether any entry has that name |
| `remove(name) -> Option<AttrValue>` | removes **every** entry with that name, returns the first |
| `iter()` / `keys()` | entries / names in order (a repeated name is repeated) |

`Attributes` also implements `FromIterator<(K, V)>`, `Extend<(K, V)>` (both via `append`) and `IntoIterator<Item = (String, AttrValue)>`.

On a `Message`:

```rust
Message::with_attribute(self, name: impl Into<String>, value: impl Into<AttrValue>) -> Message
Message::set_attribute(&mut self, name: impl Into<String>, value: impl Into<AttrValue>)
Message::attribute(&self, name: &str) -> Option<&AttrValue>
Message::attribute_str(&self, name: &str) -> Option<&str>
Message::with_routing_key(self, key: impl Into<String>) -> Message
```

`with_attribute` / `set_attribute` call `Attributes::insert`, so they **replace** an existing attribute with the same name. For repeated names use `msg.attributes.append(…)`.

```rust
use ferroly::messaging::{AttrValue, Message};

let msg = Message::text("ship it")
    .with_attribute("tenant", "acme")
    .with_attribute("attempt", 2)
    .with_attribute("express", true)
    .with_routing_key("customer-42");
assert_eq!(msg.attribute_str("tenant"), Some("acme"));
assert_eq!(msg.attributes.get_i64("attempt"), Some(2));
assert_eq!(msg.attribute("express"), Some(&AttrValue::Bool(true)));
```

### Properties

System metadata lives in `Message::properties`, a `#[non_exhaustive]` struct of optional fields. Each has a typed setter/getter on `Message`, and a conventional **wire name** in the `ferroly::messaging::headers` module — the name an adapter uses when its broker has a single flat attribute map.

| Field | Wire name (`headers::…`) | Wire type | Setter / getter |
| --- | --- | --- | --- |
| `message_id` | `MESSAGE_ID` = `message-id` | string | `with_id`, `with_new_id` / `id` |
| `correlation_id` | `CORRELATION_ID` = `correlation-id` | string | `with_correlation_id` / `correlation_id` |
| `timestamp` | `TIMESTAMP` = `timestamp` | int: milliseconds since the Unix epoch | `with_timestamp(SystemTime)` / `timestamp` |
| `reply_to` | `REPLY_TO` = `reply-to` | string: an address | `with_reply_to` / `reply_to` |
| `traceparent` | `TRACEPARENT` = `traceparent` | string: W3C Trace Context | `with_traceparent` / `traceparent` |
| `tracestate` | `TRACESTATE` = `tracestate` | string: W3C Trace Context | `with_tracestate` / `tracestate` |
| `topic` | `TOPIC` = `topic` | string: the concrete topic a copy was published to | set by the provider / `topic` |
| `content_type` | `CONTENT_TYPE` = `content-type` | string: a media type | `with_content_type` / `content_type` |
| `content_encoding` | `CONTENT_ENCODING` = `content-encoding` | string, e.g. `gzip` | `with_content_encoding` / `content_encoding` |
| `subject` | `SUBJECT` = `subject` | string | `with_subject` / `subject` |
| `event_type` | `EVENT_TYPE` = `event-type` | string, e.g. `order.created` | `with_event_type` / `event_type` |
| `source` | `SOURCE` = `source` | string: what produced the message | `with_source` / `source` |
| `user_id` | `USER_ID` = `user-id` | string: the sender's identity | `with_user_id` / `user_id` |
| `dedup_key` | `DEDUP_KEY` = `dedup-key` | string | `with_dedup_key` / `dedup_key` |
| `priority` | `PRIORITY` = `priority` | int, 0–255 | `with_priority(u8)` / `priority() -> u8` (0 when unset) |

`headers::PROPERTY_NAMES` lists those wire names in the order above. The module also names the four attributes a provider adds to a [dead-lettered](#ackredelivery--dead-lettering) message: `DEAD_LETTER_REASON`, `DEAD_LETTER_DESCRIPTION`, `DEAD_LETTER_SOURCE`, `DEAD_LETTER_ATTEMPTS`.

The timestamp is stored with **millisecond precision**: `with_timestamp` truncates to whole milliseconds, so it survives a trip through the wire form unchanged.

Helpers that combine them:

- `stamped(self)` — fills in a generated id and the current timestamp where they are missing.
- `in_reply_to(self, &request)` — marks a message as a reply: the correlation id becomes the request's correlation id (or its message id), and the request's trace context is carried over.
- `with_trace_from(self, &parent)` — copies `traceparent`/`tracestate` from the message being handled, unless this one already has a `traceparent`.
- `new_message_id() -> String` — 32 hex characters, unique within the process and unlikely to collide across processes. It is an identifier, not a secret.

```rust
use ferroly::messaging::Message;

let msg = Message::text("payload")
    .stamped()                                   // message-id + timestamp
    .with_correlation_id("checkout-7781")
    .with_event_type("order.created")
    .with_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01");

assert!(msg.id().is_some());
assert_eq!(msg.correlation_id(), Some("checkout-7781"));
assert_eq!(msg.event_type(), Some("order.created"));
```

**Properties and attributes are separate.** `with_id("m1")` and `with_attribute("message-id", "app")` set two different things: `id()` returns `"m1"`, `attribute_str("message-id")` returns `"app"`.

**Trace propagation.** Without the `obs` feature the module creates no spans; with it, wrap the provider in [`Traced`](#tracing-spans-traced) and the steps below happen for you. By hand, the convention is: when you send, copy the active span's W3C `traceparent` (and `tracestate`) into the message with `with_traceparent`; when you handle a message, start your consumer span from `msg.traceparent()`; and when a handler produces a follow-up message, call `with_trace_from(&msg)` so the trace continues.

#### The wire form

An adapter for a broker that has one flat attribute map (SQS message attributes, Pub/Sub attributes, AMQP application properties, …) uses three helpers:

```rust
Message::to_wire_attributes(&self) -> Attributes
Message::from_wire(body: impl Into<Vec<u8>>, attributes: Attributes) -> Message
Message::lookup(&self, name: &str) -> Option<AttrValue>
```

- `to_wire_attributes` writes every **set** property under its wire name (in `PROPERTY_NAMES` order), then the application attributes. An application attribute whose name is a property name is left out — the property wins.
- `from_wire` is the reverse: an entry whose name is a property name **and** whose value has that property's wire type becomes the property (the timestamp from an `Int` of milliseconds, the priority from an `Int` in `0..=255`, everything else from a `Str`); every other entry — including a property name with the wrong type — is kept as an application attribute. If a property name appears more than once, the first well-typed entry becomes the property and the rest stay attributes.
- `lookup` finds a name the way a subscription filter does: a set property under its wire name first, then the first application attribute.

```rust
use ferroly::messaging::{headers, AttrValue, Message};

let msg = Message::text("hi")
    .with_id("m-1")
    .with_priority(3)
    .with_attribute("tenant", "acme");

let wire = msg.to_wire_attributes();
let names: Vec<&str> = wire.keys().collect();
assert_eq!(names, [headers::MESSAGE_ID, headers::PRIORITY, "tenant"]);
assert_eq!(wire.get_i64(headers::PRIORITY), Some(3));

// … hand `wire` and `msg.body` to the broker; on the way back:
let back = Message::from_wire(msg.body.clone(), wire);
assert_eq!(back.id(), Some("m-1"));
assert_eq!(back.priority(), 3);
assert_eq!(back.attribute_str("tenant"), Some("acme"));
assert_eq!(back.lookup("message-id"), Some(AttrValue::Str("m-1".into())));
```

### Reading the body

```rust
Message::body_str(&self) -> Cow<'_, str>                                     // lossy UTF-8 view
Message::decode<T: Decode>(&self) -> Result<T, CodecError>                   // body → T, by content type
Message::encode_as<T: Encode + ?Sized>(content_type: &str, value: &T)
    -> Result<Message, CodecError>                                           // T → body, content type set
```

`body_str` gives you a lossy UTF-8 view for text payloads.

`decode` parses the body into any [codec](codec.md) `Decode` type. It uses the message's `content_type` property when it is one the codec registry knows (JSON, XML, YAML or TOML, including `+json`-style suffixes), and **JSON otherwise** — also when no content type is set, or when it is one the registry does not know.

`encode_as` is the other direction: it encodes `value` for `content_type` with the codec registry and sets that content type on the message. An unknown content type is an error. (`Message::json` is the infallible JSON shorthand.)

```rust
use ferroly::messaging::Message;

#[derive(ferroly::codec::Encode, ferroly::codec::Decode)]
struct Order { id: u32, total: u64 }

let yaml = Message::encode_as("application/yaml", &Order { id: 1, total: 4200 }).unwrap();
assert_eq!(yaml.content_type(), Some("application/yaml"));

fn handle(msg: &Message) {
    if let Ok(order) = msg.decode::<Order>() {     // YAML here, because of the content type
        println!("order {} = {}", order.id, order.total);
    } else {
        println!("plain text: {}", msg.body_str());
    }
}
# handle(&yaml);
```

## Ack

```rust
#[non_exhaustive]
pub enum Ack {
    Ok,                    // processed successfully; remove the message
    Retry,                 // failed transiently; redeliver after the listener's backoff (then dead-letter)
    Reject,                // failed permanently; discard (no redelivery, no dead-lettering)
    RetryAfter(Duration),  // like Retry, but after this delay instead of the backoff
    DeadLetter,            // failed permanently; dead-letter now, skipping the remaining attempts
    Pending,               // not decided yet: an Acker will settle it later
}
```

**This is the central design choice of the module.** A handler is a pure function of its input: it **returns an `Ack`**, rather than mutating the message or flipping an ack/nack flag on it. That keeps the control flow explicit — no interior mutability, no hidden side effects — and makes a handler trivially testable in isolation.

The value you return *is* the disposition. What `LocalProvider` does for each:

| `Ack` | What `LocalProvider` does | Observer |
| --- | --- | --- |
| `Ok` | drops the message | `on_ack` |
| `Retry` | increments `delivery_count`; while attempts remain, redelivers to the same listener after the [retry backoff](#backoff); once `delivery_count` reaches `max_delivery_attempts`, [dead-letters](#ackredelivery--dead-lettering) it (or drops it if no destination is configured) | `on_nack(requeue = true)`, finally `on_nack(requeue = false)` + `on_dead_letter` |
| `Reject` | discards the message immediately | `on_nack(requeue = false)` |
| `RetryAfter(d)` | like `Retry`, but waits `d` instead of the backoff. It **counts as a delivery attempt**, so it still ends in the dead-letter destination after `max_delivery_attempts` | as `Retry` |
| `DeadLetter` | moves the message to the dead-letter destination **now**, without using the remaining attempts (reason `rejected`); without a destination it is dropped and reported | `on_nack(requeue = false)` + `on_dead_letter` |
| `Pending` | waits for the delivery's [`Acker`](#delivery-context) to settle it, then acts on that `Ack`. If every `Acker` is dropped unsettled, it is treated as `Retry`. From a plain [`Handler`](#handler) — which has no `Acker` — it is treated as `Retry` at once | per the final `Ack` |

**A handler that panics is treated as having returned `Ack::Retry`.** The panic is contained, reported through [`Observer::on_handler_panic`](#observer), and the consumer keeps running.

`Ack` is `Copy`, so returning it is free. It is `#[non_exhaustive]`: a `match` on it outside the crate needs a wildcard arm.

```rust
use std::time::Duration;
use ferroly::messaging::{handler, Ack, Message};

let h = handler(|msg: Message| async move {
    match validate(&msg) {
        Verdict::Good      => Ack::Ok,
        Verdict::TryLater  => Ack::Retry,                                // transient — redelivered after the backoff
        Verdict::RateLimit => Ack::RetryAfter(Duration::from_secs(30)), // transient — redelivered in 30 s
        Verdict::Poison    => Ack::DeadLetter,                          // permanent — park it for inspection
        Verdict::Junk      => Ack::Reject,                              // permanent — drop now
    }
});
# enum Verdict { Good, TryLater, RateLimit, Poison, Junk }
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

## Delivery context

A plain [`Handler`](#handler) sees only the `Message`. A **delivery handler** also gets the delivery context — facts about this delivery, a way to ask for more time, and a way to settle the message later:

```rust
pub type DeliveryHandler = Arc<dyn Fn(Delivery) -> BoxFuture<'static, Ack> + Send + Sync>;

pub fn delivery_handler<F, Fut>(f: F) -> DeliveryHandler
where
    F: Fn(Delivery) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Ack> + Send + 'static;

// on Receiver — defaulted to MessagingError::Unsupported; LocalProvider implements it
fn add_delivery_listener(&self, addr: &str, handler: DeliveryHandler, opts: ListenerOptions)
    -> BoxFuture<'_, Result<(), MessagingError>>;
```

`add_delivery_listener` takes the same [`ListenerOptions`](#listeneroptions) as `add_listener`, and `ProviderRegistry` forwards it. A provider that supports it says so with [`Capabilities::delivery_context`](#capabilities).

### `Delivery`

```rust
pub struct Delivery {
    pub message: Message,
    /* the info and the provider's control */
}

impl Delivery {
    pub fn new(message: Message, info: DeliveryInfo, control: Arc<dyn DeliveryControl>) -> Self;
    pub fn detached(message: Message) -> Self;          // no provider behind it — for unit tests
    pub fn info(&self) -> &DeliveryInfo;
    pub fn into_message(self) -> Message;
    pub async fn extend(&self, by: Duration) -> Result<(), MessagingError>;
    pub fn acker(&self) -> Acker;
    pub fn dead_letter_description(&self, description: impl Into<String>);
}
```

- `info()` — the [`DeliveryInfo`](#deliveryinfo).
- `extend(by)` — asks for `by` more time before the message is redelivered to someone else (a broker's visibility timeout or lease). **`LocalProvider` has no leases, so `extend` is a successful no-op there.**
- `acker()` — an [`Acker`](#acker) to settle the message later; return `Ack::Pending` after taking one.
- `dead_letter_description(text)` — free text recorded on the message (as `dead-letter-description`) if the handler returns `Ack::DeadLetter` or the message is dead-lettered after its last attempt.
- `detached(msg)` — a delivery with no provider: `extend` succeeds and does nothing, and an `Acker` settles nothing (`ack()` returns `false`). Use it to unit-test a delivery handler.

### `DeliveryInfo`

```rust
#[non_exhaustive]
pub struct DeliveryInfo {
    pub address: String,               // the address the message was consumed from
    pub attempt: u32,                  // which delivery attempt this is, from 1
    pub redelivered: bool,             // attempt > 1
    pub enqueued_at: Option<SystemTime>,
    pub sequence: Option<u64>,         // the provider's sequence number or offset
    pub subscription: Option<String>,  // the subscription name, for topics
}
```

Providers build one with `DeliveryInfo::new(address)` (attempt 1) and `with_attempt`, `with_enqueued_at`, `with_sequence`, `with_subscription`. `LocalProvider` fills every field: `address` is the listener's canonical address (`chan://jobs`, `topic://orders.*`), `enqueued_at` is when the message was accepted, `sequence` counts the messages accepted by that listener's buffer (from 1, increasing in acceptance order), and `subscription` is the listener's `ListenerOptions::subscription`.

### `Acker`

```rust
#[derive(Clone)]
pub struct Acker { /* … */ }

impl Acker {
    pub fn ack(&self) -> bool;
    pub fn retry(&self) -> bool;
    pub fn retry_after(&self, delay: Duration) -> bool;
    pub fn reject(&self) -> bool;
    pub fn dead_letter(&self, description: impl Into<String>) -> bool;
}
```

An `Acker` settles a delivery whose handler returned `Ack::Pending` — later, or from another task (it is `Send + Sync + Clone`). Each method has the same effect as returning the matching `Ack`, and returns `true` if **this** call settled the delivery, `false` if it was already settled (or the delivery was not pending).

```rust
use ferroly::messaging::{delivery_handler, Ack, Delivery, LocalProvider, Receiver};

# async fn ex(bus: LocalProvider, work: tokio::sync::mpsc::UnboundedSender<(String, ferroly::messaging::Acker)>) {
bus.add_delivery_listener(
    "chan://jobs",
    delivery_handler(move |delivery: Delivery| {
        let work = work.clone();
        async move {
            if delivery.info().redelivered {
                eprintln!("attempt {} of {}", delivery.info().attempt, delivery.message.body_str());
            }
            // Hand the job to a worker pool; the worker calls `acker.ack()` when done.
            let job = delivery.message.body_str().into_owned();
            let _ = work.send((job, delivery.acker()));
            Ack::Pending
        }
    }),
    Default::default(),
)
.await
.unwrap();
# }
```

**The consumer does not wait forever.** While a delivery is pending, a serial listener handles nothing else (as for a retry backoff), and the message still counts towards `pending()`. If every `Acker` for it is dropped without settling, the provider treats the message as `Retry`. A [shutdown](#graceful-shutdown-and-draining) that reaches its deadline gives up on a pending delivery and reports it as undelivered.

### `DeliveryControl` — for provider implementations

```rust
pub trait DeliveryControl: Send + Sync {
    fn extend(&self, by: Duration) -> BoxFuture<'_, Result<(), MessagingError>>;
    fn set_dead_letter_description(&self, description: String);
    fn resolve(&self, ack: Ack) -> bool;
}
```

The provider's side of a `Delivery`, one per delivery: `extend` renews the broker's lease (a provider without leases returns `Ok(())`), `set_dead_letter_description` records the text for a later dead-letter, and `resolve` settles a pending delivery — returning `true` only for the call that settled it, and never accepting `Ack::Pending` itself as a resolution.

## Options

### SendOptions

```rust
pub struct SendOptions {
    pub routing_key: Option<String>,     // overrides the message's own routing_key
    pub block_on_full: bool,             // shorthand for on_full = Some(OnFull::Block)
    pub on_full: Option<OnFull>,         // backpressure policy
    pub ttl: Option<Duration>,           // time to live
    pub delay: Option<Duration>,         // delayed delivery
    pub deliver_at: Option<SystemTime>,  // delivery at an absolute time
    pub extensions: Attributes,          // provider-specific options
}
```

Per-`send` options. `Default` gives no routing-key override, [`OnFull::Reject`](#onfull), no TTL, no delay, no schedule and no extensions. Build one with a struct literal and `..Default::default()`, or fluently:

```rust
use std::time::Duration;
use ferroly::messaging::{OnFull, SendOptions};

let opts = SendOptions::new()
    .routing_key("customer-7")
    .on_full(OnFull::BlockFor(Duration::from_secs(1)))
    .ttl(Duration::from_secs(30))
    .delay(Duration::from_secs(5))
    .extension("sqs.message-group-id", "orders");
```

- `routing_key` — takes precedence over `Message::routing_key` when picking the target listener.
- `on_full` — what to do when the target listener's buffer is full; see [`OnFull`](#onfull). `block_on_full: true` is the older spelling of `OnFull::Block` and is ignored when `on_full` is set. `full_policy()` returns the effective policy.
- `ttl` — [time to live](#ttl-and-expiry), counted from the moment the message is accepted.
- `delay` — [delayed delivery](#delayed-delivery): the message becomes visible to its listener only after this long.
- `deliver_at` (builder `deliver_at(SystemTime)`) — [scheduled delivery](#delayed-delivery) at an absolute time instead of after a delay. Setting both `delay` and `deliver_at` is an error.
- `extensions` (builder `extension(name, value)`) — provider-specific options for this one send, e.g. `"sqs.message-group-id"`. Prefix each name with the provider's name; a provider ignores names that are not its own.

**Options a provider cannot honour.** `delay`, `deliver_at` and `ttl` change what delivery means, so a provider that does not support one must **refuse** the send with `InvalidOptions` rather than deliver it differently. `unsupported_by` is the check an adapter runs:

```rust
SendOptions::unsupported_by(&self, caps: &Capabilities) -> Option<&'static str>
```

It returns the name of the first set option that `caps` does not support (`"delay"`, `"deliver_at"`, `"ttl"`), or `None`. `on_full`, `block_on_full` and foreign `extensions` are hints and may be ignored.

```rust
use std::time::Duration;
use ferroly::messaging::{Capabilities, SendOptions};

let caps = Capabilities::new().ttl(true);
let opts = SendOptions::new().delay(Duration::from_secs(1)).ttl(Duration::from_secs(9));
assert_eq!(opts.unsupported_by(&caps), Some("delay"));
assert_eq!(SendOptions::new().ttl(Duration::from_secs(9)).unsupported_by(&caps), None);
```

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
    pub filter: Option<Filter>,        // topic:// only; default None
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
- `filter` — for `topic://` listeners, only messages matching this [filter](#filtering-subscriptions) are delivered to the subscription. A filter on a `chan://` listener is `InvalidOptions`.

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

    // Defaulted: calls `send` and returns a receipt holding the message's own id.
    fn send_with_receipt(&self, addr: &str, msg: Message, opts: SendOptions)
        -> BoxFuture<'_, Result<Receipt, MessagingError>>;

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

### Receipts

`send_with_receipt` sends like `send` and returns what the provider knows about the accepted message:

```rust
#[non_exhaustive]
pub struct Receipt {
    pub message_id: Option<String>,  // the broker's id, or the message's own
    pub sequence: Option<String>,    // the broker's sequence number or offset, if any
    pub extra: Attributes,           // anything else the broker returned
}
```

Build one with `Receipt::new()` and `with_message_id`, `with_sequence`, `with_extra(name, value)`. The default implementation calls `send` and returns a receipt holding the message's own id (`msg.id()`); a backend whose broker assigns ids or offsets overrides it.

```rust
use ferroly::messaging::{LocalProvider, Message, Producer};

# async fn ex(bus: LocalProvider) -> Result<(), ferroly::messaging::MessagingError> {
let receipt = bus
    .send_with_receipt("chan://orders", Message::text("o").with_id("m-9"), Default::default())
    .await?;
assert_eq!(receipt.message_id.as_deref(), Some("m-9"));
# Ok(())
# }
```

### Batch send

`send_batch` sends several messages to one address, applying the same options to each. It is **not atomic**. When some messages are not accepted it returns `MessagingError::PartialBatch(BatchFailure)`, which says exactly which ones:

```rust
#[non_exhaustive]
pub struct BatchFailure {
    pub sent: usize,                       // how many were accepted
    pub failures: Vec<BatchEntryFailure>,  // tried and failed, in ascending index order
    pub unsent: Vec<Message>,              // never tried, in their original order
}

#[non_exhaustive]
pub struct BatchEntryFailure {
    pub index: usize,              // position in the batch that was passed in (0-based)
    pub error: MessagingError,     // why it failed
    pub message: Option<Message>,  // the message, when the failing operation handed it back
}
```

| Method | Description |
| --- | --- |
| `BatchFailure::new(sent, failures, unsent)` | Builds one (for provider implementations); `failures` are sorted by index. |
| `BatchFailure::first_error()` | The error of the first failed entry. |
| `BatchFailure::into_messages()` | Every message not accepted: the failed ones that were handed back (in index order), then the ones never tried. |
| `BatchFailure::failed()` | `failures.len() + unsent.len()`. |
| `BatchEntryFailure::new(index, error)` | An entry failure; a message carried by `error` is moved into `message`. |
| `BatchEntryFailure::with_message(index, error, message)` | An entry failure with the message supplied. |

How much a `BatchFailure` reports depends on the provider:

- **The default `send_batch`** (used by `LocalProvider`) is **sequential**: it calls `send` once per message and **stops at the first failure**. `failures` then has exactly one entry — at `index == sent` — and the rest of the batch is in `unsent`.
- **A provider with a native batch call** tries every message and reports **every** failed entry in `failures`, with `unsent` empty.

`PartialBatch` is [retryable](#error-handling) when any of its entries is. A retry loop that resends only what was not accepted:

```rust
use ferroly::messaging::{Message, MessagingError, Producer};

# async fn ex(bus: &dyn Producer, mut batch: Vec<Message>) -> Result<(), MessagingError> {
for _attempt in 0..3 {
    match bus.send_batch("chan://orders", batch, Default::default()).await {
        Ok(()) => return Ok(()),
        Err(MessagingError::PartialBatch(failure)) => {
            eprintln!("{failure}");               // "2 sent, 1 failed: …"
            batch = failure.into_messages();      // only what was not accepted
        }
        Err(other) => return Err(other),
    }
}
# Ok(())
# }
```

A real retry loop would also check `is_retryable()` and back off between attempts.

## Receiver

```rust
pub trait Receiver: Send + Sync {
    // Defaulted: returns MessagingError::Unsupported.
    fn add_listener(&self, addr: &str, handler: Handler, opts: ListenerOptions)
        -> BoxFuture<'_, Result<(), MessagingError>>;

    // Defaulted: returns MessagingError::Unsupported.
    fn add_delivery_listener(&self, addr: &str, handler: DeliveryHandler, opts: ListenerOptions)
        -> BoxFuture<'_, Result<(), MessagingError>>;

    // Defaulted: returns MessagingError::Unsupported.
    fn remove_listeners(&self, addr: &str)
        -> BoxFuture<'_, Result<usize, MessagingError>>;
}
```

`add_listener` registers a [`Handler`](#handler) on an address. With `LocalProvider` each call creates a dedicated buffer and consumer task; the returned future must be awaited for the listener to be registered.

`remove_listeners` removes every listener on an address and returns how many were removed. With `LocalProvider`, removed listeners take no new messages but **finish the backlog already buffered for them**. For `topic://`, the address is matched against subscription patterns literally (`topic://orders.*` removes the subscriptions registered with exactly that pattern).

`add_delivery_listener` registers a [`DeliveryHandler`](#delivery-context), which receives the delivery context as well as the message.

All three methods have defaults that return `MessagingError::Unsupported`,
 which is exactly what a **publish-only** provider wants: it implements `Producer` and writes `impl Receiver for MyProvider {}`.

## Provider

```rust
pub trait Provider: Producer + Receiver {
    fn schemes(&self) -> &[&str];                             // e.g. ["chan"]

    // Defaulted: Ok(()).
    fn setup(&self) -> BoxFuture<'_, Result<(), MessagingError>>;
    // Defaulted: does nothing.
    fn close(&self) -> BoxFuture<'_, Result<(), MessagingError>>;
    // Defaulted: claims nothing.
    fn capabilities(&self) -> Capabilities;
    // Defaulted: returns MessagingError::Unsupported.
    fn set_observer(&self, observer: Arc<dyn Observer>) -> Result<(), MessagingError>;
}
```

A full backend: both a [`Producer`](#producer) and a [`Receiver`](#receiver), plus the URL `schemes` it handles, a `setup` hook to connect / declare topology, a `close` hook for a graceful shutdown, a statement of what it supports, and a way to install an [`Observer`](#observer) through `Arc<dyn Provider>`.

### Capabilities

```rust
#[non_exhaustive]
pub struct Capabilities {
    pub delay: bool,               // SendOptions::delay
    pub deliver_at: bool,          // SendOptions::deliver_at
    pub ttl: bool,                 // SendOptions::ttl
    pub priority: bool,            // message priority
    pub ordering: bool,            // ordering by routing key
    pub filters: bool,             // attribute filters on subscriptions
    pub dedup: bool,               // de-duplication
    pub delivery_context: bool,    // add_delivery_listener: delivery details, lease extension, deferred acks
    pub native_batch: bool,        // a batch send that reports per-entry results
    pub max_message_bytes: Option<usize>,
    pub max_batch_len: Option<usize>,
    pub max_attributes: Option<usize>,
}
```

A field left `false` / `None` means "no" or "not stated". Outside the crate build one with `Capabilities::new()` and the chainable setter named after each field (`.delay(true)`, `.max_message_bytes(1024)`, …). Application code can ask before relying on a feature — `registry.capabilities_for("sqs://orders")?.delay` — and an adapter uses its own capabilities with [`SendOptions::unsupported_by`](#sendoptions).

For [`LocalProvider`](#localprovider): `schemes()` returns `["chan", "topic", "reply"]`, `setup()` is a no-op, `close()` is a [draining shutdown](#graceful-shutdown-and-draining) that fails with `MessagingError::Undelivered(n)` if messages were left over, `set_observer` installs the observer (like the inherent `LocalProvider::set_observer`), and `capabilities()` claims `delay`, `deliver_at`, `ttl`, `priority`, `ordering`, `filters`, `dedup` and `delivery_context`, with no native batch and no size limits.

### Implementing a provider

`Provider` is the extension point for external brokers — Kafka, NATS, SQS, Google Pub/Sub. No such backend ships with ferroly; **durable backends are out of scope for this crate** and belong in adapter crates that implement these traits. Application code written against `Arc<dyn Provider>` (or a [`ProviderRegistry`](#providerregistry)) then does not change when the backend does.

The traits are object-safe (every method returns a [`BoxFuture`](#boxfuture)), and only two methods are required: `Producer::send` and `Provider::schemes`. What an adapter is expected to do:

- **Addresses.** Claim one or more schemes in `schemes()` (`"kafka"`, `"nats"`, `"sqs"`); use `split_address(addr)` to separate the scheme from the destination name.
- **Sending.** Map `Message::properties` and `Message::attributes` onto the broker's native fields — or, for a broker with one flat attribute map, use [`to_wire_attributes()` / `from_wire`](#the-wire-form) — plus `Message::body`, `Message::routing_key` (partition / group key) and `SendOptions`.
- **Options.** A provider must **refuse** a send whose options it cannot honour when they change what delivery means — `delay`, `deliver_at`, `ttl` — by returning `InvalidOptions` (use `SendOptions::unsupported_by`). It may ignore hints (`on_full`, `block_on_full`) and `extensions` that are not its own.
- **Failures.** Return `MessagingError::refused(…)` / `refused_transient(…)` (handing the message back) when one message is not accepted, and `backend(…)` / `backend_transient(…)` for transport failures. See [error handling](#error-handling).
- **Receipts and batches.** Override `send_with_receipt` if the broker assigns ids or offsets, and `send_batch` if it has a native batch call — reporting every failed entry in [`BatchFailure::failures`](#batch-send).
- **Receiving.** Drive the `Handler` and translate its `Ack` into the broker's acknowledgement: `Ok` → ack/commit, `Retry` → nack/redeliver (bounded by `max_delivery_attempts`, then `dead_letter`), `RetryAfter(d)` → nack with a redelivery delay (a visibility-timeout change), `Reject` → discard, `DeadLetter` → move to the dead-letter destination now, `Pending` → wait for the `Acker`. Set `Message::delivery_count` from the broker's redelivery counter. Contain handler panics and treat them as `Retry`. When dead-lettering, add the four `dead-letter-*` attributes. Override `remove_listeners` if the broker can unsubscribe.
- **Delivery context.** To support `add_delivery_listener`, build one [`Delivery`](#delivery-context) per delivery: `Delivery::new(message, info, control)` with a `DeliveryInfo` (`DeliveryInfo::new(addr).with_attempt(n).with_enqueued_at(t).with_sequence(s)`) and an `Arc<dyn DeliveryControl>` of your own. `extend` renews the broker lease, `set_dead_letter_description` stores the text for a later dead-letter, and `resolve(ack)` hands a deferred `Ack` back to the code that is waiting on a `Pending` delivery (a one-shot channel works well; return `true` only for the call that settled it). Keep the receiving end in the consumer and the sending end inside the control, so that when every `Acker` is dropped unsettled the consumer sees the channel close and retries. A plain `Handler` is adapted by calling it with `delivery.message` and treating `Pending` as `Retry`. Claim `delivery_context` in `capabilities()`.
- **Capabilities and observability.** Report what the backend supports in `capabilities()`; accept an observer in `set_observer` and fire its hooks (at least `on_published` / `on_send_failed`).
- **Lifecycle.** Connect and declare topology in `setup`; flush, drain and disconnect in `close`. Both default to doing nothing.
- **Publish-only providers** implement `Producer`, write `impl Receiver for X {}`, and implement `Provider::schemes` (and usually `capabilities`).

The semantics a backend actually provides — durability, ordering, delivery guarantee — are the backend's; the trait does not promise more than the underlying broker does.

A complete publish-only provider:

```rust
use ferroly::messaging::{
    BoxFuture, Capabilities, Message, MessagingError, Producer, Provider, Receiver, SendOptions,
};

struct QueueClient { /* the broker's SDK client */ }

impl Producer for QueueClient {
    fn send(&self, addr: &str, msg: Message, opts: SendOptions)
        -> BoxFuture<'_, Result<(), MessagingError>>
    {
        let addr = addr.to_string();
        Box::pin(async move {
            if let Some(option) = opts.unsupported_by(&self.capabilities()) {
                return Err(MessagingError::InvalidOptions(format!("{option} is not supported")));
            }
            if msg.body.len() > 256 * 1024 {
                return Err(MessagingError::refused("message too large", addr, msg));
            }
            let queue = ferroly::messaging::split_address(&addr).1.to_string();
            let group = opts.extensions.get_str("queue.group").map(str::to_string);
            match self.put(&queue, msg.to_wire_attributes(), &msg.body, group).await {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    Err(MessagingError::refused_transient(e, addr, msg))   // throttled
                }
                Err(e) => Err(MessagingError::backend_transient(e)),
            }
        })
    }
}

impl Receiver for QueueClient {}   // publish-only: add_listener is Unsupported

impl Provider for QueueClient {
    fn schemes(&self) -> &[&str] { &["queue"] }
    fn capabilities(&self) -> Capabilities {
        Capabilities::new().max_message_bytes(256 * 1024)
    }
}
# impl QueueClient {
#     async fn put(&self, _: &str, _: ferroly::messaging::Attributes, _: &[u8], _: Option<String>)
#         -> Result<(), std::io::Error> { Ok(()) }
# }
```

### ProviderRegistry

Routes sends and listeners to the provider that owns an address's scheme, so one handle can front several backends. It implements `Producer` and `Receiver` itself, forwarding every method (including `send_with_receipt` and `send_batch`) to the provider for the address.

```rust
use std::sync::Arc;
use ferroly::messaging::{
    handler, Ack, LocalProvider, Message, MessagingError, Producer, ProviderRegistry, Receiver,
};

# async fn ex() -> Result<(), MessagingError> {
let registry = ProviderRegistry::new();
registry.register(Arc::new(LocalProvider::new("local")))?;   // chan://, topic://, reply://
// registry.register(Arc::new(nats_provider))?;              // nats://
registry.setup_all().await?;

registry
    .add_listener("chan://orders", handler(|_m| async { Ack::Ok }), Default::default())
    .await?;
registry
    .send("chan://orders", Message::text("o-1"), Default::default())
    .await?;
assert!(registry.capabilities_for("chan://orders")?.delay);

registry.close_all().await?;
# Ok(())
# }
```

| Method | Description |
| --- | --- |
| `register(&self, Arc<dyn Provider>) -> Result<(), MessagingError>` | Register a provider for every scheme it reports; `InvalidOptions` (registering nothing) if a scheme is already taken. |
| `provider_for(&self, addr) -> Result<Arc<dyn Provider>, MessagingError>` | The provider for an address; `InvalidAddress` if the address has no scheme or an unregistered one. |
| `schemes(&self) -> Vec<String>` | The registered schemes, sorted. |
| `providers(&self) -> Vec<Arc<dyn Provider>>` | The registered providers, each once (a provider registered under several schemes is listed a single time), in no particular order. |
| `capabilities_for(&self, addr) -> Result<Capabilities, MessagingError>` | The capabilities of the provider that handles `addr`. |
| `setup_all(&self).await -> Result<(), MessagingError>` | Calls `setup` on every provider; all are attempted, the first error is returned. |
| `close_all(&self).await -> Result<(), MessagingError>` | Calls `close` on every provider; all are attempted, the first error is returned. |
| `set_observer(&self, Arc<dyn Observer>) -> Result<(), MessagingError>` | Installs the observer on every provider that accepts one; providers that return `Unsupported` are skipped, the first other error is returned. |

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
    pub drain_timeout: Duration,   // default 20 s — used by `stop` and `close`
    pub dedup_window: usize,       // default 0 (de-duplication off)
}
```

An in-process bus that implements [`Provider`](#provider) over bounded in-memory buffers. It is cheap to `Clone` (an `Arc` handle to shared state), so you can hand copies to producers freely.

- `new(id)` / `with_options(id, options)` — creates the bus with a component id (used by the [lifecycle](#lifecycle-integration) integration).
- `set_observer(obs)` — installs an [`Observer`](#observer).
- `pending()` — messages accepted and not yet completed, across all destinations (buffered, delayed, or being handled). A send that is waiting for buffer room (`OnFull::Block`) is counted too.
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
- **The concrete topic** is stamped on every copy in the [`topic` property](#properties) (`msg.topic()`), so a wildcard subscriber can tell which topic matched.
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

### Filtering subscriptions

A `topic://` listener can take only the messages that match a **filter** on their properties and attributes. Set [`ListenerOptions::filter`](#listeneroptions):

```rust
#[non_exhaustive]
pub enum Filter {
    Exists(String),               // the name is present
    Eq(String, AttrValue),        // the value equals this one
    In(String, Vec<AttrValue>),   // the value equals one of these
    Prefix(String, String),       // the value is a string starting with this
    Gt(String, f64),              // the value is a number >
    Gte(String, f64),             //                       >=
    Lt(String, f64),              //                       <
    Lte(String, f64),             //                       <=
    And(Vec<Filter>),             // all match (an empty list matches everything)
    Or(Vec<Filter>),              // any matches (an empty list matches nothing)
    Not(Box<Filter>),             // the inner filter does not match
}
```

Build one with the constructors — `Filter::exists(name)`, `eq(name, value)`, `one_of(name, values)`, `prefix(name, prefix)`, `gt` / `gte` / `lt` / `lte(name, n)`, `all(filters)`, `any(filters)`, `not(filter)` — and test one by hand with `filter.matches(&msg)`.

```rust
use ferroly::messaging::{handler, Ack, Filter, ListenerOptions, LocalProvider, Message, Producer, Receiver};

# async fn ex() {
let bus = LocalProvider::new("bus");
bus.add_listener(
    "topic://orders.*",
    handler(|m: Message| async move {
        println!("big EU order: {}", m.body_str());
        Ack::Ok
    }),
    ListenerOptions {
        filter: Some(Filter::all([
            Filter::eq("region", "eu"),
            Filter::gte("amount", 100.0),
        ])),
        ..Default::default()
    },
)
.await
.unwrap();

let order = Message::text("o-1").with_attribute("region", "eu").with_attribute("amount", 250);
bus.send("topic://orders.created", order, Default::default()).await.unwrap();
# }
```

The rules:

- **Lookup.** A name is looked up with [`Message::lookup`](#the-wire-form): a set property under its wire name (`subject`, `event-type`, `priority`, …) first, then the first application attribute with that name. The `topic` property is stamped on the message only *after* routing, so a filter cannot see it — use the subscription pattern for that.
- **Missing names.** A missing name makes every leaf filter false (`Exists`, `Eq`, `In`, `Prefix` and the comparisons). So `Filter::not(Filter::exists("x"))` is true when `x` is missing.
- **Equality.** Two numbers (`Int` or `Float`, in any combination) compare as numbers, so `Int(5)` equals `Float(5.0)`. Anything else must be the same variant with the same value: `Str("5")` does **not** equal `Int(5)`.
- **Prefix** matches only `Str` values.
- **Comparisons** need a number; a non-number value, or a `NaN` on either side, is false.
- **Topic subscriptions only.** A filter on a `chan://` listener is rejected with `InvalidOptions`.
- **Shared subscriptions agree on one filter.** Listeners that share a [subscription name](#publishsubscribe-topics) on a pattern form one subscription with one filter; adding a listener with a different filter is `InvalidOptions`.
- **Every subscription filtered out.** When at least one subscription's pattern matched but every such subscription's filter excluded the message, `send` still returns `Ok(())`, and the message is reported through `on_dropped(…, DropReason::Filtered)` (no pattern matching at all is still `DropReason::NoSubscribers`).

`LocalProvider` evaluates filters in process. For adapter authors, the same filters map onto broker-side features:

| Broker | Mapping |
| --- | --- |
| AWS SNS filter policies | `Eq` / `In` → a list of exact values; `Prefix` → `{"prefix": …}`; comparisons → `{"numeric": [">=", n]}`; `Exists` / `Not(Exists)` → `{"exists": true/false}`; `And` → several keys in one policy; `Or` → `$or`. |
| Google Pub/Sub subscription filters | `Eq` → `attributes.name = "v"`; `Exists` → `attributes:name`; `Prefix` → `hasPrefix(attributes.name, "p")`; `And` / `Or` / `Not` → `AND` / `OR` / `NOT`. Pub/Sub attributes are strings, so numeric comparisons have no equivalent. |
| Azure Service Bus correlation filters | `Eq` on system properties (`correlation-id`, `subject`, `content-type`, `reply-to`, `message-id`) and on application properties, combined with `And`; anything richer needs a SQL filter. |

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
- **Priority:** a message with a [priority](#priority) above 0 overtakes lower-priority messages already buffered for the same listener, so per-key order holds only among messages of equal priority.

### Priority

`Message::with_priority(p)` (0, the default, to 255) lets a message overtake others waiting in a listener's buffer. When a message becomes ready — on acceptance, or when its delay is up — it is placed **ahead of every buffered message of lower priority and behind every one of the same or higher priority**, so messages of equal priority stay first-in, first-out.

```rust
use ferroly::messaging::{LocalProvider, Message, Producer};

# async fn ex(bus: LocalProvider) {
bus.send("chan://jobs", Message::text("batch report"), Default::default()).await.unwrap();
bus.send("chan://jobs", Message::text("page the on-call").with_priority(9), Default::default())
    .await
    .unwrap();   // handled before the report if the report is still buffered
# }
```

Priority only reorders what is **buffered**: it does not interrupt a message a handler is already working on (or backing off on), and it does not move messages between listeners. Priority 0 messages are never moved ahead of anything.

### Ack/redelivery & dead-lettering

When a handler returns [`Ack::Retry`](#ack) (or panics):

1. The provider increments `delivery_count`.
2. If the new `delivery_count` is **below** `max_delivery_attempts`, it fires `on_nack(requeue = true)`, waits for [`retry_backoff`](#backoff)`.delay(delivery_count)`, and redelivers the message to the same listener.
3. Once `delivery_count` **reaches** `max_delivery_attempts`, it fires `on_nack(requeue = false)`, then:
   - if a [`dead_letter`](#listeneroptions) destination is configured, moves the message there and fires `on_dead_letter(to_dlq = true)`;
   - otherwise drops the message and fires `on_dead_letter(to_dlq = false)` so the loss is observable rather than silent.

The dead-letter move waits for room in the destination's buffer rather than dropping under load. If the dead-letter destination has no listener, the message is lost and that is reported as `on_dead_letter(to_dlq = false)`. The message keeps its `delivery_count` when it arrives in the dead-letter queue, so the handler there can see how many attempts were made.

`Ack::Reject` skips all of this and discards immediately, firing `on_nack(requeue = false)`. `Ack::DeadLetter` skips the remaining attempts and goes to step 3 at once (firing `on_nack(requeue = false)`), and `Ack::RetryAfter(d)` is a `Retry` that waits `d` instead of the backoff.

**Why it was dead-lettered.** Before a message is moved (or dropped for want of a destination) the provider adds four application attributes; the names are constants in `headers`:

| Attribute | Type | Value |
| --- | --- | --- |
| `dead-letter-reason` (`DEAD_LETTER_REASON`) | string | `max-delivery-attempts` (retries exhausted), `rejected` (the handler returned `Ack::DeadLetter`, or an `Acker` called `dead_letter`), or `expired` (the TTL ran out, with `ExpiryAction::DeadLetter`) |
| `dead-letter-source` (`DEAD_LETTER_SOURCE`) | string | the canonical address it was consumed from, e.g. `chan://orders` |
| `dead-letter-attempts` (`DEAD_LETTER_ATTEMPTS`) | int | how many delivery attempts it had |
| `dead-letter-description` (`DEAD_LETTER_DESCRIPTION`) | string | only when there is one: the text set with `Delivery::dead_letter_description` / `Acker::dead_letter`, or — for exhausted retries — the panic message of a handler that panicked on its last attempt |

The message keeps its `delivery_count` unchanged.

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

[`SendOptions::deliver_at`](#sendoptions) schedules a message for an **absolute time** instead (a `SystemTime`). It behaves like a delay of `at - now`, measured when the message is accepted; a time in the past means "now". Setting both `delay` and `deliver_at` on one send is `InvalidOptions`.

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

// The same, at a fixed time.
let at = std::time::SystemTime::now() + Duration::from_secs(60);
bus.send("chan://reminders", Message::text("follow up"), SendOptions::new().deliver_at(at))
    .await
    .unwrap();
# }
```

The delay lives in memory only: a delayed message does not survive a restart, and a [shutdown](#graceful-shutdown-and-draining) hands back any message that would only become due after the drain deadline.

### De-duplication

With [`LocalOptions::dedup_window`](#localprovider) set to `n > 0`, the provider remembers the last `n` **de-duplication keys** accepted **per destination**. A message's key is its `dedup_key` property ([`with_dedup_key`](#properties)) or, when that is unset, its message id. A `send` whose key is still in that window is treated as a duplicate: it returns `Ok(())`, the message is not delivered, and `on_dropped(…, DropReason::Duplicate)` fires.

Use a `dedup_key` when the logical identity of a message is not its id — for example two sends with fresh ids that both describe "order 17 was paid":

```rust
use ferroly::messaging::Message;

let first = Message::text("paid").with_new_id().with_dedup_key("order-17/paid");
let again = Message::text("paid").with_new_id().with_dedup_key("order-17/paid");   // a duplicate
# let _ = (first, again);
```

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

- Messages with **neither** a dedup key nor an id are never de-duplicated.
- The window is bounded by **count**, not time: once `n` newer keys have been accepted for the destination, an old key is forgotten.
- A send that fails (`NoListeners`, `BufferFull`, …) does not record its key, so retrying the message you were handed back works.
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

- gives the message a [message id and correlation id](#properties) if it has none;
- sets its `reply_to` property to a private `reply://…` inbox;
- gives it a **TTL equal to the timeout**, so a request still waiting in a buffer when the caller gives up expires instead of being handled for nobody;
- waits up to `timeout` for the reply, then returns `MessagingError::RequestTimeout(addr)`.

On the answering side, `responder(f)` sends `f`'s `Ok(reply)` to the request's reply address — marked with [`in_reply_to`](#properties), so the correlation id and trace context carry over — and acks. `Err(ack)` retries or rejects the request without replying. To reply by hand, call `bus.reply(&request, reply)` from a handler holding a [weak handle](#handle-lifetime). A reply whose requester is no longer waiting fails with `NoListeners`; `responder` ignores that.

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

Things to keep in mind:

- Sends made **by handlers** are refused during the drain like any other send. A handler that forwards to another destination should return `Ack::Retry` when its send fails; if the drain ends first, the message is reported as undelivered.
- A handler that blocks its thread without ever awaiting cannot be cancelled. `shutdown` still returns at the deadline, but that handler's task runs on until it yields.
- **An abandoned shutdown still stops the provider.** If the `shutdown` future is dropped before it completes — typically because a supervisor's own stop timeout fired first — the drain is cut short: consumers are stopped at once and the provider ends up stopped, as if the drain timeout had passed. The undelivered messages are still reported through `on_dropped`; only the `DrainReport` is lost with the future. For that reason keep `LocalOptions::drain_timeout` (default 20 s) **below** the stop timeout of whatever supervises the provider (the lifecycle manager's default is 30 s), so the drain finishes — and reports — on its own terms.

### Listener self-healing

Each listener is a task serving its own buffer. If a listener is removed, or its task ends for any reason, its buffer is closed and the provider stops selecting it on the next send, so the remaining listeners keep receiving. A handler panic does **not** end the task — it is [contained and retried](#ack). If no listener is left on a queue, `send` returns [`MessagingError::NoListeners`](#error-handling).

## Observer

```rust
pub trait Observer: Send + Sync {
    fn on_published(&self, addr: &str, receipt: &Receipt) {}
    fn on_send_failed(&self, addr: &str, error: &MessagingError) {}
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

Hooks for every step of a message's life. Every method has a default no-op body, so you only override what you care about. Install one with [`LocalProvider::set_observer`](#localprovider), with `Provider::set_observer` through an `Arc<dyn Provider>`, or on every provider of a registry with [`ProviderRegistry::set_observer`](#providerregistry).

`addr` is the canonical address of the listener's buffer (`chan://orders`, `topic://orders.*`) — a stable value to use as a metrics label. Callbacks run synchronously on the task that caused the event and never while a provider lock is held; keep them short and non-blocking. A panic inside a callback is caught and discarded.

| Hook | Fired when | Metric |
| --- | --- | --- |
| `on_published` | a `send` was accepted by the provider; **once per send**, however many topic subscriptions got a copy. `addr` is the address passed to `send`; `receipt` is what [`send_with_receipt`](#receipts) would return | published |
| `on_send_failed` | a `send` failed; `error.message()` is the message when it was handed back | send failures |
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
    Filtered,       // matching subscriptions existed, but every one's filter excluded it
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

## Tracing spans: `Traced`

With the `obs` feature, `Traced<P>` wraps any `Producer` / `Receiver` / `Provider` (it implements the same traits, so it can stand in as an `Arc<dyn Provider>`) and records [`ferroly::obs`](obs.md) spans. Without the wrapper nothing is traced.

```rust
# #[tokio::main]
# async fn main() {
use ferroly::messaging::{handler, Ack, LocalProvider, Message, Producer, Receiver, Traced};

let bus = Traced::new(LocalProvider::new("bus"), "local"); // "local" = messaging.system
bus.add_listener("chan://orders", handler(|_msg| async { Ack::Ok }), Default::default())
    .await
    .unwrap();
bus.send("chan://orders", Message::text("order-1"), Default::default()).await.unwrap();
bus.inner().shutdown(std::time::Duration::from_secs(5)).await;
# }
```

| Operation | Span | Parent | Effect on the message |
|---|---|---|---|
| `send`, `send_with_receipt` | `Producer`, `send {destination}` | the message's own `traceparent` if it has one, else the current context (as seen when the returned future is first polled) | `traceparent` / `tracestate` set to the producer span's context |
| `send_batch` | one `Producer` span for the batch, with `messaging.batch.message_count` | the current context; a message that carried a context is **linked** | every message gets the batch span's context |
| `add_listener`, `add_delivery_listener` | `Consumer`, `process {destination}`, per delivery (each redelivery is its own span) | extracted from the message's `traceparent` | none |

- The destination is the address without its scheme (`orders` for `chan://orders`); fields follow the OpenTelemetry messaging conventions (`messaging.system`, `messaging.destination.name`, `messaging.operation.type`, `messaging.message.id`, `messaging.message.delivery_count`).
- A failed send marks the producer span as an error.
- The consumer span is current while the handler runs, including the call that creates the handler's future (code a raw `Handler` runs before returning it), so the handler's own spans (and follow-up sends through the wrapper) are its children. A handler that panics ends the span with the error `handler panicked`.
- The handler's verdict is recorded as `messaging.ack` = `ok`, `retry`, `retry_after`, `reject`, `dead_letter` or `pending`. Every verdict except `ok` and `pending` also sets an error status (`message not processed: retry`, …). A pending delivery's span ends when the handler returns, not when the message is settled later.
- `inner()` / `into_inner()` reach the wrapped provider's own methods (`LocalProvider::shutdown`, …).

## LogObserver

**Requires the `log` feature.**

```rust
#[cfg(feature = "log")]
pub struct LogObserver { /* … */ }

impl LogObserver {
    pub fn new(logger: ferroly::log::Logger) -> Self;
}
```

An [`Observer`](#observer) that emits structured logs through [`ferroly::log`](log.md): `debug` on publish/send/receive/ack, `warn` on a failed send, nack, dead-letter, expiry and drop, and `error` on a handler panic, with fields such as `addr`, `message_id`, `error`, `retryable`, `bytes`, `delivery`, `requeue`, `dead_lettered`, `dropped`, `reason` and `panic`.
 It is the zero-code way to get message-level visibility — just hand it a [`Logger`](log.md#logger).

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

#[derive(ferroly::codec::Encode, ferroly::codec::Decode)]
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
    InvalidOptions(String),     // e.g. buffer_size: 0, or an option the provider cannot honour
    Unsupported(String),        // the provider does not implement the operation
    Undelivered(usize),         // shutdown left this many messages undelivered
    PartialBatch(BatchFailure), // some messages of a batch were not accepted
    Backend {                   // a failure from an external backend
        source: Box<dyn Error + Send + Sync>,
        transient: bool,
    },
    Refused {                   // a backend refused one message and hands it back
        source: Box<dyn Error + Send + Sync>,
        rejected: Rejected,
        transient: bool,
    },
}

pub struct Rejected {
    pub destination: String,
    pub message: Option<Box<Message>>,
}
```

`MessagingError` derives ferroly's error machinery (via `ferroly_derive::FerrolyError`) — it implements `std::error::Error` and `Display`. `Backend` and `Refused` report their backend error through `source()`. Because `Backend` and `Refused` have named fields, match them with `{ .. }` (`MessagingError::Backend { source, .. }`).

**Rejected sends hand the message back.** The four send-side variants and `Refused` carry a `Rejected`: the destination plus the message that was not accepted. Recover it instead of losing it:

| Method | Description |
| --- | --- |
| `MessagingError::into_message(self) -> Option<Message>` | Take the rejected message back. |
| `MessagingError::take_message(&mut self) -> Option<Message>` | Take it back, leaving the error in place (to log or wrap it afterwards). |
| `MessagingError::message(&self) -> Option<&Message>` | Look at it without consuming the error. |
| `MessagingError::destination(&self) -> Option<&str>` | The destination the operation was addressed to. |
| `MessagingError::is_retryable(&self) -> bool` | Whether the same operation may succeed if tried again later. |
| `Rejected::into_message(self) -> Option<Message>` | The same, on the payload. |

```rust
use ferroly::messaging::{LocalProvider, Message, Producer};

# async fn ex(bus: LocalProvider, parked: &mut Vec<Message>) {
if let Err(error) = bus.send("chan://orders", Message::text("o-1"), Default::default()).await {
    eprintln!("send failed: {error}");
    if error.is_retryable() {
        if let Some(msg) = error.into_message() {
            parked.push(msg); // retry later
        }
    }
}
# }
```

`Rejected::message` is `None` only when the failing operation carried no message (for example `add_listener` on a stopped provider returns `Closed`).

**Transient or permanent.** `is_retryable()` is `true` for `BufferFull`, `SendTimeout`, and for `Backend` / `Refused` built as transient; `true` for `PartialBatch` when at least one of its entries is retryable; `false` for everything else. With the `errutils` feature, the root `ferroly::Error` classifies messaging errors the same way: `BufferFull` / `SendTimeout` / `Closed` and transient `Backend` / `Refused` are `Unavailable`, `NoListeners` is `NotFound`, `RequestTimeout` is `Timeout`, `InvalidAddress` / `InvalidOptions` are `InvalidInput`, `Unsupported` is `Unsupported`, the rest `Unknown`.

**For provider implementations**, four constructors build the backend variants:

| Constructor | Variant | Use it for |
| --- | --- | --- |
| `MessagingError::backend(err)` | `Backend { transient: false }` | a transport or protocol failure that will repeat (bad credentials, a missing queue) |
| `MessagingError::backend_transient(err)` | `Backend { transient: true }` | an outage, a timeout, a dropped connection |
| `MessagingError::refused(err, addr, msg)` | `Refused { transient: false }` | the broker will never take this message (too large, invalid) — the message is handed back |
| `MessagingError::refused_transient(err, addr, msg)` | `Refused { transient: true }` | the broker will not take it *now* (throttled) — the message is handed back |

Prefer `refused*` whenever the adapter still holds the message, so the caller never loses it.

- [`send`](#producer) to a queue returns `NoListeners` if nothing is listening, `BufferFull`/`SendTimeout` per the [backpressure policy](#backpressure), and `Closed` if the provider is draining or stopped.
- [`send`](#producer) to a topic returns `Ok(())` even if some or all subscriptions did not take the message; see [topics](#publishsubscribe-topics).
- [`send_batch`](#batch-send) returns `PartialBatch` with the detail of what was and was not accepted.
- [`add_listener`](#receiver) returns `InvalidOptions`, `InvalidAddress`, or `Closed`.
- Redelivery and dead-letter moves happen inside the provider and cannot be returned to a caller. A dead-letter move that fails is reported through [`Observer::on_dead_letter`](#observer) with `to_dlq = false`. Configure the DLQ listener before you start producing, and watch that hook.

## Migrating from 0.4

0.5 changes the shape of several messaging types. Most code needs only the
renames below; the behaviour changes are listed after the tables.

### `Message`

| 0.4 | 0.5 |
|---|---|
| `msg.headers` (`codec::Value`) | `msg.attributes` (`Attributes`) for application data; `msg.properties` (`Properties`) for system metadata |
| `msg.with_header(k, v)` | `msg.with_attribute(k, v)` |
| `msg.set_header(k, v)` | `msg.set_attribute(k, v)` |
| `msg.header(k)` → `Option<&Value>` | `msg.attribute(k)` → `Option<&AttrValue>` |
| `msg.header_str(k)` | `msg.attribute_str(k)` |
| `msg.with_header(headers::MESSAGE_ID, id)` | `msg.with_id(id)` (and likewise `with_correlation_id`, `with_reply_to`, …) |
| `msg.header_str(headers::TOPIC)` | `msg.topic()` |
| `Message { body, ..Default::default() }` | `Message::new(body)` — `Message` is `#[non_exhaustive]` |
| a `u64` header value | cast to `i64`: `with_attribute("n", n as i64)` |
| nested / array header values | not supported; encode into the body or a string attribute |
| `msg.decode::<T>()` is always JSON | uses `content_type` when the codec knows it, else JSON |

### Acknowledgements and errors

| 0.4 | 0.5 |
|---|---|
| `match ack { Ack::Ok => …, Ack::Retry => …, Ack::Reject => … }` | add a wildcard arm; `Ack` is `#[non_exhaustive]` and has `RetryAfter`, `DeadLetter`, `Pending` |
| `MessagingError::Batch(f)` | `MessagingError::PartialBatch(f)` |
| `f.error` | `f.first_error()` or `f.failures[i].error` |
| `f.unsent` (failed message first, then the rest) | `f.failures[i].message` for failed entries, `f.unsent` for those never tried; `f.into_messages()` for all of them |
| `MessagingError::Backend { source }` (pattern) | `MessagingError::Backend { source, .. }` |
| a backend error that loses the message | `MessagingError::refused(err, addr, msg)` |
| a provider ignores an unsupported `delay` / `ttl` | it must return `InvalidOptions` (`SendOptions::unsupported_by`) |

### Providers

| 0.4 | 0.5 |
|---|---|
| a publish-only provider stubs `add_listener` | `impl Receiver for P {}` |
| `Provider::setup` is required | optional (defaults to `Ok(())`) |
| de-duplication is by message id | by `dedup_key`, then message id |

Behaviour changes to check:

- **`Message::decode`** uses the message's content type when the codec knows it
  (JSON, XML, YAML, TOML) and falls back to JSON. `Message::json` now sets the
  content type to `application/json`.
- **Providers must refuse what they cannot honour.** A provider that cannot do
  `delay`, `deliver_at` or `ttl` returns `InvalidOptions` instead of delivering
  the message differently. `LocalProvider` supports all three.
- **Dead-lettered messages carry four extra attributes**
  (`dead-letter-reason`, `-source`, `-attempts`, `-description`); their
  `delivery_count` is unchanged.
- **Priority reorders a listener's buffer**: a message with a priority above 0
  overtakes lower-priority ones, so per-key order holds only among messages of
  equal priority.

## Limitations

- **In-process, in-memory only.** No persistence: nothing survives a restart or a crash, and nothing crosses a process boundary. See the [delivery guarantee](#delivery-guarantee).
- **No durable or external backends are included.** Kafka, NATS, SQS and similar belong in adapter crates [implementing `Provider`](#implementing-a-provider).
- **Deferred:** a transactional outbox (atomically committing a database change and a message), and consumer groups / shared subscriptions **across processes**. Named subscriptions give you competing consumers within one process only.
- **At-least-once, not exactly-once.** A redelivered message reaches the handler again; make handlers idempotent. [De-duplication](#de-duplication) covers repeated sends, not redeliveries.
- **Retry blocks the listener.** A message that is backing off holds its place ([ordering guarantee](#ordering-guarantee)), so a serial listener handles nothing else meanwhile. Use `concurrency`, a shorter [`Backoff`](#backoff), or a lower `max_delivery_attempts` with a dead-letter queue if that matters.
- **Topics have no replay or retention.** A subscription receives only what is published after it was added.
- **De-duplication is bounded by count**, per destination, in memory.
- **DLQ moves can still fail.** If the `dead_letter` destination has no listener, the exhausted message is lost — reported via `on_dead_letter(to_dlq = false)`.
- **Filters are topic-only.** Queues route on the address and the [routing key](#queues-competing-consumers--routing-key-affinity) only; [attribute filters](#filtering-subscriptions) apply to `topic://` subscriptions and cannot see the `topic` property.
- **Cancellation is cooperative.** A handler cancelled at a shutdown deadline stops at its next `.await`.
- **Not provided:** pull or batch consumption (handlers are push-only, one message at a time), replay / retention / seek, transactions, a claim-check helper for payloads over a broker's size limit, an admin API (create / delete / purge / peek destinations), and cancelling a scheduled message. External providers can still offer these through their own types.
- **Leases are a no-op locally.** `Delivery::extend` always succeeds on `LocalProvider`, which has no visibility timeout; the call matters for providers over a broker.

### Design rationale recap

- **Cancellation is by dropping the future** — there are no separate context-carrying method variants to thread through.
- **Properties, attributes and body live in one [`Message`](#message) struct**, rather than a split interface.
- **Messages are built with the [`Message::new` / `text` / `json`](#constructors) constructors.**
- **A handler signals its verdict by returning an [`Ack`](#ack)**, not by mutating the message — explicit and side-effect-free.
- **A rejected message goes back to its sender**, and a discarded one is always reported.

## See also

- [codec](codec.md) — `Encode`, `Decode` used for bodies.

- [lifecycle](lifecycle.md) — the `Component` trait `LocalProvider` implements.
- [log](log.md) — backs [`LogObserver`](#logobserver).

---
**Related:** [codec](codec.md), [lifecycle](lifecycle.md), [log](log.md).

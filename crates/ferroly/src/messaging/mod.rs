//! Provider-agnostic messaging with the full local-provider feature set:
//! ack/redelivery, routing keys, DLQ, consumer concurrency, backpressure,
//! observability, and lifecycle integration.
//!
//! Design choices:
//! - A handler **returns an [`Ack`]** rather than mutating the message in
//!   place — the disposition is explicit, with no interior mutability.
//! - A message is one [`Message`] struct plus a codec, not a split
//!   header/body/message interface trio.
//! - Destinations are addressed by a `&str` (`chan://name`); the scheme is
//!   stripped to the destination name.
//! - Async work is cancelled by dropping the future, so there are no separate
//!   context-carrying method variants.
//!
//! ```
//! # use ferroly::messaging::{handler, Ack, LocalProvider, Message, Producer, Receiver};
//! # #[tokio::main]
//! # async fn main() {
//! let bus = LocalProvider::new("bus");
//! bus.add_listener("chan://orders", handler(|msg| async move {
//!     println!("got {}", msg.body_str());
//!     Ack::Ok
//! }), Default::default()).await.unwrap();
//!
//! bus.send("chan://orders", Message::text("order-1"), Default::default()).await.unwrap();
//! # }
//! ```

#![deny(missing_docs)]

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use ferroly::codec::{json, CodecError, Decode, Encode, Value};
use ferroly_derive::FerrolyError;
use tokio::sync::{mpsc, Semaphore};

/// A boxed, `Send` future — the object-safe async desugaring.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// ---- message -------------------------------------------------------------

/// A message: headers, a byte body, an optional routing key, and a delivery
/// counter (incremented on each redelivery).
#[derive(Debug, Clone, Default)]
pub struct Message {
    /// Metadata headers.
    pub headers: Value,
    /// The payload bytes.
    pub body: Vec<u8>,
    /// A routing/partition key — same key always lands on the same listener.
    pub routing_key: Option<String>,
    /// How many times this message has been (re)delivered (starts at 0).
    pub delivery_count: u32,
}

impl Message {
    /// A message with the given raw body.
    pub fn new(body: impl Into<Vec<u8>>) -> Self {
        Self {
            body: body.into(),
            ..Default::default()
        }
    }

    /// A message whose body is the UTF-8 bytes of `text`.
    pub fn text(text: impl Into<String>) -> Self {
        Self::new(text.into().into_bytes())
    }

    /// A message whose body is the JSON encoding of `value`.
    pub fn json<T: Encode>(value: &T) -> Self {
        Self::new(json::encode(value).into_bytes())
    }

    /// Sets a header.
    pub fn with_header(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        let entry = (key.into(), value.into());
        match &mut self.headers {
            Value::Object(o) => o.push(entry),
            _ => self.headers = Value::Object(vec![entry]),
        }
        self
    }

    /// Sets the routing key.
    pub fn with_routing_key(mut self, key: impl Into<String>) -> Self {
        self.routing_key = Some(key.into());
        self
    }

    /// The body as a UTF-8 string (lossy).
    pub fn body_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }

    /// Decodes the JSON body into `T`.
    pub fn decode<T: Decode>(&self) -> Result<T, CodecError> {
        json::decode_from_slice(&self.body)
    }
}

/// The disposition a listener returns for a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ack {
    /// Processed successfully; remove the message.
    Ok,
    /// Failed transiently; requeue (up to `max_delivery_attempts`, then DLQ).
    Retry,
    /// Failed permanently; discard (no requeue).
    Reject,
}

/// A message handler: given a [`Message`], returns its [`Ack`] disposition.
pub type Handler = Arc<dyn Fn(Message) -> BoxFuture<'static, Ack> + Send + Sync>;

/// Wraps an async closure as a [`Handler`].
pub fn handler<F, Fut>(f: F) -> Handler
where
    F: Fn(Message) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Ack> + Send + 'static,
{
    Arc::new(move |msg| Box::pin(f(msg)))
}

/// Wraps a fallible async closure as a [`Handler`]: `Ok(())` acks, `Err(_)`
/// requeues ([`Ack::Retry`]) — provider redelivery + DLQ take over from there.
pub fn fallible<F, Fut, E>(f: F) -> Handler
where
    F: Fn(Message) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), E>> + Send + 'static,
{
    Arc::new(move |msg| {
        let fut = f(msg);
        Box::pin(async move {
            match fut.await {
                Ok(()) => Ack::Ok,
                Err(_) => Ack::Retry,
            }
        })
    })
}

// ---- options -------------------------------------------------------------

/// Per-send options.
#[derive(Debug, Clone, Default)]
pub struct SendOptions {
    /// Overrides the message's routing key.
    pub routing_key: Option<String>,
    /// Block until the listener's buffer has room instead of erroring when full.
    pub block_on_full: bool,
}

/// Per-listener options.
#[derive(Debug, Clone)]
pub struct ListenerOptions {
    /// Per-listener channel capacity (backpressure bound).
    pub buffer_size: usize,
    /// Bound concurrent handler invocations (unset = serial, ordered).
    pub concurrency: Option<usize>,
    /// Total delivery attempts before a `Retry` is dead-lettered.
    pub max_delivery_attempts: u32,
    /// Destination to route exhausted messages to (`None` = drop + log).
    pub dead_letter: Option<String>,
}

impl Default for ListenerOptions {
    fn default() -> Self {
        Self {
            buffer_size: 256,
            concurrency: None,
            max_delivery_attempts: 3,
            dead_letter: None,
        }
    }
}

// ---- observability -------------------------------------------------------

/// Lifecycle hooks for messages (no-ops by default). Wire one to emit logs,
/// metrics, or traces.
pub trait Observer: Send + Sync {
    /// A message was sent to `addr`.
    fn on_send(&self, addr: &str, msg: &Message) {
        let _ = (addr, msg);
    }
    /// A message was received from `addr`.
    fn on_receive(&self, addr: &str, msg: &Message) {
        let _ = (addr, msg);
    }
    /// A message was acked.
    fn on_ack(&self, addr: &str, msg: &Message) {
        let _ = (addr, msg);
    }
    /// A message was nacked (`requeue` = whether it will be redelivered).
    fn on_nack(&self, addr: &str, msg: &Message, requeue: bool) {
        let _ = (addr, msg, requeue);
    }
    /// A message reached the end of its redelivery attempts. `to_dlq` is `true`
    /// when it was moved to a dead-letter destination, `false` when it was
    /// dropped (no DLQ configured, or the requeue/DLQ send itself failed).
    fn on_dead_letter(&self, addr: &str, msg: &Message, to_dlq: bool) {
        let _ = (addr, msg, to_dlq);
    }
}

/// An [`Observer`] that emits structured logs via [`ferroly::log`]. Requires the
/// `log` feature.
#[cfg(feature = "log")]
pub struct LogObserver {
    logger: ferroly::log::Logger,
}

#[cfg(feature = "log")]
impl LogObserver {
    /// Wraps `logger`.
    pub fn new(logger: ferroly::log::Logger) -> Self {
        Self { logger }
    }
}

#[cfg(feature = "log")]
impl Observer for LogObserver {
    fn on_send(&self, addr: &str, msg: &Message) {
        self.logger.debug(
            "message.send",
            &[
                ("addr", addr.into()),
                ("bytes", (msg.body.len() as u64).into()),
            ],
        );
    }
    fn on_receive(&self, addr: &str, msg: &Message) {
        self.logger.debug(
            "message.receive",
            &[
                ("addr", addr.into()),
                ("delivery", (msg.delivery_count as u64).into()),
            ],
        );
    }
    fn on_ack(&self, addr: &str, _msg: &Message) {
        self.logger.debug("message.ack", &[("addr", addr.into())]);
    }
    fn on_nack(&self, addr: &str, msg: &Message, requeue: bool) {
        self.logger.warn(
            "message.nack",
            &[
                ("addr", addr.into()),
                ("requeue", requeue.into()),
                ("delivery", (msg.delivery_count as u64).into()),
            ],
        );
    }
    fn on_dead_letter(&self, addr: &str, msg: &Message, to_dlq: bool) {
        self.logger.warn(
            "message.dead_letter",
            &[
                ("addr", addr.into()),
                ("dead_lettered", to_dlq.into()),
                ("dropped", (!to_dlq).into()),
                ("delivery", (msg.delivery_count as u64).into()),
            ],
        );
    }
}

// ---- errors --------------------------------------------------------------

/// Errors raised by messaging operations.
#[derive(Debug, FerrolyError)]
#[non_exhaustive]
pub enum MessagingError {
    /// No listener is registered for the destination.
    #[error("no listeners for destination: {0}")]
    NoListeners(String),
    /// The listener's buffer was full (non-blocking send).
    #[error("destination buffer full: {0}")]
    BufferFull(String),
    /// The destination's channel is closed.
    #[error("destination closed: {0}")]
    Closed(String),
}

// ---- traits --------------------------------------------------------------

/// Sends messages to a destination.
pub trait Producer: Send + Sync {
    /// Sends `msg` to `addr`.
    fn send(
        &self,
        addr: &str,
        msg: Message,
        opts: SendOptions,
    ) -> BoxFuture<'_, Result<(), MessagingError>>;
}

/// Consumes messages from a destination.
pub trait Receiver: Send + Sync {
    /// Registers `handler` as a listener on `addr`.
    fn add_listener(
        &self,
        addr: &str,
        handler: Handler,
        opts: ListenerOptions,
    ) -> BoxFuture<'_, Result<(), MessagingError>>;
}

/// A messaging backend: a [`Producer`] and [`Receiver`] for a set of URL schemes.
pub trait Provider: Producer + Receiver {
    /// The URL schemes this provider handles (e.g. `["chan"]`).
    fn schemes(&self) -> &[&str];
    /// Initializes the provider (connect, declare topology, …).
    fn setup(&self) -> BoxFuture<'_, Result<(), MessagingError>>;
}

/// The destination name from an address: `chan://orders` → `orders`.
fn dest_name(addr: &str) -> &str {
    addr.split_once("://").map(|(_, n)| n).unwrap_or(addr)
}

fn hash_key(key: &str) -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    h.finish() as usize
}

// ---- local provider ------------------------------------------------------

struct ListenerHandle {
    tx: mpsc::Sender<Message>,
}

#[derive(Default)]
struct DestState {
    listeners: Vec<ListenerHandle>,
    round_robin: usize,
}

struct Inner {
    id: String,
    destinations: Mutex<HashMap<String, DestState>>,
    observer: Mutex<Option<Arc<dyn Observer>>>,
}

/// An in-process [`Provider`] over tokio channels (`chan://…`). Multiple
/// listeners on one destination are **competing consumers** (each message goes
/// to one), with routing-key affinity (same key → same listener).
#[derive(Clone)]
pub struct LocalProvider {
    inner: Arc<Inner>,
}

impl LocalProvider {
    /// Creates a provider with the given component id.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(Inner {
                id: id.into(),
                destinations: Mutex::new(HashMap::new()),
                observer: Mutex::new(None),
            }),
        }
    }

    /// Installs an [`Observer`] for send/receive/ack/nack events.
    pub fn set_observer(&self, observer: Arc<dyn Observer>) {
        *self.inner.observer.lock().unwrap() = Some(observer);
    }

    fn observer(&self) -> Option<Arc<dyn Observer>> {
        self.inner.observer.lock().unwrap().clone()
    }

    /// Picks a listener's sender for `name` honoring routing-key affinity.
    fn pick(&self, name: &str, key: &Option<String>) -> Option<mpsc::Sender<Message>> {
        let mut dests = self.inner.destinations.lock().unwrap();
        let state = dests.get_mut(name)?;
        // Drop dead consumers (their receiver is gone) so a crashed/finished
        // consumer's orphaned sender is never selected — the bus self-heals.
        state.listeners.retain(|h| !h.tx.is_closed());
        if state.listeners.is_empty() {
            return None;
        }
        let idx = match key {
            Some(k) => hash_key(k) % state.listeners.len(),
            None => {
                let i = state.round_robin % state.listeners.len();
                state.round_robin = state.round_robin.wrapping_add(1);
                i
            }
        };
        Some(state.listeners[idx].tx.clone())
    }
}

impl Producer for LocalProvider {
    fn send(
        &self,
        addr: &str,
        msg: Message,
        opts: SendOptions,
    ) -> BoxFuture<'_, Result<(), MessagingError>> {
        let addr = addr.to_string();
        let me = self.clone();
        Box::pin(async move {
            let name = dest_name(&addr).to_string();
            let key = opts.routing_key.clone().or_else(|| msg.routing_key.clone());
            let tx = me
                .pick(&name, &key)
                .ok_or_else(|| MessagingError::NoListeners(name.clone()))?;
            if let Some(obs) = me.observer() {
                obs.on_send(&addr, &msg);
            }
            if opts.block_on_full {
                tx.send(msg).await.map_err(|_| MessagingError::Closed(name))
            } else {
                tx.try_send(msg).map_err(|e| match e {
                    mpsc::error::TrySendError::Full(_) => MessagingError::BufferFull(name),
                    mpsc::error::TrySendError::Closed(_) => MessagingError::Closed(name),
                })
            }
        })
    }
}

impl Receiver for LocalProvider {
    fn add_listener(
        &self,
        addr: &str,
        handler: Handler,
        opts: ListenerOptions,
    ) -> BoxFuture<'_, Result<(), MessagingError>> {
        let name = dest_name(addr).to_string();
        let me = self.clone();
        let (tx, rx) = mpsc::channel::<Message>(opts.buffer_size);
        tokio::spawn(consumer_loop(me.clone(), name.clone(), rx, handler, opts));
        Box::pin(async move {
            me.inner
                .destinations
                .lock()
                .unwrap()
                .entry(name)
                .or_default()
                .listeners
                .push(ListenerHandle { tx });
            Ok(())
        })
    }
}

impl Provider for LocalProvider {
    fn schemes(&self) -> &[&str] {
        &["chan"]
    }
    fn setup(&self) -> BoxFuture<'_, Result<(), MessagingError>> {
        Box::pin(async { Ok(()) })
    }
}

async fn consumer_loop(
    provider: LocalProvider,
    name: String,
    mut rx: mpsc::Receiver<Message>,
    handler: Handler,
    opts: ListenerOptions,
) {
    let semaphore = opts.concurrency.map(|n| Arc::new(Semaphore::new(n.max(1))));
    while let Some(msg) = rx.recv().await {
        let permit = match &semaphore {
            Some(s) => s.clone().acquire_owned().await.ok(),
            None => None,
        };
        let job = process(
            provider.clone(),
            name.clone(),
            handler.clone(),
            opts.clone(),
            msg,
        );
        match &semaphore {
            Some(_) => {
                tokio::spawn(async move {
                    job.await;
                    drop(permit);
                });
            }
            None => job.await, // serial => ordered delivery
        }
    }
}

async fn process(
    provider: LocalProvider,
    name: String,
    handler: Handler,
    opts: ListenerOptions,
    msg: Message,
) {
    let addr = format!("chan://{name}");
    if let Some(obs) = provider.observer() {
        obs.on_receive(&addr, &msg);
    }
    let redelivery = msg.clone();
    match handler(msg).await {
        Ack::Ok => {
            if let Some(obs) = provider.observer() {
                obs.on_ack(&addr, &redelivery);
            }
        }
        Ack::Reject => {
            if let Some(obs) = provider.observer() {
                obs.on_nack(&addr, &redelivery, false);
            }
        }
        Ack::Retry => {
            let mut m = redelivery;
            m.delivery_count += 1;
            let will_requeue = m.delivery_count < opts.max_delivery_attempts;
            if let Some(obs) = provider.observer() {
                // Report the *actual* fate: requeue only when attempts remain.
                obs.on_nack(&addr, &m, will_requeue);
            }
            if will_requeue {
                // Requeue to the same destination. The send blocks on a full
                // buffer instead of dropping, but runs in its own task so it can
                // never deadlock a serial consumer that must drain to make room.
                spawn_guaranteed_send(provider, addr, m, false);
            } else if let Some(dlq) = opts.dead_letter.clone() {
                // Exhausted: move to the dead-letter destination (blocking on a
                // full DLQ buffer rather than silently dropping).
                spawn_guaranteed_send(provider, dlq, m, true);
            } else if let Some(obs) = provider.observer() {
                // Exhausted with no DLQ configured: surface the drop rather than
                // losing the message silently.
                obs.on_dead_letter(&addr, &m, false);
            }
        }
    }
}

/// Sends `msg` to `addr`, blocking on a full buffer so it is not dropped under
/// backpressure, on a detached task so it never blocks the calling consumer.
/// `is_dlq` distinguishes a dead-letter move from an ordinary requeue for the
/// observer. A send failure (destination gone/closed) is reported as a drop.
fn spawn_guaranteed_send(provider: LocalProvider, addr: String, msg: Message, is_dlq: bool) {
    tokio::spawn(async move {
        let opts = SendOptions {
            block_on_full: true,
            ..Default::default()
        };
        match provider.send(&addr, msg.clone(), opts).await {
            Ok(()) => {
                if is_dlq {
                    if let Some(obs) = provider.observer() {
                        obs.on_dead_letter(&addr, &msg, true);
                    }
                }
            }
            Err(_) => {
                if let Some(obs) = provider.observer() {
                    obs.on_dead_letter(&addr, &msg, false);
                }
            }
        }
    });
}

// ---- lifecycle integration ----------------------------------------------

impl ferroly::lifecycle::Component for LocalProvider {
    fn id(&self) -> &str {
        &self.inner.id
    }

    fn start(
        &self,
    ) -> ferroly::lifecycle::BoxFuture<'_, Result<(), ferroly::lifecycle::LifecycleError>> {
        Box::pin(async { Ok(()) })
    }

    fn stop(
        &self,
    ) -> ferroly::lifecycle::BoxFuture<'_, Result<(), ferroly::lifecycle::LifecycleError>> {
        Box::pin(async move {
            // Dropping the listener senders closes each channel; consumer tasks
            // then drain and exit.
            self.inner.destinations.lock().unwrap().clear();
            Ok(())
        })
    }
}

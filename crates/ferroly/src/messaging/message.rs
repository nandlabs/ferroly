//! The [`Message`] type, its metadata conventions, and the handler adapters.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ferroly::codec::{json, CodecError, Decode, Encode, Value};
use ferroly::messaging::BoxFuture;

/// The conventional header keys. They are plain entries in
/// [`Message::headers`]; the typed accessors on [`Message`] read and write
/// them, and an adapter for an external broker should map them to that
/// broker's native attributes.
pub mod headers {
    /// A unique id for the message (string). Used for de-duplication and as the
    /// default correlation id of a request.
    pub const MESSAGE_ID: &str = "message-id";
    /// The id that ties a reply (or any follow-up) to the message that caused
    /// it (string).
    pub const CORRELATION_ID: &str = "correlation-id";
    /// When the message was created: milliseconds since the Unix epoch
    /// (unsigned integer).
    pub const TIMESTAMP: &str = "timestamp";
    /// The address a reply should be sent to (string).
    pub const REPLY_TO: &str = "reply-to";
    /// The W3C Trace Context `traceparent` value (string), e.g.
    /// `00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01`. Copy it from
    /// the active span when sending and start the consumer span from it.
    pub const TRACEPARENT: &str = "traceparent";
    /// The W3C Trace Context `tracestate` value (string), carried alongside
    /// [`TRACEPARENT`].
    pub const TRACESTATE: &str = "tracestate";
    /// The concrete topic a message was published to (string). Set by the
    /// provider on every copy fanned out to a `topic://` subscription, so a
    /// wildcard subscriber can tell which topic matched.
    pub const TOPIC: &str = "topic";
}

/// A message: headers, a byte body, an optional routing key, and a delivery
/// counter (incremented on each redelivery).
#[derive(Debug, Clone, Default)]
pub struct Message {
    /// Metadata headers. See [`headers`] for the conventional keys.
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

    /// Sets a header, replacing any existing header with the same key.
    pub fn with_header(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.set_header(key, value);
        self
    }

    /// Sets a header in place, replacing any existing header with the same key.
    pub fn set_header(&mut self, key: impl Into<String>, value: impl Into<Value>) {
        let (key, value) = (key.into(), value.into());
        match &mut self.headers {
            Value::Object(entries) => {
                entries.retain(|(k, _)| *k != key);
                entries.push((key, value));
            }
            _ => self.headers = Value::Object(vec![(key, value)]),
        }
    }

    /// The value of a header, if present.
    pub fn header(&self, key: &str) -> Option<&Value> {
        self.headers.get(key)
    }

    /// The value of a string header, if present and a string.
    pub fn header_str(&self, key: &str) -> Option<&str> {
        self.header(key).and_then(Value::as_str)
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

    // ---- metadata conventions --------------------------------------------

    /// Sets the message id ([`headers::MESSAGE_ID`]).
    pub fn with_id(self, id: impl Into<String>) -> Self {
        self.with_header(headers::MESSAGE_ID, id.into())
    }

    /// Assigns a freshly generated id ([`new_message_id`]).
    pub fn with_new_id(self) -> Self {
        self.with_id(new_message_id())
    }

    /// The message id, if one was set.
    pub fn id(&self) -> Option<&str> {
        self.header_str(headers::MESSAGE_ID)
    }

    /// Sets the correlation id ([`headers::CORRELATION_ID`]).
    pub fn with_correlation_id(self, id: impl Into<String>) -> Self {
        self.with_header(headers::CORRELATION_ID, id.into())
    }

    /// The correlation id, if one was set.
    pub fn correlation_id(&self) -> Option<&str> {
        self.header_str(headers::CORRELATION_ID)
    }

    /// Sets the creation time ([`headers::TIMESTAMP`], milliseconds since the
    /// Unix epoch).
    pub fn with_timestamp(self, at: SystemTime) -> Self {
        let millis = at
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        self.with_header(headers::TIMESTAMP, millis)
    }

    /// The creation time, if one was set.
    pub fn timestamp(&self) -> Option<SystemTime> {
        let millis = self.header(headers::TIMESTAMP)?.as_u64()?;
        Some(UNIX_EPOCH + Duration::from_millis(millis))
    }

    /// Fills in the id and the timestamp (now) where they are missing.
    pub fn stamped(mut self) -> Self {
        if self.id().is_none() {
            self = self.with_new_id();
        }
        if self.header(headers::TIMESTAMP).is_none() {
            self = self.with_timestamp(SystemTime::now());
        }
        self
    }

    /// Sets the reply address ([`headers::REPLY_TO`]).
    pub fn with_reply_to(self, addr: impl Into<String>) -> Self {
        self.with_header(headers::REPLY_TO, addr.into())
    }

    /// The reply address, if one was set.
    pub fn reply_to(&self) -> Option<&str> {
        self.header_str(headers::REPLY_TO)
    }

    /// Sets the W3C `traceparent` ([`headers::TRACEPARENT`]).
    pub fn with_traceparent(self, traceparent: impl Into<String>) -> Self {
        self.with_header(headers::TRACEPARENT, traceparent.into())
    }

    /// The W3C `traceparent`, if one was set.
    pub fn traceparent(&self) -> Option<&str> {
        self.header_str(headers::TRACEPARENT)
    }

    /// Sets the W3C `tracestate` ([`headers::TRACESTATE`]).
    pub fn with_tracestate(self, tracestate: impl Into<String>) -> Self {
        self.with_header(headers::TRACESTATE, tracestate.into())
    }

    /// The W3C `tracestate`, if one was set.
    pub fn tracestate(&self) -> Option<&str> {
        self.header_str(headers::TRACESTATE)
    }

    /// The concrete topic this copy was published to ([`headers::TOPIC`]); set
    /// by the provider for `topic://` deliveries.
    pub fn topic(&self) -> Option<&str> {
        self.header_str(headers::TOPIC)
    }

    /// Marks this message as a reply to `request`: the correlation id becomes
    /// the request's correlation id (or, failing that, its message id), and
    /// the request's trace context is carried over unless this message already
    /// has one.
    pub fn in_reply_to(mut self, request: &Message) -> Self {
        if let Some(id) = request.correlation_id().or_else(|| request.id()) {
            self = self.with_correlation_id(id);
        }
        self.with_trace_from(request)
    }

    /// Copies `traceparent`/`tracestate` from `parent` unless this message
    /// already carries a `traceparent` — the propagation step for a message
    /// produced while handling another.
    pub fn with_trace_from(mut self, parent: &Message) -> Self {
        if self.traceparent().is_none() {
            if let Some(traceparent) = parent.traceparent() {
                self = self.with_traceparent(traceparent);
                if let Some(tracestate) = parent.tracestate() {
                    self = self.with_tracestate(tracestate);
                }
            }
        }
        self
    }
}

/// Generates a message id: 32 lowercase hex characters, unique within the
/// process (a per-process random prefix plus a counter) and unlikely to collide
/// across processes. It is an identifier, not a secret — do not use it as a
/// token.
pub fn new_message_id() -> String {
    static SEED: OnceLock<u64> = OnceLock::new();
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let seed = *SEED.get_or_init(|| {
        use std::hash::{BuildHasher, Hasher};
        // `RandomState` is seeded from the OS, which gives a per-process value
        // without a random-number dependency.
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u32(std::process::id());
        if let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) {
            hasher.write_u128(now.as_nanos());
        }
        hasher.finish()
    });
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{seed:016x}{n:016x}")
}

/// The disposition a listener returns for a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ack {
    /// Processed successfully; remove the message.
    Ok,
    /// Failed transiently; redeliver after a backoff (up to
    /// `max_delivery_attempts`, then dead-letter). A handler that panics is
    /// treated as having returned `Retry`.
    Retry,
    /// Failed permanently; discard (no redelivery, no dead-lettering).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_header_replaces_an_existing_key() {
        let msg = Message::text("x")
            .with_header("k", "one")
            .with_header("other", 1u64)
            .with_header("k", "two");
        assert_eq!(msg.header_str("k"), Some("two"));
        assert_eq!(msg.headers.as_object().unwrap().len(), 2);
    }

    #[test]
    fn metadata_helpers_round_trip() {
        let at = UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
        let msg = Message::text("x")
            .with_id("m-1")
            .with_correlation_id("c-1")
            .with_timestamp(at)
            .with_reply_to("reply://inbox")
            .with_traceparent("00-aa-bb-01")
            .with_tracestate("vendor=1");
        assert_eq!(msg.id(), Some("m-1"));
        assert_eq!(msg.correlation_id(), Some("c-1"));
        assert_eq!(msg.timestamp(), Some(at));
        assert_eq!(msg.reply_to(), Some("reply://inbox"));
        assert_eq!(msg.traceparent(), Some("00-aa-bb-01"));
        assert_eq!(msg.tracestate(), Some("vendor=1"));
        assert_eq!(msg.topic(), None);
        assert_eq!(Message::text("bare").id(), None);
    }

    #[test]
    fn stamped_fills_only_what_is_missing() {
        let msg = Message::text("x").with_id("keep").stamped();
        assert_eq!(msg.id(), Some("keep"));
        assert!(msg.timestamp().is_some());

        let fresh = Message::text("y").stamped();
        assert_eq!(fresh.id().unwrap().len(), 32);
    }

    #[test]
    fn generated_ids_are_unique() {
        let ids: std::collections::HashSet<String> = (0..1000).map(|_| new_message_id()).collect();
        assert_eq!(ids.len(), 1000);
    }

    #[test]
    fn a_reply_inherits_correlation_and_trace_context() {
        let request = Message::text("ping")
            .with_id("req-1")
            .with_traceparent("00-aa-bb-01")
            .with_tracestate("vendor=1");
        let reply = Message::text("pong").in_reply_to(&request);
        // No explicit correlation id on the request: its message id is used.
        assert_eq!(reply.correlation_id(), Some("req-1"));
        assert_eq!(reply.traceparent(), Some("00-aa-bb-01"));
        assert_eq!(reply.tracestate(), Some("vendor=1"));

        let correlated = request.clone().with_correlation_id("saga-9");
        let reply = Message::text("pong")
            .with_traceparent("00-cc-dd-01")
            .in_reply_to(&correlated);
        assert_eq!(reply.correlation_id(), Some("saga-9"));
        // An existing trace context is not overwritten.
        assert_eq!(reply.traceparent(), Some("00-cc-dd-01"));
        assert_eq!(reply.tracestate(), None);
    }
}

//! The [`Message`] type, its properties and attributes, and the handler
//! adapters.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ferroly::codec::{json, CodecError, Decode, Encode};
use ferroly::messaging::{AttrValue, Attributes, BoxFuture};

/// The conventional **wire names** of the message properties. An adapter for
/// a broker with a single flat attribute map uses these names; see
/// [`Message::to_wire_attributes`].
pub mod headers {
    /// A unique id for the message (string). Used for de-duplication and as the
    /// default correlation id of a request.
    pub const MESSAGE_ID: &str = "message-id";
    /// The id that ties a reply (or any follow-up) to the message that caused
    /// it (string).
    pub const CORRELATION_ID: &str = "correlation-id";
    /// When the message was created: milliseconds since the Unix epoch (int).
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
    /// The media type of the body, e.g. `application/json` (string).
    pub const CONTENT_TYPE: &str = "content-type";
    /// The content coding of the body, e.g. `gzip` (string).
    pub const CONTENT_ENCODING: &str = "content-encoding";
    /// A short subject or label (string).
    pub const SUBJECT: &str = "subject";
    /// The kind of event the message describes, e.g. `order.created` (string).
    pub const EVENT_TYPE: &str = "event-type";
    /// What produced the message, e.g. a service name or URI (string).
    pub const SOURCE: &str = "source";
    /// The identity of the sender (string).
    pub const USER_ID: &str = "user-id";
    /// The de-duplication key (string).
    pub const DEDUP_KEY: &str = "dedup-key";
    /// The message priority, 0 to 255 (int).
    pub const PRIORITY: &str = "priority";

    /// Set on a dead-lettered message: why it was dead-lettered (string).
    pub const DEAD_LETTER_REASON: &str = "dead-letter-reason";
    /// Set on a dead-lettered message: free-text detail, when there is any (string).
    pub const DEAD_LETTER_DESCRIPTION: &str = "dead-letter-description";
    /// Set on a dead-lettered message: the address it was consumed from (string).
    pub const DEAD_LETTER_SOURCE: &str = "dead-letter-source";
    /// Set on a dead-lettered message: how many delivery attempts it had (int).
    pub const DEAD_LETTER_ATTEMPTS: &str = "dead-letter-attempts";

    /// The wire names of every message property, in the order
    /// [`Message::to_wire_attributes`](crate::messaging::Message::to_wire_attributes)
    /// writes them.
    pub const PROPERTY_NAMES: &[&str] = &[
        MESSAGE_ID,
        CORRELATION_ID,
        TIMESTAMP,
        REPLY_TO,
        TRACEPARENT,
        TRACESTATE,
        TOPIC,
        CONTENT_TYPE,
        CONTENT_ENCODING,
        SUBJECT,
        EVENT_TYPE,
        SOURCE,
        USER_ID,
        DEDUP_KEY,
        PRIORITY,
    ];
}

/// The system properties of a message: well-known metadata with a fixed
/// meaning, kept apart from the application's own [`Attributes`].
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Properties {
    /// A unique id for the message. Used for de-duplication (when there is no
    /// [`dedup_key`](Self::dedup_key)) and as the default correlation id of a
    /// request.
    pub message_id: Option<String>,
    /// The id that ties a reply (or any follow-up) to the message that caused
    /// it.
    pub correlation_id: Option<String>,
    /// When the message was created. Stored with millisecond precision.
    pub timestamp: Option<SystemTime>,
    /// The address a reply should be sent to.
    pub reply_to: Option<String>,
    /// The W3C Trace Context `traceparent` value.
    pub traceparent: Option<String>,
    /// The W3C Trace Context `tracestate` value.
    pub tracestate: Option<String>,
    /// The concrete topic the message was published to; set by the provider.
    pub topic: Option<String>,
    /// The media type of the body, e.g. `application/json`.
    pub content_type: Option<String>,
    /// The content coding of the body, e.g. `gzip`.
    pub content_encoding: Option<String>,
    /// A short subject or label.
    pub subject: Option<String>,
    /// The kind of event this message describes, e.g. `order.created`.
    pub event_type: Option<String>,
    /// What produced the message, e.g. a service name or URI.
    pub source: Option<String>,
    /// The identity of the sender.
    pub user_id: Option<String>,
    /// De-duplication key; when unset the message id is used.
    pub dedup_key: Option<String>,
    /// Priority, 0 (lowest, the default) to 255.
    pub priority: Option<u8>,
}

/// A message: system properties, application attributes, a byte body, an
/// optional routing key, and a delivery counter.
///
/// The struct is `#[non_exhaustive]`: build one with [`Message::new`],
/// [`Message::text`], [`Message::json`] or [`Message::default`] and the
/// `with_*` methods.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct Message {
    /// System properties (id, correlation id, content type, …).
    pub properties: Properties,
    /// Application attributes: typed key/value pairs.
    pub attributes: Attributes,
    /// The payload bytes.
    pub body: Vec<u8>,
    /// A routing/partition key — same key always lands on the same listener.
    pub routing_key: Option<String>,
    /// How many times this message has been (re)delivered (starts at 0).
    pub delivery_count: u32,
}

/// Milliseconds since the Unix epoch, saturating at 0 for earlier times.
fn epoch_millis(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
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

    /// A message whose body is the JSON encoding of `value`, with the content
    /// type set to `application/json`.
    pub fn json<T: Encode>(value: &T) -> Self {
        Self::new(json::encode(value).into_bytes()).with_content_type("application/json")
    }

    // ---- attributes -------------------------------------------------------

    /// Sets an application attribute, replacing any existing attribute with
    /// the same name ([`Attributes::insert`]).
    pub fn with_attribute(mut self, name: impl Into<String>, value: impl Into<AttrValue>) -> Self {
        self.attributes.insert(name, value);
        self
    }

    /// Sets an application attribute in place, replacing any existing
    /// attribute with the same name ([`Attributes::insert`]).
    pub fn set_attribute(&mut self, name: impl Into<String>, value: impl Into<AttrValue>) {
        self.attributes.insert(name, value);
    }

    /// The first application attribute with that name, if present.
    pub fn attribute(&self, name: &str) -> Option<&AttrValue> {
        self.attributes.get(name)
    }

    /// The first application attribute with that name, if present and a
    /// string.
    pub fn attribute_str(&self, name: &str) -> Option<&str> {
        self.attributes.get_str(name)
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

    /// A message whose body is `value` encoded for `content_type` (any type the
    /// codec registry knows: JSON, XML, YAML, TOML), with the content type set.
    ///
    /// ```
    /// use ferroly::messaging::Message;
    ///
    /// #[derive(ferroly::codec::Encode, ferroly::codec::Decode, PartialEq, Debug)]
    /// struct Order { id: u32 }
    ///
    /// let msg = Message::encode_as("application/yaml", &Order { id: 7 }).unwrap();
    /// assert_eq!(msg.content_type(), Some("application/yaml"));
    /// assert_eq!(msg.decode::<Order>().unwrap(), Order { id: 7 });
    /// ```
    pub fn encode_as<T: Encode + ?Sized>(
        content_type: &str,
        value: &T,
    ) -> Result<Self, CodecError> {
        let body = ferroly::codec::encode(content_type, value)?;
        Ok(Self::new(body).with_content_type(content_type))
    }

    /// Decodes the body into `T` using the message's content type when it is one
    /// the codec registry knows, and as JSON otherwise.
    pub fn decode<T: Decode>(&self) -> Result<T, CodecError> {
        match self.content_type() {
            Some(content_type) if ferroly::codec::resolve(content_type).is_ok() => {
                ferroly::codec::decode(content_type, &self.body)
            }
            _ => json::decode_from_slice(&self.body),
        }
    }

    // ---- properties -------------------------------------------------------

    /// Sets the message id ([`Properties::message_id`]).
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.properties.message_id = Some(id.into());
        self
    }

    /// Assigns a freshly generated id ([`new_message_id`]).
    pub fn with_new_id(self) -> Self {
        self.with_id(new_message_id())
    }

    /// The message id, if one was set.
    pub fn id(&self) -> Option<&str> {
        self.properties.message_id.as_deref()
    }

    /// Sets the correlation id ([`Properties::correlation_id`]).
    pub fn with_correlation_id(mut self, id: impl Into<String>) -> Self {
        self.properties.correlation_id = Some(id.into());
        self
    }

    /// The correlation id, if one was set.
    pub fn correlation_id(&self) -> Option<&str> {
        self.properties.correlation_id.as_deref()
    }

    /// Sets the creation time ([`Properties::timestamp`]), truncated to whole
    /// milliseconds since the Unix epoch.
    pub fn with_timestamp(mut self, at: SystemTime) -> Self {
        let ms = epoch_millis(at);
        self.properties.timestamp = Some(UNIX_EPOCH + Duration::from_millis(ms));
        self
    }

    /// The creation time, if one was set.
    pub fn timestamp(&self) -> Option<SystemTime> {
        self.properties.timestamp
    }

    /// Fills in the id and the timestamp (now) where they are missing.
    pub fn stamped(mut self) -> Self {
        if self.id().is_none() {
            self = self.with_new_id();
        }
        if self.properties.timestamp.is_none() {
            self = self.with_timestamp(SystemTime::now());
        }
        self
    }

    /// Sets the reply address ([`Properties::reply_to`]).
    pub fn with_reply_to(mut self, addr: impl Into<String>) -> Self {
        self.properties.reply_to = Some(addr.into());
        self
    }

    /// The reply address, if one was set.
    pub fn reply_to(&self) -> Option<&str> {
        self.properties.reply_to.as_deref()
    }

    /// Sets the W3C `traceparent` ([`Properties::traceparent`]).
    pub fn with_traceparent(mut self, traceparent: impl Into<String>) -> Self {
        self.properties.traceparent = Some(traceparent.into());
        self
    }

    /// The W3C `traceparent`, if one was set.
    pub fn traceparent(&self) -> Option<&str> {
        self.properties.traceparent.as_deref()
    }

    /// Sets the W3C `tracestate` ([`Properties::tracestate`]).
    pub fn with_tracestate(mut self, tracestate: impl Into<String>) -> Self {
        self.properties.tracestate = Some(tracestate.into());
        self
    }

    /// The W3C `tracestate`, if one was set.
    pub fn tracestate(&self) -> Option<&str> {
        self.properties.tracestate.as_deref()
    }

    /// The concrete topic this copy was published to ([`Properties::topic`]);
    /// set by the provider for `topic://` deliveries.
    pub fn topic(&self) -> Option<&str> {
        self.properties.topic.as_deref()
    }

    /// Sets the media type of the body ([`Properties::content_type`]).
    pub fn with_content_type(mut self, content_type: impl Into<String>) -> Self {
        self.properties.content_type = Some(content_type.into());
        self
    }

    /// The media type of the body, if one was set.
    pub fn content_type(&self) -> Option<&str> {
        self.properties.content_type.as_deref()
    }

    /// Sets the content coding of the body ([`Properties::content_encoding`]).
    pub fn with_content_encoding(mut self, content_encoding: impl Into<String>) -> Self {
        self.properties.content_encoding = Some(content_encoding.into());
        self
    }

    /// The content coding of the body, if one was set.
    pub fn content_encoding(&self) -> Option<&str> {
        self.properties.content_encoding.as_deref()
    }

    /// Sets the subject ([`Properties::subject`]).
    pub fn with_subject(mut self, subject: impl Into<String>) -> Self {
        self.properties.subject = Some(subject.into());
        self
    }

    /// The subject, if one was set.
    pub fn subject(&self) -> Option<&str> {
        self.properties.subject.as_deref()
    }

    /// Sets the event type ([`Properties::event_type`]).
    pub fn with_event_type(mut self, event_type: impl Into<String>) -> Self {
        self.properties.event_type = Some(event_type.into());
        self
    }

    /// The event type, if one was set.
    pub fn event_type(&self) -> Option<&str> {
        self.properties.event_type.as_deref()
    }

    /// Sets the source ([`Properties::source`]).
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.properties.source = Some(source.into());
        self
    }

    /// The source, if one was set.
    pub fn source(&self) -> Option<&str> {
        self.properties.source.as_deref()
    }

    /// Sets the sender identity ([`Properties::user_id`]).
    pub fn with_user_id(mut self, user_id: impl Into<String>) -> Self {
        self.properties.user_id = Some(user_id.into());
        self
    }

    /// The sender identity, if one was set.
    pub fn user_id(&self) -> Option<&str> {
        self.properties.user_id.as_deref()
    }

    /// Sets the de-duplication key ([`Properties::dedup_key`]).
    pub fn with_dedup_key(mut self, dedup_key: impl Into<String>) -> Self {
        self.properties.dedup_key = Some(dedup_key.into());
        self
    }

    /// The de-duplication key, if one was set.
    pub fn dedup_key(&self) -> Option<&str> {
        self.properties.dedup_key.as_deref()
    }

    /// Sets the priority ([`Properties::priority`]), 0 (lowest) to 255.
    pub fn with_priority(mut self, priority: u8) -> Self {
        self.properties.priority = Some(priority);
        self
    }

    /// The priority; 0 when none was set.
    pub fn priority(&self) -> u8 {
        self.properties.priority.unwrap_or(0)
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

    // ---- wire mapping -----------------------------------------------------

    /// Flattens the message metadata into one attribute map: every set property
    /// under its conventional name from [`headers`] (in [`headers::PROPERTY_NAMES`]
    /// order), then the application attributes. An application attribute whose
    /// name is a property name is left out — the property wins.
    ///
    /// ```
    /// use ferroly::messaging::{headers, Message};
    ///
    /// let msg = Message::text("hi")
    ///     .with_id("m-1")
    ///     .with_priority(3)
    ///     .with_attribute("tenant", "acme");
    /// let wire = msg.to_wire_attributes();
    /// let names: Vec<&str> = wire.keys().collect();
    /// assert_eq!(names, [headers::MESSAGE_ID, headers::PRIORITY, "tenant"]);
    ///
    /// let back = Message::from_wire(msg.body.clone(), wire);
    /// assert_eq!(back.id(), Some("m-1"));
    /// assert_eq!(back.priority(), 3);
    /// assert_eq!(back.attribute_str("tenant"), Some("acme"));
    /// assert_eq!(back.lookup("message-id").unwrap().as_str(), Some("m-1"));
    /// ```
    pub fn to_wire_attributes(&self) -> Attributes {
        let mut wire = Attributes::new();
        for name in headers::PROPERTY_NAMES {
            if let Some(value) = self.property(name) {
                wire.append(*name, value);
            }
        }
        for (name, value) in self.attributes.iter() {
            if !headers::PROPERTY_NAMES.contains(&name) {
                wire.append(name, value.clone());
            }
        }
        wire
    }

    /// Builds a message from a body and a flat attribute map, the reverse of
    /// [`to_wire_attributes`](Self::to_wire_attributes): an entry whose name is a
    /// property name and whose value has that property's type becomes the
    /// property; every other entry is kept as an application attribute.
    ///
    /// When a property name appears more than once, the first entry of the
    /// right type becomes the property and the others are kept as attributes.
    pub fn from_wire(body: impl Into<Vec<u8>>, attributes: Attributes) -> Self {
        let mut msg = Message::new(body);
        for (name, value) in attributes {
            let taken = msg.property(&name).is_none() && msg.set_property(&name, &value);
            if !taken {
                msg.attributes.append(name, value);
            }
        }
        msg
    }

    /// Looks a name up the way a filter does: a set property under its
    /// conventional name first, then the first application attribute.
    pub fn lookup(&self, name: &str) -> Option<AttrValue> {
        self.property(name)
            .or_else(|| self.attributes.get(name).cloned())
    }

    /// The property with that wire name, if it is a property name and set.
    fn property(&self, name: &str) -> Option<AttrValue> {
        let p = &self.properties;
        let text = |v: &Option<String>| v.as_ref().map(|s| AttrValue::Str(s.clone()));
        match name {
            headers::MESSAGE_ID => text(&p.message_id),
            headers::CORRELATION_ID => text(&p.correlation_id),
            headers::TIMESTAMP => p
                .timestamp
                .map(|at| AttrValue::Int(epoch_millis(at) as i64)),
            headers::REPLY_TO => text(&p.reply_to),
            headers::TRACEPARENT => text(&p.traceparent),
            headers::TRACESTATE => text(&p.tracestate),
            headers::TOPIC => text(&p.topic),
            headers::CONTENT_TYPE => text(&p.content_type),
            headers::CONTENT_ENCODING => text(&p.content_encoding),
            headers::SUBJECT => text(&p.subject),
            headers::EVENT_TYPE => text(&p.event_type),
            headers::SOURCE => text(&p.source),
            headers::USER_ID => text(&p.user_id),
            headers::DEDUP_KEY => text(&p.dedup_key),
            headers::PRIORITY => p.priority.map(|n| AttrValue::Int(i64::from(n))),
            _ => None,
        }
    }

    /// Sets the property with that wire name from `value`. Returns `false`
    /// (changing nothing) when the name is not a property name or the value
    /// does not have the property's type.
    fn set_property(&mut self, name: &str, value: &AttrValue) -> bool {
        let p = &mut self.properties;
        let slot = match name {
            headers::TIMESTAMP => {
                return match value.as_i64() {
                    Some(ms) if ms >= 0 => {
                        p.timestamp = Some(UNIX_EPOCH + Duration::from_millis(ms as u64));
                        true
                    }
                    _ => false,
                };
            }
            headers::PRIORITY => {
                return match value.as_i64().and_then(|n| u8::try_from(n).ok()) {
                    Some(n) => {
                        p.priority = Some(n);
                        true
                    }
                    None => false,
                };
            }
            headers::MESSAGE_ID => &mut p.message_id,
            headers::CORRELATION_ID => &mut p.correlation_id,
            headers::REPLY_TO => &mut p.reply_to,
            headers::TRACEPARENT => &mut p.traceparent,
            headers::TRACESTATE => &mut p.tracestate,
            headers::TOPIC => &mut p.topic,
            headers::CONTENT_TYPE => &mut p.content_type,
            headers::CONTENT_ENCODING => &mut p.content_encoding,
            headers::SUBJECT => &mut p.subject,
            headers::EVENT_TYPE => &mut p.event_type,
            headers::SOURCE => &mut p.source,
            headers::USER_ID => &mut p.user_id,
            headers::DEDUP_KEY => &mut p.dedup_key,
            _ => return false,
        };
        match value.as_str() {
            Some(s) => {
                *slot = Some(s.to_string());
                true
            }
            None => false,
        }
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
#[non_exhaustive]
pub enum Ack {
    /// Processed successfully; remove the message.
    Ok,
    /// Failed transiently; redeliver after the listener's backoff (up to
    /// `max_delivery_attempts`, then dead-letter). A handler that panics is
    /// treated as having returned `Retry`.
    Retry,
    /// Failed permanently; discard (no redelivery, no dead-lettering).
    Reject,
    /// Like [`Retry`](Ack::Retry), but redeliver after this delay instead of
    /// the listener's backoff. Counts as a delivery attempt.
    RetryAfter(Duration),
    /// Failed permanently; move to the dead-letter destination now, without
    /// using the remaining attempts. Without a dead-letter destination the
    /// message is dropped and reported through the observer.
    DeadLetter,
    /// Not decided yet: the handler kept an [`Acker`] and will settle the
    /// message later. Only meaningful from a delivery handler (see
    /// [`Delivery::acker`]); from a plain handler it is treated as `Retry`.
    ///
    /// [`Acker`]: crate::messaging::Acker
    /// [`Delivery::acker`]: crate::messaging::Delivery::acker
    Pending,
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
    fn with_attribute_replaces_an_existing_key() {
        let msg = Message::text("x")
            .with_attribute("k", "one")
            .with_attribute("other", 1)
            .with_attribute("k", "two");
        assert_eq!(msg.attribute_str("k"), Some("two"));
        assert_eq!(msg.attributes.len(), 2);
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

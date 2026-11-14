//! What a provider supports ([`Capabilities`]) and what it returns for an
//! accepted message ([`Receipt`]).

use ferroly::messaging::{AttrValue, Attributes};

#[cfg(doc)]
use ferroly::messaging::SendOptions;

/// What a provider supports. A field left `false` / `None` means "no" or
/// "not stated".
///
/// The struct is `#[non_exhaustive]`; build one with [`Capabilities::new`]
/// and the chainable setters:
///
/// ```
/// use ferroly::messaging::Capabilities;
///
/// let caps = Capabilities::new()
///     .native_batch(true)
///     .max_message_bytes(256 * 1024)
///     .max_batch_len(10);
/// assert!(!caps.delay);
/// assert_eq!(caps.max_batch_len, Some(10));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Capabilities {
    /// Delayed delivery ([`SendOptions::delay`]).
    pub delay: bool,
    /// Delivery at an absolute time ([`SendOptions::deliver_at`]).
    pub deliver_at: bool,
    /// Time to live ([`SendOptions::ttl`]).
    pub ttl: bool,
    /// Message priority.
    pub priority: bool,
    /// Ordering by routing key.
    pub ordering: bool,
    /// Attribute filters on subscriptions.
    pub filters: bool,
    /// De-duplication.
    pub dedup: bool,
    /// The delivery context: delivery details, lease extension and deferred
    /// acknowledgements (`add_delivery_listener`).
    pub delivery_context: bool,
    /// A native batch send that reports per-entry results.
    pub native_batch: bool,
    /// The largest accepted message, in bytes.
    pub max_message_bytes: Option<usize>,
    /// The largest accepted batch, in messages.
    pub max_batch_len: Option<usize>,
    /// The largest number of attributes on one message.
    pub max_attributes: Option<usize>,
}

impl Capabilities {
    /// Claims nothing: every flag `false`, every limit `None`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets [`delay`](Self::delay).
    pub fn delay(mut self, yes: bool) -> Self {
        self.delay = yes;
        self
    }

    /// Sets [`deliver_at`](Self::deliver_at).
    pub fn deliver_at(mut self, yes: bool) -> Self {
        self.deliver_at = yes;
        self
    }

    /// Sets [`ttl`](Self::ttl).
    pub fn ttl(mut self, yes: bool) -> Self {
        self.ttl = yes;
        self
    }

    /// Sets [`priority`](Self::priority).
    pub fn priority(mut self, yes: bool) -> Self {
        self.priority = yes;
        self
    }

    /// Sets [`ordering`](Self::ordering).
    pub fn ordering(mut self, yes: bool) -> Self {
        self.ordering = yes;
        self
    }

    /// Sets [`filters`](Self::filters).
    pub fn filters(mut self, yes: bool) -> Self {
        self.filters = yes;
        self
    }

    /// Sets [`dedup`](Self::dedup).
    pub fn dedup(mut self, yes: bool) -> Self {
        self.dedup = yes;
        self
    }

    /// Sets [`delivery_context`](Self::delivery_context).
    pub fn delivery_context(mut self, yes: bool) -> Self {
        self.delivery_context = yes;
        self
    }

    /// Sets [`native_batch`](Self::native_batch).
    pub fn native_batch(mut self, yes: bool) -> Self {
        self.native_batch = yes;
        self
    }

    /// Sets [`max_message_bytes`](Self::max_message_bytes).
    pub fn max_message_bytes(mut self, bytes: usize) -> Self {
        self.max_message_bytes = Some(bytes);
        self
    }

    /// Sets [`max_batch_len`](Self::max_batch_len).
    pub fn max_batch_len(mut self, messages: usize) -> Self {
        self.max_batch_len = Some(messages);
        self
    }

    /// Sets [`max_attributes`](Self::max_attributes).
    pub fn max_attributes(mut self, attributes: usize) -> Self {
        self.max_attributes = Some(attributes);
        self
    }
}

/// What a provider returns for an accepted message.
///
/// ```
/// use ferroly::messaging::Receipt;
///
/// let receipt = Receipt::new()
///     .with_message_id("b1946ac9")
///     .with_sequence("4711")
///     .with_extra("shard", "3");
/// assert_eq!(receipt.message_id.as_deref(), Some("b1946ac9"));
/// assert_eq!(receipt.extra.get_str("shard"), Some("3"));
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Receipt {
    /// The id the message is known by (the broker's, or the message's own).
    pub message_id: Option<String>,
    /// The broker's sequence number or offset, when it reports one.
    pub sequence: Option<String>,
    /// Anything else the broker returned.
    pub extra: Attributes,
}

impl Receipt {
    /// An empty receipt.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the [`message_id`](Self::message_id).
    pub fn with_message_id(mut self, id: impl Into<String>) -> Self {
        self.message_id = Some(id.into());
        self
    }

    /// Sets the [`sequence`](Self::sequence).
    pub fn with_sequence(mut self, sequence: impl Into<String>) -> Self {
        self.sequence = Some(sequence.into());
        self
    }

    /// Adds an entry to [`extra`](Self::extra), replacing one with the same
    /// name.
    pub fn with_extra(mut self, name: impl Into<String>, value: impl Into<AttrValue>) -> Self {
        self.extra.insert(name, value);
        self
    }
}

//! The provider traits — the extension point for messaging backends — plus
//! address parsing, topic matching and the scheme-based [`ProviderRegistry`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ferroly::messaging::{
    BatchEntryFailure, BatchFailure, BoxFuture, Capabilities, DeliveryHandler, Handler,
    ListenerOptions, Message, MessagingError, Observer, Receipt, SendOptions,
};

/// Sends messages to a destination.
pub trait Producer: Send + Sync {
    /// Sends `msg` to `addr`.
    ///
    /// An implementation that cannot accept the message should hand it back in
    /// the error ([`MessagingError::into_message`]) wherever it still has it.
    fn send(
        &self,
        addr: &str,
        msg: Message,
        opts: SendOptions,
    ) -> BoxFuture<'_, Result<(), MessagingError>>;

    /// Sends `msg` and returns the provider's [`Receipt`].
    ///
    /// The default calls [`send`](Self::send) and returns a receipt holding
    /// the message's own id. A backend whose broker assigns ids overrides it.
    fn send_with_receipt(
        &self,
        addr: &str,
        msg: Message,
        opts: SendOptions,
    ) -> BoxFuture<'_, Result<Receipt, MessagingError>> {
        let addr = addr.to_string();
        Box::pin(async move {
            let message_id = msg.id().map(str::to_string);
            self.send(&addr, msg, opts).await?;
            Ok(Receipt {
                message_id,
                ..Receipt::default()
            })
        })
    }

    /// Sends `msgs` to `addr` in order, applying `opts` to each.
    ///
    /// The default implementation is **sequential**: it calls
    /// [`send`](Self::send) once per message and stops at the first failure,
    /// returning [`MessagingError::PartialBatch`] with the number already
    /// sent, one [`BatchEntryFailure`] for the message that failed (carrying
    /// it, when the error handed it back), and the messages never tried in
    /// [`BatchFailure::unsent`]. It is **not** atomic.
    ///
    /// A backend with a native batch call overrides it and reports **every**
    /// failed entry in [`BatchFailure::failures`], with `unsent` empty.
    fn send_batch(
        &self,
        addr: &str,
        msgs: Vec<Message>,
        opts: SendOptions,
    ) -> BoxFuture<'_, Result<(), MessagingError>> {
        let addr = addr.to_string();
        Box::pin(async move {
            let mut pending = msgs.into_iter();
            let mut sent = 0;
            while let Some(msg) = pending.next() {
                if let Err(error) = self.send(&addr, msg, opts.clone()).await {
                    return Err(MessagingError::PartialBatch(BatchFailure::new(
                        sent,
                        vec![BatchEntryFailure::new(sent, error)], // index == number already sent
                        pending.collect(),
                    )));
                }
                sent += 1;
            }
            Ok(())
        })
    }
}

/// Consumes messages from a destination.
pub trait Receiver: Send + Sync {
    /// Registers `handler` as a listener on `addr`.
    ///
    /// The default returns [`MessagingError::Unsupported`], which is what a
    /// publish-only provider wants: it implements [`Producer`] and writes
    /// `impl Receiver for MyProvider {}`.
    fn add_listener(
        &self,
        addr: &str,
        handler: Handler,
        opts: ListenerOptions,
    ) -> BoxFuture<'_, Result<(), MessagingError>> {
        let _ = (handler, opts);
        let addr = addr.to_string();
        Box::pin(async move { Err(MessagingError::Unsupported(format!("add_listener({addr})"))) })
    }

    /// Registers a handler that receives the delivery context ([`Delivery`]).
    ///
    /// The default returns [`MessagingError::Unsupported`].
    ///
    /// [`Delivery`]: crate::messaging::Delivery
    fn add_delivery_listener(
        &self,
        addr: &str,
        handler: DeliveryHandler,
        opts: ListenerOptions,
    ) -> BoxFuture<'_, Result<(), MessagingError>> {
        let _ = (handler, opts);
        let addr = addr.to_string();
        Box::pin(async move {
            Err(MessagingError::Unsupported(format!(
                "add_delivery_listener({addr})"
            )))
        })
    }

    /// Removes every listener registered on `addr`, returning how many were
    /// removed. Messages already buffered for them are still handled.
    ///
    /// The default implementation returns [`MessagingError::Unsupported`].
    fn remove_listeners(&self, addr: &str) -> BoxFuture<'_, Result<usize, MessagingError>> {
        let addr = addr.to_string();
        Box::pin(async move {
            Err(MessagingError::Unsupported(format!(
                "remove_listeners({addr})"
            )))
        })
    }
}

/// A messaging backend: a [`Producer`] and [`Receiver`] for a set of URL schemes.
///
/// # Implementing a backend
///
/// This trait is the extension point for external brokers (Kafka, NATS, SQS,
/// …). An adapter crate implements [`Producer`], [`Receiver`] and `Provider`
/// for its client type; application code then talks to
/// `Arc<dyn Provider>` (optionally through a [`ProviderRegistry`]) and does
/// not change when the backend does. All methods return a [`BoxFuture`], so the
/// traits are object-safe without `async-trait`.
///
/// What an adapter is expected to do:
///
/// - **Addresses.** Claim one or more URL schemes in
///   [`schemes`](Self::schemes) (`"kafka"`, `"nats"`, `"sqs"`); use
///   [`split_address`] to separate the scheme from the destination name.
/// - **Sending.** Map [`Message::properties`] and [`Message::attributes`] (or
///   [`Message::to_wire_attributes`] for a broker with one flat attribute
///   map), [`Message::routing_key`] (partition / group key) and
///   [`SendOptions`] onto the broker's native fields.
/// - **Options.** A provider must **refuse** a send whose options it cannot
///   honour when they change what delivery means — `delay`, `deliver_at`,
///   `ttl` — by returning [`MessagingError::InvalidOptions`] (use
///   [`SendOptions::unsupported_by`]). It may ignore hints (`on_full`,
///   `block_on_full`) and `extensions` that are not its own.
/// - **Failures.** Return [`MessagingError::refused`] (or
///   [`refused_transient`](MessagingError::refused_transient)), handing the
///   message back, when one message is not accepted, and
///   [`MessagingError::backend`] /
///   [`backend_transient`](MessagingError::backend_transient) for transport
///   failures.
/// - **Publish-only providers.** Implement [`Producer`], write
///   `impl Receiver for X {}` (the defaults return
///   [`MessagingError::Unsupported`]), and implement
///   [`schemes`](Self::schemes). Report what the backend supports in
///   [`capabilities`](Self::capabilities).
/// - **Receiving.** Drive the [`Handler`] and translate its
///   [`Ack`](crate::messaging::Ack) into the broker's acknowledgement:
///   `Ok` → ack/commit, `Retry` → nack/redeliver (bounded by
///   [`ListenerOptions::max_delivery_attempts`], then
///   [`ListenerOptions::dead_letter`]), `Reject` → discard. Set
///   [`Message::delivery_count`] from the broker's redelivery counter. Contain
///   handler panics and treat them as `Retry`.
/// - **Lifecycle.** Connect and declare topology in [`setup`](Self::setup);
///   flush, drain and disconnect in [`close`](Self::close). Both default to
///   doing nothing.
/// - **Observability.** Accept an [`Observer`] in
///   [`set_observer`](Self::set_observer) and fire at least
///   [`on_published`](Observer::on_published) and
///   [`on_send_failed`](Observer::on_send_failed).
///
/// The semantics a backend actually provides — durability, ordering, delivery
/// guarantee — are the backend's; this trait does not promise more than the
/// underlying broker does.
pub trait Provider: Producer + Receiver {
    /// The URL schemes this provider handles (e.g. `["chan"]`).
    fn schemes(&self) -> &[&str];

    /// Initializes the provider (connect, declare topology, …). The default
    /// does nothing.
    fn setup(&self) -> BoxFuture<'_, Result<(), MessagingError>> {
        Box::pin(async { Ok(()) })
    }

    /// Shuts the provider down gracefully: stop accepting sends, let in-flight
    /// and buffered work finish, release connections. The default does
    /// nothing.
    fn close(&self) -> BoxFuture<'_, Result<(), MessagingError>> {
        Box::pin(async { Ok(()) })
    }

    /// What this provider supports. The default claims nothing.
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    /// Installs an [`Observer`], replacing any previous one. The default
    /// returns [`MessagingError::Unsupported`].
    fn set_observer(&self, observer: Arc<dyn Observer>) -> Result<(), MessagingError> {
        let _ = observer;
        Err(MessagingError::Unsupported("set_observer".to_string()))
    }
}

/// Splits an address into its scheme and destination name:
/// `chan://orders` → `(Some("chan"), "orders")`, `orders` → `(None, "orders")`.
pub fn split_address(addr: &str) -> (Option<&str>, &str) {
    match addr.split_once("://") {
        Some((scheme, name)) => (Some(scheme), name),
        None => (None, addr),
    }
}

/// Whether `topic` matches the subscription `pattern`.
///
/// Topics are dot-separated tokens (`orders.eu.created`). In a pattern, `*`
/// matches exactly one token and a trailing `>` matches one or more remaining
/// tokens; every other token must match literally.
///
/// ```
/// use ferroly::messaging::topic_matches;
///
/// assert!(topic_matches("orders.*", "orders.created"));
/// assert!(!topic_matches("orders.*", "orders.eu.created"));
/// assert!(topic_matches("orders.>", "orders.eu.created"));
/// assert!(!topic_matches("orders.>", "orders"));
/// assert!(topic_matches("orders.created", "orders.created"));
/// ```
pub fn topic_matches(pattern: &str, topic: &str) -> bool {
    let mut pattern = pattern.split('.');
    let mut topic = topic.split('.');
    loop {
        match (pattern.next(), topic.next()) {
            // `>` swallows the rest, but needs at least one token to swallow.
            (Some(">"), Some(_)) => return pattern.next().is_none(),
            (Some("*"), Some(_)) => {}
            (Some(p), Some(t)) if p == t => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

/// Checks a concrete topic name: non-empty tokens, no wildcards.
pub(crate) fn validate_topic(name: &str) -> Result<(), MessagingError> {
    if name.is_empty()
        || name
            .split('.')
            .any(|t| t.is_empty() || t == "*" || t == ">")
    {
        return Err(MessagingError::InvalidAddress(format!(
            "topic://{name} (a topic to publish to needs non-empty tokens and no wildcards)"
        )));
    }
    Ok(())
}

/// Checks a subscription pattern: non-empty tokens, `>` only as the last one.
pub(crate) fn validate_pattern(pattern: &str) -> Result<(), MessagingError> {
    let tokens: Vec<&str> = pattern.split('.').collect();
    let misplaced_tail = tokens[..tokens.len() - 1].contains(&">");
    if pattern.is_empty() || tokens.iter().any(|t| t.is_empty()) || misplaced_tail {
        return Err(MessagingError::InvalidAddress(format!(
            "topic://{pattern} (tokens must be non-empty and `>` may only come last)"
        )));
    }
    Ok(())
}

/// Routes sends and listeners to the [`Provider`] that owns an address's
/// scheme, so application code can mix backends behind one handle.
///
/// ```
/// use std::sync::Arc;
/// use ferroly::messaging::{
///     handler, Ack, LocalProvider, Message, Producer, ProviderRegistry, Receiver,
/// };
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), ferroly::messaging::MessagingError> {
/// let registry = ProviderRegistry::new();
/// registry.register(Arc::new(LocalProvider::new("local")))?; // chan://, topic://
/// // registry.register(Arc::new(my_kafka_adapter))?;          // kafka://
///
/// registry
///     .add_listener("chan://orders", handler(|_m| async { Ack::Ok }), Default::default())
///     .await?;
/// registry
///     .send("chan://orders", Message::text("o-1"), Default::default())
///     .await?;
/// # Ok(())
/// # }
/// ```
#[derive(Default, Clone)]
pub struct ProviderRegistry {
    providers: Arc<Mutex<HashMap<String, Arc<dyn Provider>>>>,
}

impl ProviderRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `provider` for every scheme it reports. Fails with
    /// [`MessagingError::InvalidOptions`] — registering nothing — if one of its
    /// schemes is already taken.
    pub fn register(&self, provider: Arc<dyn Provider>) -> Result<(), MessagingError> {
        let mut providers = self.providers.lock().unwrap();
        if let Some(taken) = provider
            .schemes()
            .iter()
            .find(|s| providers.contains_key(**s))
        {
            return Err(MessagingError::InvalidOptions(format!(
                "scheme `{taken}` already has a provider"
            )));
        }
        for scheme in provider.schemes() {
            providers.insert((*scheme).to_string(), provider.clone());
        }
        Ok(())
    }

    /// The provider that handles `addr`'s scheme. An address without a scheme,
    /// or with an unregistered one, is a [`MessagingError::InvalidAddress`].
    pub fn provider_for(&self, addr: &str) -> Result<Arc<dyn Provider>, MessagingError> {
        let (scheme, _) = split_address(addr);
        scheme
            .and_then(|s| self.providers.lock().unwrap().get(s).cloned())
            .ok_or_else(|| {
                MessagingError::InvalidAddress(format!("{addr} (no provider for its scheme)"))
            })
    }

    /// The registered schemes, sorted.
    pub fn schemes(&self) -> Vec<String> {
        let mut schemes: Vec<String> = self.providers.lock().unwrap().keys().cloned().collect();
        schemes.sort_unstable();
        schemes
    }

    /// The registered providers, each once (a provider registered under several
    /// schemes is listed a single time), in no particular order.
    pub fn providers(&self) -> Vec<Arc<dyn Provider>> {
        let providers = self.providers.lock().unwrap();
        let mut unique: Vec<Arc<dyn Provider>> = Vec::new();
        for provider in providers.values() {
            if !unique.iter().any(|seen| Arc::ptr_eq(seen, provider)) {
                unique.push(provider.clone());
            }
        }
        unique
    }

    /// The capabilities of the provider that handles `addr`.
    pub fn capabilities_for(&self, addr: &str) -> Result<Capabilities, MessagingError> {
        Ok(self.provider_for(addr)?.capabilities())
    }

    /// Calls [`Provider::setup`] on every provider. All are attempted; the first
    /// error (if any) is returned.
    pub async fn setup_all(&self) -> Result<(), MessagingError> {
        // Collected first, so the registry's lock is not held across an await.
        let providers = self.providers();
        let mut first = None;
        for provider in providers {
            if let Err(error) = provider.setup().await {
                first.get_or_insert(error);
            }
        }
        first.map_or(Ok(()), Err)
    }

    /// Calls [`Provider::close`] on every provider. All are attempted; the first
    /// error (if any) is returned.
    pub async fn close_all(&self) -> Result<(), MessagingError> {
        let providers = self.providers();
        let mut first = None;
        for provider in providers {
            if let Err(error) = provider.close().await {
                first.get_or_insert(error);
            }
        }
        first.map_or(Ok(()), Err)
    }

    /// Installs `observer` on every provider that accepts one. Providers that
    /// return `Unsupported` are skipped; the first other error is returned.
    pub fn set_observer(&self, observer: Arc<dyn Observer>) -> Result<(), MessagingError> {
        let mut first = None;
        for provider in self.providers() {
            match provider.set_observer(observer.clone()) {
                Ok(()) | Err(MessagingError::Unsupported(_)) => {}
                Err(error) => {
                    first.get_or_insert(error);
                }
            }
        }
        first.map_or(Ok(()), Err)
    }
}

impl Producer for ProviderRegistry {
    fn send(
        &self,
        addr: &str,
        msg: Message,
        opts: SendOptions,
    ) -> BoxFuture<'_, Result<(), MessagingError>> {
        let addr = addr.to_string();
        Box::pin(async move { self.provider_for(&addr)?.send(&addr, msg, opts).await })
    }

    fn send_with_receipt(
        &self,
        addr: &str,
        msg: Message,
        opts: SendOptions,
    ) -> BoxFuture<'_, Result<Receipt, MessagingError>> {
        let addr = addr.to_string();
        Box::pin(async move {
            self.provider_for(&addr)?
                .send_with_receipt(&addr, msg, opts)
                .await
        })
    }

    fn send_batch(
        &self,
        addr: &str,
        msgs: Vec<Message>,
        opts: SendOptions,
    ) -> BoxFuture<'_, Result<(), MessagingError>> {
        let addr = addr.to_string();
        Box::pin(async move {
            self.provider_for(&addr)?
                .send_batch(&addr, msgs, opts)
                .await
        })
    }
}

impl Receiver for ProviderRegistry {
    fn add_listener(
        &self,
        addr: &str,
        handler: Handler,
        opts: ListenerOptions,
    ) -> BoxFuture<'_, Result<(), MessagingError>> {
        let addr = addr.to_string();
        Box::pin(async move {
            self.provider_for(&addr)?
                .add_listener(&addr, handler, opts)
                .await
        })
    }

    fn add_delivery_listener(
        &self,
        addr: &str,
        handler: DeliveryHandler,
        opts: ListenerOptions,
    ) -> BoxFuture<'_, Result<(), MessagingError>> {
        let addr = addr.to_string();
        Box::pin(async move {
            self.provider_for(&addr)?
                .add_delivery_listener(&addr, handler, opts)
                .await
        })
    }

    fn remove_listeners(&self, addr: &str) -> BoxFuture<'_, Result<usize, MessagingError>> {
        let addr = addr.to_string();
        Box::pin(async move { self.provider_for(&addr)?.remove_listeners(&addr).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_addresses() {
        assert_eq!(split_address("chan://orders"), (Some("chan"), "orders"));
        assert_eq!(split_address("orders"), (None, "orders"));
        assert_eq!(split_address("topic://a.b"), (Some("topic"), "a.b"));
    }

    #[test]
    fn matches_wildcards() {
        let cases = [
            ("orders.created", "orders.created", true),
            ("orders.created", "orders.updated", false),
            ("orders.created", "orders", false),
            ("orders", "orders.created", false),
            ("orders.*", "orders.created", true),
            ("orders.*", "orders", false),
            ("orders.*", "orders.eu.created", false),
            ("*.created", "orders.created", true),
            ("orders.*.created", "orders.eu.created", true),
            ("orders.*.created", "orders.eu.updated", false),
            ("orders.>", "orders.created", true),
            ("orders.>", "orders.eu.created", true),
            ("orders.>", "orders", false),
            (">", "anything.at.all", true),
            ("*", "one", true),
            ("*", "one.two", false),
        ];
        for (pattern, topic, expected) in cases {
            assert_eq!(
                topic_matches(pattern, topic),
                expected,
                "pattern {pattern:?} vs topic {topic:?}"
            );
        }
    }

    #[test]
    fn validates_topics_and_patterns() {
        assert!(validate_topic("orders.created").is_ok());
        for bad in ["", "orders..created", "orders.*", "orders.>", ".x"] {
            assert!(validate_topic(bad).is_err(), "{bad:?}");
        }
        for good in ["orders", "orders.*", "orders.>", "*.created", ">"] {
            assert!(validate_pattern(good).is_ok(), "{good:?}");
        }
        for bad in ["", "orders..x", "orders.>.x", ">.x"] {
            assert!(validate_pattern(bad).is_err(), "{bad:?}");
        }
    }
}

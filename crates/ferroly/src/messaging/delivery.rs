//! The delivery context: [`Delivery`], [`DeliveryInfo`], [`Acker`] and the
//! provider-side [`DeliveryControl`].

use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ferroly::messaging::{Ack, BoxFuture, Handler, Message, MessagingError};

/// Facts about one delivery of a message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DeliveryInfo {
    /// The address the message was consumed from.
    pub address: String,
    /// Which delivery attempt this is, starting at 1.
    pub attempt: u32,
    /// Whether the message was delivered before (`attempt > 1`).
    pub redelivered: bool,
    /// When the provider accepted the message, if it knows.
    pub enqueued_at: Option<SystemTime>,
    /// The provider's sequence number or offset for the message, if it has one.
    pub sequence: Option<u64>,
    /// The subscription the message was delivered to, for topics.
    pub subscription: Option<String>,
}

impl DeliveryInfo {
    /// The first delivery attempt from `address`.
    pub fn new(address: impl Into<String>) -> Self {
        Self {
            address: address.into(),
            attempt: 1,
            ..Self::default()
        }
    }

    /// Sets the attempt number (and [`redelivered`](Self::redelivered) to
    /// `attempt > 1`).
    pub fn with_attempt(mut self, attempt: u32) -> Self {
        self.attempt = attempt;
        self.redelivered = attempt > 1;
        self
    }

    /// Sets when the provider accepted the message.
    pub fn with_enqueued_at(mut self, at: SystemTime) -> Self {
        self.enqueued_at = Some(at);
        self
    }

    /// Sets the provider's sequence number for the message.
    pub fn with_sequence(mut self, sequence: u64) -> Self {
        self.sequence = Some(sequence);
        self
    }

    /// Sets the subscription the message was delivered to.
    pub fn with_subscription(mut self, subscription: impl Into<String>) -> Self {
        self.subscription = Some(subscription.into());
        self
    }
}

/// The provider's side of a [`Delivery`]: what a handler can ask of the
/// broker about the message it is handling. Providers implement this.
pub trait DeliveryControl: Send + Sync {
    /// Asks for `by` more time before the message is redelivered to someone
    /// else. A provider without leases returns `Ok(())`.
    fn extend(&self, by: Duration) -> BoxFuture<'_, Result<(), MessagingError>>;
    /// Records a free-text description to attach if the message is
    /// dead-lettered.
    fn set_dead_letter_description(&self, description: String);
    /// Settles a delivery whose handler returned [`Ack::Pending`]. Returns
    /// `true` if this call settled it, `false` if it was already settled or
    /// was not pending. `Ack::Pending` itself is never a valid resolution.
    fn resolve(&self, ack: Ack) -> bool;
}

/// A message together with its delivery context.
pub struct Delivery {
    /// The message.
    pub message: Message,
    info: DeliveryInfo,
    control: Arc<dyn DeliveryControl>,
}

/// Settles a delivery later, or from another task. Obtained from
/// [`Delivery::acker`]; the handler must return [`Ack::Pending`].
#[derive(Clone)]
pub struct Acker {
    control: Arc<dyn DeliveryControl>,
}

/// A handler that receives the delivery context.
pub type DeliveryHandler = Arc<dyn Fn(Delivery) -> BoxFuture<'static, Ack> + Send + Sync>;

/// The control behind [`Delivery::detached`]: no lease, nothing to settle.
struct Detached;

impl DeliveryControl for Detached {
    fn extend(&self, _by: Duration) -> BoxFuture<'_, Result<(), MessagingError>> {
        Box::pin(async { Ok(()) })
    }

    fn set_dead_letter_description(&self, _description: String) {}

    fn resolve(&self, _ack: Ack) -> bool {
        false
    }
}

impl Delivery {
    /// A delivery of `message`, described by `info`, controlled by the
    /// provider through `control`.
    pub fn new(message: Message, info: DeliveryInfo, control: Arc<dyn DeliveryControl>) -> Self {
        Self {
            message,
            info,
            control,
        }
    }

    /// A delivery with no provider behind it: `extend` succeeds and does nothing,
    /// and nothing can be settled later. For unit-testing handlers.
    pub fn detached(message: Message) -> Self {
        Self::new(message, DeliveryInfo::new(""), Arc::new(Detached))
    }

    /// Facts about this delivery.
    pub fn info(&self) -> &DeliveryInfo {
        &self.info
    }

    /// The message, giving up the delivery context.
    pub fn into_message(self) -> Message {
        self.message
    }

    /// Asks for `by` more time before the message is redelivered elsewhere
    /// ([`DeliveryControl::extend`]).
    pub async fn extend(&self, by: Duration) -> Result<(), MessagingError> {
        self.control.extend(by).await
    }

    /// A handle that settles this delivery later; return [`Ack::Pending`]
    /// from the handler after taking one.
    pub fn acker(&self) -> Acker {
        Acker {
            control: self.control.clone(),
        }
    }

    /// Sets the description recorded if the handler returns `Ack::DeadLetter`
    /// (or the message is dead-lettered after its last attempt).
    pub fn dead_letter_description(&self, description: impl Into<String>) {
        self.control.set_dead_letter_description(description.into());
    }
}

impl fmt::Debug for Delivery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Delivery")
            .field("message", &self.message)
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

impl Acker {
    /// Settles the delivery as [`Ack::Ok`]. `true` if this call settled it.
    pub fn ack(&self) -> bool {
        self.control.resolve(Ack::Ok)
    }

    /// Settles the delivery as [`Ack::Retry`]. `true` if this call settled it.
    pub fn retry(&self) -> bool {
        self.control.resolve(Ack::Retry)
    }

    /// Settles the delivery as [`Ack::RetryAfter`]. `true` if this call
    /// settled it.
    pub fn retry_after(&self, delay: Duration) -> bool {
        self.control.resolve(Ack::RetryAfter(delay))
    }

    /// Settles the delivery as [`Ack::Reject`]. `true` if this call settled it.
    pub fn reject(&self) -> bool {
        self.control.resolve(Ack::Reject)
    }

    /// Records `description` and settles the delivery as [`Ack::DeadLetter`].
    /// `true` if this call settled it.
    pub fn dead_letter(&self, description: impl Into<String>) -> bool {
        self.control.set_dead_letter_description(description.into());
        self.control.resolve(Ack::DeadLetter)
    }
}

impl fmt::Debug for Acker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Acker")
    }
}

/// Wraps an async closure as a [`DeliveryHandler`].
pub fn delivery_handler<F, Fut>(f: F) -> DeliveryHandler
where
    F: Fn(Delivery) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Ack> + Send + 'static,
{
    Arc::new(move |delivery| Box::pin(f(delivery)))
}

/// Adapts a plain [`Handler`] to a [`DeliveryHandler`]. An `Ack::Pending`
/// from the plain handler becomes `Ack::Retry`, since it has no `Acker`.
pub(crate) fn from_handler(handler: Handler) -> DeliveryHandler {
    Arc::new(move |delivery: Delivery| {
        let fut = handler(delivery.message);
        Box::pin(async move {
            match fut.await {
                Ack::Pending => Ack::Retry,
                ack => ack,
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_builders_set_the_fields() {
        let info = DeliveryInfo::new("chan://q");
        assert_eq!(info.attempt, 1);
        assert!(!info.redelivered);
        let info = info
            .with_attempt(3)
            .with_sequence(9)
            .with_subscription("billing");
        assert!(info.redelivered);
        assert_eq!(info.sequence, Some(9));
        assert_eq!(info.subscription.as_deref(), Some("billing"));
    }

    #[test]
    fn debug_does_not_need_the_control() {
        let delivery = Delivery::detached(Message::text("x"));
        assert!(format!("{delivery:?}").starts_with("Delivery"));
        assert_eq!(format!("{:?}", delivery.acker()), "Acker");
    }
}

//! Send, listener and provider options.

use std::time::Duration;

/// What `send` does when the target listener's buffer is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnFull {
    /// Fail at once with [`MessagingError::BufferFull`](crate::messaging::MessagingError::BufferFull),
    /// handing the message back. The default.
    #[default]
    Reject,
    /// Wait until the buffer has room (unbounded backpressure).
    Block,
    /// Wait up to the given time, then fail with
    /// [`MessagingError::SendTimeout`](crate::messaging::MessagingError::SendTimeout),
    /// handing the message back.
    BlockFor(Duration),
    /// Make room by discarding the oldest buffered message, which is reported
    /// through [`Observer::on_dropped`](crate::messaging::Observer::on_dropped)
    /// with [`DropReason::Evicted`](crate::messaging::DropReason::Evicted).
    /// Never blocks, never fails for lack of room.
    DropOldest,
}

/// Per-send options.
///
/// ```
/// use std::time::Duration;
/// use ferroly::messaging::{OnFull, SendOptions};
///
/// let opts = SendOptions::new()
///     .routing_key("customer-42")
///     .on_full(OnFull::BlockFor(Duration::from_secs(1)))
///     .ttl(Duration::from_secs(30));
/// assert_eq!(opts.full_policy(), OnFull::BlockFor(Duration::from_secs(1)));
/// ```
#[derive(Debug, Clone, Default)]
pub struct SendOptions {
    /// Overrides the message's routing key.
    pub routing_key: Option<String>,
    /// Shorthand for `on_full: Some(OnFull::Block)`; ignored when
    /// [`on_full`](Self::on_full) is set.
    pub block_on_full: bool,
    /// The backpressure policy. `None` means [`OnFull::Block`] if
    /// [`block_on_full`](Self::block_on_full) is set, else [`OnFull::Reject`].
    pub on_full: Option<OnFull>,
    /// Time to live: a message still undelivered this long after it was
    /// accepted expires instead of being handed to a handler (see
    /// [`ExpiryAction`]). `None` = never expires.
    pub ttl: Option<Duration>,
    /// Delayed delivery: the message becomes visible to its listener only after
    /// this long. `None` = immediately.
    pub delay: Option<Duration>,
}

impl SendOptions {
    /// The defaults: no routing-key override, reject when full, no TTL, no
    /// delay.
    pub fn new() -> Self {
        Self::default()
    }

    /// Overrides the message's routing key.
    pub fn routing_key(mut self, key: impl Into<String>) -> Self {
        self.routing_key = Some(key.into());
        self
    }

    /// Sets the backpressure policy.
    pub fn on_full(mut self, policy: OnFull) -> Self {
        self.on_full = Some(policy);
        self
    }

    /// Sets the time to live.
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = Some(ttl);
        self
    }

    /// Delays delivery.
    pub fn delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }

    /// The effective backpressure policy (resolving the
    /// [`block_on_full`](Self::block_on_full) shorthand).
    pub fn full_policy(&self) -> OnFull {
        self.on_full.unwrap_or(if self.block_on_full {
            OnFull::Block
        } else {
            OnFull::Reject
        })
    }
}

/// The delay between redeliveries of a message whose handler returned
/// [`Ack::Retry`](crate::messaging::Ack::Retry) (or panicked).
///
/// The delay before redelivery *n* (1-based) is
/// `initial * multiplier^(n-1)`, capped at `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// The delay before the first redelivery.
    pub initial: Duration,
    /// The upper bound on the delay.
    pub max: Duration,
    /// The growth factor per redelivery (`1` = constant delay). `0` is treated
    /// as `1`.
    pub multiplier: u32,
}

impl Default for Backoff {
    /// 100 ms, doubling, capped at 30 s.
    fn default() -> Self {
        Self::exponential(Duration::from_millis(100), Duration::from_secs(30))
    }
}

impl Backoff {
    /// No delay: redeliver as soon as the consumer has yielded to the
    /// scheduler once.
    pub fn none() -> Self {
        Self::fixed(Duration::ZERO)
    }

    /// The same delay before every redelivery.
    pub fn fixed(delay: Duration) -> Self {
        Self {
            initial: delay,
            max: delay,
            multiplier: 1,
        }
    }

    /// A delay that doubles from `initial` up to `max`.
    pub fn exponential(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max: max.max(initial),
            multiplier: 2,
        }
    }

    /// The delay before redelivery number `redelivery` (1-based).
    pub fn delay(&self, redelivery: u32) -> Duration {
        let factor = self
            .multiplier
            .max(1)
            .checked_pow(redelivery.saturating_sub(1));
        factor
            .and_then(|f| self.initial.checked_mul(f))
            .map_or(self.max, |d| d.min(self.max))
    }
}

/// What happens to a message whose [`SendOptions::ttl`] ran out before a
/// handler took it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExpiryAction {
    /// Move it to the listener's dead-letter destination; if none is
    /// configured, drop it. The default.
    #[default]
    DeadLetter,
    /// Drop it.
    Drop,
}

/// Per-listener options.
#[derive(Debug, Clone)]
pub struct ListenerOptions {
    /// Per-listener buffer capacity (the backpressure bound). Must be at least
    /// 1; `0` is rejected with
    /// [`MessagingError::InvalidOptions`](crate::messaging::MessagingError::InvalidOptions).
    pub buffer_size: usize,
    /// Bound concurrent handler invocations (unset = serial, ordered). `Some(0)`
    /// is treated as `Some(1)`.
    pub concurrency: Option<usize>,
    /// Total delivery attempts before a `Retry` is dead-lettered.
    pub max_delivery_attempts: u32,
    /// Destination to route exhausted (and, by default, expired) messages to.
    /// `None` = drop and report through the observer.
    pub dead_letter: Option<String>,
    /// The delay between redeliveries. Default: 100 ms doubling up to 30 s.
    pub retry_backoff: Backoff,
    /// What to do with a message whose TTL ran out. Default:
    /// [`ExpiryAction::DeadLetter`].
    pub on_expired: ExpiryAction,
    /// For `topic://` listeners: the subscription name. Listeners that share a
    /// name on the same pattern form one subscription and compete for its
    /// messages; a listener with `None` is a subscription of its own and
    /// receives every matching message. Ignored for `chan://`.
    pub subscription: Option<String>,
}

impl Default for ListenerOptions {
    fn default() -> Self {
        Self {
            buffer_size: 256,
            concurrency: None,
            max_delivery_attempts: 3,
            dead_letter: None,
            retry_backoff: Backoff::default(),
            on_expired: ExpiryAction::default(),
            subscription: None,
        }
    }
}

/// Options for a [`LocalProvider`](crate::messaging::LocalProvider).
#[derive(Debug, Clone)]
pub struct LocalOptions {
    /// How long `stop()` (the lifecycle `Component` impl) and
    /// [`Provider::close`](crate::messaging::Provider::close) wait for buffered
    /// messages to be handled before giving up. Default 20 s.
    ///
    /// Keep it below the stop timeout of whatever supervises the provider (the
    /// lifecycle manager's default is 30 s): if the supervisor gives up first,
    /// the drain is cut short and the provider is stopped at once.
    pub drain_timeout: Duration,
    /// De-duplication window: the number of recent message ids remembered per
    /// destination. A message whose id ([`headers::MESSAGE_ID`]) is still in
    /// the window is dropped as a duplicate. `0` (the default) disables
    /// de-duplication.
    ///
    /// [`headers::MESSAGE_ID`]: crate::messaging::headers::MESSAGE_ID
    pub dedup_window: usize,
}

impl Default for LocalOptions {
    fn default() -> Self {
        Self {
            drain_timeout: Duration::from_secs(20),
            dedup_window: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let b = Backoff::exponential(Duration::from_millis(100), Duration::from_millis(450));
        assert_eq!(b.delay(1), Duration::from_millis(100));
        assert_eq!(b.delay(2), Duration::from_millis(200));
        assert_eq!(b.delay(3), Duration::from_millis(400));
        assert_eq!(b.delay(4), Duration::from_millis(450));
        assert_eq!(b.delay(200), Duration::from_millis(450));
        assert_eq!(b.delay(0), Duration::from_millis(100));

        assert_eq!(
            Backoff::fixed(Duration::from_secs(1)).delay(9),
            Duration::from_secs(1)
        );
        assert_eq!(Backoff::none().delay(3), Duration::ZERO);
        let zero_multiplier = Backoff {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(9),
            multiplier: 0,
        };
        assert_eq!(zero_multiplier.delay(5), Duration::from_secs(1));
    }

    #[test]
    fn full_policy_resolves_the_legacy_flag() {
        assert_eq!(SendOptions::default().full_policy(), OnFull::Reject);
        let legacy = SendOptions {
            block_on_full: true,
            ..Default::default()
        };
        assert_eq!(legacy.full_policy(), OnFull::Block);
        let explicit = SendOptions {
            block_on_full: true,
            on_full: Some(OnFull::DropOldest),
            ..Default::default()
        };
        assert_eq!(explicit.full_policy(), OnFull::DropOldest);
    }
}

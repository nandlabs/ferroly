//! The [`Observer`] hooks and the log-backed observer.

use std::time::Duration;

use ferroly::messaging::{Ack, Message};

/// Why a message was discarded without reaching a handler (or without being
/// completed by one).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DropReason {
    /// Its time to live ran out and it was not dead-lettered.
    Expired,
    /// It was the oldest buffered message when a send used
    /// [`OnFull::DropOldest`](crate::messaging::OnFull::DropOldest).
    Evicted,
    /// A message with the same id was accepted within the de-duplication
    /// window.
    Duplicate,
    /// It was published to a topic that has no matching subscription.
    NoSubscribers,
    /// A topic subscription's buffer was full (or stayed full for the allowed
    /// wait), so that subscription missed the message.
    BufferFull,
    /// The provider was stopped or dropped before the message could be
    /// handled.
    Shutdown,
}

impl DropReason {
    /// The lowercase name of the reason, handy as a metrics label.
    pub fn as_str(self) -> &'static str {
        match self {
            DropReason::Expired => "expired",
            DropReason::Evicted => "evicted",
            DropReason::Duplicate => "duplicate",
            DropReason::NoSubscribers => "no_subscribers",
            DropReason::BufferFull => "buffer_full",
            DropReason::Shutdown => "shutdown",
        }
    }
}

/// Hooks for the life of each message (no-ops by default). Wire one to emit
/// logs, metrics, or traces.
///
/// Callbacks run synchronously on the task that caused the event and never
/// while a provider lock is held; keep them short and non-blocking. A panic
/// inside a callback is caught and discarded.
///
/// The hooks map onto the usual messaging metrics:
///
/// | Metric | Hook |
/// |---|---|
/// | enqueued | [`on_send`](Self::on_send) |
/// | delivered | [`on_receive`](Self::on_receive) |
/// | acked | [`on_ack`](Self::on_ack) |
/// | retried | [`on_nack`](Self::on_nack) with `requeue = true` |
/// | rejected | [`on_nack`](Self::on_nack) with `requeue = false` |
/// | dead-lettered | [`on_dead_letter`](Self::on_dead_letter) |
/// | dropped | [`on_dropped`](Self::on_dropped), [`on_dead_letter`](Self::on_dead_letter) with `to_dlq = false` |
/// | expired | [`on_expired`](Self::on_expired) |
/// | queue depth | [`on_queue_depth`](Self::on_queue_depth) |
/// | handler duration | [`on_handled`](Self::on_handled) |
/// | handler panics | [`on_handler_panic`](Self::on_handler_panic) |
pub trait Observer: Send + Sync {
    /// A message was accepted into a listener's buffer at `addr`.
    fn on_send(&self, addr: &str, msg: &Message) {
        let _ = (addr, msg);
    }
    /// A message is about to be handed to the handler listening on `addr`.
    /// Fires once per delivery attempt.
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
    /// A message reached the end of its redelivery attempts (or expired and was
    /// routed onwards). `to_dlq` is `true` when it was moved to a dead-letter
    /// destination, `false` when it was dropped (no DLQ configured, or the DLQ
    /// could not take it).
    fn on_dead_letter(&self, addr: &str, msg: &Message, to_dlq: bool) {
        let _ = (addr, msg, to_dlq);
    }
    /// A handler invocation finished after `elapsed`, with `ack` as its
    /// disposition (a panic counts as [`Ack::Retry`]).
    fn on_handled(&self, addr: &str, msg: &Message, elapsed: Duration, ack: Ack) {
        let _ = (addr, msg, elapsed, ack);
    }
    /// A handler panicked while processing `msg`; `panic` is the panic payload
    /// when it was a string. The message is then treated as
    /// [`Ack::Retry`].
    fn on_handler_panic(&self, addr: &str, msg: &Message, panic: &str) {
        let _ = (addr, msg, panic);
    }
    /// A message's time to live ran out before a handler took it. Followed by
    /// [`on_dead_letter`](Self::on_dead_letter) or
    /// [`on_dropped`](Self::on_dropped), depending on the listener's
    /// [`ExpiryAction`](crate::messaging::ExpiryAction).
    fn on_expired(&self, addr: &str, msg: &Message) {
        let _ = (addr, msg);
    }
    /// A message was discarded for `reason` without being completed by a
    /// handler.
    fn on_dropped(&self, addr: &str, msg: &Message, reason: DropReason) {
        let _ = (addr, msg, reason);
    }
    /// The number of messages buffered for `addr` changed to `depth`. Fires
    /// after each enqueue and dequeue.
    fn on_queue_depth(&self, addr: &str, depth: usize) {
        let _ = (addr, depth);
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
    fn on_handler_panic(&self, addr: &str, msg: &Message, panic: &str) {
        self.logger.error(
            "message.handler_panic",
            &[
                ("addr", addr.into()),
                ("panic", panic.into()),
                ("delivery", (msg.delivery_count as u64).into()),
            ],
        );
    }
    fn on_expired(&self, addr: &str, _msg: &Message) {
        self.logger
            .warn("message.expired", &[("addr", addr.into())]);
    }
    fn on_dropped(&self, addr: &str, msg: &Message, reason: DropReason) {
        self.logger.warn(
            "message.dropped",
            &[
                ("addr", addr.into()),
                ("reason", reason.as_str().into()),
                ("bytes", (msg.body.len() as u64).into()),
            ],
        );
    }
}

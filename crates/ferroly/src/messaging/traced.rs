//! [`Traced`]: opt-in `obs` spans around any messaging provider.

use std::sync::Arc;

use ferroly::messaging::{
    split_address, Ack, BoxFuture, Capabilities, Delivery, DeliveryHandler, Handler,
    ListenerOptions, Message, MessagingError, Observer, Producer, Provider, Receipt, Receiver,
    SendOptions,
};
use ferroly::obs::{self, ActiveSpan, Context, Link, Status};

/// Wraps a [`Producer`] / [`Receiver`] / [`Provider`] so that sends and
/// deliveries are traced with [`ferroly::obs`] spans. Requires the `obs`
/// feature; without the wrapper, messaging creates no spans.
///
/// - **Publishing** ([`send`](Producer::send),
///   [`send_with_receipt`](Producer::send_with_receipt)): a
///   [`SpanKind::Producer`](obs::SpanKind::Producer) span
///   ([`obs::messaging::publish_span`], named `send {destination}`) whose
///   parent is the trace context the message already carries (its
///   `traceparent`, e.g. from [`Message::with_trace_from`]), else the current
///   context. The message's `traceparent` / `tracestate` are then set to the
///   producer span's context, so the consumer continues the trace from it. A
///   failed send marks the span as an error.
/// - **Batches** ([`send_batch`](Producer::send_batch)): one producer span
///   for the whole batch (field `messaging.batch.message_count`), child of
///   the current context; every message carries its context, and a message
///   that already carried a trace context is linked from it.
/// - **Consuming** ([`add_listener`](Receiver::add_listener),
///   [`add_delivery_listener`](Receiver::add_delivery_listener)): each
///   delivery runs inside a [`SpanKind::Consumer`](obs::SpanKind::Consumer)
///   span ([`obs::messaging::process_span`], named `process {destination}`)
///   whose parent is extracted from the message's `traceparent`. The span is
///   current while the handler runs, so the handler's own spans are its
///   children. The handler's [`Ack`] is recorded as `messaging.ack` (`ok`,
///   `retry`, `retry_after`, `reject`, `dead_letter`, `pending`); every
///   outcome except `ok` and `pending` also marks the span as an error.
///
/// `system` names the backend in `messaging.system` (`"local"`, `"kafka"`,
/// …); the destination is the address without its scheme (`orders` for
/// `chan://orders`).
///
/// ```
/// # #[tokio::main]
/// # async fn main() {
/// use ferroly::messaging::{handler, Ack, LocalProvider, Message, Producer, Receiver, Traced};
///
/// let bus = Traced::new(LocalProvider::new("bus"), "local");
/// bus.add_listener("chan://orders", handler(|_msg| async { Ack::Ok }), Default::default())
///     .await
///     .unwrap();
/// bus.send("chan://orders", Message::text("order-1"), Default::default())
///     .await
///     .unwrap();
/// bus.inner().shutdown(std::time::Duration::from_secs(5)).await;
/// # }
/// ```
pub struct Traced<P> {
    inner: P,
    system: Arc<str>,
}

impl<P> std::fmt::Debug for Traced<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Traced")
            .field("system", &&*self.system)
            .finish_non_exhaustive()
    }
}

impl<P> Traced<P> {
    /// Wraps `inner`, reporting `system` as `messaging.system`.
    pub fn new(inner: P, system: impl Into<String>) -> Self {
        Self {
            inner,
            system: Arc::from(system.into()),
        }
    }

    /// The wrapped provider (for its own methods, such as
    /// `LocalProvider::shutdown`).
    pub fn inner(&self) -> &P {
        &self.inner
    }

    /// Unwraps the provider.
    pub fn into_inner(self) -> P {
        self.inner
    }
}

/// The trace context a message carries, if any.
fn message_context(msg: &Message) -> Option<Context> {
    obs::extract(|name| match name {
        obs::TRACEPARENT_HEADER => msg.traceparent(),
        obs::TRACESTATE_HEADER => msg.tracestate(),
        _ => None,
    })
}

/// Writes `span`'s context into `msg` (or, for an inert span, the current
/// context when the message carries none).
fn stamp(msg: &mut Message, span: &ActiveSpan) {
    match span.context() {
        Some(ctx) => {
            msg.properties.traceparent = Some(ctx.to_traceparent());
            msg.properties.tracestate = ctx.trace_state().map(str::to_string);
        }
        None if msg.traceparent().is_none() => {
            if let Some(ctx) = obs::current() {
                msg.properties.traceparent = Some(ctx.to_traceparent());
                msg.properties.tracestate = ctx.trace_state().map(str::to_string);
            }
        }
        None => {}
    }
}

fn record_send<T>(span: ActiveSpan, result: &Result<T, MessagingError>) {
    if let Err(e) = result {
        span.set_error(e.to_string());
    }
    span.end();
}

fn record_ack(span: &ActiveSpan, ack: Ack) {
    let (name, failed) = match ack {
        Ack::Ok => ("ok", false),
        Ack::Retry => ("retry", true),
        Ack::RetryAfter(_) => ("retry_after", true),
        Ack::Reject => ("reject", true),
        Ack::DeadLetter => ("dead_letter", true),
        Ack::Pending => ("pending", false),
    };
    span.record("messaging.ack", name);
    if failed {
        span.set_status(Status::error(format!("message not processed: {name}")));
    }
}

impl<P> Traced<P> {
    fn destination(addr: &str) -> &str {
        split_address(addr).1
    }

    fn publish_span(&self, addr: &str, msg: &Message) -> ActiveSpan {
        let parent = message_context(msg);
        let mut span = obs::messaging::publish_span(&self.system, Self::destination(addr));
        if let Some(parent) = &parent {
            span = span.child_of(parent);
        }
        if let Some(id) = msg.id() {
            span = span.field("messaging.message.id", id.to_string());
        }
        span.start()
    }
}

/// Starts the consumer span for one delivery of `msg`; returns it with the
/// message's own trace context.
fn consumer_span(system: &str, destination: &str, msg: &Message) -> (ActiveSpan, Option<Context>) {
    let parent = message_context(msg);
    let mut span = obs::messaging::process_span(system, destination)
        .child_of_opt(parent.as_ref())
        .field(
            "messaging.message.delivery_count",
            u64::from(msg.delivery_count),
        );
    if let Some(id) = msg.id() {
        span = span.field("messaging.message.id", id.to_string());
    }
    (span.start(), parent)
}

/// Calls the handler inside its consumer span's context, so code it runs
/// before returning its future is a child of the span too, awaits it, and
/// records the outcome. A handler that panics (while called or polled) ends
/// the span with the error `handler panicked`.
fn run_in(
    span: ActiveSpan,
    parent: Option<Context>,
    start: impl FnOnce() -> BoxFuture<'static, Ack> + Send + 'static,
) -> BoxFuture<'static, Ack> {
    /// Ends the span as failed if the handler unwinds through it.
    struct OnPanic(Option<ActiveSpan>);
    impl Drop for OnPanic {
        fn drop(&mut self) {
            if let Some(span) = self.0.take() {
                if std::thread::panicking() {
                    span.set_error("handler panicked");
                }
                span.end();
            }
        }
    }
    Box::pin(async move {
        let ctx = span.context().or(parent);
        let mut guard = OnPanic(Some(span));
        let run = async move { start().await };
        let ack = match ctx {
            Some(ctx) => obs::with_context(ctx, run).await,
            None => run.await,
        };
        if let Some(span) = guard.0.take() {
            record_ack(&span, ack);
            span.end();
        }
        ack
    })
}

impl<P: Producer> Producer for Traced<P> {
    fn send(
        &self,
        addr: &str,
        mut msg: Message,
        opts: SendOptions,
    ) -> BoxFuture<'_, Result<(), MessagingError>> {
        // Spans start on the first poll, so a context attached around the
        // returned future (`obs::with_context`, `instrument`) is the parent.
        let addr = addr.to_string();
        Box::pin(async move {
            let span = self.publish_span(&addr, &msg);
            stamp(&mut msg, &span);
            let result = self.inner.send(&addr, msg, opts).await;
            record_send(span, &result);
            result
        })
    }

    fn send_with_receipt(
        &self,
        addr: &str,
        mut msg: Message,
        opts: SendOptions,
    ) -> BoxFuture<'_, Result<Receipt, MessagingError>> {
        let addr = addr.to_string();
        Box::pin(async move {
            let span = self.publish_span(&addr, &msg);
            stamp(&mut msg, &span);
            let result = self.inner.send_with_receipt(&addr, msg, opts).await;
            record_send(span, &result);
            result
        })
    }

    fn send_batch(
        &self,
        addr: &str,
        mut msgs: Vec<Message>,
        opts: SendOptions,
    ) -> BoxFuture<'_, Result<(), MessagingError>> {
        let addr = addr.to_string();
        Box::pin(async move {
            let mut span = obs::messaging::publish_span(&self.system, Self::destination(&addr))
                .field("messaging.batch.message_count", msgs.len() as u64);
            for msg in &msgs {
                if let Some(ctx) = message_context(msg) {
                    span = span.link(Link::new(&ctx));
                }
            }
            let span = span.start();
            for msg in &mut msgs {
                stamp(msg, &span);
            }
            let result = self.inner.send_batch(&addr, msgs, opts).await;
            record_send(span, &result);
            result
        })
    }
}

impl<P: Receiver> Receiver for Traced<P> {
    fn add_listener(
        &self,
        addr: &str,
        handler: Handler,
        opts: ListenerOptions,
    ) -> BoxFuture<'_, Result<(), MessagingError>> {
        let system = self.system.clone();
        let destination: Arc<str> = Arc::from(Self::destination(addr));
        let traced: Handler = Arc::new(move |msg: Message| {
            let (span, parent) = consumer_span(&system, &destination, &msg);
            let handler = handler.clone();
            run_in(span, parent, move || handler(msg))
        });
        self.inner.add_listener(addr, traced, opts)
    }

    fn add_delivery_listener(
        &self,
        addr: &str,
        handler: DeliveryHandler,
        opts: ListenerOptions,
    ) -> BoxFuture<'_, Result<(), MessagingError>> {
        let system = self.system.clone();
        let destination: Arc<str> = Arc::from(Self::destination(addr));
        let traced: DeliveryHandler = Arc::new(move |delivery: Delivery| {
            let (span, parent) = consumer_span(&system, &destination, &delivery.message);
            let handler = handler.clone();
            run_in(span, parent, move || handler(delivery))
        });
        self.inner.add_delivery_listener(addr, traced, opts)
    }

    fn remove_listeners(&self, addr: &str) -> BoxFuture<'_, Result<usize, MessagingError>> {
        self.inner.remove_listeners(addr)
    }
}

impl<P: Provider> Provider for Traced<P> {
    fn schemes(&self) -> &[&str] {
        self.inner.schemes()
    }

    fn setup(&self) -> BoxFuture<'_, Result<(), MessagingError>> {
        self.inner.setup()
    }

    fn close(&self) -> BoxFuture<'_, Result<(), MessagingError>> {
        self.inner.close()
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn set_observer(&self, observer: Arc<dyn Observer>) -> Result<(), MessagingError> {
        self.inner.set_observer(observer)
    }
}

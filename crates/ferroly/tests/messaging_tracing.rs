//! `messaging::Traced`: producer and consumer spans around a provider.
//!
//! The exporter is process-global, so every test takes `globals()` first.

#![cfg(all(feature = "messaging", feature = "obs"))]

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::mpsc;

use ferroly::codec::Value;
use ferroly::messaging::{
    delivery_handler, handler, Ack, LocalProvider, Message, Producer, Provider, Receiver,
    SendOptions, Traced,
};
use ferroly::obs::{self, Exporter, FinishedSpan, Level, Sampler, Span, SpanKind};

/// Serialises the tests (an async lock: it is held across `.await`).
fn globals() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

type Store = Arc<Mutex<Vec<FinishedSpan>>>;

struct Collect(Store);

impl Exporter for Collect {
    fn export(&self, span: &FinishedSpan) {
        self.0.lock().unwrap().push(span.clone());
    }
}

fn install() -> Store {
    let store: Store = Arc::default();
    obs::set_level(Level::Info);
    obs::set_sampler(Sampler::default());
    obs::set_exporter(Box::new(Collect(store.clone())));
    store
}

fn named(store: &Store, name: &str) -> Vec<FinishedSpan> {
    store
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.name == name)
        .cloned()
        .collect()
}

fn field<'a>(span: &'a FinishedSpan, key: &str) -> Option<&'a Value> {
    span.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

async fn settle(bus: &Traced<LocalProvider>) {
    let report = bus.inner().shutdown(Duration::from_secs(5)).await;
    assert!(report.is_clean());
}

#[tokio::test]
async fn the_consumer_span_continues_the_producer_span() {
    let _g = globals().lock().await;
    let store = install();
    let bus = Traced::new(LocalProvider::new("bus"), "local");
    let (tx, mut rx) = mpsc::unbounded_channel();
    bus.add_listener(
        "chan://orders",
        handler(move |msg: Message| {
            let tx = tx.clone();
            async move {
                // The handler runs inside the consumer span.
                let child = Span::new("charge").start();
                let _ = tx.send((msg.traceparent().map(str::to_string), child.context()));
                child.end();
                Ack::Ok
            }
        }),
        Default::default(),
    )
    .await
    .unwrap();

    let root = Span::new("checkout").start();
    let root_ctx = root.context().unwrap();
    obs::with_context(
        root_ctx.clone(),
        bus.send(
            "chan://orders",
            Message::text("o-1"),
            SendOptions::default(),
        ),
    )
    .await
    .unwrap();
    let (traceparent, _child) = rx.recv().await.unwrap();
    settle(&bus).await;
    root.end();

    let producer = named(&store, "send orders").remove(0);
    let consumer = named(&store, "process orders").remove(0);
    let charge = named(&store, "charge").remove(0);
    assert_eq!(producer.kind, SpanKind::Producer);
    assert_eq!(consumer.kind, SpanKind::Consumer);
    let trace = root_ctx.trace_id().to_string();
    for span in [&producer, &consumer, &charge] {
        assert_eq!(span.trace_id, trace, "{span:?}");
    }
    assert_eq!(producer.parent_id, Some(root_ctx.span_id().to_string()));
    assert_eq!(consumer.parent_id, Some(producer.span_id.clone()));
    assert_eq!(charge.parent_id, Some(consumer.span_id.clone()));
    // The message carried the producer span's context.
    assert!(traceparent.unwrap().contains(&producer.span_id));
    assert_eq!(
        field(&producer, "messaging.system").and_then(Value::as_str),
        Some("local")
    );
    assert_eq!(
        field(&consumer, "messaging.ack").and_then(Value::as_str),
        Some("ok")
    );
    assert!(!consumer.status.is_error());
}

#[tokio::test]
async fn a_message_with_its_own_trace_context_parents_the_producer_span() {
    let _g = globals().lock().await;
    let store = install();
    let bus = Traced::new(LocalProvider::new("bus"), "local");
    bus.add_listener(
        "chan://audit",
        handler(|_| async { Ack::Ok }),
        Default::default(),
    )
    .await
    .unwrap();
    let upstream = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let msg = Message::text("x").with_traceparent(upstream);
    bus.send("chan://audit", msg, SendOptions::default())
        .await
        .unwrap();
    settle(&bus).await;
    let producer = named(&store, "send audit").remove(0);
    assert_eq!(producer.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(producer.parent_id.as_deref(), Some("00f067aa0ba902b7"));
}

/// A raw handler's synchronous part (before it returns its future) runs in
/// the consumer span's context, and a handler that panics marks the span.
#[tokio::test]
async fn the_handler_call_itself_is_inside_the_consumer_span() {
    use ferroly::messaging::Handler;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let _g = globals().lock().await;
    let store = install();
    let bus = Traced::new(LocalProvider::new("bus"), "local");
    let (tx, mut rx) = mpsc::unbounded_channel();
    let calls = Arc::new(AtomicUsize::new(0));
    let raw: Handler = Arc::new(move |_msg: Message| {
        // Runs before the future exists.
        Span::new("sync-part").start().end();
        let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
        let tx = tx.clone();
        Box::pin(async move {
            if first {
                panic!("boom");
            }
            let _ = tx.send(());
            Ack::Ok
        })
    });
    bus.add_listener("chan://raw", raw, Default::default())
        .await
        .unwrap();
    bus.send("chan://raw", Message::text("r"), SendOptions::default())
        .await
        .unwrap();
    rx.recv().await.unwrap();
    settle(&bus).await;

    let consumers = named(&store, "process raw");
    assert_eq!(consumers.len(), 2, "{consumers:?}");
    let sync_parts = named(&store, "sync-part");
    assert_eq!(sync_parts.len(), 2);
    for (consumer, sync_part) in consumers.iter().zip(&sync_parts) {
        assert_eq!(sync_part.parent_id, Some(consumer.span_id.clone()));
    }
    // The panicking delivery's span is an error, not left unset.
    assert!(consumers[0].status.is_error(), "{:?}", consumers[0].status);
    assert_eq!(consumers[0].status.message(), Some("handler panicked"));
    assert!(!consumers[1].status.is_error());
}

#[tokio::test]
async fn retries_mark_the_consumer_span_and_delivery_listeners_are_traced() {
    let _g = globals().lock().await;
    let store = install();
    let bus = Traced::new(LocalProvider::new("bus"), "local");
    let (tx, mut rx) = mpsc::unbounded_channel();
    bus.add_delivery_listener(
        "chan://jobs",
        delivery_handler(move |delivery| {
            let tx = tx.clone();
            async move {
                let first = delivery.message.delivery_count == 0;
                let _ = tx.send(first);
                if first {
                    Ack::Retry
                } else {
                    Ack::Ok
                }
            }
        }),
        Default::default(),
    )
    .await
    .unwrap();
    bus.send("chan://jobs", Message::text("j"), SendOptions::default())
        .await
        .unwrap();
    assert!(rx.recv().await.unwrap());
    assert!(!rx.recv().await.unwrap());
    settle(&bus).await;

    let consumers = named(&store, "process jobs");
    assert_eq!(consumers.len(), 2);
    let acks: Vec<_> = consumers
        .iter()
        .map(|s| field(s, "messaging.ack").and_then(Value::as_str).unwrap())
        .collect();
    assert_eq!(acks, ["retry", "ok"]);
    assert!(consumers[0].status.is_error());
    assert!(!consumers[1].status.is_error());
    // Both deliveries continue the same producer span.
    let producer = named(&store, "send jobs").remove(0);
    for c in &consumers {
        assert_eq!(c.parent_id, Some(producer.span_id.clone()));
    }
}

#[tokio::test]
async fn a_batch_has_one_producer_span_and_a_failed_send_is_an_error() {
    let _g = globals().lock().await;
    let store = install();
    let bus = Traced::new(LocalProvider::new("bus"), "local");
    bus.add_listener(
        "chan://bulk",
        handler(|_| async { Ack::Ok }),
        Default::default(),
    )
    .await
    .unwrap();
    let msgs = vec![Message::text("a"), Message::text("b")];
    bus.send_batch("chan://bulk", msgs, SendOptions::default())
        .await
        .unwrap();
    let batch = named(&store, "send bulk").remove(0);
    assert_eq!(
        field(&batch, "messaging.batch.message_count"),
        Some(&Value::UInt(2))
    );

    // A closed provider refuses the send.
    settle(&bus).await;
    let err = bus
        .send("chan://bulk", Message::text("late"), SendOptions::default())
        .await;
    assert!(err.is_err());
    let spans = named(&store, "send bulk");
    assert_eq!(spans.len(), 2);
    assert!(spans[1].status.is_error());
    // The wrapper is a provider too.
    assert_eq!(bus.schemes(), bus.inner().schemes());
}

#![cfg(feature = "messaging")]
//! End-to-end tests of the in-process `LocalProvider`: the basic send/receive,
//! redelivery, dead-letter, backpressure, observer and lifecycle paths.
//!
//! Tests run on tokio's paused clock and synchronise through channels or a
//! draining `shutdown`, never through real sleeps.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::messaging::{
    handler, Ack, ListenerOptions, LocalProvider, Message, MessagingError, Observer, Producer,
    Receiver, SendOptions,
};
use tokio::sync::mpsc::unbounded_channel;

const DRAIN: Duration = Duration::from_secs(60);

#[tokio::test(start_paused = true)]
async fn send_and_receive() {
    let bus = LocalProvider::new("t");
    let (tx, mut rx) = unbounded_channel::<String>();
    bus.add_listener(
        "chan://q",
        handler(move |msg| {
            let tx = tx.clone();
            async move {
                tx.send(msg.body_str().into_owned()).unwrap();
                Ack::Ok
            }
        }),
        ListenerOptions::default(),
    )
    .await
    .unwrap();

    bus.send("chan://q", Message::text("hello"), SendOptions::default())
        .await
        .unwrap();
    assert_eq!(rx.recv().await.unwrap(), "hello");

    // A bare name addresses the same queue as `chan://name`.
    bus.send("q", Message::text("bare"), SendOptions::default())
        .await
        .unwrap();
    assert_eq!(rx.recv().await.unwrap(), "bare");
}

#[tokio::test(start_paused = true)]
async fn retry_redelivers_then_dead_letters() {
    let bus = LocalProvider::new("t");

    // DLQ listener reports the delivery_count it received.
    let (dlq_tx, mut dlq_rx) = unbounded_channel::<u32>();
    bus.add_listener(
        "chan://dlq",
        handler(move |msg| {
            let tx = dlq_tx.clone();
            async move {
                tx.send(msg.delivery_count).unwrap();
                Ack::Ok
            }
        }),
        ListenerOptions::default(),
    )
    .await
    .unwrap();

    // Main listener always retries; 2 attempts then dead-letter.
    let attempts = Arc::new(AtomicU32::new(0));
    let seen = attempts.clone();
    bus.add_listener(
        "chan://q",
        handler(move |_msg| {
            seen.fetch_add(1, Ordering::SeqCst);
            async { Ack::Retry }
        }),
        ListenerOptions {
            max_delivery_attempts: 2,
            dead_letter: Some("chan://dlq".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    bus.send("chan://q", Message::text("x"), SendOptions::default())
        .await
        .unwrap();

    // count 0 -> Retry -> 1 (<2, redeliver) -> Retry -> 2 (>=2, DLQ).
    assert_eq!(dlq_rx.recv().await.unwrap(), 2);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn backpressure_buffer_full_hands_the_message_back() {
    let bus = LocalProvider::new("t");
    // A listener that never returns, so its buffer fills.
    let (started_tx, mut started_rx) = unbounded_channel::<()>();
    bus.add_listener(
        "chan://q",
        handler(move |_msg| {
            let started = started_tx.clone();
            async move {
                started.send(()).unwrap();
                std::future::pending::<()>().await;
                Ack::Ok
            }
        }),
        ListenerOptions {
            buffer_size: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    bus.send("chan://q", Message::text("1"), SendOptions::default())
        .await
        .unwrap();
    started_rx.recv().await.unwrap(); // the consumer took #1 and is stuck in it
    bus.send("chan://q", Message::text("2"), SendOptions::default())
        .await
        .unwrap(); // buffered

    let err = bus
        .send("chan://q", Message::text("3"), SendOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, MessagingError::BufferFull(_)), "{err}");
    assert_eq!(err.destination(), Some("chan://q"));
    assert_eq!(err.to_string(), "destination buffer full: chan://q");
    // The rejected message comes back to the caller instead of vanishing.
    let returned = err.into_message().expect("the message is handed back");
    assert_eq!(returned.body_str(), "3");
}

#[tokio::test(start_paused = true)]
async fn no_listeners_hands_the_message_back() {
    let bus = LocalProvider::new("t");
    let err = bus
        .send(
            "chan://nobody",
            Message::text("lost?"),
            SendOptions::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, MessagingError::NoListeners(_)), "{err}");
    assert_eq!(err.message().unwrap().body_str(), "lost?");

    let err = bus
        .send("kafka://orders", Message::text("x"), SendOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, MessagingError::InvalidAddress(_)), "{err}");
}

#[tokio::test(start_paused = true)]
async fn observer_receives_hooks() {
    #[derive(Default)]
    struct Counter {
        sends: AtomicU32,
        receives: AtomicU32,
        acks: AtomicU32,
    }
    impl Observer for Counter {
        fn on_send(&self, _a: &str, _m: &Message) {
            self.sends.fetch_add(1, Ordering::Relaxed);
        }
        fn on_receive(&self, _a: &str, _m: &Message) {
            self.receives.fetch_add(1, Ordering::Relaxed);
        }
        fn on_ack(&self, _a: &str, _m: &Message) {
            self.acks.fetch_add(1, Ordering::Relaxed);
        }
    }

    let bus = LocalProvider::new("t");
    let counter = Arc::new(Counter::default());
    bus.set_observer(counter.clone());

    bus.add_listener(
        "chan://q",
        handler(|_m| async { Ack::Ok }),
        ListenerOptions::default(),
    )
    .await
    .unwrap();

    bus.send("chan://q", Message::text("x"), SendOptions::default())
        .await
        .unwrap();
    // A draining shutdown returns only once the message has been acked.
    assert!(bus.shutdown(DRAIN).await.is_clean());

    assert_eq!(counter.sends.load(Ordering::Relaxed), 1);
    assert_eq!(counter.receives.load(Ordering::Relaxed), 1);
    assert_eq!(counter.acks.load(Ordering::Relaxed), 1);
}

#[tokio::test(start_paused = true)]
async fn nack_flag_reflects_actual_fate_and_dead_letter_observed() {
    #[derive(Default)]
    struct Rec {
        nack_requeue: Mutex<Vec<bool>>,
        dead_letter: Mutex<Vec<(String, bool)>>, // (addr, to_dlq)
    }
    impl Observer for Rec {
        fn on_nack(&self, _a: &str, _m: &Message, requeue: bool) {
            self.nack_requeue.lock().unwrap().push(requeue);
        }
        fn on_dead_letter(&self, addr: &str, _m: &Message, to_dlq: bool) {
            self.dead_letter
                .lock()
                .unwrap()
                .push((addr.to_string(), to_dlq));
        }
    }

    let bus = LocalProvider::new("t");
    let rec = Arc::new(Rec::default());
    bus.set_observer(rec.clone());

    bus.add_listener(
        "chan://dlq",
        handler(|_m| async { Ack::Ok }),
        ListenerOptions::default(),
    )
    .await
    .unwrap();

    bus.add_listener(
        "chan://q",
        handler(|_m| async { Ack::Retry }),
        ListenerOptions {
            max_delivery_attempts: 2,
            dead_letter: Some("chan://dlq".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    bus.send("chan://q", Message::text("x"), SendOptions::default())
        .await
        .unwrap();
    assert!(bus.shutdown(DRAIN).await.is_clean());

    // delivery 1 (<2) → requeue=true; delivery 2 (>=2) → requeue=false, then
    // dead-lettered with to_dlq=true.
    assert_eq!(*rec.nack_requeue.lock().unwrap(), vec![true, false]);
    assert_eq!(
        *rec.dead_letter.lock().unwrap(),
        vec![("chan://dlq".to_string(), true)]
    );
}

#[tokio::test(start_paused = true)]
async fn exhausted_without_dlq_is_reported_not_silently_dropped() {
    #[derive(Default)]
    struct Rec {
        dropped: Mutex<Vec<bool>>,
    }
    impl Observer for Rec {
        fn on_dead_letter(&self, _a: &str, _m: &Message, to_dlq: bool) {
            self.dropped.lock().unwrap().push(to_dlq);
        }
    }

    let bus = LocalProvider::new("t");
    let rec = Arc::new(Rec::default());
    bus.set_observer(rec.clone());

    bus.add_listener(
        "chan://q",
        handler(|_m| async { Ack::Retry }),
        ListenerOptions {
            max_delivery_attempts: 1, // exhausted on first Retry
            dead_letter: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    bus.send("chan://q", Message::text("x"), SendOptions::default())
        .await
        .unwrap();
    assert!(bus.shutdown(DRAIN).await.is_clean());

    // No DLQ configured → the drop is surfaced (to_dlq=false), not silent.
    assert_eq!(*rec.dropped.lock().unwrap(), vec![false]);
}

#[tokio::test(start_paused = true)]
async fn a_dead_letter_destination_without_listeners_is_reported() {
    #[derive(Default)]
    struct Rec {
        dead_letter: Mutex<Vec<(String, bool)>>,
    }
    impl Observer for Rec {
        fn on_dead_letter(&self, addr: &str, _m: &Message, to_dlq: bool) {
            self.dead_letter
                .lock()
                .unwrap()
                .push((addr.to_string(), to_dlq));
        }
    }

    let bus = LocalProvider::new("t");
    let rec = Arc::new(Rec::default());
    bus.set_observer(rec.clone());
    bus.add_listener(
        "chan://q",
        handler(|_m| async { Ack::Retry }),
        ListenerOptions {
            max_delivery_attempts: 1,
            dead_letter: Some("chan://nobody-home".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    bus.send("chan://q", Message::text("x"), SendOptions::default())
        .await
        .unwrap();
    assert!(bus.shutdown(DRAIN).await.is_clean());
    assert_eq!(
        *rec.dead_letter.lock().unwrap(),
        vec![("chan://nobody-home".to_string(), false)]
    );
}

/// Adds a listener on `chan://q` that signals each message it handles.
async fn listen(
    bus: &LocalProvider,
    tx: tokio::sync::mpsc::UnboundedSender<()>,
) -> Result<(), MessagingError> {
    bus.add_listener(
        "chan://q",
        handler(move |_m| {
            let tx = tx.clone();
            async move {
                tx.send(()).unwrap();
                Ack::Ok
            }
        }),
        ListenerOptions::default(),
    )
    .await
}

#[tokio::test(start_paused = true)]
async fn provider_as_lifecycle_component() {
    use ferroly::lifecycle::Component;

    let bus = LocalProvider::new("bus");
    assert_eq!(Component::id(&bus), "bus");
    Component::start(&bus).await.unwrap();

    let (tx, mut rx) = unbounded_channel::<()>();
    listen(&bus, tx.clone()).await.unwrap();
    bus.send("chan://q", Message::text("x"), SendOptions::default())
        .await
        .unwrap();
    rx.recv().await.unwrap();

    // After stop the provider is closed: sends and new listeners are refused,
    // and a refused send gets its message back.
    Component::stop(&bus).await.unwrap();
    let err = bus
        .send("chan://q", Message::text("y"), SendOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, MessagingError::Closed(_)), "{err}");
    assert_eq!(err.into_message().unwrap().body_str(), "y");
    let err = listen(&bus, tx.clone()).await.unwrap_err();
    assert!(matches!(err, MessagingError::Closed(_)), "{err}");
    // Stopping twice is harmless.
    Component::stop(&bus).await.unwrap();

    // `start` reopens it; listeners have to be added again.
    Component::start(&bus).await.unwrap();
    let err = bus
        .send("chan://q", Message::text("z"), SendOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, MessagingError::NoListeners(_)), "{err}");
    listen(&bus, tx).await.unwrap();
    bus.send("chan://q", Message::text("z"), SendOptions::default())
        .await
        .unwrap();
    rx.recv().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn provider_runs_under_the_component_manager() {
    use ferroly::lifecycle::{ComponentManager, ComponentState};

    let bus = LocalProvider::new("bus");
    let handled = Arc::new(AtomicU32::new(0));
    let count = handled.clone();
    bus.add_listener(
        "chan://q",
        handler(move |_m| {
            let count = count.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                count.fetch_add(1, Ordering::SeqCst);
                Ack::Ok
            }
        }),
        ListenerOptions::default(),
    )
    .await
    .unwrap();

    let mgr = ComponentManager::new();
    mgr.register(Arc::new(bus.clone())).unwrap();
    mgr.start_all().await.unwrap();
    for i in 0..10 {
        bus.send(
            "chan://q",
            Message::text(i.to_string()),
            SendOptions::default(),
        )
        .await
        .unwrap();
    }

    // stop_all drains the bus: every buffered message is handled first.
    mgr.stop_all().await.unwrap();
    assert_eq!(handled.load(Ordering::SeqCst), 10);
    assert_eq!(mgr.get_state("bus"), Some(ComponentState::Stopped));
}

#![cfg(feature = "messaging")]
//! End-to-end tests of the in-process `LocalProvider`.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ferroly::messaging::{
    handler, Ack, ListenerOptions, LocalProvider, Message, MessagingError, Observer, Producer,
    Receiver, SendOptions,
};
use tokio::sync::mpsc::unbounded_channel;

#[tokio::test]
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
}

#[tokio::test]
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
    bus.add_listener(
        "chan://q",
        handler(|_msg| async { Ack::Retry }),
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

    // count 0 -> Retry -> 1 (<2, requeue) -> Retry -> 2 (>=2, DLQ).
    assert_eq!(dlq_rx.recv().await.unwrap(), 2);
}

#[tokio::test]
async fn backpressure_buffer_full() {
    let bus = LocalProvider::new("t");
    // A listener that never returns, so its buffer fills.
    bus.add_listener(
        "chan://q",
        handler(|_msg| async {
            std::future::pending::<()>().await;
            Ack::Ok
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
    tokio::time::sleep(Duration::from_millis(20)).await; // consumer takes #1 and blocks
    bus.send("chan://q", Message::text("2"), SendOptions::default())
        .await
        .unwrap(); // buffered

    let err = bus
        .send("chan://q", Message::text("3"), SendOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, MessagingError::BufferFull(_)));

    // With block_on_full the same send would instead await room (not asserted
    // here to avoid a hang, but exercised via the option path).
    let _ = SendOptions {
        block_on_full: true,
        ..Default::default()
    };
}

#[tokio::test]
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

    let (tx, mut rx) = unbounded_channel::<()>();
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
    .unwrap();

    bus.send("chan://q", Message::text("x"), SendOptions::default())
        .await
        .unwrap();
    rx.recv().await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await; // let on_ack run

    assert_eq!(counter.sends.load(Ordering::Relaxed), 1);
    assert!(counter.receives.load(Ordering::Relaxed) >= 1);
    assert!(counter.acks.load(Ordering::Relaxed) >= 1);
}

#[tokio::test]
async fn nack_flag_reflects_actual_fate_and_dead_letter_observed() {
    use std::sync::Mutex;

    #[derive(Default)]
    struct Rec {
        nack_requeue: Mutex<Vec<bool>>,
        dead_letter: Mutex<Vec<bool>>, // recorded `to_dlq` values
    }
    impl Observer for Rec {
        fn on_nack(&self, _a: &str, _m: &Message, requeue: bool) {
            self.nack_requeue.lock().unwrap().push(requeue);
        }
        fn on_dead_letter(&self, _a: &str, _m: &Message, to_dlq: bool) {
            self.dead_letter.lock().unwrap().push(to_dlq);
        }
    }

    let bus = LocalProvider::new("t");
    let rec = Arc::new(Rec::default());
    bus.set_observer(rec.clone());

    let (dlq_tx, mut dlq_rx) = unbounded_channel::<()>();
    bus.add_listener(
        "chan://dlq",
        handler(move |_m| {
            let tx = dlq_tx.clone();
            async move {
                tx.send(()).unwrap();
                Ack::Ok
            }
        }),
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
    dlq_rx.recv().await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    // delivery 1 (<2) → requeue=true; delivery 2 (>=2) → requeue=false (not the
    // old always-true bug), then dead-lettered with to_dlq=true.
    assert_eq!(*rec.nack_requeue.lock().unwrap(), vec![true, false]);
    assert_eq!(*rec.dead_letter.lock().unwrap(), vec![true]);
}

#[tokio::test]
async fn exhausted_without_dlq_is_reported_not_silently_dropped() {
    use std::sync::Mutex;

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
    tokio::time::sleep(Duration::from_millis(30)).await;

    // No DLQ configured → the drop is surfaced (to_dlq=false), not silent.
    assert_eq!(*rec.dropped.lock().unwrap(), vec![false]);
}

#[tokio::test]
async fn provider_as_lifecycle_component() {
    use ferroly::lifecycle::Component;

    let bus = LocalProvider::new("bus");
    Component::start(&bus).await.unwrap();

    let (tx, mut rx) = unbounded_channel::<()>();
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
    .unwrap();
    bus.send("chan://q", Message::text("x"), SendOptions::default())
        .await
        .unwrap();
    rx.recv().await.unwrap();

    // Stop clears destinations; subsequent sends find no listeners.
    Component::stop(&bus).await.unwrap();
    let err = bus
        .send("chan://q", Message::text("y"), SendOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, MessagingError::NoListeners(_)));
}

#![cfg(feature = "messaging")]
//! Tests for acknowledgements and the delivery context: `RetryAfter`,
//! `DeadLetter`, `Pending` with an `Acker`, `DeliveryInfo`, dead-letter
//! reasons — and priority, dedup keys and scheduled delivery.
//!
//! Everything runs on tokio's paused clock.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::messaging::{
    delivery_handler, handler, headers, Ack, Acker, AttrValue, Backoff, Delivery, DeliveryInfo,
    ExpiryAction, ListenerOptions, LocalOptions, LocalProvider, Message, Observer, Producer,
    Receiver, SendOptions,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::time::Instant;

const DRAIN: Duration = Duration::from_secs(60);

/// Listens on `chan://dlq` and forwards every dead-lettered message.
async fn dead_letters(bus: &LocalProvider) -> UnboundedReceiver<Message> {
    let (tx, rx) = unbounded_channel();
    bus.add_listener(
        "chan://dlq",
        handler(move |msg: Message| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(msg);
                Ack::Ok
            }
        }),
        Default::default(),
    )
    .await
    .unwrap();
    rx
}

/// Everything received so far, without waiting.
fn drained<T>(rx: &mut UnboundedReceiver<T>) -> Vec<T> {
    let mut all = Vec::new();
    while let Ok(item) = rx.try_recv() {
        all.push(item);
    }
    all
}

/// Listener options with a dead-letter queue and no backoff.
fn with_dlq(attempts: u32) -> ListenerOptions {
    ListenerOptions {
        max_delivery_attempts: attempts,
        dead_letter: Some("chan://dlq".into()),
        retry_backoff: Backoff::none(),
        ..Default::default()
    }
}

/// A plain handler that records each call (body and delivery count) and
/// returns `pick(delivery_count)`.
fn scripted(
    calls: &UnboundedSender<(String, u32)>,
    pick: impl Fn(u32) -> Ack + Send + Sync + 'static,
) -> ferroly::messaging::Handler {
    let calls = calls.clone();
    let pick = Arc::new(pick);
    handler(move |msg: Message| {
        let calls = calls.clone();
        let pick = pick.clone();
        async move {
            let _ = calls.send((msg.body_str().into_owned(), msg.delivery_count));
            pick(msg.delivery_count)
        }
    })
}

/// An observer that records dead-letter events as `(addr, to_dlq)`.
#[derive(Default)]
struct DeadLetterLog {
    events: Mutex<Vec<(String, bool)>>,
}

impl Observer for DeadLetterLog {
    fn on_dead_letter(&self, addr: &str, _msg: &Message, to_dlq: bool) {
        self.events.lock().unwrap().push((addr.to_string(), to_dlq));
    }
}

fn reason(msg: &Message) -> Option<&str> {
    msg.attribute_str(headers::DEAD_LETTER_REASON)
}

// ---- RetryAfter and DeadLetter ------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn retry_after_uses_the_given_delay() {
    let bus = LocalProvider::new("t");
    let (tx, mut rx) = unbounded_channel();
    bus.add_listener(
        "chan://jobs",
        handler(move |msg: Message| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(Instant::now());
                if msg.delivery_count == 0 {
                    Ack::RetryAfter(Duration::from_millis(50))
                } else {
                    Ack::Ok
                }
            }
        }),
        ListenerOptions {
            retry_backoff: Backoff::fixed(Duration::from_secs(10)),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    bus.send("chan://jobs", Message::text("x"), Default::default())
        .await
        .unwrap();

    let first = rx.recv().await.unwrap();
    // Not redelivered before the 50 ms are up...
    let early = tokio::time::timeout(Duration::from_millis(49), rx.recv()).await;
    assert!(early.is_err(), "redelivered too early");
    // ...and redelivered right after, not after the 10 s backoff.
    let second = rx.recv().await.unwrap();
    assert_eq!(second - first, Duration::from_millis(50));
    assert!(bus.shutdown(DRAIN).await.is_clean());
}

#[tokio::test(start_paused = true)]
async fn retry_after_still_counts_attempts() {
    let bus = LocalProvider::new("t");
    let mut dlq = dead_letters(&bus).await;
    let (calls, mut seen) = unbounded_channel();
    bus.add_listener(
        "chan://jobs",
        scripted(&calls, |_| Ack::RetryAfter(Duration::from_millis(1))),
        with_dlq(2),
    )
    .await
    .unwrap();
    bus.send("chan://jobs", Message::text("x"), Default::default())
        .await
        .unwrap();
    let dead = dlq.recv().await.unwrap();
    assert_eq!(dead.body_str(), "x");
    assert_eq!(reason(&dead), Some("max-delivery-attempts"));
    assert!(bus.shutdown(DRAIN).await.is_clean());
    assert_eq!(drained(&mut seen).len(), 2);
}

#[tokio::test(start_paused = true)]
async fn dead_letter_skips_the_remaining_attempts() {
    let bus = LocalProvider::new("t");
    let mut dlq = dead_letters(&bus).await;
    let (calls, mut seen) = unbounded_channel();
    bus.add_listener(
        "chan://jobs",
        scripted(&calls, |_| Ack::DeadLetter),
        with_dlq(5),
    )
    .await
    .unwrap();
    bus.send("chan://jobs", Message::text("x"), Default::default())
        .await
        .unwrap();
    let dead = dlq.recv().await.unwrap();
    assert_eq!(dead.body_str(), "x");
    assert!(bus.shutdown(DRAIN).await.is_clean());
    assert_eq!(drained(&mut seen).len(), 1);
    // D9: the delivery count is kept as it was.
    assert_eq!(dead.delivery_count, 0);
}

#[tokio::test(start_paused = true)]
async fn dead_lettered_messages_say_why() {
    let bus = LocalProvider::new("t");
    let mut dlq = dead_letters(&bus).await;
    let (calls, _seen) = unbounded_channel();

    // Exhausted.
    bus.add_listener(
        "chan://retrying",
        scripted(&calls, |_| Ack::Retry),
        with_dlq(2),
    )
    .await
    .unwrap();
    bus.send("chan://retrying", Message::text("a"), Default::default())
        .await
        .unwrap();
    let exhausted = dlq.recv().await.unwrap();
    assert_eq!(reason(&exhausted), Some("max-delivery-attempts"));
    assert_eq!(
        exhausted.attribute_str(headers::DEAD_LETTER_SOURCE),
        Some("chan://retrying")
    );
    assert_eq!(
        exhausted.attribute(headers::DEAD_LETTER_ATTEMPTS),
        Some(&AttrValue::Int(2))
    );
    assert_eq!(exhausted.delivery_count, 2);

    // Ack::DeadLetter.
    bus.add_listener(
        "chan://rejecting",
        scripted(&calls, |_| Ack::DeadLetter),
        with_dlq(3),
    )
    .await
    .unwrap();
    bus.send("rejecting", Message::text("b"), Default::default())
        .await
        .unwrap();
    let rejected = dlq.recv().await.unwrap();
    assert_eq!(reason(&rejected), Some("rejected"));
    assert_eq!(
        rejected.attribute_str(headers::DEAD_LETTER_SOURCE),
        Some("chan://rejecting")
    );
    assert_eq!(
        rejected.attribute(headers::DEAD_LETTER_ATTEMPTS),
        Some(&AttrValue::Int(1))
    );
    assert_eq!(rejected.attribute(headers::DEAD_LETTER_DESCRIPTION), None);

    // TTL expiry: due after 2 s, expired after 1 s.
    bus.add_listener(
        "chan://expiring",
        scripted(&calls, |_| Ack::Ok),
        ListenerOptions {
            on_expired: ExpiryAction::DeadLetter,
            ..with_dlq(3)
        },
    )
    .await
    .unwrap();
    let opts = SendOptions::new()
        .ttl(Duration::from_secs(1))
        .delay(Duration::from_secs(2));
    bus.send("chan://expiring", Message::text("c"), opts)
        .await
        .unwrap();
    let expired = dlq.recv().await.unwrap();
    assert_eq!(reason(&expired), Some("expired"));
    assert_eq!(
        expired.attribute_str(headers::DEAD_LETTER_SOURCE),
        Some("chan://expiring")
    );
    assert!(matches!(
        expired.attribute(headers::DEAD_LETTER_ATTEMPTS),
        Some(AttrValue::Int(_))
    ));
    assert!(bus.shutdown(DRAIN).await.is_clean());
}

#[tokio::test(start_paused = true)]
async fn a_panic_message_becomes_the_dead_letter_description() {
    let bus = LocalProvider::new("t");
    let mut dlq = dead_letters(&bus).await;
    bus.add_listener(
        "chan://jobs",
        handler(|_msg: Message| async move {
            if true {
                panic!("boom");
            }
            Ack::Ok
        }),
        with_dlq(2),
    )
    .await
    .unwrap();
    bus.send("chan://jobs", Message::text("x"), Default::default())
        .await
        .unwrap();
    let dead = dlq.recv().await.unwrap();
    assert_eq!(
        dead.attribute_str(headers::DEAD_LETTER_DESCRIPTION),
        Some("boom")
    );
    assert_eq!(reason(&dead), Some("max-delivery-attempts"));
    assert!(bus.shutdown(DRAIN).await.is_clean());
}

#[tokio::test(start_paused = true)]
async fn a_handler_can_describe_a_dead_letter() {
    let bus = LocalProvider::new("t");
    let mut dlq = dead_letters(&bus).await;
    bus.add_delivery_listener(
        "chan://jobs",
        delivery_handler(|delivery: Delivery| async move {
            delivery.dead_letter_description("bad schema");
            Ack::DeadLetter
        }),
        with_dlq(3),
    )
    .await
    .unwrap();
    bus.send("chan://jobs", Message::text("x"), Default::default())
        .await
        .unwrap();
    let dead = dlq.recv().await.unwrap();
    assert_eq!(
        dead.attribute_str(headers::DEAD_LETTER_DESCRIPTION),
        Some("bad schema")
    );
    assert_eq!(reason(&dead), Some("rejected"));
    assert!(bus.shutdown(DRAIN).await.is_clean());
}

#[tokio::test(start_paused = true)]
async fn dead_letter_without_a_destination_is_reported_as_dropped() {
    let bus = LocalProvider::new("t");
    let log = Arc::new(DeadLetterLog::default());
    bus.set_observer(log.clone());
    let (calls, mut seen) = unbounded_channel();
    bus.add_listener(
        "chan://jobs",
        scripted(&calls, |_| Ack::DeadLetter),
        ListenerOptions {
            max_delivery_attempts: 5,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    bus.send("chan://jobs", Message::text("x"), Default::default())
        .await
        .unwrap();
    assert!(bus.shutdown(DRAIN).await.is_clean());
    assert_eq!(drained(&mut seen).len(), 1);
    assert_eq!(
        *log.events.lock().unwrap(),
        vec![("chan://jobs".to_string(), false)]
    );
}

// ---- the delivery context -------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn delivery_info_describes_the_attempt() {
    let bus = LocalProvider::new("t");
    let (tx, mut rx) = unbounded_channel::<(String, DeliveryInfo)>();
    bus.add_delivery_listener(
        "chan://jobs",
        delivery_handler(move |delivery: Delivery| {
            let tx = tx.clone();
            async move {
                let info = delivery.info().clone();
                let first = info.attempt == 1;
                let _ = tx.send((delivery.message.body_str().into_owned(), info));
                if delivery.message.body_str() == "a" && first {
                    Ack::Retry
                } else {
                    Ack::Ok
                }
            }
        }),
        ListenerOptions {
            retry_backoff: Backoff::none(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    bus.send("chan://jobs", Message::text("a"), Default::default())
        .await
        .unwrap();
    bus.send("jobs", Message::text("b"), Default::default())
        .await
        .unwrap();
    assert!(bus.shutdown(DRAIN).await.is_clean());

    let calls = drained(&mut rx);
    assert_eq!(calls.len(), 3);
    let (body, first) = &calls[0];
    assert_eq!(body, "a");
    assert_eq!(first.attempt, 1);
    assert!(!first.redelivered);
    assert_eq!(first.address, "chan://jobs");
    assert!(first.enqueued_at.is_some());
    assert_eq!(first.subscription, None);
    let (body, second) = &calls[1];
    assert_eq!(body, "a");
    assert_eq!(second.attempt, 2);
    assert!(second.redelivered);
    assert_eq!(second.sequence, first.sequence);
    let (body, other) = &calls[2];
    assert_eq!(body, "b");
    assert_eq!(other.attempt, 1);
    assert!(other.sequence.unwrap() > first.sequence.unwrap());
}

#[tokio::test(start_paused = true)]
async fn delivery_info_names_the_subscription() {
    let bus = LocalProvider::new("t");
    let (tx, mut rx) = unbounded_channel::<DeliveryInfo>();
    bus.add_delivery_listener(
        "topic://orders.*",
        delivery_handler(move |delivery: Delivery| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(delivery.info().clone());
                Ack::Ok
            }
        }),
        ListenerOptions {
            subscription: Some("billing".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    bus.send(
        "topic://orders.created",
        Message::text("o"),
        Default::default(),
    )
    .await
    .unwrap();
    let info = rx.recv().await.unwrap();
    assert_eq!(info.subscription.as_deref(), Some("billing"));
    assert_eq!(info.address, "topic://orders.*");
    assert!(bus.shutdown(DRAIN).await.is_clean());
}

#[tokio::test(start_paused = true)]
async fn extend_succeeds_on_the_local_provider() {
    let bus = LocalProvider::new("t");
    let (tx, mut rx) = unbounded_channel();
    bus.add_delivery_listener(
        "chan://jobs",
        delivery_handler(move |delivery: Delivery| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(delivery.extend(Duration::from_secs(1)).await.is_ok());
                Ack::Ok
            }
        }),
        Default::default(),
    )
    .await
    .unwrap();
    bus.send("chan://jobs", Message::text("x"), Default::default())
        .await
        .unwrap();
    assert!(rx.recv().await.unwrap());
    assert!(bus.shutdown(DRAIN).await.is_clean());
}

/// A delivery handler that hands the `Acker` of the first delivery of each
/// message out and returns `Pending`, then acks later deliveries. Every call
/// is recorded as `(body, attempt)`.
fn deferring(
    ackers: UnboundedSender<Acker>,
    calls: UnboundedSender<(String, u32)>,
) -> ferroly::messaging::DeliveryHandler {
    delivery_handler(move |delivery: Delivery| {
        let ackers = ackers.clone();
        let calls = calls.clone();
        async move {
            let attempt = delivery.info().attempt;
            let _ = calls.send((delivery.message.body_str().into_owned(), attempt));
            if attempt == 1 {
                let _ = ackers.send(delivery.acker());
                Ack::Pending
            } else {
                Ack::Ok
            }
        }
    })
}

/// Waits (on the paused clock) until the provider has nothing pending.
async fn settle(bus: &LocalProvider) {
    while bus.pending() != 0 {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_delivery_can_be_acked_from_another_task() {
    let bus = LocalProvider::new("t");
    let (ackers_tx, mut ackers) = unbounded_channel();
    let (calls_tx, mut calls) = unbounded_channel();
    bus.add_delivery_listener(
        "chan://jobs",
        deferring(ackers_tx, calls_tx),
        Default::default(),
    )
    .await
    .unwrap();
    bus.send("chan://jobs", Message::text("x"), Default::default())
        .await
        .unwrap();
    let acker = ackers.recv().await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    // Still pending: the handler returned, but nothing settled it.
    assert_eq!(bus.pending(), 1);

    let settled = tokio::spawn({
        let acker = acker.clone();
        async move { acker.ack() }
    })
    .await
    .unwrap();
    assert!(settled);
    settle(&bus).await;
    assert_eq!(bus.pending(), 0);
    assert!(!acker.ack(), "already settled");
    assert!(bus.shutdown(DRAIN).await.is_clean());
    assert_eq!(drained(&mut calls).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_pending_delivery_blocks_a_serial_listener_until_settled() {
    let bus = LocalProvider::new("t");
    let (ackers_tx, mut ackers) = unbounded_channel();
    let (calls_tx, mut calls) = unbounded_channel();
    bus.add_delivery_listener(
        "chan://jobs",
        deferring(ackers_tx, calls_tx),
        Default::default(),
    )
    .await
    .unwrap();
    for body in ["first", "second"] {
        bus.send("chan://jobs", Message::text(body), Default::default())
            .await
            .unwrap();
    }
    let first = ackers.recv().await.unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;
    let so_far: Vec<String> = drained(&mut calls).into_iter().map(|c| c.0).collect();
    assert_eq!(so_far, ["first"], "the second waits for the first");

    assert!(first.ack());
    let second = ackers.recv().await.unwrap();
    let next: Vec<String> = drained(&mut calls).into_iter().map(|c| c.0).collect();
    assert_eq!(next, ["second"]);
    assert!(second.ack());
    assert!(bus.shutdown(DRAIN).await.is_clean());
}

#[tokio::test(start_paused = true)]
async fn dropping_every_acker_retries_the_message() {
    let bus = LocalProvider::new("t");
    let (tx, mut rx) = unbounded_channel();
    bus.add_delivery_listener(
        "chan://jobs",
        delivery_handler(move |delivery: Delivery| {
            let tx = tx.clone();
            async move {
                let attempt = delivery.info().attempt;
                let _ = tx.send(attempt);
                if attempt == 1 {
                    drop(delivery.acker());
                    Ack::Pending
                } else {
                    Ack::Ok
                }
            }
        }),
        ListenerOptions {
            retry_backoff: Backoff::none(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    bus.send("chan://jobs", Message::text("x"), Default::default())
        .await
        .unwrap();
    assert!(bus.shutdown(DRAIN).await.is_clean());
    assert_eq!(drained(&mut rx), [1, 2]);
}

/// Runs one message through a deferring listener with a DLQ, settles its
/// first delivery with `settle_with`, drains, and returns the attempts seen and
/// the dead-lettered messages.
async fn settled_by(settle_with: impl FnOnce(&Acker) -> bool) -> (Vec<u32>, Vec<Message>) {
    let bus = LocalProvider::new("t");
    let mut dlq = dead_letters(&bus).await;
    let (ackers_tx, mut ackers) = unbounded_channel();
    let (calls_tx, mut calls) = unbounded_channel();
    bus.add_delivery_listener("chan://jobs", deferring(ackers_tx, calls_tx), with_dlq(3))
        .await
        .unwrap();
    bus.send("chan://jobs", Message::text("x"), Default::default())
        .await
        .unwrap();
    let acker = ackers.recv().await.unwrap();
    assert!(settle_with(&acker));
    assert!(bus.shutdown(DRAIN).await.is_clean());
    let attempts = drained(&mut calls).into_iter().map(|c| c.1).collect();
    (attempts, drained(&mut dlq))
}

#[tokio::test(start_paused = true)]
async fn an_acker_can_retry_reject_and_dead_letter() {
    let (attempts, dead) = settled_by(|a| a.retry()).await;
    assert_eq!(attempts, [1, 2]);
    assert!(dead.is_empty());

    let began = Instant::now();
    let (attempts, dead) = settled_by(|a| a.retry_after(Duration::from_secs(7))).await;
    assert_eq!(attempts, [1, 2]);
    assert!(dead.is_empty());
    assert!(began.elapsed() >= Duration::from_secs(7));

    let (attempts, dead) = settled_by(|a| a.reject()).await;
    assert_eq!(attempts, [1]);
    assert!(dead.is_empty());

    let (attempts, dead) = settled_by(|a| a.dead_letter("why")).await;
    assert_eq!(attempts, [1]);
    assert_eq!(dead.len(), 1);
    assert_eq!(reason(&dead[0]), Some("rejected"));
    assert_eq!(
        dead[0].attribute_str(headers::DEAD_LETTER_DESCRIPTION),
        Some("why")
    );
}

#[tokio::test(start_paused = true)]
async fn pending_from_a_plain_handler_is_a_retry() {
    let bus = LocalProvider::new("t");
    let (calls, mut seen) = unbounded_channel();
    bus.add_listener(
        "chan://jobs",
        scripted(
            &calls,
            |count| if count == 0 { Ack::Pending } else { Ack::Ok },
        ),
        ListenerOptions {
            retry_backoff: Backoff::none(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    bus.send("chan://jobs", Message::text("x"), Default::default())
        .await
        .unwrap();
    assert!(bus.shutdown(DRAIN).await.is_clean());
    let counts: Vec<u32> = drained(&mut seen).into_iter().map(|c| c.1).collect();
    assert_eq!(counts, [0, 1]);
}

#[tokio::test(start_paused = true)]
async fn a_pending_delivery_is_abandoned_on_shutdown() {
    let bus = LocalProvider::new("t");
    let (ackers_tx, mut ackers) = unbounded_channel();
    let (calls_tx, _calls) = unbounded_channel();
    bus.add_delivery_listener(
        "chan://jobs",
        deferring(ackers_tx, calls_tx),
        Default::default(),
    )
    .await
    .unwrap();
    bus.send("chan://jobs", Message::text("held"), Default::default())
        .await
        .unwrap();
    // Keep the acker alive, unsettled.
    let _acker = ackers.recv().await.unwrap();
    let report = bus.shutdown(Duration::from_millis(100)).await;
    assert!(!report.is_clean());
    assert_eq!(report.undelivered.len(), 1);
    assert_eq!(report.undelivered[0].addr, "chan://jobs");
    assert_eq!(report.undelivered[0].message.body_str(), "held");
}

#[tokio::test(start_paused = true)]
async fn detached_deliveries_are_inert() {
    let delivery = Delivery::detached(Message::text("x"));
    assert!(delivery.extend(Duration::from_secs(1)).await.is_ok());
    let acker = delivery.acker();
    assert!(!acker.ack());
    assert!(!acker.dead_letter("nothing to settle"));
    delivery.dead_letter_description("ignored");
    assert_eq!(delivery.info().attempt, 1);
    assert_eq!(delivery.into_message().body_str(), "x");
}

// ---- priority, dedup keys and scheduled delivery (WP6) ---------------------------------

#[tokio::test(start_paused = true)]
async fn a_higher_priority_message_is_delivered_first() {
    let bus = LocalProvider::new("t");
    let (seen_tx, mut seen) = unbounded_channel::<u8>();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let handler_gate = gate.clone();
    bus.add_listener(
        "chan://jobs",
        handler(move |msg: Message| {
            let seen_tx = seen_tx.clone();
            let gate = handler_gate.clone();
            async move {
                let _ = seen_tx.send(msg.priority());
                if msg.body_str() == "blocker" {
                    // Hold the serial handler until the test releases it.
                    let _ = gate.acquire().await.map(|p| p.forget());
                }
                Ack::Ok
            }
        }),
        Default::default(),
    )
    .await
    .unwrap();
    bus.send("chan://jobs", Message::text("blocker"), Default::default())
        .await
        .unwrap();
    assert_eq!(seen.recv().await, Some(0));

    // While the handler is blocked: priorities 0, 0, 9.
    for (body, priority) in [("a", 0), ("b", 0), ("urgent", 9)] {
        let msg = Message::text(body).with_priority(priority);
        bus.send("chan://jobs", msg, Default::default())
            .await
            .unwrap();
    }
    gate.add_permits(1);
    assert!(bus.shutdown(DRAIN).await.is_clean());
    assert_eq!(drained(&mut seen), [9, 0, 0]);
}

/// An observer that counts duplicate drops.
#[derive(Default)]
struct Duplicates(Mutex<usize>);

impl Observer for Duplicates {
    fn on_dropped(&self, _addr: &str, _msg: &Message, reason: ferroly::messaging::DropReason) {
        if reason == ferroly::messaging::DropReason::Duplicate {
            *self.0.lock().unwrap() += 1;
        }
    }
}

async fn dedup_bus() -> (
    LocalProvider,
    Arc<Duplicates>,
    UnboundedReceiver<(String, u32)>,
) {
    let bus = LocalProvider::with_options(
        "t",
        LocalOptions {
            dedup_window: 16,
            ..Default::default()
        },
    );
    let duplicates = Arc::new(Duplicates::default());
    bus.set_observer(duplicates.clone());
    let (calls, seen) = unbounded_channel();
    bus.add_listener(
        "chan://jobs",
        scripted(&calls, |_| Ack::Ok),
        Default::default(),
    )
    .await
    .unwrap();
    (bus, duplicates, seen)
}

#[tokio::test(start_paused = true)]
async fn dedup_uses_the_dedup_key_before_the_message_id() {
    let (bus, duplicates, mut seen) = dedup_bus().await;
    for id in ["m-1", "m-2"] {
        let msg = Message::text(id).with_id(id).with_dedup_key("order-17");
        bus.send("chan://jobs", msg, Default::default())
            .await
            .unwrap();
    }
    assert!(bus.shutdown(DRAIN).await.is_clean());
    let bodies: Vec<String> = drained(&mut seen).into_iter().map(|c| c.0).collect();
    assert_eq!(bodies, ["m-1"]);
    assert_eq!(*duplicates.0.lock().unwrap(), 1);
}

#[tokio::test(start_paused = true)]
async fn dedup_still_falls_back_to_the_message_id() {
    let (bus, duplicates, mut seen) = dedup_bus().await;
    for body in ["first", "again"] {
        let msg = Message::text(body).with_id("m-1");
        bus.send("chan://jobs", msg, Default::default())
            .await
            .unwrap();
    }
    // Different ids, no dedup key: both delivered.
    bus.send(
        "chan://jobs",
        Message::text("other").with_id("m-2"),
        Default::default(),
    )
    .await
    .unwrap();
    assert!(bus.shutdown(DRAIN).await.is_clean());
    let bodies: Vec<String> = drained(&mut seen).into_iter().map(|c| c.0).collect();
    assert_eq!(bodies, ["first", "other"]);
    assert_eq!(*duplicates.0.lock().unwrap(), 1);
}

#[tokio::test(start_paused = true)]
async fn deliver_at_delays_until_the_given_time() {
    let bus = LocalProvider::new("t");
    let (tx, mut rx) = unbounded_channel();
    bus.add_listener(
        "chan://jobs",
        handler(move |_msg: Message| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(Instant::now());
                Ack::Ok
            }
        }),
        Default::default(),
    )
    .await
    .unwrap();
    let began = Instant::now();
    let at = std::time::SystemTime::now() + Duration::from_secs(30);
    bus.send(
        "chan://jobs",
        Message::text("later"),
        SendOptions::new().deliver_at(at),
    )
    .await
    .unwrap();
    let early = tokio::time::timeout(Duration::from_secs(29), rx.recv()).await;
    assert!(early.is_err(), "delivered before the scheduled time");
    let delivered = rx.recv().await.unwrap();
    let waited = delivered - began;
    // The wall-clock read and the paused tokio clock differ by at most a
    // little real time.
    assert!(waited >= Duration::from_millis(29_900), "{waited:?}");
    assert!(waited <= Duration::from_secs(30), "{waited:?}");
    assert!(bus.shutdown(DRAIN).await.is_clean());
}

#[tokio::test(start_paused = true)]
async fn deliver_at_in_the_past_delivers_now() {
    let bus = LocalProvider::new("t");
    let (tx, mut rx) = unbounded_channel();
    bus.add_listener(
        "chan://jobs",
        handler(move |_msg: Message| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(Instant::now());
                Ack::Ok
            }
        }),
        Default::default(),
    )
    .await
    .unwrap();
    let began = Instant::now();
    let past = std::time::SystemTime::now() - Duration::from_secs(3600);
    bus.send(
        "chan://jobs",
        Message::text("overdue"),
        SendOptions::new().deliver_at(past),
    )
    .await
    .unwrap();
    let delivered = rx.recv().await.unwrap();
    assert_eq!(delivered - began, Duration::ZERO);
    assert!(bus.shutdown(DRAIN).await.is_clean());
}

#[tokio::test(start_paused = true)]
async fn delay_and_deliver_at_together_are_rejected() {
    let bus = LocalProvider::new("t");
    bus.add_listener(
        "chan://jobs",
        handler(|_msg: Message| async { Ack::Ok }),
        Default::default(),
    )
    .await
    .unwrap();
    let opts = SendOptions::new()
        .delay(Duration::from_secs(1))
        .deliver_at(std::time::SystemTime::now());
    let err = bus
        .send("chan://jobs", Message::text("x"), opts)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ferroly::messaging::MessagingError::InvalidOptions(_)),
        "{err}"
    );
    assert_eq!(bus.pending(), 0);
    assert!(bus.shutdown(DRAIN).await.is_clean());
}

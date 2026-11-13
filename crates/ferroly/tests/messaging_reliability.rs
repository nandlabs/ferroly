#![cfg(feature = "messaging")]
//! Regression tests for the messaging reliability fixes: handler panics,
//! draining shutdown, consumer-task lifetime, option validation, retry backoff
//! and ordering.
//!
//! Everything runs on tokio's paused clock, so sleeps, backoffs and timeouts
//! are virtual and elapsed times can be asserted exactly.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::lifecycle::Component;
use ferroly::messaging::{
    handler, Ack, Backoff, DropReason, Handler, ListenerOptions, LocalOptions, LocalProvider,
    Message, MessagingError, Observer, OnFull, Producer, Receiver, SendOptions,
};
use tokio::time::Instant;

const DRAIN: Duration = Duration::from_secs(60);

type Log = Arc<Mutex<Vec<String>>>;

fn new_log() -> Log {
    Arc::new(Mutex::new(Vec::new()))
}

fn entries(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

/// A handler that records each body it acks.
fn acking(log: &Log) -> Handler {
    let log = log.clone();
    handler(move |msg: Message| {
        let log = log.clone();
        async move {
            log.lock().unwrap().push(msg.body_str().into_owned());
            Ack::Ok
        }
    })
}

async fn send(bus: &LocalProvider, addr: &str, body: &str) {
    bus.send(addr, Message::text(body), SendOptions::default())
        .await
        .unwrap();
}

/// Records what the provider reports about handler panics and lost messages.
#[derive(Default)]
struct Watcher {
    panics: Mutex<Vec<String>>,
    dropped: Mutex<Vec<(String, String, DropReason)>>,
    dead_letters: Mutex<Vec<(String, String, bool)>>,
}

impl Observer for Watcher {
    fn on_handler_panic(&self, _addr: &str, msg: &Message, panic: &str) {
        self.panics
            .lock()
            .unwrap()
            .push(format!("{}: {panic}", msg.body_str()));
    }
    fn on_dropped(&self, addr: &str, msg: &Message, reason: DropReason) {
        self.dropped
            .lock()
            .unwrap()
            .push((addr.to_string(), msg.body_str().into_owned(), reason));
    }
    fn on_dead_letter(&self, addr: &str, msg: &Message, to_dlq: bool) {
        self.dead_letters.lock().unwrap().push((
            addr.to_string(),
            msg.body_str().into_owned(),
            to_dlq,
        ));
    }
}

// ---- P0-9a: a handler panic neither kills the consumer nor loses messages --

#[tokio::test(start_paused = true)]
async fn a_panicking_serial_handler_keeps_the_consumer_and_the_messages() {
    let bus = LocalProvider::new("t");
    let watcher = Arc::new(Watcher::default());
    bus.set_observer(watcher.clone());
    let log = new_log();
    let acked = log.clone();
    bus.add_listener(
        "chan://q",
        handler(move |msg: Message| {
            let acked = acked.clone();
            async move {
                // Blow up on the first delivery of "boom" only.
                if msg.body_str() == "boom" && msg.delivery_count == 0 {
                    panic!("handler bug");
                }
                acked
                    .lock()
                    .unwrap()
                    .push(format!("{}#{}", msg.body_str(), msg.delivery_count));
                Ack::Ok
            }
        }),
        ListenerOptions::default(),
    )
    .await
    .unwrap();

    for body in ["a", "boom", "c"] {
        send(&bus, "chan://q", body).await;
    }
    // The consumer used to die on the panic: buffered messages were dropped and
    // this send returned NoListeners.
    send(&bus, "chan://q", "d").await;

    assert!(bus.shutdown(DRAIN).await.is_clean());
    // The panic counted as a Retry: "boom" was redelivered (delivery 1) and
    // nothing after it was lost.
    assert_eq!(entries(&log), ["a#0", "boom#1", "c#0", "d#0"]);
    assert_eq!(
        *watcher.panics.lock().unwrap(),
        vec!["boom: handler bug".to_string()]
    );
}

#[tokio::test(start_paused = true)]
async fn a_handler_that_always_panics_is_dead_lettered_after_max_attempts() {
    let bus = LocalProvider::new("t");
    let watcher = Arc::new(Watcher::default());
    bus.set_observer(watcher.clone());
    let dlq = new_log();
    bus.add_listener("chan://dlq", acking(&dlq), ListenerOptions::default())
        .await
        .unwrap();

    let attempts = Arc::new(AtomicU32::new(0));
    let log = new_log();
    let (count, acked) = (attempts.clone(), log.clone());
    bus.add_listener(
        "chan://q",
        handler(move |msg: Message| {
            let (count, acked) = (count.clone(), acked.clone());
            async move {
                if msg.body_str() == "poison" {
                    count.fetch_add(1, Ordering::SeqCst);
                    panic!("cannot digest");
                }
                acked.lock().unwrap().push(msg.body_str().into_owned());
                Ack::Ok
            }
        }),
        ListenerOptions {
            max_delivery_attempts: 3,
            dead_letter: Some("chan://dlq".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    send(&bus, "chan://q", "poison").await;
    send(&bus, "chan://q", "fine").await;
    assert!(bus.shutdown(DRAIN).await.is_clean());

    assert_eq!(attempts.load(Ordering::SeqCst), 3, "max attempts respected");
    assert_eq!(entries(&dlq), ["poison"]);
    assert_eq!(entries(&log), ["fine"], "the consumer kept running");
    assert_eq!(watcher.panics.lock().unwrap().len(), 3);
    assert_eq!(
        *watcher.dead_letters.lock().unwrap(),
        vec![("chan://dlq".to_string(), "poison".to_string(), true)]
    );
}

#[tokio::test(start_paused = true)]
async fn a_panicking_concurrent_handler_is_retried_too() {
    let bus = LocalProvider::new("t");
    let log = new_log();
    let acked = log.clone();
    bus.add_listener(
        "chan://q",
        handler(move |msg: Message| {
            let acked = acked.clone();
            async move {
                if msg.delivery_count == 0 && msg.body_str().starts_with("bad") {
                    panic!("first attempt fails");
                }
                acked.lock().unwrap().push(msg.body_str().into_owned());
                Ack::Ok
            }
        }),
        ListenerOptions {
            concurrency: Some(4),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let bodies = ["bad-1", "ok-1", "bad-2", "ok-2", "bad-3", "ok-3"];
    for body in bodies {
        send(&bus, "chan://q", body).await;
    }
    assert!(bus.shutdown(DRAIN).await.is_clean());

    let mut handled = entries(&log);
    handled.sort();
    let mut expected: Vec<String> = bodies.iter().map(|b| b.to_string()).collect();
    expected.sort();
    assert_eq!(handled, expected, "every message was eventually acked");
}

// ---- P0-9b: stop() drains ---------------------------------------------------

#[tokio::test(start_paused = true)]
async fn stop_drains_every_buffered_message_before_returning() {
    let bus = LocalProvider::new("t");
    let log = new_log();
    let acked = log.clone();
    bus.add_listener(
        "chan://q",
        handler(move |msg: Message| {
            let acked = acked.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                acked.lock().unwrap().push(msg.body_str().into_owned());
                Ack::Ok
            }
        }),
        ListenerOptions::default(),
    )
    .await
    .unwrap();

    for i in 0..20 {
        send(&bus, "chan://q", &i.to_string()).await;
    }
    // Used to clear the destinations and return at once: 0 of 20 acked.
    let began = Instant::now();
    Component::stop(&bus).await.unwrap();

    let expected: Vec<String> = (0..20).map(|i| i.to_string()).collect();
    assert_eq!(entries(&log), expected, "all handled, in order");
    assert_eq!(began.elapsed(), Duration::from_millis(200));
    assert_eq!(bus.pending(), 0);
}

#[tokio::test(start_paused = true)]
async fn stop_completes_retries_that_are_in_progress() {
    let bus = LocalProvider::new("t");
    let log = new_log();
    let acked = log.clone();
    bus.add_listener(
        "chan://q",
        handler(move |msg: Message| {
            let acked = acked.clone();
            async move {
                // Every message needs two deliveries.
                if msg.delivery_count == 0 {
                    return Ack::Retry;
                }
                acked.lock().unwrap().push(msg.body_str().into_owned());
                Ack::Ok
            }
        }),
        ListenerOptions::default(),
    )
    .await
    .unwrap();

    for i in 0..20 {
        send(&bus, "chan://q", &i.to_string()).await;
    }
    // Used to drop the Retry requeues during the drain.
    assert!(bus.shutdown(DRAIN).await.is_clean());
    assert_eq!(entries(&log).len(), 20);
}

#[tokio::test(start_paused = true)]
async fn retries_exhausted_during_the_drain_reach_the_dead_letter_queue() {
    let bus = LocalProvider::new("t");
    let dlq = new_log();
    bus.add_listener("chan://dlq", acking(&dlq), ListenerOptions::default())
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

    for i in 0..5 {
        send(&bus, "chan://q", &i.to_string()).await;
    }
    // The dead-letter sends happen while the provider is already draining;
    // they must still be accepted and handled.
    assert!(bus.shutdown(DRAIN).await.is_clean());
    assert_eq!(entries(&dlq), ["0", "1", "2", "3", "4"]);
}

#[tokio::test(start_paused = true)]
async fn stop_reports_what_it_could_not_deliver_within_the_timeout() {
    let bus = LocalProvider::with_options(
        "t",
        LocalOptions {
            drain_timeout: Duration::from_secs(5),
            ..Default::default()
        },
    );
    let watcher = Arc::new(Watcher::default());
    bus.set_observer(watcher.clone());
    // A handler that never finishes: one message in flight, two buffered.
    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    bus.add_listener(
        "chan://q",
        handler(move |_m| {
            let started = started_tx.clone();
            async move {
                started.send(()).unwrap();
                std::future::pending::<()>().await;
                Ack::Ok
            }
        }),
        ListenerOptions::default(),
    )
    .await
    .unwrap();
    for body in ["a", "b", "c"] {
        send(&bus, "chan://q", body).await;
    }
    started_rx.recv().await.unwrap();

    let began = Instant::now();
    let err = Component::stop(&bus).await.unwrap_err();
    // Bounded by the drain timeout, not hung on the stuck handler.
    assert_eq!(began.elapsed(), Duration::from_secs(5));
    assert!(
        err.to_string()
            .contains("3 message(s) could not be delivered"),
        "{err}"
    );

    // Nothing was dropped silently: each message was reported.
    let mut dropped = watcher.dropped.lock().unwrap().clone();
    dropped.sort_by(|a, b| a.1.cmp(&b.1));
    let shutdown = |body: &str| {
        (
            "chan://q".to_string(),
            body.to_string(),
            DropReason::Shutdown,
        )
    };
    assert_eq!(dropped, vec![shutdown("a"), shutdown("b"), shutdown("c")]);
    assert_eq!(bus.pending(), 0);
}

#[tokio::test(start_paused = true)]
async fn shutdown_returns_the_undelivered_messages_to_the_caller() {
    let bus = LocalProvider::new("t");
    bus.add_listener(
        "chan://q",
        handler(|_m| async {
            std::future::pending::<()>().await;
            Ack::Ok
        }),
        ListenerOptions::default(),
    )
    .await
    .unwrap();
    for body in ["a", "b"] {
        send(&bus, "chan://q", body).await;
    }
    // Scheduled for after the deadline: not worth waiting for.
    bus.send(
        "chan://q",
        Message::text("later"),
        SendOptions::new().delay(Duration::from_secs(3600)),
    )
    .await
    .unwrap();

    let report = bus.shutdown(Duration::from_secs(1)).await;
    assert!(!report.is_clean());
    let mut bodies: Vec<String> = report
        .undelivered
        .iter()
        .map(|u| {
            assert_eq!(u.addr, "chan://q");
            u.message.body_str().into_owned()
        })
        .collect();
    bodies.sort();
    assert_eq!(bodies, ["a", "b", "later"]);

    // A second shutdown has nothing left to report.
    assert!(bus.shutdown(Duration::from_secs(1)).await.is_clean());
}

#[tokio::test(start_paused = true)]
async fn sends_are_refused_while_draining() {
    let bus = LocalProvider::new("t");
    let log = new_log();
    let acked = log.clone();
    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    bus.add_listener(
        "chan://q",
        handler(move |msg: Message| {
            let (acked, started) = (acked.clone(), started_tx.clone());
            async move {
                started.send(()).unwrap();
                tokio::time::sleep(Duration::from_secs(10)).await;
                acked.lock().unwrap().push(msg.body_str().into_owned());
                Ack::Ok
            }
        }),
        ListenerOptions::default(),
    )
    .await
    .unwrap();
    send(&bus, "chan://q", "in-flight").await;
    started_rx.recv().await.unwrap();

    let stopping = tokio::spawn({
        let bus = bus.clone();
        async move { bus.shutdown(DRAIN).await }
    });
    // Let the shutdown begin; the handler still has ~9 s to go.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!stopping.is_finished());

    let err = bus
        .send(
            "chan://q",
            Message::text("too late"),
            SendOptions::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, MessagingError::Closed(_)), "{err}");
    assert_eq!(err.into_message().unwrap().body_str(), "too late");

    // The in-flight message is still completed.
    assert!(stopping.await.unwrap().is_clean());
    assert_eq!(entries(&log), ["in-flight"]);
}

#[tokio::test(start_paused = true)]
async fn a_request_being_handled_is_still_answered_while_draining() {
    let bus = LocalProvider::new("t");
    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let slow = bus.responder(move |request: Message| {
        let started = started_tx.clone();
        async move {
            started.send(()).unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(Message::text(format!("done: {}", request.body_str())))
        }
    });
    bus.add_listener("chan://work", slow, ListenerOptions::default())
        .await
        .unwrap();

    let requester = tokio::spawn({
        let bus = bus.clone();
        async move {
            bus.request("chan://work", Message::text("job"), Duration::from_secs(30))
                .await
        }
    });
    started_rx.recv().await.unwrap();

    // The shutdown starts while the request is in the handler. Replies are
    // not "new sends": the requester still gets its answer.
    assert!(bus.shutdown(DRAIN).await.is_clean());
    let reply = requester.await.unwrap().unwrap();
    assert_eq!(reply.body_str(), "done: job");
}

#[tokio::test(start_paused = true)]
async fn a_removed_listener_stuck_in_a_handler_does_not_hang_shutdown() {
    let bus = LocalProvider::new("t");
    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    bus.add_listener(
        "chan://q",
        handler(move |_m| {
            let started = started_tx.clone();
            async move {
                started.send(()).unwrap();
                std::future::pending::<()>().await;
                Ack::Ok
            }
        }),
        ListenerOptions::default(),
    )
    .await
    .unwrap();
    send(&bus, "chan://q", "in-flight").await;
    send(&bus, "chan://q", "backlog").await;
    started_rx.recv().await.unwrap();

    // The listener is detached from routing but still owns its backlog.
    assert_eq!(bus.remove_listeners("chan://q").await.unwrap(), 1);

    let began = Instant::now();
    let report = bus.shutdown(Duration::from_secs(3)).await;
    assert_eq!(began.elapsed(), Duration::from_secs(3));
    let mut bodies: Vec<String> = report
        .undelivered
        .iter()
        .map(|u| u.message.body_str().into_owned())
        .collect();
    bodies.sort();
    assert_eq!(bodies, ["backlog", "in-flight"]);
}

#[tokio::test(start_paused = true)]
async fn stop_awaits_the_consumer_tasks() {
    let bus = LocalProvider::new("t");
    // The handler owns a clone of `token`; it is released only when the
    // consumer task that holds the handler has ended.
    let token = Arc::new(());
    let held = token.clone();
    bus.add_listener(
        "chan://q",
        handler(move |_m| {
            let _held = held.clone();
            async { Ack::Ok }
        }),
        ListenerOptions {
            concurrency: Some(2),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    send(&bus, "chan://q", "x").await;
    assert_eq!(Arc::strong_count(&token), 2);

    assert!(bus.shutdown(DRAIN).await.is_clean());
    // No yielding in between: the task was joined by `shutdown` itself.
    assert_eq!(Arc::strong_count(&token), 1);
}

// ---- I13: consumer tasks do not outlive the provider ------------------------

#[tokio::test(start_paused = true)]
async fn dropping_every_handle_ends_the_consumer_tasks() {
    let watcher = Arc::new(Watcher::default());
    let token = Arc::new(());
    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    {
        let bus = LocalProvider::new("t");
        bus.set_observer(watcher.clone());
        // An idle listener and one stuck in a handler with a backlog.
        let idle = token.clone();
        bus.add_listener(
            "chan://idle",
            handler(move |_m| {
                let _held = idle.clone();
                async { Ack::Ok }
            }),
            ListenerOptions::default(),
        )
        .await
        .unwrap();
        let busy = token.clone();
        bus.add_listener(
            "chan://busy",
            handler(move |_m| {
                let (_held, started) = (busy.clone(), started_tx.clone());
                async move {
                    started.send(()).unwrap();
                    std::future::pending::<()>().await;
                    Ack::Ok
                }
            }),
            ListenerOptions::default(),
        )
        .await
        .unwrap();
        send(&bus, "chan://busy", "in-flight").await;
        send(&bus, "chan://busy", "buffered").await;
        started_rx.recv().await.unwrap();
        assert_eq!(Arc::strong_count(&token), 3);
        // `bus` — the only handle — is dropped here, without a stop().
    }

    // Give the consumer tasks a chance to observe the drop and finish.
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(
        Arc::strong_count(&token),
        1,
        "the consumer tasks (and their handlers) must be gone"
    );
    // The abandoned messages were reported rather than dropped silently.
    let mut dropped = watcher.dropped.lock().unwrap().clone();
    dropped.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(
        dropped,
        vec![
            (
                "chan://busy".to_string(),
                "buffered".to_string(),
                DropReason::Shutdown
            ),
            (
                "chan://busy".to_string(),
                "in-flight".to_string(),
                DropReason::Shutdown
            ),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_zero_buffer_size_is_an_error_not_a_panic() {
    let bus = LocalProvider::new("t");
    let err = bus
        .add_listener(
            "chan://q",
            handler(|_m| async { Ack::Ok }),
            ListenerOptions {
                buffer_size: 0,
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, MessagingError::InvalidOptions(_)), "{err}");
    assert!(err.to_string().contains("buffer_size"), "{err}");

    // Nothing was registered, and the provider is still usable.
    assert!(matches!(
        bus.send("chan://q", Message::text("x"), SendOptions::default())
            .await,
        Err(MessagingError::NoListeners(_))
    ));

    // Other invalid options are rejected the same way.
    let self_dlq = bus
        .add_listener(
            "chan://q",
            handler(|_m| async { Ack::Ok }),
            ListenerOptions {
                dead_letter: Some("q".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(self_dlq, MessagingError::InvalidOptions(_)),
        "{self_dlq}"
    );
    // `concurrency: Some(0)` is clamped to one worker rather than rejected.
    let log = new_log();
    bus.add_listener(
        "chan://q",
        acking(&log),
        ListenerOptions {
            concurrency: Some(0),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    send(&bus, "chan://q", "x").await;
    assert!(bus.shutdown(DRAIN).await.is_clean());
    assert_eq!(entries(&log), ["x"]);
}

// ---- retry backoff and ordering ---------------------------------------------

#[tokio::test(start_paused = true)]
async fn redeliveries_wait_for_the_backoff() {
    let bus = LocalProvider::new("t");
    let began = Instant::now();
    let times = Arc::new(Mutex::new(Vec::new()));
    let seen = times.clone();
    bus.add_listener(
        "chan://q",
        handler(move |msg: Message| {
            seen.lock().unwrap().push(began.elapsed());
            async move {
                if msg.delivery_count < 3 {
                    Ack::Retry
                } else {
                    Ack::Ok
                }
            }
        }),
        ListenerOptions {
            max_delivery_attempts: 10,
            retry_backoff: Backoff::exponential(
                Duration::from_millis(100),
                Duration::from_millis(350),
            ),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    send(&bus, "chan://q", "x").await;
    assert!(bus.shutdown(DRAIN).await.is_clean());

    // Delays of 100 ms, 200 ms, then 350 ms (400 ms capped): no hot loop.
    let ms = Duration::from_millis;
    assert_eq!(
        *times.lock().unwrap(),
        vec![ms(0), ms(100), ms(300), ms(650)]
    );
}

#[tokio::test(start_paused = true)]
async fn a_zero_backoff_redelivers_without_delay() {
    let bus = LocalProvider::new("t");
    let attempts = Arc::new(AtomicU32::new(0));
    let count = attempts.clone();
    bus.add_listener(
        "chan://q",
        handler(move |msg: Message| {
            count.fetch_add(1, Ordering::SeqCst);
            async move {
                if msg.delivery_count < 4 {
                    Ack::Retry
                } else {
                    Ack::Ok
                }
            }
        }),
        ListenerOptions {
            max_delivery_attempts: 10,
            retry_backoff: Backoff::none(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let began = Instant::now();
    send(&bus, "chan://q", "x").await;
    assert!(bus.shutdown(DRAIN).await.is_clean());
    assert_eq!(attempts.load(Ordering::SeqCst), 5);
    assert_eq!(began.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn a_retried_message_keeps_its_place_in_a_serial_listener() {
    let bus = LocalProvider::new("t");
    let log = new_log();
    let seen = log.clone();
    bus.add_listener(
        "chan://q",
        handler(move |msg: Message| {
            seen.lock()
                .unwrap()
                .push(format!("{}#{}", msg.body_str(), msg.delivery_count));
            async move {
                // The first message needs three deliveries.
                if msg.body_str() == "1" && msg.delivery_count < 2 {
                    Ack::Retry
                } else {
                    Ack::Ok
                }
            }
        }),
        ListenerOptions::default(),
    )
    .await
    .unwrap();

    for body in ["1", "2", "3"] {
        send(&bus, "chan://q", body).await;
    }
    assert!(bus.shutdown(DRAIN).await.is_clean());
    // "2" and "3" used to overtake the retried "1" (it was requeued at the
    // tail). Now a message is redelivered before anything behind it.
    assert_eq!(entries(&log), ["1#0", "1#1", "1#2", "2#0", "3#0"]);
}

#[tokio::test(start_paused = true)]
async fn per_key_order_holds_across_competing_listeners_despite_retries() {
    let bus = LocalProvider::new("t");
    // Two serial listeners compete for one queue; each records what it acks.
    let logs = [new_log(), new_log()];
    for log in &logs {
        let acked = log.clone();
        bus.add_listener(
            "chan://orders",
            handler(move |msg: Message| {
                let acked = acked.clone();
                async move {
                    // The first message of every key fails once.
                    if msg.body_str().ends_with("-0") && msg.delivery_count == 0 {
                        return Ack::Retry;
                    }
                    acked.lock().unwrap().push(msg.body_str().into_owned());
                    Ack::Ok
                }
            }),
            ListenerOptions::default(),
        )
        .await
        .unwrap();
    }

    let keys = ["alice", "bob", "carol", "dave"];
    for seq in 0..5 {
        for key in keys {
            bus.send(
                "chan://orders",
                Message::text(format!("{key}-{seq}")).with_routing_key(key),
                SendOptions::default(),
            )
            .await
            .unwrap();
        }
    }
    assert!(bus.shutdown(DRAIN).await.is_clean());

    let acked: Vec<Vec<String>> = logs.iter().map(entries).collect();
    assert_eq!(acked[0].len() + acked[1].len(), 20);
    for key in keys {
        let expected: Vec<String> = (0..5).map(|seq| format!("{key}-{seq}")).collect();
        // Affinity: all of a key's messages went to exactly one listener...
        let owners: Vec<&Vec<String>> = acked
            .iter()
            .filter(|log| log.iter().any(|body| body.starts_with(key)))
            .collect();
        assert_eq!(owners.len(), 1, "key {key} must stick to one listener");
        // ...which handled them in send order, the retry notwithstanding.
        let in_order: Vec<String> = owners[0]
            .iter()
            .filter(|body| body.starts_with(key))
            .cloned()
            .collect();
        assert_eq!(in_order, expected);
    }
}

// ---- multi-threaded runtime ---------------------------------------------------

/// The same guarantees on a real multi-threaded runtime (real clock, no
/// sleeps): the draining shutdown is the only synchronisation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nothing_is_lost_under_concurrent_load_with_panics_and_retries() {
    const PRODUCERS: usize = 8;
    const PER_PRODUCER: usize = 250;

    let bus = LocalProvider::new("t");
    let dlq = new_log();
    bus.add_listener("chan://dlq", acking(&dlq), ListenerOptions::default())
        .await
        .unwrap();

    let acked = new_log();
    for concurrency in [None, Some(4), None, Some(2)] {
        let log = acked.clone();
        bus.add_listener(
            "chan://work",
            handler(move |msg: Message| {
                let log = log.clone();
                async move {
                    let n: usize = msg.body_str().parse().unwrap();
                    match (n % 10, msg.delivery_count) {
                        (3, 0) => panic!("first delivery of {n} panics"),
                        (5, 0) => Ack::Retry,
                        (7, _) => Ack::Retry, // poison: ends in the DLQ
                        _ => {
                            log.lock().unwrap().push(msg.body_str().into_owned());
                            Ack::Ok
                        }
                    }
                }
            }),
            ListenerOptions {
                concurrency,
                buffer_size: 16,
                max_delivery_attempts: 3,
                dead_letter: Some("chan://dlq".into()),
                retry_backoff: Backoff::none(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }

    let producers: Vec<_> = (0..PRODUCERS)
        .map(|p| {
            let bus = bus.clone();
            tokio::spawn(async move {
                for i in 0..PER_PRODUCER {
                    let n = p * PER_PRODUCER + i;
                    bus.send(
                        "chan://work",
                        Message::text(n.to_string()).with_routing_key((n % 13).to_string()),
                        SendOptions::new().on_full(OnFull::Block),
                    )
                    .await
                    .unwrap();
                }
            })
        })
        .collect();
    for producer in producers {
        producer.await.unwrap();
    }

    assert!(bus.shutdown(DRAIN).await.is_clean());
    assert_eq!(bus.pending(), 0);

    let total = PRODUCERS * PER_PRODUCER;
    let mut seen: Vec<usize> = entries(&acked)
        .iter()
        .chain(entries(&dlq).iter())
        .map(|body| body.parse().unwrap())
        .collect();
    seen.sort_unstable();
    let expected: Vec<usize> = (0..total).collect();
    assert_eq!(
        seen, expected,
        "every message was acked or dead-lettered exactly once"
    );
    assert_eq!(
        entries(&dlq).len(),
        total / 10,
        "exactly the poison messages"
    );
}

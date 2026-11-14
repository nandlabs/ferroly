#![cfg(feature = "messaging")]
//! Tests for the messaging capabilities: publish/subscribe topics and
//! wildcards, competing consumers, routing keys and concurrency, TTL, delayed
//! delivery, request/reply, de-duplication, batch sends, backpressure
//! policies, the observer hooks, and the `Provider` extension point.
//!
//! Everything runs on tokio's paused clock.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::messaging::{
    handler, headers, Ack, Backoff, BoxFuture, DropReason, ExpiryAction, Handler, ListenerOptions,
    LocalOptions, LocalProvider, Message, MessagingError, Observer, OnFull, Producer, Provider,
    ProviderRegistry, Receiver, SendOptions,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::time::Instant;

const DRAIN: Duration = Duration::from_secs(60);

type Log = Arc<Mutex<Vec<String>>>;

fn new_log() -> Log {
    Arc::new(Mutex::new(Vec::new()))
}

fn entries(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

fn sorted(log: &Log) -> Vec<String> {
    let mut all = entries(log);
    all.sort();
    all
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

/// A handler that signals `started` and then never finishes.
fn stuck(started: UnboundedSender<()>) -> Handler {
    handler(move |_msg| {
        let started = started.clone();
        async move {
            let _ = started.send(());
            std::future::pending::<()>().await;
            Ack::Ok
        }
    })
}

async fn send(bus: &LocalProvider, addr: &str, body: &str) {
    bus.send(addr, Message::text(body), SendOptions::default())
        .await
        .unwrap();
}

async fn listen(bus: &LocalProvider, addr: &str, log: &Log) {
    bus.add_listener(addr, acking(log), ListenerOptions::default())
        .await
        .unwrap();
}

// ---- publish/subscribe -------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn every_subscription_receives_every_message() {
    let bus = LocalProvider::new("t");
    let (audit, billing, shipping) = (new_log(), new_log(), new_log());
    for log in [&audit, &billing, &shipping] {
        listen(&bus, "topic://orders.created", log).await;
    }

    for body in ["o-1", "o-2", "o-3"] {
        send(&bus, "topic://orders.created", body).await;
    }
    assert!(bus.shutdown(DRAIN).await.is_clean());

    // Fan-out: unlike a queue, each subscription sees the full stream, in order.
    for log in [&audit, &billing, &shipping] {
        assert_eq!(entries(log), ["o-1", "o-2", "o-3"]);
    }
}

#[tokio::test(start_paused = true)]
async fn wildcard_subscriptions_match_by_token() {
    let bus = LocalProvider::new("t");
    let (exact, one, many, other) = (new_log(), new_log(), new_log(), new_log());
    listen(&bus, "topic://orders.created", &exact).await;
    listen(&bus, "topic://orders.*", &one).await;
    listen(&bus, "topic://orders.>", &many).await;
    listen(&bus, "topic://payments.>", &other).await;

    // The wildcard subscriber learns the concrete topic from a header.
    let topics = new_log();
    let seen = topics.clone();
    bus.add_listener(
        "topic://>",
        handler(move |msg: Message| {
            seen.lock()
                .unwrap()
                .push(msg.topic().unwrap_or("?").to_string());
            async { Ack::Ok }
        }),
        ListenerOptions::default(),
    )
    .await
    .unwrap();

    for topic in ["orders.created", "orders.cancelled", "orders.eu.created"] {
        send(&bus, &format!("topic://{topic}"), topic).await;
    }
    assert!(bus.shutdown(DRAIN).await.is_clean());

    assert_eq!(entries(&exact), ["orders.created"]);
    assert_eq!(entries(&one), ["orders.created", "orders.cancelled"]);
    assert_eq!(
        entries(&many),
        ["orders.created", "orders.cancelled", "orders.eu.created"]
    );
    assert!(entries(&other).is_empty());
    assert_eq!(
        entries(&topics),
        ["orders.created", "orders.cancelled", "orders.eu.created"]
    );
}

#[tokio::test(start_paused = true)]
async fn listeners_sharing_a_named_subscription_compete() {
    let bus = LocalProvider::new("t");
    let (worker_a, worker_b, audit) = (new_log(), new_log(), new_log());
    for log in [&worker_a, &worker_b] {
        bus.add_listener(
            "topic://jobs.*",
            acking(log),
            ListenerOptions {
                subscription: Some("workers".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    listen(&bus, "topic://jobs.*", &audit).await;

    for i in 0..6 {
        send(&bus, "topic://jobs.resize", &i.to_string()).await;
    }
    assert!(bus.shutdown(DRAIN).await.is_clean());

    // The "workers" subscription got each message once, split between its two
    // listeners; the separate audit subscription got all of them.
    assert_eq!(entries(&worker_a), ["0", "2", "4"]);
    assert_eq!(entries(&worker_b), ["1", "3", "5"]);
    assert_eq!(entries(&audit).len(), 6);
}

#[derive(Default)]
struct Drops(Mutex<Vec<(String, String, DropReason)>>);

impl Observer for Drops {
    fn on_dropped(&self, addr: &str, msg: &Message, reason: DropReason) {
        self.0
            .lock()
            .unwrap()
            .push((addr.to_string(), msg.body_str().into_owned(), reason));
    }
}

impl Drops {
    fn take(&self) -> Vec<(String, String, DropReason)> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

fn dropped(addr: &str, body: &str, reason: DropReason) -> (String, String, DropReason) {
    (addr.to_string(), body.to_string(), reason)
}

#[tokio::test(start_paused = true)]
async fn publishing_without_subscribers_is_reported_but_not_an_error() {
    let bus = LocalProvider::new("t");
    let drops = Arc::new(Drops::default());
    bus.set_observer(drops.clone());

    send(&bus, "topic://nobody.listens", "x").await;
    assert_eq!(
        drops.take(),
        vec![dropped(
            "topic://nobody.listens",
            "x",
            DropReason::NoSubscribers
        )]
    );

    // Wildcards belong in subscriptions, not in the topic being published to.
    let err = bus
        .send(
            "topic://orders.*",
            Message::text("x"),
            SendOptions::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, MessagingError::InvalidAddress(_)), "{err}");
    // And a malformed pattern is rejected when subscribing.
    let err = bus
        .add_listener(
            "topic://orders.>.created",
            handler(|_m| async { Ack::Ok }),
            ListenerOptions::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, MessagingError::InvalidAddress(_)), "{err}");
}

#[tokio::test(start_paused = true)]
async fn a_full_subscription_misses_the_message_without_affecting_the_others() {
    let bus = LocalProvider::new("t");
    let drops = Arc::new(Drops::default());
    bus.set_observer(drops.clone());

    let healthy = new_log();
    listen(&bus, "topic://events", &healthy).await;
    let (started_tx, mut started_rx) = unbounded_channel();
    bus.add_listener(
        "topic://events",
        stuck(started_tx),
        ListenerOptions {
            buffer_size: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    send(&bus, "topic://events", "1").await;
    started_rx.recv().await.unwrap(); // the slow subscriber is stuck in "1"
    send(&bus, "topic://events", "2").await; // fills its one-slot buffer
    send(&bus, "topic://events", "3").await; // it has no room for this one

    assert_eq!(
        drops.take(),
        vec![dropped("topic://events", "3", DropReason::BufferFull)]
    );
    let report = bus.shutdown(Duration::from_secs(1)).await;
    // The healthy subscription got everything.
    assert_eq!(entries(&healthy), ["1", "2", "3"]);
    assert_eq!(
        report.undelivered.len(),
        2,
        "the stuck subscriber's 1 and 2"
    );
}

// ---- queues: competing consumers, routing keys, concurrency ------------------

#[tokio::test(start_paused = true)]
async fn competing_consumers_take_turns_without_a_routing_key() {
    let bus = LocalProvider::new("t");
    let (a, b) = (new_log(), new_log());
    listen(&bus, "chan://work", &a).await;
    listen(&bus, "chan://work", &b).await;

    for i in 0..6 {
        send(&bus, "chan://work", &i.to_string()).await;
    }
    assert!(bus.shutdown(DRAIN).await.is_clean());
    // Each message went to exactly one listener, round-robin.
    assert_eq!(entries(&a), ["0", "2", "4"]);
    assert_eq!(entries(&b), ["1", "3", "5"]);
}

#[tokio::test(start_paused = true)]
async fn a_routing_key_always_lands_on_the_same_listener() {
    let bus = LocalProvider::new("t");
    let logs = [new_log(), new_log(), new_log()];
    for log in &logs {
        listen(&bus, "chan://work", log).await;
    }

    for i in 0..12 {
        // The key on the message...
        let msg = Message::text(format!("tenant-a/{i}")).with_routing_key("tenant-a");
        bus.send("chan://work", msg, SendOptions::default())
            .await
            .unwrap();
        // ...or overridden per send; the option wins over the message's key.
        let msg = Message::text(format!("tenant-b/{i}")).with_routing_key("tenant-a");
        bus.send(
            "chan://work",
            msg,
            SendOptions::new().routing_key("tenant-b"),
        )
        .await
        .unwrap();
    }
    assert!(bus.shutdown(DRAIN).await.is_clean());

    for tenant in ["tenant-a", "tenant-b"] {
        let owners: Vec<Vec<String>> = logs
            .iter()
            .map(|log| {
                entries(log)
                    .into_iter()
                    .filter(|b| b.starts_with(tenant))
                    .collect::<Vec<_>>()
            })
            .filter(|bodies| !bodies.is_empty())
            .collect();
        assert_eq!(owners.len(), 1, "{tenant} must stick to one listener");
        let expected: Vec<String> = (0..12).map(|i| format!("{tenant}/{i}")).collect();
        assert_eq!(owners[0], expected, "and stay in send order");
    }
}

#[tokio::test(start_paused = true)]
async fn concurrency_bounds_the_handlers_running_at_once() {
    let bus = LocalProvider::new("t");
    let running = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let done = new_log();
    let (r, p, d) = (running.clone(), peak.clone(), done.clone());
    bus.add_listener(
        "chan://io",
        handler(move |msg: Message| {
            let (running, peak, done) = (r.clone(), p.clone(), d.clone());
            async move {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_secs(1)).await;
                running.fetch_sub(1, Ordering::SeqCst);
                done.lock().unwrap().push(msg.body_str().into_owned());
                Ack::Ok
            }
        }),
        ListenerOptions {
            concurrency: Some(3),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let began = Instant::now();
    for i in 0..9 {
        send(&bus, "chan://io", &i.to_string()).await;
    }
    assert!(bus.shutdown(DRAIN).await.is_clean());

    assert_eq!(peak.load(Ordering::SeqCst), 3, "never more than 3 at once");
    // 9 one-second handlers, 3 at a time: 3 s rather than 9 s.
    assert_eq!(began.elapsed(), Duration::from_secs(3));
    assert_eq!(sorted(&done).len(), 9);
}

// ---- TTL and delayed delivery -------------------------------------------------

#[derive(Default)]
struct Fates {
    expired: Mutex<Vec<String>>,
    dropped: Mutex<Vec<(String, DropReason)>>,
    dead_letters: Mutex<Vec<(String, bool)>>,
}

impl Observer for Fates {
    fn on_expired(&self, _addr: &str, msg: &Message) {
        self.expired
            .lock()
            .unwrap()
            .push(msg.body_str().into_owned());
    }
    fn on_dropped(&self, _addr: &str, msg: &Message, reason: DropReason) {
        self.dropped
            .lock()
            .unwrap()
            .push((msg.body_str().into_owned(), reason));
    }
    fn on_dead_letter(&self, _addr: &str, msg: &Message, to_dlq: bool) {
        self.dead_letters
            .lock()
            .unwrap()
            .push((msg.body_str().into_owned(), to_dlq));
    }
}

/// A listener whose handler takes 10 s per message, so later messages wait.
async fn slow_listener(bus: &LocalProvider, log: &Log, opts: ListenerOptions) {
    let acked = log.clone();
    bus.add_listener(
        "chan://q",
        handler(move |msg: Message| {
            let acked = acked.clone();
            async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
                acked.lock().unwrap().push(msg.body_str().into_owned());
                Ack::Ok
            }
        }),
        opts,
    )
    .await
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn an_expired_message_goes_to_the_dead_letter_queue() {
    let bus = LocalProvider::new("t");
    let fates = Arc::new(Fates::default());
    bus.set_observer(fates.clone());
    let (handled, dlq) = (new_log(), new_log());
    listen(&bus, "chan://dlq", &dlq).await;
    slow_listener(
        &bus,
        &handled,
        ListenerOptions {
            dead_letter: Some("chan://dlq".into()),
            ..Default::default()
        },
    )
    .await;

    send(&bus, "chan://q", "first").await; // occupies the handler for 10 s
    let ttl = |secs| SendOptions::new().ttl(Duration::from_secs(secs));
    bus.send("chan://q", Message::text("stale"), ttl(5))
        .await
        .unwrap();
    bus.send("chan://q", Message::text("fresh"), ttl(60))
        .await
        .unwrap();
    assert!(bus.shutdown(DRAIN).await.is_clean());

    // "stale" waited 10 s behind "first", past its 5 s TTL: never handled.
    assert_eq!(entries(&handled), ["first", "fresh"]);
    assert_eq!(entries(&dlq), ["stale"]);
    assert_eq!(*fates.expired.lock().unwrap(), vec!["stale".to_string()]);
    assert_eq!(
        *fates.dead_letters.lock().unwrap(),
        vec![("stale".to_string(), true)]
    );
    assert!(fates.dropped.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_expired_message_is_dropped_when_so_configured() {
    let bus = LocalProvider::new("t");
    let fates = Arc::new(Fates::default());
    bus.set_observer(fates.clone());
    let (handled, dlq) = (new_log(), new_log());
    listen(&bus, "chan://dlq", &dlq).await;
    slow_listener(
        &bus,
        &handled,
        ListenerOptions {
            dead_letter: Some("chan://dlq".into()),
            on_expired: ExpiryAction::Drop,
            ..Default::default()
        },
    )
    .await;

    send(&bus, "chan://q", "first").await;
    bus.send(
        "chan://q",
        Message::text("stale"),
        SendOptions::new().ttl(Duration::from_secs(5)),
    )
    .await
    .unwrap();
    assert!(bus.shutdown(DRAIN).await.is_clean());

    assert_eq!(entries(&handled), ["first"]);
    assert!(entries(&dlq).is_empty());
    assert_eq!(
        *fates.dropped.lock().unwrap(),
        vec![("stale".to_string(), DropReason::Expired)]
    );
}

#[tokio::test(start_paused = true)]
async fn a_delayed_message_is_delivered_when_it_is_due() {
    let bus = LocalProvider::new("t");
    let began = Instant::now();
    let log: Arc<Mutex<Vec<(String, Duration)>>> = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    bus.add_listener(
        "chan://q",
        handler(move |msg: Message| {
            seen.lock()
                .unwrap()
                .push((msg.body_str().into_owned(), began.elapsed()));
            async { Ack::Ok }
        }),
        ListenerOptions::default(),
    )
    .await
    .unwrap();

    let after = |secs| SendOptions::new().delay(Duration::from_secs(secs));
    bus.send("chan://q", Message::text("in-30s"), after(30))
        .await
        .unwrap();
    bus.send("chan://q", Message::text("in-10s"), after(10))
        .await
        .unwrap();
    send(&bus, "chan://q", "now").await;
    assert_eq!(bus.pending(), 3);

    assert!(bus.shutdown(DRAIN).await.is_clean());
    let secs = Duration::from_secs;
    assert_eq!(
        *log.lock().unwrap(),
        vec![
            ("now".to_string(), secs(0)),
            ("in-10s".to_string(), secs(10)),
            ("in-30s".to_string(), secs(30)),
        ]
    );
}

// ---- request / reply ----------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn request_gets_the_responders_reply() {
    let bus = LocalProvider::new("t");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let requests = seen.clone();
    let echo = bus.responder(move |request: Message| {
        requests.lock().unwrap().push(request.clone());
        async move { Ok(Message::text(format!("echo: {}", request.body_str()))) }
    });
    bus.add_listener("chan://echo", echo, ListenerOptions::default())
        .await
        .unwrap();

    let request = Message::text("hi").with_traceparent("00-abc-def-01");
    let reply = bus
        .request("chan://echo", request, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(reply.body_str(), "echo: hi");

    // The request was stamped with an id, a correlation id and a reply address;
    // the reply carries the correlation id and the trace context back.
    let request = seen.lock().unwrap()[0].clone();
    let id = request.id().expect("message id").to_string();
    assert_eq!(request.correlation_id(), Some(id.as_str()));
    assert!(request.reply_to().unwrap().starts_with("reply://"));
    assert_eq!(reply.correlation_id(), Some(id.as_str()));
    assert_eq!(reply.traceparent(), Some("00-abc-def-01"));
}

#[tokio::test(start_paused = true)]
async fn request_times_out_and_does_not_leave_work_behind() {
    let bus = LocalProvider::new("t");
    let handled = new_log();
    // A responder that takes 10 s; requests behind it wait in the buffer.
    let log = handled.clone();
    let slow = bus.responder(move |request: Message| {
        let log = log.clone();
        async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            log.lock().unwrap().push(request.body_str().into_owned());
            Ok(Message::text("late"))
        }
    });
    bus.add_listener("chan://slow", slow, ListenerOptions::default())
        .await
        .unwrap();

    let began = Instant::now();
    let (first, second) = tokio::join!(
        bus.request("chan://slow", Message::text("a"), Duration::from_secs(2)),
        bus.request("chan://slow", Message::text("b"), Duration::from_secs(2)),
    );
    assert_eq!(began.elapsed(), Duration::from_secs(2));
    for result in [first, second] {
        let err = result.unwrap_err();
        assert!(
            matches!(&err, MessagingError::RequestTimeout(addr) if addr == "chan://slow"),
            "{err}"
        );
    }

    assert!(bus.shutdown(DRAIN).await.is_clean());
    // "a" was already being handled; "b" expired in the buffer (its TTL is the
    // request timeout) instead of being processed for nobody.
    assert_eq!(entries(&handled), ["a"]);

    // A request to nowhere fails at once, with the message handed back.
    let bus = LocalProvider::new("t2");
    let err = bus
        .request("chan://nobody", Message::text("x"), Duration::from_secs(2))
        .await
        .unwrap_err();
    assert!(matches!(err, MessagingError::NoListeners(_)), "{err}");
}

#[tokio::test(start_paused = true)]
async fn a_handler_can_reply_by_hand_and_decline_to_reply() {
    let bus = LocalProvider::new("t");
    // Replies explicitly through a weak handle (so the handler does not keep
    // the provider alive).
    let weak = bus.downgrade();
    bus.add_listener(
        "chan://upper",
        handler(move |request: Message| {
            let weak = weak.clone();
            async move {
                let Some(bus) = weak.upgrade() else {
                    return Ack::Reject;
                };
                let reply = Message::text(request.body_str().to_uppercase());
                match bus.reply(&request, reply).await {
                    Ok(()) => Ack::Ok,
                    Err(_) => Ack::Reject,
                }
            }
        }),
        ListenerOptions::default(),
    )
    .await
    .unwrap();
    // A responder that rejects the request instead of answering.
    let refuse = bus.responder(|_request| async { Err(Ack::Reject) });
    bus.add_listener("chan://refuse", refuse, ListenerOptions::default())
        .await
        .unwrap();

    let reply = bus
        .request("chan://upper", Message::text("abc"), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(reply.body_str(), "ABC");

    let err = bus
        .request("chan://refuse", Message::text("x"), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(matches!(err, MessagingError::RequestTimeout(_)), "{err}");

    // Replying to a message that is not a request is an error, not a panic.
    let err = bus
        .reply(&Message::text("plain"), Message::text("?"))
        .await
        .unwrap_err();
    assert!(matches!(err, MessagingError::InvalidAddress(_)), "{err}");
}

// ---- de-duplication -------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn duplicates_within_the_window_are_dropped() {
    let bus = LocalProvider::with_options(
        "t",
        LocalOptions {
            dedup_window: 2,
            ..Default::default()
        },
    );
    let drops = Arc::new(Drops::default());
    bus.set_observer(drops.clone());
    let (orders, audit) = (new_log(), new_log());
    listen(&bus, "chan://orders", &orders).await;
    listen(&bus, "chan://audit", &audit).await;

    let with_id = |body: &str, id: &str| Message::text(body).with_id(id);
    let opts = SendOptions::default;
    for (body, id) in [("a", "1"), ("a-again", "1"), ("b", "2"), ("c", "3")] {
        bus.send("chan://orders", with_id(body, id), opts())
            .await
            .unwrap();
    }
    // Id 1 has left the two-entry window (2 and 3 pushed it out): accepted.
    bus.send("chan://orders", with_id("a-later", "1"), opts())
        .await
        .unwrap();
    // The window is per destination, and messages without an id are never
    // treated as duplicates.
    bus.send("chan://audit", with_id("other-dest", "3"), opts())
        .await
        .unwrap();
    send(&bus, "chan://orders", "no-id").await;
    send(&bus, "chan://orders", "no-id").await;

    assert!(bus.shutdown(DRAIN).await.is_clean());
    assert_eq!(
        entries(&orders),
        ["a", "b", "c", "a-later", "no-id", "no-id"]
    );
    assert_eq!(entries(&audit), ["other-dest"]);
    assert_eq!(
        drops.take(),
        vec![dropped("chan://orders", "a-again", DropReason::Duplicate)]
    );
}

#[tokio::test(start_paused = true)]
async fn a_rejected_send_does_not_poison_the_dedup_window() {
    let bus = LocalProvider::with_options(
        "t",
        LocalOptions {
            dedup_window: 16,
            ..Default::default()
        },
    );
    // Nobody listens yet: the send fails and hands the message back...
    let err = bus
        .send(
            "chan://q",
            Message::text("x").with_id("42"),
            SendOptions::default(),
        )
        .await
        .unwrap_err();
    let msg = err.into_message().unwrap();

    // ...and retrying the very same message later is not a "duplicate".
    let log = new_log();
    listen(&bus, "chan://q", &log).await;
    bus.send("chan://q", msg, SendOptions::default())
        .await
        .unwrap();
    assert!(bus.shutdown(DRAIN).await.is_clean());
    assert_eq!(entries(&log), ["x"]);
}

// ---- batch send -----------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn send_batch_sends_in_order_and_reports_a_partial_failure() {
    let bus = LocalProvider::new("t");
    let log = new_log();
    listen(&bus, "chan://q", &log).await;
    let batch = |bodies: &[&str]| bodies.iter().map(|b| Message::text(*b)).collect::<Vec<_>>();

    bus.send_batch("chan://q", batch(&["1", "2", "3"]), SendOptions::default())
        .await
        .unwrap();
    assert!(bus.shutdown(DRAIN).await.is_clean());
    assert_eq!(entries(&log), ["1", "2", "3"]);

    // A two-slot buffer behind a stuck handler takes one in flight plus two
    // buffered; the rest of the batch comes back.
    let bus = LocalProvider::new("t2");
    let (started_tx, mut started_rx) = unbounded_channel();
    bus.add_listener(
        "chan://q",
        stuck(started_tx),
        ListenerOptions {
            buffer_size: 2,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    send(&bus, "chan://q", "in-flight").await;
    started_rx.recv().await.unwrap();

    let err = bus
        .send_batch(
            "chan://q",
            batch(&["a", "b", "c", "d"]),
            SendOptions::default(),
        )
        .await
        .unwrap_err();
    let MessagingError::Batch(failure) = err else {
        panic!("expected a Batch error, got {err}");
    };
    assert_eq!(failure.sent, 2);
    assert!(
        matches!(*failure.error, MessagingError::BufferFull(_)),
        "{}",
        failure.error
    );
    let unsent: Vec<String> = failure
        .unsent
        .iter()
        .map(|m| m.body_str().into_owned())
        .collect();
    assert_eq!(
        unsent,
        ["c", "d"],
        "the failed message first, then the rest"
    );
}

// ---- backpressure policies -------------------------------------------------------

/// A one-slot queue whose handler is stuck on a first message, with a second
/// message ("buffered") filling the slot.
async fn full_queue(bus: &LocalProvider) {
    let (started_tx, mut started_rx) = unbounded_channel();
    bus.add_listener(
        "chan://q",
        stuck(started_tx),
        ListenerOptions {
            buffer_size: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    send(bus, "chan://q", "in-flight").await;
    started_rx.recv().await.unwrap();
    send(bus, "chan://q", "buffered").await;
}

#[tokio::test(start_paused = true)]
async fn block_for_waits_then_times_out_with_the_message() {
    let bus = LocalProvider::new("t");
    full_queue(&bus).await;

    let began = Instant::now();
    let err = bus
        .send(
            "chan://q",
            Message::text("patient"),
            SendOptions::new().on_full(OnFull::BlockFor(Duration::from_secs(3))),
        )
        .await
        .unwrap_err();
    assert_eq!(began.elapsed(), Duration::from_secs(3));
    assert!(matches!(err, MessagingError::SendTimeout(_)), "{err}");
    assert_eq!(err.into_message().unwrap().body_str(), "patient");
}

#[tokio::test(start_paused = true)]
async fn drop_oldest_evicts_and_reports_the_displaced_message() {
    let bus = LocalProvider::new("t");
    let drops = Arc::new(Drops::default());
    bus.set_observer(drops.clone());
    full_queue(&bus).await;

    bus.send(
        "chan://q",
        Message::text("newest"),
        SendOptions::new().on_full(OnFull::DropOldest),
    )
    .await
    .unwrap();
    assert_eq!(
        drops.take(),
        vec![dropped("chan://q", "buffered", DropReason::Evicted)]
    );
    assert_eq!(bus.queue_depth("chan://q"), Some(1));

    let report = bus.shutdown(Duration::from_secs(1)).await;
    let mut left: Vec<String> = report
        .undelivered
        .iter()
        .map(|u| u.message.body_str().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["in-flight", "newest"]);
}

#[tokio::test(start_paused = true)]
async fn block_waits_for_room() {
    let bus = LocalProvider::new("t");
    let log = new_log();
    let acked = log.clone();
    bus.add_listener(
        "chan://q",
        handler(move |msg: Message| {
            let acked = acked.clone();
            async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                acked.lock().unwrap().push(msg.body_str().into_owned());
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

    // Five sends into a one-slot buffer: each waits for the consumer.
    let began = Instant::now();
    for i in 0..5 {
        bus.send(
            "chan://q",
            Message::text(i.to_string()),
            SendOptions::new().on_full(OnFull::Block),
        )
        .await
        .unwrap();
    }
    // The legacy flag means the same thing.
    bus.send(
        "chan://q",
        Message::text("5"),
        SendOptions {
            block_on_full: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        began.elapsed() >= Duration::from_secs(4),
        "sends were paced"
    );

    assert!(bus.shutdown(DRAIN).await.is_clean());
    assert_eq!(entries(&log), ["0", "1", "2", "3", "4", "5"]);
}

// ---- observer: enough for metrics -------------------------------------------------

#[derive(Default)]
struct Metrics {
    counters: Mutex<HashMap<&'static str, u64>>,
    depths: Mutex<Vec<usize>>,
    durations: Mutex<Vec<(Duration, Ack)>>,
}

impl Metrics {
    fn bump(&self, name: &'static str) {
        *self.counters.lock().unwrap().entry(name).or_default() += 1;
    }

    fn count(&self, name: &'static str) -> u64 {
        self.counters
            .lock()
            .unwrap()
            .get(name)
            .copied()
            .unwrap_or(0)
    }
}

impl Observer for Metrics {
    fn on_send(&self, _addr: &str, _msg: &Message) {
        self.bump("enqueued");
    }
    fn on_receive(&self, _addr: &str, _msg: &Message) {
        self.bump("delivered");
    }
    fn on_ack(&self, _addr: &str, _msg: &Message) {
        self.bump("acked");
    }
    fn on_nack(&self, _addr: &str, _msg: &Message, requeue: bool) {
        self.bump(if requeue { "retried" } else { "rejected" });
    }
    fn on_dead_letter(&self, _addr: &str, _msg: &Message, to_dlq: bool) {
        self.bump(if to_dlq { "dead_lettered" } else { "lost" });
    }
    fn on_handled(&self, _addr: &str, _msg: &Message, elapsed: Duration, ack: Ack) {
        self.durations.lock().unwrap().push((elapsed, ack));
    }
    fn on_dropped(&self, _addr: &str, _msg: &Message, _reason: DropReason) {
        self.bump("dropped");
    }
    fn on_queue_depth(&self, addr: &str, depth: usize) {
        if addr == "chan://q" {
            self.depths.lock().unwrap().push(depth);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn the_observer_hooks_are_sufficient_for_metrics() {
    let bus = LocalProvider::new("t");
    let metrics = Arc::new(Metrics::default());
    bus.set_observer(metrics.clone());

    listen(&bus, "chan://dlq", &new_log()).await;
    bus.add_listener(
        "chan://q",
        handler(|msg: Message| async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            match &*msg.body_str() {
                "ok" => Ack::Ok,
                "reject" => Ack::Reject,
                _ => Ack::Retry, // "poison"
            }
        }),
        ListenerOptions {
            max_delivery_attempts: 2,
            dead_letter: Some("chan://dlq".into()),
            retry_backoff: Backoff::none(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    for body in ["ok", "reject", "poison"] {
        send(&bus, "chan://q", body).await;
    }
    assert_eq!(bus.queue_depth("chan://q"), Some(3));
    assert_eq!(bus.queue_depth("chan://nobody"), None);
    assert!(bus.shutdown(DRAIN).await.is_clean());

    // 3 sends to q + 1 dead-letter move into dlq.
    assert_eq!(metrics.count("enqueued"), 4);
    // q: ok, reject, poison twice; dlq: poison once.
    assert_eq!(metrics.count("delivered"), 5);
    assert_eq!(metrics.count("acked"), 2, "ok on q, poison on dlq");
    assert_eq!(metrics.count("retried"), 1);
    assert_eq!(
        metrics.count("rejected"),
        2,
        "reject, and poison's last nack"
    );
    assert_eq!(metrics.count("dead_lettered"), 1);
    assert_eq!(metrics.count("lost"), 0);
    assert_eq!(metrics.count("dropped"), 0);

    // Queue depth rose with each send and fell with each dequeue.
    assert_eq!(*metrics.depths.lock().unwrap(), vec![1, 2, 3, 2, 1, 0]);

    // Handler duration and outcome for each invocation on q (250 ms each).
    let durations = metrics.durations.lock().unwrap().clone();
    let on_q: Vec<Ack> = durations
        .iter()
        .filter(|(elapsed, _)| *elapsed == Duration::from_millis(250))
        .map(|(_, ack)| *ack)
        .collect();
    assert_eq!(on_q, vec![Ack::Ok, Ack::Reject, Ack::Retry, Ack::Retry]);
    assert_eq!(durations.len(), 5);
}

#[tokio::test(start_paused = true)]
async fn a_panicking_observer_does_not_disturb_delivery() {
    struct Grumpy;
    impl Observer for Grumpy {
        fn on_receive(&self, _addr: &str, _msg: &Message) {
            panic!("observer bug");
        }
    }
    let bus = LocalProvider::new("t");
    bus.set_observer(Arc::new(Grumpy));
    let log = new_log();
    listen(&bus, "chan://q", &log).await;
    send(&bus, "chan://q", "x").await;
    assert!(bus.shutdown(DRAIN).await.is_clean());
    assert_eq!(entries(&log), ["x"]);
}

// ---- listener removal --------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn removed_listeners_finish_their_backlog_and_take_nothing_new() {
    let bus = LocalProvider::new("t");
    let log = new_log();
    let acked = log.clone();
    bus.add_listener(
        "chan://q",
        handler(move |msg: Message| {
            let acked = acked.clone();
            async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                acked.lock().unwrap().push(msg.body_str().into_owned());
                Ack::Ok
            }
        }),
        ListenerOptions::default(),
    )
    .await
    .unwrap();
    for body in ["1", "2", "3"] {
        send(&bus, "chan://q", body).await;
    }

    assert_eq!(bus.remove_listeners("chan://q").await.unwrap(), 1);
    assert_eq!(bus.remove_listeners("chan://q").await.unwrap(), 0);
    let err = bus
        .send("chan://q", Message::text("4"), SendOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, MessagingError::NoListeners(_)), "{err}");

    assert!(bus.shutdown(DRAIN).await.is_clean());
    assert_eq!(
        entries(&log),
        ["1", "2", "3"],
        "the backlog was still handled"
    );

    // Topic subscriptions are removed by pattern.
    let bus = LocalProvider::new("t2");
    listen(&bus, "topic://a.*", &new_log()).await;
    listen(&bus, "topic://a.*", &new_log()).await;
    listen(&bus, "topic://b.*", &new_log()).await;
    assert_eq!(bus.remove_listeners("topic://a.*").await.unwrap(), 2);
    assert_eq!(bus.queue_depth("topic://a.*"), None);
    assert_eq!(bus.queue_depth("topic://b.*"), Some(0));
}

// ---- the Provider extension point --------------------------------------------------

/// A stand-in for an external broker adapter: it implements only the required
/// methods of `Producer`, `Receiver` and `Provider`, the way a Kafka or SQS
/// adapter crate would.
#[derive(Default)]
struct FakeBroker {
    sent: Mutex<Vec<(String, String)>>,
    listeners: Mutex<Vec<String>>,
}

impl Producer for FakeBroker {
    fn send(
        &self,
        addr: &str,
        msg: Message,
        _opts: SendOptions,
    ) -> BoxFuture<'_, Result<(), MessagingError>> {
        let addr = addr.to_string();
        Box::pin(async move {
            if msg.body_str() == "unroutable" {
                // A transport failure, reported the way an adapter would.
                return Err(MessagingError::backend(std::io::Error::other(
                    "broker unreachable",
                )));
            }
            self.sent
                .lock()
                .unwrap()
                .push((addr, msg.body_str().into_owned()));
            Ok(())
        })
    }
}

impl Receiver for FakeBroker {
    fn add_listener(
        &self,
        addr: &str,
        _handler: Handler,
        _opts: ListenerOptions,
    ) -> BoxFuture<'_, Result<(), MessagingError>> {
        self.listeners.lock().unwrap().push(addr.to_string());
        Box::pin(async { Ok(()) })
    }
}

impl Provider for FakeBroker {
    fn schemes(&self) -> &[&str] {
        &["fake"]
    }
    fn setup(&self) -> BoxFuture<'_, Result<(), MessagingError>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test(start_paused = true)]
async fn an_external_provider_plugs_in_through_the_traits() {
    let broker = Arc::new(FakeBroker::default());
    let local = LocalProvider::new("local");
    let registry = ProviderRegistry::new();
    registry.register(broker.clone()).unwrap();
    registry.register(Arc::new(local.clone())).unwrap();
    assert_eq!(registry.schemes(), ["chan", "fake", "reply", "topic"]);
    // A scheme can only have one provider.
    let err = registry
        .register(Arc::new(FakeBroker::default()))
        .unwrap_err();
    assert!(matches!(err, MessagingError::InvalidOptions(_)), "{err}");

    // The registry dispatches on the address scheme.
    let log = new_log();
    registry
        .add_listener("chan://orders", acking(&log), ListenerOptions::default())
        .await
        .unwrap();
    registry
        .add_listener("fake://orders", acking(&log), ListenerOptions::default())
        .await
        .unwrap();
    registry
        .send(
            "chan://orders",
            Message::text("local"),
            SendOptions::default(),
        )
        .await
        .unwrap();
    registry
        .send(
            "fake://orders",
            Message::text("remote"),
            SendOptions::default(),
        )
        .await
        .unwrap();
    let err = registry
        .send("sqs://orders", Message::text("x"), SendOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, MessagingError::InvalidAddress(_)), "{err}");

    // The defaulted methods work for a provider that implements only the
    // required ones: batch falls back to per-message sends...
    let provider: Arc<dyn Provider> = broker.clone();
    let batch = vec![
        Message::text("b1"),
        Message::text("unroutable"),
        Message::text("b3"),
    ];
    let err = provider
        .send_batch("fake://orders", batch, SendOptions::default())
        .await
        .unwrap_err();
    let MessagingError::Batch(failure) = err else {
        panic!("expected a Batch error, got {err}");
    };
    assert_eq!(failure.sent, 1);
    assert!(matches!(*failure.error, MessagingError::Backend { .. }));
    assert!(std::error::Error::source(&*failure.error).is_some());
    // (the backend error did not hand the failed message back, so only the
    // untried remainder is listed)
    assert_eq!(failure.unsent.len(), 1);
    // ...listener removal reports "unsupported", and close is a no-op.
    let err = provider
        .remove_listeners("fake://orders")
        .await
        .unwrap_err();
    assert!(matches!(err, MessagingError::Unsupported(_)), "{err}");
    provider.setup().await.unwrap();
    provider.close().await.unwrap();

    assert_eq!(
        *broker.sent.lock().unwrap(),
        vec![
            ("fake://orders".to_string(), "remote".to_string()),
            ("fake://orders".to_string(), "b1".to_string()),
        ]
    );
    assert_eq!(*broker.listeners.lock().unwrap(), vec!["fake://orders"]);

    // `close` on the local provider is its draining shutdown.
    let local: Arc<dyn Provider> = Arc::new(local);
    local.close().await.unwrap();
    assert_eq!(entries(&log), ["local"]);
}

#[tokio::test(start_paused = true)]
async fn headers_carry_the_metadata_conventions_end_to_end() {
    let bus = LocalProvider::new("t");
    let received = Arc::new(Mutex::new(Vec::new()));
    let seen = received.clone();
    bus.add_listener(
        "chan://q",
        handler(move |msg: Message| {
            seen.lock().unwrap().push(msg);
            async { Ack::Ok }
        }),
        ListenerOptions::default(),
    )
    .await
    .unwrap();

    let sent = Message::text("payload")
        .stamped()
        .with_correlation_id("saga-7")
        .with_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
        .with_header("tenant", "acme");
    let (id, at) = (sent.id().unwrap().to_string(), sent.timestamp().unwrap());
    bus.send("chan://q", sent, SendOptions::default())
        .await
        .unwrap();
    assert!(bus.shutdown(DRAIN).await.is_clean());

    let got = received.lock().unwrap()[0].clone();
    assert_eq!(got.id(), Some(id.as_str()));
    assert_eq!(got.timestamp(), Some(at));
    assert_eq!(got.correlation_id(), Some("saga-7"));
    assert_eq!(
        got.header_str(headers::TRACEPARENT),
        Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
    );
    assert_eq!(got.header_str("tenant"), Some("acme"));
    // A message produced while handling `got` continues the same trace.
    let follow_up = Message::text("next").with_trace_from(&got);
    assert_eq!(follow_up.traceparent(), got.traceparent());
}

#![cfg(feature = "messaging")]
//! Tests for the seams an external provider plugs into: errors that hand a
//! refused message back, per-entry batch failures, receipts, capabilities,
//! publish-only providers, the registry lifecycle, and subscription filters.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::messaging::{
    handler, Ack, BatchEntryFailure, BatchFailure, BoxFuture, Capabilities, DropReason, Filter,
    ListenerOptions, LocalProvider, Message, MessagingError, Observer, Producer, Provider,
    ProviderRegistry, Receipt, Receiver, Rejected, SendOptions,
};
use tokio::sync::mpsc::unbounded_channel;

/// A publish-only provider standing in for a cloud queue: it records what it
/// accepts and can be told to refuse.
#[derive(Default)]
struct FakeCloud {
    accepted: Mutex<Vec<Message>>,
    /// Bodies that are refused as permanently invalid.
    poison: Mutex<Vec<String>>,
    /// When set, every send is refused as throttled (transient).
    throttled: AtomicBool,
    /// The options of every send that reached the provider.
    options: Mutex<Vec<SendOptions>>,
    /// Receipts handed out so far.
    receipts: AtomicUsize,
}

impl FakeCloud {
    fn poisoned(bodies: &[&str]) -> Self {
        let cloud = FakeCloud::default();
        *cloud.poison.lock().unwrap() = bodies.iter().map(|b| b.to_string()).collect();
        cloud
    }

    fn accepted_bodies(&self) -> Vec<String> {
        self.accepted
            .lock()
            .unwrap()
            .iter()
            .map(|m| m.body_str().into_owned())
            .collect()
    }

    /// The provider's decision for one message.
    fn accept(&self, addr: &str, msg: Message) -> Result<(), MessagingError> {
        if self.throttled.load(Ordering::SeqCst) {
            return Err(MessagingError::refused_transient("throttled", addr, msg));
        }
        let body = msg.body_str().into_owned();
        if self.poison.lock().unwrap().contains(&body) {
            return Err(MessagingError::refused("poison", addr, msg));
        }
        if msg.body.len() > 1024 {
            return Err(MessagingError::refused("too large", addr, msg));
        }
        self.accepted.lock().unwrap().push(msg);
        Ok(())
    }
}

impl Producer for FakeCloud {
    fn send(
        &self,
        addr: &str,
        msg: Message,
        opts: SendOptions,
    ) -> BoxFuture<'_, Result<(), MessagingError>> {
        let addr = addr.to_string();
        Box::pin(async move {
            if let Some(option) = opts.unsupported_by(&self.capabilities()) {
                return Err(MessagingError::InvalidOptions(format!(
                    "{option} is not supported by fakecloud"
                )));
            }
            self.options.lock().unwrap().push(opts);
            self.accept(&addr, msg)
        })
    }

    fn send_with_receipt(
        &self,
        addr: &str,
        msg: Message,
        opts: SendOptions,
    ) -> BoxFuture<'_, Result<Receipt, MessagingError>> {
        let addr = addr.to_string();
        Box::pin(async move {
            self.send(&addr, msg, opts).await?;
            let n = self.receipts.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(Receipt::new()
                .with_message_id(format!("cloud-{n}"))
                .with_sequence(n.to_string()))
        })
    }

    /// A native batch: every message is tried, every refusal is reported.
    fn send_batch(
        &self,
        addr: &str,
        msgs: Vec<Message>,
        _opts: SendOptions,
    ) -> BoxFuture<'_, Result<(), MessagingError>> {
        let addr = addr.to_string();
        Box::pin(async move {
            let mut sent = 0;
            let mut failures = Vec::new();
            for (i, msg) in msgs.into_iter().enumerate() {
                match self.accept(&addr, msg) {
                    Ok(()) => sent += 1,
                    Err(err) => failures.push(BatchEntryFailure::new(i, err)),
                }
            }
            if failures.is_empty() {
                Ok(())
            } else {
                Err(MessagingError::PartialBatch(BatchFailure::new(
                    sent,
                    failures,
                    Vec::new(),
                )))
            }
        })
    }
}

impl Receiver for FakeCloud {}

impl Provider for FakeCloud {
    fn schemes(&self) -> &[&str] {
        &["fakecloud"]
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::new()
            .native_batch(true)
            .max_message_bytes(1024)
            .max_batch_len(10)
    }
}

/// A provider that sends one message at a time and keeps the default
/// (sequential) `send_batch`.
#[derive(Default)]
struct Sequential(FakeCloud);

impl Producer for Sequential {
    fn send(
        &self,
        addr: &str,
        msg: Message,
        opts: SendOptions,
    ) -> BoxFuture<'_, Result<(), MessagingError>> {
        self.0.send(addr, msg, opts)
    }
}

fn texts(bodies: &[&str]) -> Vec<Message> {
    bodies.iter().map(|b| Message::text(*b)).collect()
}

fn bodies(msgs: &[Message]) -> Vec<String> {
    msgs.iter().map(|m| m.body_str().into_owned()).collect()
}

// ---- errors (WP2) ------------------------------------------------------------------

#[tokio::test]
async fn a_refused_send_hands_the_message_back() {
    let cloud = FakeCloud::poisoned(&["bad"]);
    let msg = Message::text("bad").with_attribute("tenant", "acme");
    let err = cloud
        .send("fakecloud://q", msg, SendOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            MessagingError::Refused {
                transient: false,
                ..
            }
        ),
        "{err}"
    );
    assert!(!err.is_retryable());
    assert_eq!(err.destination(), Some("fakecloud://q"));
    assert!(std::error::Error::source(&err).is_some());
    let back = err.into_message().expect("handed back");
    assert_eq!(back.body_str(), "bad");
    assert_eq!(back.attribute_str("tenant"), Some("acme"));
    assert!(cloud.accepted_bodies().is_empty());

    // Too large is a permanent refusal as well.
    let err = cloud
        .send(
            "fakecloud://q",
            Message::new(vec![0u8; 2048]),
            SendOptions::default(),
        )
        .await
        .unwrap_err();
    assert!(!err.is_retryable());
    assert_eq!(err.into_message().map(|m| m.body.len()), Some(2048));
}

#[tokio::test]
async fn a_throttled_send_is_retryable_and_hands_the_message_back() {
    let cloud = FakeCloud::default();
    cloud.throttled.store(true, Ordering::SeqCst);
    let mut err = cloud
        .send("fakecloud://q", Message::text("hi"), SendOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            MessagingError::Refused {
                transient: true,
                ..
            }
        ),
        "{err}"
    );
    assert!(err.is_retryable());
    assert_eq!(
        err.message().map(|m| m.body_str().into_owned()),
        Some("hi".into())
    );
    let back = err.take_message().expect("handed back");
    assert_eq!(back.body_str(), "hi");
    // Taken once; the error stays in place without it.
    assert!(err.message().is_none());

    // Once the throttle lifts, the same message goes through.
    cloud.throttled.store(false, Ordering::SeqCst);
    cloud
        .send("fakecloud://q", back, SendOptions::default())
        .await
        .unwrap();
    assert_eq!(cloud.accepted_bodies(), ["hi"]);
}

#[test]
fn is_retryable_covers_each_variant() {
    let rejected = || Rejected::new("chan://q", Message::text("x"));
    let cases = vec![
        (MessagingError::BufferFull(rejected()), true),
        (MessagingError::SendTimeout(rejected()), true),
        (MessagingError::backend_transient("x"), true),
        (
            MessagingError::refused_transient("x", "chan://q", Message::text("x")),
            true,
        ),
        (MessagingError::backend("x"), false),
        (
            MessagingError::refused("x", "chan://q", Message::text("x")),
            false,
        ),
        (MessagingError::InvalidOptions("x".into()), false),
        (MessagingError::Unsupported("x".into()), false),
        (MessagingError::NoListeners(rejected()), false),
    ];
    for (err, retryable) in cases {
        assert_eq!(err.is_retryable(), retryable, "{err}");
    }
}

#[tokio::test]
async fn a_native_batch_reports_every_failed_entry() {
    let cloud = FakeCloud::poisoned(&["p1", "p3"]);
    let err = cloud
        .send_batch(
            "fakecloud://q",
            texts(&["m0", "p1", "m2", "p3", "m4"]),
            SendOptions::default(),
        )
        .await
        .unwrap_err();
    let MessagingError::PartialBatch(failure) = err else {
        panic!("expected PartialBatch, got {err}");
    };
    assert_eq!(failure.sent, 3);
    let indexes: Vec<usize> = failure.failures.iter().map(|f| f.index).collect();
    assert_eq!(indexes, [1, 3]);
    assert!(failure.unsent.is_empty());
    assert_eq!(failure.failed(), 2);
    assert!(matches!(
        failure.first_error(),
        Some(MessagingError::Refused { .. })
    ));
    assert_eq!(
        failure.to_string().split(':').next(),
        Some("3 sent, 2 failed")
    );
    assert_eq!(bodies(&failure.into_messages()), ["p1", "p3"]);
    assert_eq!(cloud.accepted_bodies(), ["m0", "m2", "m4"]);
}

#[tokio::test]
async fn the_default_batch_stops_at_the_first_failure() {
    let sequential = Sequential(FakeCloud::poisoned(&["c"]));
    let err = sequential
        .send_batch(
            "fakecloud://q",
            texts(&["a", "b", "c", "d", "e"]),
            SendOptions::default(),
        )
        .await
        .unwrap_err();
    let MessagingError::PartialBatch(failure) = err else {
        panic!("expected PartialBatch, got {err}");
    };
    assert_eq!(failure.sent, 2);
    assert_eq!(failure.failures.len(), 1);
    assert_eq!(failure.failures[0].index, 2);
    assert_eq!(
        failure.failures[0]
            .message
            .as_ref()
            .map(|m| m.body_str().into_owned()),
        Some("c".to_string())
    );
    assert_eq!(failure.unsent.len(), 2);
    assert_eq!(bodies(&failure.unsent), ["d", "e"]);
    assert_eq!(sequential.0.accepted_bodies(), ["a", "b"]);
}

#[tokio::test]
async fn a_partial_batch_is_retryable_when_any_entry_is() {
    let permanent = |i| {
        BatchEntryFailure::new(
            i,
            MessagingError::refused("bad", "fakecloud://q", Message::text("x")),
        )
    };
    let transient = |i| {
        BatchEntryFailure::new(
            i,
            MessagingError::refused_transient("slow", "fakecloud://q", Message::text("y")),
        )
    };
    let mixed = MessagingError::PartialBatch(BatchFailure::new(
        0,
        vec![transient(1), permanent(0)],
        Vec::new(),
    ));
    assert!(mixed.is_retryable());
    if let MessagingError::PartialBatch(failure) = &mixed {
        // Sorted by index on construction.
        let indexes: Vec<usize> = failure.failures.iter().map(|f| f.index).collect();
        assert_eq!(indexes, [0, 1]);
    }
    let only_permanent =
        MessagingError::PartialBatch(BatchFailure::new(0, vec![permanent(0)], Vec::new()));
    assert!(!only_permanent.is_retryable());

    // Through a provider: every entry throttled.
    let cloud = FakeCloud::default();
    cloud.throttled.store(true, Ordering::SeqCst);
    let err = cloud
        .send_batch("fakecloud://q", texts(&["a", "b"]), SendOptions::default())
        .await
        .unwrap_err();
    assert!(err.is_retryable(), "{err}");
}

#[cfg(feature = "errutils")]
#[test]
fn root_error_classifies_messaging_errors() {
    use ferroly::errutils::{Classify, ErrorKind};

    let err = ferroly::Error::from(MessagingError::backend_transient("x"));
    assert_eq!(err.kind(), ErrorKind::Unavailable);
    let err = ferroly::Error::from(MessagingError::InvalidOptions("x".into()));
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    let err = ferroly::Error::from(MessagingError::refused_transient(
        "x",
        "q",
        Message::text("x"),
    ));
    assert_eq!(err.kind(), ErrorKind::Unavailable);
    let err = ferroly::Error::from(MessagingError::backend("x"));
    assert_eq!(err.kind(), ErrorKind::Unknown);
    let err = ferroly::Error::from(MessagingError::NoListeners(Rejected::without_message("q")));
    assert_eq!(err.kind(), ErrorKind::NotFound);
}

// ---- provider seams (WP3) ----------------------------------------------------------

/// An observer that records the publish-side hooks and every drop.
#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<String>>,
}

impl Recorder {
    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    fn count(&self, prefix: &str) -> usize {
        self.events()
            .iter()
            .filter(|e| e.starts_with(prefix))
            .count()
    }
}

impl Observer for Recorder {
    fn on_published(&self, addr: &str, receipt: &Receipt) {
        self.events.lock().unwrap().push(format!(
            "published {addr} {}",
            receipt.message_id.as_deref().unwrap_or("-")
        ));
    }

    fn on_send_failed(&self, addr: &str, error: &MessagingError) {
        self.events.lock().unwrap().push(format!(
            "failed {addr} handed_back={}",
            error.message().is_some()
        ));
    }

    fn on_dropped(&self, addr: &str, _msg: &Message, reason: DropReason) {
        self.events
            .lock()
            .unwrap()
            .push(format!("dropped {addr} {}", reason.as_str()));
    }
}

/// A provider that counts its lifecycle calls.
#[derive(Default)]
struct Counting {
    setups: AtomicUsize,
    closes: AtomicUsize,
}

impl Producer for Counting {
    fn send(
        &self,
        _addr: &str,
        _msg: Message,
        _opts: SendOptions,
    ) -> BoxFuture<'_, Result<(), MessagingError>> {
        Box::pin(async { Ok(()) })
    }
}

impl Receiver for Counting {}

impl Provider for Counting {
    fn schemes(&self) -> &[&str] {
        &["one", "two"]
    }

    fn setup(&self) -> BoxFuture<'_, Result<(), MessagingError>> {
        Box::pin(async {
            self.setups.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    fn close(&self) -> BoxFuture<'_, Result<(), MessagingError>> {
        Box::pin(async {
            self.closes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

fn ack_all() -> ferroly::messaging::Handler {
    handler(|_msg: Message| async { Ack::Ok })
}

#[tokio::test]
async fn a_publish_only_provider_needs_no_receiver_code() {
    let cloud = Arc::new(FakeCloud::default());
    let registry = ProviderRegistry::new();
    registry.register(cloud.clone()).unwrap();

    let err = registry
        .add_listener("fakecloud://q", ack_all(), Default::default())
        .await
        .unwrap_err();
    assert!(matches!(err, MessagingError::Unsupported(_)), "{err}");
    let err = registry
        .remove_listeners("fakecloud://q")
        .await
        .unwrap_err();
    assert!(matches!(err, MessagingError::Unsupported(_)), "{err}");

    registry
        .send("fakecloud://q", Message::text("hello"), Default::default())
        .await
        .unwrap();
    assert_eq!(cloud.accepted_bodies(), ["hello"]);

    // `setup`, `close` and `set_observer` have defaults too.
    let provider: Arc<dyn Provider> = cloud.clone();
    provider.setup().await.unwrap();
    provider.close().await.unwrap();
    let err = provider
        .set_observer(Arc::new(Recorder::default()))
        .unwrap_err();
    assert!(matches!(err, MessagingError::Unsupported(_)), "{err}");
}

#[tokio::test]
async fn a_provider_refuses_options_it_cannot_honour() {
    let cloud = FakeCloud::default();
    for opts in [
        SendOptions::new().delay(Duration::from_secs(1)),
        SendOptions::new().ttl(Duration::from_secs(1)),
        SendOptions::new().deliver_at(std::time::SystemTime::now()),
    ] {
        let err = cloud
            .send("fakecloud://q", Message::text("x"), opts)
            .await
            .unwrap_err();
        assert!(matches!(err, MessagingError::InvalidOptions(_)), "{err}");
    }
    let err = cloud
        .send(
            "fakecloud://q",
            Message::text("x"),
            SendOptions::new().delay(Duration::from_secs(1)),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("delay"), "{err}");
    assert!(cloud.accepted_bodies().is_empty());
    assert!(cloud.options.lock().unwrap().is_empty());
}

#[test]
fn unsupported_by_names_the_first_unsupported_option() {
    let caps = Capabilities::new().ttl(true);
    let both = SendOptions::new()
        .delay(Duration::from_secs(1))
        .ttl(Duration::from_secs(1));
    assert_eq!(both.unsupported_by(&caps), Some("delay"));
    let ttl_only = SendOptions::new().ttl(Duration::from_secs(1));
    assert_eq!(ttl_only.unsupported_by(&caps), None);
    let scheduled = SendOptions::new().deliver_at(std::time::SystemTime::now());
    assert_eq!(scheduled.unsupported_by(&caps), Some("deliver_at"));
    assert_eq!(
        SendOptions::new().unsupported_by(&Capabilities::new()),
        None
    );
    // Hints are never reported.
    let hints = SendOptions::new()
        .on_full(ferroly::messaging::OnFull::Block)
        .extension("other.thing", 1);
    assert_eq!(hints.unsupported_by(&Capabilities::new()), None);
}

#[tokio::test]
async fn receipts_reach_the_caller_through_the_registry() {
    let registry = ProviderRegistry::new();
    registry.register(Arc::new(FakeCloud::default())).unwrap();
    let first = registry
        .send_with_receipt("fakecloud://q", Message::text("a"), Default::default())
        .await
        .unwrap();
    assert_eq!(first.message_id.as_deref(), Some("cloud-1"));
    assert_eq!(first.sequence.as_deref(), Some("1"));
    let second = registry
        .send_with_receipt("fakecloud://q", Message::text("b"), Default::default())
        .await
        .unwrap();
    assert_eq!(second.message_id.as_deref(), Some("cloud-2"));
    assert_eq!(second.sequence.as_deref(), Some("2"));
}

#[tokio::test]
async fn the_default_receipt_carries_the_message_id() {
    let bus = LocalProvider::new("t");
    bus.add_listener("chan://q", ack_all(), Default::default())
        .await
        .unwrap();
    let receipt = bus
        .send_with_receipt(
            "chan://q",
            Message::text("x").with_id("m-9"),
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.message_id.as_deref(), Some("m-9"));
    assert_eq!(receipt.sequence, None);
    assert!(bus.shutdown(Duration::from_secs(5)).await.is_clean());
}

#[tokio::test]
async fn extensions_reach_the_provider() {
    let cloud = FakeCloud::default();
    cloud
        .send(
            "fakecloud://q",
            Message::text("x"),
            SendOptions::new().extension("fakecloud.group", "g1"),
        )
        .await
        .unwrap();
    let seen = cloud.options.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].extensions.get_str("fakecloud.group"), Some("g1"));
}

#[tokio::test]
async fn capabilities_are_reported_per_address() {
    let registry = ProviderRegistry::new();
    registry.register(Arc::new(FakeCloud::default())).unwrap();
    registry
        .register(Arc::new(LocalProvider::new("local")))
        .unwrap();
    let cloud = registry.capabilities_for("fakecloud://q").unwrap();
    assert_eq!(cloud.max_message_bytes, Some(1024));
    assert!(cloud.native_batch);
    assert!(!cloud.delay);
    let local = registry.capabilities_for("chan://q").unwrap();
    assert!(local.delay);
    assert!(local.filters);
    assert!(!local.native_batch);
    let err = registry.capabilities_for("sqs://q").unwrap_err();
    assert!(matches!(err, MessagingError::InvalidAddress(_)), "{err}");
}

#[tokio::test]
async fn registry_setup_and_close_reach_each_provider_once() {
    let counting = Arc::new(Counting::default());
    let registry = ProviderRegistry::new();
    registry.register(counting.clone()).unwrap();
    registry.register(Arc::new(FakeCloud::default())).unwrap();
    assert_eq!(registry.schemes(), ["fakecloud", "one", "two"]);
    assert_eq!(registry.providers().len(), 2);

    registry.setup_all().await.unwrap();
    registry.close_all().await.unwrap();
    assert_eq!(counting.setups.load(Ordering::SeqCst), 1);
    assert_eq!(counting.closes.load(Ordering::SeqCst), 1);

    // Neither provider takes an observer: both are skipped, not an error.
    registry
        .set_observer(Arc::new(Recorder::default()))
        .unwrap();
}

#[tokio::test]
async fn local_provider_reports_published_and_failed_sends() {
    let bus = LocalProvider::new("t");
    let recorder = Arc::new(Recorder::default());
    let provider: Arc<dyn Provider> = Arc::new(bus.clone());
    provider.set_observer(recorder.clone()).unwrap();

    let (tx, mut rx) = unbounded_channel();
    bus.add_listener(
        "chan://q",
        handler(move |_msg: Message| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(());
                Ack::Ok
            }
        }),
        Default::default(),
    )
    .await
    .unwrap();

    provider
        .send(
            "chan://q",
            Message::text("x").with_id("m-1"),
            Default::default(),
        )
        .await
        .unwrap();
    rx.recv().await.unwrap();
    assert_eq!(recorder.count("published"), 1);
    assert!(recorder
        .events()
        .contains(&"published chan://q m-1".to_string()));

    let err = provider
        .send("chan://nobody", Message::text("y"), Default::default())
        .await
        .unwrap_err();
    assert!(matches!(err, MessagingError::NoListeners(_)), "{err}");
    assert_eq!(recorder.count("failed"), 1);
    assert!(recorder
        .events()
        .contains(&"failed chan://nobody handed_back=true".to_string()));
    assert_eq!(recorder.count("published"), 1);
    drop(provider);
    assert!(bus.shutdown(Duration::from_secs(5)).await.is_clean());
}

#[tokio::test]
async fn topic_publish_fires_on_published_once() {
    let bus = LocalProvider::new("t");
    let recorder = Arc::new(Recorder::default());
    bus.set_observer(recorder.clone());
    bus.add_listener("topic://orders.*", ack_all(), Default::default())
        .await
        .unwrap();
    bus.add_listener("topic://orders.created", ack_all(), Default::default())
        .await
        .unwrap();
    bus.send(
        "topic://orders.created",
        Message::text("o-1"),
        Default::default(),
    )
    .await
    .unwrap();
    assert_eq!(recorder.count("published"), 1);
    assert_eq!(recorder.count("failed"), 0);
    assert!(bus.shutdown(Duration::from_secs(5)).await.is_clean());
}

// ---- subscription filters (WP4) ----------------------------------------------------

/// A handler that records each body it acks.
fn recording(log: &Arc<Mutex<Vec<String>>>) -> ferroly::messaging::Handler {
    let log = log.clone();
    handler(move |msg: Message| {
        let log = log.clone();
        async move {
            log.lock().unwrap().push(msg.body_str().into_owned());
            Ack::Ok
        }
    })
}

fn new_log() -> Arc<Mutex<Vec<String>>> {
    Arc::new(Mutex::new(Vec::new()))
}

fn logged(log: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    log.lock().unwrap().clone()
}

#[tokio::test]
async fn a_filtered_subscription_receives_only_matching_messages() {
    let bus = LocalProvider::new("t");
    let (eu, all) = (new_log(), new_log());
    bus.add_listener(
        "topic://orders.*",
        recording(&eu),
        ListenerOptions {
            filter: Some(Filter::eq("region", "eu")),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    bus.add_listener("topic://orders.*", recording(&all), Default::default())
        .await
        .unwrap();

    for region in ["eu", "us"] {
        let msg = Message::text(region).with_attribute("region", region);
        bus.send("topic://orders.created", msg, Default::default())
            .await
            .unwrap();
    }
    assert!(bus.shutdown(Duration::from_secs(5)).await.is_clean());
    assert_eq!(logged(&eu), ["eu"]);
    assert_eq!(logged(&all), ["eu", "us"]);
}

#[tokio::test]
async fn filters_see_properties_and_numbers() {
    let bus = LocalProvider::new("t");
    let log = new_log();
    bus.add_listener(
        "topic://orders.>",
        recording(&log),
        ListenerOptions {
            filter: Some(Filter::all([
                Filter::eq("event-type", "order.created"),
                Filter::gte("amount", 100.0),
            ])),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    for (body, amount) in [("big", 150i64), ("small", 50)] {
        let msg = Message::text(body)
            .with_event_type("order.created")
            .with_attribute("amount", amount);
        bus.send("topic://orders.eu", msg, Default::default())
            .await
            .unwrap();
    }
    // The property is required: an attribute alone of another name does not count.
    let no_event = Message::text("no-event").with_attribute("amount", 500);
    bus.send("topic://orders.eu", no_event, Default::default())
        .await
        .unwrap();
    assert!(bus.shutdown(Duration::from_secs(5)).await.is_clean());
    assert_eq!(logged(&log), ["big"]);
}

#[tokio::test]
async fn a_message_every_filter_excludes_is_dropped_as_filtered() {
    let bus = LocalProvider::new("t");
    let recorder = Arc::new(Recorder::default());
    bus.set_observer(recorder.clone());
    for region in ["eu", "apac"] {
        bus.add_listener(
            "topic://orders.*",
            ack_all(),
            ListenerOptions {
                filter: Some(Filter::eq("region", region)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    let msg = Message::text("x").with_attribute("region", "us");
    bus.send("topic://orders.created", msg, Default::default())
        .await
        .unwrap();
    assert!(recorder
        .events()
        .contains(&"dropped topic://orders.created filtered".to_string()));
    assert_eq!(recorder.count("published"), 1);

    // No matching pattern at all is still "no subscribers".
    bus.send(
        "topic://invoices.created",
        Message::text("y"),
        Default::default(),
    )
    .await
    .unwrap();
    assert!(recorder
        .events()
        .contains(&"dropped topic://invoices.created no_subscribers".to_string()));
    assert_eq!(DropReason::Filtered.as_str(), "filtered");
    assert!(bus.shutdown(Duration::from_secs(5)).await.is_clean());
}

#[tokio::test]
async fn a_filter_on_a_queue_listener_is_rejected() {
    let bus = LocalProvider::new("t");
    let err = bus
        .add_listener(
            "chan://q",
            ack_all(),
            ListenerOptions {
                filter: Some(Filter::exists("x")),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, MessagingError::InvalidOptions(_)), "{err}");
    // Nothing was registered.
    assert_eq!(bus.queue_depth("chan://q"), None);
}

#[tokio::test]
async fn a_shared_subscription_must_agree_on_its_filter() {
    let bus = LocalProvider::new("t");
    let shared = |filter: Filter| ListenerOptions {
        subscription: Some("billing".into()),
        filter: Some(filter),
        ..Default::default()
    };
    bus.add_listener(
        "topic://orders.*",
        ack_all(),
        shared(Filter::eq("region", "eu")),
    )
    .await
    .unwrap();
    let err = bus
        .add_listener(
            "topic://orders.*",
            ack_all(),
            shared(Filter::eq("region", "us")),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, MessagingError::InvalidOptions(_)), "{err}");
    assert!(err.to_string().contains("billing"), "{err}");
    bus.add_listener(
        "topic://orders.*",
        ack_all(),
        shared(Filter::eq("region", "eu")),
    )
    .await
    .unwrap();
    assert!(bus.shutdown(Duration::from_secs(5)).await.is_clean());
}

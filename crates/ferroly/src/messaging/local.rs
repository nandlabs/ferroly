//! [`LocalProvider`] — the in-process provider.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::sync::{oneshot, watch, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use ferroly::messaging::provider::{
    split_address, topic_matches, validate_pattern, validate_topic,
};
use ferroly::messaging::queue::{Envelope, PushError, Queue};
use ferroly::messaging::{
    headers, new_message_id, Ack, BoxFuture, DropReason, ExpiryAction, Handler, ListenerOptions,
    LocalOptions, Message, MessagingError, Observer, OnFull, Producer, Provider, Receiver,
    Rejected, SendOptions,
};

/// A message that a shutdown could not deliver, with the address of the
/// listener it was buffered for.
#[derive(Debug, Clone)]
pub struct Undelivered {
    /// The listener address (`chan://orders`, `topic://orders.*`).
    pub addr: String,
    /// The message, as it would have been handed to the handler.
    pub message: Message,
}

/// The outcome of [`LocalProvider::shutdown`].
#[derive(Debug, Default)]
pub struct DrainReport {
    /// Messages that were accepted but not completed by a handler before the
    /// drain timeout: still buffered, scheduled for later than the deadline,
    /// waiting out a retry backoff, or in a handler that had to be cancelled.
    /// Each has also been reported through
    /// [`Observer::on_dropped`] with [`DropReason::Shutdown`].
    pub undelivered: Vec<Undelivered>,
}

impl DrainReport {
    /// Whether every accepted message was completed.
    pub fn is_clean(&self) -> bool {
        self.undelivered.is_empty()
    }
}

/// State the consumer tasks share with the provider **without** keeping the
/// provider alive: it holds no routing table and no handlers.
struct Core {
    observer: Mutex<Option<Arc<dyn Observer>>>,
    /// Messages accepted and not yet completed (buffered, delayed or being
    /// handled). Shutdown waits for this to reach zero.
    pending: watch::Sender<usize>,
    /// In-flight messages given up on at shutdown, collected for the report.
    leftovers: Mutex<Vec<Undelivered>>,
}

impl Core {
    /// Calls the observer (if any) outside every lock, containing a panic.
    fn notify(&self, f: impl FnOnce(&dyn Observer)) {
        let observer = self.observer.lock().unwrap().clone();
        if let Some(observer) = observer {
            let _ = catch_unwind(AssertUnwindSafe(|| f(observer.as_ref())));
        }
    }

    fn accepted(&self) {
        self.pending.send_modify(|n| *n += 1);
    }

    fn completed(&self) {
        self.pending.send_modify(|n| *n = n.saturating_sub(1));
    }

    /// Gives up on a message at shutdown: report it and keep it for the
    /// [`DrainReport`].
    fn abandon(&self, addr: &str, message: Message) {
        self.notify(|o| o.on_dropped(addr, &message, DropReason::Shutdown));
        self.leftovers.lock().unwrap().push(Undelivered {
            addr: addr.to_string(),
            message,
        });
        self.completed();
    }
}

/// The listeners that compete for one stream of messages.
#[derive(Default)]
struct Group {
    listeners: Vec<Arc<Queue>>,
    round_robin: usize,
    depth: Arc<AtomicUsize>,
}

impl Group {
    /// Picks the listener for a message: the same routing key always maps to
    /// the same listener (while the listener set is unchanged); without a key
    /// the listeners take turns.
    fn pick(&mut self, key: Option<&str>) -> Option<Arc<Queue>> {
        // Forget listeners whose queue is closed (removed, or their consumer
        // ended), so they are never selected.
        self.listeners.retain(|q| !q.is_closed());
        if self.listeners.is_empty() {
            return None;
        }
        let index = match key {
            Some(key) => hash_key(key) % self.listeners.len(),
            None => {
                let i = self.round_robin % self.listeners.len();
                self.round_robin = self.round_robin.wrapping_add(1);
                i
            }
        };
        Some(self.listeners[index].clone())
    }
}

fn hash_key(key: &str) -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    h.finish() as usize
}

/// One `topic://` subscription: a pattern and the listeners sharing it.
struct TopicSub {
    pattern: String,
    /// `Some` for a named (shared) subscription.
    name: Option<String>,
    group: Group,
}

/// The most recent message ids accepted for one destination.
#[derive(Default)]
struct DedupWindow {
    order: VecDeque<String>,
    seen: HashSet<String>,
}

impl DedupWindow {
    /// Records `id`; returns `false` if it is already in the window.
    fn insert(&mut self, id: &str, capacity: usize) -> bool {
        if self.seen.contains(id) {
            return false;
        }
        while self.order.len() >= capacity {
            match self.order.pop_front() {
                Some(old) => self.seen.remove(&old),
                None => break,
            };
        }
        self.order.push_back(id.to_string());
        self.seen.insert(id.to_string());
        true
    }

    fn remove(&mut self, id: &str) {
        if self.seen.remove(id) {
            self.order.retain(|x| x != id);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Accepting sends and listeners.
    Open,
    /// `shutdown` is waiting for buffered messages; only internal
    /// (dead-letter) sends are accepted.
    Draining,
    /// Shut down; `start` reopens it.
    Stopped,
}

struct Routing {
    phase: Phase,
    /// `chan://` destinations: one group of competing listeners each.
    queues: HashMap<String, Group>,
    /// `topic://` subscriptions.
    topics: Vec<TopicSub>,
    /// De-duplication windows, keyed by canonical destination address.
    dedup: HashMap<String, DedupWindow>,
}

/// Where an address points.
enum Target {
    Queue(String),
    Topic(String),
    Reply(String),
}

impl Target {
    fn parse(addr: &str) -> Result<Self, MessagingError> {
        let (scheme, name) = split_address(addr);
        if name.is_empty() {
            return Err(MessagingError::InvalidAddress(addr.to_string()));
        }
        match scheme {
            None | Some("chan") => Ok(Target::Queue(name.to_string())),
            Some("topic") => Ok(Target::Topic(name.to_string())),
            Some("reply") => Ok(Target::Reply(name.to_string())),
            Some(_) => Err(MessagingError::InvalidAddress(format!(
                "{addr} (LocalProvider handles chan://, topic:// and reply://)"
            ))),
        }
    }

    fn canonical(&self) -> String {
        match self {
            Target::Queue(name) => format!("chan://{name}"),
            Target::Topic(name) => format!("topic://{name}"),
            Target::Reply(name) => format!("reply://{name}"),
        }
    }
}

/// Who is sending.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// A caller of the public API: refused once the provider is draining, and
    /// subject to de-duplication.
    External,
    /// The provider itself, moving a message to a dead-letter destination:
    /// still accepted while draining.
    Internal,
}

/// The result of resolving an address against the routing table.
enum Route {
    /// Deliver to these listener buffers (one per matching subscription).
    Queues {
        targets: Vec<Arc<Queue>>,
        /// The canonical destination address.
        addr: String,
        /// The topic name to stamp on each copy, for `topic://` sends.
        topic: Option<String>,
        /// The id recorded in the de-duplication window, to undo on failure.
        dedup_id: Option<String>,
    },
    /// Complete a pending request.
    Reply(oneshot::Sender<Message>),
    /// Drop the message: already seen, or nobody is subscribed.
    Discard { addr: String, reason: DropReason },
}

struct Inner {
    id: String,
    options: LocalOptions,
    core: Arc<Core>,
    routing: Mutex<Routing>,
    /// Every live consumer with the queue it serves — including listeners that
    /// were removed and are still finishing their backlog.
    consumers: Mutex<Vec<(Arc<Queue>, JoinHandle<()>)>>,
    /// Pending requests, keyed by reply inbox.
    replies: Mutex<HashMap<String, oneshot::Sender<Message>>>,
    /// Held for the length of a shutdown, so shutdowns run one at a time.
    stop_lock: tokio::sync::Mutex<()>,
}

impl Inner {
    /// Resolves `addr` to the buffers `msg` should go to. Synchronous: nothing
    /// awaits while the routing table is locked. On failure the message is
    /// handed back inside the error.
    fn route(
        &self,
        addr: &str,
        msg: Message,
        opts: &SendOptions,
        origin: Origin,
    ) -> Result<(Route, Message), MessagingError> {
        let target = Target::parse(addr)?;
        let canonical = target.canonical();
        if let Target::Topic(name) = &target {
            validate_topic(name)?;
        }

        let mut routing = self.routing.lock().unwrap();
        if let Target::Reply(inbox) = &target {
            // A reply only completes a request that is already waiting, so it
            // is still accepted while the provider drains: a handler that
            // finishes during shutdown can answer its requester.
            let stopped = routing.phase == Phase::Stopped;
            drop(routing);
            let waiting = match stopped {
                true => None,
                false => self.replies.lock().unwrap().remove(inbox),
            };
            return match waiting {
                Some(tx) => Ok((Route::Reply(tx), msg)),
                None if stopped => Err(MessagingError::Closed(Rejected::new(canonical, msg))),
                None => Err(MessagingError::NoListeners(Rejected::new(canonical, msg))),
            };
        }
        let open = match origin {
            Origin::External => routing.phase == Phase::Open,
            Origin::Internal => routing.phase != Phase::Stopped,
        };
        if !open {
            return Err(MessagingError::Closed(Rejected::new(canonical, msg)));
        }

        // De-duplicate caller sends by message id.
        let window = self.options.dedup_window;
        let dedup_id = match (origin, window > 0, msg.id()) {
            (Origin::External, true, Some(id)) => Some(id.to_string()),
            _ => None,
        };
        if let Some(id) = &dedup_id {
            let fresh = routing
                .dedup
                .entry(canonical.clone())
                .or_default()
                .insert(id, window);
            if !fresh {
                let reason = DropReason::Duplicate;
                return Ok((
                    Route::Discard {
                        addr: canonical,
                        reason,
                    },
                    msg,
                ));
            }
        }

        let key = opts.routing_key.as_deref().or(msg.routing_key.as_deref());
        let (targets, topic) = match &target {
            Target::Queue(name) => {
                let picked = routing.queues.get_mut(name).and_then(|g| g.pick(key));
                (picked.into_iter().collect::<Vec<_>>(), None)
            }
            Target::Topic(name) => {
                let picked = routing
                    .topics
                    .iter_mut()
                    .filter(|sub| topic_matches(&sub.pattern, name))
                    .filter_map(|sub| sub.group.pick(key))
                    .collect();
                (picked, Some(name.clone()))
            }
            Target::Reply(_) => unreachable!("handled above"),
        };

        if targets.is_empty() {
            // Nothing took the message: do not let its id block a later retry.
            if let (Some(id), Some(seen)) = (&dedup_id, routing.dedup.get_mut(&canonical)) {
                seen.remove(id);
            }
            return match target {
                // Publishing to a topic nobody subscribes to is not an error.
                Target::Topic(_) => Ok((
                    Route::Discard {
                        addr: canonical,
                        reason: DropReason::NoSubscribers,
                    },
                    msg,
                )),
                _ => Err(MessagingError::NoListeners(Rejected::new(canonical, msg))),
            };
        }
        Ok((
            Route::Queues {
                targets,
                addr: canonical,
                topic,
                dedup_id,
            },
            msg,
        ))
    }

    /// Forgets a de-duplication entry after a failed delivery, so the caller's
    /// retry of the same message is not mistaken for a duplicate.
    fn forget_dedup(&self, addr: &str, id: &str) {
        if let Some(seen) = self.routing.lock().unwrap().dedup.get_mut(addr) {
            seen.remove(id);
        }
    }

    /// The whole send path for a caller of the public API.
    async fn send(
        &self,
        addr: &str,
        msg: Message,
        opts: SendOptions,
    ) -> Result<(), MessagingError> {
        let (route, msg) = self.route(addr, msg, &opts, Origin::External)?;
        match route {
            Route::Reply(tx) => tx.send(msg).map_err(|msg| {
                // The requester stopped waiting between lookup and delivery.
                MessagingError::NoListeners(Rejected::new(addr, msg))
            }),
            Route::Discard { addr, reason } => {
                self.core.notify(|o| o.on_dropped(&addr, &msg, reason));
                Ok(())
            }
            Route::Queues {
                mut targets,
                addr,
                topic: None,
                dedup_id,
            } => {
                // A queue: exactly one listener gets the message; a failure is
                // the caller's to handle, with the message handed back.
                let queue = targets.remove(0);
                let result = enqueue(&self.core, &queue, msg, &opts).await;
                if let (Err(_), Some(id)) = (&result, &dedup_id) {
                    self.forget_dedup(&addr, id);
                }
                result.map_err(|(kind, msg)| kind.into_error(Rejected::new(addr, msg)))
            }
            Route::Queues {
                targets,
                topic: Some(topic),
                ..
            } => {
                // A topic: every matching subscription gets its own copy. A
                // subscription that cannot take it misses the message (reported
                // through the observer); the others are unaffected.
                let mut msg = msg;
                if msg.topic().is_none() {
                    msg.set_header(headers::TOPIC, topic);
                }
                let last = targets.len() - 1;
                let mut original = Some(msg);
                for (i, queue) in targets.iter().enumerate() {
                    let copy = if i == last {
                        original.take()
                    } else {
                        original.clone()
                    };
                    let Some(copy) = copy else { break };
                    if let Err((kind, lost)) = enqueue(&self.core, queue, copy, &opts).await {
                        let reason = match kind {
                            Refusal::Closed => DropReason::Shutdown,
                            Refusal::Full | Refusal::TimedOut => DropReason::BufferFull,
                        };
                        self.core
                            .notify(|o| o.on_dropped(&queue.addr, &lost, reason));
                    }
                }
                Ok(())
            }
        }
    }

    /// Stops the consumers at once and reports what was still buffered. Used
    /// when the last handle is dropped without a `shutdown`.
    fn abandon_all(&mut self) {
        let consumers = std::mem::take(
            self.consumers
                .get_mut()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for (queue, _task) in consumers {
            queue.abort();
            for env in queue.take_all() {
                self.core
                    .notify(|o| o.on_dropped(&queue.addr, &env.msg, DropReason::Shutdown));
                self.core.completed();
            }
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Every `LocalProvider` handle is gone. The consumer tasks hold only a
        // weak reference, so this runs even while they are alive; aborting
        // their queues makes each of them finish instead of leaking.
        self.abandon_all();
    }
}

/// Why a buffer did not take a message.
enum Refusal {
    Full,
    Closed,
    TimedOut,
}

impl Refusal {
    fn into_error(self, rejected: Rejected) -> MessagingError {
        match self {
            Refusal::Full => MessagingError::BufferFull(rejected),
            Refusal::Closed => MessagingError::Closed(rejected),
            Refusal::TimedOut => MessagingError::SendTimeout(rejected),
        }
    }
}

/// Puts `msg` into `queue`, honouring the send options. On refusal the message
/// is handed back. Needs only the [`Core`], so a consumer can dead-letter
/// without holding the provider alive while it waits for room.
async fn enqueue(
    core: &Core,
    queue: &Queue,
    msg: Message,
    opts: &SendOptions,
) -> Result<(), (Refusal, Message)> {
    let now = Instant::now();
    let env = Envelope {
        msg,
        expires_at: opts.ttl.map(|ttl| now + ttl),
    };
    let due = opts.delay.map(|delay| now + delay);
    // Keep a copy for the observer only when there is one to tell.
    let observed = core
        .observer
        .lock()
        .unwrap()
        .is_some()
        .then(|| env.msg.clone());

    // Count the message before it becomes visible, so a shutdown can never see
    // "nothing pending" while a consumer is already working on it.
    core.accepted();
    match queue.push(env, due, opts.full_policy()).await {
        Ok(evicted) => {
            if let Some(observed) = &observed {
                core.notify(|o| o.on_send(&queue.addr, observed));
            }
            if let Some(evicted) = evicted {
                core.notify(|o| o.on_dropped(&queue.addr, &evicted.msg, DropReason::Evicted));
                core.completed();
            }
            core.notify(|o| o.on_queue_depth(&queue.addr, queue.depth()));
            Ok(())
        }
        Err(refused) => {
            core.completed();
            Err(match refused {
                PushError::Full(env) => (Refusal::Full, env.msg),
                PushError::Closed(env) => (Refusal::Closed, env.msg),
                PushError::TimedOut(env) => (Refusal::TimedOut, env.msg),
            })
        }
    }
}

/// An in-process [`Provider`] over bounded in-memory buffers.
///
/// - `chan://name` is a **queue**: its listeners are competing consumers (each
///   message goes to one), with routing-key affinity (same key → same
///   listener).
/// - `topic://name` is **publish/subscribe**: every subscription whose pattern
///   matches receives its own copy; `*` and `>` are wildcards
///   ([`topic_matches`]).
///
/// Delivery is **at-least-once within the life of the process**: a message
/// that was accepted is handed to a handler until it is acked, rejected or
/// dead-lettered — a handler error or panic leads to a redelivery, not a loss.
/// Nothing is written to disk: a message that has not been completed when the
/// process exits is gone, and nothing crosses a process boundary.
///
/// The handle is cheap to clone. Consumer tasks hold only a weak reference, so
/// dropping every handle ends them; call [`shutdown`](Self::shutdown) (or
/// `stop` through the lifecycle manager) first to drain what is buffered. A
/// handler that needs to send should capture a
/// [`downgrade`](Self::downgrade)d handle rather than a clone, or it will keep
/// the provider alive.
#[derive(Clone)]
pub struct LocalProvider {
    inner: Arc<Inner>,
}

/// A handle to a [`LocalProvider`] that does not keep it alive. Obtain one with
/// [`LocalProvider::downgrade`]; use it inside handlers that send messages.
#[derive(Clone)]
pub struct WeakLocalProvider {
    inner: Weak<Inner>,
}

impl WeakLocalProvider {
    /// The provider, if a strong handle to it still exists.
    pub fn upgrade(&self) -> Option<LocalProvider> {
        self.inner.upgrade().map(|inner| LocalProvider { inner })
    }
}

impl LocalProvider {
    /// Creates a provider with the given component id and default
    /// [`LocalOptions`].
    pub fn new(id: impl Into<String>) -> Self {
        Self::with_options(id, LocalOptions::default())
    }

    /// Creates a provider with explicit options.
    pub fn with_options(id: impl Into<String>, options: LocalOptions) -> Self {
        Self {
            inner: Arc::new(Inner {
                id: id.into(),
                options,
                core: Arc::new(Core {
                    observer: Mutex::new(None),
                    pending: watch::channel(0).0,
                    leftovers: Mutex::new(Vec::new()),
                }),
                routing: Mutex::new(Routing {
                    phase: Phase::Open,
                    queues: HashMap::new(),
                    topics: Vec::new(),
                    dedup: HashMap::new(),
                }),
                consumers: Mutex::new(Vec::new()),
                replies: Mutex::new(HashMap::new()),
                stop_lock: tokio::sync::Mutex::new(()),
            }),
        }
    }

    /// The options this provider was created with.
    pub fn options(&self) -> &LocalOptions {
        &self.inner.options
    }

    /// Installs an [`Observer`] (replacing any previous one).
    pub fn set_observer(&self, observer: Arc<dyn Observer>) {
        *self.inner.core.observer.lock().unwrap() = Some(observer);
    }

    /// A handle that does not keep the provider alive.
    pub fn downgrade(&self) -> WeakLocalProvider {
        WeakLocalProvider {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// The number of messages accepted and not yet completed across all
    /// destinations (buffered, delayed, or being handled).
    pub fn pending(&self) -> usize {
        *self.inner.core.pending.borrow()
    }

    /// The number of messages buffered for `addr` (not counting ones being
    /// handled), or `None` if nothing listens there. For a `topic://` pattern
    /// it is the total across the subscriptions registered with exactly that
    /// pattern.
    pub fn queue_depth(&self, addr: &str) -> Option<usize> {
        let target = Target::parse(addr).ok()?;
        let routing = self.inner.routing.lock().unwrap();
        match target {
            Target::Queue(name) => routing
                .queues
                .get(&name)
                .map(|group| group.depth.load(Ordering::Relaxed)),
            Target::Topic(pattern) => {
                let depths: Vec<usize> = routing
                    .topics
                    .iter()
                    .filter(|sub| sub.pattern == pattern)
                    .map(|sub| sub.group.depth.load(Ordering::Relaxed))
                    .collect();
                (!depths.is_empty()).then(|| depths.iter().sum())
            }
            Target::Reply(_) => None,
        }
    }

    /// Drains and stops the provider.
    ///
    /// 1. New sends and listeners are refused with
    ///    [`MessagingError::Closed`] (a send gets its message back).
    /// 2. Consumers keep working until every accepted message has been
    ///    completed — acked, rejected or dead-lettered, retries and their
    ///    backoff included — or `drain_timeout` has passed. Messages scheduled
    ///    for later than the deadline are not waited for.
    /// 3. Consumers are stopped and awaited. A handler still running at the
    ///    deadline is cancelled at its next `.await`.
    ///
    /// Whatever could not be completed is **not dropped silently**: each such
    /// message is reported through [`Observer::on_dropped`] with
    /// [`DropReason::Shutdown`] and returned in the [`DrainReport`], so the
    /// caller can persist or re-route it.
    ///
    /// Afterwards the provider is stopped and has no listeners; the lifecycle
    /// `start` reopens it. Calling `shutdown` on a stopped provider returns an
    /// empty report.
    pub async fn shutdown(&self, drain_timeout: Duration) -> DrainReport {
        let inner = &self.inner;
        let core = &inner.core;
        let _serial = inner.stop_lock.lock().await;

        {
            let mut routing = inner.routing.lock().unwrap();
            if routing.phase == Phase::Stopped {
                return DrainReport::default();
            }
            routing.phase = Phase::Draining;
        }
        // No listener can be added from here on, so this is every consumer.
        let (queues, consumers): (Vec<Arc<Queue>>, Vec<JoinHandle<()>>) =
            std::mem::take(&mut *inner.consumers.lock().unwrap())
                .into_iter()
                .unzip();
        let deadline = Instant::now() + drain_timeout;
        let mut unhandled: Vec<Undelivered> = Vec::new();

        // Messages that only become due after the deadline cannot complete.
        for queue in &queues {
            for env in queue.take_delayed_after(deadline) {
                core.completed();
                unhandled.push(Undelivered {
                    addr: queue.addr.clone(),
                    message: env.msg,
                });
            }
        }

        // Let the consumers finish what was accepted.
        let mut pending = core.pending.subscribe();
        let _ = tokio::time::timeout_at(deadline, pending.wait_for(|n| *n == 0)).await;

        // Stop the consumers and wait for them. An idle one sees its queue end;
        // a busy one cancels its handler and hands the message over.
        for queue in &queues {
            queue.abort();
        }
        for consumer in consumers {
            let _ = consumer.await;
        }

        for queue in &queues {
            for env in queue.take_all() {
                core.completed();
                unhandled.push(Undelivered {
                    addr: queue.addr.clone(),
                    message: env.msg,
                });
            }
        }
        for lost in &unhandled {
            core.notify(|o| o.on_dropped(&lost.addr, &lost.message, DropReason::Shutdown));
        }
        // In-flight messages were reported by the consumer that gave them up.
        unhandled.append(&mut core.leftovers.lock().unwrap());

        {
            let mut routing = inner.routing.lock().unwrap();
            routing.queues.clear();
            routing.topics.clear();
            routing.dedup.clear();
            routing.phase = Phase::Stopped;
        }
        // Dropping the reply slots fails the requests still waiting.
        inner.replies.lock().unwrap().clear();

        DrainReport {
            undelivered: unhandled,
        }
    }

    /// Sends `msg` to `addr` and waits up to `timeout` for a reply.
    ///
    /// The request is given a message id and correlation id if it has none, a
    /// private `reply://…` address in its [`headers::REPLY_TO`] header, and a
    /// time to live equal to `timeout` (so it is not handled after the caller
    /// has given up). The listener answers with [`reply`](Self::reply) or by
    /// being built with [`responder`](Self::responder). Returns
    /// [`MessagingError::RequestTimeout`] if no reply arrives in time.
    ///
    /// ```
    /// use std::time::Duration;
    /// use ferroly::messaging::{LocalProvider, Message, Receiver};
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), ferroly::messaging::MessagingError> {
    /// let bus = LocalProvider::new("bus");
    /// let echo = bus.responder(|request| async move {
    ///     Ok(Message::text(format!("echo: {}", request.body_str())))
    /// });
    /// bus.add_listener("chan://echo", echo, Default::default()).await?;
    ///
    /// let reply = bus
    ///     .request("chan://echo", Message::text("hi"), Duration::from_secs(1))
    ///     .await?;
    /// assert_eq!(reply.body_str(), "echo: hi");
    /// # Ok(())
    /// # }
    /// ```
    pub async fn request(
        &self,
        addr: &str,
        msg: Message,
        timeout: Duration,
    ) -> Result<Message, MessagingError> {
        /// Removes the reply slot when the request ends, however it ends.
        struct Slot<'a> {
            inner: &'a Inner,
            inbox: String,
        }
        impl Drop for Slot<'_> {
            fn drop(&mut self) {
                self.inner.replies.lock().unwrap().remove(&self.inbox);
            }
        }

        let mut msg = msg;
        if msg.id().is_none() {
            msg = msg.with_new_id();
        }
        if msg.correlation_id().is_none() {
            let id = msg.id().unwrap_or_default().to_string();
            msg = msg.with_correlation_id(id);
        }
        let inbox = new_message_id();
        let msg = msg.with_reply_to(format!("reply://{inbox}"));

        let (tx, rx) = oneshot::channel();
        self.inner.replies.lock().unwrap().insert(inbox.clone(), tx);
        let _slot = Slot {
            inner: &self.inner,
            inbox,
        };

        let deadline = Instant::now() + timeout;
        let timed_out = || MessagingError::RequestTimeout(addr.to_string());
        let opts = SendOptions::new()
            .ttl(timeout)
            .on_full(OnFull::BlockFor(timeout));
        match tokio::time::timeout_at(deadline, self.inner.send(addr, msg, opts)).await {
            Ok(sent) => sent?,
            Err(_) => return Err(timed_out()),
        }
        match tokio::time::timeout_at(deadline, rx).await {
            Ok(Ok(reply)) => Ok(reply),
            // The slot was dropped: the provider was shut down.
            Ok(Err(_)) => Err(MessagingError::Closed(Rejected::without_message(addr))),
            Err(_) => Err(timed_out()),
        }
    }

    /// Sends `reply` to the address in `request`'s [`headers::REPLY_TO`]
    /// header, after marking it as a reply ([`Message::in_reply_to`]: the
    /// correlation id and trace context are carried over).
    ///
    /// Fails with [`MessagingError::InvalidAddress`] if the request has no
    /// reply address, and [`MessagingError::NoListeners`] if the requester is
    /// no longer waiting.
    pub async fn reply(&self, request: &Message, reply: Message) -> Result<(), MessagingError> {
        let Some(to) = request.reply_to() else {
            return Err(MessagingError::InvalidAddress(
                "the request has no reply-to header".to_string(),
            ));
        };
        self.inner
            .send(to, reply.in_reply_to(request), SendOptions::default())
            .await
    }

    /// Builds a [`Handler`] for the answering side of
    /// [`request`](Self::request): `f` turns a request into `Ok(reply)` — which
    /// is sent to the request's reply address and acked — or `Err(ack)` to
    /// retry or reject the request without replying.
    ///
    /// A request without a reply address is still handled; the reply is simply
    /// discarded. So is a reply the requester no longer waits for. The handler
    /// holds the provider weakly, so it does not keep it alive.
    pub fn responder<F, Fut>(&self, f: F) -> Handler
    where
        F: Fn(Message) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Message, Ack>> + Send + 'static,
    {
        let bus = self.downgrade();
        Arc::new(move |msg: Message| {
            let bus = bus.clone();
            // Only the headers are needed to address the reply.
            let request = Message {
                headers: msg.headers.clone(),
                ..Message::default()
            };
            let work = f(msg);
            Box::pin(async move {
                match work.await {
                    Ok(reply) => {
                        if request.reply_to().is_some() {
                            if let Some(bus) = bus.upgrade() {
                                let _ = bus.reply(&request, reply).await;
                            }
                        }
                        Ack::Ok
                    }
                    Err(ack) => ack,
                }
            })
        })
    }

    fn register(
        &self,
        addr: &str,
        handler: Handler,
        opts: ListenerOptions,
    ) -> Result<(), MessagingError> {
        let target = Target::parse(addr)?;
        if opts.buffer_size == 0 {
            return Err(MessagingError::InvalidOptions(
                "buffer_size must be at least 1".to_string(),
            ));
        }
        if opts.buffer_size > Semaphore::MAX_PERMITS {
            return Err(MessagingError::InvalidOptions(format!(
                "buffer_size must be at most {}",
                Semaphore::MAX_PERMITS
            )));
        }
        let canonical = target.canonical();
        if let Some(dead_letter) = &opts.dead_letter {
            match Target::parse(dead_letter)? {
                Target::Reply(_) => {
                    return Err(MessagingError::InvalidOptions(
                        "dead_letter cannot be a reply:// address".to_string(),
                    ))
                }
                Target::Topic(name) => validate_topic(&name)?,
                dlq @ Target::Queue(_) => {
                    if dlq.canonical() == canonical {
                        return Err(MessagingError::InvalidOptions(format!(
                            "dead_letter of {canonical} points back at itself"
                        )));
                    }
                }
            }
        }

        let inner = &self.inner;
        let mut routing = inner.routing.lock().unwrap();
        if routing.phase != Phase::Open {
            return Err(MessagingError::Closed(Rejected::without_message(canonical)));
        }
        let group = match &target {
            Target::Queue(name) => routing.queues.entry(name.clone()).or_default(),
            Target::Topic(pattern) => {
                validate_pattern(pattern)?;
                let name = opts.subscription.clone();
                let existing = match &name {
                    // Listeners sharing a named subscription compete for it.
                    Some(_) => routing
                        .topics
                        .iter()
                        .position(|sub| sub.pattern == *pattern && sub.name == name),
                    // An unnamed listener is always a subscription of its own.
                    None => None,
                };
                let index = match existing {
                    Some(index) => index,
                    None => {
                        routing.topics.push(TopicSub {
                            pattern: pattern.clone(),
                            name,
                            group: Group::default(),
                        });
                        routing.topics.len() - 1
                    }
                };
                &mut routing.topics[index].group
            }
            Target::Reply(_) => {
                return Err(MessagingError::InvalidAddress(format!(
                    "{addr} (reply:// addresses are created by `request`)"
                )))
            }
        };

        let queue = Queue::new(canonical, opts.buffer_size, group.depth.clone());
        group.listeners.push(queue.clone());

        let consumer = tokio::spawn(consume(Arc::new(Consumer {
            core: inner.core.clone(),
            provider: Arc::downgrade(inner),
            queue: queue.clone(),
            handler,
            opts,
        })));
        // Recorded while the routing table is still locked, so a concurrent
        // `shutdown` (which flips the phase under that lock) either refuses
        // this listener or finds its consumer in the list.
        let mut consumers = inner.consumers.lock().unwrap();
        consumers.retain(|(_, task)| !task.is_finished());
        consumers.push((queue, consumer));
        drop(consumers);
        drop(routing);
        Ok(())
    }

    fn unregister(&self, addr: &str) -> Result<usize, MessagingError> {
        let target = Target::parse(addr)?;
        let removed: Vec<Arc<Queue>> = {
            let mut routing = self.inner.routing.lock().unwrap();
            match &target {
                Target::Queue(name) => routing
                    .queues
                    .remove(name)
                    .map(|group| group.listeners)
                    .unwrap_or_default(),
                Target::Topic(pattern) => {
                    let (gone, kept): (Vec<TopicSub>, Vec<TopicSub>) =
                        std::mem::take(&mut routing.topics)
                            .into_iter()
                            .partition(|sub| sub.pattern == *pattern);
                    routing.topics = kept;
                    gone.into_iter()
                        .flat_map(|sub| sub.group.listeners)
                        .collect()
                }
                Target::Reply(_) => Vec::new(),
            }
        };
        // Closing (not aborting) lets each consumer finish what is buffered.
        for queue in &removed {
            queue.close();
        }
        Ok(removed.len())
    }
}

impl Producer for LocalProvider {
    fn send(
        &self,
        addr: &str,
        msg: Message,
        opts: SendOptions,
    ) -> BoxFuture<'_, Result<(), MessagingError>> {
        let addr = addr.to_string();
        Box::pin(async move { self.inner.send(&addr, msg, opts).await })
    }
}

impl Receiver for LocalProvider {
    fn add_listener(
        &self,
        addr: &str,
        handler: Handler,
        opts: ListenerOptions,
    ) -> BoxFuture<'_, Result<(), MessagingError>> {
        let addr = addr.to_string();
        Box::pin(async move { self.register(&addr, handler, opts) })
    }

    fn remove_listeners(&self, addr: &str) -> BoxFuture<'_, Result<usize, MessagingError>> {
        let addr = addr.to_string();
        Box::pin(async move { self.unregister(&addr) })
    }
}

impl Provider for LocalProvider {
    fn schemes(&self) -> &[&str] {
        &["chan", "topic", "reply"]
    }

    fn setup(&self) -> BoxFuture<'_, Result<(), MessagingError>> {
        Box::pin(async { Ok(()) })
    }

    /// Drains with [`LocalOptions::drain_timeout`]; fails with
    /// [`MessagingError::Undelivered`] if messages were left over.
    fn close(&self) -> BoxFuture<'_, Result<(), MessagingError>> {
        Box::pin(async move {
            let report = self.shutdown(self.inner.options.drain_timeout).await;
            match report.undelivered.len() {
                0 => Ok(()),
                n => Err(MessagingError::Undelivered(n)),
            }
        })
    }
}

// ---- consumers -----------------------------------------------------------

/// Everything one listener's consumer task needs. It holds the provider only
/// weakly: it is reached for solely to resolve a dead-letter destination.
struct Consumer {
    core: Arc<Core>,
    provider: Weak<Inner>,
    queue: Arc<Queue>,
    handler: Handler,
    opts: ListenerOptions,
}

/// Aborts the wrapped task when dropped.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Aborts a queue when the consumer that served it ends, so it is never picked
/// for another message.
struct RetireQueue(Arc<Queue>);

impl Drop for RetireQueue {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Completes once the queue has been aborted.
async fn aborted(signal: &mut watch::Receiver<bool>) {
    let _ = signal.wait_for(|stop| *stop).await;
}

fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// The consumer task: takes messages off the listener's queue and runs each to
/// completion — serially, or up to `concurrency` at a time.
async fn consume(consumer: Arc<Consumer>) {
    let _retire = RetireQueue(consumer.queue.clone());
    // Bounded so the permit count fits both the semaphore and `acquire_many`.
    let most = Semaphore::MAX_PERMITS.min(u32::MAX as usize);
    let workers = consumer.opts.concurrency.map(|n| n.clamp(1, most));
    let permits = workers.map(|n| Arc::new(Semaphore::new(n)));
    let mut stop = consumer.queue.aborted();

    while let Some(env) = consumer.queue.pop().await {
        consumer.core.notify(|o| {
            o.on_queue_depth(&consumer.queue.addr, consumer.queue.depth());
        });
        match &permits {
            // Serial: the next message is not taken until this one is done,
            // retries included, which is what preserves order.
            None => consumer.process(env, &mut stop).await,
            Some(permits) => {
                let permit = tokio::select! {
                    permit = permits.clone().acquire_owned() => permit.ok(),
                    _ = aborted(&mut stop) => None,
                };
                let Some(permit) = permit else {
                    consumer.core.abandon(&consumer.queue.addr, env.msg);
                    break;
                };
                let worker = consumer.clone();
                let mut stop = consumer.queue.aborted();
                tokio::spawn(async move {
                    worker.process(env, &mut stop).await;
                    // Release the handler before the permit: the consumer
                    // waits on the permits, so once it finishes nothing still
                    // holds the handler.
                    drop(worker);
                    drop(permit);
                });
            }
        }
    }

    // Wait for the workers still running (they stop promptly once aborted).
    if let (Some(permits), Some(n)) = (&permits, workers) {
        let _ = permits.acquire_many(n as u32).await;
    }
}

/// How one handler invocation ended.
enum Attempt {
    Done(Ack),
    /// The provider is stopping and the handler was cancelled.
    Cancelled,
}

impl Consumer {
    /// Runs one message to completion: deliver, and on `Retry` (or a panic)
    /// wait out the backoff and deliver again, until it is acked, rejected,
    /// dead-lettered, expired — or given up on because the provider stopped.
    async fn process(&self, mut env: Envelope, stop: &mut watch::Receiver<bool>) {
        let addr = self.queue.addr.as_str();
        let core = &self.core;
        loop {
            if env.is_expired(Instant::now()) {
                return self.expire(env, stop).await;
            }
            core.notify(|o| o.on_receive(addr, &env.msg));
            let began = Instant::now();
            let ack = match self.invoke(&env.msg, stop).await {
                Attempt::Done(ack) => ack,
                Attempt::Cancelled => return core.abandon(addr, env.msg),
            };
            core.notify(|o| o.on_handled(addr, &env.msg, began.elapsed(), ack));

            match ack {
                Ack::Ok => {
                    core.notify(|o| o.on_ack(addr, &env.msg));
                    return core.completed();
                }
                Ack::Reject => {
                    core.notify(|o| o.on_nack(addr, &env.msg, false));
                    return core.completed();
                }
                Ack::Retry => {}
            }

            env.msg.delivery_count += 1;
            let attempts_left = env.msg.delivery_count < self.opts.max_delivery_attempts;
            // Report the *actual* fate: requeue only when attempts remain.
            core.notify(|o| o.on_nack(addr, &env.msg, attempts_left));
            if !attempts_left {
                return self.dead_letter(env.msg, stop).await;
            }

            // Back off before redelivering. The message stays with this
            // consumer, so a serial listener handles nothing else meanwhile.
            let delay = self.opts.retry_backoff.delay(env.msg.delivery_count);
            let interrupted = if delay.is_zero() {
                tokio::task::yield_now().await;
                *stop.borrow()
            } else {
                tokio::select! {
                    _ = tokio::time::sleep(delay) => false,
                    _ = aborted(stop) => true,
                }
            };
            if interrupted {
                return core.abandon(addr, env.msg);
            }
        }
    }

    /// Calls the handler on its own task, so a panic is contained and reported
    /// as [`Ack::Retry`] instead of ending the consumer.
    async fn invoke(&self, msg: &Message, stop: &mut watch::Receiver<bool>) -> Attempt {
        let handler = self.handler.clone();
        let copy = msg.clone();
        // The handler is called inside the task, so a panic while it builds
        // its future is contained as well.
        let mut task = AbortOnDrop(tokio::spawn(async move { handler(copy).await }));
        let joined = tokio::select! {
            joined = &mut task.0 => joined,
            // Dropping `task` cancels the handler at its next await point.
            _ = aborted(stop) => return Attempt::Cancelled,
        };
        match joined {
            Ok(ack) => Attempt::Done(ack),
            Err(error) if error.is_panic() => {
                let text = panic_text(error.into_panic());
                self.core
                    .notify(|o| o.on_handler_panic(&self.queue.addr, msg, &text));
                Attempt::Done(Ack::Retry)
            }
            // Cancelled from outside: the runtime is shutting down.
            Err(_) => Attempt::Cancelled,
        }
    }

    /// Handles a message whose time to live ran out.
    async fn expire(&self, env: Envelope, stop: &mut watch::Receiver<bool>) {
        let addr = self.queue.addr.as_str();
        self.core.notify(|o| o.on_expired(addr, &env.msg));
        match (self.opts.on_expired, &self.opts.dead_letter) {
            (ExpiryAction::DeadLetter, Some(_)) => self.dead_letter(env.msg, stop).await,
            _ => {
                self.core
                    .notify(|o| o.on_dropped(addr, &env.msg, DropReason::Expired));
                self.core.completed();
            }
        }
    }

    /// Moves a message to the dead-letter destination, waiting for room there
    /// rather than dropping it. Without a destination — or if it cannot take
    /// the message — the loss is reported as `on_dead_letter(to_dlq = false)`.
    async fn dead_letter(&self, msg: Message, stop: &mut watch::Receiver<bool>) {
        let addr = self.queue.addr.as_str();
        let core = &self.core;
        let Some(dlq) = self.opts.dead_letter.as_deref() else {
            core.notify(|o| o.on_dead_letter(addr, &msg, false));
            return core.completed();
        };

        // Resolve the destination now, then let go of the provider: waiting
        // for room must not keep it alive.
        let opts = SendOptions::new().on_full(OnFull::Block);
        let resolved = match self.provider.upgrade() {
            Some(provider) => provider.route(dlq, msg, &opts, Origin::Internal),
            None => Err(MessagingError::Closed(Rejected::new(dlq, msg))),
        };
        let (targets, mut msg, topic) = match resolved {
            Ok((Route::Queues { targets, topic, .. }, msg)) => (targets, msg, topic),
            Ok((_, msg)) => {
                // A dead-letter topic with no subscribers.
                core.notify(|o| o.on_dead_letter(dlq, &msg, false));
                return core.completed();
            }
            Err(mut error) => {
                if let Some(msg) = error.take_message() {
                    core.notify(|o| o.on_dead_letter(dlq, &msg, false));
                }
                return core.completed();
            }
        };
        if let Some(topic) = topic {
            msg.set_header(headers::TOPIC, topic);
        }

        let mut moved = false;
        for queue in &targets {
            let stored = tokio::select! {
                stored = enqueue(core, queue, msg.clone(), &opts) => stored,
                _ = aborted(stop) => return core.abandon(addr, msg),
            };
            moved |= stored.is_ok();
        }
        core.notify(|o| o.on_dead_letter(dlq, &msg, moved));
        core.completed();
    }
}

// ---- lifecycle integration ----------------------------------------------

impl ferroly::lifecycle::Component for LocalProvider {
    fn id(&self) -> &str {
        &self.inner.id
    }

    /// Reopens a provider that was stopped. A new provider is already open, so
    /// calling `start` first is optional.
    fn start(
        &self,
    ) -> ferroly::lifecycle::BoxFuture<'_, Result<(), ferroly::lifecycle::LifecycleError>> {
        Box::pin(async move {
            let _serial = self.inner.stop_lock.lock().await;
            self.inner.routing.lock().unwrap().phase = Phase::Open;
            Ok(())
        })
    }

    /// Drains with [`LocalOptions::drain_timeout`] (see
    /// [`LocalProvider::shutdown`]). Fails — leaving the manager to mark the
    /// component `Error` — if messages could not be delivered in time.
    fn stop(
        &self,
    ) -> ferroly::lifecycle::BoxFuture<'_, Result<(), ferroly::lifecycle::LifecycleError>> {
        Box::pin(async move {
            let report = self.shutdown(self.inner.options.drain_timeout).await;
            match report.undelivered.len() {
                0 => Ok(()),
                n => Err(ferroly::lifecycle::LifecycleError::failure(
                    self.inner.id.clone(),
                    MessagingError::Undelivered(n),
                )),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_window_is_bounded_and_forgets_the_oldest() {
        let mut window = DedupWindow::default();
        assert!(window.insert("a", 2));
        assert!(!window.insert("a", 2));
        assert!(window.insert("b", 2));
        // "a" falls out of the two-entry window when "c" arrives.
        assert!(window.insert("c", 2));
        assert!(window.insert("a", 2));
        assert!(!window.insert("c", 2));
        assert_eq!(window.order.len(), 2);

        window.remove("c");
        assert!(window.insert("c", 2));
    }

    #[test]
    fn targets_parse_and_canonicalize() {
        let canonical = |addr: &str| Target::parse(addr).map(|t| t.canonical());
        assert_eq!(canonical("orders").unwrap(), "chan://orders");
        assert_eq!(canonical("chan://orders").unwrap(), "chan://orders");
        assert_eq!(canonical("topic://a.b").unwrap(), "topic://a.b");
        assert_eq!(canonical("reply://x").unwrap(), "reply://x");
        assert!(canonical("kafka://orders").is_err());
        assert!(canonical("chan://").is_err());
    }
}

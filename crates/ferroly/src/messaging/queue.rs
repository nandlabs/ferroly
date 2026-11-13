//! The bounded per-listener buffer behind [`LocalProvider`].
//!
//! A plain `mpsc` channel cannot evict its oldest entry, hold back a delayed
//! message, or hand its contents back at shutdown, so the buffer is a small
//! purpose-built queue: a `VecDeque` for ready messages, a list of delayed
//! ones, and a semaphore whose permits are the free slots.
//!
//! [`LocalProvider`]: crate::messaging::LocalProvider

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{watch, Notify, Semaphore, TryAcquireError};
use tokio::time::Instant;

use ferroly::messaging::{Message, OnFull};

/// A message together with the bookkeeping the provider keeps for it.
pub(crate) struct Envelope {
    pub(crate) msg: Message,
    /// When the message's time to live runs out, if it has one.
    pub(crate) expires_at: Option<Instant>,
}

impl Envelope {
    pub(crate) fn is_expired(&self, now: Instant) -> bool {
        self.expires_at.is_some_and(|at| now >= at)
    }
}

struct Delayed {
    due: Instant,
    /// Insertion order, to keep messages due at the same instant in order.
    seq: u64,
    env: Envelope,
}

#[derive(Default)]
struct State {
    ready: VecDeque<Envelope>,
    delayed: Vec<Delayed>,
    /// No further pushes; the consumer drains what is buffered, then stops.
    closed: bool,
    /// The consumer stops at once, leaving what is buffered behind.
    aborted: bool,
    seq: u64,
}

impl State {
    #[cfg(test)]
    fn len(&self) -> usize {
        self.ready.len() + self.delayed.len()
    }

    fn insert(&mut self, env: Envelope, due: Option<Instant>, now: Instant) {
        match due {
            Some(due) if due > now => {
                self.seq += 1;
                let seq = self.seq;
                self.delayed.push(Delayed { due, seq, env });
            }
            _ => self.ready.push_back(env),
        }
    }

    /// Moves every delayed message that is due to the back of the ready list,
    /// in due order.
    fn promote_due(&mut self, now: Instant) {
        if self.delayed.iter().all(|d| d.due > now) {
            return;
        }
        let (mut due, later): (Vec<Delayed>, Vec<Delayed>) = std::mem::take(&mut self.delayed)
            .into_iter()
            .partition(|d| d.due <= now);
        self.delayed = later;
        due.sort_by_key(|d| (d.due, d.seq));
        self.ready.extend(due.into_iter().map(|d| d.env));
    }

    /// Removes the oldest buffered message: the head of the ready list, or
    /// failing that the delayed message that was accepted first.
    fn evict_oldest(&mut self) -> Option<Envelope> {
        if let Some(env) = self.ready.pop_front() {
            return Some(env);
        }
        let oldest = self
            .delayed
            .iter()
            .enumerate()
            .min_by_key(|(_, d)| d.seq)
            .map(|(i, _)| i)?;
        Some(self.delayed.swap_remove(oldest).env)
    }
}

/// Why a push did not store the message; the envelope is handed back.
pub(crate) enum PushError {
    Full(Envelope),
    Closed(Envelope),
    TimedOut(Envelope),
}

/// A bounded, single-consumer queue of [`Envelope`]s.
pub(crate) struct Queue {
    /// The listener's canonical address (`chan://orders`), used for observer
    /// callbacks.
    pub(crate) addr: String,
    /// Free slots. Closed together with the queue so blocked senders wake up.
    slots: Semaphore,
    state: Mutex<State>,
    /// Wakes the consumer; `notify_one` stores a permit, so a wake-up that
    /// races with the consumer going to sleep is not lost.
    readable: Notify,
    /// Latched `true` on abort: tells in-flight handlers to stop.
    abort: watch::Sender<bool>,
    /// Buffered messages across the listener group this queue belongs to.
    depth: Arc<AtomicUsize>,
}

impl Queue {
    pub(crate) fn new(addr: String, capacity: usize, depth: Arc<AtomicUsize>) -> Arc<Self> {
        Arc::new(Self {
            addr,
            slots: Semaphore::new(capacity),
            state: Mutex::new(State::default()),
            readable: Notify::new(),
            abort: watch::channel(false).0,
            depth,
        })
    }

    /// Buffered messages across this queue's listener group.
    pub(crate) fn depth(&self) -> usize {
        self.depth.load(Ordering::Relaxed)
    }

    /// Stores `env` (visible from `due`, or at once), applying `policy` when
    /// the queue is full. With [`OnFull::DropOldest`] the evicted envelope is
    /// returned.
    pub(crate) async fn push(
        &self,
        env: Envelope,
        due: Option<Instant>,
        policy: OnFull,
    ) -> Result<Option<Envelope>, PushError> {
        match policy {
            OnFull::Reject => match self.slots.try_acquire() {
                Ok(permit) => permit.forget(),
                Err(TryAcquireError::NoPermits) => return Err(PushError::Full(env)),
                Err(TryAcquireError::Closed) => return Err(PushError::Closed(env)),
            },
            OnFull::Block => match self.slots.acquire().await {
                Ok(permit) => permit.forget(),
                Err(_) => return Err(PushError::Closed(env)),
            },
            OnFull::BlockFor(limit) => {
                match tokio::time::timeout(limit, self.slots.acquire()).await {
                    Ok(Ok(permit)) => permit.forget(),
                    Ok(Err(_)) => return Err(PushError::Closed(env)),
                    Err(_) => return Err(PushError::TimedOut(env)),
                }
            }
            OnFull::DropOldest => loop {
                match self.slots.try_acquire() {
                    Ok(permit) => {
                        permit.forget();
                        break;
                    }
                    Err(TryAcquireError::Closed) => return Err(PushError::Closed(env)),
                    Err(TryAcquireError::NoPermits) => {}
                }
                // Full: take the oldest message's slot instead of a new one.
                {
                    let mut state = self.state.lock().unwrap();
                    if state.closed {
                        return Err(PushError::Closed(env));
                    }
                    if let Some(evicted) = state.evict_oldest() {
                        state.insert(env, due, Instant::now());
                        drop(state);
                        self.readable.notify_one();
                        return Ok(Some(evicted));
                    }
                }
                // Every slot is held by a sender that has not stored its
                // message yet; let it finish and look again.
                tokio::task::yield_now().await;
            },
        }

        let mut state = self.state.lock().unwrap();
        if state.closed {
            return Err(PushError::Closed(env));
        }
        state.insert(env, due, Instant::now());
        drop(state);
        self.depth.fetch_add(1, Ordering::Relaxed);
        self.readable.notify_one();
        Ok(None)
    }

    /// The next message, waiting for one to arrive or become due. Returns
    /// `None` once the queue is closed and empty, or as soon as it is aborted.
    pub(crate) async fn pop(&self) -> Option<Envelope> {
        loop {
            let next_due = {
                let mut state = self.state.lock().unwrap();
                if state.aborted {
                    return None;
                }
                state.promote_due(Instant::now());
                if let Some(env) = state.ready.pop_front() {
                    drop(state);
                    self.slots.add_permits(1);
                    self.depth.fetch_sub(1, Ordering::Relaxed);
                    return Some(env);
                }
                if state.closed && state.delayed.is_empty() {
                    return None;
                }
                state.delayed.iter().map(|d| d.due).min()
            };
            match next_due {
                Some(due) => {
                    tokio::select! {
                        _ = self.readable.notified() => {}
                        _ = tokio::time::sleep_until(due) => {}
                    }
                }
                None => self.readable.notified().await,
            }
        }
    }

    /// Stops accepting pushes. The consumer keeps draining what is buffered.
    pub(crate) fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.slots.close();
        self.readable.notify_one();
    }

    /// Closes the queue and stops the consumer at once: `pop` returns `None`
    /// even with messages buffered, and in-flight handlers are told to stop.
    pub(crate) fn abort(&self) {
        {
            let mut state = self.state.lock().unwrap();
            state.closed = true;
            state.aborted = true;
        }
        self.slots.close();
        self.abort.send_replace(true);
        self.readable.notify_one();
    }

    /// Whether the queue no longer accepts pushes.
    pub(crate) fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }

    /// A receiver that turns `true` when the queue is aborted.
    pub(crate) fn aborted(&self) -> watch::Receiver<bool> {
        self.abort.subscribe()
    }

    /// Removes and returns everything still buffered (ready first, then
    /// delayed in due order).
    pub(crate) fn take_all(&self) -> Vec<Envelope> {
        let mut state = self.state.lock().unwrap();
        let mut delayed = std::mem::take(&mut state.delayed);
        delayed.sort_by_key(|d| (d.due, d.seq));
        let taken: Vec<Envelope> = state
            .ready
            .drain(..)
            .chain(delayed.into_iter().map(|d| d.env))
            .collect();
        drop(state);
        self.release(taken.len());
        taken
    }

    /// Removes and returns the delayed messages that only become due after
    /// `deadline`.
    pub(crate) fn take_delayed_after(&self, deadline: Instant) -> Vec<Envelope> {
        let mut state = self.state.lock().unwrap();
        let (mut late, kept): (Vec<Delayed>, Vec<Delayed>) = std::mem::take(&mut state.delayed)
            .into_iter()
            .partition(|d| d.due > deadline);
        state.delayed = kept;
        drop(state);
        late.sort_by_key(|d| (d.due, d.seq));
        self.release(late.len());
        late.into_iter().map(|d| d.env).collect()
    }

    /// Accounts for `n` messages that left the queue other than through `pop`.
    fn release(&self, n: usize) {
        if n > 0 {
            self.slots.add_permits(n);
            self.depth.fetch_sub(n, Ordering::Relaxed);
        }
    }

    /// The number of messages buffered in this queue alone.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.state.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn env(body: &str) -> Envelope {
        Envelope {
            msg: Message::text(body),
            expires_at: None,
        }
    }

    fn queue(capacity: usize) -> Arc<Queue> {
        Queue::new("chan://q".into(), capacity, Arc::new(AtomicUsize::new(0)))
    }

    async fn body(q: &Queue) -> String {
        q.pop().await.unwrap().msg.body_str().into_owned()
    }

    #[tokio::test(start_paused = true)]
    async fn reject_hands_the_envelope_back_when_full() {
        let q = queue(1);
        assert!(q.push(env("a"), None, OnFull::Reject).await.is_ok());
        match q.push(env("b"), None, OnFull::Reject).await {
            Err(PushError::Full(back)) => assert_eq!(back.msg.body_str(), "b"),
            _ => panic!("expected Full"),
        }
        assert_eq!(q.depth(), 1);
        assert_eq!(body(&q).await, "a");
        assert_eq!(q.depth(), 0);
        // The slot is free again.
        assert!(q.push(env("c"), None, OnFull::Reject).await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn drop_oldest_evicts_the_head_and_keeps_the_bound() {
        let q = queue(2);
        for b in ["a", "b"] {
            assert!(q.push(env(b), None, OnFull::Reject).await.is_ok());
        }
        let evicted = q.push(env("c"), None, OnFull::DropOldest).await;
        match evicted {
            Ok(Some(old)) => assert_eq!(old.msg.body_str(), "a"),
            _ => panic!("expected an eviction"),
        }
        assert_eq!(q.len(), 2);
        assert_eq!(body(&q).await, "b");
        assert_eq!(body(&q).await, "c");
    }

    #[tokio::test(start_paused = true)]
    async fn block_for_times_out_and_block_waits_for_room() {
        let q = queue(1);
        assert!(q.push(env("a"), None, OnFull::Reject).await.is_ok());

        let began = Instant::now();
        let limit = Duration::from_secs(2);
        match q.push(env("b"), None, OnFull::BlockFor(limit)).await {
            Err(PushError::TimedOut(back)) => assert_eq!(back.msg.body_str(), "b"),
            _ => panic!("expected TimedOut"),
        }
        assert_eq!(began.elapsed(), limit);

        let blocked = tokio::spawn({
            let q = q.clone();
            async move { q.push(env("c"), None, OnFull::Block).await.is_ok() }
        });
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!blocked.is_finished());
        assert_eq!(body(&q).await, "a");
        assert!(blocked.await.unwrap());
        assert_eq!(body(&q).await, "c");
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_messages_become_visible_when_due_in_due_order() {
        let q = queue(8);
        let now = Instant::now();
        let at = |secs| Some(now + Duration::from_secs(secs));
        assert!(q.push(env("late"), at(10), OnFull::Reject).await.is_ok());
        assert!(q.push(env("soon"), at(5), OnFull::Reject).await.is_ok());
        assert!(q.push(env("now"), None, OnFull::Reject).await.is_ok());

        assert_eq!(body(&q).await, "now");
        assert_eq!(body(&q).await, "soon");
        assert_eq!(now.elapsed(), Duration::from_secs(5));
        assert_eq!(body(&q).await, "late");
        assert_eq!(now.elapsed(), Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn close_drains_but_abort_stops_at_once() {
        let q = queue(4);
        assert!(q.push(env("a"), None, OnFull::Reject).await.is_ok());
        q.close();
        assert!(q.is_closed());
        assert!(matches!(
            q.push(env("b"), None, OnFull::Block).await,
            Err(PushError::Closed(_))
        ));
        assert_eq!(body(&q).await, "a");
        assert!(q.pop().await.is_none());

        let q = queue(4);
        let far = Some(Instant::now() + Duration::from_secs(60));
        assert!(q.push(env("a"), None, OnFull::Reject).await.is_ok());
        assert!(q.push(env("b"), far, OnFull::Reject).await.is_ok());
        let mut aborted = q.aborted();
        q.abort();
        assert!(*aborted.borrow_and_update());
        assert!(q.pop().await.is_none());
        let left: Vec<String> = q
            .take_all()
            .into_iter()
            .map(|e| e.msg.body_str().into_owned())
            .collect();
        assert_eq!(left, ["a", "b"]);
        assert_eq!(q.depth(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn take_delayed_after_splits_on_the_deadline() {
        let q = queue(4);
        let now = Instant::now();
        let at = |secs| Some(now + Duration::from_secs(secs));
        assert!(q.push(env("in"), at(5), OnFull::Reject).await.is_ok());
        assert!(q.push(env("out"), at(50), OnFull::Reject).await.is_ok());

        let late = q.take_delayed_after(now + Duration::from_secs(10));
        assert_eq!(late.len(), 1);
        assert_eq!(late[0].msg.body_str(), "out");
        assert_eq!(q.len(), 1);
        assert_eq!(body(&q).await, "in");
    }
}

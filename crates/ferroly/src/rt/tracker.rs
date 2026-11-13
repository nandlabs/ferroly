//! [`TaskTracker`] — wait for a set of spawned tasks to finish.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use ferroly_derive::FerrolyError;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Counts the tasks spawned through it so a shutdown path can wait for them.
///
/// Unlike a `JoinSet`, a tracker does not keep task results or handles around:
/// it only knows how many tracked tasks are still alive, which makes it suited
/// to long-running servers that spawn a task per connection. A task counts
/// from the moment it is spawned until its future is dropped — whether it
/// finished, panicked, or was aborted.
///
/// [`wait`](TaskTracker::wait) completes once the tracker is **closed and
/// empty**. Closing is what makes "empty" final: an open tracker with zero
/// tasks may simply not have received its next one yet. Closing does not stop
/// tasks and does not prevent new ones from being spawned (they are tracked
/// too); pair the tracker with a [`CancellationToken`](super::CancellationToken)
/// to tell tasks to stop.
///
/// ```
/// use ferroly::rt::{CancellationToken, TaskTracker, time::Duration};
///
/// # async fn demo() {
/// let tracker = TaskTracker::new();
/// let stop = CancellationToken::new();
///
/// for _ in 0..4 {
///     let stop = stop.clone();
///     tracker.spawn(async move {
///         stop.cancelled().await; // ... work until told to stop ...
///     });
/// }
///
/// // Shutdown: tell tasks to stop, then give them a bounded time to do so.
/// stop.cancel();
/// tracker.shutdown(Duration::from_secs(10)).await.expect("tasks drained");
/// # }
/// ```
#[derive(Clone)]
pub struct TaskTracker {
    inner: Arc<Inner>,
}

struct Inner {
    state: watch::Sender<State>,
}

#[derive(Clone, Copy)]
struct State {
    active: usize,
    closed: bool,
}

impl State {
    fn drained(self) -> bool {
        self.closed && self.active == 0
    }
}

/// Decrements the tracker's count when the tracked future (or closure) is
/// dropped, however that happens.
struct Tracked {
    inner: Arc<Inner>,
}

impl Tracked {
    fn new(inner: &Arc<Inner>) -> Tracked {
        inner.state.send_modify(|s| s.active += 1);
        Tracked {
            inner: inner.clone(),
        }
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.inner.state.send_modify(|s| s.active -= 1);
    }
}

/// [`TaskTracker::wait_timeout`] / [`TaskTracker::shutdown`] gave up waiting.
#[derive(Debug, Clone, PartialEq, Eq, FerrolyError)]
#[error("timed out waiting for tasks: {remaining} still running")]
pub struct TrackerTimeout {
    /// How many tracked tasks were still alive when the wait gave up.
    pub remaining: usize,
}

impl TaskTracker {
    /// Creates an open tracker with no tasks.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                state: watch::channel(State {
                    active: 0,
                    closed: false,
                })
                .0,
            }),
        }
    }

    /// Spawns `future` on the current runtime and tracks it until it ends.
    ///
    /// # Panics
    ///
    /// Panics if called outside a runtime, like [`spawn`](super::spawn).
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        tokio::spawn(self.track_future(future))
    }

    /// Runs the blocking closure `f` on the blocking pool and tracks it until
    /// it returns.
    ///
    /// # Panics
    ///
    /// Panics if called outside a runtime, like
    /// [`spawn_blocking`](super::spawn_blocking).
    pub fn spawn_blocking<F, R>(&self, f: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let tracked = Tracked::new(&self.inner);
        tokio::task::spawn_blocking(move || {
            let _tracked = tracked;
            f()
        })
    }

    /// Wraps `future` so it is tracked from **now** until it completes or is
    /// dropped, without spawning it — for futures you run yourself (in a
    /// `JoinSet`, a `LocalSet`, or inline).
    pub fn track_future<F: Future>(&self, future: F) -> impl Future<Output = F::Output> {
        let tracked = Tracked::new(&self.inner);
        async move {
            let _tracked = tracked;
            future.await
        }
    }

    /// Closes the tracker, allowing [`wait`](TaskTracker::wait) to complete
    /// once no tasks remain. Returns `true` if this call closed it.
    pub fn close(&self) -> bool {
        self.inner
            .state
            .send_if_modified(|s| !std::mem::replace(&mut s.closed, true))
    }

    /// Reopens a closed tracker, so `wait` blocks again. Returns `true` if
    /// this call reopened it.
    pub fn reopen(&self) -> bool {
        self.inner
            .state
            .send_if_modified(|s| std::mem::replace(&mut s.closed, false))
    }

    /// Whether the tracker has been closed.
    pub fn is_closed(&self) -> bool {
        self.inner.state.borrow().closed
    }

    /// How many tracked tasks are still alive.
    pub fn len(&self) -> usize {
        self.inner.state.borrow().active
    }

    /// Whether no tracked tasks are alive.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Completes once the tracker is closed **and** has no live tasks.
    ///
    /// Never completes while the tracker is open; call
    /// [`close`](TaskTracker::close) first (in either order with the tasks
    /// finishing).
    pub async fn wait(&self) {
        let mut changes = self.inner.state.subscribe();
        loop {
            if changes.borrow_and_update().drained() {
                return;
            }
            if changes.changed().await.is_err() {
                // The sender lives in `self.inner`; unreachable while `self`
                // is borrowed.
                std::future::pending::<()>().await;
            }
        }
    }

    /// Like [`wait`](TaskTracker::wait), but gives up after `timeout`,
    /// reporting how many tasks were still running.
    pub async fn wait_timeout(&self, timeout: Duration) -> Result<(), TrackerTimeout> {
        match tokio::time::timeout(timeout, self.wait()).await {
            Ok(()) => Ok(()),
            Err(_) => Err(TrackerTimeout {
                remaining: self.len(),
            }),
        }
    }

    /// Graceful-shutdown shorthand: [`close`](TaskTracker::close) the tracker,
    /// then wait up to `timeout` for the remaining tasks to end.
    ///
    /// This does not signal or abort anything — cancel a
    /// [`CancellationToken`](super::CancellationToken) the tasks listen to
    /// first. Tasks still running at the timeout keep running; abort their
    /// `JoinHandle`s (or drop the runtime) to force them down.
    pub async fn shutdown(&self, timeout: Duration) -> Result<(), TrackerTimeout> {
        self.close();
        self.wait_timeout(timeout).await
    }
}

impl Default for TaskTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for TaskTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskTracker")
            .field("len", &self.len())
            .field("is_closed", &self.is_closed())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rt::CancellationToken;
    use tokio::time::timeout;

    const SOON: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn wait_needs_close_and_empty() {
        let tracker = TaskTracker::new();
        assert!(tracker.is_empty());
        assert!(!tracker.is_closed());

        // Open and empty: wait must not complete.
        assert_eq!(
            tracker.wait_timeout(Duration::from_millis(20)).await,
            Err(TrackerTimeout { remaining: 0 })
        );

        assert!(tracker.close());
        assert!(!tracker.close(), "second close reports no change");
        timeout(SOON, tracker.wait()).await.unwrap();

        assert!(tracker.reopen());
        assert!(!tracker.reopen());
        assert!(tracker
            .wait_timeout(Duration::from_millis(20))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn tracks_tasks_until_they_finish() {
        let tracker = TaskTracker::new();
        let stop = CancellationToken::new();
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let stop = stop.clone();
                tracker.spawn(async move {
                    stop.cancelled().await;
                    i
                })
            })
            .collect();
        assert_eq!(tracker.len(), 8);
        tracker.close();

        // Closed but not empty.
        assert_eq!(
            tracker.wait_timeout(Duration::from_millis(20)).await,
            Err(TrackerTimeout { remaining: 8 })
        );

        stop.cancel();
        timeout(SOON, tracker.wait()).await.unwrap();
        assert!(tracker.is_empty());
        for (i, handle) in handles.into_iter().enumerate() {
            assert_eq!(handle.await.unwrap(), i);
        }
    }

    #[tokio::test]
    async fn waiter_started_before_close_is_woken() {
        let tracker = TaskTracker::new();
        let waiter = {
            let tracker = tracker.clone();
            tokio::spawn(async move { tracker.wait().await })
        };
        tokio::task::yield_now().await;
        let task = tracker.spawn(async {});
        task.await.unwrap();
        assert!(!waiter.is_finished());
        tracker.close();
        timeout(SOON, waiter).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn aborted_and_panicked_tasks_are_released() {
        let tracker = TaskTracker::new();
        let aborted = tracker.spawn(std::future::pending::<()>());
        let panicked = tracker.spawn(async { panic!("boom") });
        assert!(panicked.await.unwrap_err().is_panic());
        aborted.abort();
        assert!(aborted.await.unwrap_err().is_cancelled());
        tracker.shutdown(SOON).await.unwrap();
        assert_eq!(tracker.len(), 0);
    }

    #[tokio::test]
    async fn shutdown_times_out_and_reports_stragglers() {
        let tracker = TaskTracker::new();
        let quick = tracker.spawn(async {});
        let stuck = tracker.spawn(std::future::pending::<()>());
        quick.await.unwrap();
        let err = tracker
            .shutdown(Duration::from_millis(30))
            .await
            .unwrap_err();
        assert_eq!(err, TrackerTimeout { remaining: 1 });
        assert!(err.to_string().contains("1 still running"));
        assert!(tracker.is_closed());
        stuck.abort();
        let _ = stuck.await;
        tracker.wait_timeout(SOON).await.unwrap();
    }

    #[tokio::test]
    async fn track_future_counts_from_creation_and_on_drop() {
        let tracker = TaskTracker::new();
        let tracked = tracker.track_future(async { 5 });
        assert_eq!(tracker.len(), 1, "counted before first poll");
        assert_eq!(tracked.await, 5);
        assert_eq!(tracker.len(), 0);

        let never_polled = tracker.track_future(std::future::pending::<()>());
        assert_eq!(tracker.len(), 1);
        drop(never_polled);
        assert_eq!(tracker.len(), 0);
    }

    #[tokio::test]
    async fn spawn_blocking_is_tracked() {
        let tracker = TaskTracker::new();
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let job = tracker.spawn_blocking(move || {
            gate.recv().ok();
            42
        });
        assert_eq!(tracker.len(), 1);
        tracker.close();
        assert!(tracker
            .wait_timeout(Duration::from_millis(20))
            .await
            .is_err());
        release.send(()).unwrap();
        assert_eq!(job.await.unwrap(), 42);
        tracker.wait_timeout(SOON).await.unwrap();
    }

    #[tokio::test]
    async fn tasks_spawned_after_close_are_still_tracked() {
        let tracker = TaskTracker::new();
        tracker.close();
        let stop = CancellationToken::new();
        let late = tracker.spawn(stop.clone().cancelled_owned());
        assert!(tracker
            .wait_timeout(Duration::from_millis(20))
            .await
            .is_err());
        stop.cancel();
        late.await.unwrap();
        tracker.wait_timeout(SOON).await.unwrap();
    }
}

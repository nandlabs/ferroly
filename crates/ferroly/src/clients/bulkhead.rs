//! A bulkhead: a cap on how many calls run concurrently.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

use ferroly::clients::error::BulkheadFullError;

/// Limits how many operations run at the same time, so one slow dependency
/// cannot absorb every task, connection or thread of the caller (the
/// "bulkhead" pattern).
///
/// What happens when all slots are busy is the *queue timeout*:
///
/// - none (the default from [`new`](Bulkhead::new)): callers wait until a
///   slot frees up;
/// - [`with_queue_timeout`](Bulkhead::with_queue_timeout): callers wait at
///   most that long, then get [`BulkheadFullError`];
/// - a zero timeout ([`fail_fast`](Bulkhead::fail_fast)): callers are
///   rejected immediately — load shedding.
///
/// ```
/// use std::time::Duration;
/// use ferroly::clients::Bulkhead;
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// let bulkhead = Bulkhead::new(2).with_queue_timeout(Duration::from_millis(50));
///
/// let answer = bulkhead.run(async { 21 * 2 }).await;
/// assert_eq!(answer, Ok(42));
///
/// // Or hold a slot explicitly:
/// let slot = bulkhead.acquire().await.unwrap();
/// assert_eq!(bulkhead.available(), 1);
/// drop(slot);
/// assert_eq!(bulkhead.available(), 2);
/// # }
/// ```
///
/// Waiters are served in FIFO order. Cancelling a waiting or running call
/// releases its place.
#[derive(Debug)]
pub struct Bulkhead {
    semaphore: Arc<Semaphore>,
    max_concurrent: usize,
    queue_timeout: Option<Duration>,
}

/// A held bulkhead slot; the slot is released when this is dropped.
#[derive(Debug)]
#[must_use = "the slot is released as soon as the permit is dropped"]
pub struct BulkheadPermit {
    _permit: OwnedSemaphorePermit,
}

impl Bulkhead {
    /// A bulkhead admitting `max_concurrent` operations at a time (at least
    /// one). Callers beyond that wait without limit.
    pub fn new(max_concurrent: usize) -> Self {
        let max_concurrent = max_concurrent.clamp(1, Semaphore::MAX_PERMITS);
        Bulkhead {
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            max_concurrent,
            queue_timeout: None,
        }
    }

    /// Bounds how long a caller waits for a slot before being rejected.
    pub fn with_queue_timeout(mut self, timeout: Duration) -> Self {
        self.queue_timeout = Some(timeout);
        self
    }

    /// Rejects callers immediately when no slot is free (a zero queue
    /// timeout).
    pub fn fail_fast(self) -> Self {
        self.with_queue_timeout(Duration::ZERO)
    }

    /// The configured concurrency limit.
    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    /// The number of free slots right now.
    pub fn available(&self) -> usize {
        self.semaphore.available_permits()
    }

    /// The number of slots in use right now.
    pub fn in_flight(&self) -> usize {
        self.max_concurrent.saturating_sub(self.available())
    }

    /// Takes a slot if one is free, without waiting.
    pub fn try_acquire(&self) -> Result<BulkheadPermit, BulkheadFullError> {
        match self.semaphore.clone().try_acquire_owned() {
            Ok(permit) => Ok(BulkheadPermit { _permit: permit }),
            Err(TryAcquireError::NoPermits) | Err(TryAcquireError::Closed) => {
                Err(BulkheadFullError)
            }
        }
    }

    /// Waits for a slot, up to the queue timeout.
    pub async fn acquire(&self) -> Result<BulkheadPermit, BulkheadFullError> {
        let wait = self.semaphore.clone().acquire_owned();
        let permit = match self.queue_timeout {
            None => wait.await,
            Some(timeout) if timeout.is_zero() => return self.try_acquire(),
            Some(timeout) => match tokio::time::timeout(timeout, wait).await {
                Ok(permit) => permit,
                Err(_) => return Err(BulkheadFullError),
            },
        };
        // The semaphore is never closed, so this cannot fail in practice.
        permit
            .map(|permit| BulkheadPermit { _permit: permit })
            .map_err(|_| BulkheadFullError)
    }

    /// Runs `operation` in a slot, waiting for one up to the queue timeout.
    /// The future is not polled at all if no slot is obtained.
    pub async fn run<F>(&self, operation: F) -> Result<F::Output, BulkheadFullError>
    where
        F: Future,
    {
        let _slot = self.acquire().await?;
        Ok(operation.await)
    }
}

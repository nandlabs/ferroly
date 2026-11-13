//! Timeouts and deadlines for async operations.

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

use ferroly::clients::error::TimeoutError;

/// Runs `operation`, giving up with [`TimeoutError`] if it has not finished
/// after `limit`.
///
/// On timeout the operation's future is dropped, which cancels it at its next
/// `.await` point. Work that cannot be cancelled that way (a blocking call on
/// another thread, a request the server already started processing) keeps
/// running; only the wait is abandoned.
///
/// ```
/// use std::time::Duration;
/// use ferroly::clients::{with_timeout, TimeoutError};
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// let fast = with_timeout(Duration::from_secs(1), async { 7 }).await;
/// assert_eq!(fast, Ok(7));
///
/// let slow = with_timeout(
///     Duration::from_millis(5),
///     tokio::time::sleep(Duration::from_secs(60)),
/// )
/// .await;
/// assert_eq!(slow, Err(TimeoutError));
/// # }
/// ```
pub async fn with_timeout<F>(limit: Duration, operation: F) -> Result<F::Output, TimeoutError>
where
    F: Future,
{
    tokio::time::timeout(limit, operation)
        .await
        .map_err(|_| TimeoutError)
}

/// A fixed point in time by which a piece of work must be done.
///
/// A [`with_timeout`] restarts for every step; a `Deadline` is shared by all
/// the steps of one request, so the total stays bounded however many calls,
/// retries or hops it takes. Create it once, pass it down (it is `Copy`), and
/// run each step with [`run`](Deadline::run):
///
/// ```
/// use std::time::Duration;
/// use ferroly::clients::Deadline;
///
/// # async fn step() -> u32 { 1 }
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// let deadline = Deadline::after(Duration::from_secs(2));
/// let a = deadline.run(step()).await.unwrap();
/// let b = deadline.run(step()).await.unwrap(); // shares the same 2 seconds
/// assert_eq!(a + b, 2);
/// assert!(!deadline.is_expired());
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadline {
    at: Instant,
}

impl Deadline {
    /// A deadline `limit` from now.
    pub fn after(limit: Duration) -> Self {
        let now = Instant::now();
        Deadline {
            // An unrepresentably distant deadline is effectively "never".
            at: now
                .checked_add(limit)
                .unwrap_or_else(|| now + Duration::from_secs(60 * 60 * 24 * 365 * 30)),
        }
    }

    /// A deadline at the given instant.
    pub fn at(at: Instant) -> Self {
        Deadline { at }
    }

    /// The instant of the deadline.
    pub fn instant(&self) -> Instant {
        self.at
    }

    /// The time left, or zero if the deadline has passed.
    pub fn remaining(&self) -> Duration {
        self.at.saturating_duration_since(Instant::now())
    }

    /// Whether the deadline has passed.
    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.at
    }

    /// Fails with [`TimeoutError`] if the deadline has passed — a cheap check
    /// before starting a step.
    pub fn check(&self) -> Result<(), TimeoutError> {
        if self.is_expired() {
            Err(TimeoutError)
        } else {
            Ok(())
        }
    }

    /// The earlier of this deadline and `limit` from now — for a step that
    /// has its own, shorter budget.
    pub fn min_with(&self, limit: Duration) -> Deadline {
        let other = Deadline::after(limit);
        if other.at < self.at {
            other
        } else {
            *self
        }
    }

    /// Runs `operation` until this deadline, giving up with [`TimeoutError`]
    /// when it passes. An operation started after the deadline is polled once
    /// and times out unless it completes immediately.
    pub async fn run<F>(&self, operation: F) -> Result<F::Output, TimeoutError>
    where
        F: Future,
    {
        tokio::time::timeout_at(self.at, operation)
            .await
            .map_err(|_| TimeoutError)
    }
}

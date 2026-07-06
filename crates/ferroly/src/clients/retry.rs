//! Retry policy and driver.

use std::future::Future;
use std::time::Duration;

/// Configures a bounded retry loop with optional exponential backoff.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum number of *retries* after the initial attempt.
    pub max_retries: u32,
    /// Base wait between attempts.
    pub wait: Duration,
    /// Whether to double the wait after each attempt.
    pub exponential: bool,
    /// An optional cap on the backoff when `exponential` is set.
    pub max_backoff: Option<Duration>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            wait: Duration::from_millis(200),
            exponential: true,
            max_backoff: Some(Duration::from_secs(10)),
        }
    }
}

impl RetryPolicy {
    /// A fixed-wait policy (no backoff growth).
    pub fn fixed(max_retries: u32, wait: Duration) -> Self {
        Self {
            max_retries,
            wait,
            exponential: false,
            max_backoff: None,
        }
    }

    /// The delay to wait before the retry numbered `attempt` (0-based).
    fn backoff_for(&self, attempt: u32) -> Duration {
        if !self.exponential {
            return self.wait;
        }
        let factor = 2u32.saturating_pow(attempt);
        let scaled = self.wait.saturating_mul(factor);
        match self.max_backoff {
            Some(cap) if scaled > cap => cap,
            _ => scaled,
        }
    }
}

/// Runs `op` until it succeeds or the policy's retries are exhausted.
///
/// The predicate `retry_if` decides whether a given error is retryable; return
/// `false` to fail fast. The last error is returned on exhaustion.
///
/// ```
/// # use ferroly::clients::{retry, RetryPolicy};
/// # use std::time::Duration;
/// # #[tokio::main] async fn main() {
/// let policy = RetryPolicy::fixed(2, Duration::from_millis(1));
/// let mut attempts = 0;
/// let result: Result<u32, &str> = retry(&policy, |_| true, || {
///     attempts += 1;
///     async move { if attempts < 2 { Err("transient") } else { Ok(42) } }
/// }).await;
/// assert_eq!(result, Ok(42));
/// # }
/// ```
pub async fn retry<T, E, P, F, Fut>(policy: &RetryPolicy, retry_if: P, mut op: F) -> Result<T, E>
where
    P: Fn(&E) -> bool,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut attempt = 0u32;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if attempt >= policy.max_retries || !retry_if(&e) {
                    return Err(e);
                }
                tokio::time::sleep(policy.backoff_for(attempt)).await;
                attempt += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn succeeds_after_retries() {
        let policy = RetryPolicy::fixed(5, Duration::from_millis(1));
        let calls = AtomicU32::new(0);
        let result: Result<u32, &str> = retry(
            &policy,
            |_| true,
            || {
                let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                async move {
                    if n < 3 {
                        Err("transient")
                    } else {
                        Ok(n)
                    }
                }
            },
        )
        .await;
        assert_eq!(result, Ok(3));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn gives_up_after_max_retries() {
        let policy = RetryPolicy::fixed(2, Duration::from_millis(1));
        let calls = AtomicU32::new(0);
        let result: Result<u32, &str> = retry(
            &policy,
            |_| true,
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err("always") }
            },
        )
        .await;
        assert_eq!(result, Err("always"));
        // initial attempt + 2 retries = 3 calls
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn fails_fast_on_non_retryable() {
        let policy = RetryPolicy::fixed(5, Duration::from_millis(1));
        let calls = AtomicU32::new(0);
        let result: Result<u32, &str> = retry(
            &policy,
            |_| false,
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err("fatal") }
            },
        )
        .await;
        assert_eq!(result, Err("fatal"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn exponential_backoff_is_capped() {
        let policy = RetryPolicy {
            max_retries: 10,
            wait: Duration::from_millis(100),
            exponential: true,
            max_backoff: Some(Duration::from_millis(500)),
        };
        assert_eq!(policy.backoff_for(0), Duration::from_millis(100));
        assert_eq!(policy.backoff_for(1), Duration::from_millis(200));
        assert_eq!(policy.backoff_for(2), Duration::from_millis(400));
        assert_eq!(policy.backoff_for(3), Duration::from_millis(500)); // capped
    }
}

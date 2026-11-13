//! A composed resilience pipeline.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use ferroly::clients::breaker::{CircuitBreaker, Outcome};
use ferroly::clients::bulkhead::Bulkhead;
use ferroly::clients::error::PolicyError;
use ferroly::clients::ratelimit::RateLimiter;
use ferroly::clients::retry::{Retry, RetryDecision};

/// How one error of the operation is treated by a [`Policy`]: whether to
/// retry it, and how it counts toward the circuit breaker.
///
/// The two questions are independent, and getting them right matters:
///
/// | Error | Constructor | Retry? | Breaker |
/// |---|---|---|---|
/// | timeout, connection reset, `502`/`503`/`504` | [`transient`](Self::transient) | yes | failure |
/// | `429` / `503` with `Retry-After` | [`retry_after`](Self::retry_after) | yes, after the given delay | failure |
/// | `500` on a non-idempotent call, corrupt response | [`fatal`](Self::fatal) | no | failure |
/// | `400`, `401`, `403`, `404`, `422` | [`caller_error`](Self::caller_error) | no | ignored |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorClass {
    /// Whether (and when) to retry.
    pub retry: RetryDecision,
    /// How the error counts toward the circuit breaker.
    pub outcome: Outcome,
}

impl ErrorClass {
    /// A temporary failure of the dependency: retry it, and count it against
    /// the breaker.
    pub const fn transient() -> Self {
        ErrorClass {
            retry: RetryDecision::Retry,
            outcome: Outcome::Failure,
        }
    }

    /// A failure of the dependency that must not be retried (for example
    /// because the call is not idempotent): count it against the breaker.
    pub const fn fatal() -> Self {
        ErrorClass {
            retry: RetryDecision::Stop,
            outcome: Outcome::Failure,
        }
    }

    /// The request itself was wrong (bad input, missing permission, not
    /// found): do not retry, and do **not** count it against the breaker —
    /// the dependency is healthy.
    pub const fn caller_error() -> Self {
        ErrorClass {
            retry: RetryDecision::Stop,
            outcome: Outcome::Ignored,
        }
    }

    /// The dependency asked the caller to come back after `delay` (a
    /// `Retry-After`): retry after exactly that long, and count it against
    /// the breaker (it is overloaded).
    pub const fn retry_after(delay: Duration) -> Self {
        ErrorClass {
            retry: RetryDecision::RetryAfter(delay),
            outcome: Outcome::Failure,
        }
    }
}

/// Composes the resilience primitives around an async operation, in a fixed,
/// sensible order (outermost first):
///
/// 1. **timeout** — an overall deadline for everything below, retries and
///    waiting included;
/// 2. **retry** — a [`Retry`] around each *attempt*;
/// 3. **circuit breaker** — each attempt asks for a permit and reports its
///    outcome; an open circuit fails the call at once and is not retried;
/// 4. **bulkhead** — each attempt takes a concurrency slot; a full bulkhead
///    fails the call and is not retried;
/// 5. **rate limiter** — each attempt waits for a token;
/// 6. **attempt timeout** — a limit on one run of the operation, which counts
///    as a breaker failure and is retried.
///
/// Every stage is optional; an empty `Policy` just runs the operation.
///
/// ```
/// use std::sync::Arc;
/// use std::time::Duration;
/// use ferroly::clients::{
///     Bulkhead, CircuitBreaker, CircuitBreakerConfig, ErrorClass, Jitter, Policy, PolicyError,
///     RetryPolicy,
/// };
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// let policy = Policy::new()
///     .timeout(Duration::from_secs(10))
///     .retry(RetryPolicy::fixed(3, Duration::from_millis(1)).with_jitter(Jitter::Full))
///     .circuit_breaker(Arc::new(CircuitBreaker::new(CircuitBreakerConfig::default())))
///     .bulkhead(Arc::new(Bulkhead::new(16)))
///     .attempt_timeout(Duration::from_secs(2));
///
/// let mut calls = 0;
/// let result: Result<&str, PolicyError<u16>> = policy
///     .execute_with(
///         |status: &u16| match *status {
///             500..=599 => ErrorClass::transient(),
///             _ => ErrorClass::caller_error(),
///         },
///         || {
///             calls += 1;
///             let n = calls;
///             async move { if n < 3 { Err(503) } else { Ok("ok") } }
///         },
///     )
///     .await;
/// assert_eq!(result, Ok("ok"));
/// assert_eq!(calls, 3);
/// # }
/// ```
///
/// Share the breaker, bulkhead and limiter (they are `Arc`s) between every
/// policy that talks to the same dependency; cloning a `Policy` shares them
/// too.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    timeout: Option<Duration>,
    attempt_timeout: Option<Duration>,
    retry: Option<Retry>,
    breaker: Option<Arc<CircuitBreaker>>,
    bulkhead: Option<Arc<Bulkhead>>,
    rate_limiter: Option<Arc<RateLimiter>>,
}

impl Policy {
    /// An empty policy.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bounds the whole call — all attempts, backoff sleeps and time spent
    /// queueing — returning [`PolicyError::Timeout`] when it elapses.
    pub fn timeout(mut self, limit: Duration) -> Self {
        self.timeout = Some(limit);
        self
    }

    /// Bounds each single run of the operation. A timed-out attempt is
    /// reported to the breaker as a failure and is retried if a retry is
    /// configured.
    pub fn attempt_timeout(mut self, limit: Duration) -> Self {
        self.attempt_timeout = Some(limit);
        self
    }

    /// Retries failed attempts. Accepts a [`Retry`] or a plain
    /// [`RetryPolicy`](ferroly::clients::RetryPolicy).
    pub fn retry(mut self, retry: impl Into<Retry>) -> Self {
        self.retry = Some(retry.into());
        self
    }

    /// Guards every attempt with `breaker`.
    pub fn circuit_breaker(mut self, breaker: Arc<CircuitBreaker>) -> Self {
        self.breaker = Some(breaker);
        self
    }

    /// Runs every attempt in a slot of `bulkhead`.
    pub fn bulkhead(mut self, bulkhead: Arc<Bulkhead>) -> Self {
        self.bulkhead = Some(bulkhead);
        self
    }

    /// Paces every attempt with `limiter`.
    pub fn rate_limiter(mut self, limiter: Arc<RateLimiter>) -> Self {
        self.rate_limiter = Some(limiter);
        self
    }

    /// Runs `op` through the pipeline, treating every error of the operation
    /// as [`ErrorClass::transient`] (retryable, counted by the breaker).
    ///
    /// `op` is called once per attempt to create that attempt's future.
    pub async fn execute<T, E, F, Fut>(&self, op: F) -> Result<T, PolicyError<E>>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        self.execute_with(|_| ErrorClass::transient(), op).await
    }

    /// Runs `op` through the pipeline; `classify` decides, for each error of
    /// the operation, whether it is retried and how the breaker counts it
    /// (see [`ErrorClass`]).
    ///
    /// `op` is called once per attempt to create that attempt's future. The
    /// future is created before the breaker and bulkhead are consulted and is
    /// dropped without being polled if they reject the attempt, so `op`
    /// itself should not do work — only build the future.
    pub async fn execute_with<T, E, C, F, Fut>(
        &self,
        classify: C,
        mut op: F,
    ) -> Result<T, PolicyError<E>>
    where
        C: Fn(&E) -> ErrorClass,
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let attempts = async {
            match &self.retry {
                None => self.attempt(&classify, op()).await,
                Some(retry) => {
                    retry
                        .run_with(
                            |error: &PolicyError<E>, _| match error {
                                PolicyError::Inner(inner) => classify(inner).retry,
                                PolicyError::Timeout => RetryDecision::Retry,
                                // Rejections are load-shedding signals:
                                // retrying into them defeats their purpose.
                                PolicyError::CircuitOpen | PolicyError::BulkheadFull => {
                                    RetryDecision::Stop
                                }
                            },
                            || {
                                let operation = op();
                                self.attempt(&classify, operation)
                            },
                        )
                        .await
                }
            }
        };
        match self.timeout {
            Some(limit) => match tokio::time::timeout(limit, attempts).await {
                Ok(result) => result,
                Err(_) => Err(PolicyError::Timeout),
            },
            None => attempts.await,
        }
    }

    /// One attempt: breaker permit, bulkhead slot, rate-limit token, then the
    /// operation under the attempt timeout.
    async fn attempt<T, E, C, Fut>(&self, classify: &C, operation: Fut) -> Result<T, PolicyError<E>>
    where
        C: Fn(&E) -> ErrorClass,
        Fut: Future<Output = Result<T, E>>,
    {
        // If anything below returns early or this future is dropped, the
        // permit is dropped unresolved, which records an ignored call.
        let permit = match &self.breaker {
            Some(breaker) => Some(breaker.try_acquire()?),
            None => None,
        };
        let _slot = match &self.bulkhead {
            Some(bulkhead) => Some(bulkhead.acquire().await?),
            None => None,
        };
        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await;
        }
        let result = match self.attempt_timeout {
            Some(limit) => match tokio::time::timeout(limit, operation).await {
                Ok(result) => result.map_err(PolicyError::Inner),
                Err(_) => Err(PolicyError::Timeout),
            },
            None => operation.await.map_err(PolicyError::Inner),
        };
        if let Some(permit) = permit {
            permit.record(match &result {
                Ok(_) => Outcome::Success,
                Err(PolicyError::Inner(error)) => classify(error).outcome,
                Err(_) => Outcome::Failure,
            });
        }
        result
    }
}

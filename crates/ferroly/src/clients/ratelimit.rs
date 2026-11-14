//! A client-side token-bucket rate limiter.

use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

use ferroly::clients::error::RateLimitedError;

/// Paces outbound calls to stay under a quota the *server* enforces — so the
/// client waits its turn instead of collecting `429`s.
///
/// A token bucket: it holds up to `burst` tokens and refills at `rate` tokens
/// per second. Each call takes one token; [`acquire`](RateLimiter::acquire)
/// waits when the bucket is empty, [`try_acquire`](RateLimiter::try_acquire)
/// refuses instead. The bucket starts full, so the first `burst` calls go
/// through immediately.
///
/// ```
/// use ferroly::clients::RateLimiter;
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// // 50 requests per second, bursts of up to 10.
/// let limiter = RateLimiter::new(50.0, 10);
/// limiter.acquire().await;            // waits if the bucket is empty
/// assert!(limiter.try_acquire());     // or: take one only if available
/// # }
/// ```
///
/// Waiting callers are served in arrival order: each reserves its token up
/// front and sleeps until that token has been earned. A caller that is
/// cancelled while waiting does not give its reservation back, so the pace
/// stays at or below the configured rate.
#[derive(Debug)]
pub struct RateLimiter {
    rate: f64,
    burst: f64,
    state: Mutex<Bucket>,
}

#[derive(Debug)]
struct Bucket {
    /// May be negative: tokens already promised to waiting callers.
    tokens: f64,
    last_refill: Instant,
}

/// Converts a wait in seconds to a `Duration`, rounded to the nearest
/// nanosecond (so that e.g. 3 tokens at 10/s is exactly 300 ms) and saturating
/// for values too large to represent.
fn seconds(wait: f64) -> Duration {
    let nanos = (wait * 1e9).round();
    if nanos.is_nan() || nanos <= 0.0 {
        Duration::ZERO
    } else if nanos >= u64::MAX as f64 {
        Duration::MAX
    } else {
        Duration::from_nanos(nanos as u64)
    }
}

impl RateLimiter {
    /// A limiter refilling at `rate_per_second` tokens per second with a
    /// bucket of `burst` tokens (at least one).
    ///
    /// A `rate_per_second` that is zero, negative or not finite means the
    /// bucket never refills: once the initial `burst` is spent,
    /// [`try_acquire`](Self::try_acquire) always refuses and
    /// [`acquire`](Self::acquire) waits forever.
    pub fn new(rate_per_second: f64, burst: u32) -> Self {
        let rate = if rate_per_second.is_finite() && rate_per_second > 0.0 {
            rate_per_second
        } else {
            0.0
        };
        let burst = burst.max(1) as f64;
        RateLimiter {
            rate,
            burst,
            state: Mutex::new(Bucket {
                tokens: burst,
                last_refill: Instant::now(),
            }),
        }
    }

    /// A limiter allowing `count` calls per `period`, with a burst of
    /// `count`.
    pub fn per(count: u32, period: Duration) -> Self {
        let seconds = period.as_secs_f64();
        let rate = if seconds > 0.0 {
            count as f64 / seconds
        } else {
            f64::INFINITY
        };
        // An "infinite" rate is not finite, so `new` would freeze the bucket;
        // use the largest finite rate instead.
        RateLimiter::new(rate.min(f64::MAX), count)
    }

    fn refilled(&self) -> std::sync::MutexGuard<'_, Bucket> {
        let mut bucket = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(bucket.last_refill);
        bucket.last_refill = now;
        bucket.tokens = (bucket.tokens + elapsed.as_secs_f64() * self.rate).min(self.burst);
        bucket
    }

    /// Reserves `tokens` and returns how long to wait until they are earned
    /// (`None` if that never happens).
    fn reserve(&self, tokens: f64) -> Option<Duration> {
        let mut bucket = self.refilled();
        if bucket.tokens >= tokens {
            bucket.tokens -= tokens;
            return Some(Duration::ZERO);
        }
        if self.rate <= 0.0 {
            return None;
        }
        bucket.tokens -= tokens;
        Some(seconds(-bucket.tokens / self.rate))
    }

    /// Takes one token if it is available right now.
    pub fn try_acquire(&self) -> bool {
        self.try_acquire_many(1)
    }

    /// Takes `tokens` tokens if all of them are available right now.
    pub fn try_acquire_many(&self, tokens: u32) -> bool {
        let mut bucket = self.refilled();
        if bucket.tokens >= tokens as f64 {
            bucket.tokens -= tokens as f64;
            true
        } else {
            false
        }
    }

    /// Takes one token, waiting until it is available.
    pub async fn acquire(&self) {
        self.acquire_many(1).await;
    }

    /// Takes `tokens` tokens, waiting until they are available. A request
    /// larger than the burst size is allowed; it simply waits longer.
    pub async fn acquire_many(&self, tokens: u32) {
        match self.reserve(tokens as f64) {
            Some(wait) if wait.is_zero() => {}
            Some(wait) => tokio::time::sleep(wait).await,
            None => std::future::pending::<()>().await,
        }
    }

    /// Takes one token if it can be had within `max_wait`; otherwise takes
    /// nothing and returns [`RateLimitedError`] immediately, without waiting.
    pub async fn acquire_timeout(&self, max_wait: Duration) -> Result<(), RateLimitedError> {
        let wait = {
            let mut bucket = self.refilled();
            if bucket.tokens >= 1.0 {
                bucket.tokens -= 1.0;
                Duration::ZERO
            } else if self.rate <= 0.0 {
                return Err(RateLimitedError);
            } else {
                let wait = seconds((1.0 - bucket.tokens) / self.rate);
                if wait > max_wait {
                    return Err(RateLimitedError);
                }
                bucket.tokens -= 1.0;
                wait
            }
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        Ok(())
    }

    /// The tokens available right now (fractional; negative while callers
    /// are queued).
    pub fn available(&self) -> f64 {
        self.refilled().tokens
    }
}

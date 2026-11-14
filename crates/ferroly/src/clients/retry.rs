//! Retry policy, jitter, budgets and drivers.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// `tokio::time::Instant` follows a paused test clock; in production it is the
// same monotonic clock as `std::time::Instant`.
use tokio::time::Instant;

use super::rng;

/// Configures a bounded retry loop with optional exponential backoff.
///
/// This is the backoff *schedule*. Jitter, a total time limit and a retry
/// budget are layered on with [`Retry`] — either `Retry::new(policy)` or the
/// shortcuts [`with_jitter`](RetryPolicy::with_jitter),
/// [`with_max_elapsed`](RetryPolicy::with_max_elapsed) and
/// [`with_budget`](RetryPolicy::with_budget).
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

    /// An exponential policy: waits `wait`, `2 * wait`, `4 * wait`, … capped
    /// at `max_backoff`.
    pub fn exponential(max_retries: u32, wait: Duration, max_backoff: Duration) -> Self {
        Self {
            max_retries,
            wait,
            exponential: true,
            max_backoff: Some(max_backoff),
        }
    }

    /// The un-jittered delay before the retry numbered `attempt` (0-based).
    pub fn backoff_for(&self, attempt: u32) -> Duration {
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

    /// This schedule with random jitter applied to every delay.
    pub fn with_jitter(self, jitter: Jitter) -> Retry {
        Retry::new(self).jitter(jitter)
    }

    /// This schedule with a limit on the total time spent retrying.
    pub fn with_max_elapsed(self, max_elapsed: Duration) -> Retry {
        Retry::new(self).max_elapsed(max_elapsed)
    }

    /// This schedule drawing its retries from a shared [`RetryBudget`].
    pub fn with_budget(self, budget: Arc<RetryBudget>) -> Retry {
        Retry::new(self).budget(budget)
    }
}

/// How a retry delay is randomised.
///
/// Without jitter, clients that failed together retry together, and the
/// synchronized waves can keep a recovering service down. Jitter spreads them
/// out. The randomness comes from a small internal pseudo-random generator
/// seeded from the clock and address-space layout: adequate for spreading
/// load, **not** cryptographically secure, and not for any security purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Jitter {
    /// Use the schedule's delay exactly (deterministic).
    #[default]
    None,
    /// "Full jitter": a uniformly random delay in `[0, delay)`. Spreads load
    /// best; the usual choice.
    Full,
    /// "Equal jitter": half the delay plus a random part of the other half —
    /// a delay in `[delay / 2, delay)`. Keeps a minimum wait.
    Equal,
}

impl Jitter {
    fn apply(self, delay: Duration) -> Duration {
        match self {
            Jitter::None => delay,
            Jitter::Full => delay.mul_f64(rng::fraction()),
            Jitter::Equal => {
                let half = delay / 2;
                half + half.mul_f64(rng::fraction())
            }
        }
    }
}

/// What to do after a failed attempt — the return value of a
/// [`Retry::run_with`] / [`retry_with`] decision hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    /// Do not retry; return this error to the caller.
    Stop,
    /// Retry after the policy's (jittered) backoff delay.
    Retry,
    /// Retry after exactly this delay instead of the policy's — for a
    /// server-provided `Retry-After` (see [`parse_retry_after`]). The delay is
    /// not jittered. If it exceeds the configured
    /// [`max_retry_after`](Retry::max_retry_after) the retry loop stops
    /// instead of waiting less than the server asked for.
    RetryAfter(Duration),
}

impl From<bool> for RetryDecision {
    /// `true` is [`Retry`](RetryDecision::Retry), `false` is
    /// [`Stop`](RetryDecision::Stop).
    fn from(retry: bool) -> Self {
        if retry {
            RetryDecision::Retry
        } else {
            RetryDecision::Stop
        }
    }
}

/// Context handed to a retry decision hook after each failed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RetryAttempt {
    /// The number of the attempt that just failed, starting at 1.
    pub attempt: u32,
    /// How many retries the policy still allows (at least 1 when the hook is
    /// called).
    pub retries_left: u32,
    /// Time since the first attempt started.
    pub elapsed: Duration,
    /// The delay the policy would wait before the next attempt (after
    /// jitter).
    pub delay: Duration,
}

/// A token bucket that caps how many retries a group of calls may spend, so a
/// widespread outage cannot multiply the load on a struggling dependency
/// ("retry storm").
///
/// Share one budget (in an [`Arc`]) between every caller of the same
/// dependency. Each retry withdraws one token; when the bucket is empty,
/// callers stop retrying and return their error. Tokens come back at
/// `refill_per_second`, and optionally a fraction of a token per call
/// ([`with_deposit_per_call`](Self::with_deposit_per_call)) — `0.1` means
/// "retries may add at most about 10 % to the request volume".
///
/// ```
/// use std::sync::Arc;
/// use std::time::Duration;
/// use ferroly::clients::{Jitter, RetryBudget, RetryPolicy};
///
/// // At most a burst of 10 retries, refilling at 1 retry per second.
/// let budget = Arc::new(RetryBudget::new(10, 1.0));
/// let retry = RetryPolicy::default()
///     .with_budget(budget.clone())
///     .jitter(Jitter::Full);
/// # let _ = retry;
/// assert_eq!(budget.available(), 10.0);
/// ```
#[derive(Debug)]
pub struct RetryBudget {
    capacity: f64,
    refill_per_second: f64,
    deposit_per_call: f64,
    state: Mutex<BudgetState>,
}

#[derive(Debug)]
struct BudgetState {
    tokens: f64,
    last_refill: Instant,
}

fn non_negative(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        0.0
    }
}

impl RetryBudget {
    /// A budget holding at most `capacity` retries (and starting full),
    /// refilled continuously at `refill_per_second` (which may be `0.0`).
    pub fn new(capacity: u32, refill_per_second: f64) -> Self {
        RetryBudget {
            capacity: capacity as f64,
            refill_per_second: non_negative(refill_per_second),
            deposit_per_call: 0.0,
            state: Mutex::new(BudgetState {
                tokens: capacity as f64,
                last_refill: Instant::now(),
            }),
        }
    }

    /// Additionally credits `tokens` (typically a fraction such as `0.1`) for
    /// every call that goes through a [`Retry`] using this budget.
    pub fn with_deposit_per_call(mut self, tokens: f64) -> Self {
        self.deposit_per_call = non_negative(tokens);
        self
    }

    fn refilled(&self) -> std::sync::MutexGuard<'_, BudgetState> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(state.last_refill);
        state.last_refill = now;
        state.tokens =
            (state.tokens + elapsed.as_secs_f64() * self.refill_per_second).min(self.capacity);
        state
    }

    /// Takes one retry token. Returns `false` (and takes nothing) if less
    /// than one whole token is available.
    pub fn try_withdraw(&self) -> bool {
        let mut state = self.refilled();
        if state.tokens >= 1.0 {
            state.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Records one call, crediting the per-call deposit. [`Retry`] does this
    /// for you once per `run`.
    pub fn record_call(&self) {
        if self.deposit_per_call > 0.0 {
            let mut state = self.refilled();
            state.tokens = (state.tokens + self.deposit_per_call).min(self.capacity);
        }
    }

    /// The tokens currently available (fractional).
    pub fn available(&self) -> f64 {
        self.refilled().tokens
    }
}

/// A [`RetryPolicy`] plus the runtime behaviour around it: jitter, a total
/// time limit, a cap on server-requested delays, and a shared retry budget.
///
/// ```
/// use std::time::Duration;
/// use ferroly::clients::{Jitter, Retry, RetryDecision, RetryPolicy};
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// let retry = Retry::new(RetryPolicy::exponential(
///         4, Duration::from_millis(1), Duration::from_millis(20)))
///     .jitter(Jitter::Full)
///     .max_elapsed(Duration::from_secs(5));
///
/// let mut calls = 0;
/// let result: Result<&str, &str> = retry
///     .run_with(
///         // The hook sees the error and may override the delay.
///         |err, _attempt| match *err {
///             "throttled" => RetryDecision::RetryAfter(Duration::from_millis(2)),
///             "transient" => RetryDecision::Retry,
///             _ => RetryDecision::Stop,
///         },
///         || {
///             calls += 1;
///             let n = calls;
///             async move {
///                 match n {
///                     1 => Err("transient"),
///                     2 => Err("throttled"),
///                     _ => Ok("done"),
///                 }
///             }
///         },
///     )
///     .await;
/// assert_eq!(result, Ok("done"));
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct Retry {
    policy: RetryPolicy,
    jitter: Jitter,
    max_elapsed: Option<Duration>,
    max_retry_after: Option<Duration>,
    budget: Option<Arc<RetryBudget>>,
}

impl From<RetryPolicy> for Retry {
    fn from(policy: RetryPolicy) -> Self {
        Retry::new(policy)
    }
}

impl From<&RetryPolicy> for Retry {
    fn from(policy: &RetryPolicy) -> Self {
        Retry::new(policy.clone())
    }
}

impl Default for Retry {
    /// The default [`RetryPolicy`] with no jitter, time limit or budget.
    fn default() -> Self {
        Retry::new(RetryPolicy::default())
    }
}

impl Retry {
    /// Wraps a schedule with no jitter, no time limit, no budget, and a
    /// 5-minute cap on server-requested delays.
    pub fn new(policy: RetryPolicy) -> Self {
        Retry {
            policy,
            jitter: Jitter::None,
            max_elapsed: None,
            max_retry_after: Some(Duration::from_secs(300)),
            budget: None,
        }
    }

    /// Sets how delays are randomised. Default [`Jitter::None`].
    pub fn jitter(mut self, jitter: Jitter) -> Self {
        self.jitter = jitter;
        self
    }

    /// Limits the total time spent, measured from the start of the first
    /// attempt: no retry is scheduled whose delay would end after this. (An
    /// attempt already in flight is not interrupted — combine with a timeout
    /// for that.)
    pub fn max_elapsed(mut self, max_elapsed: Duration) -> Self {
        self.max_elapsed = Some(max_elapsed);
        self
    }

    /// The longest [`RetryDecision::RetryAfter`] delay that is honoured;
    /// a longer one stops the retry loop. Default 5 minutes. `None` removes
    /// the cap, which lets a server stall the caller for as long as it likes.
    pub fn max_retry_after(mut self, cap: Option<Duration>) -> Self {
        self.max_retry_after = cap;
        self
    }

    /// Draws every retry from `budget`; when it is empty the loop stops.
    pub fn budget(mut self, budget: Arc<RetryBudget>) -> Self {
        self.budget = Some(budget);
        self
    }

    /// The underlying schedule.
    pub fn policy(&self) -> &RetryPolicy {
        &self.policy
    }

    /// The delay before the retry numbered `attempt` (0-based), with jitter
    /// applied — random unless the jitter is [`Jitter::None`].
    pub fn delay_for(&self, attempt: u32) -> Duration {
        self.jitter.apply(self.policy.backoff_for(attempt))
    }

    /// Runs `op` until it succeeds, `retry_if` returns `false`, or the
    /// retries, time limit or budget run out. The last error is returned.
    pub async fn run<T, E, P, F, Fut>(&self, retry_if: P, op: F) -> Result<T, E>
    where
        P: Fn(&E) -> bool,
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        self.run_with(|error, _| RetryDecision::from(retry_if(error)), op)
            .await
    }

    /// Like [`run`](Self::run), with a decision hook that is called after
    /// every failed attempt that still has a retry available. The hook
    /// receives the error and a [`RetryAttempt`] and returns a
    /// [`RetryDecision`] — including [`RetryDecision::RetryAfter`] to honour
    /// a delay the server asked for. It is also the place to log or count
    /// attempts.
    pub async fn run_with<T, E, D, F, Fut>(&self, mut decide: D, mut op: F) -> Result<T, E>
    where
        D: FnMut(&E, &RetryAttempt) -> RetryDecision,
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let started = Instant::now();
        if let Some(budget) = &self.budget {
            budget.record_call();
        }
        let mut retries = 0u32;
        loop {
            let error = match op().await {
                Ok(value) => return Ok(value),
                Err(error) => error,
            };
            if retries >= self.policy.max_retries {
                return Err(error);
            }
            let planned = self.delay_for(retries);
            let attempt = RetryAttempt {
                attempt: retries + 1,
                retries_left: self.policy.max_retries - retries,
                elapsed: started.elapsed(),
                delay: planned,
            };
            let delay = match decide(&error, &attempt) {
                RetryDecision::Stop => return Err(error),
                RetryDecision::Retry => planned,
                RetryDecision::RetryAfter(requested) => {
                    if self.max_retry_after.is_some_and(|cap| requested > cap) {
                        return Err(error);
                    }
                    requested
                }
            };
            if let Some(max_elapsed) = self.max_elapsed {
                if started.elapsed().saturating_add(delay) > max_elapsed {
                    return Err(error);
                }
            }
            if let Some(budget) = &self.budget {
                if !budget.try_withdraw() {
                    return Err(error);
                }
            }
            tokio::time::sleep(delay).await;
            retries += 1;
        }
    }
}

/// Runs `op` until it succeeds or the policy's retries are exhausted.
///
/// The predicate `retry_if` decides whether a given error is retryable; return
/// `false` to fail fast. The last error is returned on exhaustion. Delays are
/// the policy's exact backoff — for jitter, a time limit, a budget or
/// server-provided delays use [`Retry`] / [`retry_with`].
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

/// The free-function form of [`Retry::run_with`].
pub async fn retry_with<T, E, D, F, Fut>(retry: &Retry, decide: D, op: F) -> Result<T, E>
where
    D: FnMut(&E, &RetryAttempt) -> RetryDecision,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    retry.run_with(decide, op).await
}

// ---- Retry-After -----------------------------------------------------------

/// Parses an HTTP `Retry-After` header value into a delay.
///
/// Accepts both forms of RFC 9110 §10.2.3: a number of seconds (`"120"`) and
/// an HTTP date in the preferred IMF-fixdate format (`"Wed, 21 Oct 2015
/// 07:28:00 GMT"`), which is converted using the system clock (a date in the
/// past yields a zero delay). Returns `None` for anything else, including the
/// obsolete RFC 850 and asctime date formats.
///
/// ```
/// use std::time::Duration;
/// use ferroly::clients::{parse_retry_after, RetryDecision};
///
/// assert_eq!(parse_retry_after("120"), Some(Duration::from_secs(120)));
/// assert_eq!(parse_retry_after("soon"), None);
///
/// // In a retry hook:
/// let header: Option<&str> = Some("3");
/// let decision = match header.and_then(parse_retry_after) {
///     Some(delay) => RetryDecision::RetryAfter(delay),
///     None => RetryDecision::Retry,
/// };
/// assert_eq!(decision, RetryDecision::RetryAfter(Duration::from_secs(3)));
/// ```
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    parse_retry_after_at(value, now)
}

fn parse_retry_after_at(value: &str, now_unix: u64) -> Option<Duration> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        return value.parse().ok().map(Duration::from_secs);
    }
    let at = parse_imf_fixdate(value)?;
    Some(Duration::from_secs(at.saturating_sub(now_unix)))
}

/// Parses `"Sun, 06 Nov 1994 08:49:37 GMT"` into Unix seconds.
fn parse_imf_fixdate(value: &str) -> Option<u64> {
    let mut fields = value.split(' ');
    let weekday = fields.next()?;
    if weekday.len() != 4 || !weekday.ends_with(',') {
        return None;
    }
    let day = two_digits(fields.next()?)?;
    let month = match fields.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year = fields.next()?;
    if year.len() != 4 || !year.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let year: u64 = year.parse().ok()?;
    let mut clock = fields.next()?.split(':');
    let hour = two_digits(clock.next()?)?;
    let minute = two_digits(clock.next()?)?;
    let second = two_digits(clock.next()?)?;
    if clock.next().is_some() || fields.next() != Some("GMT") || fields.next().is_some() {
        return None;
    }
    if year < 1970 || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    // Days since 1970-01-01 (the civil-from-days algorithm, inverted).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y / 400;
    let year_of_era = y % 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = (era * 146_097 + day_of_era).checked_sub(719_468)?;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

fn two_digits(text: &str) -> Option<u64> {
    if text.len() == 2 && text.bytes().all(|b| b.is_ascii_digit()) {
        text.parse().ok()
    } else {
        None
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
        assert_eq!(policy.backoff_for(200), Duration::from_millis(500)); // saturates
        let same =
            RetryPolicy::exponential(10, Duration::from_millis(100), Duration::from_millis(500));
        assert_eq!(same.backoff_for(3), Duration::from_millis(500));
    }

    #[test]
    fn retry_after_seconds_and_dates() {
        assert_eq!(parse_retry_after("0"), Some(Duration::ZERO));
        assert_eq!(parse_retry_after(" 120 "), Some(Duration::from_secs(120)));
        for bad in ["", "-1", "1.5", "1e3", "ten", "+5", "5s"] {
            assert_eq!(parse_retry_after(bad), None, "{bad:?}");
        }
        // The RFC 9110 example date is 784111777 seconds after the epoch.
        let date = "Sun, 06 Nov 1994 08:49:37 GMT";
        assert_eq!(parse_imf_fixdate(date), Some(784_111_777));
        assert_eq!(
            parse_retry_after_at(date, 784_111_777 - 90),
            Some(Duration::from_secs(90))
        );
        // A date in the past means "now".
        assert_eq!(
            parse_retry_after_at(date, 784_111_777 + 5),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_imf_fixdate("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(
            parse_imf_fixdate("Tue, 29 Feb 2000 12:00:00 GMT"),
            Some(951_825_600)
        );
        assert_eq!(
            parse_imf_fixdate("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(1_445_412_480)
        );
        for bad in [
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
            "Sun, 06 Nov 1994 08:49:37",
            "Sun, 06 Nov 1994 08:49:37 UTC",
            "Sun, 6 Nov 1994 08:49:37 GMT",
            "Sun, 06 Foo 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 24:00:00 GMT",
            "Sun, 06 Nov 1969 08:49:37 GMT",
            "Sun, 06 Nov 1994 08:49 GMT",
        ] {
            assert_eq!(parse_imf_fixdate(bad), None, "{bad:?}");
        }
    }
}

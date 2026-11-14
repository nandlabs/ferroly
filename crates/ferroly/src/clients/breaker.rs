//! A three-state circuit breaker (closed / open / half-open).

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

// `tokio::time::Instant` respects a paused test clock (`tokio::time::pause`),
// making the cooldown deterministically testable; in production it tracks real
// time exactly like `std::time::Instant`.
use tokio::time::Instant;

use ferroly::clients::error::{CircuitOpenError, PolicyError};

/// The state of a [`CircuitBreaker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Requests flow normally; failures are counted.
    Closed,
    /// Requests are rejected until the cooldown timeout elapses.
    Open,
    /// A limited number of trial requests probe whether the dependency recovered.
    HalfOpen,
}

/// Tuning for a [`CircuitBreaker`].
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Consecutive failures in `Closed` that trip the breaker to `Open`.
    pub failure_threshold: u32,
    /// Successful trials in `HalfOpen` that close the breaker.
    pub success_threshold: u32,
    /// Maximum concurrent trial requests permitted in `HalfOpen`.
    pub max_half_open: u32,
    /// How long the breaker stays `Open` before allowing trials.
    pub timeout: Duration,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            success_threshold: 2,
            max_half_open: 1,
            timeout: Duration::from_secs(30),
        }
    }
}

/// A failure-rate trip condition over a sliding time window, used alongside
/// the consecutive-failure count.
///
/// The breaker trips when, within the last `window`, at least `min_calls`
/// outcomes were recorded and the fraction that failed is at least
/// `threshold`. Unlike the consecutive count, this catches a dependency that
/// fails *often* but not *every time* (a success never resets the window).
///
/// To trip on the rate only, set
/// [`failure_threshold`](CircuitBreakerConfig::failure_threshold) to
/// `u32::MAX`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FailureRate {
    /// The length of the sliding window. It is tracked in ten buckets, so
    /// outcomes age out with a granularity of a tenth of the window.
    pub window: Duration,
    /// The minimum number of recorded outcomes in the window before the rate
    /// is evaluated (so one failure out of one call does not trip).
    pub min_calls: u32,
    /// The failure fraction, `0.0..=1.0`, at or above which the breaker
    /// trips. `0.5` means "half of the calls failed".
    pub threshold: f64,
}

/// How a finished call counts toward a [`CircuitBreaker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The dependency worked.
    Success,
    /// The dependency failed (timeout, connection error, 5xx, overload).
    Failure,
    /// The call says nothing about the dependency's health and must not be
    /// counted either way — the caller's own mistake (a `400`, `401`, `404`,
    /// `422`), or a cancelled call. In `HalfOpen` it frees the trial slot
    /// without moving the breaker.
    Ignored,
}

/// A point-in-time view of a breaker's counters, for metrics and logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct BreakerMetrics {
    /// The current state.
    pub state: CircuitState,
    /// Calls recorded as successful since creation.
    pub successes: u64,
    /// Calls recorded as failed since creation.
    pub failures: u64,
    /// Calls rejected because the breaker was open or out of trial slots.
    pub rejected: u64,
    /// How many times the breaker has tripped to `Open`.
    pub trips: u64,
    /// The current run of consecutive failures (in `Closed`).
    pub consecutive_failures: u32,
    /// Trial calls currently in flight (in `HalfOpen`).
    pub half_open_in_flight: u32,
}

const WINDOW_BUCKETS: usize = 10;

#[derive(Debug, Clone, Copy, Default)]
struct Bucket {
    epoch: u64,
    successes: u32,
    failures: u32,
}

/// A sliding window of outcomes in [`WINDOW_BUCKETS`] time buckets.
#[derive(Debug)]
struct Window {
    rate: FailureRate,
    origin: Instant,
    bucket_nanos: u128,
    buckets: [Bucket; WINDOW_BUCKETS],
}

impl Window {
    fn new(rate: FailureRate) -> Self {
        Window {
            rate,
            origin: Instant::now(),
            bucket_nanos: (rate.window.as_nanos() / WINDOW_BUCKETS as u128).max(1),
            buckets: [Bucket::default(); WINDOW_BUCKETS],
        }
    }

    /// The 1-based bucket number of the current instant (0 marks "unused").
    fn epoch(&self) -> u64 {
        let nanos = Instant::now()
            .saturating_duration_since(self.origin)
            .as_nanos();
        (nanos / self.bucket_nanos) as u64 + 1
    }

    fn record(&mut self, failure: bool) {
        let epoch = self.epoch();
        let bucket = &mut self.buckets[(epoch % WINDOW_BUCKETS as u64) as usize];
        if bucket.epoch != epoch {
            *bucket = Bucket {
                epoch,
                successes: 0,
                failures: 0,
            };
        }
        if failure {
            bucket.failures += 1;
        } else {
            bucket.successes += 1;
        }
    }

    /// Whether the failure rate over the live buckets meets the threshold.
    fn tripped(&self) -> bool {
        let epoch = self.epoch();
        let (mut total, mut failures) = (0u64, 0u64);
        for bucket in &self.buckets {
            if bucket.epoch != 0 && bucket.epoch + WINDOW_BUCKETS as u64 > epoch {
                total += (bucket.successes + bucket.failures) as u64;
                failures += bucket.failures as u64;
            }
        }
        total > 0
            && total >= self.rate.min_calls as u64
            && failures as f64 / total as f64 >= self.rate.threshold
    }

    fn reset(&mut self) {
        self.buckets = [Bucket::default(); WINDOW_BUCKETS];
    }
}

/// One admitted half-open trial.
#[derive(Debug)]
struct Probe {
    id: u64,
    admitted_at: Instant,
    /// Admitted through `can_execute` (no permit to release it).
    anonymous: bool,
}

#[derive(Debug)]
struct State {
    circuit: CircuitState,
    /// Bumped on every state change; a permit from an older generation no
    /// longer counts.
    generation: u64,
    failures: u32,
    successes: u32,
    opened_at: Option<Instant>,
    probes: Vec<Probe>,
    next_probe_id: u64,
    window: Option<Window>,
    total_successes: u64,
    total_failures: u64,
    rejected: u64,
    trips: u64,
}

type Listener = Arc<dyn Fn(CircuitState, CircuitState) + Send + Sync>;
type Transitions = Vec<(CircuitState, CircuitState)>;

struct Inner {
    config: CircuitBreakerConfig,
    half_open_timeout: Duration,
    listeners: Vec<Listener>,
    state: Mutex<State>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        // The state is plain counters, valid after any panic; do not let one
        // panicking caller disable the breaker for everyone.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Runs the listeners for the transitions collected under the lock. Must
    /// be called with the lock released, so a listener may use the breaker.
    fn notify(&self, transitions: Transitions) {
        for (from, to) in transitions {
            for listener in &self.listeners {
                listener(from, to);
            }
        }
    }

    fn transition(&self, s: &mut State, to: CircuitState, transitions: &mut Transitions) {
        let from = s.circuit;
        s.circuit = to;
        s.generation += 1;
        s.failures = 0;
        s.successes = 0;
        s.probes.clear();
        if let Some(window) = &mut s.window {
            window.reset();
        }
        match to {
            CircuitState::Open => {
                s.opened_at = Some(Instant::now());
                s.trips += 1;
            }
            CircuitState::Closed => s.opened_at = None,
            CircuitState::HalfOpen => {}
        }
        transitions.push((from, to));
    }

    fn push_probe(&self, s: &mut State, anonymous: bool) -> u64 {
        let id = s.next_probe_id;
        s.next_probe_id += 1;
        s.probes.push(Probe {
            id,
            admitted_at: Instant::now(),
            anonymous,
        });
        id
    }

    /// Admits or rejects a call. Returns the generation and, for a half-open
    /// trial, the probe id.
    fn admit(
        &self,
        s: &mut State,
        anonymous: bool,
        transitions: &mut Transitions,
    ) -> Result<(u64, Option<u64>), CircuitOpenError> {
        match s.circuit {
            CircuitState::Closed => Ok((s.generation, None)),
            CircuitState::Open => {
                let elapsed = s.opened_at.map(|t| t.elapsed()).unwrap_or_default();
                if elapsed >= self.config.timeout {
                    self.transition(s, CircuitState::HalfOpen, transitions);
                    let id = self.push_probe(s, anonymous);
                    Ok((s.generation, Some(id)))
                } else {
                    s.rejected += 1;
                    Err(CircuitOpenError)
                }
            }
            CircuitState::HalfOpen => {
                // A trial that was admitted but never reported (its caller
                // was cancelled, panicked or forgot) must not hold its slot
                // forever: after the probe timeout the slot is reclaimed.
                let timeout = self.half_open_timeout;
                s.probes.retain(|p| p.admitted_at.elapsed() < timeout);
                if (s.probes.len() as u32) < self.config.max_half_open.max(1) {
                    let id = self.push_probe(s, anonymous);
                    Ok((s.generation, Some(id)))
                } else {
                    s.rejected += 1;
                    Err(CircuitOpenError)
                }
            }
        }
    }

    /// Applies an outcome in the current state.
    fn apply(&self, s: &mut State, outcome: Outcome, transitions: &mut Transitions) {
        match outcome {
            Outcome::Success => s.total_successes += 1,
            Outcome::Failure => s.total_failures += 1,
            Outcome::Ignored => return,
        }
        let failed = outcome == Outcome::Failure;
        match s.circuit {
            CircuitState::Closed => {
                if failed {
                    s.failures = s.failures.saturating_add(1);
                } else {
                    s.failures = 0;
                }
                let rate_tripped = match &mut s.window {
                    Some(window) => {
                        window.record(failed);
                        failed && window.tripped()
                    }
                    None => false,
                };
                if failed && (s.failures >= self.config.failure_threshold || rate_tripped) {
                    self.transition(s, CircuitState::Open, transitions);
                }
            }
            CircuitState::HalfOpen => {
                if failed {
                    self.transition(s, CircuitState::Open, transitions);
                } else {
                    s.successes += 1;
                    if s.successes >= self.config.success_threshold {
                        self.transition(s, CircuitState::Closed, transitions);
                    }
                }
            }
            CircuitState::Open => {
                // A stray report while open; if it failed, refresh the cooldown.
                if failed {
                    s.opened_at = Some(Instant::now());
                }
            }
        }
    }

    /// Resolves a permit.
    fn resolve(&self, generation: u64, probe: Option<u64>, outcome: Outcome) {
        let mut transitions = Vec::new();
        {
            let mut s = self.lock();
            if s.generation != generation {
                // The breaker changed state since the permit was issued; the
                // outcome describes a period that is over (and the probe slot,
                // if any, was already cleared by the transition).
                return;
            }
            if let Some(id) = probe {
                s.probes.retain(|p| p.id != id);
            }
            self.apply(&mut s, outcome, &mut transitions);
        }
        self.notify(transitions);
    }
}

/// A concurrency-safe circuit breaker.
///
/// # Preferred: the permit API
///
/// [`try_acquire`](CircuitBreaker::try_acquire) returns a [`Permit`] that you
/// resolve with the call's outcome. A permit that is dropped unresolved — the
/// caller was cancelled, timed out from outside, or panicked — releases its
/// half-open trial slot automatically, so a breaker can never be left stuck:
///
/// ```
/// use ferroly::clients::{CircuitBreaker, CircuitBreakerConfig};
///
/// # async fn call() -> Result<(), ()> { Ok(()) }
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// let cb = CircuitBreaker::new(CircuitBreakerConfig::default());
/// match cb.try_acquire() {
///     Ok(permit) => match call().await {
///         Ok(()) => permit.success(),
///         Err(()) => permit.failure(),
///     },
///     Err(_open) => { /* short-circuit: serve a fallback */ }
/// }
/// # }
/// ```
///
/// [`call`](CircuitBreaker::call) and [`call_with`](CircuitBreaker::call_with)
/// wrap that pattern around an async operation.
///
/// # Legacy: check, then report
///
/// [`can_execute`](CircuitBreaker::can_execute) followed by
/// [`on_execution`](CircuitBreaker::on_execution) still works. Because nothing
/// ties the report to the check, a caller that never reports would hold a
/// half-open slot; such a slot is reclaimed after the
/// [half-open timeout](CircuitBreakerBuilder::half_open_timeout).
///
/// ```
/// use ferroly::clients::{CircuitBreaker, CircuitBreakerConfig};
///
/// let cb = CircuitBreaker::new(CircuitBreakerConfig::default());
/// if cb.can_execute().is_ok() {
///     // perform the call, then:
///     cb.on_execution(true);
/// }
/// ```
pub struct CircuitBreaker {
    inner: Arc<Inner>,
}

impl fmt::Debug for CircuitBreaker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CircuitBreaker")
            .field("config", &self.inner.config)
            .field("half_open_timeout", &self.inner.half_open_timeout)
            .field("state", &*self.inner.lock())
            .finish_non_exhaustive()
    }
}

/// Configures a [`CircuitBreaker`] beyond its [`CircuitBreakerConfig`];
/// created by [`CircuitBreaker::builder`].
///
/// ```
/// use std::time::Duration;
/// use ferroly::clients::{CircuitBreaker, CircuitBreakerConfig, FailureRate};
///
/// let cb = CircuitBreaker::builder(CircuitBreakerConfig::default())
///     .failure_rate(FailureRate {
///         window: Duration::from_secs(60),
///         min_calls: 20,
///         threshold: 0.5,
///     })
///     .half_open_timeout(Duration::from_secs(10))
///     .on_state_change(|from, to| eprintln!("breaker: {from:?} -> {to:?}"))
///     .build();
/// # let _ = cb;
/// ```
pub struct CircuitBreakerBuilder {
    config: CircuitBreakerConfig,
    half_open_timeout: Option<Duration>,
    failure_rate: Option<FailureRate>,
    listeners: Vec<Listener>,
}

impl fmt::Debug for CircuitBreakerBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CircuitBreakerBuilder")
            .field("config", &self.config)
            .field("half_open_timeout", &self.half_open_timeout)
            .field("failure_rate", &self.failure_rate)
            .field("listeners", &self.listeners.len())
            .finish()
    }
}

impl CircuitBreakerBuilder {
    /// How long an admitted half-open trial may stay unreported before its
    /// slot is given to another trial. Defaults to the cooldown
    /// ([`CircuitBreakerConfig::timeout`]). Set it above the longest time a
    /// healthy call can take.
    pub fn half_open_timeout(mut self, timeout: Duration) -> Self {
        self.half_open_timeout = Some(timeout);
        self
    }

    /// Adds a sliding-window failure-rate trip condition (see
    /// [`FailureRate`]), in addition to the consecutive-failure threshold.
    pub fn failure_rate(mut self, rate: FailureRate) -> Self {
        self.failure_rate = Some(rate);
        self
    }

    /// Registers a callback invoked with `(from, to)` on every state change —
    /// for logging, metrics or alerting. It runs on the thread that caused the
    /// change, after the breaker's lock is released; keep it short and do not
    /// block. Several callbacks may be registered.
    pub fn on_state_change<F>(mut self, listener: F) -> Self
    where
        F: Fn(CircuitState, CircuitState) + Send + Sync + 'static,
    {
        self.listeners.push(Arc::new(listener));
        self
    }

    /// Builds the breaker, in the closed state.
    pub fn build(self) -> CircuitBreaker {
        let half_open_timeout = self.half_open_timeout.unwrap_or(self.config.timeout);
        CircuitBreaker {
            inner: Arc::new(Inner {
                config: self.config,
                half_open_timeout,
                listeners: self.listeners,
                state: Mutex::new(State {
                    circuit: CircuitState::Closed,
                    generation: 0,
                    failures: 0,
                    successes: 0,
                    opened_at: None,
                    probes: Vec::new(),
                    next_probe_id: 0,
                    window: self.failure_rate.map(Window::new),
                    total_successes: 0,
                    total_failures: 0,
                    rejected: 0,
                    trips: 0,
                }),
            }),
        }
    }
}

impl CircuitBreaker {
    /// Creates a breaker in the closed state.
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self::builder(config).build()
    }

    /// Starts building a breaker with a failure-rate window, a half-open
    /// trial timeout or state-change callbacks.
    pub fn builder(config: CircuitBreakerConfig) -> CircuitBreakerBuilder {
        CircuitBreakerBuilder {
            config,
            half_open_timeout: None,
            failure_rate: None,
            listeners: Vec::new(),
        }
    }

    /// Returns the current circuit state.
    ///
    /// An `Open` breaker whose cooldown has elapsed still reports `Open`
    /// until the next call asks for admission.
    pub fn state(&self) -> CircuitState {
        self.inner.lock().circuit
    }

    /// A snapshot of the breaker's counters.
    pub fn metrics(&self) -> BreakerMetrics {
        let s = self.inner.lock();
        BreakerMetrics {
            state: s.circuit,
            successes: s.total_successes,
            failures: s.total_failures,
            rejected: s.rejected,
            trips: s.trips,
            consecutive_failures: s.failures,
            half_open_in_flight: s.probes.len() as u32,
        }
    }

    /// Asks to run one call. On admission returns a [`Permit`] to resolve
    /// with the outcome; otherwise [`CircuitOpenError`].
    ///
    /// `Open` becomes `HalfOpen` once the cooldown has elapsed, admitting
    /// this call as the first trial; `HalfOpen` admits up to
    /// [`max_half_open`](CircuitBreakerConfig::max_half_open) concurrent
    /// trials.
    pub fn try_acquire(&self) -> Result<Permit, CircuitOpenError> {
        let mut transitions = Vec::new();
        let admitted = {
            let mut s = self.inner.lock();
            self.inner.admit(&mut s, false, &mut transitions)
        };
        self.inner.notify(transitions);
        let (generation, probe) = admitted?;
        Ok(Permit {
            inner: self.inner.clone(),
            generation,
            probe,
            resolved: false,
        })
    }

    /// Runs `op` under the breaker: rejects with [`PolicyError::CircuitOpen`]
    /// when not admitted, otherwise records `Ok` as a success and any `Err`
    /// as a failure. If the returned future is dropped mid-call, the call is
    /// recorded as [`Outcome::Ignored`].
    pub async fn call<T, E, F, Fut>(&self, op: F) -> Result<T, PolicyError<E>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        self.call_with(|_| Outcome::Failure, op).await
    }

    /// Like [`call`](Self::call), with `classify` deciding how each error
    /// counts — return [`Outcome::Ignored`] for errors that are the caller's
    /// fault (bad request, unauthorized, not found) so they cannot open the
    /// breaker for everyone else.
    ///
    /// ```
    /// use ferroly::clients::{CircuitBreaker, CircuitBreakerConfig, Outcome};
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let cb = CircuitBreaker::new(CircuitBreakerConfig { failure_threshold: 1, ..Default::default() });
    /// let status_of = |status: &u16| if *status >= 500 { Outcome::Failure } else { Outcome::Ignored };
    ///
    /// // A 404 is the caller's problem: it does not trip the breaker.
    /// let _ = cb.call_with(status_of, || async { Err::<(), u16>(404) }).await;
    /// assert!(cb.try_acquire().is_ok());
    /// // A 503 is the dependency's problem.
    /// let _ = cb.call_with(status_of, || async { Err::<(), u16>(503) }).await;
    /// assert!(cb.try_acquire().is_err());
    /// # }
    /// ```
    pub async fn call_with<T, E, C, F, Fut>(&self, classify: C, op: F) -> Result<T, PolicyError<E>>
    where
        C: FnOnce(&E) -> Outcome,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let permit = self.try_acquire()?;
        match op().await {
            Ok(value) => {
                permit.success();
                Ok(value)
            }
            Err(error) => {
                permit.record(classify(&error));
                Err(PolicyError::Inner(error))
            }
        }
    }

    /// Checks whether an operation may proceed, transitioning `Open` → `HalfOpen`
    /// once the cooldown elapses. Occupies a trial slot when it admits a
    /// half-open probe; report the outcome with
    /// [`on_execution`](CircuitBreaker::on_execution).
    ///
    /// Prefer [`try_acquire`](CircuitBreaker::try_acquire): a slot taken here
    /// is only released by `on_execution` or, if that never comes, by the
    /// half-open timeout.
    pub fn can_execute(&self) -> Result<(), CircuitOpenError> {
        let mut transitions = Vec::new();
        let admitted = {
            let mut s = self.inner.lock();
            self.inner.admit(&mut s, true, &mut transitions)
        };
        self.inner.notify(transitions);
        admitted.map(|_| ())
    }

    /// Reports the outcome of an operation admitted by [`can_execute`](CircuitBreaker::can_execute).
    pub fn on_execution(&self, success: bool) {
        let mut transitions = Vec::new();
        {
            let mut s = self.inner.lock();
            if s.circuit == CircuitState::HalfOpen {
                // Free the oldest slot taken through `can_execute`.
                if let Some(index) = s.probes.iter().position(|p| p.anonymous) {
                    s.probes.remove(index);
                }
            }
            let outcome = if success {
                Outcome::Success
            } else {
                Outcome::Failure
            };
            self.inner.apply(&mut s, outcome, &mut transitions);
        }
        self.inner.notify(transitions);
    }

    /// Forces the breaker back to `Closed`, clearing its counters and window
    /// (an operator override). Permits issued before the reset no longer
    /// count.
    pub fn reset(&self) {
        let mut transitions = Vec::new();
        {
            let mut s = self.inner.lock();
            if s.circuit == CircuitState::Closed {
                s.failures = 0;
                if let Some(window) = &mut s.window {
                    window.reset();
                }
            } else {
                self.inner
                    .transition(&mut s, CircuitState::Closed, &mut transitions);
            }
        }
        self.inner.notify(transitions);
    }
}

/// Admission to run one call through a [`CircuitBreaker`], obtained from
/// [`CircuitBreaker::try_acquire`].
///
/// Resolve it with [`success`](Permit::success), [`failure`](Permit::failure),
/// [`ignore`](Permit::ignore) or [`record`](Permit::record).
///
/// **Dropping an unresolved permit records the call as
/// [`Outcome::Ignored`]**: a half-open trial slot is released immediately and
/// nothing is counted as a success or a failure. A cancelled caller therefore
/// can neither leak a slot nor trip the breaker; if you want a cancellation or
/// an external timeout to count against the dependency, call `failure()`
/// before dropping.
///
/// A permit is not tied to the breaker's lifetime and is `Send`, so it can be
/// moved into a spawned task or held for the duration of a streamed response.
/// If the breaker changes state before the permit is resolved, the outcome is
/// discarded.
#[must_use = "resolve the permit with success() / failure(); dropping it records an ignored call"]
pub struct Permit {
    inner: Arc<Inner>,
    generation: u64,
    probe: Option<u64>,
    resolved: bool,
}

impl fmt::Debug for Permit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Permit")
            .field("is_trial", &self.is_trial())
            .finish_non_exhaustive()
    }
}

impl Permit {
    /// Whether this call is a half-open trial (the breaker is probing
    /// recovery).
    pub fn is_trial(&self) -> bool {
        self.probe.is_some()
    }

    /// Records the call as successful.
    pub fn success(self) {
        self.record(Outcome::Success);
    }

    /// Records the call as a failure of the dependency.
    pub fn failure(self) {
        self.record(Outcome::Failure);
    }

    /// Releases the permit without counting the call either way.
    pub fn ignore(self) {
        self.record(Outcome::Ignored);
    }

    /// Records the given outcome.
    pub fn record(mut self, outcome: Outcome) {
        self.resolved = true;
        self.inner.resolve(self.generation, self.probe, outcome);
    }

    /// Records `Ok` as a success and `Err` as a failure.
    pub fn record_result<T, E>(self, result: &Result<T, E>) {
        self.record(if result.is_ok() {
            Outcome::Success
        } else {
            Outcome::Failure
        });
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        if !self.resolved {
            self.inner
                .resolve(self.generation, self.probe, Outcome::Ignored);
        }
    }
}

type Factory = Box<dyn Fn(&str) -> CircuitBreaker + Send + Sync>;

/// A set of circuit breakers keyed by name — one per host, endpoint, tenant
/// or model — created on first use.
///
/// ```
/// use ferroly::clients::{CircuitBreakerConfig, CircuitBreakerRegistry};
///
/// let breakers = CircuitBreakerRegistry::new(CircuitBreakerConfig::default());
/// let permit = breakers.get("api.example.com").try_acquire().unwrap();
/// permit.success();
/// // The same key returns the same breaker.
/// assert_eq!(breakers.get("api.example.com").metrics().successes, 1);
/// assert_eq!(breakers.len(), 1);
/// ```
///
/// Keys are never evicted automatically; if they are unbounded (for example
/// derived from request data) call [`remove`](Self::remove) or
/// [`clear`](Self::clear) yourself.
pub struct CircuitBreakerRegistry {
    factory: Factory,
    breakers: Mutex<HashMap<String, Arc<CircuitBreaker>>>,
}

impl fmt::Debug for CircuitBreakerRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CircuitBreakerRegistry")
            .field("keys", &self.keys())
            .finish_non_exhaustive()
    }
}

impl CircuitBreakerRegistry {
    /// A registry whose breakers all use `config`.
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self::with_factory(move |_| CircuitBreaker::new(config.clone()))
    }

    /// A registry that builds each breaker with `factory`, which receives the
    /// key — use it for a failure-rate window, per-key tuning, or a
    /// state-change callback that knows which key changed.
    ///
    /// ```
    /// use ferroly::clients::{CircuitBreaker, CircuitBreakerConfig, CircuitBreakerRegistry};
    ///
    /// let breakers = CircuitBreakerRegistry::with_factory(|key| {
    ///     let key = key.to_string();
    ///     CircuitBreaker::builder(CircuitBreakerConfig::default())
    ///         .on_state_change(move |from, to| eprintln!("{key}: {from:?} -> {to:?}"))
    ///         .build()
    /// });
    /// # let _ = breakers.get("a");
    /// ```
    pub fn with_factory<F>(factory: F) -> Self
    where
        F: Fn(&str) -> CircuitBreaker + Send + Sync + 'static,
    {
        CircuitBreakerRegistry {
            factory: Box::new(factory),
            breakers: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Arc<CircuitBreaker>>> {
        self.breakers.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The breaker for `key`, created on first use.
    pub fn get(&self, key: &str) -> Arc<CircuitBreaker> {
        let mut breakers = self.lock();
        if let Some(existing) = breakers.get(key) {
            return existing.clone();
        }
        let created = Arc::new((self.factory)(key));
        breakers.insert(key.to_string(), created.clone());
        created
    }

    /// The breaker for `key`, if one has been created.
    pub fn get_existing(&self, key: &str) -> Option<Arc<CircuitBreaker>> {
        self.lock().get(key).cloned()
    }

    /// Removes and returns the breaker for `key`. Callers still holding it
    /// keep a working breaker; the next [`get`](Self::get) creates a new one.
    pub fn remove(&self, key: &str) -> Option<Arc<CircuitBreaker>> {
        self.lock().remove(key)
    }

    /// Removes every breaker.
    pub fn clear(&self) {
        self.lock().clear();
    }

    /// The keys that have a breaker, sorted.
    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.lock().keys().cloned().collect();
        keys.sort();
        keys
    }

    /// The number of breakers.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no breaker has been created yet.
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// The state of every breaker, sorted by key — for a health endpoint or
    /// periodic metrics.
    pub fn states(&self) -> Vec<(String, CircuitState)> {
        let mut states: Vec<(String, CircuitState)> = self
            .lock()
            .iter()
            .map(|(key, breaker)| (key.clone(), breaker.state()))
            .collect();
        states.sort_by(|a, b| a.0.cmp(&b.0));
        states
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            failure_threshold: 3,
            success_threshold: 2,
            max_half_open: 1,
            timeout: Duration::from_millis(20),
        }
    }

    #[test]
    fn trips_open_after_threshold_failures() {
        let cb = CircuitBreaker::new(cfg());
        assert_eq!(cb.state(), CircuitState::Closed);
        for _ in 0..3 {
            assert!(cb.can_execute().is_ok());
            cb.on_execution(false);
        }
        assert_eq!(cb.state(), CircuitState::Open);
        assert!(cb.can_execute().is_err());
    }

    // `start_paused` freezes the tokio clock; `advance` moves it deterministically
    // past the cooldown, so these no longer race a wall-clock sleep.
    #[tokio::test(start_paused = true)]
    async fn recovers_through_half_open() {
        let cb = CircuitBreaker::new(cfg());
        for _ in 0..3 {
            let _ = cb.can_execute();
            cb.on_execution(false);
        }
        assert_eq!(cb.state(), CircuitState::Open);

        tokio::time::advance(Duration::from_millis(21)).await; // past the 20ms cooldown
                                                               // First probe admitted -> HalfOpen.
        assert!(cb.can_execute().is_ok());
        assert_eq!(cb.state(), CircuitState::HalfOpen);
        cb.on_execution(true);
        // Second success closes it.
        assert!(cb.can_execute().is_ok());
        cb.on_execution(true);
        assert_eq!(cb.state(), CircuitState::Closed);
    }

    #[tokio::test(start_paused = true)]
    async fn half_open_failure_reopens() {
        let cb = CircuitBreaker::new(cfg());
        for _ in 0..3 {
            let _ = cb.can_execute();
            cb.on_execution(false);
        }
        tokio::time::advance(Duration::from_millis(21)).await;
        assert!(cb.can_execute().is_ok());
        cb.on_execution(false);
        assert_eq!(cb.state(), CircuitState::Open);
    }
}

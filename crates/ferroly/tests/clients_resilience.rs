#![cfg(feature = "clients")]
//! Bulkhead, rate limiter, timeouts/deadlines and the composed `Policy`
//! pipeline. Timing runs on tokio's paused clock (exact virtual time).

use std::cell::Cell;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::clients::{
    with_timeout, Bulkhead, BulkheadFullError, CircuitBreaker, CircuitBreakerConfig, CircuitState,
    Deadline, ErrorClass, Jitter, Outcome, Policy, PolicyError, RateLimitedError, RateLimiter,
    RetryBudget, RetryDecision, RetryPolicy, TimeoutError,
};
use tokio::time::Instant;

const MS: Duration = Duration::from_millis(1);

// ---- bulkhead -----------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn bulkhead_limits_concurrency() {
    let bulkhead = Arc::new(Bulkhead::new(3));
    let running = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for i in 0..20u32 {
        let (bulkhead, running, peak) = (bulkhead.clone(), running.clone(), peak.clone());
        tasks.push(tokio::spawn(async move {
            bulkhead
                .run(async {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(10 * MS).await;
                    running.fetch_sub(1, Ordering::SeqCst);
                    i
                })
                .await
        }));
    }
    let mut done = Vec::new();
    for task in tasks {
        done.push(task.await.unwrap().unwrap());
    }
    done.sort_unstable();
    assert_eq!(done, (0..20).collect::<Vec<_>>());
    assert_eq!(peak.load(Ordering::SeqCst), 3);
    assert_eq!(bulkhead.available(), 3);
    assert_eq!(bulkhead.in_flight(), 0);
    assert_eq!(bulkhead.max_concurrent(), 3);
}

#[tokio::test(start_paused = true)]
async fn bulkhead_fail_fast_rejects_when_full() {
    let bulkhead = Bulkhead::new(2).fail_fast();
    let a = bulkhead.acquire().await.unwrap();
    let b = bulkhead.try_acquire().unwrap();
    assert_eq!(bulkhead.in_flight(), 2);
    assert_eq!(bulkhead.try_acquire().unwrap_err(), BulkheadFullError);

    // A rejected operation is never started.
    let started = Cell::new(false);
    let start = Instant::now();
    let out = bulkhead.run(async { started.set(true) }).await;
    assert_eq!(out, Err(BulkheadFullError));
    assert!(!started.get());
    assert_eq!(start.elapsed(), Duration::ZERO);

    drop(a);
    assert_eq!(bulkhead.run(async { 5 }).await, Ok(5));
    drop(b);
    assert_eq!(bulkhead.available(), 2);
    // At least one slot, whatever is asked for.
    assert_eq!(Bulkhead::new(0).max_concurrent(), 1);
}

#[tokio::test(start_paused = true)]
async fn bulkhead_queue_timeout_bounds_the_wait() {
    let bulkhead = Arc::new(Bulkhead::new(1).with_queue_timeout(50 * MS));
    let held = bulkhead.acquire().await.unwrap();

    // Nobody frees the slot: rejected after exactly the queue timeout.
    let start = Instant::now();
    assert_eq!(bulkhead.acquire().await.unwrap_err(), BulkheadFullError);
    assert_eq!(start.elapsed(), 50 * MS);

    // The slot frees up in time: admitted when it does.
    let waiter = tokio::spawn({
        let bulkhead = bulkhead.clone();
        async move {
            let start = Instant::now();
            let slot = bulkhead.acquire().await;
            (slot.is_ok(), start.elapsed())
        }
    });
    tokio::time::sleep(20 * MS).await;
    drop(held);
    assert_eq!(waiter.await.unwrap(), (true, 20 * MS));
    assert_eq!(bulkhead.available(), 1);
}

#[tokio::test(start_paused = true)]
async fn bulkhead_without_timeout_waits_and_cancellation_frees_the_slot() {
    let bulkhead = Arc::new(Bulkhead::new(1));
    // A running operation is cancelled: its slot must come back.
    let running = tokio::spawn({
        let bulkhead = bulkhead.clone();
        async move {
            bulkhead
                .run(tokio::time::sleep(Duration::from_secs(3600)))
                .await
        }
    });
    tokio::task::yield_now().await;
    assert_eq!(bulkhead.available(), 0);

    // A second caller queues behind it (no timeout: it just waits).
    let queued = tokio::spawn({
        let bulkhead = bulkhead.clone();
        async move { bulkhead.run(async { "ran" }).await }
    });
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert!(!queued.is_finished());

    running.abort();
    assert_eq!(queued.await.unwrap(), Ok("ran"));
    assert_eq!(bulkhead.available(), 1);

    // A cancelled waiter gives up its place in the queue.
    let held = bulkhead.acquire().await.unwrap();
    let waiter = tokio::spawn({
        let bulkhead = bulkhead.clone();
        async move { bulkhead.acquire().await.map(|_| ()) }
    });
    tokio::task::yield_now().await;
    waiter.abort();
    let _ = waiter.await;
    drop(held);
    assert_eq!(bulkhead.available(), 1);
}

// ---- rate limiter ---------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn rate_limiter_allows_the_burst_then_paces() {
    let limiter = RateLimiter::new(10.0, 5); // 10/s, burst 5
    let start = Instant::now();
    for _ in 0..5 {
        limiter.acquire().await;
    }
    assert_eq!(start.elapsed(), Duration::ZERO);
    // From here on: one every 100 ms.
    for n in 1..=10u32 {
        limiter.acquire().await;
        assert_eq!(start.elapsed(), 100 * MS * n);
    }
}

#[tokio::test(start_paused = true)]
async fn rate_limiter_try_acquire_and_refill() {
    let limiter = RateLimiter::new(2.0, 2);
    assert!(limiter.try_acquire());
    assert!(limiter.try_acquire());
    assert!(!limiter.try_acquire());
    tokio::time::advance(499 * MS).await;
    assert!(!limiter.try_acquire());
    tokio::time::advance(MS).await;
    assert!(limiter.try_acquire());
    // The bucket never holds more than the burst.
    tokio::time::advance(Duration::from_secs(3600)).await;
    assert_eq!(limiter.available(), 2.0);
    assert!(limiter.try_acquire_many(2));
    assert!(!limiter.try_acquire_many(1));
    // A refused request takes nothing.
    tokio::time::advance(500 * MS).await;
    assert!(!limiter.try_acquire_many(2));
    assert!(limiter.try_acquire());
}

#[tokio::test(start_paused = true)]
async fn rate_limiter_serves_waiters_in_order() {
    let limiter = Arc::new(RateLimiter::new(10.0, 1));
    limiter.acquire().await;
    let order: Arc<Mutex<Vec<(u32, Duration)>>> = Arc::default();
    let start = Instant::now();
    let mut tasks = Vec::new();
    for id in 0..4u32 {
        let (limiter, order) = (limiter.clone(), order.clone());
        tasks.push(tokio::spawn(async move {
            limiter.acquire().await;
            order.lock().unwrap().push((id, start.elapsed()));
        }));
        tokio::task::yield_now().await; // let it reserve its place
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(
        *order.lock().unwrap(),
        [(0, 100 * MS), (1, 200 * MS), (2, 300 * MS), (3, 400 * MS)]
    );
}

#[tokio::test(start_paused = true)]
async fn rate_limiter_acquire_timeout_refuses_long_waits() {
    let limiter = RateLimiter::new(1.0, 1);
    assert_eq!(limiter.acquire_timeout(Duration::ZERO).await, Ok(()));
    // The next token is a second away.
    let start = Instant::now();
    assert_eq!(
        limiter.acquire_timeout(999 * MS).await,
        Err(RateLimitedError)
    );
    assert_eq!(start.elapsed(), Duration::ZERO); // refused at once, not after waiting
                                                 // The refusal reserved nothing: a patient caller still waits just 1 s.
    assert_eq!(
        limiter.acquire_timeout(Duration::from_secs(1)).await,
        Ok(())
    );
    assert_eq!(start.elapsed(), Duration::from_secs(1));
}

#[tokio::test(start_paused = true)]
async fn rate_limiter_edge_configurations() {
    // 120 per minute = 2 per second, burst 120.
    let per = RateLimiter::per(120, Duration::from_secs(60));
    assert!(per.try_acquire_many(120));
    assert!(!per.try_acquire());
    tokio::time::advance(500 * MS).await;
    assert!(per.try_acquire());

    // A request larger than the burst waits for the difference.
    let limiter = RateLimiter::new(10.0, 2);
    let start = Instant::now();
    limiter.acquire_many(5).await;
    assert_eq!(start.elapsed(), 300 * MS);

    // A zero rate never refills: the burst is all there is.
    let frozen = RateLimiter::new(0.0, 1);
    assert!(frozen.try_acquire());
    tokio::time::advance(Duration::from_secs(3600)).await;
    assert!(!frozen.try_acquire());
    assert_eq!(
        frozen.acquire_timeout(Duration::from_secs(3600)).await,
        Err(RateLimitedError)
    );
    assert_eq!(
        with_timeout(Duration::from_secs(10), frozen.acquire()).await,
        Err(TimeoutError)
    );
    // Burst 0 is treated as 1; a NaN rate as 0.
    let odd = RateLimiter::new(f64::NAN, 0);
    assert!(odd.try_acquire());
    assert!(!odd.try_acquire());
}

// ---- timeouts and deadlines -------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn with_timeout_returns_the_value_or_times_out() {
    let start = Instant::now();
    let fast = with_timeout(100 * MS, async {
        tokio::time::sleep(30 * MS).await;
        "done"
    })
    .await;
    assert_eq!(fast, Ok("done"));
    assert_eq!(start.elapsed(), 30 * MS);

    let dropped = Arc::new(AtomicU32::new(0));
    struct OnDrop(Arc<AtomicU32>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let guard = OnDrop(dropped.clone());
    let start = Instant::now();
    let slow = with_timeout(100 * MS, async move {
        let _guard = guard;
        tokio::time::sleep(Duration::from_secs(60)).await;
    })
    .await;
    assert_eq!(slow, Err(TimeoutError));
    assert_eq!(start.elapsed(), 100 * MS);
    // The operation was cancelled (dropped), not left running.
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert_eq!(TimeoutError.to_string(), "operation timed out");
}

#[tokio::test(start_paused = true)]
async fn a_deadline_is_shared_by_every_step() {
    let start = Instant::now();
    let deadline = Deadline::after(100 * MS);
    assert_eq!(deadline.remaining(), 100 * MS);
    assert!(!deadline.is_expired());
    assert_eq!(deadline.check(), Ok(()));

    // Step one uses 60 ms of the budget...
    assert_eq!(deadline.run(tokio::time::sleep(60 * MS)).await, Ok(()));
    assert_eq!(deadline.remaining(), 40 * MS);
    // ...so step two, which also needs 60 ms, is cut off at the deadline.
    assert_eq!(
        deadline.run(tokio::time::sleep(60 * MS)).await,
        Err(TimeoutError)
    );
    assert_eq!(start.elapsed(), 100 * MS);
    assert!(deadline.is_expired());
    assert_eq!(deadline.remaining(), Duration::ZERO);
    assert_eq!(deadline.check(), Err(TimeoutError));
    // After the deadline only an immediately-ready step can still finish.
    assert_eq!(deadline.run(async { 1 }).await, Ok(1));
    assert_eq!(
        deadline.run(tokio::time::sleep(MS)).await,
        Err(TimeoutError)
    );
}

#[tokio::test(start_paused = true)]
async fn deadlines_compose() {
    let outer = Deadline::after(Duration::from_secs(10));
    // A step with a tighter budget of its own...
    assert_eq!(
        outer.min_with(Duration::from_secs(2)).remaining(),
        Duration::from_secs(2)
    );
    // ...never gets more than the request has left.
    assert_eq!(outer.min_with(Duration::from_secs(60)), outer);
    assert_eq!(Deadline::at(outer.instant()), outer);
    // An absurdly distant deadline does not overflow.
    let never = Deadline::after(Duration::MAX);
    assert!(never.remaining() > Duration::from_secs(60 * 60 * 24 * 365));
    assert_eq!(never.run(async { 3 }).await, Ok(3));
}

// ---- policy ----------------------------------------------------------------------

fn breaker(failure_threshold: u32) -> Arc<CircuitBreaker> {
    Arc::new(CircuitBreaker::new(CircuitBreakerConfig {
        failure_threshold,
        success_threshold: 1,
        max_half_open: 1,
        timeout: Duration::from_secs(30),
    }))
}

#[tokio::test(start_paused = true)]
async fn an_empty_policy_just_runs_the_operation() {
    let policy = Policy::new();
    assert_eq!(policy.execute(|| async { Ok::<_, ()>(7) }).await, Ok(7));
    let calls = Cell::new(0);
    let out = policy
        .execute(|| {
            calls.set(calls.get() + 1);
            async { Err::<(), _>("boom") }
        })
        .await;
    assert_eq!(out, Err(PolicyError::Inner("boom")));
    assert_eq!(calls.get(), 1);
    assert_eq!(out.clone().unwrap_err().into_inner(), Some("boom"));
    assert_eq!(out.unwrap_err().to_string(), "boom");
}

#[tokio::test(start_paused = true)]
async fn policy_retries_transient_errors_and_reports_to_the_breaker() {
    let cb = breaker(10);
    let policy = Policy::new()
        .retry(RetryPolicy::fixed(5, 100 * MS))
        .circuit_breaker(cb.clone());
    let calls = Cell::new(0);
    let start = Instant::now();
    let out = policy
        .execute(|| {
            calls.set(calls.get() + 1);
            let n = calls.get();
            async move {
                if n < 4 {
                    Err("transient")
                } else {
                    Ok(n)
                }
            }
        })
        .await;
    assert_eq!(out, Ok(4));
    assert_eq!(start.elapsed(), 300 * MS);
    // Each attempt was reported individually.
    let m = cb.metrics();
    assert_eq!((m.failures, m.successes), (3, 1));
    assert_eq!(m.consecutive_failures, 0);
}

#[tokio::test(start_paused = true)]
async fn policy_stops_retrying_when_the_breaker_opens() {
    let cb = breaker(3);
    let policy = Policy::new()
        .retry(RetryPolicy::fixed(10, 100 * MS))
        .circuit_breaker(cb.clone());
    let calls = Cell::new(0);
    let out: Result<(), _> = policy
        .execute(|| {
            calls.set(calls.get() + 1);
            async { Err("down") }
        })
        .await;
    // Three failures open the breaker; the fourth attempt is rejected and the
    // remaining six retries are not spent hammering a dead dependency.
    assert_eq!(out, Err(PolicyError::CircuitOpen));
    assert_eq!(calls.get(), 4); // the 4th future was built but never polled
    assert_eq!(cb.metrics().failures, 3);
    assert_eq!(cb.state(), CircuitState::Open);
    assert!(out.unwrap_err().is_rejection());

    // While open, calls fail immediately without running.
    let ran = Cell::new(false);
    let start = Instant::now();
    let out: Result<(), PolicyError<&str>> = policy
        .execute(|| async {
            ran.set(true);
            Ok(())
        })
        .await;
    assert_eq!(out, Err(PolicyError::CircuitOpen));
    assert!(!ran.get());
    assert_eq!(start.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn policy_classification_separates_retry_from_breaker_accounting() {
    let cb = breaker(3);
    let policy = Policy::new()
        .retry(RetryPolicy::fixed(5, 100 * MS))
        .circuit_breaker(cb.clone());
    let classify = |status: &u16| match *status {
        429 => ErrorClass::retry_after(Duration::from_secs(2)),
        500 => ErrorClass::fatal(),
        502..=504 => ErrorClass::transient(),
        _ => ErrorClass::caller_error(),
    };

    // Caller errors: not retried, and never counted against the dependency.
    for status in [400u16, 401, 403, 404, 422].into_iter().cycle().take(30) {
        let calls = Cell::new(0);
        let out: Result<(), _> = policy
            .execute_with(classify, || {
                calls.set(calls.get() + 1);
                async move { Err(status) }
            })
            .await;
        assert_eq!(out, Err(PolicyError::Inner(status)));
        assert_eq!(calls.get(), 1);
    }
    assert_eq!(cb.state(), CircuitState::Closed);
    assert_eq!(cb.metrics().failures, 0);

    // A server-provided delay is honoured exactly.
    let calls = Cell::new(0);
    let start = Instant::now();
    let out = policy
        .execute_with(classify, || {
            calls.set(calls.get() + 1);
            let n = calls.get();
            async move {
                if n == 1 {
                    Err(429u16)
                } else {
                    Ok("served")
                }
            }
        })
        .await;
    assert_eq!(out, Ok("served"));
    assert_eq!(start.elapsed(), Duration::from_secs(2));
    assert_eq!(cb.metrics().failures, 1);

    // Fatal: counted, not retried.
    let calls = Cell::new(0);
    let out: Result<(), _> = policy
        .execute_with(classify, || {
            calls.set(calls.get() + 1);
            async { Err(500u16) }
        })
        .await;
    assert_eq!(out, Err(PolicyError::Inner(500)));
    assert_eq!(calls.get(), 1);
    assert_eq!(cb.metrics().failures, 2);

    assert_eq!(ErrorClass::transient().retry, RetryDecision::Retry);
    assert_eq!(ErrorClass::caller_error().outcome, Outcome::Ignored);
}

#[tokio::test(start_paused = true)]
async fn policy_attempt_timeout_is_a_retryable_breaker_failure() {
    let cb = breaker(10);
    let policy = Policy::new()
        .retry(RetryPolicy::fixed(2, 100 * MS))
        .circuit_breaker(cb.clone())
        .attempt_timeout(500 * MS);
    let calls = Cell::new(0);
    let start = Instant::now();
    let out = policy
        .execute(|| {
            calls.set(calls.get() + 1);
            let n = calls.get();
            async move {
                if n < 3 {
                    tokio::time::sleep(Duration::from_secs(60)).await; // hangs
                }
                Ok::<_, ()>(n)
            }
        })
        .await;
    assert_eq!(out, Ok(3));
    // Two attempts of 500 ms each, two backoffs of 100 ms.
    assert_eq!(start.elapsed(), 1200 * MS);
    assert_eq!(cb.metrics().failures, 2);

    // When every attempt hangs, the last timeout is what the caller gets.
    let out: Result<(), PolicyError<()>> = policy
        .execute(|| async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        })
        .await;
    assert_eq!(out, Err(PolicyError::Timeout));
    assert_eq!(
        PolicyError::<&str>::Timeout.to_string(),
        "operation timed out"
    );
}

#[tokio::test(start_paused = true)]
async fn policy_overall_timeout_bounds_retries_and_backoff() {
    let cb = breaker(100);
    let policy = Policy::new()
        .timeout(2500 * MS)
        .retry(RetryPolicy::fixed(100, 1000 * MS))
        .circuit_breaker(cb.clone());
    let calls = Cell::new(0);
    let start = Instant::now();
    let out: Result<(), _> = policy
        .execute(|| {
            calls.set(calls.get() + 1);
            async { Err("down") }
        })
        .await;
    assert_eq!(out, Err(PolicyError::Timeout));
    assert_eq!(start.elapsed(), 2500 * MS);
    assert_eq!(calls.get(), 3); // t = 0, 1, 2 s

    // A hanging attempt cut off by the overall timeout is not a breaker
    // failure (its permit is dropped unresolved)...
    let before = cb.metrics().failures;
    let out: Result<(), PolicyError<()>> = Policy::new()
        .timeout(100 * MS)
        .circuit_breaker(cb.clone())
        .execute(|| async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        })
        .await;
    assert_eq!(out, Err(PolicyError::Timeout));
    assert_eq!(cb.metrics().failures, before);
    // ...which is why a per-attempt timeout is the one to use for that.
}

#[tokio::test(start_paused = true)]
async fn policy_bulkhead_rejection_is_not_retried_or_counted() {
    let cb = breaker(1);
    let bulkhead = Arc::new(Bulkhead::new(1).fail_fast());
    let policy = Policy::new()
        .retry(RetryPolicy::fixed(5, 100 * MS))
        .circuit_breaker(cb.clone())
        .bulkhead(bulkhead.clone());

    let held = bulkhead.acquire().await.unwrap();
    let calls = Cell::new(0);
    let start = Instant::now();
    let out: Result<(), PolicyError<&str>> = policy
        .execute(|| {
            calls.set(calls.get() + 1);
            async { Ok(()) }
        })
        .await;
    assert_eq!(out, Err(PolicyError::BulkheadFull));
    assert_eq!(start.elapsed(), Duration::ZERO);
    assert_eq!(calls.get(), 1);
    assert!(out.unwrap_err().is_rejection());
    // Shedding load says nothing about the dependency's health.
    assert_eq!(cb.state(), CircuitState::Closed);
    assert_eq!(cb.metrics().failures, 0);

    drop(held);
    assert_eq!(policy.execute(|| async { Ok::<_, &str>(1) }).await, Ok(1));
    assert_eq!(bulkhead.available(), 1);
}

#[tokio::test(start_paused = true)]
async fn policy_paces_attempts_with_the_rate_limiter() {
    let policy = Policy::new().rate_limiter(Arc::new(RateLimiter::new(10.0, 1)));
    let start = Instant::now();
    for _ in 0..5 {
        assert_eq!(policy.execute(|| async { Ok::<_, ()>(()) }).await, Ok(()));
    }
    assert_eq!(start.elapsed(), 400 * MS);
}

#[tokio::test(start_paused = true)]
async fn cancelling_a_policy_call_releases_everything_it_held() {
    let cb = breaker(1);
    let bulkhead = Arc::new(Bulkhead::new(1));
    let policy = Policy::new()
        .circuit_breaker(cb.clone())
        .bulkhead(bulkhead.clone());

    // Open the breaker, wait out the cooldown: the next call is the trial.
    let _: Result<(), _> = policy.execute(|| async { Err("down") }).await;
    assert_eq!(cb.state(), CircuitState::Open);
    tokio::time::advance(Duration::from_secs(31)).await;

    let hung = tokio::spawn({
        let policy = policy.clone();
        async move {
            policy
                .execute(|| async {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    Ok::<(), &str>(())
                })
                .await
        }
    });
    tokio::task::yield_now().await;
    assert_eq!(cb.metrics().half_open_in_flight, 1);
    assert_eq!(bulkhead.available(), 0);

    hung.abort();
    let _ = hung.await;
    assert_eq!(cb.metrics().half_open_in_flight, 0);
    assert_eq!(bulkhead.available(), 1);
    // The trial slot is free: the dependency can be probed again.
    assert_eq!(policy.execute(|| async { Ok::<_, &str>(9) }).await, Ok(9));
    assert_eq!(cb.state(), CircuitState::Closed);
}

#[tokio::test(start_paused = true)]
async fn policy_composes_jitter_and_budget_from_a_retry() {
    let budget = Arc::new(RetryBudget::new(2, 0.0));
    let policy = Policy::new().retry(
        RetryPolicy::fixed(10, 100 * MS)
            .with_jitter(Jitter::Equal)
            .budget(budget.clone()),
    );
    let calls = Cell::new(0);
    let start = Instant::now();
    let out: Result<(), _> = policy
        .execute(|| {
            calls.set(calls.get() + 1);
            async { Err("down") }
        })
        .await;
    assert_eq!(out, Err(PolicyError::Inner("down")));
    assert_eq!(calls.get(), 3); // first attempt + the two budgeted retries
    let waited = start.elapsed();
    assert!(waited >= 100 * MS && waited <= 200 * MS, "{waited:?}");
    assert!(format!("{policy:?}").contains("Policy"));
}

#![cfg(feature = "clients")]
//! Circuit breaker: the RAII permit API, half-open recovery when trials are
//! abandoned, outcome classification, the failure-rate window, state-change
//! callbacks and the keyed registry.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::clients::{
    CircuitBreaker, CircuitBreakerConfig, CircuitBreakerRegistry, CircuitState, FailureRate,
    Outcome, PolicyError,
};

const COOLDOWN: Duration = Duration::from_millis(20);

fn cfg() -> CircuitBreakerConfig {
    CircuitBreakerConfig {
        failure_threshold: 3,
        success_threshold: 2,
        max_half_open: 1,
        timeout: COOLDOWN,
    }
}

/// Opens `cb` with `failure_threshold` failed permits.
fn trip(cb: &CircuitBreaker) {
    for _ in 0..3 {
        cb.try_acquire().expect("closed breaker admits").failure();
    }
    assert_eq!(cb.state(), CircuitState::Open);
}

async fn past_cooldown() {
    tokio::time::advance(COOLDOWN + Duration::from_millis(1)).await;
}

// ---- permits ---------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn permits_drive_the_full_state_machine() {
    let cb = CircuitBreaker::new(cfg());
    assert_eq!(cb.state(), CircuitState::Closed);

    // Successes keep it closed and reset the failure run.
    cb.try_acquire().unwrap().failure();
    cb.try_acquire().unwrap().failure();
    cb.try_acquire().unwrap().success();
    assert_eq!(cb.metrics().consecutive_failures, 0);
    assert_eq!(cb.state(), CircuitState::Closed);

    trip(&cb);
    assert!(cb.try_acquire().is_err());

    // After the cooldown the next call is admitted as a trial.
    past_cooldown().await;
    let trial = cb.try_acquire().unwrap();
    assert!(trial.is_trial());
    assert_eq!(cb.state(), CircuitState::HalfOpen);
    trial.success();
    assert_eq!(cb.state(), CircuitState::HalfOpen);
    cb.try_acquire().unwrap().success();
    assert_eq!(cb.state(), CircuitState::Closed);
    assert!(!cb.try_acquire().unwrap().is_trial());
}

#[tokio::test(start_paused = true)]
async fn half_open_failure_reopens_and_restarts_the_cooldown() {
    let cb = CircuitBreaker::new(cfg());
    trip(&cb);
    past_cooldown().await;
    cb.try_acquire().unwrap().failure();
    assert_eq!(cb.state(), CircuitState::Open);
    // A fresh cooldown: not yet.
    tokio::time::advance(COOLDOWN / 2).await;
    assert!(cb.try_acquire().is_err());
    past_cooldown().await;
    assert!(cb.try_acquire().is_ok());
}

#[tokio::test(start_paused = true)]
async fn record_result_maps_ok_and_err() {
    let cb = CircuitBreaker::new(cfg());
    cb.try_acquire().unwrap().record_result(&Ok::<u8, ()>(1));
    for _ in 0..3 {
        cb.try_acquire().unwrap().record_result(&Err::<u8, ()>(()));
    }
    assert_eq!(cb.state(), CircuitState::Open);
    let m = cb.metrics();
    assert_eq!((m.successes, m.failures, m.trips), (1, 3, 1));
}

// ---- G6: an abandoned half-open trial must not disable the breaker ----------

#[tokio::test(start_paused = true)]
async fn dropping_an_unresolved_permit_releases_the_half_open_slot() {
    let cb = CircuitBreaker::new(cfg());
    trip(&cb);
    past_cooldown().await;

    // Abandon the trial a hundred times over; the slot always comes back.
    for round in 0..100 {
        let trial = cb.try_acquire().unwrap_or_else(|_| panic!("round {round}"));
        assert_eq!(cb.metrics().half_open_in_flight, 1);
        // The single slot is taken.
        assert!(cb.try_acquire().is_err());
        drop(trial);
        assert_eq!(cb.metrics().half_open_in_flight, 0);
        // Abandoning is neither a success nor a failure.
        assert_eq!(cb.state(), CircuitState::HalfOpen);
    }
    // The breaker still recovers normally.
    cb.try_acquire().unwrap().success();
    cb.try_acquire().unwrap().success();
    assert_eq!(cb.state(), CircuitState::Closed);
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_call_cannot_leak_the_half_open_slot() {
    let cb = Arc::new(CircuitBreaker::new(cfg()));
    trip(&cb);
    past_cooldown().await;

    // The trial call hangs and its caller gives up (timeout / client gone).
    let hung = cb.call(|| async {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Ok::<(), ()>(())
    });
    assert!(tokio::time::timeout(Duration::from_millis(1), hung)
        .await
        .is_err());
    assert_eq!(cb.state(), CircuitState::HalfOpen);
    assert_eq!(cb.metrics().half_open_in_flight, 0);

    // Same through an aborted task.
    let task = tokio::spawn({
        let cb = cb.clone();
        async move {
            cb.call(|| async {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok::<(), ()>(())
            })
            .await
        }
    });
    tokio::task::yield_now().await;
    assert_eq!(cb.metrics().half_open_in_flight, 1);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(cb.metrics().half_open_in_flight, 0);

    // A later caller gets the trial and closes the breaker.
    assert_eq!(cb.call(|| async { Ok::<u8, ()>(1) }).await, Ok(1));
    assert_eq!(cb.call(|| async { Ok::<u8, ()>(2) }).await, Ok(2));
    assert_eq!(cb.state(), CircuitState::Closed);
}

#[tokio::test(start_paused = true)]
async fn legacy_probe_that_never_reports_is_reclaimed_after_the_timeout() {
    // The original protocol: can_execute() admitted a trial, and the caller
    // was cancelled before on_execution(). That used to pin the breaker in
    // HalfOpen with its only slot taken, forever.
    let cb = CircuitBreaker::new(cfg());
    for _ in 0..3 {
        assert!(cb.can_execute().is_ok());
        cb.on_execution(false);
    }
    past_cooldown().await;
    assert!(cb.can_execute().is_ok()); // trial admitted, never reported
    assert_eq!(cb.state(), CircuitState::HalfOpen);
    assert!(cb.can_execute().is_err());
    assert!(cb.try_acquire().is_err());

    // Default trial timeout = the cooldown. Just before it: still taken.
    tokio::time::advance(COOLDOWN - Duration::from_millis(1)).await;
    assert!(cb.can_execute().is_err());
    // After it: the slot is reclaimed and the breaker can recover.
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(cb.can_execute().is_ok());
    cb.on_execution(true);
    assert!(cb.can_execute().is_ok());
    cb.on_execution(true);
    assert_eq!(cb.state(), CircuitState::Closed);
}

#[tokio::test(start_paused = true)]
async fn half_open_timeout_is_configurable() {
    let cb = CircuitBreaker::builder(cfg())
        .half_open_timeout(Duration::from_secs(5))
        .build();
    trip(&cb);
    past_cooldown().await;
    assert!(cb.can_execute().is_ok());
    tokio::time::advance(Duration::from_millis(4_999)).await;
    assert!(cb.can_execute().is_err());
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(cb.can_execute().is_ok());
}

#[tokio::test(start_paused = true)]
async fn late_report_from_a_reclaimed_legacy_probe_is_harmless() {
    let cb = CircuitBreaker::new(cfg());
    trip(&cb);
    past_cooldown().await;
    assert!(cb.can_execute().is_ok()); // probe A, slow
    past_cooldown().await; // A's slot is reclaimed
    assert!(cb.can_execute().is_ok()); // probe B
    cb.on_execution(true); // A reports late
    cb.on_execution(true); // B reports
    assert_eq!(cb.state(), CircuitState::Closed);
    assert_eq!(cb.metrics().half_open_in_flight, 0);
}

#[tokio::test(start_paused = true)]
async fn half_open_admits_up_to_max_half_open_trials() {
    let cb = CircuitBreaker::new(CircuitBreakerConfig {
        max_half_open: 3,
        success_threshold: 3,
        ..cfg()
    });
    trip(&cb);
    past_cooldown().await;
    let a = cb.try_acquire().unwrap();
    let b = cb.try_acquire().unwrap();
    let c = cb.try_acquire().unwrap();
    assert!(cb.try_acquire().is_err());
    assert_eq!(cb.metrics().half_open_in_flight, 3);
    a.success();
    // One slot is free again.
    let d = cb.try_acquire().unwrap();
    assert!(cb.try_acquire().is_err());
    b.success();
    assert_eq!(cb.state(), CircuitState::HalfOpen);
    c.success();
    assert_eq!(cb.state(), CircuitState::Closed);
    // `d` was issued while half-open; its outcome no longer counts.
    d.failure();
    assert_eq!(cb.metrics().consecutive_failures, 0);
    assert_eq!(cb.state(), CircuitState::Closed);
}

#[tokio::test(start_paused = true)]
async fn permits_can_move_to_other_tasks() {
    let cb = Arc::new(CircuitBreaker::new(cfg()));
    let permit = cb.try_acquire().unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(5)).await;
        permit.failure();
    })
    .await
    .unwrap();
    assert_eq!(cb.metrics().failures, 1);
}

#[tokio::test(start_paused = true)]
async fn outcomes_from_an_earlier_state_are_discarded() {
    let cb = CircuitBreaker::new(cfg());
    let early = cb.try_acquire().unwrap(); // issued while Closed
    trip(&cb);
    let before = cb.metrics();
    // Its failure arrives while Open: it must not extend the cooldown.
    past_cooldown().await;
    early.failure();
    assert_eq!(cb.metrics().failures, before.failures);
    assert!(cb.try_acquire().is_ok()); // cooldown was not refreshed
}

// ---- classification (G7) ----------------------------------------------------

#[tokio::test(start_paused = true)]
async fn ignored_outcomes_never_move_the_breaker() {
    let cb = CircuitBreaker::new(cfg());
    for _ in 0..50 {
        cb.try_acquire().unwrap().ignore();
        cb.try_acquire().unwrap().record(Outcome::Ignored);
    }
    assert_eq!(cb.state(), CircuitState::Closed);
    let m = cb.metrics();
    assert_eq!((m.successes, m.failures), (0, 0));

    // Ignored calls do not reset a failure run either.
    cb.try_acquire().unwrap().failure();
    cb.try_acquire().unwrap().ignore();
    cb.try_acquire().unwrap().failure();
    cb.try_acquire().unwrap().ignore();
    assert_eq!(cb.state(), CircuitState::Closed);
    cb.try_acquire().unwrap().failure();
    assert_eq!(cb.state(), CircuitState::Open);

    // In HalfOpen an ignored trial frees its slot and decides nothing.
    past_cooldown().await;
    cb.try_acquire().unwrap().ignore();
    assert_eq!(cb.state(), CircuitState::HalfOpen);
    cb.try_acquire().unwrap().success();
    cb.try_acquire().unwrap().success();
    assert_eq!(cb.state(), CircuitState::Closed);
}

#[tokio::test(start_paused = true)]
async fn call_with_classifies_errors() {
    let cb = CircuitBreaker::new(cfg());
    let classify = |status: &u16| {
        if *status >= 500 {
            Outcome::Failure
        } else {
            Outcome::Ignored
        }
    };
    // Bad requests are returned to the caller but never open the breaker.
    for status in [400u16, 401, 403, 404, 422].into_iter().cycle().take(40) {
        let out = cb
            .call_with(classify, || async move { Err::<(), u16>(status) })
            .await;
        assert_eq!(out, Err(PolicyError::Inner(status)));
    }
    assert_eq!(cb.state(), CircuitState::Closed);
    // Server errors do.
    for _ in 0..3 {
        let _ = cb
            .call_with(classify, || async { Err::<(), u16>(503) })
            .await;
    }
    assert_eq!(cb.state(), CircuitState::Open);
    // While open, the operation is not even started.
    let mut ran = false;
    let out = cb
        .call(|| {
            ran = true;
            async { Ok::<(), u16>(()) }
        })
        .await;
    assert_eq!(out, Err(PolicyError::CircuitOpen));
    assert!(!ran);
    assert_eq!(cb.metrics().rejected, 1);
}

#[tokio::test(start_paused = true)]
async fn call_counts_every_error_as_failure() {
    let cb = CircuitBreaker::new(cfg());
    assert_eq!(cb.call(|| async { Ok::<u8, &str>(7) }).await, Ok(7));
    for _ in 0..3 {
        assert_eq!(
            cb.call(|| async { Err::<u8, &str>("boom") }).await,
            Err(PolicyError::Inner("boom"))
        );
    }
    assert_eq!(cb.state(), CircuitState::Open);
}

// ---- failure-rate window ----------------------------------------------------

fn rate_only(window: Duration, min_calls: u32, threshold: f64) -> CircuitBreaker {
    CircuitBreaker::builder(CircuitBreakerConfig {
        failure_threshold: u32::MAX, // consecutive mode off
        ..cfg()
    })
    .failure_rate(FailureRate {
        window,
        min_calls,
        threshold,
    })
    .build()
}

#[tokio::test(start_paused = true)]
async fn failure_rate_trips_without_consecutive_failures() {
    let cb = rate_only(Duration::from_secs(10), 10, 0.5);
    // Alternate success / failure: never two failures in a row.
    for i in 0..9 {
        let permit = cb.try_acquire().unwrap();
        if i % 2 == 0 {
            permit.success();
        } else {
            permit.failure();
        }
        tokio::time::advance(Duration::from_millis(100)).await;
        assert_eq!(cb.state(), CircuitState::Closed, "call {i}");
    }
    // The tenth outcome is the fifth failure: 5 / 10 >= 0.5.
    cb.try_acquire().unwrap().failure();
    assert_eq!(cb.state(), CircuitState::Open);
    assert_eq!(cb.metrics().trips, 1);
}

#[tokio::test(start_paused = true)]
async fn failure_rate_needs_min_calls_and_the_threshold() {
    // Below min_calls: even 100 % failures do not trip.
    let cb = rate_only(Duration::from_secs(10), 10, 0.5);
    for _ in 0..9 {
        cb.try_acquire().unwrap().failure();
    }
    assert_eq!(cb.state(), CircuitState::Closed);
    cb.try_acquire().unwrap().failure();
    assert_eq!(cb.state(), CircuitState::Open);

    // Enough calls, but a rate under the threshold.
    let cb = rate_only(Duration::from_secs(10), 10, 0.5);
    for i in 0..100 {
        let permit = cb.try_acquire().unwrap();
        if i % 5 == 0 {
            permit.failure(); // 20 %
        } else {
            permit.success();
        }
    }
    assert_eq!(cb.state(), CircuitState::Closed);
}

#[tokio::test(start_paused = true)]
async fn failure_rate_window_forgets_old_outcomes() {
    let cb = rate_only(Duration::from_secs(10), 6, 0.5);
    // Five failures: one short of min_calls.
    for _ in 0..5 {
        cb.try_acquire().unwrap().failure();
    }
    assert_eq!(cb.state(), CircuitState::Closed);
    // They age out of the 10 s window...
    tokio::time::advance(Duration::from_secs(11)).await;
    // ...so one more failure is 1 / 1, below min_calls: no trip.
    cb.try_acquire().unwrap().failure();
    assert_eq!(cb.state(), CircuitState::Closed);

    // Within the window they do add up.
    let cb = rate_only(Duration::from_secs(10), 6, 0.5);
    for _ in 0..5 {
        cb.try_acquire().unwrap().failure();
    }
    tokio::time::advance(Duration::from_secs(5)).await;
    cb.try_acquire().unwrap().failure();
    assert_eq!(cb.state(), CircuitState::Open);
}

#[tokio::test(start_paused = true)]
async fn failure_rate_and_consecutive_modes_work_side_by_side() {
    let cb = CircuitBreaker::builder(cfg()) // 3 consecutive failures
        .failure_rate(FailureRate {
            window: Duration::from_secs(60),
            min_calls: 100,
            threshold: 0.5,
        })
        .build();
    // Far below the window's min_calls, the consecutive count still trips.
    trip(&cb);
    // The window starts clean after recovery.
    past_cooldown().await;
    cb.try_acquire().unwrap().success();
    cb.try_acquire().unwrap().success();
    assert_eq!(cb.state(), CircuitState::Closed);
    cb.try_acquire().unwrap().failure();
    assert_eq!(cb.state(), CircuitState::Closed);
}

// ---- callbacks, metrics, reset ------------------------------------------------

#[tokio::test(start_paused = true)]
async fn state_change_callbacks_see_every_transition() {
    let seen: Arc<Mutex<Vec<(CircuitState, CircuitState)>>> = Arc::default();
    let also: Arc<Mutex<usize>> = Arc::default();
    let cb = CircuitBreaker::builder(cfg())
        .on_state_change({
            let seen = seen.clone();
            move |from, to| seen.lock().unwrap().push((from, to))
        })
        .on_state_change({
            let also = also.clone();
            move |_, _| *also.lock().unwrap() += 1
        })
        .build();

    trip(&cb);
    past_cooldown().await;
    cb.try_acquire().unwrap().failure(); // HalfOpen -> Open
    past_cooldown().await;
    cb.try_acquire().unwrap().success();
    cb.try_acquire().unwrap().success(); // HalfOpen -> Closed
    trip(&cb);
    cb.reset(); // Open -> Closed

    use CircuitState::{Closed, HalfOpen, Open};
    assert_eq!(
        *seen.lock().unwrap(),
        [
            (Closed, Open),
            (Open, HalfOpen),
            (HalfOpen, Open),
            (Open, HalfOpen),
            (HalfOpen, Closed),
            (Closed, Open),
            (Open, Closed),
        ]
    );
    assert_eq!(*also.lock().unwrap(), 7);
}

#[tokio::test(start_paused = true)]
async fn a_callback_may_use_the_breaker_it_observes() {
    // Callbacks run after the internal lock is released, so reading the
    // breaker from inside one must not deadlock.
    let registry = Arc::new(CircuitBreakerRegistry::with_factory(|_| {
        CircuitBreaker::new(cfg())
    }));
    let observed: Arc<Mutex<Vec<CircuitState>>> = Arc::default();
    let cb = CircuitBreaker::builder(cfg())
        .on_state_change({
            let registry = registry.clone();
            let observed = observed.clone();
            move |_, _| {
                let other = registry.get("other");
                other.try_acquire().unwrap().success();
                observed.lock().unwrap().push(other.state());
            }
        })
        .build();
    trip(&cb);
    assert_eq!(*observed.lock().unwrap(), [CircuitState::Closed]);
}

#[tokio::test(start_paused = true)]
async fn metrics_report_counters() {
    let cb = CircuitBreaker::new(cfg());
    cb.try_acquire().unwrap().success();
    cb.try_acquire().unwrap().failure();
    let m = cb.metrics();
    assert_eq!(m.state, CircuitState::Closed);
    assert_eq!((m.successes, m.failures, m.rejected, m.trips), (1, 1, 0, 0));
    assert_eq!(m.consecutive_failures, 1);
    cb.try_acquire().unwrap().failure();
    cb.try_acquire().unwrap().failure();
    for _ in 0..4 {
        assert!(cb.try_acquire().is_err());
    }
    let m = cb.metrics();
    assert_eq!(m.state, CircuitState::Open);
    assert_eq!((m.successes, m.failures, m.rejected, m.trips), (1, 3, 4, 1));
    let debug = format!("{cb:?}");
    assert!(debug.contains("Open"), "{debug}");
}

#[tokio::test(start_paused = true)]
async fn reset_closes_the_breaker() {
    let cb = CircuitBreaker::new(cfg());
    trip(&cb);
    cb.reset();
    assert_eq!(cb.state(), CircuitState::Closed);
    assert!(cb.try_acquire().is_ok());
    // Resetting a closed breaker clears a failure run.
    cb.try_acquire().unwrap().failure();
    cb.try_acquire().unwrap().failure();
    cb.reset();
    cb.try_acquire().unwrap().failure();
    assert_eq!(cb.state(), CircuitState::Closed);
}

#[test]
fn concurrent_callers_are_counted_exactly() {
    let cb = Arc::new(CircuitBreaker::new(CircuitBreakerConfig {
        failure_threshold: u32::MAX,
        ..cfg()
    }));
    let threads: Vec<_> = (0..8u32)
        .map(|t| {
            let cb = cb.clone();
            std::thread::spawn(move || {
                let (mut successes, mut failures) = (0u64, 0u64);
                for i in 0..2_000u32 {
                    let permit = cb.try_acquire().unwrap();
                    match (t + i) % 3 {
                        0 => {
                            permit.success();
                            successes += 1;
                        }
                        1 => {
                            permit.failure();
                            failures += 1;
                        }
                        _ => drop(permit),
                    }
                }
                (successes, failures)
            })
        })
        .collect();
    let (mut successes, mut failures) = (0u64, 0u64);
    for thread in threads {
        let (s, f) = thread.join().unwrap();
        successes += s;
        failures += f;
    }
    let m = cb.metrics();
    assert_eq!((m.successes, m.failures), (successes, failures));
    assert!(successes > 5_000 && failures > 5_000);
    assert_eq!(m.state, CircuitState::Closed);
}

// ---- registry -----------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn registry_returns_one_breaker_per_key() {
    let breakers = CircuitBreakerRegistry::new(cfg());
    assert!(breakers.is_empty());
    let a1 = breakers.get("a");
    let a2 = breakers.get("a");
    let b = breakers.get("b");
    assert!(Arc::ptr_eq(&a1, &a2));
    assert!(!Arc::ptr_eq(&a1, &b));
    assert_eq!(breakers.len(), 2);
    assert_eq!(breakers.keys(), ["a", "b"]);

    // Tripping one key leaves the others alone.
    trip(&a1);
    assert!(breakers.get("a").try_acquire().is_err());
    assert!(breakers.get("b").try_acquire().is_ok());
    assert_eq!(
        breakers.states(),
        [
            ("a".to_string(), CircuitState::Open),
            ("b".to_string(), CircuitState::Closed)
        ]
    );

    assert!(breakers.get_existing("a").is_some());
    assert!(breakers.get_existing("zzz").is_none());
    assert_eq!(breakers.len(), 2); // get_existing never creates

    // Removing a key hands back the breaker; the next get() starts fresh.
    let removed = breakers.remove("a").unwrap();
    assert!(Arc::ptr_eq(&removed, &a1));
    assert_eq!(breakers.get("a").state(), CircuitState::Closed);
    breakers.clear();
    assert!(breakers.is_empty());
    assert!(format!("{breakers:?}").contains("CircuitBreakerRegistry"));
}

#[tokio::test(start_paused = true)]
async fn registry_factory_receives_the_key() {
    let changes: Arc<Mutex<Vec<String>>> = Arc::default();
    let breakers = CircuitBreakerRegistry::with_factory({
        let changes = changes.clone();
        move |key| {
            let key = key.to_string();
            let changes = changes.clone();
            // Per-key tuning: "fragile" trips on the first failure.
            let failure_threshold = if key == "fragile" { 1 } else { 3 };
            CircuitBreaker::builder(CircuitBreakerConfig {
                failure_threshold,
                ..cfg()
            })
            .on_state_change(move |from, to| {
                changes
                    .lock()
                    .unwrap()
                    .push(format!("{key}: {from:?}->{to:?}"))
            })
            .build()
        }
    });
    breakers.get("fragile").try_acquire().unwrap().failure();
    breakers.get("sturdy").try_acquire().unwrap().failure();
    assert_eq!(breakers.get("fragile").state(), CircuitState::Open);
    assert_eq!(breakers.get("sturdy").state(), CircuitState::Closed);
    assert_eq!(*changes.lock().unwrap(), ["fragile: Closed->Open"]);
}

#[test]
fn registry_is_safe_under_concurrent_first_use() {
    let breakers = Arc::new(CircuitBreakerRegistry::new(cfg()));
    let threads: Vec<_> = (0..16)
        .map(|_| {
            let breakers = breakers.clone();
            std::thread::spawn(move || breakers.get("shared"))
        })
        .collect();
    let all: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert!(all.iter().all(|b| Arc::ptr_eq(b, &all[0])));
    assert_eq!(breakers.len(), 1);
}

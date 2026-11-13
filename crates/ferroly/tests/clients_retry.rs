#![cfg(feature = "clients")]
//! Retry: jitter, total elapsed limit, the decision hook (server-provided
//! delays) and the retry budget. Timing runs on tokio's paused clock, so every
//! duration asserted here is exact virtual time.

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::Duration;

use ferroly::clients::{
    retry, retry_with, Jitter, Retry, RetryAttempt, RetryBudget, RetryDecision, RetryPolicy,
};
use tokio::time::Instant;

const MS: Duration = Duration::from_millis(1);

/// An operation that fails `failures` times, then succeeds with the number
/// of calls made.
fn flaky(
    calls: &Cell<u32>,
    failures: u32,
) -> impl FnMut() -> std::future::Ready<Result<u32, &'static str>> + '_ {
    move || {
        calls.set(calls.get() + 1);
        std::future::ready(if calls.get() <= failures {
            Err("transient")
        } else {
            Ok(calls.get())
        })
    }
}

// ---- the original driver is unchanged ----------------------------------------

#[tokio::test(start_paused = true)]
async fn retry_driver_waits_the_exact_backoff() {
    let calls = Cell::new(0);
    let start = Instant::now();
    let policy = RetryPolicy::fixed(5, 100 * MS);
    assert_eq!(retry(&policy, |_| true, flaky(&calls, 3)).await, Ok(4));
    assert_eq!(start.elapsed(), 300 * MS);

    let calls = Cell::new(0);
    let start = Instant::now();
    let policy = RetryPolicy::exponential(5, 100 * MS, 250 * MS);
    assert_eq!(retry(&policy, |_| true, flaky(&calls, 4)).await, Ok(5));
    // 100 + 200 + 250 (capped) + 250 (capped)
    assert_eq!(start.elapsed(), 800 * MS);
}

// ---- jitter ---------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn no_jitter_is_deterministic() {
    let retry = Retry::new(RetryPolicy::fixed(3, 100 * MS));
    assert_eq!(retry.delay_for(0), 100 * MS);
    let calls = Cell::new(0);
    let start = Instant::now();
    assert_eq!(retry.run(|_| true, flaky(&calls, 3)).await, Ok(4));
    assert_eq!(start.elapsed(), 300 * MS);
    assert_eq!(retry.policy().max_retries, 3);
}

#[test]
fn full_jitter_spreads_delays_over_zero_to_base() {
    let retry = RetryPolicy::fixed(3, 1000 * MS).with_jitter(Jitter::Full);
    let samples: Vec<Duration> = (0..2_000).map(|_| retry.delay_for(0)).collect();
    assert!(samples.iter().all(|d| *d <= 1000 * MS));
    // Spread: both the lowest and the highest fifth of the range are hit.
    assert!(samples.iter().any(|d| *d < 200 * MS));
    assert!(samples.iter().any(|d| *d >= 800 * MS));
    let mean = samples.iter().sum::<Duration>() / samples.len() as u32;
    assert!((400 * MS..600 * MS).contains(&mean), "mean {mean:?}");
    let distinct: std::collections::HashSet<_> = samples.iter().collect();
    assert!(distinct.len() > 1_900, "{} distinct delays", distinct.len());
}

#[test]
fn equal_jitter_keeps_at_least_half_the_delay() {
    let retry = Retry::new(RetryPolicy::fixed(3, 1000 * MS)).jitter(Jitter::Equal);
    let samples: Vec<Duration> = (0..2_000).map(|_| retry.delay_for(0)).collect();
    assert!(samples.iter().all(|d| *d >= 500 * MS && *d <= 1000 * MS));
    assert!(samples.iter().any(|d| *d < 600 * MS));
    assert!(samples.iter().any(|d| *d >= 900 * MS));
}

#[test]
fn jitter_applies_to_the_capped_exponential_delay() {
    let retry = RetryPolicy::exponential(10, 100 * MS, 400 * MS).with_jitter(Jitter::Full);
    for attempt in 0..10 {
        let cap = (100 * MS * 2u32.pow(attempt)).min(400 * MS);
        for _ in 0..200 {
            assert!(retry.delay_for(attempt) <= cap);
        }
    }
    assert_eq!(Jitter::default(), Jitter::None);
}

#[tokio::test(start_paused = true)]
async fn jittered_run_never_waits_longer_than_the_schedule() {
    let retry = RetryPolicy::fixed(4, 100 * MS).with_jitter(Jitter::Full);
    for _ in 0..50 {
        let calls = Cell::new(0);
        let start = Instant::now();
        assert_eq!(retry.run(|_| true, flaky(&calls, 4)).await, Ok(5));
        assert!(start.elapsed() <= 400 * MS);
    }
}

// ---- max elapsed ----------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn max_elapsed_stops_scheduling_retries() {
    let retry = RetryPolicy::fixed(100, 1000 * MS).with_max_elapsed(3500 * MS);
    let calls = Cell::new(0);
    let start = Instant::now();
    // Attempts at t = 0, 1, 2, 3 s; the next would start at 4 s > 3.5 s.
    assert_eq!(
        retry.run(|_| true, flaky(&calls, u32::MAX)).await,
        Err("transient")
    );
    assert_eq!(calls.get(), 4);
    assert_eq!(start.elapsed(), 3000 * MS);
}

#[tokio::test(start_paused = true)]
async fn max_elapsed_counts_time_spent_inside_attempts() {
    let retry = RetryPolicy::fixed(100, 100 * MS).with_max_elapsed(1000 * MS);
    let calls = Cell::new(0);
    let start = Instant::now();
    let out: Result<(), &str> = retry
        .run(
            |_| true,
            || {
                calls.set(calls.get() + 1);
                async {
                    tokio::time::sleep(400 * MS).await; // a slow failing attempt
                    Err("slow")
                }
            },
        )
        .await;
    assert_eq!(out, Err("slow"));
    // Attempts end at 0.4 s and 0.9 s; a third would start at 1.0 s — on the
    // limit, so it runs and ends at 1.4 s; a fourth (1.5 s) is not scheduled.
    assert_eq!(calls.get(), 3);
    assert_eq!(start.elapsed(), 1400 * MS);
}

// ---- the decision hook ------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn hook_sees_each_failed_attempt() {
    let retry = Retry::new(RetryPolicy::exponential(3, 100 * MS, 10_000 * MS));
    let seen: RefCell<Vec<(String, RetryAttempt)>> = RefCell::default();
    let calls = Cell::new(0);
    let out: Result<u32, String> = retry
        .run_with(
            |error: &String, attempt| {
                seen.borrow_mut().push((error.clone(), *attempt));
                RetryDecision::Retry
            },
            || {
                calls.set(calls.get() + 1);
                let n = calls.get();
                async move { Err(format!("failure {n}")) }
            },
        )
        .await;
    assert_eq!(out, Err("failure 4".to_string()));
    let seen = seen.into_inner();
    // Three retries => the hook ran three times; the final failure is
    // returned without asking it.
    assert_eq!(seen.len(), 3);
    for (i, (error, attempt)) in seen.iter().enumerate() {
        let n = i as u32 + 1;
        assert_eq!(error, &format!("failure {n}"));
        assert_eq!(attempt.attempt, n);
        assert_eq!(attempt.retries_left, 4 - n);
        assert_eq!(attempt.delay, 100 * MS * 2u32.pow(i as u32));
    }
    assert_eq!(seen[0].1.elapsed, Duration::ZERO);
    assert_eq!(seen[1].1.elapsed, 100 * MS);
    assert_eq!(seen[2].1.elapsed, 300 * MS);
}

#[tokio::test(start_paused = true)]
async fn hook_can_stop_and_is_skipped_when_no_retry_is_left() {
    let retry = Retry::new(RetryPolicy::fixed(5, 100 * MS));
    let calls = Cell::new(0);
    let out = retry
        .run_with(|_, _| RetryDecision::Stop, flaky(&calls, u32::MAX))
        .await;
    assert_eq!(out, Err("transient"));
    assert_eq!(calls.get(), 1);

    // Zero retries: the hook is never consulted.
    let none = Retry::new(RetryPolicy::fixed(0, 100 * MS));
    let asked = Cell::new(false);
    let calls = Cell::new(0);
    let out = none
        .run_with(
            |_, _| {
                asked.set(true);
                RetryDecision::Retry
            },
            flaky(&calls, u32::MAX),
        )
        .await;
    assert_eq!(out, Err("transient"));
    assert!(!asked.get());

    // Success on the first try: no hook, no delay.
    let calls = Cell::new(0);
    let start = Instant::now();
    let out = retry
        .run_with(|_, _| panic!("must not be called"), flaky(&calls, 0))
        .await;
    assert_eq!(out, Ok(1));
    assert_eq!(start.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn retry_after_overrides_the_policy_delay() {
    // The server says "come back in 7 s"; the policy alone would wait 100 ms.
    let retry = Retry::new(RetryPolicy::fixed(3, 100 * MS)).jitter(Jitter::Full);
    let calls = Cell::new(0);
    let start = Instant::now();
    let out = retry_with(
        &retry,
        |error: &&str, _| match *error {
            "throttled" => RetryDecision::RetryAfter(Duration::from_secs(7)),
            _ => RetryDecision::Stop,
        },
        || {
            calls.set(calls.get() + 1);
            std::future::ready(if calls.get() == 1 {
                Err("throttled")
            } else {
                Ok("served")
            })
        },
    )
    .await;
    assert_eq!(out, Ok("served"));
    // Exactly the requested delay: it replaces the backoff and is not jittered.
    assert_eq!(start.elapsed(), Duration::from_secs(7));
    assert_eq!(RetryDecision::from(true), RetryDecision::Retry);
    assert_eq!(RetryDecision::from(false), RetryDecision::Stop);
}

#[tokio::test(start_paused = true)]
async fn an_excessive_retry_after_stops_instead_of_stalling() {
    let hour = Duration::from_secs(3600);
    // Default cap: 5 minutes.
    let retry = Retry::new(RetryPolicy::fixed(3, 100 * MS));
    let calls = Cell::new(0);
    let start = Instant::now();
    let out = retry
        .run_with(
            |_, _| RetryDecision::RetryAfter(hour),
            flaky(&calls, u32::MAX),
        )
        .await;
    assert_eq!(out, Err("transient"));
    assert_eq!(calls.get(), 1);
    assert_eq!(start.elapsed(), Duration::ZERO);

    // At the cap it is honoured.
    let calls = Cell::new(0);
    let start = Instant::now();
    let out = retry
        .run_with(
            |_, _| RetryDecision::RetryAfter(Duration::from_secs(300)),
            flaky(&calls, 1),
        )
        .await;
    assert_eq!(out, Ok(2));
    assert_eq!(start.elapsed(), Duration::from_secs(300));

    // A custom cap, and no cap at all.
    let strict = Retry::new(RetryPolicy::fixed(3, 100 * MS)).max_retry_after(Some(10 * MS));
    let calls = Cell::new(0);
    let out = strict
        .run_with(|_, _| RetryDecision::RetryAfter(11 * MS), flaky(&calls, 1))
        .await;
    assert_eq!(out, Err("transient"));
    let patient = Retry::new(RetryPolicy::fixed(3, 100 * MS)).max_retry_after(None);
    let calls = Cell::new(0);
    let start = Instant::now();
    let out = patient
        .run_with(|_, _| RetryDecision::RetryAfter(hour), flaky(&calls, 1))
        .await;
    assert_eq!(out, Ok(2));
    assert_eq!(start.elapsed(), hour);
}

#[tokio::test(start_paused = true)]
async fn retry_after_is_bounded_by_max_elapsed() {
    let retry = Retry::new(RetryPolicy::fixed(3, 100 * MS)).max_elapsed(Duration::from_secs(5));
    let calls = Cell::new(0);
    let out = retry
        .run_with(
            |_, _| RetryDecision::RetryAfter(Duration::from_secs(6)),
            flaky(&calls, u32::MAX),
        )
        .await;
    assert_eq!(out, Err("transient"));
    assert_eq!(calls.get(), 1);
}

// ---- retry budget -------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn budget_caps_retries_across_calls() {
    let budget = Arc::new(RetryBudget::new(3, 0.0));
    let retry = RetryPolicy::fixed(5, MS).with_budget(budget.clone());
    assert_eq!(budget.available(), 3.0);

    // The first failing call spends the whole budget (3 of its 5 retries).
    let calls = Cell::new(0);
    assert_eq!(
        retry.run(|_| true, flaky(&calls, u32::MAX)).await,
        Err("transient")
    );
    assert_eq!(calls.get(), 4);
    assert_eq!(budget.available(), 0.0);

    // Every later call gets exactly one attempt: no retry storm.
    for _ in 0..10 {
        let calls = Cell::new(0);
        assert_eq!(
            retry.run(|_| true, flaky(&calls, u32::MAX)).await,
            Err("transient")
        );
        assert_eq!(calls.get(), 1);
    }
    // Calls that succeed first time are unaffected.
    let calls = Cell::new(0);
    assert_eq!(retry.run(|_| true, flaky(&calls, 0)).await, Ok(1));
}

#[tokio::test(start_paused = true)]
async fn budget_refills_over_time_up_to_its_capacity() {
    let budget = RetryBudget::new(2, 0.5); // one retry every two seconds
    assert!(budget.try_withdraw());
    assert!(budget.try_withdraw());
    assert!(!budget.try_withdraw());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(!budget.try_withdraw()); // half a token is not a token
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(budget.try_withdraw());
    assert!(!budget.try_withdraw());
    // Never above capacity, however long it rests.
    tokio::time::advance(Duration::from_secs(3600)).await;
    assert_eq!(budget.available(), 2.0);
}

#[tokio::test(start_paused = true)]
async fn budget_can_be_earned_per_call() {
    // Retries may add at most ~50 % to the call volume.
    let budget = Arc::new(RetryBudget::new(10, 0.0).with_deposit_per_call(0.5));
    while budget.try_withdraw() {}
    let retry = RetryPolicy::fixed(5, MS).with_budget(budget.clone());

    // One call earns half a token: not enough for a retry.
    let calls = Cell::new(0);
    assert_eq!(
        retry.run(|_| true, flaky(&calls, u32::MAX)).await,
        Err("transient")
    );
    assert_eq!(calls.get(), 1);
    // The second call brings it to a whole token: one retry.
    let calls = Cell::new(0);
    assert_eq!(
        retry.run(|_| true, flaky(&calls, u32::MAX)).await,
        Err("transient")
    );
    assert_eq!(calls.get(), 2);
    assert_eq!(budget.available(), 0.0);

    // Nonsense parameters behave as "no refill".
    let odd = RetryBudget::new(1, f64::NAN).with_deposit_per_call(-3.0);
    assert!(odd.try_withdraw());
    odd.record_call();
    tokio::time::advance(Duration::from_secs(100)).await;
    assert!(!odd.try_withdraw());
}

#[tokio::test(start_paused = true)]
async fn budget_is_shared_between_concurrent_callers() {
    let budget = Arc::new(RetryBudget::new(5, 0.0));
    let mut tasks = Vec::new();
    for _ in 0..20 {
        let retry = RetryPolicy::fixed(3, MS).with_budget(budget.clone());
        tasks.push(tokio::spawn(async move {
            let mut calls = 0u32;
            let _: Result<(), ()> = retry
                .run(
                    |_| true,
                    || {
                        calls += 1;
                        std::future::ready(Err(()))
                    },
                )
                .await;
            calls
        }));
    }
    let mut total = 0;
    for task in tasks {
        total += task.await.unwrap();
    }
    // 20 first attempts + exactly the 5 budgeted retries.
    assert_eq!(total, 25);
}

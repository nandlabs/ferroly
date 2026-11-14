#![cfg(feature = "rt")]
//! `ferroly::rt` shutdown helpers, exercised the way an application uses them.
//!
//! The signal test lives in its own test binary because delivering `SIGTERM`
//! affects the whole process.

use ferroly::rt::{self, time::Duration, CancellationToken, TaskTracker};

/// The whole graceful-shutdown story: tracked workers stop when the token is
/// cancelled, and the tracker reports when they have drained.
#[tokio::test]
async fn token_and_tracker_drain_workers() {
    let stop = CancellationToken::new();
    let tasks = TaskTracker::new();
    let (done_tx, mut done_rx) = rt::sync::mpsc::unbounded_channel::<u32>();

    for id in 0..5u32 {
        let stop = stop.child_token();
        let done = done_tx.clone();
        tasks.spawn(async move {
            stop.cancelled().await;
            // Simulated cleanup after the stop request.
            rt::time::sleep(Duration::from_millis(5)).await;
            done.send(id).ok();
        });
    }
    drop(done_tx);
    assert_eq!(tasks.len(), 5);

    stop.cancel();
    tasks
        .shutdown(Duration::from_secs(10))
        .await
        .expect("all workers drain");

    let mut finished = Vec::new();
    while let Some(id) = done_rx.recv().await {
        finished.push(id);
    }
    finished.sort_unstable();
    assert_eq!(finished, vec![0, 1, 2, 3, 4]);
}

/// A worker that ignores the stop request makes `shutdown` time out rather
/// than hang.
#[tokio::test]
async fn shutdown_reports_a_stuck_worker() {
    let tasks = TaskTracker::new();
    let stuck = tasks.spawn(std::future::pending::<()>());
    let err = tasks
        .shutdown(Duration::from_millis(50))
        .await
        .expect_err("the stuck worker must cause a timeout");
    assert_eq!(err.remaining, 1);
    stuck.abort();
}

/// `shutdown_signal` resolves with `Terminate` when the process receives
/// `SIGTERM`.
#[cfg(unix)]
#[tokio::test]
async fn shutdown_signal_resolves_on_sigterm() {
    use rt::signal::unix::{signal, SignalKind};
    use rt::ShutdownSignal;

    // Install a SIGTERM handler up front. From here on the signal's default
    // action (terminating this test process) is replaced for good, so sending
    // it below is safe even if `shutdown_signal` has not been polled yet.
    let _keepalive = signal(SignalKind::terminate()).expect("install SIGTERM handler");

    let waiter = rt::spawn(rt::shutdown_signal());
    let pid = std::process::id().to_string();

    // A listener only sees signals delivered after it registered, so keep
    // sending until the waiter reports one.
    let outcome = rt::time::timeout(Duration::from_secs(20), async {
        let mut waiter = waiter;
        loop {
            let status = std::process::Command::new("kill")
                .args(["-TERM", &pid])
                .status()
                .expect("run kill");
            assert!(status.success(), "kill -TERM failed");
            rt::select! {
                result = &mut waiter => break result,
                _ = rt::time::sleep(Duration::from_millis(50)) => {}
            }
        }
    })
    .await
    .expect("shutdown_signal did not resolve after SIGTERM");

    let signal = outcome.expect("task joined").expect("handlers installed");
    assert_eq!(signal, ShutdownSignal::Terminate);
}

#![cfg(all(feature = "lifecycle", unix))]
//! `start_and_wait` against real OS signals.
//!
//! This lives in its own test binary on purpose: signals are delivered to the
//! whole process, so a `SIGTERM` raised here would also reach any other
//! `start_and_wait` running in the same binary. Keep it to this one test.

use std::sync::Arc;
use std::time::Duration;

use ferroly::lifecycle::{ComponentManager, ComponentState, ManagerOptions, SimpleComponent};

/// Sends `signal` (e.g. `"HUP"`) to this process.
fn raise(signal: &str) {
    let status = std::process::Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(std::process::id().to_string())
        .status()
        .expect("the `kill` utility is available");
    assert!(status.success(), "kill -{signal} failed");
}

#[tokio::test]
async fn sighup_is_a_reload_and_sigterm_requests_a_shutdown() {
    // Generous real-time bound on signal delivery; it is not a pacing sleep.
    const DELIVERY: Duration = Duration::from_secs(30);

    let mgr = Arc::new(ComponentManager::with_options(
        ManagerOptions::new().reload_on_sighup(true),
    ));
    mgr.register(Arc::new(SimpleComponent::new(
        "svc",
        || Box::pin(async { Ok(()) }),
        || Box::pin(async { Ok(()) }),
    )))
    .unwrap();
    let mut reload = mgr.subscribe_reload();

    let runner = tokio::spawn({
        let mgr = mgr.clone();
        async move { mgr.start_and_wait().await }
    });
    // The handlers are installed before any component starts, so once the
    // component is running it is safe to raise the signals.
    let mut state = mgr.watch("svc").unwrap();
    state
        .wait_for(|s| *s == ComponentState::Running)
        .await
        .unwrap();

    raise("HUP");
    tokio::time::timeout(DELIVERY, reload.changed())
        .await
        .expect("SIGHUP must produce a reload notification")
        .unwrap();
    assert_eq!(*reload.borrow_and_update(), 1);
    // A reload is not a shutdown.
    assert_eq!(mgr.get_state("svc"), Some(ComponentState::Running));
    assert!(!runner.is_finished());

    raise("TERM");
    tokio::time::timeout(DELIVERY, runner)
        .await
        .expect("SIGTERM must end start_and_wait")
        .unwrap()
        .unwrap();
    assert_eq!(mgr.get_state("svc"), Some(ComponentState::Stopped));
}

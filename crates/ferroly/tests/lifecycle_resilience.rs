#![cfg(feature = "lifecycle")]
//! Regression tests for the lifecycle correctness fixes: cancelled and
//! panicking starts, concurrent starts, latched `wait`, best-effort `stop_all`,
//! rollback, duplicate registration, and health derived from component state.
//!
//! Timing-sensitive tests run on tokio's paused clock, so every sleep and
//! timeout is virtual and the outcome is deterministic.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::lifecycle::{
    BoxFuture, Component, ComponentManager, ComponentState, HealthRegistry, HealthStatus,
    LifecycleError, ManagerOptions, SimpleComponent,
};
use tokio::time::Instant;

type Log = Arc<Mutex<Vec<String>>>;

fn new_log() -> Log {
    Arc::new(Mutex::new(Vec::new()))
}

fn events(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

/// What a scripted start/stop does once its delay has passed.
#[derive(Clone, Copy)]
enum Step {
    Ok,
    Fail,
    Panic,
    Hang,
}

type Script = Box<dyn Fn(u32) -> Step + Send + Sync>;

/// A component whose start/stop follow a script keyed by the attempt number
/// (1-based), and which records `start:<id>` / `stop:<id>` when each begins.
struct Scripted {
    id: String,
    log: Log,
    start_delay: Duration,
    stop_delay: Duration,
    on_start: Script,
    on_stop: Script,
    starts: AtomicU32,
    stops: AtomicU32,
}

impl Scripted {
    fn new(id: &str, log: &Log) -> Self {
        Self {
            id: id.to_string(),
            log: log.clone(),
            start_delay: Duration::ZERO,
            stop_delay: Duration::ZERO,
            on_start: Box::new(|_| Step::Ok),
            on_stop: Box::new(|_| Step::Ok),
            starts: AtomicU32::new(0),
            stops: AtomicU32::new(0),
        }
    }

    fn start_delay(mut self, delay: Duration) -> Self {
        self.start_delay = delay;
        self
    }

    fn on_start(mut self, script: impl Fn(u32) -> Step + Send + Sync + 'static) -> Self {
        self.on_start = Box::new(script);
        self
    }

    fn on_stop(mut self, script: impl Fn(u32) -> Step + Send + Sync + 'static) -> Self {
        self.on_stop = Box::new(script);
        self
    }

    fn arc(self) -> Arc<Self> {
        Arc::new(self)
    }

    fn starts(&self) -> u32 {
        self.starts.load(Ordering::SeqCst)
    }

    fn stops(&self) -> u32 {
        self.stops.load(Ordering::SeqCst)
    }

    async fn run(&self, what: &str, delay: Duration, step: Step) -> Result<(), LifecycleError> {
        self.log.lock().unwrap().push(format!("{what}:{}", self.id));
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        match step {
            Step::Ok => Ok(()),
            Step::Fail => Err(LifecycleError::failure(
                self.id.clone(),
                std::io::Error::other(format!("{what} failed")),
            )),
            Step::Panic => panic!("{what} of {} panicked", self.id),
            Step::Hang => std::future::pending().await,
        }
    }
}

impl Component for Scripted {
    fn id(&self) -> &str {
        &self.id
    }

    fn start(&self) -> BoxFuture<'_, Result<(), LifecycleError>> {
        Box::pin(async move {
            let attempt = self.starts.fetch_add(1, Ordering::SeqCst) + 1;
            self.run("start", self.start_delay, (self.on_start)(attempt))
                .await
        })
    }

    fn stop(&self) -> BoxFuture<'_, Result<(), LifecycleError>> {
        Box::pin(async move {
            let attempt = self.stops.fetch_add(1, Ordering::SeqCst) + 1;
            self.run("stop", self.stop_delay, (self.on_stop)(attempt))
                .await
        })
    }
}

/// Waits until `id` is in `state`.
async fn wait_state(mgr: &ComponentManager, id: &str, state: ComponentState) {
    let mut rx = mgr.watch(id).expect("registered");
    rx.wait_for(|s| *s == state)
        .await
        .expect("still registered");
}

const GUARD: Duration = Duration::from_secs(3600);

// ---- P0-10: a cancelled or panicking start never stays in `Starting` -------

#[tokio::test(start_paused = true)]
async fn timed_out_start_is_settled_as_error_and_the_retry_really_starts() {
    let log = new_log();
    let mgr = ComponentManager::new();
    // `db` hangs on its first start and succeeds on the second.
    let db = Scripted::new("db", &log)
        .on_start(|attempt| if attempt == 1 { Step::Hang } else { Step::Ok })
        .arc();
    mgr.register(db.clone()).unwrap();
    mgr.register(Scripted::new("api", &log).arc()).unwrap();
    mgr.add_dependency("api", "db").unwrap();

    let err = mgr
        .start_all_with_timeout(Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(matches!(err, LifecycleError::Timeout(_)), "{err}");
    // The bug: `db` used to stay `Starting` forever.
    assert_eq!(mgr.get_state("db"), Some(ComponentState::Error));
    assert_eq!(mgr.get_state("api"), Some(ComponentState::Unknown));
    assert!(!events(&log).contains(&"start:api".to_string()));

    // The retry must run `db.start()` again rather than report an instant Ok.
    mgr.start_all().await.unwrap();
    assert_eq!(db.starts(), 2);
    assert_eq!(mgr.get_state("db"), Some(ComponentState::Running));
    assert_eq!(mgr.get_state("api"), Some(ComponentState::Running));
}

#[tokio::test(start_paused = true)]
async fn retry_after_timeout_does_not_report_ok_or_start_dependents_when_it_fails_again() {
    let log = new_log();
    let mgr = ComponentManager::new();
    let db = Scripted::new("db", &log)
        .on_start(|attempt| if attempt == 1 { Step::Hang } else { Step::Fail })
        .arc();
    mgr.register(db.clone()).unwrap();
    mgr.register(Scripted::new("api", &log).arc()).unwrap();
    mgr.add_dependency("api", "db").unwrap();

    assert!(mgr
        .start_all_with_timeout(Duration::from_secs(1))
        .await
        .is_err());

    let err = mgr.start_all().await.unwrap_err();
    assert!(
        matches!(&err, LifecycleError::ComponentFailure { id, .. } if id == "db"),
        "{err}"
    );
    assert_eq!(mgr.get_state("db"), Some(ComponentState::Error));
    // The dependent was never started on top of a dependency that is not up.
    assert_eq!(mgr.get_state("api"), Some(ComponentState::Unknown));
    assert!(!events(&log).contains(&"start:api".to_string()));
}

#[tokio::test(start_paused = true)]
async fn dropping_the_start_future_settles_the_component_as_error() {
    let log = new_log();
    let mgr = Arc::new(ComponentManager::new());
    mgr.register(Scripted::new("svc", &log).on_start(|_| Step::Hang).arc())
        .unwrap();

    let task = tokio::spawn({
        let mgr = mgr.clone();
        async move { mgr.start("svc").await }
    });
    wait_state(&mgr, "svc", ComponentState::Starting).await;

    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(mgr.get_state("svc"), Some(ComponentState::Error));
}

#[tokio::test(start_paused = true)]
async fn concurrent_start_waits_for_the_owner_and_shares_its_success() {
    let log = new_log();
    let mgr = ComponentManager::new();
    let db = Scripted::new("db", &log)
        .start_delay(Duration::from_secs(5))
        .arc();
    mgr.register(db.clone()).unwrap();

    let began = Instant::now();
    let (first, second) = tokio::join!(mgr.start("db"), mgr.start("db"));
    first.unwrap();
    second.unwrap();

    // The second caller waited for the real outcome instead of returning at once.
    assert_eq!(began.elapsed(), Duration::from_secs(5));
    assert_eq!(db.starts(), 1, "the component must be started exactly once");
    assert_eq!(mgr.get_state("db"), Some(ComponentState::Running));
}

#[tokio::test(start_paused = true)]
async fn concurrent_start_reports_the_failure_to_every_caller() {
    let log = new_log();
    let mgr = ComponentManager::new();
    let db = Scripted::new("db", &log)
        .start_delay(Duration::from_secs(5))
        .on_start(|_| Step::Fail)
        .arc();
    mgr.register(db.clone()).unwrap();
    mgr.register(Scripted::new("api", &log).arc()).unwrap();
    mgr.add_dependency("api", "db").unwrap();

    // The first call owns `db`'s start; the second needs `db` as a dependency.
    let (owner, dependent) = tokio::join!(mgr.start("db"), mgr.start("api"));

    let owner = owner.unwrap_err();
    assert!(
        matches!(&owner, LifecycleError::ComponentFailure { id, .. } if id == "db"),
        "{owner}"
    );
    let dependent = dependent.unwrap_err();
    assert!(
        matches!(&dependent, LifecycleError::StartFailed(id) if id == "db"),
        "{dependent}"
    );
    assert_eq!(db.starts(), 1);
    assert_eq!(mgr.get_state("api"), Some(ComponentState::Unknown));
    assert!(!events(&log).contains(&"start:api".to_string()));
}

#[tokio::test(start_paused = true)]
async fn a_panicking_start_is_contained_and_reported() {
    let log = new_log();
    let mgr = ComponentManager::new();
    let svc = Scripted::new("svc", &log)
        .on_start(|attempt| if attempt == 1 { Step::Panic } else { Step::Ok })
        .arc();
    mgr.register(svc.clone()).unwrap();

    let err = mgr.start_all().await.unwrap_err();
    match &err {
        LifecycleError::ComponentPanicked { id, message } => {
            assert_eq!(id, "svc");
            assert!(message.contains("start of svc panicked"), "{message}");
        }
        other => panic!("expected ComponentPanicked, got {other}"),
    }
    assert_eq!(mgr.get_state("svc"), Some(ComponentState::Error));

    // The manager is still usable and the component can be started again.
    mgr.start_all().await.unwrap();
    assert_eq!(mgr.get_state("svc"), Some(ComponentState::Running));
}

#[tokio::test(start_paused = true)]
async fn a_panicking_stop_is_contained_and_the_sweep_continues() {
    let log = new_log();
    let mgr = ComponentManager::new();
    mgr.register(Scripted::new("a", &log).arc()).unwrap();
    mgr.register(Scripted::new("b", &log).on_stop(|_| Step::Panic).arc())
        .unwrap();
    mgr.add_dependency("b", "a").unwrap();
    mgr.start_all().await.unwrap();

    let err = mgr.stop_all().await.unwrap_err();
    assert!(
        matches!(&err, LifecycleError::ComponentPanicked { id, .. } if id == "b"),
        "{err}"
    );
    assert_eq!(mgr.get_state("b"), Some(ComponentState::Error));
    assert_eq!(mgr.get_state("a"), Some(ComponentState::Stopped));
}

// ---- I4: latched wait, best-effort stop_all, start_and_wait ----------------

#[tokio::test(start_paused = true)]
async fn wait_after_stop_all_returns_immediately() {
    let log = new_log();
    let mgr = ComponentManager::new();
    mgr.register(Scripted::new("svc", &log).arc()).unwrap();
    mgr.start_all().await.unwrap();
    mgr.stop_all().await.unwrap();

    // Used to hang forever: the notification had already been delivered.
    let began = Instant::now();
    tokio::time::timeout(GUARD, mgr.wait())
        .await
        .expect("wait() must return once stop_all has completed");
    assert_eq!(began.elapsed(), Duration::ZERO);

    // A new start clears the latch, so `wait` blocks again.
    mgr.start_all().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(60), mgr.wait())
            .await
            .is_err(),
        "wait() must block while the components are running again"
    );
}

#[tokio::test(start_paused = true)]
async fn wait_blocks_until_a_stop_all_completes() {
    let log = new_log();
    let mgr = Arc::new(ComponentManager::new());
    mgr.register(Scripted::new("svc", &log).arc()).unwrap();
    mgr.start_all().await.unwrap();

    let waiter = tokio::spawn({
        let mgr = mgr.clone();
        async move { mgr.wait().await }
    });
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert!(!waiter.is_finished());

    mgr.stop_all().await.unwrap();
    tokio::time::timeout(GUARD, waiter).await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn stop_all_attempts_every_component_and_collects_all_errors() {
    let log = new_log();
    let mgr = Arc::new(ComponentManager::new());
    // Stop order is c, b, a. Both `c` and `b` fail; `a` must still be stopped.
    mgr.register(Scripted::new("a", &log).arc()).unwrap();
    mgr.register(Scripted::new("b", &log).on_stop(|_| Step::Fail).arc())
        .unwrap();
    mgr.register(Scripted::new("c", &log).on_stop(|_| Step::Fail).arc())
        .unwrap();
    mgr.add_dependency("b", "a").unwrap();
    mgr.add_dependency("c", "b").unwrap();
    mgr.start_all().await.unwrap();

    let waiter = tokio::spawn({
        let mgr = mgr.clone();
        async move { mgr.wait().await }
    });

    let err = mgr.stop_all().await.unwrap_err();
    assert!(matches!(err, LifecycleError::Multiple(_)), "{err}");
    let failed: Vec<&str> = err
        .errors()
        .iter()
        .map(|e| match e {
            LifecycleError::ComponentFailure { id, .. } => id.as_str(),
            other => panic!("unexpected error {other}"),
        })
        .collect();
    assert_eq!(failed, vec!["c", "b"]);
    assert!(err.to_string().starts_with("2 errors: "), "{err}");

    assert_eq!(
        events(&log)[3..],
        ["stop:c", "stop:b", "stop:a"].map(String::from)
    );
    assert_eq!(mgr.get_state("c"), Some(ComponentState::Error));
    assert_eq!(mgr.get_state("b"), Some(ComponentState::Error));
    // Used to be left `Running` because the sweep aborted on the first error.
    assert_eq!(mgr.get_state("a"), Some(ComponentState::Stopped));

    // Waiters are released even though the sweep reported errors.
    tokio::time::timeout(GUARD, waiter).await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn targeted_stop_is_best_effort_too() {
    let log = new_log();
    let mgr = ComponentManager::new();
    mgr.register(Scripted::new("db", &log).arc()).unwrap();
    mgr.register(Scripted::new("api", &log).on_stop(|_| Step::Fail).arc())
        .unwrap();
    mgr.add_dependency("api", "db").unwrap();
    mgr.start_all().await.unwrap();

    // Stopping `db` stops its dependent first; that fails, `db` still stops.
    let err = mgr.stop("db").await.unwrap_err();
    assert!(
        matches!(&err, LifecycleError::ComponentFailure { id, .. } if id == "api"),
        "{err}"
    );
    assert_eq!(mgr.get_state("db"), Some(ComponentState::Stopped));
}

#[tokio::test(start_paused = true)]
async fn start_and_wait_returns_when_stop_all_is_called_from_elsewhere() {
    let log = new_log();
    let mgr = Arc::new(ComponentManager::new());
    mgr.register(Scripted::new("svc", &log).arc()).unwrap();

    let runner = tokio::spawn({
        let mgr = mgr.clone();
        async move { mgr.start_and_wait().await }
    });
    wait_state(&mgr, "svc", ComponentState::Running).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        !runner.is_finished(),
        "must keep running until asked to stop"
    );

    mgr.stop_all().await.unwrap();
    tokio::time::timeout(GUARD, runner)
        .await
        .expect("start_and_wait must return after an external stop_all")
        .unwrap()
        .unwrap();
    assert_eq!(mgr.get_state("svc"), Some(ComponentState::Stopped));
}

// ---- rollback of a failed start -------------------------------------------

#[tokio::test(start_paused = true)]
async fn failed_start_all_rolls_back_started_components_in_reverse_order() {
    let log = new_log();
    let mgr = ComponentManager::new();
    mgr.register(Scripted::new("a", &log).arc()).unwrap();
    mgr.register(Scripted::new("b", &log).arc()).unwrap();
    mgr.register(Scripted::new("c", &log).on_start(|_| Step::Fail).arc())
        .unwrap();
    mgr.add_dependency("b", "a").unwrap();
    mgr.add_dependency("c", "b").unwrap();

    let err = mgr.start_all().await.unwrap_err();
    // A clean rollback leaves the start failure as the error.
    assert!(
        matches!(&err, LifecycleError::ComponentFailure { id, .. } if id == "c"),
        "{err}"
    );
    assert_eq!(
        events(&log),
        ["start:a", "start:b", "start:c", "stop:b", "stop:a"].map(String::from)
    );
    assert_eq!(mgr.get_state("a"), Some(ComponentState::Stopped));
    assert_eq!(mgr.get_state("b"), Some(ComponentState::Stopped));
    assert_eq!(mgr.get_state("c"), Some(ComponentState::Error));
}

#[tokio::test(start_paused = true)]
async fn rollback_can_be_switched_off() {
    let log = new_log();
    let mgr =
        ComponentManager::with_options(ManagerOptions::new().rollback_on_start_failure(false));
    mgr.register(Scripted::new("a", &log).arc()).unwrap();
    mgr.register(Scripted::new("b", &log).on_start(|_| Step::Fail).arc())
        .unwrap();
    mgr.add_dependency("b", "a").unwrap();

    assert!(mgr.start_all().await.is_err());
    assert_eq!(mgr.get_state("a"), Some(ComponentState::Running));
    assert_eq!(events(&log), ["start:a", "start:b"].map(String::from));
}

#[tokio::test(start_paused = true)]
async fn timed_out_start_all_rolls_back_what_it_started() {
    let log = new_log();
    let mgr = ComponentManager::new();
    mgr.register(Scripted::new("a", &log).arc()).unwrap();
    mgr.register(Scripted::new("b", &log).on_start(|_| Step::Hang).arc())
        .unwrap();
    mgr.add_dependency("b", "a").unwrap();

    let err = mgr
        .start_all_with_timeout(Duration::from_secs(2))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, LifecycleError::Timeout(what) if what == "<all>"),
        "{err}"
    );
    assert_eq!(mgr.get_state("a"), Some(ComponentState::Stopped));
    assert_eq!(mgr.get_state("b"), Some(ComponentState::Error));
}

#[tokio::test(start_paused = true)]
async fn a_failing_rollback_is_reported_after_the_start_error() {
    let log = new_log();
    let mgr = ComponentManager::new();
    mgr.register(Scripted::new("a", &log).on_stop(|_| Step::Fail).arc())
        .unwrap();
    mgr.register(Scripted::new("b", &log).on_start(|_| Step::Fail).arc())
        .unwrap();
    mgr.add_dependency("b", "a").unwrap();

    let err = mgr.start_all().await.unwrap_err();
    let ids: Vec<&str> = err
        .errors()
        .iter()
        .map(|e| match e {
            LifecycleError::ComponentFailure { id, .. } => id.as_str(),
            other => panic!("unexpected error {other}"),
        })
        .collect();
    assert_eq!(
        ids,
        vec!["b", "a"],
        "start failure first, then the rollback"
    );
}

#[tokio::test(start_paused = true)]
async fn targeted_start_rolls_back_only_what_it_started() {
    let log = new_log();
    let mgr = ComponentManager::new();
    mgr.register(Scripted::new("db", &log).arc()).unwrap();
    mgr.register(Scripted::new("cache", &log).arc()).unwrap();
    mgr.register(Scripted::new("api", &log).on_start(|_| Step::Fail).arc())
        .unwrap();
    mgr.add_dependency("api", "db").unwrap();
    mgr.add_dependency("api", "cache").unwrap();

    // `db` was already running before the failing call: it must be left alone.
    mgr.start("db").await.unwrap();
    assert!(mgr.start("api").await.is_err());
    assert_eq!(mgr.get_state("db"), Some(ComponentState::Running));
    assert_eq!(mgr.get_state("cache"), Some(ComponentState::Stopped));
    assert_eq!(mgr.get_state("api"), Some(ComponentState::Error));
}

// ---- I5: registration ------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn registering_a_duplicate_id_is_an_error_and_keeps_the_original() {
    let log = new_log();
    let mgr = ComponentManager::new();
    let original = Scripted::new("svc", &log).arc();
    let impostor = Scripted::new("svc", &log).arc();
    mgr.register(original.clone()).unwrap();
    mgr.start_all().await.unwrap();

    let err = mgr.register(impostor.clone()).unwrap_err();
    assert!(
        matches!(&err, LifecycleError::DuplicateComponent(id) if id == "svc"),
        "{err}"
    );
    // The running component was not replaced: its state and identity survive.
    assert_eq!(mgr.get_state("svc"), Some(ComponentState::Running));
    mgr.stop_all().await.unwrap();
    assert_eq!(original.stops(), 1);
    assert_eq!(impostor.starts() + impostor.stops(), 0);
}

#[tokio::test(start_paused = true)]
async fn unregistering_an_active_component_is_refused() {
    let log = new_log();
    let mgr = ComponentManager::new();
    let svc = Scripted::new("svc", &log).arc();
    mgr.register(svc.clone()).unwrap();
    mgr.start_all().await.unwrap();

    let err = mgr.unregister("svc").unwrap_err();
    assert!(
        matches!(&err, LifecycleError::ComponentActive(id) if id == "svc"),
        "{err}"
    );
    assert_eq!(mgr.get_state("svc"), Some(ComponentState::Running));

    // Stop first, then it can go — and the id can be registered again.
    mgr.stop("svc").await.unwrap();
    mgr.unregister("svc").unwrap();
    assert_eq!(mgr.get_state("svc"), None);
    mgr.register(Scripted::new("svc", &log).arc()).unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_repeated_dependency_edge_survives_a_rejected_cycle() {
    let log = new_log();
    let mgr = ComponentManager::new();
    mgr.register(Scripted::new("a", &log).arc()).unwrap();
    mgr.register(Scripted::new("b", &log).arc()).unwrap();
    mgr.add_dependency("b", "a").unwrap();
    mgr.add_dependency("b", "a").unwrap(); // declaring it twice is harmless
    assert!(mgr.add_dependency("a", "b").is_err());

    mgr.start_all().await.unwrap();
    assert_eq!(events(&log), ["start:a", "start:b"].map(String::from));
}

// ---- D1: health derived from component state -------------------------------

#[tokio::test(start_paused = true)]
async fn health_and_readiness_follow_component_state() {
    let log = new_log();
    let mgr = ComponentManager::new();
    mgr.register(Scripted::new("db", &log).arc()).unwrap();
    mgr.register(Scripted::new("api", &log).arc()).unwrap();
    mgr.add_dependency("api", "db").unwrap();

    // Registered but not started: not ready, yet nothing has failed.
    let report = mgr.health();
    assert_eq!(
        report.checks(),
        [
            ("api".to_string(), HealthStatus::Degraded),
            ("db".to_string(), HealthStatus::Degraded)
        ]
    );
    assert!(!mgr.is_ready());
    assert!(mgr.is_live());

    // Only `db` running: still not ready.
    mgr.start("db").await.unwrap();
    assert_eq!(mgr.health().get("db"), Some(HealthStatus::Up));
    assert!(!mgr.is_ready());

    mgr.start_all().await.unwrap();
    assert!(mgr.is_ready());
    assert_eq!(mgr.health().overall(), HealthStatus::Up);

    // A failed component makes the manager neither ready nor live.
    mgr.report_failure("db", "connection lost").unwrap();
    assert_eq!(mgr.get_state("db"), Some(ComponentState::Error));
    assert_eq!(mgr.health().get("db"), Some(HealthStatus::Down));
    assert!(!mgr.is_ready());
    assert!(!mgr.is_live());

    mgr.stop_all().await.unwrap();
    assert!(!mgr.is_ready());
    assert!(mgr.is_live());
    assert_eq!(
        mgr.health().to_json(),
        r#"{"overall":"degraded","checks":{"api":"degraded","db":"degraded"}}"#
    );
}

#[tokio::test(start_paused = true)]
async fn a_component_can_refine_its_health_while_running() {
    let status = Arc::new(Mutex::new(HealthStatus::Up));
    let probe = status.clone();
    let mgr = ComponentManager::new();
    mgr.register(Arc::new(
        SimpleComponent::new(
            "pool",
            || Box::pin(async { Ok(()) }),
            || Box::pin(async { Ok(()) }),
        )
        .with_health(move || *probe.lock().unwrap()),
    ))
    .unwrap();

    // The self-reported health is ignored until the component is running.
    *status.lock().unwrap() = HealthStatus::Down;
    assert_eq!(mgr.health().get("pool"), Some(HealthStatus::Degraded));

    *status.lock().unwrap() = HealthStatus::Up;
    mgr.start_all().await.unwrap();
    assert!(mgr.is_ready());

    *status.lock().unwrap() = HealthStatus::Degraded;
    assert!(!mgr.is_ready());
    assert!(mgr.is_live());

    *status.lock().unwrap() = HealthStatus::Down;
    assert!(!mgr.is_live());
}

#[tokio::test(start_paused = true)]
async fn a_registry_reports_components_without_keeping_the_manager_alive() {
    let log = new_log();
    let mgr = ComponentManager::new();
    mgr.register(Scripted::new("db", &log).arc()).unwrap();

    let registry = HealthRegistry::new();
    registry.register("disk", || HealthStatus::Up);
    registry.register_components(&mgr);

    // Not started yet: the component keeps the registry from being ready.
    assert!(!registry.is_ready());
    assert_eq!(registry.overall(), HealthStatus::Degraded);
    assert_eq!(
        registry.report(),
        vec![
            ("disk".to_string(), HealthStatus::Up),
            ("db".to_string(), HealthStatus::Degraded)
        ]
    );

    mgr.start_all().await.unwrap();
    assert!(registry.is_ready());
    assert!(registry.check().await.is_ready());
    assert_eq!(
        registry.to_json(),
        r#"{"overall":"up","checks":{"disk":"up","db":"up"}}"#
    );

    // The registry holds the manager weakly: its entries vanish with it.
    drop(mgr);
    assert_eq!(
        registry.report(),
        vec![("disk".to_string(), HealthStatus::Up)]
    );
}

// ---- multi-threaded runtime -------------------------------------------------

/// Many tasks racing to start the same components on a real multi-threaded
/// runtime: each component is started exactly once and every caller sees the
/// real outcome.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_starts_run_each_component_once() {
    for _ in 0..50 {
        let log = new_log();
        let mgr = Arc::new(ComponentManager::with_options(
            ManagerOptions::new().parallel_start(true),
        ));
        let components: Vec<Arc<Scripted>> = ["a", "b", "c", "d"]
            .iter()
            .map(|id| Scripted::new(id, &log).arc())
            .collect();
        for component in &components {
            mgr.register(component.clone()).unwrap();
        }
        mgr.add_dependency("c", "a").unwrap();
        mgr.add_dependency("d", "c").unwrap();

        let racers: Vec<_> = (0..8)
            .map(|_| {
                let mgr = mgr.clone();
                tokio::spawn(async move { mgr.start_all().await })
            })
            .collect();
        for racer in racers {
            racer.await.unwrap().unwrap();
        }

        assert!(mgr.is_ready());
        for component in &components {
            assert_eq!(component.starts(), 1, "{} started once", component.id());
        }
        mgr.stop_all().await.unwrap();
        for component in &components {
            assert_eq!(component.stops(), 1, "{} stopped once", component.id());
        }
    }
}

#![cfg(feature = "lifecycle")]
//! Tests for the opt-in lifecycle capabilities: parallel start, per-component
//! timeouts, supervised restarts, the global event stream and observer, the
//! shutdown handle, reload notifications, and asynchronous health checks.
//!
//! Everything runs on tokio's paused clock: sleeps, backoffs and timeouts are
//! virtual, so elapsed times can be asserted exactly.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::lifecycle::{
    CheckOptions, ComponentManager, ComponentOptions, ComponentState, HealthRegistry, HealthStatus,
    LifecycleError, LifecycleObserver, ManagerOptions, ProbeKind, RestartPolicy, SimpleComponent,
    StateChange,
};
use tokio::sync::broadcast::error::TryRecvError;
use tokio::time::Instant;

type Log = Arc<Mutex<Vec<String>>>;

fn new_log() -> Log {
    Arc::new(Mutex::new(Vec::new()))
}

fn events(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

fn failure(id: &str, what: &str) -> LifecycleError {
    LifecycleError::failure(id, std::io::Error::other(what.to_string()))
}

/// A component that logs `start:<id>` / `started:<id>` / `stop:<id>` and takes
/// `start_delay` to start.
fn timed(id: &'static str, log: &Log, start_delay: Duration) -> Arc<SimpleComponent> {
    let (on_start, on_stop) = (log.clone(), log.clone());
    Arc::new(SimpleComponent::new(
        id,
        move || {
            let log = on_start.clone();
            Box::pin(async move {
                log.lock().unwrap().push(format!("start:{id}"));
                tokio::time::sleep(start_delay).await;
                log.lock().unwrap().push(format!("started:{id}"));
                Ok(())
            })
        },
        move || {
            let log = on_stop.clone();
            Box::pin(async move {
                log.lock().unwrap().push(format!("stop:{id}"));
                Ok(())
            })
        },
    ))
}

/// A component whose `start` never completes.
fn hanging_start(id: &'static str) -> Arc<SimpleComponent> {
    Arc::new(SimpleComponent::new(
        id,
        || Box::pin(std::future::pending()),
        || Box::pin(async { Ok(()) }),
    ))
}

/// A component that counts its starts/stops and fails the start attempts for
/// which `fails` returns `true` (attempts are 1-based).
struct Flaky {
    component: Arc<SimpleComponent>,
    starts: Arc<AtomicU32>,
    stops: Arc<AtomicU32>,
}

fn flaky(id: &'static str, fails: impl Fn(u32) -> bool + Send + Sync + 'static) -> Flaky {
    let starts = Arc::new(AtomicU32::new(0));
    let stops = Arc::new(AtomicU32::new(0));
    let (s, t) = (starts.clone(), stops.clone());
    let fails = Arc::new(fails);
    let component = Arc::new(SimpleComponent::new(
        id,
        move || {
            let attempt = s.fetch_add(1, Ordering::SeqCst) + 1;
            let fails = fails.clone();
            Box::pin(async move {
                if fails(attempt) {
                    Err(failure(id, "start failed"))
                } else {
                    Ok(())
                }
            })
        },
        move || {
            t.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        },
    ));
    Flaky {
        component,
        starts,
        stops,
    }
}

impl Flaky {
    fn starts(&self) -> u32 {
        self.starts.load(Ordering::SeqCst)
    }

    fn stops(&self) -> u32 {
        self.stops.load(Ordering::SeqCst)
    }
}

async fn wait_state(mgr: &ComponentManager, id: &str, state: ComponentState) {
    let mut rx = mgr.watch(id).expect("registered");
    rx.wait_for(|s| *s == state)
        .await
        .expect("still registered");
}

const GUARD: Duration = Duration::from_secs(3600);

// ---- parallel start --------------------------------------------------------

fn three_tier(mgr: &ComponentManager, log: &Log) {
    mgr.register(timed("db", log, Duration::from_secs(10)))
        .unwrap();
    mgr.register(timed("cache", log, Duration::from_secs(10)))
        .unwrap();
    mgr.register(timed("api", log, Duration::from_secs(5)))
        .unwrap();
    mgr.add_dependency("api", "db").unwrap();
    mgr.add_dependency("api", "cache").unwrap();
}

#[tokio::test(start_paused = true)]
async fn sequential_start_is_the_default() {
    let log = new_log();
    let mgr = ComponentManager::new();
    three_tier(&mgr, &log);

    let began = Instant::now();
    mgr.start_all().await.unwrap();
    assert_eq!(began.elapsed(), Duration::from_secs(25));
}

#[tokio::test(start_paused = true)]
async fn parallel_start_overlaps_the_components_of_a_level() {
    let log = new_log();
    let mgr = ComponentManager::with_options(ManagerOptions::new().parallel_start(true));
    three_tier(&mgr, &log);

    let began = Instant::now();
    mgr.start_all().await.unwrap();
    // db and cache (level 0) overlap: 10 s, then api: 5 s.
    assert_eq!(began.elapsed(), Duration::from_secs(15));

    // The dependent only starts once the whole level below it is up.
    let events = events(&log);
    let at = |e: &str| events.iter().position(|x| x == e).unwrap();
    assert!(at("start:cache") < at("started:db"), "level 0 must overlap");
    assert!(at("started:db") < at("start:api"));
    assert!(at("started:cache") < at("start:api"));
    assert!(mgr.is_ready());

    // Stop is still sequential, dependents first.
    mgr.stop_all().await.unwrap();
    assert_eq!(events_tail(&log, 3)[0], "stop:api");
}

fn events_tail(log: &Log, n: usize) -> Vec<String> {
    let all = events(log);
    all[all.len() - n..].to_vec()
}

#[tokio::test(start_paused = true)]
async fn parallel_start_failure_waits_for_the_level_then_rolls_back() {
    let log = new_log();
    let mgr = ComponentManager::with_options(ManagerOptions::new().parallel_start(true));
    mgr.register(timed("db", &log, Duration::from_secs(10)))
        .unwrap();
    mgr.register(Arc::new(SimpleComponent::new(
        "cache",
        || {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(2)).await;
                Err(failure("cache", "no route"))
            })
        },
        || Box::pin(async { Ok(()) }),
    )))
    .unwrap();
    mgr.register(timed("api", &log, Duration::ZERO)).unwrap();
    mgr.add_dependency("api", "db").unwrap();
    mgr.add_dependency("api", "cache").unwrap();

    let began = Instant::now();
    let err = mgr.start_all().await.unwrap_err();
    assert!(
        matches!(&err, LifecycleError::ComponentFailure { id, .. } if id == "cache"),
        "{err}"
    );
    // The sibling was allowed to finish (10 s) and was then rolled back.
    assert_eq!(began.elapsed(), Duration::from_secs(10));
    assert_eq!(mgr.get_state("db"), Some(ComponentState::Stopped));
    assert_eq!(mgr.get_state("cache"), Some(ComponentState::Error));
    assert_eq!(mgr.get_state("api"), Some(ComponentState::Unknown));
    assert_eq!(
        events(&log),
        ["start:db", "started:db", "stop:db"].map(String::from)
    );
}

#[tokio::test(start_paused = true)]
async fn parallel_start_timeout_leaves_nothing_starting() {
    let log = new_log();
    let mgr = ComponentManager::with_options(ManagerOptions::new().parallel_start(true));
    mgr.register(hanging_start("db")).unwrap();
    mgr.register(timed("cache", &log, Duration::from_secs(1)))
        .unwrap();

    let err = mgr
        .start_all_with_timeout(Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(matches!(err, LifecycleError::Timeout(_)), "{err}");
    // By the time the call returns, the cancelled start has been settled and
    // the component that did start has been rolled back.
    assert_eq!(mgr.get_state("db"), Some(ComponentState::Error));
    assert_eq!(mgr.get_state("cache"), Some(ComponentState::Stopped));
}

// ---- per-component timeouts -------------------------------------------------

#[tokio::test(start_paused = true)]
async fn per_component_start_timeout_bounds_one_component() {
    let log = new_log();
    let mgr = ComponentManager::new();
    mgr.register(timed("fast", &log, Duration::from_secs(1)))
        .unwrap();
    mgr.register_with(
        hanging_start("slow"),
        ComponentOptions::new().start_timeout(Duration::from_secs(2)),
    )
    .unwrap();
    mgr.add_dependency("slow", "fast").unwrap();

    let began = Instant::now();
    let err = mgr.start_all().await.unwrap_err();
    assert!(
        matches!(&err, LifecycleError::Timeout(id) if id == "slow"),
        "{err}"
    );
    assert_eq!(began.elapsed(), Duration::from_secs(3));
    assert_eq!(mgr.get_state("slow"), Some(ComponentState::Error));
    assert_eq!(mgr.get_state("fast"), Some(ComponentState::Stopped));
}

#[tokio::test(start_paused = true)]
async fn the_manager_default_start_timeout_applies_unless_overridden() {
    let mgr =
        ComponentManager::with_options(ManagerOptions::new().start_timeout(Duration::from_secs(4)));
    mgr.register(hanging_start("default")).unwrap();
    mgr.register_with(
        hanging_start("own"),
        ComponentOptions::new().start_timeout(Duration::from_secs(9)),
    )
    .unwrap();

    let began = Instant::now();
    assert!(matches!(
        mgr.start("default").await,
        Err(LifecycleError::Timeout(_))
    ));
    assert_eq!(began.elapsed(), Duration::from_secs(4));

    let began = Instant::now();
    assert!(matches!(
        mgr.start("own").await,
        Err(LifecycleError::Timeout(_))
    ));
    assert_eq!(began.elapsed(), Duration::from_secs(9));
}

#[tokio::test(start_paused = true)]
async fn per_component_stop_timeout_overrides_the_sweep_bound() {
    let slow_stop = |id: &'static str| {
        Arc::new(SimpleComponent::new(
            id,
            || Box::pin(async { Ok(()) }),
            || {
                Box::pin(async {
                    tokio::time::sleep(Duration::from_secs(20)).await;
                    Ok(())
                })
            },
        ))
    };
    let mgr = ComponentManager::new();
    // `patient` is allowed 60 s by its own options; `hasty` gets the sweep's 5 s.
    mgr.register_with(
        slow_stop("patient"),
        ComponentOptions::new().stop_timeout(Duration::from_secs(60)),
    )
    .unwrap();
    mgr.register(slow_stop("hasty")).unwrap();
    mgr.start_all().await.unwrap();

    let err = mgr
        .stop_all_with_timeout(Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, LifecycleError::Timeout(id) if id == "hasty"),
        "{err}"
    );
    assert_eq!(mgr.get_state("patient"), Some(ComponentState::Stopped));
    assert_eq!(mgr.get_state("hasty"), Some(ComponentState::Error));
}

// ---- supervision ------------------------------------------------------------

#[derive(Default)]
struct Recorder {
    changes: Mutex<Vec<(String, ComponentState, ComponentState)>>,
    failures: Mutex<Vec<String>>,
    restarts: Mutex<Vec<(String, u32, Duration)>>,
}

impl LifecycleObserver for Recorder {
    fn on_state_change(&self, change: &StateChange) {
        self.changes
            .lock()
            .unwrap()
            .push((change.id.clone(), change.from, change.to));
    }

    fn on_failure(&self, id: &str, _error: &LifecycleError) {
        self.failures.lock().unwrap().push(id.to_string());
    }

    fn on_restart(&self, id: &str, restart: u32, delay: Duration, _cause: &LifecycleError) {
        self.restarts
            .lock()
            .unwrap()
            .push((id.to_string(), restart, delay));
    }
}

fn policy(max_restarts: u32) -> ComponentOptions {
    ComponentOptions::new().restart(
        RestartPolicy::new(max_restarts)
            .with_backoff(Duration::from_millis(100), Duration::from_secs(1)),
    )
}

#[tokio::test(start_paused = true)]
async fn a_failed_start_is_retried_with_exponential_backoff() {
    let mgr = ComponentManager::new();
    let recorder = Arc::new(Recorder::default());
    mgr.set_observer(recorder.clone());
    // Fails twice, then starts.
    let svc = flaky("svc", |attempt| attempt <= 2);
    mgr.register_with(svc.component.clone(), policy(5)).unwrap();

    let began = Instant::now();
    mgr.start_all().await.unwrap();

    // Backoff: 100 ms before restart 1, 200 ms before restart 2.
    assert_eq!(began.elapsed(), Duration::from_millis(300));
    assert_eq!(svc.starts(), 3);
    assert_eq!(mgr.get_state("svc"), Some(ComponentState::Running));
    assert_eq!(
        *recorder.restarts.lock().unwrap(),
        vec![
            ("svc".to_string(), 1, Duration::from_millis(100)),
            ("svc".to_string(), 2, Duration::from_millis(200)),
        ]
    );
    // The component stays `Starting` across the retries: one clean transition.
    assert_eq!(
        *recorder.changes.lock().unwrap(),
        vec![
            (
                "svc".to_string(),
                ComponentState::Unknown,
                ComponentState::Starting
            ),
            (
                "svc".to_string(),
                ComponentState::Starting,
                ComponentState::Running
            ),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn retries_stop_once_the_restart_budget_is_spent() {
    let mgr = ComponentManager::new();
    let svc = flaky("svc", |_| true);
    mgr.register_with(svc.component.clone(), policy(2)).unwrap();

    let err = mgr.start_all().await.unwrap_err();
    assert!(
        matches!(err, LifecycleError::ComponentFailure { .. }),
        "{err}"
    );
    assert_eq!(svc.starts(), 3, "one start plus two restarts");
    assert_eq!(mgr.get_state("svc"), Some(ComponentState::Error));

    // An explicit start grants a fresh budget.
    assert!(mgr.start_all().await.is_err());
    assert_eq!(svc.starts(), 6);
}

#[tokio::test(start_paused = true)]
async fn a_reported_failure_restarts_the_component() {
    let mgr = ComponentManager::new();
    let recorder = Arc::new(Recorder::default());
    mgr.set_observer(recorder.clone());
    let svc = flaky("svc", |_| false);
    mgr.register_with(svc.component.clone(), policy(3)).unwrap();
    mgr.start_all().await.unwrap();

    mgr.report_failure("svc", "lost its connection").unwrap();
    assert_eq!(mgr.get_state("svc"), Some(ComponentState::Error));
    assert_eq!(*recorder.failures.lock().unwrap(), vec!["svc".to_string()]);

    let began = Instant::now();
    tokio::time::timeout(GUARD, wait_state(&mgr, "svc", ComponentState::Running))
        .await
        .expect("the component must be restarted");
    assert_eq!(began.elapsed(), Duration::from_millis(100));
    // Restart = stop (to release resources) followed by a fresh start.
    assert_eq!(svc.stops(), 1);
    assert_eq!(svc.starts(), 2);
    assert_eq!(
        *recorder.restarts.lock().unwrap(),
        vec![("svc".to_string(), 1, Duration::from_millis(100))]
    );
}

#[tokio::test(start_paused = true)]
async fn the_restart_budget_bounds_reported_failures() {
    let mgr = ComponentManager::new();
    let svc = flaky("svc", |_| false);
    mgr.register_with(svc.component.clone(), policy(2)).unwrap();
    mgr.start_all().await.unwrap();

    for _ in 0..2 {
        mgr.report_failure("svc", "crashed").unwrap();
        tokio::time::timeout(GUARD, wait_state(&mgr, "svc", ComponentState::Running))
            .await
            .unwrap();
    }
    // The third failure exceeds the budget: the component stays down.
    mgr.report_failure("svc", "crashed").unwrap();
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(mgr.get_state("svc"), Some(ComponentState::Error));
    assert_eq!(svc.starts(), 3);
}

#[tokio::test(start_paused = true)]
async fn a_reported_failure_without_a_policy_stays_in_error() {
    let mgr = ComponentManager::new();
    let svc = flaky("svc", |_| false);
    mgr.register(svc.component.clone()).unwrap();

    // Not running yet: the report is ignored.
    mgr.report_failure("svc", "too early").unwrap();
    assert_eq!(mgr.get_state("svc"), Some(ComponentState::Unknown));
    assert!(matches!(
        mgr.report_failure("ghost", "x"),
        Err(LifecycleError::ComponentNotFound(_))
    ));

    mgr.start_all().await.unwrap();
    mgr.report_failure("svc", "crashed").unwrap();
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(mgr.get_state("svc"), Some(ComponentState::Error));
    assert_eq!(svc.starts(), 1);
}

#[tokio::test(start_paused = true)]
async fn an_explicit_stop_cancels_a_scheduled_restart() {
    let mgr = ComponentManager::new();
    let svc = flaky("svc", |_| false);
    mgr.register_with(svc.component.clone(), policy(3)).unwrap();
    mgr.start_all().await.unwrap();

    mgr.report_failure("svc", "crashed").unwrap();
    // The operator steps in before the backoff has elapsed.
    mgr.stop("svc").await.unwrap();
    assert_eq!(mgr.get_state("svc"), Some(ComponentState::Stopped));

    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(mgr.get_state("svc"), Some(ComponentState::Stopped));
    assert_eq!(svc.starts(), 1, "the scheduled restart must not run");
}

// ---- event stream and observer ---------------------------------------------

#[tokio::test(start_paused = true)]
async fn the_global_stream_reports_every_transition_in_order() {
    let log = new_log();
    let mgr = ComponentManager::new();
    let mut stream = mgr.subscribe();
    mgr.register(timed("db", &log, Duration::from_secs(2)))
        .unwrap();
    mgr.register(timed("api", &log, Duration::from_secs(3)))
        .unwrap();
    mgr.add_dependency("api", "db").unwrap();

    mgr.start_all().await.unwrap();
    mgr.stop_all().await.unwrap();

    let mut seen = Vec::new();
    loop {
        match stream.try_recv() {
            Ok(change) => seen.push(change),
            Err(TryRecvError::Empty) => break,
            Err(other) => panic!("unexpected stream error: {other}"),
        }
    }
    use ComponentState::{Running, Starting, Stopped, Stopping, Unknown};
    let summary: Vec<(&str, ComponentState, ComponentState)> =
        seen.iter().map(|c| (c.id.as_str(), c.from, c.to)).collect();
    assert_eq!(
        summary,
        vec![
            ("db", Unknown, Starting),
            ("db", Starting, Running),
            ("api", Unknown, Starting),
            ("api", Starting, Running),
            ("api", Running, Stopping),
            ("api", Stopping, Stopped),
            ("db", Running, Stopping),
            ("db", Stopping, Stopped),
        ]
    );
    // `elapsed` is the time spent in the state being left: the start duration.
    assert_eq!(seen[1].elapsed, Duration::from_secs(2));
    assert_eq!(seen[3].elapsed, Duration::from_secs(3));
}

#[tokio::test(start_paused = true)]
async fn the_observer_sees_failures_and_a_panicking_observer_is_harmless() {
    struct Grumpy(Recorder);
    impl LifecycleObserver for Grumpy {
        fn on_state_change(&self, change: &StateChange) {
            self.0.on_state_change(change);
            panic!("observer bug");
        }
        fn on_failure(&self, id: &str, error: &LifecycleError) {
            self.0.on_failure(id, error);
        }
    }

    let mgr = ComponentManager::new();
    let observer = Arc::new(Grumpy(Recorder::default()));
    mgr.set_observer(observer.clone());
    let svc = flaky("svc", |attempt| attempt == 1);
    mgr.register(svc.component.clone()).unwrap();

    assert!(mgr.start_all().await.is_err());
    mgr.start_all().await.unwrap();
    assert_eq!(mgr.get_state("svc"), Some(ComponentState::Running));

    assert_eq!(
        *observer.0.failures.lock().unwrap(),
        vec!["svc".to_string()]
    );
    assert_eq!(observer.0.changes.lock().unwrap().len(), 4);
}

// ---- shutdown handle and reload --------------------------------------------

#[tokio::test(start_paused = true)]
async fn the_shutdown_handle_stops_start_and_wait() {
    let log = new_log();
    let mgr = Arc::new(ComponentManager::new());
    mgr.register(timed("svc", &log, Duration::ZERO)).unwrap();
    let handle = mgr.shutdown_handle();

    let runner = tokio::spawn({
        let mgr = mgr.clone();
        async move { mgr.start_and_wait().await }
    });
    wait_state(&mgr, "svc", ComponentState::Running).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(!runner.is_finished());
    assert!(!handle.is_triggered());

    // Any clone works, from any task.
    let clone = handle.clone();
    let waiter = tokio::spawn(async move { clone.triggered().await });
    handle.clone().trigger();
    tokio::time::timeout(GUARD, waiter).await.unwrap().unwrap();

    tokio::time::timeout(GUARD, runner)
        .await
        .expect("start_and_wait must return after trigger()")
        .unwrap()
        .unwrap();
    assert_eq!(mgr.get_state("svc"), Some(ComponentState::Stopped));
    // The latch is cleared once the shutdown has completed, and `wait` is done.
    assert!(!handle.is_triggered());
    tokio::time::timeout(GUARD, mgr.wait()).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_shutdown_requested_during_startup_cancels_the_start() {
    let log = new_log();
    let mgr = Arc::new(ComponentManager::new());
    mgr.register(timed("db", &log, Duration::ZERO)).unwrap();
    mgr.register(timed("slow", &log, Duration::from_secs(3600)))
        .unwrap();
    mgr.add_dependency("slow", "db").unwrap();

    let runner = tokio::spawn({
        let mgr = mgr.clone();
        async move { mgr.start_and_wait().await }
    });
    wait_state(&mgr, "slow", ComponentState::Starting).await;

    let began = Instant::now();
    mgr.shutdown_handle().trigger();
    tokio::time::timeout(GUARD, runner)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    // It did not wait for the hour-long start.
    assert_eq!(began.elapsed(), Duration::ZERO);
    assert_eq!(mgr.get_state("db"), Some(ComponentState::Stopped));
    // The interrupted component was stopped too, so it can release resources.
    assert_eq!(mgr.get_state("slow"), Some(ComponentState::Stopped));
    assert_eq!(
        events(&log),
        [
            "start:db",
            "started:db",
            "start:slow",
            "stop:slow",
            "stop:db"
        ]
        .map(String::from)
    );
}

#[tokio::test(start_paused = true)]
async fn a_shutdown_requested_before_start_and_wait_skips_the_start() {
    let log = new_log();
    let mgr = ComponentManager::new();
    mgr.register(timed("svc", &log, Duration::ZERO)).unwrap();

    mgr.shutdown_handle().trigger();
    mgr.start_and_wait().await.unwrap();
    assert!(events(&log).is_empty());
    assert_eq!(mgr.get_state("svc"), Some(ComponentState::Unknown));
}

#[tokio::test(start_paused = true)]
async fn start_and_wait_returns_the_start_error_after_rolling_back() {
    let log = new_log();
    let mgr = ComponentManager::new();
    mgr.register(timed("db", &log, Duration::ZERO)).unwrap();
    let bad = flaky("api", |_| true);
    mgr.register(bad.component.clone()).unwrap();
    mgr.add_dependency("api", "db").unwrap();

    let err = tokio::time::timeout(GUARD, mgr.start_and_wait())
        .await
        .expect("a failed start must not wait for a signal")
        .unwrap_err();
    assert!(
        matches!(&err, LifecycleError::ComponentFailure { id, .. } if id == "api"),
        "{err}"
    );
    assert_eq!(mgr.get_state("db"), Some(ComponentState::Stopped));
}

#[tokio::test(start_paused = true)]
async fn trigger_reload_notifies_subscribers() {
    let mgr = ComponentManager::new();
    let mut reload = mgr.subscribe_reload();
    assert_eq!(*reload.borrow_and_update(), 0);

    mgr.trigger_reload();
    tokio::time::timeout(GUARD, reload.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(*reload.borrow_and_update(), 1);

    mgr.trigger_reload();
    mgr.trigger_reload();
    assert_eq!(*reload.borrow_and_update(), 3);
}

// ---- asynchronous health checks --------------------------------------------

#[tokio::test(start_paused = true)]
async fn async_checks_overlap_and_a_slow_one_times_out_as_down() {
    let registry = HealthRegistry::new();
    registry.register_async("db", CheckOptions::new(), || async {
        tokio::time::sleep(Duration::from_secs(3)).await;
        HealthStatus::Up
    });
    registry.register_async("cache", CheckOptions::new(), || async {
        tokio::time::sleep(Duration::from_secs(3)).await;
        HealthStatus::Degraded
    });
    registry.register_async(
        "stuck",
        CheckOptions::new().timeout(Duration::from_secs(4)),
        || async {
            std::future::pending::<()>().await;
            HealthStatus::Up
        },
    );
    registry.register("sync", || HealthStatus::Up);

    let began = Instant::now();
    let report = registry.check().await;
    // Not 3 + 3 + 4: the checks ran concurrently, bounded by the slowest timeout.
    assert_eq!(began.elapsed(), Duration::from_secs(4));
    assert_eq!(
        report.checks(),
        [
            ("db".to_string(), HealthStatus::Up),
            ("cache".to_string(), HealthStatus::Degraded),
            ("stuck".to_string(), HealthStatus::Down),
            ("sync".to_string(), HealthStatus::Up),
        ]
    );
    assert_eq!(report.overall(), HealthStatus::Down);
    assert!(!report.is_live());
    assert_eq!(report.get("cache"), Some(HealthStatus::Degraded));
    assert_eq!(report.get("nope"), None);
}

#[tokio::test(start_paused = true)]
async fn a_cached_result_is_reused_until_its_ttl_expires() {
    let calls = Arc::new(AtomicU32::new(0));
    let registry = HealthRegistry::new();
    registry.register_async(
        "db",
        CheckOptions::new().cache_for(Duration::from_secs(10)),
        {
            let calls = calls.clone();
            move || {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    HealthStatus::Up
                }
            }
        },
    );
    let sync_calls = Arc::new(AtomicU32::new(0));
    registry.register_with(
        "disk",
        CheckOptions::new().cache_for(Duration::from_secs(10)),
        {
            let calls = sync_calls.clone();
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                HealthStatus::Up
            }
        },
    );

    for _ in 0..5 {
        assert!(registry.check().await.is_ready());
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(sync_calls.load(Ordering::SeqCst), 1);

    // 10 s after the first run the result has expired and the check runs again.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(registry.check().await.is_ready());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(sync_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn probe_groups_are_evaluated_independently() {
    let registry = HealthRegistry::new();
    registry.register_with(
        "process",
        CheckOptions::new().probes(&[ProbeKind::Liveness]),
        || HealthStatus::Up,
    );
    registry.register_async(
        "db",
        CheckOptions::new().probes(&[ProbeKind::Readiness]),
        || async { HealthStatus::Down },
    );
    registry.register_async(
        "migrations",
        CheckOptions::new().probes(&[ProbeKind::Startup, ProbeKind::Readiness]),
        || async { HealthStatus::Up },
    );

    // A failed dependency fails readiness without failing liveness.
    let live = registry.check_probe(ProbeKind::Liveness).await;
    assert_eq!(live.checks(), [("process".to_string(), HealthStatus::Up)]);
    assert!(live.is_live());

    let ready = registry.check_probe(ProbeKind::Readiness).await;
    assert_eq!(ready.checks().len(), 2);
    assert!(!ready.is_ready());

    let startup = registry.check_probe(ProbeKind::Startup).await;
    assert_eq!(
        startup.checks(),
        [("migrations".to_string(), HealthStatus::Up)]
    );
    assert!(startup.is_ready());

    // The ungrouped report covers every check.
    assert_eq!(registry.check().await.checks().len(), 3);
    assert_eq!(
        registry.report_probe(ProbeKind::Liveness).checks(),
        [("process".to_string(), HealthStatus::Up)]
    );
}

#[tokio::test(start_paused = true)]
async fn the_sync_api_reports_the_last_async_result() {
    let healthy = Arc::new(Mutex::new(HealthStatus::Up));
    let registry = HealthRegistry::new();
    registry.register_async("db", CheckOptions::new(), {
        let healthy = healthy.clone();
        move || {
            let healthy = healthy.clone();
            async move { *healthy.lock().unwrap() }
        }
    });

    // Never run: unknown is reported as down rather than assumed healthy.
    assert_eq!(registry.overall(), HealthStatus::Down);
    assert!(!registry.is_ready());

    registry.check().await;
    assert_eq!(registry.overall(), HealthStatus::Up);
    assert!(registry.is_ready());

    *healthy.lock().unwrap() = HealthStatus::Degraded;
    assert_eq!(registry.overall(), HealthStatus::Up, "stale until re-run");
    registry.check().await;
    assert_eq!(registry.overall(), HealthStatus::Degraded);
    assert_eq!(
        registry.to_json(),
        r#"{"overall":"degraded","checks":{"db":"degraded"}}"#
    );
}

#[tokio::test(start_paused = true)]
async fn a_panicking_async_check_is_down() {
    let registry = HealthRegistry::new();
    registry.register_async("boom", CheckOptions::new(), || async {
        if true {
            panic!("check exploded");
        }
        HealthStatus::Up
    });
    registry.register_async("fine", CheckOptions::new(), || async { HealthStatus::Up });

    let report = registry.check().await;
    assert_eq!(report.get("boom"), Some(HealthStatus::Down));
    assert_eq!(report.get("fine"), Some(HealthStatus::Up));
}

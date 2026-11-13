//! The [`ComponentManager`] — registration, dependency ordering, supervision
//! and shutdown.

use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use ferroly::lifecycle::{
    Component, ComponentOptions, ComponentState, HealthReport, HealthStatus, LifecycleError,
    StateChange,
};

/// Default per-component stop deadline used by
/// [`ComponentManager::start_and_wait`] and by the rollback of a failed start
/// when no other stop timeout is configured.
pub const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// Capacity of the global state-change stream; a subscriber that falls further
/// behind than this sees a `Lagged` error and resumes from the oldest retained
/// event.
const EVENT_CAPACITY: usize = 256;

/// Manager-wide settings ([`ComponentManager::with_options`]).
///
/// ```
/// use std::time::Duration;
/// use ferroly::lifecycle::{ComponentManager, ManagerOptions};
///
/// let mgr = ComponentManager::with_options(
///     ManagerOptions::new()
///         .parallel_start(true)
///         .start_timeout(Duration::from_secs(20)),
/// );
/// assert!(mgr.options().parallel_start);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ManagerOptions {
    /// Start components of the same dependency level concurrently instead of
    /// one after another. Default `false`.
    pub parallel_start: bool,
    /// When a start fails, stop the components that the failing call had
    /// already started (in reverse order) before returning the error. Default
    /// `true`.
    pub rollback_on_start_failure: bool,
    /// Default bound on each component `start` attempt (a component's own
    /// [`ComponentOptions::start_timeout`] wins). Default `None` (unbounded).
    pub start_timeout: Option<Duration>,
    /// Default bound on each component `stop` (a component's own
    /// [`ComponentOptions::stop_timeout`] and the bound passed to
    /// [`ComponentManager::stop_all_with_timeout`] win). Default `None`:
    /// `stop`/`stop_all` are unbounded, while `start_and_wait` and rollback fall
    /// back to [`DEFAULT_STOP_TIMEOUT`].
    pub stop_timeout: Option<Duration>,
    /// On Unix, make [`ComponentManager::start_and_wait`] treat `SIGHUP` as a
    /// reload notification ([`ComponentManager::subscribe_reload`]) instead of
    /// leaving the signal at its default disposition. Default `false`.
    pub reload_on_sighup: bool,
}

impl Default for ManagerOptions {
    fn default() -> Self {
        Self {
            parallel_start: false,
            rollback_on_start_failure: true,
            start_timeout: None,
            stop_timeout: None,
            reload_on_sighup: false,
        }
    }
}

impl ManagerOptions {
    /// The defaults: sequential start, rollback on, no timeouts, no `SIGHUP`
    /// handling.
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts components of the same dependency level concurrently.
    pub fn parallel_start(mut self, enabled: bool) -> Self {
        self.parallel_start = enabled;
        self
    }

    /// Enables or disables the rollback of a failed start.
    pub fn rollback_on_start_failure(mut self, enabled: bool) -> Self {
        self.rollback_on_start_failure = enabled;
        self
    }

    /// Sets the default per-component start timeout.
    pub fn start_timeout(mut self, timeout: Duration) -> Self {
        self.start_timeout = Some(timeout);
        self
    }

    /// Sets the default per-component stop timeout.
    pub fn stop_timeout(mut self, timeout: Duration) -> Self {
        self.stop_timeout = Some(timeout);
        self
    }

    /// Treats `SIGHUP` as a reload notification in `start_and_wait` (Unix).
    pub fn reload_on_sighup(mut self, enabled: bool) -> Self {
        self.reload_on_sighup = enabled;
        self
    }
}

/// A hook for observing the manager, suitable for metrics and logging. Every
/// method has a no-op default.
///
/// Callbacks run synchronously on the task that caused the event, outside the
/// manager's internal lock; keep them short and non-blocking. A panic inside a
/// callback is caught and discarded.
pub trait LifecycleObserver: Send + Sync {
    /// A component changed state.
    fn on_state_change(&self, change: &StateChange) {
        let _ = change;
    }

    /// A start or stop of `id` failed (error, panic, timeout), or the component
    /// reported a failure while running.
    fn on_failure(&self, id: &str, error: &LifecycleError) {
        let _ = (id, error);
    }

    /// An automatic restart of `id` was scheduled: restart number `restart`
    /// (1-based) will begin after `delay` because of `cause`.
    fn on_restart(&self, id: &str, restart: u32, delay: Duration, cause: &LifecycleError) {
        let _ = (id, restart, delay, cause);
    }
}

/// A cloneable handle that requests a shutdown, as an alternative (or addition)
/// to OS signals. Obtained from [`ComponentManager::shutdown_handle`].
///
/// [`trigger`](Self::trigger) is latched: it makes a running (or later)
/// [`ComponentManager::start_and_wait`] stop everything and return, and wakes
/// every task awaiting [`triggered`](Self::triggered). `start_and_wait` clears
/// the latch once its shutdown has completed.
#[derive(Clone)]
pub struct ShutdownHandle {
    tx: Arc<watch::Sender<bool>>,
}

impl ShutdownHandle {
    fn new() -> Self {
        Self {
            tx: Arc::new(watch::channel(false).0),
        }
    }

    /// Requests a shutdown. Idempotent; never blocks.
    pub fn trigger(&self) {
        self.tx.send_replace(true);
    }

    /// Whether a shutdown has been requested and not yet cleared.
    pub fn is_triggered(&self) -> bool {
        *self.tx.borrow()
    }

    /// Completes once a shutdown has been requested (immediately if it already
    /// was).
    pub async fn triggered(&self) {
        let mut rx = self.tx.subscribe();
        latched(&mut rx).await;
    }

    /// Clears a previous request so the handle can be used again.
    pub fn reset(&self) {
        self.tx.send_replace(false);
    }
}

impl std::fmt::Debug for ShutdownHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShutdownHandle")
            .field("triggered", &self.is_triggered())
            .finish()
    }
}

/// Waits until a latched `watch<bool>` is `true` (or its sender is gone).
async fn latched(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|v| *v).await;
}

struct Entry {
    component: Arc<dyn Component>,
    /// The authoritative state, published to [`ComponentManager::watch`]
    /// subscribers.
    state: watch::Sender<ComponentState>,
    options: ComponentOptions,
    /// When the component entered its current state.
    since: Instant,
    /// Automatic restarts used since the last explicit start/stop.
    restarts: u32,
    /// Bumped by every explicit start/stop; lets a scheduled restart detect
    /// that someone else has acted on the component in the meantime.
    epoch: u64,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<String, Entry>,
    /// `deps[id]` = the set of ids that `id` depends on.
    deps: HashMap<String, HashSet<String>>,
    observer: Option<Arc<dyn LifecycleObserver>>,
}

/// The manager's shared state. Held behind an `Arc` so spawned tasks (parallel
/// starts, scheduled restarts) and a [`HealthRegistry`] can refer to it.
///
/// [`HealthRegistry`]: crate::lifecycle::HealthRegistry
pub(crate) struct Shared {
    inner: Mutex<Inner>,
    options: ManagerOptions,
    events: broadcast::Sender<StateChange>,
    /// Latched `true` once a `stop_all` sweep has completed.
    stopped: watch::Sender<bool>,
    shutdown: ShutdownHandle,
    /// Incremented on every reload notification.
    reload: watch::Sender<u64>,
}

/// Orchestrates a set of [`Component`]s with dependency-aware start/stop.
///
/// On start, dependencies start before dependents; on stop, the order reverses.
/// Cycles are rejected by [`add_dependency`](ComponentManager::add_dependency).
///
/// The async methods must be called on a Tokio runtime: every component
/// `start`/`stop` runs on its own spawned task so that a panic is contained.
pub struct ComponentManager {
    shared: Arc<Shared>,
}

impl Default for ComponentManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ComponentManager {
    /// Creates an empty manager with the default [`ManagerOptions`].
    pub fn new() -> Self {
        Self::with_options(ManagerOptions::default())
    }

    /// Creates an empty manager with the given options.
    pub fn with_options(options: ManagerOptions) -> Self {
        Self {
            shared: Arc::new(Shared {
                inner: Mutex::new(Inner::default()),
                options,
                events: broadcast::channel(EVENT_CAPACITY).0,
                stopped: watch::channel(false).0,
                shutdown: ShutdownHandle::new(),
                reload: watch::channel(0).0,
            }),
        }
    }

    /// The options this manager was created with.
    pub fn options(&self) -> &ManagerOptions {
        &self.shared.options
    }

    pub(crate) fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// Registers a component, initially in the [`ComponentState::Unknown`]
    /// state, with default [`ComponentOptions`].
    ///
    /// Returns [`LifecycleError::DuplicateComponent`] if a component with the
    /// same id is already registered — the existing one is left untouched.
    pub fn register(&self, component: Arc<dyn Component>) -> Result<(), LifecycleError> {
        self.register_with(component, ComponentOptions::default())
    }

    /// Registers a component with per-component timeouts and/or a restart
    /// policy. Fails like [`register`](Self::register) on a duplicate id.
    pub fn register_with(
        &self,
        component: Arc<dyn Component>,
        options: ComponentOptions,
    ) -> Result<(), LifecycleError> {
        let id = component.id().to_string();
        let mut inner = self.shared.inner.lock().unwrap();
        if inner.entries.contains_key(&id) {
            return Err(LifecycleError::DuplicateComponent(id));
        }
        inner.deps.entry(id.clone()).or_default();
        inner.entries.insert(
            id,
            Entry {
                component,
                state: watch::channel(ComponentState::Unknown).0,
                options,
                since: Instant::now(),
                restarts: 0,
                epoch: 0,
            },
        );
        Ok(())
    }

    /// Removes a component and any dependency edges referencing it.
    ///
    /// A component that is `Starting`, `Running` or `Stopping` is **not**
    /// removed: [`LifecycleError::ComponentActive`] is returned and the caller
    /// must [`stop`](Self::stop) it first. An unknown id yields
    /// [`LifecycleError::ComponentNotFound`].
    pub fn unregister(&self, id: &str) -> Result<(), LifecycleError> {
        let mut inner = self.shared.inner.lock().unwrap();
        let state = match inner.entries.get(id) {
            Some(entry) => *entry.state.borrow(),
            None => return Err(LifecycleError::ComponentNotFound(id.to_string())),
        };
        if state.is_running() || state.is_transitional() {
            return Err(LifecycleError::ComponentActive(id.to_string()));
        }
        inner.entries.remove(id);
        inner.deps.remove(id);
        for set in inner.deps.values_mut() {
            set.remove(id);
        }
        Ok(())
    }

    /// Declares that `id` depends on `depends_on` (so `depends_on` starts first).
    ///
    /// Returns [`LifecycleError::CyclicDependency`] if the edge would introduce
    /// a cycle, and [`LifecycleError::ComponentNotFound`] if either id is
    /// unregistered.
    pub fn add_dependency(&self, id: &str, depends_on: &str) -> Result<(), LifecycleError> {
        let mut inner = self.shared.inner.lock().unwrap();
        if !inner.entries.contains_key(id) {
            return Err(LifecycleError::ComponentNotFound(id.to_string()));
        }
        if !inner.entries.contains_key(depends_on) {
            return Err(LifecycleError::ComponentNotFound(depends_on.to_string()));
        }
        let inserted = inner
            .deps
            .entry(id.to_string())
            .or_default()
            .insert(depends_on.to_string());

        // Validate acyclicity; revert the edge if a cycle appears.
        if topo_order(&inner.deps).is_err() {
            if inserted {
                inner.deps.get_mut(id).unwrap().remove(depends_on);
            }
            return Err(LifecycleError::CyclicDependency(format!(
                "{id} -> {depends_on}"
            )));
        }
        Ok(())
    }

    /// Returns the current state of a component, if registered.
    pub fn get_state(&self, id: &str) -> Option<ComponentState> {
        self.shared.state_of(id)
    }

    /// Returns the ids of all registered components.
    pub fn list(&self) -> Vec<String> {
        self.shared
            .inner
            .lock()
            .unwrap()
            .entries
            .keys()
            .cloned()
            .collect()
    }

    /// Subscribes to `id`'s state transitions, if registered.
    ///
    /// The returned [`watch::Receiver`] observes the current state immediately
    /// (via [`borrow`](watch::Receiver::borrow)) and each subsequent change
    /// (await [`changed`](watch::Receiver::changed)). A `watch` channel keeps
    /// only the latest value; use [`subscribe`](Self::subscribe) to see every
    /// transition.
    pub fn watch(&self, id: &str) -> Option<watch::Receiver<ComponentState>> {
        self.shared
            .inner
            .lock()
            .unwrap()
            .entries
            .get(id)
            .map(|entry| entry.state.subscribe())
    }

    /// Subscribes to the global stream of state changes — every transition of
    /// every component, in order.
    ///
    /// The stream is a bounded [`broadcast`] channel: a receiver that falls more
    /// than 256 events behind gets
    /// [`RecvError::Lagged`](broadcast::error::RecvError::Lagged) and then
    /// resumes with the oldest retained event.
    pub fn subscribe(&self) -> broadcast::Receiver<StateChange> {
        self.shared.events.subscribe()
    }

    /// Installs a [`LifecycleObserver`] (replacing any previous one).
    pub fn set_observer(&self, observer: Arc<dyn LifecycleObserver>) {
        self.shared.inner.lock().unwrap().observer = Some(observer);
    }

    /// Starts a single component after its transitive dependencies.
    ///
    /// Returns [`LifecycleError::ComponentAlreadyStarted`] if `id` is already
    /// `Running`. If `id` (or a dependency) is being started by another caller,
    /// this call waits for that attempt and reports its outcome: `Ok(())` only
    /// if the component reached `Running`, otherwise
    /// [`LifecycleError::StartFailed`].
    ///
    /// On failure the components this call started are rolled back (stopped in
    /// reverse order) unless
    /// [`ManagerOptions::rollback_on_start_failure`] is off.
    pub async fn start(&self, id: &str) -> Result<(), LifecycleError> {
        match self.get_state(id) {
            None => return Err(LifecycleError::ComponentNotFound(id.to_string())),
            Some(ComponentState::Running) => {
                return Err(LifecycleError::ComponentAlreadyStarted(id.to_string()))
            }
            _ => {}
        }
        let closure = {
            let inner = self.shared.inner.lock().unwrap();
            dependency_closure(&inner.deps, id)
        };
        self.run_start(Some(closure), None).await
    }

    /// Starts all registered components in dependency order.
    ///
    /// If a component fails to start, the components this call had already
    /// started are stopped again in reverse order before the error is returned
    /// (see [`ManagerOptions::rollback_on_start_failure`]). The error is the
    /// start failure; if the rollback itself fails too, the result is a
    /// [`LifecycleError::Multiple`] with the start failure first.
    pub async fn start_all(&self) -> Result<(), LifecycleError> {
        self.run_start(None, None).await
    }

    /// Starts all registered components with an overall timeout.
    ///
    /// When the timeout elapses the in-flight start is cancelled — that
    /// component ends in [`ComponentState::Error`], never stuck in `Starting` —
    /// the components already started are rolled back, and
    /// [`LifecycleError::Timeout`]`("<all>")` is returned. The rollback runs
    /// after the deadline, so the call can outlast `timeout` by the time the
    /// stops take.
    pub async fn start_all_with_timeout(&self, timeout: Duration) -> Result<(), LifecycleError> {
        self.run_start(None, Some(timeout)).await
    }

    /// Stops a single component after its dependents.
    ///
    /// Best-effort: every component in the set is attempted even if one fails;
    /// the failures are returned together (a single error as itself, several as
    /// [`LifecycleError::Multiple`]).
    pub async fn stop(&self, id: &str) -> Result<(), LifecycleError> {
        if self.get_state(id).is_none() {
            return Err(LifecycleError::ComponentNotFound(id.to_string()));
        }
        // Stop dependents first: reverse of the start order that reaches `id`.
        let mut order = {
            let inner = self.shared.inner.lock().unwrap();
            let dependents = dependent_closure(&inner.deps, id);
            let full = topo_order(&inner.deps)?;
            full.into_iter()
                .filter(|n| dependents.contains(n))
                .collect::<Vec<_>>()
        };
        order.reverse();
        LifecycleError::collect(self.shared.stop_ids(order, None).await)
    }

    /// Stops all registered components in reverse dependency order.
    ///
    /// Best-effort: every component is attempted even if an earlier one fails
    /// (a failing component ends in [`ComponentState::Error`]); all failures
    /// are returned once the sweep is done — a single error as itself, several
    /// as [`LifecycleError::Multiple`]. Tasks blocked in [`wait`](Self::wait)
    /// are released whether or not the sweep succeeded.
    ///
    /// Each `stop()` is bounded only by the component's own
    /// [`ComponentOptions::stop_timeout`] or the manager's
    /// [`ManagerOptions::stop_timeout`]; with neither set a hung `stop()` blocks
    /// the sweep. Use [`stop_all_with_timeout`](Self::stop_all_with_timeout) to
    /// bound every component.
    pub async fn stop_all(&self) -> Result<(), LifecycleError> {
        self.shared.stop_all(None).await
    }

    /// Like [`stop_all`](Self::stop_all), but bounds **each** component's
    /// `stop()` by `per_component` (a component's own
    /// [`ComponentOptions::stop_timeout`] still wins), so one hung component
    /// cannot block shutdown until `SIGKILL`. A component that times out is
    /// reported as [`LifecycleError::Timeout`] and marked
    /// [`ComponentState::Error`].
    pub async fn stop_all_with_timeout(
        &self,
        per_component: Duration,
    ) -> Result<(), LifecycleError> {
        self.shared.stop_all(Some(per_component)).await
    }

    /// Blocks until a [`stop_all`](ComponentManager::stop_all) (or
    /// [`stop_all_with_timeout`](Self::stop_all_with_timeout)) sweep has
    /// completed.
    ///
    /// The completion is latched: calling `wait` after the sweep has finished
    /// returns immediately. The latch is cleared by the next
    /// [`start`](Self::start) / [`start_all`](Self::start_all).
    pub async fn wait(&self) {
        let mut rx = self.shared.stopped.subscribe();
        latched(&mut rx).await;
    }

    /// A handle that requests a shutdown without an OS signal; see
    /// [`ShutdownHandle`]. Every call returns a handle to the same latch.
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        self.shared.shutdown.clone()
    }

    /// Subscribes to reload notifications. The value is a counter that is
    /// incremented by [`trigger_reload`](Self::trigger_reload) and — when
    /// [`ManagerOptions::reload_on_sighup`] is set, on Unix, while
    /// [`start_and_wait`](Self::start_and_wait) is running — by each `SIGHUP`.
    /// Await [`changed`](watch::Receiver::changed) to react.
    pub fn subscribe_reload(&self) -> watch::Receiver<u64> {
        self.shared.reload.subscribe()
    }

    /// Emits a reload notification to every
    /// [`subscribe_reload`](Self::subscribe_reload) receiver.
    pub fn trigger_reload(&self) {
        self.shared.reload.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// Starts everything, then blocks until a shutdown is requested, then stops
    /// everything with a bounded per-component stop
    /// ([`ManagerOptions::stop_timeout`], or [`DEFAULT_STOP_TIMEOUT`] = 30 s) so
    /// a hung `stop()` cannot block shutdown until `SIGKILL`.
    ///
    /// A shutdown is requested by any of:
    ///
    /// - an OS signal — `SIGINT` or `SIGTERM` on Unix, Ctrl-C elsewhere;
    /// - [`ShutdownHandle::trigger`] on a handle from
    ///   [`shutdown_handle`](Self::shutdown_handle);
    /// - a [`stop_all`](Self::stop_all) completing from another task.
    ///
    /// Signal handlers are installed *before* the components start, so a
    /// request that arrives during startup cancels the start (the in-flight
    /// component is marked `Error`) and proceeds straight to the stop sweep. If
    /// the start fails, the started components are rolled back and the start
    /// error is returned without waiting.
    pub async fn start_and_wait(&self) -> Result<(), LifecycleError> {
        let mut signals = Signals::install(self.shared.options.reload_on_sighup)?;
        let stop_timeout = self
            .shared
            .options
            .stop_timeout
            .unwrap_or(DEFAULT_STOP_TIMEOUT);
        let mut stopped = self.shared.stopped.subscribe();

        let interrupted = tokio::select! {
            biased;
            _ = self.shared.shutdown_requested(&mut signals) => true,
            res = self.start_all() => {
                res?;
                false
            }
        };
        if !interrupted {
            tokio::select! {
                _ = self.shared.shutdown_requested(&mut signals) => {}
                _ = latched(&mut stopped) => {}
            }
        }
        let res = self.shared.stop_all(Some(stop_timeout)).await;
        self.shared.shutdown.reset();
        res
    }

    /// Reports that a **running** component has failed.
    ///
    /// The component is moved to [`ComponentState::Error`]. If it was
    /// registered with a [`RestartPolicy`](crate::lifecycle::RestartPolicy) and
    /// restarts remain, the manager restarts it in the background after the
    /// policy's backoff: `stop()` (best effort, to release resources) followed
    /// by `start()`, itself retried under the same budget. Only that component
    /// is restarted; its dependents are left as they are.
    ///
    /// A report for a component that is not `Running` is ignored. Returns
    /// [`LifecycleError::ComponentNotFound`] for an unknown id. Must be called
    /// from within a Tokio runtime when a restart policy is configured.
    pub fn report_failure(
        &self,
        id: &str,
        reason: impl Into<String>,
    ) -> Result<(), LifecycleError> {
        enum Plan {
            Ignore,
            Fail,
            Restart {
                epoch: u64,
                restart: u32,
                delay: Duration,
            },
        }
        let plan = self
            .shared
            .transition_with(id, |entry, from| {
                if from != ComponentState::Running {
                    return (None, Plan::Ignore);
                }
                let plan = match &entry.options.restart {
                    Some(policy) if entry.restarts < policy.max_restarts => {
                        entry.restarts += 1;
                        Plan::Restart {
                            epoch: entry.epoch,
                            restart: entry.restarts,
                            delay: policy.delay(entry.restarts),
                        }
                    }
                    _ => Plan::Fail,
                };
                (Some(ComponentState::Error), plan)
            })
            .ok_or_else(|| LifecycleError::ComponentNotFound(id.to_string()))?;

        if matches!(plan, Plan::Ignore) {
            return Ok(());
        }
        let reason: String = reason.into();
        let cause = LifecycleError::ComponentFailure {
            id: id.to_string(),
            source: reason.into(),
        };
        self.shared.notify(|o| o.on_failure(id, &cause));
        if let Plan::Restart {
            epoch,
            restart,
            delay,
        } = plan
        {
            self.shared
                .notify(|o| o.on_restart(id, restart, delay, &cause));
            let weak = Arc::downgrade(&self.shared);
            let id = id.to_string();
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                if let Some(shared) = weak.upgrade() {
                    shared.supervised_restart(&id, epoch).await;
                }
            });
        }
        Ok(())
    }

    /// The health of every registered component, sorted by id.
    ///
    /// Each component's status is derived from its lifecycle state, refined by
    /// [`Component::health`] while it is running:
    ///
    /// | State | Status |
    /// |---|---|
    /// | `Running` | [`Component::health`], or [`Up`](HealthStatus::Up) when it returns `None` |
    /// | `Error` | [`Down`](HealthStatus::Down) |
    /// | `Unknown`, `Starting`, `Stopping`, `Stopped` | [`Degraded`](HealthStatus::Degraded) — not serving, but not failed |
    ///
    /// So [`HealthReport::is_ready`] is `true` only when every component is
    /// `Running` and healthy, and [`HealthReport::is_live`] is `false` only
    /// when a component has failed.
    pub fn health(&self) -> HealthReport {
        HealthReport::new(self.shared.health_checks())
    }

    /// Readiness: `true` only when every registered component is `Running` and
    /// reports [`Up`](HealthStatus::Up). Shorthand for
    /// `self.health().is_ready()`.
    pub fn is_ready(&self) -> bool {
        self.health().is_ready()
    }

    /// Liveness: `false` when any component is in `Error` or reports
    /// [`Down`](HealthStatus::Down). Shorthand for `self.health().is_live()`.
    pub fn is_live(&self) -> bool {
        self.health().is_live()
    }

    /// Runs a start over `filter` (or everything), with an optional overall
    /// deadline, then rolls back on failure.
    async fn run_start(
        &self,
        filter: Option<HashSet<String>>,
        deadline: Option<Duration>,
    ) -> Result<(), LifecycleError> {
        let shared = &self.shared;
        shared.stopped.send_replace(false);

        let started = Arc::new(Mutex::new(Vec::new()));
        let (alive, mut alive_rx) = mpsc::channel::<()>(1);
        let run = StartRun {
            started: started.clone(),
            alive,
        };
        let result = match deadline {
            Some(limit) => {
                match tokio::time::timeout(limit, shared.start_set(filter, &run)).await {
                    Ok(res) => res,
                    Err(_) => Err(LifecycleError::Timeout("<all>".to_string())),
                }
            }
            None => shared.start_set(filter, &run).await,
        };
        // A cancelled parallel start leaves tasks that are being aborted; wait
        // until every one of them is gone so `started` is final before the
        // rollback reads it. (Each task holds a clone of `alive`.)
        drop(run);
        let _ = alive_rx.recv().await;

        let error = match result {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        if !shared.options.rollback_on_start_failure {
            return Err(error);
        }
        let ids: Vec<String> = started.lock().unwrap().drain(..).rev().collect();
        let limit = shared.options.stop_timeout.unwrap_or(DEFAULT_STOP_TIMEOUT);
        let rollback_errors = shared.stop_ids(ids, Some(limit)).await;
        if rollback_errors.is_empty() {
            return Err(error);
        }
        let mut all = Vec::new();
        error.flatten_into(&mut all);
        all.extend(rollback_errors);
        LifecycleError::collect(all)
    }
}

/// Bookkeeping for one start call.
struct StartRun {
    /// Ids this call brought to `Running`, in completion order.
    started: Arc<Mutex<Vec<String>>>,
    /// Cloned into every spawned start task; the receiver sees the channel
    /// close once all of them have finished or been dropped.
    alive: mpsc::Sender<()>,
}

/// Outcome of trying to take ownership of a component's start.
enum StartClaim {
    Won {
        component: Arc<dyn Component>,
        options: ComponentOptions,
    },
    AlreadyRunning,
    Busy {
        rx: watch::Receiver<ComponentState>,
        seen: ComponentState,
    },
}

/// Outcome of trying to take ownership of a component's stop.
enum StopClaim {
    Won {
        component: Arc<dyn Component>,
        options: ComponentOptions,
    },
    AlreadyStopped,
    Busy {
        rx: watch::Receiver<ComponentState>,
        seen: ComponentState,
    },
}

/// Settles a component that is mid-transition: if the owner is dropped before
/// calling [`finish`](Self::finish) — the caller timed out or was cancelled —
/// the component is moved to `Error` rather than left in `Starting`/`Stopping`.
struct TransitionGuard<'a> {
    shared: &'a Shared,
    id: &'a str,
    during: ComponentState,
    done: bool,
}

impl<'a> TransitionGuard<'a> {
    fn new(shared: &'a Shared, id: &'a str, during: ComponentState) -> Self {
        Self {
            shared,
            id,
            during,
            done: false,
        }
    }

    fn finish(mut self, to: ComponentState) {
        self.done = true;
        self.shared.settle(self.id, self.during, to);
    }
}

impl Drop for TransitionGuard<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.shared
                .settle(self.id, self.during, ComponentState::Error);
        }
    }
}

/// Aborts the wrapped task when dropped, so abandoning a start/stop also
/// cancels the component future that was running on its own task.
pub(crate) struct AbortOnDrop<T>(pub(crate) JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Shared {
    fn state_of(&self, id: &str) -> Option<ComponentState> {
        self.inner
            .lock()
            .unwrap()
            .entries
            .get(id)
            .map(|entry| *entry.state.borrow())
    }

    /// Runs `f` on `id`'s entry under the lock. If `f` returns a state that
    /// differs from the current one, the change is applied and published: to
    /// the component's `watch` channel and the global stream while still
    /// locked (so events stay ordered), and to the observer after unlocking.
    /// Returns `None` if `id` is not registered.
    fn transition_with<R>(
        &self,
        id: &str,
        f: impl FnOnce(&mut Entry, ComponentState) -> (Option<ComponentState>, R),
    ) -> Option<R> {
        let (ret, change, observer) = {
            let mut inner = self.inner.lock().unwrap();
            let observer = inner.observer.clone();
            let entry = inner.entries.get_mut(id)?;
            let from = *entry.state.borrow();
            let (to, ret) = f(entry, from);
            let change = match to {
                Some(to) if to != from => {
                    let now = Instant::now();
                    let elapsed = now.saturating_duration_since(entry.since);
                    entry.since = now;
                    entry.state.send_replace(to);
                    let change = StateChange {
                        id: id.to_string(),
                        from,
                        to,
                        elapsed,
                    };
                    let _ = self.events.send(change.clone());
                    Some(change)
                }
                _ => None,
            };
            (ret, change, observer)
        };
        if let (Some(change), Some(observer)) = (change, observer) {
            let _ = catch_unwind(AssertUnwindSafe(|| observer.on_state_change(&change)));
        }
        Some(ret)
    }

    /// Moves `id` from `during` to `to`, if it is still in `during`.
    fn settle(&self, id: &str, during: ComponentState, to: ComponentState) {
        self.transition_with(id, |_, from| ((from == during).then_some(to), ()));
    }

    /// Calls the observer (if any) outside the lock, containing a panic.
    fn notify(&self, f: impl FnOnce(&dyn LifecycleObserver)) {
        let observer = self.inner.lock().unwrap().observer.clone();
        if let Some(observer) = observer {
            let _ = catch_unwind(AssertUnwindSafe(|| f(observer.as_ref())));
        }
    }

    fn claim_start(&self, id: &str) -> Result<StartClaim, LifecycleError> {
        self.transition_with(id, |entry, from| match from {
            ComponentState::Running => (None, StartClaim::AlreadyRunning),
            ComponentState::Starting | ComponentState::Stopping => (
                None,
                StartClaim::Busy {
                    rx: entry.state.subscribe(),
                    seen: from,
                },
            ),
            _ => {
                // An explicit start: invalidate any scheduled restart and
                // grant a fresh restart budget.
                entry.epoch += 1;
                entry.restarts = 0;
                (
                    Some(ComponentState::Starting),
                    StartClaim::Won {
                        component: entry.component.clone(),
                        options: entry.options.clone(),
                    },
                )
            }
        })
        .ok_or_else(|| LifecycleError::ComponentNotFound(id.to_string()))
    }

    fn claim_stop(&self, id: &str) -> Result<StopClaim, LifecycleError> {
        self.transition_with(id, |entry, from| match from {
            ComponentState::Stopped | ComponentState::Unknown => (None, StopClaim::AlreadyStopped),
            ComponentState::Starting | ComponentState::Stopping => (
                None,
                StopClaim::Busy {
                    rx: entry.state.subscribe(),
                    seen: from,
                },
            ),
            _ => {
                entry.epoch += 1;
                entry.restarts = 0;
                (
                    Some(ComponentState::Stopping),
                    StopClaim::Won {
                        component: entry.component.clone(),
                        options: entry.options.clone(),
                    },
                )
            }
        })
        .ok_or_else(|| LifecycleError::ComponentNotFound(id.to_string()))
    }

    /// Starts one component, or waits for the start another caller owns.
    ///
    /// The state is claimed atomically (`→ Starting`), so two concurrent starts
    /// can never both run the component's `start`. The loser waits on the
    /// `watch` channel for the outcome and returns `Ok` only if the component
    /// reached `Running`.
    async fn start_one(
        &self,
        id: &str,
        started: &Mutex<Vec<String>>,
    ) -> Result<(), LifecycleError> {
        loop {
            match self.claim_start(id)? {
                StartClaim::AlreadyRunning => return Ok(()),
                StartClaim::Busy { mut rx, seen } => {
                    let settled = match rx.wait_for(|s| !s.is_transitional()).await {
                        Ok(state) => *state,
                        // Unregistered while we waited.
                        Err(_) => return Err(LifecycleError::ComponentNotFound(id.to_string())),
                    };
                    if settled == ComponentState::Running {
                        return Ok(());
                    }
                    if seen == ComponentState::Starting {
                        return Err(LifecycleError::StartFailed(id.to_string()));
                    }
                    // It was stopping and has settled: claim it ourselves.
                }
                StartClaim::Won { component, options } => {
                    let guard = TransitionGuard::new(self, id, ComponentState::Starting);
                    return match self.run_start_attempts(id, &component, &options).await {
                        Ok(()) => {
                            // Record before publishing `Running`, so the list is
                            // complete as soon as the state leaves `Starting`.
                            started.lock().unwrap().push(id.to_string());
                            guard.finish(ComponentState::Running);
                            Ok(())
                        }
                        Err(e) => {
                            guard.finish(ComponentState::Error);
                            self.notify(|o| o.on_failure(id, &e));
                            Err(e)
                        }
                    };
                }
            }
        }
    }

    /// Runs the component's `start`, retrying under its restart policy. The
    /// component stays in `Starting` for the whole sequence, backoff included.
    async fn run_start_attempts(
        &self,
        id: &str,
        component: &Arc<dyn Component>,
        options: &ComponentOptions,
    ) -> Result<(), LifecycleError> {
        let timeout = options.start_timeout.or(self.options.start_timeout);
        loop {
            let error = match run_phase(id, component.clone(), Phase::Start, timeout).await {
                Ok(()) => return Ok(()),
                Err(e) => e,
            };
            let Some(policy) = &options.restart else {
                return Err(error);
            };
            let restart = {
                let mut inner = self.inner.lock().unwrap();
                match inner.entries.get_mut(id) {
                    Some(entry) if entry.restarts < policy.max_restarts => {
                        entry.restarts += 1;
                        Some(entry.restarts)
                    }
                    _ => None,
                }
            };
            let Some(restart) = restart else {
                return Err(error);
            };
            let delay = policy.delay(restart);
            self.notify(|o| {
                o.on_failure(id, &error);
                o.on_restart(id, restart, delay, &error);
            });
            tokio::time::sleep(delay).await;
        }
    }

    /// Stops one component, or waits for the stop another caller owns.
    /// `sweep_timeout` is the bound requested by the caller's sweep; the
    /// component's own stop timeout takes precedence over it.
    async fn stop_one(
        &self,
        id: &str,
        sweep_timeout: Option<Duration>,
    ) -> Result<(), LifecycleError> {
        loop {
            match self.claim_stop(id)? {
                StopClaim::AlreadyStopped => return Ok(()),
                StopClaim::Busy { mut rx, seen } => {
                    let limit = sweep_timeout.or(self.options.stop_timeout);
                    let wait = rx.wait_for(|s| !s.is_transitional());
                    let waited = match limit {
                        Some(limit) => match tokio::time::timeout(limit, wait).await {
                            Ok(res) => res.map(|state| *state),
                            Err(_) => return Err(LifecycleError::Timeout(id.to_string())),
                        },
                        None => wait.await.map(|state| *state),
                    };
                    let settled = match waited {
                        Ok(state) => state,
                        Err(_) => return Err(LifecycleError::ComponentNotFound(id.to_string())),
                    };
                    if seen == ComponentState::Stopping {
                        return match settled {
                            ComponentState::Stopped | ComponentState::Unknown => Ok(()),
                            _ => Err(LifecycleError::StopFailed(id.to_string())),
                        };
                    }
                    // It was starting and has settled: stop whatever came of it.
                }
                StopClaim::Won { component, options } => {
                    let timeout = options
                        .stop_timeout
                        .or(sweep_timeout)
                        .or(self.options.stop_timeout);
                    let guard = TransitionGuard::new(self, id, ComponentState::Stopping);
                    return match run_phase(id, component, Phase::Stop, timeout).await {
                        Ok(()) => {
                            guard.finish(ComponentState::Stopped);
                            Ok(())
                        }
                        Err(e) => {
                            guard.finish(ComponentState::Error);
                            self.notify(|o| o.on_failure(id, &e));
                            Err(e)
                        }
                    };
                }
            }
        }
    }

    /// Stops `ids` in the given order, attempting every one and collecting the
    /// failures.
    async fn stop_ids(
        &self,
        ids: Vec<String>,
        sweep_timeout: Option<Duration>,
    ) -> Vec<LifecycleError> {
        let mut errors = Vec::new();
        for id in ids {
            match self.stop_one(&id, sweep_timeout).await {
                // Unregistered between computing the order and getting here.
                Ok(()) | Err(LifecycleError::ComponentNotFound(_)) => {}
                Err(e) => errors.push(e),
            }
        }
        errors
    }

    /// The best-effort stop sweep behind `stop_all*`. Always latches the
    /// `stopped` signal, even when components failed to stop.
    async fn stop_all(&self, sweep_timeout: Option<Duration>) -> Result<(), LifecycleError> {
        let order = { topo_order(&self.inner.lock().unwrap().deps) };
        let result = match order {
            Ok(mut order) => {
                order.reverse();
                LifecycleError::collect(self.stop_ids(order, sweep_timeout).await)
            }
            Err(e) => Err(e),
        };
        self.stopped.send_replace(true);
        result
    }

    /// Starts `filter` (or everything): sequentially in dependency order, or —
    /// with `parallel_start` — one dependency level at a time, the members of a
    /// level concurrently.
    async fn start_set(
        self: &Arc<Self>,
        filter: Option<HashSet<String>>,
        run: &StartRun,
    ) -> Result<(), LifecycleError> {
        let wanted = |id: &String| filter.as_ref().map_or(true, |f| f.contains(id));

        if !self.options.parallel_start {
            let order = { topo_order(&self.inner.lock().unwrap().deps)? };
            for id in order.into_iter().filter(wanted) {
                self.start_one(&id, &run.started).await?;
            }
            return Ok(());
        }

        let levels = { dependency_levels(&self.inner.lock().unwrap().deps)? };
        for level in levels {
            let ids: Vec<String> = level.into_iter().filter(wanted).collect();
            let tasks: Vec<AbortOnDrop<Result<(), LifecycleError>>> = ids
                .iter()
                .map(|id| {
                    let shared = self.clone();
                    let started = run.started.clone();
                    let alive = run.alive.clone();
                    let id = id.clone();
                    AbortOnDrop(tokio::spawn(async move {
                        let _alive = alive;
                        shared.start_one(&id, &started).await
                    }))
                })
                .collect();
            // Wait for the whole level even if a member fails, so nothing is
            // still starting when the rollback runs.
            let mut errors = Vec::new();
            for (id, mut task) in ids.iter().zip(tasks) {
                match (&mut task.0).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => errors.push(e),
                    Err(join) => errors.push(LifecycleError::failure(id.clone(), join)),
                }
            }
            LifecycleError::collect(errors)?;
        }
        Ok(())
    }

    /// The background half of [`ComponentManager::report_failure`]: stop (best
    /// effort) then start, unless someone acted on the component since the
    /// failure was reported.
    async fn supervised_restart(&self, id: &str, epoch: u64) {
        let claimed = self
            .transition_with(id, |entry, from| {
                if from == ComponentState::Error && entry.epoch == epoch {
                    (
                        Some(ComponentState::Stopping),
                        Some((entry.component.clone(), entry.options.clone())),
                    )
                } else {
                    (None, None)
                }
            })
            .flatten();
        let Some((component, options)) = claimed else {
            return;
        };

        let stop_timeout = options
            .stop_timeout
            .or(self.options.stop_timeout)
            .unwrap_or(DEFAULT_STOP_TIMEOUT);
        let guard = TransitionGuard::new(self, id, ComponentState::Stopping);
        // Best effort: the component already failed, so a failing stop must not
        // prevent the restart.
        let _ = run_phase(id, component.clone(), Phase::Stop, Some(stop_timeout)).await;
        guard.finish(ComponentState::Starting);

        let guard = TransitionGuard::new(self, id, ComponentState::Starting);
        match self.run_start_attempts(id, &component, &options).await {
            Ok(()) => guard.finish(ComponentState::Running),
            Err(e) => {
                guard.finish(ComponentState::Error);
                self.notify(|o| o.on_failure(id, &e));
            }
        }
    }

    /// Per-component health, sorted by id. Component callbacks run outside the
    /// lock; a panicking `health()` counts as `Down`.
    pub(crate) fn health_checks(&self) -> Vec<(String, HealthStatus)> {
        let mut snapshot: Vec<(String, Arc<dyn Component>, ComponentState)> = {
            let inner = self.inner.lock().unwrap();
            inner
                .entries
                .iter()
                .map(|(id, entry)| (id.clone(), entry.component.clone(), *entry.state.borrow()))
                .collect()
        };
        snapshot.sort_by(|a, b| a.0.cmp(&b.0));
        snapshot
            .into_iter()
            .map(|(id, component, state)| {
                let status = if state.is_running() {
                    catch_unwind(AssertUnwindSafe(|| component.health()))
                        .map_or(HealthStatus::Down, |h| h.unwrap_or(HealthStatus::Up))
                } else {
                    HealthStatus::from(state)
                };
                (id, status)
            })
            .collect()
    }

    /// Completes when a shutdown is requested by signal or by the handle;
    /// reload signals are forwarded to the reload channel along the way.
    async fn shutdown_requested(&self, signals: &mut Signals) {
        loop {
            tokio::select! {
                _ = self.shutdown.triggered() => return,
                event = signals.next() => match event {
                    SignalEvent::Shutdown => return,
                    SignalEvent::Reload => {
                        self.reload.send_modify(|n| *n = n.wrapping_add(1));
                    }
                },
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Start,
    Stop,
}

/// Runs a component's `start` or `stop` on its own task, bounded by `timeout`.
///
/// The task contains a panic (reported as `ComponentPanicked`) and is aborted
/// if this future is dropped or the timeout elapses.
async fn run_phase(
    id: &str,
    component: Arc<dyn Component>,
    phase: Phase,
    timeout: Option<Duration>,
) -> Result<(), LifecycleError> {
    let mut task = AbortOnDrop(tokio::spawn(async move {
        match phase {
            Phase::Start => component.start().await,
            Phase::Stop => component.stop().await,
        }
    }));
    let joined = match timeout {
        Some(limit) => match tokio::time::timeout(limit, &mut task.0).await {
            Ok(joined) => joined,
            Err(_) => return Err(LifecycleError::Timeout(id.to_string())),
        },
        None => (&mut task.0).await,
    };
    match joined {
        Ok(result) => result,
        Err(join) if join.is_panic() => Err(LifecycleError::ComponentPanicked {
            id: id.to_string(),
            message: panic_message(join.into_panic()),
        }),
        // Cancelled from outside (the runtime is shutting down).
        Err(join) => Err(LifecycleError::failure(id, join)),
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

enum SignalEvent {
    Shutdown,
    /// Only produced on Unix (`SIGHUP`).
    #[cfg_attr(not(unix), allow(dead_code))]
    Reload,
}

/// The OS signals `start_and_wait` listens to.
struct Signals {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(unix)]
    hangup: Option<tokio::signal::unix::Signal>,
}

impl Signals {
    #[cfg(unix)]
    fn install(reload_on_sighup: bool) -> Result<Self, LifecycleError> {
        use tokio::signal::unix::{signal, SignalKind};
        let failed = |e| LifecycleError::failure("<signals>", e);
        Ok(Self {
            interrupt: signal(SignalKind::interrupt()).map_err(failed)?,
            terminate: signal(SignalKind::terminate()).map_err(failed)?,
            hangup: if reload_on_sighup {
                Some(signal(SignalKind::hangup()).map_err(failed)?)
            } else {
                None
            },
        })
    }

    #[cfg(not(unix))]
    fn install(_reload_on_sighup: bool) -> Result<Self, LifecycleError> {
        Ok(Self {})
    }

    #[cfg(unix)]
    async fn next(&mut self) -> SignalEvent {
        let hangup = async {
            match self.hangup.as_mut() {
                Some(signal) => {
                    signal.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = self.interrupt.recv() => SignalEvent::Shutdown,
            _ = self.terminate.recv() => SignalEvent::Shutdown,
            _ = hangup => SignalEvent::Reload,
        }
    }

    #[cfg(not(unix))]
    async fn next(&mut self) -> SignalEvent {
        match tokio::signal::ctrl_c().await {
            Ok(()) => SignalEvent::Shutdown,
            // The handler could not be installed: never report a shutdown that
            // was not requested.
            Err(_) => std::future::pending().await,
        }
    }
}

/// Kahn's algorithm producing a dependencies-first ordering, or a cycle error.
fn topo_order(deps: &HashMap<String, HashSet<String>>) -> Result<Vec<String>, LifecycleError> {
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();

    for (id, id_deps) in deps {
        in_degree.entry(id.as_str()).or_insert(0);
        for d in id_deps {
            *in_degree.entry(id.as_str()).or_insert(0) += 1;
            dependents.entry(d.as_str()).or_default().push(id.as_str());
            in_degree.entry(d.as_str()).or_insert(0);
        }
    }

    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(&n, _)| n)
        .collect();
    // Deterministic ordering aids reproducible tests.
    let mut sorted_seed: Vec<&str> = queue.drain(..).collect();
    sorted_seed.sort_unstable();
    queue.extend(sorted_seed);

    let mut order = Vec::new();
    while let Some(n) = queue.pop_front() {
        order.push(n.to_string());
        if let Some(deps_of) = dependents.get(n) {
            let mut ready: Vec<&str> = Vec::new();
            for &m in deps_of {
                let e = in_degree.get_mut(m).unwrap();
                *e -= 1;
                if *e == 0 {
                    ready.push(m);
                }
            }
            ready.sort_unstable();
            queue.extend(ready);
        }
    }

    if order.len() != in_degree.len() {
        let mut unresolved: Vec<String> = in_degree
            .iter()
            .filter(|(_, &d)| d > 0)
            .map(|(&n, _)| n.to_string())
            .collect();
        unresolved.sort_unstable();
        return Err(LifecycleError::CyclicDependency(unresolved.join(", ")));
    }
    Ok(order)
}

/// Groups components by dependency level: level 0 has no dependencies, level
/// `n` depends only on lower levels (and on at least one member of level
/// `n-1`). Members of a level are independent of each other, so they can start
/// concurrently. Each level is sorted by id.
fn dependency_levels(
    deps: &HashMap<String, HashSet<String>>,
) -> Result<Vec<Vec<String>>, LifecycleError> {
    let order = topo_order(deps)?;
    let mut level_of: HashMap<&str, usize> = HashMap::new();
    let mut levels: Vec<Vec<String>> = Vec::new();
    for id in &order {
        let level = deps
            .get(id)
            .into_iter()
            .flatten()
            .filter_map(|d| level_of.get(d.as_str()))
            .max()
            .map_or(0, |deepest| deepest + 1);
        level_of.insert(id.as_str(), level);
        if levels.len() <= level {
            levels.resize_with(level + 1, Vec::new);
        }
        levels[level].push(id.clone());
    }
    for level in &mut levels {
        level.sort_unstable();
    }
    Ok(levels)
}

/// All transitive dependencies of `id`, including `id` itself.
fn dependency_closure(deps: &HashMap<String, HashSet<String>>, id: &str) -> HashSet<String> {
    let mut seen = HashSet::new();
    let mut stack = vec![id.to_string()];
    while let Some(n) = stack.pop() {
        if !seen.insert(n.clone()) {
            continue;
        }
        if let Some(ds) = deps.get(&n) {
            for d in ds {
                stack.push(d.clone());
            }
        }
    }
    seen
}

/// `id` plus everything that transitively depends on `id`.
fn dependent_closure(deps: &HashMap<String, HashSet<String>>, id: &str) -> HashSet<String> {
    let mut seen = HashSet::new();
    let mut stack = vec![id.to_string()];
    while let Some(n) = stack.pop() {
        if !seen.insert(n.clone()) {
            continue;
        }
        for (other, ds) in deps {
            if ds.contains(&n) {
                stack.push(other.clone());
            }
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(edges: &[(&str, &[&str])]) -> HashMap<String, HashSet<String>> {
        edges
            .iter()
            .map(|(id, ds)| (id.to_string(), ds.iter().map(|d| d.to_string()).collect()))
            .collect()
    }

    #[test]
    fn levels_group_independent_components() {
        let deps = graph(&[
            ("db", &[]),
            ("cache", &[]),
            ("api", &["db", "cache"]),
            ("worker", &["db"]),
            ("gateway", &["api"]),
        ]);
        let levels = dependency_levels(&deps).unwrap();
        assert_eq!(
            levels,
            vec![
                vec!["cache".to_string(), "db".to_string()],
                vec!["api".to_string(), "worker".to_string()],
                vec!["gateway".to_string()],
            ]
        );
    }

    #[test]
    fn levels_reject_a_cycle() {
        let deps = graph(&[("a", &["b"]), ("b", &["a"])]);
        assert!(matches!(
            dependency_levels(&deps),
            Err(LifecycleError::CyclicDependency(_))
        ));
    }
}

# ferroly::lifecycle

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `lifecycle` (enables `tokio`) — module `ferroly::lifecycle`.

## Overview

`lifecycle` orchestrates the startup and shutdown of a service's components in
dependency order. You describe your service
as a set of `Component`s, declare which depends on which, and let a
`ComponentManager` start them so that dependencies come up before their dependents
and go down in the reverse order. It adds graceful shutdown (OS signals or a
programmatic trigger), optional supervision, and health/readiness probes that are
tied to component state.

Key design choices in the Rust port:

- **Object-safe async without `async-trait`.** `Component::start`/`stop` return a
  hand-rolled `BoxFuture` so the manager can hold `Arc<dyn Component>` with no
  proc-macro dependency.
- **No stuck states.** A start or stop that fails, panics, times out or is cancelled
  always settles in `ComponentState::Error`. A component is never left in
  `Starting`/`Stopping`, and a second caller waits for the real outcome.
- **Channels for observation.** Each component has a `tokio::sync::watch` channel for
  its current state, and the manager has one `broadcast` stream of every transition.
  A callback-style `LifecycleObserver` is available for metrics.
- **Dependency graph validated with Kahn's algorithm.** Ordering is a topological
  sort, and every `add_dependency` is checked for cycles before it is accepted.
- **Health follows state.** `ComponentManager::health()` derives readiness and
  liveness from component state; `HealthRegistry` adds free-standing checks,
  including asynchronous ones with a timeout and a result cache.

## Enabling

```toml
[dependencies]
ferroly = { version = "0.4", features = ["lifecycle"] }
tokio = { version = "1", features = ["full"] }
```

The `lifecycle` feature enables `tokio`; the manager's async methods must run on a
Tokio runtime (every component `start`/`stop` runs on its own spawned task), and
signal handling uses `tokio::signal`.

## Quick start

```rust
use std::sync::Arc;
use ferroly::lifecycle::{ComponentManager, LifecycleError, SimpleComponent};

#[tokio::main]
async fn main() -> Result<(), LifecycleError> {
    let mgr = ComponentManager::new();

    mgr.register(Arc::new(SimpleComponent::new(
        "db",
        || Box::pin(async { Ok(()) }),
        || Box::pin(async { Ok(()) }),
    )))?;
    mgr.register(Arc::new(SimpleComponent::new(
        "api",
        || Box::pin(async { Ok(()) }),
        || Box::pin(async { Ok(()) }),
    )))?;

    mgr.add_dependency("api", "db")?; // db starts before api

    mgr.start_all().await?;
    assert!(mgr.is_ready());          // every component is Running
    mgr.stop_all().await?;            // api stops before db
    Ok(())
}
```

## API reference

### The `Component` trait

```rust
pub trait Component: Send + Sync {
    fn id(&self) -> &str;
    fn start(&self) -> BoxFuture<'_, Result<(), LifecycleError>>;
    fn stop(&self)  -> BoxFuture<'_, Result<(), LifecycleError>>;

    // Optional; the default derives health from the lifecycle state.
    fn health(&self) -> Option<HealthStatus> { None }
}
```

- `id` — a stable, unique identifier used everywhere the manager refers to the
  component.
- `start` — called *after* the component's dependencies are running.
- `stop` — called *before* the component's dependencies are stopped. It is also
  called on a component in `Error` (so it can release whatever a failed start left
  behind), so it should tolerate being called on a component that is not fully up.
- `health` — the component's self-reported health **while it is running**. `None`
  (the default) means "nothing to add": a `Running` component is `Up`. Return
  `Some(Degraded)` or `Some(Down)` for a component that is running but impaired. It is
  only consulted in the `Running` state, must be fast, and must not block. See
  [Health and readiness](#health-and-readiness-probes).

State tracking and change notification are the manager's job, not the component's; an
implementation only performs the actual start/stop work.

**How the manager runs `start` and `stop`.** Each call runs on its own spawned task.
That has two consequences worth knowing:

- A **panic** inside `start`/`stop` is contained: the component goes to `Error` and
  the caller gets `LifecycleError::ComponentPanicked { id, message }`.
- If the caller gives up — a timeout elapses, or the future is dropped — the task is
  **aborted at its next `.await`**. Keep `start`/`stop` cancel-safe at their await
  points, and do not rely on task-local state from the calling task.

### `BoxFuture`

```rust
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
```

The manual desugaring of an `async fn` return type that keeps `Component`
object-safe. The idiomatic implementation wraps an `async move { ... }` block in
`Box::pin`.

### `SimpleComponent`

A ready-made `Component` backed by start/stop closures, for cases that do not need a
dedicated struct.

```rust
SimpleComponent::new(
    id: impl Into<String>,
    start: impl Fn() -> BoxFuture<'static, Result<(), LifecycleError>> + Send + Sync + 'static,
    stop:  impl Fn() -> BoxFuture<'static, Result<(), LifecycleError>> + Send + Sync + 'static,
) -> SimpleComponent

// Optional self-reported health (see `Component::health`).
SimpleComponent::with_health(self, impl Fn() -> HealthStatus + Send + Sync + 'static) -> SimpleComponent
```

### `ComponentManager`

| Method | Description |
|---|---|
| `ComponentManager::new() -> Self` | Create an empty manager with default options (also `Default`). |
| `ComponentManager::with_options(ManagerOptions) -> Self` | Create a manager with explicit [options](#manageroptions). |
| `options(&self) -> &ManagerOptions` | The options the manager was created with. |
| `register(&self, component: Arc<dyn Component>) -> Result<(), LifecycleError>` | Register a component; initial state `Unknown`. A duplicate id is `DuplicateComponent` (the existing component is left untouched). |
| `register_with(&self, component, ComponentOptions) -> Result<(), LifecycleError>` | Register with per-component [timeouts and restart policy](#componentoptions-and-restartpolicy). |
| `unregister(&self, id: &str) -> Result<(), LifecycleError>` | Remove a component and any dependency edges referencing it. A `Starting`/`Running`/`Stopping` component is **not** removed (`ComponentActive`): stop it first. An unknown id is `ComponentNotFound`. |
| `add_dependency(&self, id, depends_on) -> Result<(), LifecycleError>` | Declare `id` depends on `depends_on`; topo-sorted and cycle-checked. |
| `get_state(&self, id: &str) -> Option<ComponentState>` | Current state, if registered. |
| `list(&self) -> Vec<String>` | Ids of all registered components. |
| `watch(&self, id: &str) -> Option<watch::Receiver<ComponentState>>` | Subscribe to one component's state (latest value only). |
| `subscribe(&self) -> broadcast::Receiver<StateChange>` | Subscribe to **every** transition of every component, in order. |
| `set_observer(&self, Arc<dyn LifecycleObserver>)` | Install a callback observer (metrics, logging). |
| `start(&self, id: &str) -> Result<(), LifecycleError>` (async) | Start one component after its transitive dependencies. |
| `start_all(&self) -> Result<(), LifecycleError>` (async) | Start everything in dependency order; roll back on failure. |
| `start_all_with_timeout(&self, timeout: Duration) -> Result<(), LifecycleError>` (async) | `start_all` bounded by an overall timeout. |
| `stop(&self, id: &str) -> Result<(), LifecycleError>` (async) | Stop one component after its dependents (best-effort). |
| `stop_all(&self) -> Result<(), LifecycleError>` (async) | Stop everything in reverse dependency order (best-effort). |
| `stop_all_with_timeout(&self, per_component: Duration) -> Result<(), LifecycleError>` (async) | `stop_all`, bounding **each** component's `stop()`. |
| `wait(&self)` (async) | Block until a `stop_all` sweep has completed. Latched: returns immediately if it already has. |
| `start_and_wait(&self) -> Result<(), LifecycleError>` (async) | Start all, block until a shutdown is requested, then stop all with a bounded per-component timeout. |
| `shutdown_handle(&self) -> ShutdownHandle` | A cloneable handle whose `trigger()` requests a shutdown. |
| `subscribe_reload(&self) -> watch::Receiver<u64>` | Subscribe to reload notifications (a counter). |
| `trigger_reload(&self)` | Emit a reload notification. |
| `report_failure(&self, id, reason) -> Result<(), LifecycleError>` | Report that a running component has failed; restarts it if it has a restart policy. |
| `health(&self) -> HealthReport` | Per-component health derived from state. |
| `is_ready(&self) -> bool` | `true` only when every component is `Running` and healthy. |
| `is_live(&self) -> bool` | `false` when a component is in `Error` or reports `Down`. |

There is also one associated constant:

| Item | Description |
|---|---|
| `DEFAULT_STOP_TIMEOUT: Duration` | 30 seconds. The per-component stop bound used by `start_and_wait` and by the rollback of a failed start when `ManagerOptions::stop_timeout` is not set. |

### `ManagerOptions`

Manager-wide settings, built fluently (the struct is `#[non_exhaustive]`):

```rust
use std::time::Duration;
use ferroly::lifecycle::{ComponentManager, ManagerOptions};

let mgr = ComponentManager::with_options(
    ManagerOptions::new()
        .parallel_start(true)                    // default: false
        .rollback_on_start_failure(true)         // default: true
        .start_timeout(Duration::from_secs(20))  // default: none
        .stop_timeout(Duration::from_secs(10))   // default: none
        .reload_on_sighup(true),                 // default: false
);
```

| Field | Default | Meaning |
|---|---|---|
| `parallel_start` | `false` | Start the components of one dependency level concurrently. See [Parallel start](#parallel-start). |
| `rollback_on_start_failure` | `true` | When a start fails, stop what the failing call had started. See [Rollback](#rollback-of-a-failed-start). |
| `start_timeout` | `None` | Default bound on each component `start` attempt. |
| `stop_timeout` | `None` | Default bound on each component `stop`. With `None`, `stop`/`stop_all` are unbounded, while `start_and_wait` and rollback fall back to `DEFAULT_STOP_TIMEOUT`. |
| `reload_on_sighup` | `false` | On Unix, treat `SIGHUP` as a reload notification in `start_and_wait`. |

### `ComponentOptions` and `RestartPolicy`

Per-component settings, supplied at registration (also `#[non_exhaustive]`):

```rust
use std::time::Duration;
use ferroly::lifecycle::{ComponentOptions, RestartPolicy};

let opts = ComponentOptions::new()
    .start_timeout(Duration::from_secs(10))
    .stop_timeout(Duration::from_secs(5))
    .restart(RestartPolicy::new(3)); // up to 3 automatic restarts
// mgr.register_with(component, opts)?;
```

| Field | Meaning |
|---|---|
| `start_timeout: Option<Duration>` | Bounds each `start` attempt of this component. Overrides `ManagerOptions::start_timeout`. |
| `stop_timeout: Option<Duration>` | Bounds each `stop` of this component. Overrides both `ManagerOptions::stop_timeout` and the bound passed to `stop_all_with_timeout`. |
| `restart: Option<RestartPolicy>` | Automatic restarts; `None` (default) disables supervision. |

**Timeout precedence.** For a start: the component's own `start_timeout`, else the
manager's. For a stop: the component's own `stop_timeout`, else the bound of the
sweep (`stop_all_with_timeout(d)`), else the manager's. A component that exceeds its
bound is cancelled, marked `Error`, and reported as `LifecycleError::Timeout(id)`.

```rust
pub struct RestartPolicy {
    pub max_restarts: u32,          // give up after this many automatic restarts
    pub initial_backoff: Duration,  // delay before the first restart
    pub max_backoff: Duration,      // cap on the doubling delay
}
```

`RestartPolicy::new(max_restarts)` uses a 100 ms initial backoff doubling up to 30 s;
`.with_backoff(initial, max)` changes both. `delay(n)` is
`initial_backoff * 2^(n-1)` capped at `max_backoff`. See
[Supervision](#supervision-automatic-restarts).

### `ComponentState`

```rust
pub enum ComponentState {
    Unknown,   // never started; initial state after registration
    Error,     // a start or stop failed, panicked, timed out or was cancelled,
               // or the component reported a failure while running
    Stopped,   // cleanly stopped
    Stopping,  // in the middle of stopping
    Running,   // running normally
    Starting,  // in the middle of starting
}
```

`ComponentState` is `Copy` and exposes `is_running()`, `is_transitional()`
(`Starting` or `Stopping`) and `as_str()` (the lowercase name, handy as a metrics
label). A typical happy path is `Unknown → Starting → Running → Stopping → Stopped`;
anything that goes wrong lands in `Error`.

### `StateChange`

```rust
pub struct StateChange {
    pub id: String,
    pub from: ComponentState,
    pub to: ComponentState,
    pub elapsed: Duration, // time spent in `from`
}
```

One transition, as delivered by `subscribe()` and `LifecycleObserver`. `elapsed` is
the time the component spent in the state it left, so a `Starting → Running` change
carries the start duration and `Stopping → Stopped` the stop duration.

### `LifecycleError`

```rust
#[non_exhaustive]
pub enum LifecycleError {
    ComponentNotFound(String),
    ComponentAlreadyStarted(String),
    DuplicateComponent(String),      // register: the id is taken
    ComponentActive(String),         // unregister: stop it first
    CyclicDependency(String),
    Timeout(String),                 // a component id, or "<all>"
    StartFailed(String),             // a start owned by another caller did not reach Running
    StopFailed(String),              // a stop owned by another caller did not reach Stopped
    ComponentPanicked { id: String, message: String },
    ComponentFailure { id: String, source: Box<dyn Error + Send + Sync> },
    Multiple(LifecycleErrors),       // several failures from one sweep
}
```

Implements `std::error::Error`. Helpers:

- `LifecycleError::failure(id, source)` wraps an arbitrary error as a
  `ComponentFailure`.
- `errors(&self) -> &[LifecycleError]` returns the members of a `Multiple`, or a
  one-element slice for any other error — so you can always iterate.
- `LifecycleErrors` (the payload of `Multiple`) offers `as_slice()`, `len()`,
  `is_empty()`, `into_vec()` and iteration by reference, and displays as
  `N errors: first; second; …`.

A sweep that produces exactly one error returns that error itself, not a one-element
`Multiple`.

## Dependency ordering in depth

Dependencies are declared with `add_dependency(id, depends_on)`, meaning
`depends_on` must be *running before* `id` starts. The manager keeps, for each
component, the set of ids it depends on.

- **Topological sort (Kahn's algorithm).** `start_all` computes a dependencies-first
  ordering and starts components in that order; `stop_all` reverses it. The sort is
  deterministic (ready nodes are visited in sorted order) for reproducible startup.
- **Cycle detection.** `add_dependency` tentatively inserts the edge, re-runs the
  topological sort, and if a cycle appears it *reverts* the edge and returns
  `LifecycleError::CyclicDependency`. Both ids must already be registered, or
  `ComponentNotFound` is returned.
- **Targeted start/stop.** `start(id)` starts only `id` and its transitive
  dependency closure, in order. `stop(id)` stops `id` plus everything that
  transitively depends on it, dependents first.
- **Idempotent transitions.** Starting a component that is already `Running` is a
  no-op during dependency starts; stopping one already `Stopped`/`Unknown` is a
  no-op. Calling `start(id)` directly on an already-`Running` component returns
  `ComponentAlreadyStarted`.

### Concurrent and cancelled starts

The manager claims a component's start atomically (`→ Starting`), so its `start()`
runs at most once at a time.

- **A second caller waits.** If a component is already `Starting` when another
  `start`/`start_all` reaches it, that caller waits on the component's `watch`
  channel for the outcome. It returns `Ok(())` **only if the component reached
  `Running`**; otherwise it gets `LifecycleError::StartFailed(id)` (the underlying
  error went to the caller that owned the attempt) and does not start any dependents.
- **A cancelled start is settled.** If the owner of a start goes away — the
  `start_all_with_timeout` deadline elapses, or the future is dropped — the component
  is moved to `Error` at once and its `start()` task is aborted. A later `start`
  runs `start()` again and reports its real result; it never reports an immediate
  `Ok` for a component that did not start.
- **Stops behave the same way.** A second `stop` waits for the first; a stop that
  reaches a `Starting` component waits for the start to settle, then stops it.

### Rollback of a failed start

When a start fails, the components that **the failing call** had brought to `Running`
are stopped again, in reverse order, before the error is returned. This applies to
`start_all`, `start_all_with_timeout`, `start_and_wait` and `start(id)`.

- Components that were already running before the call are left alone.
- The component whose `start()` failed is left in `Error` and is **not** stopped —
  its `start()` is expected to clean up after itself. (`stop_all` does call `stop()`
  on `Error` components.)
- Each rollback stop is bounded (`ManagerOptions::stop_timeout`, else
  `DEFAULT_STOP_TIMEOUT`).
- The returned error is the start failure. If the rollback fails as well, the result
  is `LifecycleError::Multiple` with the start failure first.
- With `start_all_with_timeout`, the rollback runs *after* the deadline, so the call
  can outlast `timeout` by the time the stops take.
- If you drop the start future yourself, no rollback can run: the in-flight component
  is marked `Error` and the rest stay as they were. Call `stop_all` to clean up.

Opt out with `ManagerOptions::new().rollback_on_start_failure(false)`; started
components then stay `Running` after a failure, as in earlier versions.

### Parallel start

By default components start one after another. With
`ManagerOptions::new().parallel_start(true)` the manager groups components into
**dependency levels** — level 0 has no dependencies, level *n* depends only on lower
levels — and starts the members of a level concurrently, moving on once the whole
level is up.

```rust
// db and cache start together; api starts once both are running.
let mgr = ComponentManager::with_options(ManagerOptions::new().parallel_start(true));
```

If a member of a level fails, the manager still waits for its siblings to finish
starting (so nothing is mid-start), then rolls back; several failures in one level
are returned as `LifecycleError::Multiple`. Stopping is always sequential, in reverse
dependency order.

## Observing state

### One component: `watch`

`watch(id)` returns a `tokio::sync::watch::Receiver<ComponentState>` (or `None` if
the id is unknown). The receiver observes the current state immediately via
`borrow()` and each subsequent change via `changed().await`. A `watch` channel keeps
only the **latest** value, so a slow reader may skip intermediate states.

```rust
if let Some(mut rx) = mgr.watch("api") {
    // React to transitions until the component is running.
    while *rx.borrow_and_update() != ferroly::lifecycle::ComponentState::Running {
        rx.changed().await.unwrap();
    }
    println!("api is up");
}
```

### Every component: `subscribe`

`subscribe()` returns a `tokio::sync::broadcast::Receiver<StateChange>` carrying every
transition of every component, in the order they happened.

```rust
let mut events = mgr.subscribe();
tokio::spawn(async move {
    while let Ok(change) = events.recv().await {
        println!(
            "{}: {} -> {} after {:?}",
            change.id,
            change.from.as_str(),
            change.to.as_str(),
            change.elapsed,
        );
    }
});
```

The stream is bounded: a receiver that falls more than 256 events behind gets
`RecvError::Lagged` and then resumes with the oldest retained event. Subscribe before
starting if you need the first transitions.

### Metrics: `LifecycleObserver`

```rust
pub trait LifecycleObserver: Send + Sync {
    fn on_state_change(&self, change: &StateChange) {}
    fn on_failure(&self, id: &str, error: &LifecycleError) {}
    fn on_restart(&self, id: &str, restart: u32, delay: Duration, cause: &LifecycleError) {}
}
```

Install one with `set_observer`. Every method has a no-op default.

- `on_state_change` — every transition; use `change.elapsed` for start/stop duration
  histograms and `change.to.as_str()` as a label.
- `on_failure` — a start or stop failed (error, panic, timeout), or a failure was
  reported for a running component.
- `on_restart` — an automatic restart was scheduled: restart number `restart`
  (1-based) begins after `delay`.

Callbacks run synchronously on the task that caused the event, outside the manager's
lock; keep them short and non-blocking. A panic inside a callback is caught and
discarded. `lifecycle` does not depend on `ferroly::metrics`; bridge to it (or to
any other sink) inside your observer.

## Supervision (automatic restarts)

A component registered with a `RestartPolicy` is restarted automatically in two
situations:

1. **Its start fails** (error, panic or per-component timeout). The start is retried
   after the policy's backoff while restarts remain. The component stays `Starting`
   for the whole sequence — backoff included — so other callers keep waiting for the
   final outcome. When the budget is spent, the last error is returned and the
   component is `Error`.
2. **It reports a failure while running.** Call
   `mgr.report_failure(id, reason)`: the component moves to `Error` and, after the
   backoff, the manager calls `stop()` (best effort, to release resources) and then
   `start()` in the background.

```rust
use std::sync::Arc;
use std::time::Duration;
use ferroly::lifecycle::{ComponentManager, ComponentOptions, RestartPolicy};

# fn consumer() -> Arc<dyn ferroly::lifecycle::Component> { unimplemented!() }
# fn ex() -> Result<(), ferroly::lifecycle::LifecycleError> {
let mgr = Arc::new(ComponentManager::new());
mgr.register_with(
    consumer(),
    ComponentOptions::new().restart(
        RestartPolicy::new(5).with_backoff(Duration::from_millis(200), Duration::from_secs(10)),
    ),
)?;

// Elsewhere — e.g. in the component's own background task, via a clone of `mgr`:
mgr.report_failure("consumer", "connection reset by peer")?;
# Ok(())
# }
```

Rules worth knowing:

- **One budget.** `max_restarts` covers both situations, counted since the last
  *explicit* `start`/`stop` of the component through the manager. An explicit start
  grants a fresh budget.
- **An explicit action wins.** If you `stop` (or `start`) the component while a
  restart is waiting out its backoff, the scheduled restart is abandoned.
- **Only that component is restarted.** Its dependents are left as they are.
- `report_failure` is ignored for a component that is not `Running`, returns
  `ComponentNotFound` for an unknown id, and — when a restart policy is configured —
  must be called from within a Tokio runtime.
- Without a policy, `report_failure` just moves the component to `Error`, which is
  enough to make the manager [not ready and not live](#component-health).

There is no health polling: the manager does not call `Component::health` on a timer
and restart on `Down`. Detect the failure in your component and call
`report_failure`.

## Graceful shutdown

`start_and_wait` starts every component, then blocks until a shutdown is requested,
then stops everything with a bounded per-component stop
(`ManagerOptions::stop_timeout`, else `DEFAULT_STOP_TIMEOUT` = 30 s each).

A shutdown is requested by any of:

- an **OS signal** — `SIGINT` (Ctrl-C) or `SIGTERM` on Unix; Ctrl-C elsewhere;
- **`ShutdownHandle::trigger()`** on a handle from `shutdown_handle()`;
- a **`stop_all`** completing from another task.

```rust
#[tokio::main]
async fn main() -> Result<(), ferroly::lifecycle::LifecycleError> {
    let mgr = build_manager();
    // Runs the service until SIGINT/SIGTERM (or a trigger), then shuts down in order.
    mgr.start_and_wait().await
}
```

Details:

- Signal handlers are installed **before** the components start. A request that
  arrives during startup cancels the start (the in-flight component is marked
  `Error`) and goes straight to the stop sweep, which also calls `stop()` on the
  interrupted component.
- If the start fails, the started components are rolled back and the start error is
  returned without waiting for a signal.
- If the signal handlers cannot be installed, `start_and_wait` returns a
  `ComponentFailure` for the pseudo-id `<signals>` before starting anything.

### `ShutdownHandle`

```rust
let handle = mgr.shutdown_handle();   // Clone + Send + Sync

// e.g. from an admin endpoint, a test, or a parent supervisor:
handle.trigger();
```

| Method | Description |
|---|---|
| `trigger(&self)` | Request a shutdown. Idempotent, never blocks. |
| `is_triggered(&self) -> bool` | Whether a request is pending. |
| `triggered(&self)` (async) | Completes once a shutdown has been requested (immediately if it already was) — for your own tasks that should wind down. |
| `reset(&self)` | Clear a pending request. |

The request is latched: triggering *before* `start_and_wait` is called makes that
call skip the start and return after the (no-op) stop sweep. `start_and_wait` clears
the latch when its shutdown has completed. `trigger()` on its own does not stop
anything — it is `start_and_wait` (or your own code awaiting `triggered()`) that
reacts to it.

### `wait`

`stop_all` and `stop_all_with_timeout` latch a "stopped" signal when the sweep is
done — whether or not it reported errors. `wait()` returns once that signal is set,
so it works both for a task that is already waiting and for one that calls `wait()`
*after* the sweep finished (it returns immediately). The next `start`/`start_all`
clears the latch.

### Best-effort stop

`stop_all` attempts **every** component in reverse dependency order. A component
whose `stop()` fails (or panics) is marked `Error` and the sweep continues; the
failures are returned together when the sweep is done.

```rust
if let Err(e) = mgr.stop_all().await {
    for failure in e.errors() {
        eprintln!("shutdown problem: {failure}");
    }
}
```

`stop_all` bounds each `stop()` only by the component's own `stop_timeout` or the
manager's; with neither set, a hung `stop()` blocks the sweep.
`stop_all_with_timeout(d)` bounds every component by `d` (a component's own
`stop_timeout` still wins), so one hung component cannot stall shutdown until
`SIGKILL`: it is reported as `LifecycleError::Timeout(id)` and marked `Error`.

```rust
use std::time::Duration;
use ferroly::lifecycle::{ComponentManager, DEFAULT_STOP_TIMEOUT};

async fn shutdown(mgr: &ComponentManager) -> Result<(), ferroly::lifecycle::LifecycleError> {
    // Give each component at most 5 seconds to stop; keep going past a laggard.
    mgr.stop_all_with_timeout(Duration::from_secs(5)).await?;
    // Or reuse the default that `start_and_wait` applies:
    let _ = DEFAULT_STOP_TIMEOUT; // Duration::from_secs(30)
    Ok(())
}
```

`stop(id)` is best-effort in the same way across `id` and its dependents.

### Reload notifications

`subscribe_reload()` returns a `watch::Receiver<u64>` whose value is a counter that
goes up by one for each reload notification. Notifications come from:

- `trigger_reload()` — on any platform;
- `SIGHUP` — on Unix, while `start_and_wait` is running, **if**
  `ManagerOptions::reload_on_sighup(true)` was set. Without that option the signal
  keeps its default disposition.

```rust
let mut reload = mgr.subscribe_reload();
tokio::spawn(async move {
    while reload.changed().await.is_ok() {
        // re-read configuration, rotate logs, ...
    }
});
```

The manager only notifies; what "reload" means is up to the subscribers. It does not
restart components.

## Health and readiness probes

Health has two sources that combine:

- **Component state** — `ComponentManager::health()` reports every registered
  component.
- **Named checks** — `HealthRegistry` holds free-standing checks (synchronous or
  asynchronous) that any subsystem can register without implementing `Component`.

`HealthRegistry::register_components(&mgr)` adds the manager's components to a
registry's reports, so one registry can back your probe endpoints.

### `HealthStatus`

```rust
pub enum HealthStatus { Up, Degraded, Down }
```

`as_str(self) -> &'static str` yields the lowercase wire name (`"up"`, `"degraded"`,
`"down"`); `worst(self, other)` returns the worse of two statuses.
`HealthStatus::from(ComponentState)` gives the health implied by a state alone.

### `HealthReport`

A point-in-time list of `(name, status)` pairs, returned by `ComponentManager::health`
and by the registry's asynchronous API.

| Method | Description |
|---|---|
| `checks(&self) -> &[(String, HealthStatus)]` | The individual results. |
| `get(&self, name) -> Option<HealthStatus>` | One result by name. |
| `overall(&self) -> HealthStatus` | `Down` if any is down, else `Degraded` if any is degraded, else `Up` (also `Up` when empty). |
| `is_ready(&self) -> bool` | Every check is `Up` — the **readiness** (and startup) signal. |
| `is_live(&self) -> bool` | No check is `Down` — the **liveness** signal. |
| `to_json(&self) -> String` | `{ "overall": "...", "checks": { name: "..." } }`, names JSON-escaped. |
| `into_checks(self)`, `HealthReport::new(vec)` | Convert to and from the raw pairs. |

### Component health

`ComponentManager::health()` reports one entry per component, sorted by id:

| Component state | Status |
|---|---|
| `Running` | `Component::health()`, or `Up` when it returns `None` |
| `Error` | `Down` |
| `Unknown`, `Starting`, `Stopping`, `Stopped` | `Degraded` — not serving, but not failed |

So:

- **Readiness** (`mgr.is_ready()`) is `true` only when **every** component is
  `Running` and reports `Up`. A service is not ready while anything is still
  starting, has been stopped, or has failed.
- **Liveness** (`mgr.is_live()`) is `false` only when a component is in `Error` or
  reports `Down`. A component that is merely starting or stopped does not fail
  liveness, so an orchestrator will not restart a process that is still booting.

```rust
use ferroly::lifecycle::HealthStatus;

let report = mgr.health();
if !report.is_ready() {
    for (id, status) in report.checks() {
        if *status != HealthStatus::Up {
            eprintln!("{id} is {}", status.as_str());
        }
    }
}
```

A component refines its own entry by overriding `Component::health` (or
`SimpleComponent::with_health`):

```rust
use ferroly::lifecycle::{BoxFuture, Component, HealthStatus, LifecycleError};

struct Pool { /* ... */ }

impl Component for Pool {
    fn id(&self) -> &str { "pool" }
    fn start(&self) -> BoxFuture<'_, Result<(), LifecycleError>> { Box::pin(async { Ok(()) }) }
    fn stop(&self) -> BoxFuture<'_, Result<(), LifecycleError>> { Box::pin(async { Ok(()) }) }

    fn health(&self) -> Option<HealthStatus> {
        // Running, but with no usable connections: impaired rather than up.
        Some(if self.idle_connections() > 0 { HealthStatus::Up } else { HealthStatus::Degraded })
    }
}
# impl Pool { fn idle_connections(&self) -> usize { 1 } }
```

### `HealthRegistry`

A cheaply-cloneable registry of named checks.

| Method | Description |
|---|---|
| `HealthRegistry::new() -> Self` | Empty registry (also `Default`/`Clone`). |
| `register(&self, name, check)` | Register a **synchronous** check (`Fn() -> HealthStatus`) in every probe group. Must be fast and non-blocking. |
| `register_with(&self, name, CheckOptions, check)` | Synchronous check with probe groups and/or result caching. |
| `register_async(&self, name, CheckOptions, check)` | Register an **asynchronous** check (`Fn() -> impl Future<Output = HealthStatus>`), bounded by a timeout — the place for a database ping. |
| `register_components(&self, &ComponentManager)` | Add the manager's components (one entry per id) to every report. Held weakly. |
| `check(&self) -> HealthReport` (async) | Run every check; asynchronous ones concurrently. |
| `check_probe(&self, ProbeKind) -> HealthReport` (async) | Run the checks of one probe group. |
| `report(&self) -> Vec<(String, HealthStatus)>` | Synchronous report (see below). |
| `report_probe(&self, ProbeKind) -> HealthReport` | Synchronous report for one probe group. |
| `overall(&self) -> HealthStatus` | Synchronous aggregate. |
| `is_ready(&self) -> bool` | Synchronous readiness: every check is `Up`. |
| `to_json(&self) -> String` | Synchronous JSON report; each check runs exactly once. |

#### `CheckOptions` and probe groups

```rust
pub enum ProbeKind { Liveness, Readiness, Startup }
```

Every check belongs to one or more probe groups, mirroring the three Kubernetes
probes. `CheckOptions::new()` puts a check in **all three**, with a
`DEFAULT_CHECK_TIMEOUT` (5 s) and no caching:

```rust
use std::time::Duration;
use ferroly::lifecycle::{CheckOptions, ProbeKind};

let opts = CheckOptions::new()
    .probes(&[ProbeKind::Readiness])       // readiness only
    .timeout(Duration::from_secs(2))       // async checks: cancel and count as Down
    .cache_for(Duration::from_secs(10));   // reuse the result for 10 s
```

| Builder | Meaning |
|---|---|
| `probes(&[ProbeKind])` | The groups the check belongs to. |
| `timeout(Duration)` | Bound on one evaluation of an **asynchronous** check; on expiry the check is cancelled and counts as `Down`. Does not apply to synchronous checks, which cannot be interrupted. |
| `cache_for(Duration)` | Reuse a result until it is this old, so a probe polled every second does not hit the dependency every second. |
| `applies_to(ProbeKind) -> bool` | Whether the check is in a group. |

Splitting groups is how you keep a failed dependency from restarting the process:
put dependency checks in `Readiness` only, and keep `Liveness` to "the process is
alive".

#### Asynchronous checks

```rust
use std::time::Duration;
use ferroly::lifecycle::{CheckOptions, HealthRegistry, HealthStatus, ProbeKind};

# async fn ping_database() -> bool { true }
# async fn ex() {
let registry = HealthRegistry::new();

// Liveness: cheap, synchronous, always answers.
registry.register_with(
    "process",
    CheckOptions::new().probes(&[ProbeKind::Liveness]),
    || HealthStatus::Up,
);

// Readiness: a real round-trip, bounded and cached.
registry.register_async(
    "db",
    CheckOptions::new()
        .probes(&[ProbeKind::Readiness])
        .timeout(Duration::from_secs(2))
        .cache_for(Duration::from_secs(5)),
    || async {
        if ping_database().await { HealthStatus::Up } else { HealthStatus::Down }
    },
);

let live = registry.check_probe(ProbeKind::Liveness).await.is_live();
let ready = registry.check_probe(ProbeKind::Readiness).await.is_ready();
# let _ = (live, ready);
# }
```

`check`/`check_probe` launch every asynchronous check at once, each on its own task,
so the total time is bounded by the slowest timeout rather than the sum. A check that
**times out or panics counts as `Down`**. Cancelling the `check()` future cancels the
checks still running.

#### The synchronous API and asynchronous checks

`report`, `report_probe`, `overall`, `is_ready` and `to_json` cannot await. They run
synchronous checks inline (a panicking check counts as `Down`) and, for each
asynchronous check, report its **most recent result** from a `check`/`check_probe`
run — or `Down` if it has never run. If you register asynchronous checks, serve your
probes from the asynchronous API.

Each synchronous call snapshots the checks under the lock and runs them **without**
holding it, so a slow check cannot block registration or a concurrent report.
`to_json` takes a single snapshot: every check runs exactly once and the aggregate is
derived from that same snapshot. Check names are JSON-escaped. A typical body:

```json
{"overall":"degraded","checks":{"db":"up","cache":"degraded"}}
```

#### Wiring component state into a registry

```rust
use ferroly::lifecycle::{ComponentManager, HealthRegistry, ProbeKind};

# async fn ex(mgr: &ComponentManager) {
let registry = HealthRegistry::new();
registry.register_components(mgr);

// Not ready until every component is Running; not live once one is in Error.
let ready = registry.check_probe(ProbeKind::Readiness).await.is_ready();
# let _ = ready;
# }
```

Component entries are named by component id and belong to every probe group. The
registry keeps only a weak reference to the manager, so a registry held by a
component (an HTTP server exposing it, say) does not create a reference cycle; the
entries disappear when the manager is dropped.

The [rest](rest.md) module's `health_endpoints` wires a `HealthRegistry` up to
`/health` and `/ready`.

## Worked example: a supervised service with probes

```rust
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use ferroly::lifecycle::{
    CheckOptions, ComponentManager, ComponentOptions, HealthRegistry, HealthStatus,
    LifecycleError, ManagerOptions, ProbeKind, RestartPolicy, SimpleComponent,
};

#[tokio::main]
async fn main() -> Result<(), LifecycleError> {
    let mgr = Arc::new(ComponentManager::with_options(
        ManagerOptions::new()
            .parallel_start(true)
            .stop_timeout(Duration::from_secs(10)),
    ));

    // A connection pool that reports its own health while running.
    let connected = Arc::new(AtomicBool::new(false));
    let (on_start, on_stop, probe) = (connected.clone(), connected.clone(), connected.clone());
    mgr.register_with(
        Arc::new(
            SimpleComponent::new(
                "db",
                move || {
                    let flag = on_start.clone();
                    Box::pin(async move {
                        // ... open the connection pool ...
                        flag.store(true, Ordering::Relaxed);
                        Ok(())
                    })
                },
                move || {
                    let flag = on_stop.clone();
                    Box::pin(async move {
                        flag.store(false, Ordering::Relaxed);
                        Ok(())
                    })
                },
            )
            .with_health(move || {
                if probe.load(Ordering::Relaxed) { HealthStatus::Up } else { HealthStatus::Degraded }
            }),
        ),
        // Retry a failed connect up to 5 times, and give each attempt 15 s.
        ComponentOptions::new()
            .start_timeout(Duration::from_secs(15))
            .restart(RestartPolicy::new(5)),
    )?;

    mgr.register(Arc::new(SimpleComponent::new(
        "api",
        || Box::pin(async { Ok(()) }),
        || Box::pin(async { Ok(()) }),
    )))?;
    mgr.add_dependency("api", "db")?; // db must be running before api

    // Probes: component state plus one extra readiness check.
    let health = HealthRegistry::new();
    health.register_components(&mgr);
    health.register_async(
        "downstream",
        CheckOptions::new()
            .probes(&[ProbeKind::Readiness])
            .cache_for(Duration::from_secs(5)),
        || async { HealthStatus::Up },
    );

    // Log every transition.
    let mut events = mgr.subscribe();
    tokio::spawn(async move {
        while let Ok(change) = events.recv().await {
            println!("{} -> {}", change.id, change.to.as_str());
        }
    });

    // Runs until SIGINT/SIGTERM, a `shutdown_handle().trigger()`, or a stop_all
    // from elsewhere; then stops api, then db.
    mgr.start_and_wait().await
}
```

## Error handling

- `register` returns `DuplicateComponent`; `unregister` returns `ComponentActive` or
  `ComponentNotFound`.
- `add_dependency` returns `CyclicDependency` (edge reverted) or `ComponentNotFound`
  before any component runs.
- A failed `start()`/`stop()` puts that component in `Error` and yields the
  component's own `LifecycleError` (commonly a `ComponentFailure` built via
  `LifecycleError::failure`), a `ComponentPanicked`, or a `Timeout(id)`.
- `start`/`start_all` stop at the first failure and [roll back](#rollback-of-a-failed-start).
  A caller that waited on another caller's failed start gets `StartFailed(id)`.
- `start_all_with_timeout` returns `LifecycleError::Timeout("<all>")` if the whole
  start sequence exceeds the deadline.
- `stop`, `stop_all` and `stop_all_with_timeout` are best-effort: they attempt every
  component and return all failures — one as itself, several as `Multiple`.

## Limitations

- **Stop is sequential.** Parallel start is available; components always stop one
  after another.
- **No health-driven supervision.** Restarts are triggered by a failed start or an
  explicit `report_failure`, not by polling `Component::health`. A restart does not
  cascade to dependents.
- **Cancellation is cooperative.** A timed-out or cancelled `start`/`stop` is aborted
  at its next `.await`; a component that blocks the thread without awaiting cannot be
  interrupted.
- **No rollback when you drop the future.** Rollback needs to await the stops; if the
  caller's own future is dropped mid-start, only the in-flight component is settled
  (as `Error`).
- **Synchronous health checks cannot be timed out**; use `register_async` for
  anything that does I/O.
- **Signals.** `SIGINT`/`SIGTERM` (and optionally `SIGHUP`) on Unix, Ctrl-C
  elsewhere. Other signals are not handled.

## See also

- [rest](rest.md) — its server implements `Component`, and `health_endpoints`
  exposes a `HealthRegistry` at `/health` + `/ready`.
- [messaging](messaging.md) — `LocalProvider` is a `Component` whose `stop` drains
  buffered messages.
- [config](config.md) — load the configuration that shapes the components you
  register.

---
**Related:** [rest](rest.md), [messaging](messaging.md), [config](config.md).

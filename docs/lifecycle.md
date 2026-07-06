# ferroly::lifecycle

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `lifecycle` (enables `tokio`) — module `ferroly::lifecycle`.

## Overview

`lifecycle` orchestrates the startup and shutdown of a service's components in
dependency order. You describe your service
as a set of `Component`s, declare which depends on which, and let a
`ComponentManager` start them so that dependencies come up before their dependents
and go down in the reverse order. It adds graceful, signal-driven shutdown and an
independent set of health/readiness probes.

Key design choices in the Rust port:

- **Object-safe async without `async-trait`.** `Component::start`/`stop` return a
  hand-rolled `BoxFuture` so the manager can hold `Arc<dyn Component>` with no
  proc-macro dependency.
- **`watch` channels instead of observer callbacks.** State changes are published on
  a `tokio::sync::watch` channel per component; subscribers `await` transitions
  idiomatically rather than registering callbacks.
- **Dependency graph validated with Kahn's algorithm.** Ordering is a topological
  sort, and every `add_dependency` is checked for cycles before it is accepted.
- **Health probes decoupled from components.** `HealthRegistry` lets any subsystem
  register a check without implementing the `Component` trait.

## Enabling

```toml
[dependencies]
ferroly = { version = "0.1", features = ["lifecycle"] }
tokio = { version = "1", features = ["full"] }
```

The `lifecycle` feature enables `tokio`; the manager's async methods run on a Tokio
runtime, and signal handling uses `tokio::signal`.

## Quick start

```rust
use std::sync::Arc;
use ferroly::lifecycle::{ComponentManager, SimpleComponent};

#[tokio::main]
async fn main() {
    let mgr = ComponentManager::new();

    mgr.register(Arc::new(SimpleComponent::new(
        "db",
        || Box::pin(async { Ok(()) }),
        || Box::pin(async { Ok(()) }),
    )));
    mgr.register(Arc::new(SimpleComponent::new(
        "api",
        || Box::pin(async { Ok(()) }),
        || Box::pin(async { Ok(()) }),
    )));

    mgr.add_dependency("api", "db").unwrap(); // db starts before api

    mgr.start_all().await.unwrap();
    mgr.stop_all().await.unwrap();            // api stops before db
}
```

## API reference

### The `Component` trait

```rust
pub trait Component: Send + Sync {
    fn id(&self) -> &str;
    fn start(&self) -> BoxFuture<'_, Result<(), LifecycleError>>;
    fn stop(&self)  -> BoxFuture<'_, Result<(), LifecycleError>>;
}
```

- `id` — a stable, unique identifier used everywhere the manager refers to the
  component.
- `start` — called *after* the component's dependencies are running.
- `stop` — called *before* the component's dependencies are stopped.

State tracking and change notification are the manager's job, not the component's; an
implementation only performs the actual start/stop work.

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
```

### `ComponentManager`

| Method | Description |
|---|---|
| `ComponentManager::new() -> Self` | Create an empty manager (also `Default`). |
| `register(&self, component: Arc<dyn Component>)` | Register a component; initial state `Unknown`. |
| `unregister(&self, id: &str)` | Remove a component and any dependency edges referencing it. |
| `add_dependency(&self, id, depends_on) -> Result<(), LifecycleError>` | Declare `id` depends on `depends_on`; topo-sorted and cycle-checked. |
| `get_state(&self, id: &str) -> Option<ComponentState>` | Current state, if registered. |
| `list(&self) -> Vec<String>` | Ids of all registered components. |
| `watch(&self, id: &str) -> Option<watch::Receiver<ComponentState>>` | Subscribe to a component's state transitions. |
| `start(&self, id: &str) -> Result<(), LifecycleError>` (async) | Start one component after its transitive dependencies. |
| `start_all(&self) -> Result<(), LifecycleError>` (async) | Start everything in dependency order. |
| `start_all_with_timeout(&self, timeout: Duration) -> Result<(), LifecycleError>` (async) | `start_all` bounded by an overall timeout. |
| `stop(&self, id: &str) -> Result<(), LifecycleError>` (async) | Stop one component after its dependents. |
| `stop_all(&self) -> Result<(), LifecycleError>` (async) | Stop everything in reverse dependency order. |
| `stop_all_with_timeout(&self, per_component: Duration) -> Result<(), LifecycleError>` (async) | Best-effort stop of everything, bounding **each** component's `stop()`; a slow/failing one is marked `Error` and the sweep continues. |
| `wait(&self) (async)` | Block until `stop_all`/`stop_all_with_timeout` completes. |
| `start_and_wait(&self) -> Result<(), LifecycleError>` (async) | Start all, block until a signal (or external `stop_all`), then stop all with a bounded per-component timeout. |

There is also one associated constant:

| Item | Description |
|---|---|
| `DEFAULT_STOP_TIMEOUT: Duration` | The 30-second per-component stop deadline that `start_and_wait` passes to `stop_all_with_timeout`. |

### `ComponentState`

```rust
pub enum ComponentState {
    Unknown,   // never started; initial state after registration
    Error,     // a start or stop attempt failed
    Stopped,   // cleanly stopped
    Stopping,  // in the middle of stopping
    Running,   // running normally
    Starting,  // in the middle of starting
}
```

`ComponentState` is `Copy` and exposes `is_running(self) -> bool`. A typical happy
path is `Unknown → Starting → Running → Stopping → Stopped`; a failed start/stop
lands in `Error`.

### `LifecycleError`

```rust
pub enum LifecycleError {
    ComponentNotFound(String),
    ComponentAlreadyStarted(String),
    CyclicDependency(String),
    Timeout(String),
    ComponentFailure { id: String, source: Box<dyn Error + Send + Sync> },
}
```

Implements `std::error::Error`. The helper
`LifecycleError::failure(id, source)` wraps an arbitrary error as a
`ComponentFailure`.

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
- **Idempotent transitions.** Starting a component already `Running`/`Starting` is a
  no-op during dependency starts; stopping one already `Stopped`/`Unknown` is a
  no-op. Calling `start(id)` directly on an already-`Running` component returns
  `ComponentAlreadyStarted`.

On failure, `start_one`/`stop_one` transition the offending component to `Error` and
propagate the `LifecycleError`, halting the batch.

## Observing state with `watch`

`watch(id)` returns a `tokio::sync::watch::Receiver<ComponentState>` (or `None` if
the id is unknown). The receiver observes the current state immediately via
`borrow()` and each subsequent change via `changed().await`. The manager only
notifies subscribers when the state actually changes.

```rust
if let Some(mut rx) = mgr.watch("api") {
    // React to transitions until the component is running.
    while *rx.borrow_and_update() != ferroly::lifecycle::ComponentState::Running {
        rx.changed().await.unwrap();
    }
    println!("api is up");
}
```

This channel-based model is used in place of observer callbacks.

## Graceful shutdown

`start_and_wait` starts every component, then blocks until either an OS signal or an
external `stop_all`, then stops everything with a bounded per-component stop
(`stop_all_with_timeout(DEFAULT_STOP_TIMEOUT)`, 30 s each):

- On **Unix**, it waits on both `SIGINT` (Ctrl-C) and `SIGTERM`.
- On **non-Unix**, it waits on Ctrl-C only.

```rust
#[tokio::main]
async fn main() -> Result<(), ferroly::lifecycle::LifecycleError> {
    let mgr = build_manager();
    // Runs the service until SIGINT/SIGTERM, then shuts down in order.
    mgr.start_and_wait().await
}
```

`stop_all` (and `stop_all_with_timeout`) additionally notifies any tasks blocked in
`wait()`, so a separate task can `mgr.wait().await` to learn when shutdown has
completed.

### Bounding a hung `stop()` with `stop_all_with_timeout`

`stop_all` waits for every component's `stop()` to finish, so one component that
hangs blocks shutdown indefinitely. `stop_all_with_timeout` instead bounds **each**
component's `stop()` by the given `Duration`: a component that times out or errors
is marked `ComponentState::Error` and the sweep moves on, so a single hung component
cannot stall the whole shutdown. The sweep still runs in reverse dependency order,
and the first error encountered (if any) is returned once every component has been
attempted.

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

This is exactly what `start_and_wait` runs after it observes a signal, using
`DEFAULT_STOP_TIMEOUT` as the per-component bound.

## Health and readiness probes

`HealthRegistry` is a small, cheaply-cloneable registry of named checks, independent
of the `Component` trait so any subsystem can contribute one.

### `HealthStatus`

```rust
pub enum HealthStatus { Up, Degraded, Down }
```

`as_str(self) -> &'static str` yields the lowercase wire name (`"up"`, `"degraded"`,
`"down"`).

### `HealthRegistry`

| Method | Description |
|---|---|
| `HealthRegistry::new() -> Self` | Empty registry (also `Default`/`Clone`). |
| `register<F: Fn() -> HealthStatus + Send + Sync + 'static>(&self, name, check: F)` | Register a named check. Checks should be fast and non-blocking. |
| `report(&self) -> Vec<(String, HealthStatus)>` | Run every check, returning `(name, status)` pairs. |
| `overall(&self) -> HealthStatus` | Aggregate: `Down` if any is down, else `Degraded` if any is degraded, else `Up` (also `Up` when empty). |
| `is_ready(&self) -> bool` | `true` only when *every* check is `Up` — the readiness signal. |
| `to_json(&self) -> String` | Renders `{ "overall": "...", "checks": { name: "..." } }`; runs each check exactly once and JSON-escapes names. |

Checks are stored as `Arc<dyn Fn() -> HealthStatus>` and are re-run on every
`report`/`overall`/`is_ready`, so a check can reflect live state (e.g. reading an
`AtomicBool` or pinging a pool). Each call snapshots the checks under the lock and
runs them **without** holding it, so a slow check can't block registration or a
concurrent report (there is no per-check timeout, so checks must still be
non-blocking).

`to_json` takes a single snapshot: every check runs **exactly once** — the aggregate
`overall` is derived from that same snapshot rather than a second pass — so a
side-effecting check is not double-invoked. Check names are JSON-escaped, keeping the
output valid even for names containing quotes or control characters. A typical body:

```json
{"overall":"degraded","checks":{"db":"up","cache":"degraded"}}
```

The [rest](rest.md) module's `health_endpoints` wires a `HealthRegistry` up to
`/health` (using `overall`) and `/ready` (using `is_ready`), typically serving
`to_json()` as the body.

## Worked example: components with dependencies plus a health check

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use ferroly::lifecycle::{
    ComponentManager, SimpleComponent, HealthRegistry, HealthStatus,
};

#[tokio::main]
async fn main() -> Result<(), ferroly::lifecycle::LifecycleError> {
    let mgr = ComponentManager::new();
    let health = HealthRegistry::new();

    // Shared readiness flag the db flips once connected.
    let db_up = Arc::new(AtomicBool::new(false));

    let flag = db_up.clone();
    mgr.register(Arc::new(SimpleComponent::new(
        "db",
        {
            let flag = flag.clone();
            move || {
                let flag = flag.clone();
                Box::pin(async move {
                    // ... open the connection pool ...
                    flag.store(true, Ordering::Relaxed);
                    Ok(())
                })
            }
        },
        move || {
            let flag = flag.clone();
            Box::pin(async move {
                flag.store(false, Ordering::Relaxed);
                Ok(())
            })
        },
    )));

    mgr.register(Arc::new(SimpleComponent::new(
        "api",
        || Box::pin(async { Ok(()) }),
        || Box::pin(async { Ok(()) }),
    )));

    mgr.add_dependency("api", "db")?; // db must be running before api

    // A health check that reflects the shared flag.
    let probe = db_up.clone();
    health.register("db", move || {
        if probe.load(Ordering::Relaxed) { HealthStatus::Up } else { HealthStatus::Down }
    });

    mgr.start_all().await?;           // db, then api
    assert!(health.is_ready());       // db came up, so we're ready

    mgr.stop_all().await?;            // api, then db
    Ok(())
}
```

## Error handling

- `add_dependency` returns `CyclicDependency` (edge reverted) or `ComponentNotFound`
  before any component runs.
- `start`/`start_all`/`stop`/`stop_all` propagate a component's own
  `LifecycleError` (commonly a `ComponentFailure` built via
  `LifecycleError::failure`), transitioning that component to `Error`.
- `start_all_with_timeout` returns `LifecycleError::Timeout("<all>")` if the whole
  start sequence exceeds the deadline.
- `stop_all_with_timeout` is best-effort: it does **not** halt on the first failure.
  Each component that times out (yielding `LifecycleError::Timeout(id)`) or errors is
  set to `Error`, the sweep continues, and the first such error is returned after
  every component has been attempted.

## Limitations

- **Start/stop are sequential**, not parallel — independent components at the same
  dependency level are still started one after another.
- **No automatic restart or supervision** — a component that transitions to `Error`
  stays there until you act.
- **Health checks are synchronous** (`Fn() -> HealthStatus`); do not block inside a
  check, since it runs on the caller's thread during `report`.

## See also

- [rest](rest.md) — its server implements `Component`, and `health_endpoints`
  exposes a `HealthRegistry` at `/health` + `/ready`.
- [messaging](messaging.md) — long-running consumers modeled as `Component`s.
- [config](config.md) — load the configuration that shapes the components you
  register.

---
**Related:** [rest](rest.md), [messaging](messaging.md), [config](config.md).

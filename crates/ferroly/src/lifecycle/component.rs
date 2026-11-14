//! The [`Component`] trait, per-component options and the [`SimpleComponent`]
//! helper.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use ferroly::lifecycle::{HealthStatus, LifecycleError};

/// A boxed, `Send` future — the manual desugaring of `async fn` in traits used
/// so that [`Component`] stays object-safe (`Arc<dyn Component>`) without the
/// `async-trait` dependency.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A managed component with a start/stop lifecycle.
///
/// Implemented by anything the [`ComponentManager`](crate::lifecycle::ComponentManager) can
/// orchestrate — servers, connection pools, background workers. State tracking
/// and change notifications are handled by the manager, not the component.
///
/// `start`/`stop` return a [`BoxFuture`]; the idiomatic implementation wraps an
/// `async move { ... }` block in `Box::pin`.
///
/// The manager runs `start` and `stop` on their own spawned task so that a
/// panic is contained and reported as
/// [`LifecycleError::ComponentPanicked`]. If the caller gives up (a timeout
/// elapses or the future is dropped) the task is aborted at its next `.await`,
/// so `start`/`stop` should be cancel-safe at their await points.
pub trait Component: Send + Sync {
    /// A stable, unique identifier for this component.
    fn id(&self) -> &str;

    /// Starts the component. Called after its dependencies are running.
    fn start(&self) -> BoxFuture<'_, Result<(), LifecycleError>>;

    /// Stops the component. Called before its dependencies are stopped.
    fn stop(&self) -> BoxFuture<'_, Result<(), LifecycleError>>;

    /// The component's self-reported health **while it is running**.
    ///
    /// The default, `None`, means "nothing to add": the manager derives the
    /// health from the lifecycle state alone (`Running` is
    /// [`Up`](HealthStatus::Up)). Override it to report
    /// [`Degraded`](HealthStatus::Degraded) or [`Down`](HealthStatus::Down)
    /// for a component that is running but impaired (a pool that lost its
    /// connections, a consumer that fell behind).
    ///
    /// The manager only consults this while the component is `Running`; in any
    /// other state the health comes from the state. It must be fast and must
    /// not block — it is called from
    /// [`ComponentManager::health`](crate::lifecycle::ComponentManager::health)
    /// on the caller's thread.
    fn health(&self) -> Option<HealthStatus> {
        None
    }
}

/// When and how the manager restarts a component automatically.
///
/// A policy applies in two situations:
///
/// - **A failed start** (error, panic or per-component timeout) is retried
///   after a backoff while restarts remain.
/// - **A reported failure** of a running component
///   ([`ComponentManager::report_failure`](crate::lifecycle::ComponentManager::report_failure))
///   triggers a `stop` (best effort) followed by a fresh `start`.
///
/// The delay before restart *n* (1-based) is `initial_backoff * 2^(n-1)`,
/// capped at `max_backoff`. The restart budget is shared by both situations and
/// is reset whenever the component is started or stopped explicitly through
/// the manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartPolicy {
    /// The most automatic restarts to attempt before giving up and leaving the
    /// component in [`Error`](crate::lifecycle::ComponentState::Error).
    pub max_restarts: u32,
    /// The delay before the first restart.
    pub initial_backoff: Duration,
    /// The upper bound on the (doubling) delay.
    pub max_backoff: Duration,
}

impl RestartPolicy {
    /// A policy allowing `max_restarts` restarts, with a 100 ms initial backoff
    /// doubling up to 30 s.
    pub fn new(max_restarts: u32) -> Self {
        Self {
            max_restarts,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(30),
        }
    }

    /// Sets the initial and maximum backoff.
    pub fn with_backoff(mut self, initial: Duration, max: Duration) -> Self {
        self.initial_backoff = initial;
        self.max_backoff = max.max(initial);
        self
    }

    /// The delay before restart number `restart` (1-based):
    /// `initial_backoff * 2^(restart-1)`, capped at `max_backoff`.
    pub fn delay(&self, restart: u32) -> Duration {
        let shift = restart.saturating_sub(1).min(31);
        self.initial_backoff
            .checked_mul(1u32 << shift)
            .map_or(self.max_backoff, |d| d.min(self.max_backoff))
    }
}

/// Per-component settings supplied at registration
/// ([`ComponentManager::register_with`](crate::lifecycle::ComponentManager::register_with)).
///
/// ```
/// use std::time::Duration;
/// use ferroly::lifecycle::{ComponentOptions, RestartPolicy};
///
/// let opts = ComponentOptions::new()
///     .start_timeout(Duration::from_secs(10))
///     .stop_timeout(Duration::from_secs(5))
///     .restart(RestartPolicy::new(3));
/// assert_eq!(opts.start_timeout, Some(Duration::from_secs(10)));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ComponentOptions {
    /// Bounds each `start` attempt of this component. Overrides the manager's
    /// default ([`ManagerOptions::start_timeout`](crate::lifecycle::ManagerOptions)).
    pub start_timeout: Option<Duration>,
    /// Bounds each `stop` of this component. Overrides both the manager's
    /// default and the per-component bound passed to
    /// [`stop_all_with_timeout`](crate::lifecycle::ComponentManager::stop_all_with_timeout).
    pub stop_timeout: Option<Duration>,
    /// Automatic restart policy; `None` (the default) disables supervision.
    pub restart: Option<RestartPolicy>,
}

impl ComponentOptions {
    /// Options with no timeouts and no restart policy.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bounds each `start` attempt.
    pub fn start_timeout(mut self, timeout: Duration) -> Self {
        self.start_timeout = Some(timeout);
        self
    }

    /// Bounds each `stop`.
    pub fn stop_timeout(mut self, timeout: Duration) -> Self {
        self.stop_timeout = Some(timeout);
        self
    }

    /// Enables automatic restarts.
    pub fn restart(mut self, policy: RestartPolicy) -> Self {
        self.restart = Some(policy);
        self
    }
}

type StartFn = Box<dyn Fn() -> BoxFuture<'static, Result<(), LifecycleError>> + Send + Sync>;
type StopFn = Box<dyn Fn() -> BoxFuture<'static, Result<(), LifecycleError>> + Send + Sync>;
type HealthFn = Box<dyn Fn() -> HealthStatus + Send + Sync>;

/// A ready-made [`Component`] backed by start/stop closures.
///
/// Avoids defining a full struct for simple cases:
///
/// ```
/// use ferroly::lifecycle::SimpleComponent;
///
/// let comp = SimpleComponent::new(
///     "db",
///     || Box::pin(async { Ok(()) }),
///     || Box::pin(async { Ok(()) }),
/// );
/// ```
pub struct SimpleComponent {
    id: String,
    start: StartFn,
    stop: StopFn,
    health: Option<HealthFn>,
}

impl SimpleComponent {
    /// Creates a component from an id and start/stop closures.
    pub fn new<S, T>(id: impl Into<String>, start: S, stop: T) -> Self
    where
        S: Fn() -> BoxFuture<'static, Result<(), LifecycleError>> + Send + Sync + 'static,
        T: Fn() -> BoxFuture<'static, Result<(), LifecycleError>> + Send + Sync + 'static,
    {
        Self {
            id: id.into(),
            start: Box::new(start),
            stop: Box::new(stop),
            health: None,
        }
    }

    /// Adds a self-reported health check (see [`Component::health`]). It must
    /// be fast and non-blocking.
    pub fn with_health<H>(mut self, health: H) -> Self
    where
        H: Fn() -> HealthStatus + Send + Sync + 'static,
    {
        self.health = Some(Box::new(health));
        self
    }
}

impl Component for SimpleComponent {
    fn id(&self) -> &str {
        &self.id
    }

    fn start(&self) -> BoxFuture<'_, Result<(), LifecycleError>> {
        (self.start)()
    }

    fn stop(&self) -> BoxFuture<'_, Result<(), LifecycleError>> {
        (self.stop)()
    }

    fn health(&self) -> Option<HealthStatus> {
        self.health.as_ref().map(|h| h())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_delay_doubles_and_caps() {
        let p = RestartPolicy::new(5)
            .with_backoff(Duration::from_millis(100), Duration::from_millis(350));
        assert_eq!(p.delay(1), Duration::from_millis(100));
        assert_eq!(p.delay(2), Duration::from_millis(200));
        assert_eq!(p.delay(3), Duration::from_millis(350));
        assert_eq!(p.delay(40), Duration::from_millis(350));
        // A zero restart index is treated as the first.
        assert_eq!(p.delay(0), Duration::from_millis(100));
    }
}

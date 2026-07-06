//! The [`Component`] trait and the [`SimpleComponent`] helper.

use std::future::Future;
use std::pin::Pin;

use ferroly::lifecycle::LifecycleError;

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
pub trait Component: Send + Sync {
    /// A stable, unique identifier for this component.
    fn id(&self) -> &str;

    /// Starts the component. Called after its dependencies are running.
    fn start(&self) -> BoxFuture<'_, Result<(), LifecycleError>>;

    /// Stops the component. Called before its dependencies are stopped.
    fn stop(&self) -> BoxFuture<'_, Result<(), LifecycleError>>;
}

type StartFn = Box<dyn Fn() -> BoxFuture<'static, Result<(), LifecycleError>> + Send + Sync>;
type StopFn = Box<dyn Fn() -> BoxFuture<'static, Result<(), LifecycleError>> + Send + Sync>;

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
        }
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
}

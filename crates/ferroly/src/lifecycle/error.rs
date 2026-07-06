//! Error type for the lifecycle crate.

use ferroly_derive::FerrolyError;

/// Errors raised by component and manager operations.
#[derive(Debug, FerrolyError)]
#[non_exhaustive]
pub enum LifecycleError {
    /// No component is registered under the given id.
    #[error("component not found: {0}")]
    ComponentNotFound(String),

    /// A component was already started.
    #[error("component already started: {0}")]
    ComponentAlreadyStarted(String),

    /// The dependency graph contains a cycle.
    #[error("cyclic dependency detected involving: {0}")]
    CyclicDependency(String),

    /// An operation exceeded its timeout.
    #[error("operation timed out for component: {0}")]
    Timeout(String),

    /// A component's own start/stop logic failed.
    #[error("component '{id}' failed: {source}")]
    ComponentFailure {
        /// The component id.
        id: String,
        /// The underlying error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl LifecycleError {
    /// Wraps an arbitrary error as a component failure.
    pub fn failure<E>(id: impl Into<String>, source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::ComponentFailure {
            id: id.into(),
            source: Box::new(source),
        }
    }
}

//! Error type for the lifecycle crate.

use std::fmt;

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

    /// A component with the same id is already registered.
    #[error("component already registered: {0}")]
    DuplicateComponent(String),

    /// The component is starting, running or stopping, so it cannot be
    /// unregistered; stop it first.
    #[error("component is still active (stop it first): {0}")]
    ComponentActive(String),

    /// The dependency graph contains a cycle.
    #[error("cyclic dependency detected involving: {0}")]
    CyclicDependency(String),

    /// An operation exceeded its timeout.
    #[error("operation timed out for component: {0}")]
    Timeout(String),

    /// A start that another caller was already running did not reach
    /// [`Running`](crate::lifecycle::ComponentState::Running). The underlying
    /// error was returned to the caller that owned the attempt.
    #[error("component failed to start: {0}")]
    StartFailed(String),

    /// A stop that another caller was already running did not reach
    /// [`Stopped`](crate::lifecycle::ComponentState::Stopped).
    #[error("component failed to stop: {0}")]
    StopFailed(String),

    /// A component's `start` or `stop` panicked. The panic was contained and the
    /// component is in the [`Error`](crate::lifecycle::ComponentState::Error)
    /// state.
    #[error("component '{id}' panicked: {message}")]
    ComponentPanicked {
        /// The component id.
        id: String,
        /// The panic payload, when it was a string.
        message: String,
    },

    /// A component's own start/stop logic failed.
    #[error("component '{id}' failed: {source}")]
    ComponentFailure {
        /// The component id.
        id: String,
        /// The underlying error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Several operations failed in one sweep (for example a best-effort
    /// `stop_all`, a parallel start, or a failed start followed by a failed
    /// rollback). The errors are in the order they occurred.
    #[error("{0}")]
    Multiple(LifecycleErrors),
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

    /// The individual errors this error stands for: the contents of a
    /// [`Multiple`](Self::Multiple), or a one-element slice for anything else.
    pub fn errors(&self) -> &[LifecycleError] {
        match self {
            Self::Multiple(list) => &list.0,
            other => std::slice::from_ref(other),
        }
    }

    /// Collapses a list of errors: `Ok(())` when empty, the error itself when
    /// there is exactly one, otherwise [`Multiple`](Self::Multiple).
    pub(crate) fn collect(mut errors: Vec<LifecycleError>) -> Result<(), LifecycleError> {
        match errors.len() {
            0 => Ok(()),
            1 => Err(errors.remove(0)),
            _ => Err(Self::Multiple(LifecycleErrors(errors))),
        }
    }

    /// Flattens `self` into `out` (a `Multiple` contributes its members).
    pub(crate) fn flatten_into(self, out: &mut Vec<LifecycleError>) {
        match self {
            Self::Multiple(list) => out.extend(list.0),
            other => out.push(other),
        }
    }
}

/// The list of errors carried by [`LifecycleError::Multiple`].
///
/// Displays as `N errors: first; second; …`. Use [`as_slice`](Self::as_slice)
/// or iterate to inspect the members.
#[derive(Debug)]
pub struct LifecycleErrors(Vec<LifecycleError>);

impl LifecycleErrors {
    /// The errors, in the order they occurred.
    pub fn as_slice(&self) -> &[LifecycleError] {
        &self.0
    }

    /// The number of errors.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the list is empty (never the case for a constructed
    /// [`LifecycleError::Multiple`]).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Consumes the list, returning the errors.
    pub fn into_vec(self) -> Vec<LifecycleError> {
        self.0
    }
}

impl<'a> IntoIterator for &'a LifecycleErrors {
    type Item = &'a LifecycleError;
    type IntoIter = std::slice::Iter<'a, LifecycleError>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl fmt::Display for LifecycleErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} errors: ", self.0.len())?;
        for (i, e) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{e}")?;
        }
        Ok(())
    }
}

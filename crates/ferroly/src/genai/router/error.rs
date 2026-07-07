//! Router error type.

use ferroly::genai::GenAiError;

/// Errors from the model router.
#[derive(Debug, ferroly::FerrolyError)]
#[non_exhaustive]
pub enum RouterError {
    /// No model satisfied the task's requirements.
    #[error("no route: {reason}")]
    NoRoute {
        /// Why nothing matched.
        reason: String,
    },

    /// Every candidate (primary + fallbacks) failed; `last` is the final error.
    #[error("all {attempts} route(s) failed")]
    AllRoutesFailed {
        /// How many candidates were attempted.
        attempts: usize,
        /// The last provider error.
        #[source]
        last: GenAiError,
    },

    /// A route/pin named a provider that is not registered.
    #[error("provider not registered: {0}")]
    UnknownProvider(String),

    /// The routing deadline elapsed before a route succeeded.
    #[error("routing deadline exceeded")]
    DeadlineExceeded,

    /// A wrapped provider error (a permanent failure that fallback can't fix).
    #[error(transparent)]
    GenAi(#[from] GenAiError),

    /// A configuration or registry-build error.
    #[error("router configuration error: {0}")]
    Config(String),
}

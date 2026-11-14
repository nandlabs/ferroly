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

    /// Every candidate was skipped because its circuit breaker is open: the
    /// models are registered and eligible, but all are currently shedding
    /// load after repeated failures. Nothing was sent.
    #[error("every candidate model is unavailable (circuit open): {models}")]
    CircuitOpen {
        /// The skipped candidates, `provider/model`, comma-separated.
        models: String,
    },

    /// The task's cumulative budget is used up (or too small for any
    /// candidate). Nothing was sent.
    #[error("budget '{key}' exceeded: spent ${spent_usd:.4} of ${limit_usd:.4}")]
    BudgetExceeded {
        /// The budget key.
        key: String,
        /// The configured limit, in USD.
        limit_usd: f64,
        /// What the key has spent so far, in USD.
        spent_usd: f64,
    },

    /// The request cannot satisfy what the task demands — for example the
    /// task requires tool use but the request carries no tools. A caller
    /// error, raised before any provider is called.
    #[error("invalid routed request: {0}")]
    InvalidRequest(String),
}

impl RouterError {
    /// A short, stable label for the failure, suitable as a metric label.
    pub fn kind_label(&self) -> &'static str {
        match self {
            RouterError::NoRoute { .. } => "no_route",
            RouterError::AllRoutesFailed { .. } => "all_routes_failed",
            RouterError::UnknownProvider(_) => "unknown_provider",
            RouterError::DeadlineExceeded => "deadline_exceeded",
            RouterError::GenAi(e) => e.kind_label(),
            RouterError::Config(_) => "config",
            RouterError::CircuitOpen { .. } => "circuit_open",
            RouterError::BudgetExceeded { .. } => "budget_exceeded",
            RouterError::InvalidRequest(_) => "invalid_request",
        }
    }
}

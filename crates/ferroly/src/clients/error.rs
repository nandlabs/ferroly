//! Error types for the clients module.

use ferroly_derive::FerrolyError;

/// Returned by [`CircuitBreaker::can_execute`](ferroly::clients::CircuitBreaker::can_execute)
/// and [`CircuitBreaker::try_acquire`](ferroly::clients::CircuitBreaker::try_acquire)
/// when the breaker is open (or half-open with every trial slot in use).
#[derive(Debug, Clone, FerrolyError)]
#[error("circuit breaker is open; execution not permitted")]
pub struct CircuitOpenError;

/// Returned when a [`Bulkhead`](ferroly::clients::Bulkhead) has no free slot
/// (immediately, or after its queue timeout).
#[derive(Debug, Clone, PartialEq, Eq, FerrolyError)]
#[error("bulkhead is full; execution not permitted")]
pub struct BulkheadFullError;

/// Returned when an operation did not finish before its timeout or deadline.
#[derive(Debug, Clone, PartialEq, Eq, FerrolyError)]
#[error("operation timed out")]
pub struct TimeoutError;

/// Returned when a [`RateLimiter`](ferroly::clients::RateLimiter) could not
/// grant the requested tokens (in time, or at all).
#[derive(Debug, Clone, PartialEq, Eq, FerrolyError)]
#[error("rate limit exceeded")]
pub struct RateLimitedError;

/// The error of an operation run through a resilience policy
/// ([`Policy`](ferroly::clients::Policy), or
/// [`CircuitBreaker::call`](ferroly::clients::CircuitBreaker::call)): either a
/// rejection by the policy itself, or the operation's own error.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PolicyError<E> {
    /// The overall deadline or a per-attempt timeout elapsed.
    Timeout,
    /// The circuit breaker is open; the operation was not attempted.
    CircuitOpen,
    /// The bulkhead had no free slot; the operation was not attempted.
    BulkheadFull,
    /// The operation ran and failed with its own error.
    Inner(E),
}

impl<E: std::fmt::Display> std::fmt::Display for PolicyError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyError::Timeout => f.write_str("operation timed out"),
            PolicyError::CircuitOpen => {
                f.write_str("circuit breaker is open; execution not permitted")
            }
            PolicyError::BulkheadFull => f.write_str("bulkhead is full; execution not permitted"),
            PolicyError::Inner(e) => e.fmt(f),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for PolicyError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PolicyError::Inner(e) => Some(e),
            _ => None,
        }
    }
}

impl<E> PolicyError<E> {
    /// The operation's own error, if that is what this is.
    pub fn into_inner(self) -> Option<E> {
        match self {
            PolicyError::Inner(e) => Some(e),
            _ => None,
        }
    }

    /// Whether the policy rejected the call without running the operation
    /// (open circuit or full bulkhead).
    pub fn is_rejection(&self) -> bool {
        matches!(self, PolicyError::CircuitOpen | PolicyError::BulkheadFull)
    }
}

impl<E> From<CircuitOpenError> for PolicyError<E> {
    fn from(_: CircuitOpenError) -> Self {
        PolicyError::CircuitOpen
    }
}

impl<E> From<BulkheadFullError> for PolicyError<E> {
    fn from(_: BulkheadFullError) -> Self {
        PolicyError::BulkheadFull
    }
}

impl<E> From<TimeoutError> for PolicyError<E> {
    fn from(_: TimeoutError) -> Self {
        PolicyError::Timeout
    }
}

/// An error preparing credentials for a request
/// ([`AuthProvider::prepare`](ferroly::clients::AuthProvider::prepare),
/// [`TokenSource`](ferroly::clients::TokenSource)).
///
/// Messages never contain credential values: no client secret, no token, no
/// request or response body.
#[derive(Debug, Clone, PartialEq, Eq, FerrolyError)]
#[non_exhaustive]
pub enum AuthError {
    /// The token endpoint could not be reached (connect, TLS, timeout,
    /// protocol error).
    #[error("token request failed: {0}")]
    Transport(String),
    /// The token endpoint answered with a non-success status. `code` is the
    /// OAuth 2.0 `error` code from the response body (for example
    /// `invalid_client`), when one was present.
    #[error("token endpoint rejected the request (status {status})")]
    Rejected {
        /// The HTTP status code.
        status: u16,
        /// The OAuth 2.0 error code, if the body carried one.
        code: Option<String>,
    },
    /// The token endpoint answered successfully but the body was not a usable
    /// token response.
    #[error("invalid token response: {0}")]
    InvalidResponse(String),
    /// The provider is misconfigured.
    #[error("invalid auth configuration: {0}")]
    Config(String),
    /// Any other failure reported by a custom
    /// [`TokenSource`](ferroly::clients::TokenSource).
    #[error("{0}")]
    Other(String),
}

//! Resilience primitives for HTTP clients: auth, retry, circuit breaking,
//! concurrency and rate limits, timeouts, and a pipeline that composes them.
//!
//! The shared building block that the `rest` and `genai` modules use. It provides:
//!
//! - **Authentication** — [`AuthProvider`] with the static [`BearerAuth`] /
//!   [`ApiKeyAuth`] / [`BasicAuth`] schemes, and refreshing credentials:
//!   [`TokenSource`], [`RefreshingAuth`] and (with the `codec` feature)
//!   `OAuth2ClientCredentials`. Use [`authorize`] to apply any of them.
//! - **Retry** — [`RetryPolicy`] (the backoff schedule), [`Retry`] (jitter,
//!   total time limit, [`RetryBudget`], server-provided delays) and the
//!   [`retry`] / [`retry_with`] drivers.
//! - **Circuit breaking** — [`CircuitBreaker`] with the closed/open/half-open
//!   state machine, an RAII [`Permit`], an optional [`FailureRate`] window,
//!   state-change callbacks, and a keyed [`CircuitBreakerRegistry`].
//! - **Load limits** — [`Bulkhead`] (concurrency) and [`RateLimiter`] (token
//!   bucket).
//! - **Time limits** — [`with_timeout`] and [`Deadline`].
//! - **Composition** — [`Policy`]: timeout → retry → breaker → bulkhead →
//!   rate limit around one async operation.
//! - [`Secret`], a redacting, wipe-on-drop holder for credentials.
//!
//! All of it is generic over the operation and its error type; nothing here is
//! specific to HTTP except the auth providers.

#![deny(missing_docs)]

mod auth;
mod breaker;
mod bulkhead;
mod error;
mod oauth2;
mod policy;
mod ratelimit;
mod retry;
mod rng;
mod timeout;

// `ferroly::hash` owns the single base64 implementation and the `Secret`
// type; the `clients` feature implies `hash`.
pub(crate) use ferroly::hash::base64;
pub use ferroly::hash::Secret;

pub use auth::{
    authorize, AccessToken, ApiKeyAuth, AuthProvider, BasicAuth, BearerAuth, RefreshingAuth,
    TokenSource,
};
pub use breaker::{
    BreakerMetrics, CircuitBreaker, CircuitBreakerBuilder, CircuitBreakerConfig,
    CircuitBreakerRegistry, CircuitState, FailureRate, Outcome, Permit,
};
pub use bulkhead::{Bulkhead, BulkheadPermit};
pub use error::{
    AuthError, BulkheadFullError, CircuitOpenError, PolicyError, RateLimitedError, TimeoutError,
};
pub use oauth2::{ClientAuthStyle, OAuth2ClientCredentials};
pub use policy::{ErrorClass, Policy};
pub use ratelimit::RateLimiter;
pub use retry::{
    parse_retry_after, retry, retry_with, Jitter, Retry, RetryAttempt, RetryBudget, RetryDecision,
    RetryPolicy,
};
pub use timeout::{with_timeout, Deadline};

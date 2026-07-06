//! Resilience primitives for HTTP clients: retry, circuit breaker, and auth.
//!
//! The shared building block that the `rest` and `genai` modules use. It provides:
//!
//! - [`AuthProvider`] and the [`BearerAuth`] / [`ApiKeyAuth`] / [`BasicAuth`]
//!   implementations, applied to outbound `ferroly::http::Request`s.
//! - [`RetryPolicy`] and the [`retry`] driver.
//! - [`CircuitBreaker`] with the classic closed/open/half-open state machine.

#![deny(missing_docs)]

mod auth;
mod breaker;
mod error;
mod retry;

pub use auth::{ApiKeyAuth, AuthProvider, BasicAuth, BearerAuth};
pub use breaker::{CircuitBreaker, CircuitBreakerConfig, CircuitState};
pub use error::CircuitOpenError;
pub use retry::{retry, RetryPolicy};

//! Error types for the clients crate.

use ferroly_derive::FerrolyError;

/// Returned by [`CircuitBreaker::can_execute`](ferroly::clients::CircuitBreaker::can_execute)
/// when the breaker is open.
#[derive(Debug, Clone, FerrolyError)]
#[error("circuit breaker is open; execution not permitted")]
pub struct CircuitOpenError;

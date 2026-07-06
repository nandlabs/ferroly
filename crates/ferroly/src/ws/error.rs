//! Error type for the WebSocket crate.

use ferroly_derive::FerrolyError;

/// Errors raised by WebSocket client operations.
#[derive(Debug, FerrolyError)]
#[non_exhaustive]
pub enum WsError {
    /// The connection handshake failed.
    #[error("connection error: {0}")]
    Connect(String),

    /// Sending a message failed (connection likely closed).
    #[error("send error: {0}")]
    Send(String),

    /// The underlying transport failed.
    #[error("transport error: {0}")]
    Transport(String),
}

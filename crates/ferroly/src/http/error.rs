//! Error type for the HTTP stack.

use ferroly_derive::FerrolyError;

/// Errors raised by the in-house HTTP client/server.
#[derive(Debug, FerrolyError)]
#[non_exhaustive]
pub enum HttpError {
    /// A URL could not be parsed.
    #[error("invalid url: {0}")]
    InvalidUrl(String),

    /// An underlying I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A TLS handshake or configuration error.
    #[error("tls error: {0}")]
    Tls(String),

    /// The peer sent a malformed HTTP message.
    #[error("malformed http: {0}")]
    Protocol(String),

    /// An operation exceeded its timeout.
    #[error("timed out")]
    Timeout,
}

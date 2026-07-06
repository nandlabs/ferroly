//! Error types for the REST client and server.

use ferroly_derive::FerrolyError;

/// Errors raised by the HTTP client.
#[derive(Debug, FerrolyError)]
#[non_exhaustive]
pub enum ClientError {
    /// The URL was malformed or a path placeholder was left unsubstituted.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// The underlying transport failed (DNS, TLS, connection, timeout).
    #[error("transport error: {0}")]
    Transport(#[from] ferroly::http::HttpError),

    /// The response body could not be (de)encoded by the codec.
    #[error("codec error: {0}")]
    Codec(#[from] ferroly::codec::CodecError),
}

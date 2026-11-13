//! Error types for the REST client and server.

use ferroly_derive::FerrolyError;

/// Errors raised by the HTTP client.
#[derive(Debug, FerrolyError)]
#[non_exhaustive]
pub enum ClientError {
    /// The URL was malformed, a path placeholder was left unsubstituted, a
    /// path parameter value cannot be used where its placeholder stands, or
    /// the client itself is misconfigured.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// The underlying transport failed (DNS, TLS, connection, timeout).
    #[error("transport error: {0}")]
    Transport(#[from] ferroly::http::HttpError),

    /// A request or response body could not be encoded or decoded by the
    /// codec.
    #[error("codec error: {0}")]
    Codec(#[from] ferroly::codec::CodecError),

    /// The authentication provider could not supply credentials (a token
    /// refresh failed, say). The request was not sent.
    #[error("authentication error: {0}")]
    Auth(#[from] ferroly::clients::AuthError),

    /// The server answered with a non-success status. Produced by
    /// [`Response::error_for_status`](ferroly::rest::Response::error_for_status)
    /// and by clients built with
    /// [`error_for_status(true)`](ferroly::rest::ClientOptionsBuilder::error_for_status).
    #[error("unexpected response status {status}")]
    Status {
        /// The HTTP status code.
        status: u16,
        /// The response body, for the error details servers usually put
        /// there.
        body: Vec<u8>,
    },

    /// The circuit breaker for this destination is open; the request was not
    /// sent. The value names the breaker (the `host:port`).
    #[error("circuit breaker open for {0}")]
    CircuitOpen(String),

    /// The configured [`Policy`](ferroly::clients::Policy) refused to run the
    /// request (a full bulkhead, for example); it was not sent.
    #[error("request rejected: {0}")]
    Rejected(String),

    /// The deadline of the configured [`Policy`](ferroly::clients::Policy)
    /// passed. (Timeouts of the HTTP client itself are
    /// [`Transport`](ClientError::Transport) errors.)
    #[error("request timed out")]
    Timeout,
}

impl ClientError {
    /// The HTTP status code, for a [`Status`](ClientError::Status) error.
    pub fn status(&self) -> Option<u16> {
        match self {
            ClientError::Status { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// The response body of a [`Status`](ClientError::Status) error.
    pub fn body(&self) -> Option<&[u8]> {
        match self {
            ClientError::Status { body, .. } => Some(body),
            _ => None,
        }
    }

    /// Whether the request ran out of time: a policy deadline, or a connect,
    /// response or read timeout of the transport.
    pub fn is_timeout(&self) -> bool {
        match self {
            ClientError::Timeout => true,
            ClientError::Transport(e) => e.is_timeout(),
            _ => false,
        }
    }
}

#[cfg(feature = "errutils")]
impl ferroly::errutils::Classify for ClientError {
    fn kind(&self) -> ferroly::errutils::ErrorKind {
        use ferroly::errutils::ErrorKind as K;
        use ferroly::http::HttpError;
        match self {
            ClientError::InvalidRequest(_) | ClientError::Codec(_) => K::InvalidInput,
            ClientError::Auth(_) => K::Unauthenticated,
            ClientError::CircuitOpen(_) | ClientError::Rejected(_) => K::Unavailable,
            ClientError::Timeout => K::Timeout,
            ClientError::Transport(e) if e.is_timeout() => K::Timeout,
            ClientError::Transport(HttpError::InvalidUrl(_)) => K::InvalidInput,
            ClientError::Transport(_) => K::Unavailable,
            ClientError::Status { status, .. } => match status {
                400 | 422 => K::InvalidInput,
                401 => K::Unauthenticated,
                403 => K::PermissionDenied,
                404 | 410 => K::NotFound,
                409 | 412 => K::Conflict,
                408 | 504 => K::Timeout,
                429 => K::RateLimited,
                501 => K::Unsupported,
                502 | 503 => K::Unavailable,
                400..=499 => K::InvalidInput,
                _ => K::Internal,
            },
        }
    }
}

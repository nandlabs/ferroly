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

    /// The peer sent a malformed HTTP message, or the caller asked the client
    /// to send one (an invalid method token, header name or request target).
    #[error("malformed http: {0}")]
    Protocol(String),

    /// An operation exceeded its timeout: the response-head timeout, the
    /// per-read idle timeout or the total deadline on the client; a head, body,
    /// write or handler timeout on the server.
    #[error("timed out")]
    Timeout,

    /// The client could not establish a connection (TCP connect, TLS handshake
    /// and any proxy `CONNECT`) within the connect timeout. No request byte
    /// was sent, so the request is safe to retry whatever its method.
    #[error("connect timed out")]
    ConnectTimeout,

    /// A message body exceeded the configured size limit (the client's
    /// response-body cap, counted before and after decompression).
    #[error("body exceeds the configured limit of {0} bytes")]
    BodyTooLarge(u64),

    /// The client followed the configured maximum number of redirects and the
    /// server answered with yet another one.
    #[error("too many redirects")]
    TooManyRedirects,

    /// A proxy refused or failed the request (a non-2xx answer to `CONNECT`,
    /// or an unusable proxy configuration).
    #[error("proxy error: {0}")]
    Proxy(String),

    /// A server handler panicked while producing a response. Reported through
    /// the server's error hook; the client is answered with a `500`.
    #[error("handler panicked: {0}")]
    HandlerPanic(String),
}

impl HttpError {
    /// Whether this is one of the timeout errors ([`Timeout`](Self::Timeout)
    /// or [`ConnectTimeout`](Self::ConnectTimeout)).
    pub fn is_timeout(&self) -> bool {
        matches!(self, HttpError::Timeout | HttpError::ConnectTimeout)
    }
}

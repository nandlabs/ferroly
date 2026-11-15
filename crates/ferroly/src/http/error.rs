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

    /// An HTTP/2 stream or connection failed (feature `http2`). The
    /// [`Http2Error`] says why in a structured form
    /// ([`HttpError::http2_cause`]), so callers such as a gRPC client can map
    /// it without matching message text.
    #[error("http2: {0}")]
    Http2(Http2Error),
}

impl HttpError {
    /// Whether this is one of the timeout errors: [`Timeout`](Self::Timeout),
    /// [`ConnectTimeout`](Self::ConnectTimeout), or an HTTP/2 connection
    /// timer ([`Http2Cause::Timeout`]: a write stall, an unanswered
    /// keep-alive PING or SETTINGS, a stream stall), so HTTP/1.1 and HTTP/2
    /// timeouts are told apart from other failures the same way.
    ///
    /// ```
    /// use ferroly::http::{Http2Cause, Http2Error, HttpError};
    ///
    /// assert!(HttpError::Timeout.is_timeout());
    /// assert!(HttpError::from(Http2Error::new(Http2Cause::Timeout, "stalled")).is_timeout());
    /// assert!(!HttpError::from(Http2Error::new(Http2Cause::Refused, "refused")).is_timeout());
    /// ```
    pub fn is_timeout(&self) -> bool {
        match self {
            HttpError::Timeout | HttpError::ConnectTimeout => true,
            HttpError::Http2(e) => matches!(e.cause(), Http2Cause::Timeout),
            _ => false,
        }
    }

    /// Whether the client failed while opening the connection: resolving the
    /// host name or the TCP connect (an [`Io`](Self::Io) error, whose kind and
    /// message are those of the underlying failure), or
    /// [`ConnectTimeout`](Self::ConnectTimeout). No request byte was sent, so
    /// the request is safe to retry whatever its method.
    ///
    /// An `Io` error of the same kind raised later (writing the request or
    /// reading the response on an open connection) returns `false`: a
    /// "network unreachable" after the request went out does not mean it was
    /// not processed. [`Tls`](Self::Tls) and [`Proxy`](Self::Proxy) failures
    /// also happen before the request is written, but are not counted here.
    ///
    /// ```
    /// use ferroly::http::HttpError;
    ///
    /// let late = HttpError::Io(std::io::ErrorKind::ConnectionRefused.into());
    /// assert!(!late.is_connect_failure());
    /// assert!(HttpError::ConnectTimeout.is_connect_failure());
    /// ```
    pub fn is_connect_failure(&self) -> bool {
        match self {
            HttpError::ConnectTimeout => true,
            HttpError::Io(e) => e
                .get_ref()
                .is_some_and(|inner| inner.is::<ConnectFailure>()),
            _ => false,
        }
    }

    /// Why an HTTP/2 stream or connection failed, when this is an
    /// [`Http2`](Self::Http2) error.
    ///
    /// ```
    /// use ferroly::http::{Http2Cause, HttpError};
    ///
    /// fn safe_to_retry(e: &HttpError) -> bool {
    ///     matches!(e.http2_cause(), Some(Http2Cause::Refused))
    /// }
    /// # let _ = safe_to_retry;
    /// ```
    pub fn http2_cause(&self) -> Option<Http2Cause> {
        match self {
            HttpError::Http2(e) => Some(e.cause()),
            _ => None,
        }
    }
}

/// Marks an I/O error raised while resolving the host or opening the TCP
/// connection (see [`HttpError::is_connect_failure`]). Displays as the
/// underlying error, so messages are unchanged.
#[derive(Debug)]
struct ConnectFailure(std::io::Error);

impl std::fmt::Display for ConnectFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for ConnectFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

/// Wraps an error from name resolution or the TCP connect as an
/// [`HttpError::Io`] of the same kind, which
/// [`is_connect_failure`](HttpError::is_connect_failure) recognises.
pub(crate) fn connect_failure(e: std::io::Error) -> HttpError {
    HttpError::Io(std::io::Error::new(e.kind(), ConnectFailure(e)))
}

/// An HTTP/2 error code, as carried by RST_STREAM and GOAWAY
/// (RFC 9113 §7). Codes this crate does not know are kept as they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Http2ErrorCode(pub(crate) u32);

impl Http2ErrorCode {
    /// `NO_ERROR` (0x0): a graceful shutdown, or a stream no longer needed.
    pub const NO_ERROR: Self = Self(0x0);
    /// `PROTOCOL_ERROR` (0x1).
    pub const PROTOCOL_ERROR: Self = Self(0x1);
    /// `INTERNAL_ERROR` (0x2).
    pub const INTERNAL_ERROR: Self = Self(0x2);
    /// `FLOW_CONTROL_ERROR` (0x3).
    pub const FLOW_CONTROL_ERROR: Self = Self(0x3);
    /// `SETTINGS_TIMEOUT` (0x4).
    pub const SETTINGS_TIMEOUT: Self = Self(0x4);
    /// `STREAM_CLOSED` (0x5).
    pub const STREAM_CLOSED: Self = Self(0x5);
    /// `FRAME_SIZE_ERROR` (0x6).
    pub const FRAME_SIZE_ERROR: Self = Self(0x6);
    /// `REFUSED_STREAM` (0x7): the stream was not processed.
    pub const REFUSED_STREAM: Self = Self(0x7);
    /// `CANCEL` (0x8): the stream is no longer needed.
    pub const CANCEL: Self = Self(0x8);
    /// `COMPRESSION_ERROR` (0x9).
    pub const COMPRESSION_ERROR: Self = Self(0x9);
    /// `CONNECT_ERROR` (0xa).
    pub const CONNECT_ERROR: Self = Self(0xa);
    /// `ENHANCE_YOUR_CALM` (0xb): the peer is generating excessive load.
    pub const ENHANCE_YOUR_CALM: Self = Self(0xb);
    /// `INADEQUATE_SECURITY` (0xc).
    pub const INADEQUATE_SECURITY: Self = Self(0xc);
    /// `HTTP_1_1_REQUIRED` (0xd).
    pub const HTTP_1_1_REQUIRED: Self = Self(0xd);

    /// The code for a raw value.
    pub const fn new(code: u32) -> Self {
        Self(code)
    }

    /// The raw value.
    pub const fn code(self) -> u32 {
        self.0
    }

    /// The RFC name of the code (`"unknown error code"` for others).
    pub fn name(self) -> &'static str {
        match self.0 {
            0x0 => "NO_ERROR",
            0x1 => "PROTOCOL_ERROR",
            0x2 => "INTERNAL_ERROR",
            0x3 => "FLOW_CONTROL_ERROR",
            0x4 => "SETTINGS_TIMEOUT",
            0x5 => "STREAM_CLOSED",
            0x6 => "FRAME_SIZE_ERROR",
            0x7 => "REFUSED_STREAM",
            0x8 => "CANCEL",
            0x9 => "COMPRESSION_ERROR",
            0xa => "CONNECT_ERROR",
            0xb => "ENHANCE_YOUR_CALM",
            0xc => "INADEQUATE_SECURITY",
            0xd => "HTTP_1_1_REQUIRED",
            _ => "unknown error code",
        }
    }
}

impl std::fmt::Display for Http2ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (0x{:x})", self.name(), self.0)
    }
}

/// Why an HTTP/2 stream or connection failed (see
/// [`HttpError::http2_cause`]).
///
/// The enum is `#[non_exhaustive]`: match it with a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Http2Cause {
    /// The peer reset the stream with RST_STREAM carrying this code.
    Reset(Http2ErrorCode),
    /// The peer refused the stream before processing it (`REFUSED_STREAM`,
    /// or a stream above the last stream id of its GOAWAY). Nothing was
    /// processed, so the request is safe to retry. The client already
    /// retries once on another connection when it can replay the body.
    Refused,
    /// The peer sent GOAWAY with this code and last stream id, and the
    /// connection closed before the stream completed.
    GoAway {
        /// The GOAWAY error code (`NO_ERROR` for a graceful shutdown).
        code: Http2ErrorCode,
        /// The highest stream id the peer may have processed.
        last_stream_id: u32,
    },
    /// The connection closed (the peer ended the TCP stream, or the
    /// connection was shut down) before the stream completed, without a
    /// GOAWAY from the peer.
    ConnectionClosed,
    /// The connection failed with an I/O error of this kind (for example a
    /// TCP reset).
    Io(std::io::ErrorKind),
    /// A connection-level timer fired: buffered output made no progress for
    /// the write timeout, a keep-alive PING or this side's SETTINGS was not
    /// acknowledged in time, a server's head timeout expired, or a stream's
    /// output stalled on flow control for the stream stall timeout.
    Timeout,
    /// This side found the peer exceeding one of its budgets (a rapid-reset
    /// or control-frame flood, an oversized header block or a CONTINUATION
    /// flood) and closed the connection with `GOAWAY(ENHANCE_YOUR_CALM)`.
    LocalLimit,
    /// This side found the peer breaking an HTTP/2 rule (a malformed frame,
    /// header block or message, or a flow-control violation) and reset the
    /// stream, or closed the connection, with this code.
    Protocol(Http2ErrorCode),
    /// This side reset the stream with this code for a reason of its own,
    /// not because the peer broke a rule: the stream's reader went away
    /// (`CANCEL`), a server handler stopped before completing its response
    /// or failed while streaming it (`INTERNAL_ERROR`), the response was
    /// complete before the request body, so the rest of the body is not
    /// needed (`NO_ERROR`), or a buffered request body was refused because
    /// the connection holds too many (`REFUSED_STREAM`).
    LocalReset(Http2ErrorCode),
}

/// An HTTP/2 stream or connection failure: a structured [`Http2Cause`]
/// and a human-readable description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Http2Error {
    cause: Http2Cause,
    detail: std::borrow::Cow<'static, str>,
}

impl Http2Error {
    /// An error with `cause` and a description.
    pub fn new(cause: Http2Cause, detail: impl Into<std::borrow::Cow<'static, str>>) -> Self {
        Self {
            cause,
            detail: detail.into(),
        }
    }

    /// Why the stream or connection failed.
    pub fn cause(&self) -> Http2Cause {
        self.cause
    }

    /// The description, without the cause.
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl std::fmt::Display for Http2Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)?;
        match self.cause {
            Http2Cause::Reset(code) | Http2Cause::Protocol(code) | Http2Cause::LocalReset(code) => {
                write!(f, ": {code}")
            }
            Http2Cause::GoAway {
                code,
                last_stream_id,
            } => write!(f, ": GOAWAY {code}, last stream {last_stream_id}"),
            _ => Ok(()),
        }
    }
}

impl std::error::Error for Http2Error {}

impl From<Http2Error> for HttpError {
    fn from(e: Http2Error) -> Self {
        HttpError::Http2(e)
    }
}

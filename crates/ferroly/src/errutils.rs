//! Error utilities for the Ferroly toolkit.
//!
//! Three small, dependency-free building blocks:
//!
//! - [`MultiError`] collects several errors into one value that still
//!   implements [`std::error::Error`].
//! - [`Context`] adds a human-readable message to any error while keeping the
//!   original reachable through [`Error::source`] (`.context("…")` /
//!   `.with_context(|| …)` on `Result` and `Option`).
//! - [`Classify`] / [`ErrorKind`] give errors a coarse category so callers can
//!   ask "is this worth retrying?" or "is this the caller's fault?" without
//!   matching on every concrete error type.
//!
//! ```
//! use ferroly::errutils::MultiError;
//!
//! let mut errs = MultiError::new();
//! errs.push(std::io::Error::new(std::io::ErrorKind::Other, "disk full"));
//! errs.push_msg("validation failed");
//!
//! assert!(!errs.is_empty());
//! assert_eq!(errs.len(), 2);
//! ```
//!
//! ```
//! use ferroly::errutils::Context;
//! use std::error::Error;
//!
//! fn read(path: &str) -> Result<String, ferroly::errutils::ContextError> {
//!     std::fs::read_to_string(path).with_context(|| format!("reading {path}"))
//! }
//!
//! let err = read("/definitely/not/here").unwrap_err();
//! assert_eq!(err.to_string(), "reading /definitely/not/here");
//! assert!(err.source().is_some()); // the io::Error is still there
//! ```

#![deny(missing_docs)]

use std::error::Error;
use std::fmt;

/// A boxed, thread-safe error — the element type held by [`MultiError`].
pub type BoxError = Box<dyn Error + Send + Sync + 'static>;

// ---- MultiError ------------------------------------------------------------

/// Aggregates multiple errors into a single error value.
///
/// Push errors as they occur, then return the aggregate. The `Display`
/// implementation joins each contained error's message on its own line, and
/// [`Error::source`] returns the first collected error.
#[derive(Debug, Default)]
pub struct MultiError {
    errors: Vec<BoxError>,
}

impl MultiError {
    /// Creates an empty `MultiError`.
    pub fn new() -> Self {
        Self { errors: Vec::new() }
    }

    /// Appends an error to the aggregate.
    pub fn push<E>(&mut self, err: E)
    where
        E: Into<BoxError>,
    {
        self.errors.push(err.into());
    }

    /// Appends a plain string message as an error.
    pub fn push_msg<S: Into<String>>(&mut self, msg: S) {
        self.errors.push(Box::new(StringError(msg.into())));
    }

    /// Returns `true` if no errors have been collected.
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    /// Returns the number of collected errors.
    pub fn len(&self) -> usize {
        self.errors.len()
    }

    /// Returns a slice of the collected errors.
    pub fn errors(&self) -> &[BoxError] {
        &self.errors
    }

    /// Iterates over the collected errors in insertion order.
    pub fn iter(&self) -> std::slice::Iter<'_, BoxError> {
        self.errors.iter()
    }

    /// Returns the first collected error, if any.
    pub fn first(&self) -> Option<&BoxError> {
        self.errors.first()
    }

    /// Consumes the aggregate, returning the collected errors.
    pub fn into_errors(self) -> Vec<BoxError> {
        self.errors
    }

    /// Consumes the aggregate, returning `Ok(())` if empty or `Err(self)` if any
    /// errors were collected. Convenient at the end of a fallible batch.
    pub fn into_result(self) -> Result<(), MultiError> {
        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(self)
        }
    }
}

impl fmt::Display for MultiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} error(s) occurred:", self.errors.len())?;
        for (i, err) in self.errors.iter().enumerate() {
            write!(f, "\n  [{}] {}", i + 1, err)?;
        }
        Ok(())
    }
}

impl Error for MultiError {
    /// The first collected error, so generic "walk the `source()` chain"
    /// reporters reach at least one underlying cause. Use [`MultiError::iter`]
    /// to see all of them.
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.errors.first().map(|e| {
            let e: &(dyn Error + 'static) = e.as_ref();
            e
        })
    }
}

impl<E: Into<BoxError>> Extend<E> for MultiError {
    fn extend<T: IntoIterator<Item = E>>(&mut self, iter: T) {
        self.errors.extend(iter.into_iter().map(Into::into));
    }
}

impl<E: Into<BoxError>> FromIterator<E> for MultiError {
    fn from_iter<T: IntoIterator<Item = E>>(iter: T) -> Self {
        Self {
            errors: iter.into_iter().map(Into::into).collect(),
        }
    }
}

impl<E: Into<BoxError>> From<Vec<E>> for MultiError {
    fn from(errors: Vec<E>) -> Self {
        errors.into_iter().collect()
    }
}

impl IntoIterator for MultiError {
    type Item = BoxError;
    type IntoIter = std::vec::IntoIter<BoxError>;

    fn into_iter(self) -> Self::IntoIter {
        self.errors.into_iter()
    }
}

impl<'a> IntoIterator for &'a MultiError {
    type Item = &'a BoxError;
    type IntoIter = std::slice::Iter<'a, BoxError>;

    fn into_iter(self) -> Self::IntoIter {
        self.errors.iter()
    }
}

/// A minimal error type wrapping a `String` message.
#[derive(Debug)]
struct StringError(String);

impl fmt::Display for StringError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for StringError {}

// ---- context wrapping ------------------------------------------------------

/// An error annotated with a context message.
///
/// Produced by the [`Context`] extension trait. `Display` shows only the
/// context message (`{}`) or the whole chain joined by `": "` (`{:#}`); the
/// wrapped error stays reachable through [`Error::source`], so nothing is lost.
#[derive(Debug)]
pub struct ContextError {
    context: String,
    source: Option<BoxError>,
}

impl ContextError {
    /// Wraps `source` with a context message.
    pub fn new<C, E>(context: C, source: E) -> Self
    where
        C: fmt::Display,
        E: Into<BoxError>,
    {
        Self {
            context: context.to_string(),
            source: Some(source.into()),
        }
    }

    /// Creates a context error with no underlying cause (what
    /// [`Context::context`] on an `Option` produces for `None`).
    pub fn msg<C: fmt::Display>(context: C) -> Self {
        Self {
            context: context.to_string(),
            source: None,
        }
    }

    /// The context message attached to this error.
    pub fn context(&self) -> &str {
        &self.context
    }

    /// The innermost error of the chain (this error itself when it has no
    /// source).
    pub fn root_cause(&self) -> &(dyn Error + 'static) {
        let mut cur: &(dyn Error + 'static) = self;
        while let Some(next) = cur.source() {
            cur = next;
        }
        cur
    }

    /// Consumes the wrapper, returning the wrapped error (if there is one).
    pub fn into_source(self) -> Option<BoxError> {
        self.source
    }
}

impl fmt::Display for ContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.context)?;
        if f.alternate() {
            let mut cur = self.source();
            while let Some(e) = cur {
                write!(f, ": {e}")?;
                cur = e.source();
            }
        }
        Ok(())
    }
}

impl Error for ContextError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_ref().map(|e| {
            let e: &(dyn Error + 'static) = e.as_ref();
            e
        })
    }
}

/// Extension methods that attach context to the error of a `Result` (or turn a
/// `None` into an error).
///
/// The original error is preserved as the [`Error::source`] of the returned
/// [`ContextError`]. Because `ContextError` is itself an ordinary error, calls
/// can be stacked: each layer adds one message and one link to the chain.
///
/// ```
/// use ferroly::errutils::{chain, Context};
///
/// let r: Result<u16, _> = "eighty".parse::<u16>().context("parsing the port");
/// let err = r.context("loading settings").unwrap_err();
///
/// let messages: Vec<String> = chain(&err).map(|e| e.to_string()).collect();
/// assert_eq!(messages[0], "loading settings");
/// assert_eq!(messages[1], "parsing the port");
/// assert_eq!(messages.len(), 3); // … and the ParseIntError underneath
/// ```
pub trait Context<T> {
    /// Wraps the error with `context`.
    fn context<C: fmt::Display>(self, context: C) -> Result<T, ContextError>;

    /// Wraps the error with a lazily built context message; `f` runs only on
    /// the error path.
    fn with_context<C, F>(self, f: F) -> Result<T, ContextError>
    where
        C: fmt::Display,
        F: FnOnce() -> C;
}

impl<T, E> Context<T> for Result<T, E>
where
    E: Into<BoxError>,
{
    fn context<C: fmt::Display>(self, context: C) -> Result<T, ContextError> {
        self.map_err(|e| ContextError::new(context, e))
    }

    fn with_context<C, F>(self, f: F) -> Result<T, ContextError>
    where
        C: fmt::Display,
        F: FnOnce() -> C,
    {
        self.map_err(|e| ContextError::new(f(), e))
    }
}

impl<T> Context<T> for Option<T> {
    fn context<C: fmt::Display>(self, context: C) -> Result<T, ContextError> {
        self.ok_or_else(|| ContextError::msg(context))
    }

    fn with_context<C, F>(self, f: F) -> Result<T, ContextError>
    where
        C: fmt::Display,
        F: FnOnce() -> C,
    {
        self.ok_or_else(|| ContextError::msg(f()))
    }
}

/// Iterates over `err` and then every error reachable through
/// [`Error::source`], outermost first.
pub fn chain<'a>(err: &'a (dyn Error + 'static)) -> Chain<'a> {
    Chain { next: Some(err) }
}

/// Iterator over an error and its `source()` chain; see [`chain`].
#[derive(Debug, Clone)]
pub struct Chain<'a> {
    next: Option<&'a (dyn Error + 'static)>,
}

impl<'a> Iterator for Chain<'a> {
    type Item = &'a (dyn Error + 'static);

    fn next(&mut self) -> Option<Self::Item> {
        let cur = self.next?;
        self.next = cur.source();
        Some(cur)
    }
}

// ---- classification --------------------------------------------------------

/// A coarse, transport-neutral category for an error.
///
/// The categories are deliberately few: enough to decide whether to retry,
/// whether to blame the caller, and which HTTP status family fits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The input was malformed or failed validation.
    InvalidInput,
    /// The caller is not authenticated (missing, expired or invalid
    /// credentials).
    Unauthenticated,
    /// The caller is authenticated but not allowed to do this.
    PermissionDenied,
    /// The thing asked for does not exist.
    NotFound,
    /// The request conflicts with the current state (already exists, already
    /// started, …).
    Conflict,
    /// The operation, format or capability is not supported.
    Unsupported,
    /// The caller is being throttled; retry later.
    RateLimited,
    /// The operation did not finish in time.
    Timeout,
    /// A dependency is temporarily unreachable or overloaded.
    Unavailable,
    /// An unexpected failure inside the system (a bug, a broken invariant).
    Internal,
    /// Not classified.
    Unknown,
}

impl ErrorKind {
    /// Whether retrying the same operation later can reasonably succeed
    /// ([`RateLimited`](ErrorKind::RateLimited), [`Timeout`](ErrorKind::Timeout)
    /// and [`Unavailable`](ErrorKind::Unavailable)).
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            ErrorKind::RateLimited | ErrorKind::Timeout | ErrorKind::Unavailable
        )
    }

    /// Whether the error was caused by what the caller sent rather than by the
    /// system: bad input, missing or insufficient credentials, a missing
    /// resource, a conflict, or an unsupported request.
    pub fn is_user_error(self) -> bool {
        matches!(
            self,
            ErrorKind::InvalidInput
                | ErrorKind::Unauthenticated
                | ErrorKind::PermissionDenied
                | ErrorKind::NotFound
                | ErrorKind::Conflict
                | ErrorKind::Unsupported
        )
    }

    /// The conventional HTTP status code for this kind (for example
    /// `InvalidInput` → `400`, `Unavailable` → `503`, `Unknown` → `500`).
    pub fn http_status(self) -> u16 {
        match self {
            ErrorKind::InvalidInput => 400,
            ErrorKind::Unauthenticated => 401,
            ErrorKind::PermissionDenied => 403,
            ErrorKind::NotFound => 404,
            ErrorKind::Conflict => 409,
            ErrorKind::Unsupported => 501,
            ErrorKind::RateLimited => 429,
            ErrorKind::Timeout => 504,
            ErrorKind::Unavailable => 503,
            ErrorKind::Internal | ErrorKind::Unknown => 500,
        }
    }

    /// A stable, lowercase name for logs and metrics labels.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::InvalidInput => "invalid_input",
            ErrorKind::Unauthenticated => "unauthenticated",
            ErrorKind::PermissionDenied => "permission_denied",
            ErrorKind::NotFound => "not_found",
            ErrorKind::Conflict => "conflict",
            ErrorKind::Unsupported => "unsupported",
            ErrorKind::RateLimited => "rate_limited",
            ErrorKind::Timeout => "timeout",
            ErrorKind::Unavailable => "unavailable",
            ErrorKind::Internal => "internal",
            ErrorKind::Unknown => "unknown",
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Classifies an error into an [`ErrorKind`].
///
/// Implement [`kind`](Classify::kind); the other methods follow from it (and
/// can be overridden when one variant needs a different answer).
///
/// ```
/// use ferroly::errutils::{Classify, ErrorKind};
///
/// #[derive(Debug)]
/// enum StoreError { Missing, Busy }
///
/// impl Classify for StoreError {
///     fn kind(&self) -> ErrorKind {
///         match self {
///             StoreError::Missing => ErrorKind::NotFound,
///             StoreError::Busy => ErrorKind::Unavailable,
///         }
///     }
/// }
///
/// assert!(StoreError::Busy.is_retryable());
/// assert!(StoreError::Missing.is_user_error());
/// assert_eq!(StoreError::Missing.kind().http_status(), 404);
/// ```
pub trait Classify {
    /// The category this error falls into.
    fn kind(&self) -> ErrorKind;

    /// Whether retrying later can reasonably succeed.
    fn is_retryable(&self) -> bool {
        self.kind().is_retryable()
    }

    /// Whether the caller (rather than the system) caused the error.
    fn is_user_error(&self) -> bool {
        self.kind().is_user_error()
    }
}

impl Classify for std::io::Error {
    fn kind(&self) -> ErrorKind {
        use std::io::ErrorKind as Io;
        match std::io::Error::kind(self) {
            Io::NotFound => ErrorKind::NotFound,
            Io::PermissionDenied => ErrorKind::PermissionDenied,
            Io::AlreadyExists => ErrorKind::Conflict,
            Io::InvalidInput | Io::InvalidData => ErrorKind::InvalidInput,
            Io::Unsupported => ErrorKind::Unsupported,
            Io::TimedOut | Io::WouldBlock => ErrorKind::Timeout,
            Io::ConnectionRefused
            | Io::ConnectionReset
            | Io::ConnectionAborted
            | Io::NotConnected
            | Io::BrokenPipe
            | Io::AddrInUse
            | Io::AddrNotAvailable
            | Io::Interrupted
            | Io::UnexpectedEof => ErrorKind::Unavailable,
            _ => ErrorKind::Unknown,
        }
    }
}

impl Classify for ContextError {
    /// Looks through the context layers: the kind of the first
    /// [`std::io::Error`] found in the `source()` chain, otherwise
    /// [`ErrorKind::Unknown`] (a boxed source cannot be asked for its kind).
    fn kind(&self) -> ErrorKind {
        chain(self)
            .find_map(|e| e.downcast_ref::<std::io::Error>())
            .map_or(ErrorKind::Unknown, Classify::kind)
    }
}

impl Classify for MultiError {
    /// The kind of the contained errors when it can be determined and they all
    /// agree, otherwise [`ErrorKind::Unknown`]. Only [`std::io::Error`] and
    /// [`ContextError`] elements can be inspected (boxed errors are opaque).
    fn kind(&self) -> ErrorKind {
        let mut kinds = self.errors.iter().map(|e| {
            if let Some(io) = e.downcast_ref::<std::io::Error>() {
                Classify::kind(io)
            } else if let Some(ctx) = e.downcast_ref::<ContextError>() {
                ctx.kind()
            } else {
                ErrorKind::Unknown
            }
        });
        match kinds.next() {
            Some(first) if kinds.all(|k| k == first) => first,
            _ => ErrorKind::Unknown,
        }
    }
}

// ---- module error types ------------------------------------------------------
//
// The module error enums classify themselves, so a caller holding an
// `HttpError` (say) need not convert it to `ferroly::Error` first; the root
// `Error` delegates to these. Variants are matched with `{ .. }` and a trailing
// wildcard so a module adding variants or fields never breaks the mapping (a
// new variant classifies as `Unknown` until it is listed here).

/// The kind of the `std::io::Error` directly behind `e`, when that is what its
/// `source()` is.
#[cfg(any(feature = "fsutils", feature = "http", feature = "vfs"))]
pub(crate) fn io_source_kind(e: &(dyn Error + 'static)) -> Option<ErrorKind> {
    e.source()
        .and_then(|s| s.downcast_ref::<std::io::Error>())
        .map(Classify::kind)
}

/// The conventional kind of a remote HTTP status (`401` → `Unauthenticated`,
/// `429` → `RateLimited`, `5xx` → `Unavailable`, other `4xx` → `InvalidInput`).
#[cfg(any(feature = "genai", feature = "ws"))]
fn status_kind(status: u16) -> ErrorKind {
    match status {
        401 => ErrorKind::Unauthenticated,
        403 => ErrorKind::PermissionDenied,
        404 => ErrorKind::NotFound,
        408 => ErrorKind::Timeout,
        409 => ErrorKind::Conflict,
        429 => ErrorKind::RateLimited,
        400..=499 => ErrorKind::InvalidInput,
        500..=599 => ErrorKind::Unavailable,
        _ => ErrorKind::Unknown,
    }
}

#[cfg(feature = "http")]
#[allow(unknown_lints, clippy::unneeded_struct_pattern)]
impl Classify for ferroly::http::HttpError {
    /// `InvalidInput` for a bad URL, `Timeout` for a request or connect
    /// timeout, the I/O kind for an I/O failure, `Internal` for a panicked
    /// handler. An HTTP/2 failure maps by its [`Http2Cause`]: a timer is
    /// `Timeout`, an I/O failure its I/O kind, a protocol violation `Internal`,
    /// and a reset, refusal, GOAWAY, close or local limit `Unavailable`. A
    /// reset this side made for its own reasons is `Unavailable` when it
    /// refused the stream (`REFUSED_STREAM`), `Internal` when a handler
    /// failed (`INTERNAL_ERROR`), and `Unknown` when it just gave the
    /// stream up (`CANCEL`, `NO_ERROR`). Otherwise `Unknown`.
    ///
    /// [`Http2Cause`]: ferroly::http::Http2Cause
    fn kind(&self) -> ErrorKind {
        use ferroly::http::HttpError;
        #[allow(unreachable_patterns)]
        match self {
            HttpError::InvalidUrl { .. } => ErrorKind::InvalidInput,
            e if e.is_timeout() => ErrorKind::Timeout,
            HttpError::HandlerPanic { .. } => ErrorKind::Internal,
            HttpError::Http2(e) => {
                use ferroly::http::Http2Cause as C;
                match e.cause() {
                    C::Timeout => ErrorKind::Timeout,
                    C::Io(io) => Classify::kind(&std::io::Error::from(io)),
                    C::Protocol(_) => ErrorKind::Internal,
                    C::LocalReset(code) => match code {
                        ferroly::http::Http2ErrorCode::REFUSED_STREAM => ErrorKind::Unavailable,
                        ferroly::http::Http2ErrorCode::INTERNAL_ERROR => ErrorKind::Internal,
                        _ => ErrorKind::Unknown,
                    },
                    _ => ErrorKind::Unavailable,
                }
            }
            other => io_source_kind(other).unwrap_or(ErrorKind::Unknown),
        }
    }
}

#[cfg(feature = "vfs")]
#[allow(unknown_lints, clippy::unneeded_struct_pattern)]
impl Classify for ferroly::vfs::VfsError {
    /// By variant: `NotFound`, `PermissionDenied`, `InvalidInput` (not a
    /// directory / is a directory), `Unsupported`, `Conflict` (already exists,
    /// precondition failed), `Unavailable`, `Timeout`, `Unauthenticated`; the
    /// I/O kind for an I/O failure.
    fn kind(&self) -> ErrorKind {
        use ferroly::vfs::VfsError as V;
        #[allow(unreachable_patterns)]
        match self {
            V::NotFound { .. } => ErrorKind::NotFound,
            V::Permission { .. } => ErrorKind::PermissionDenied,
            V::NotDir { .. } | V::IsDir { .. } => ErrorKind::InvalidInput,
            V::Unsupported { .. } => ErrorKind::Unsupported,
            V::AlreadyExists { .. } | V::PreconditionFailed { .. } => ErrorKind::Conflict,
            V::Unavailable { .. } => ErrorKind::Unavailable,
            V::Timeout { .. } => ErrorKind::Timeout,
            V::Unauthenticated { .. } => ErrorKind::Unauthenticated,
            other => io_source_kind(other).unwrap_or(ErrorKind::Unknown),
        }
    }
}

#[cfg(feature = "genai")]
#[allow(unknown_lints, clippy::unneeded_struct_pattern)]
impl Classify for ferroly::genai::GenAiError {
    /// By variant, and by HTTP status for provider responses (`401` →
    /// `Unauthenticated`, `429` → `RateLimited`, `5xx` → `Unavailable`, other
    /// `4xx` → `InvalidInput`).
    fn kind(&self) -> ErrorKind {
        use ferroly::genai::GenAiError as G;
        #[allow(unreachable_patterns)]
        match self {
            G::Template { .. } | G::Config { .. } => ErrorKind::InvalidInput,
            G::TemplateNotFound { .. } => ErrorKind::NotFound,
            G::Unsupported { .. } => ErrorKind::Unsupported,
            G::Transport { .. } | G::Connect { .. } => ErrorKind::Unavailable,
            G::Api { status, .. } => status_kind(*status),
            G::ResponseParse { .. } => ErrorKind::Internal,
            G::Timeout { .. } => ErrorKind::Timeout,
            G::RateLimited { .. } => ErrorKind::RateLimited,
            G::Auth { status, .. } => match *status {
                403 => ErrorKind::PermissionDenied,
                _ => ErrorKind::Unauthenticated,
            },
            G::ContentFilter { .. } | G::ContextLengthExceeded { .. } => ErrorKind::InvalidInput,
            G::Overloaded { .. } => ErrorKind::Unavailable,
            _ => ErrorKind::Unknown,
        }
    }
}

#[cfg(feature = "auth")]
#[allow(unknown_lints, clippy::unneeded_struct_pattern)]
impl Classify for ferroly::auth::JwtError {
    /// `Unauthenticated` for a rejected token (malformed, bad signature,
    /// expired, a claim or header not accepted, no matching key, replayed).
    /// The exceptions are not the caller's fault: a key-set fetch failure is
    /// `Unavailable`, a secret that is too short is `InvalidInput` (a
    /// configuration error), an unusable key or JWK is `Internal`, and a
    /// missing crypto provider is `Unsupported`. A variant added later is
    /// `Unknown` until it is classified here.
    fn kind(&self) -> ErrorKind {
        use ferroly::auth::JwtError as J;
        #[allow(unreachable_patterns)]
        match self {
            J::Malformed { .. }
            | J::UnsupportedAlg { .. }
            | J::InvalidSignature { .. }
            | J::Expired { .. }
            | J::NotYetValid { .. }
            | J::MissingClaim { .. }
            | J::InvalidAudience { .. }
            | J::InvalidIssuer { .. }
            | J::InvalidSubject { .. }
            | J::IssuedInFuture { .. }
            | J::TooOld { .. }
            | J::InvalidType { .. }
            | J::UnsupportedCritical { .. }
            | J::UnknownKey { .. }
            | J::Replayed { .. } => ErrorKind::Unauthenticated,
            J::KeyFetch { .. } => ErrorKind::Unavailable,
            J::WeakSecret { .. } => ErrorKind::InvalidInput,
            J::InvalidKey { .. } => ErrorKind::Internal,
            J::NoCryptoProvider { .. } => ErrorKind::Unsupported,
            _ => ErrorKind::Unknown,
        }
    }
}

#[cfg(feature = "ws")]
#[allow(unknown_lints, clippy::unneeded_struct_pattern)]
impl Classify for ferroly::ws::WsError {
    /// `Timeout` for a timeout, the conventional kind of the HTTP status for a
    /// rejected upgrade, `Internal` for a protocol error (the application
    /// asked for something RFC 6455 forbids, so retrying cannot help), and
    /// `Unavailable` when the connection failed, closed or its queue is
    /// full. A variant added later is `Unknown` until it is classified here.
    fn kind(&self) -> ErrorKind {
        use ferroly::ws::WsError as W;
        #[allow(unreachable_patterns)]
        match self {
            W::Timeout { .. } => ErrorKind::Timeout,
            W::Rejected { status, .. } => match status_kind(*status) {
                ErrorKind::Unknown => ErrorKind::Unavailable,
                kind => kind,
            },
            W::Protocol { .. } => ErrorKind::Internal,
            W::Connect { .. }
            | W::Send { .. }
            | W::Closed { .. }
            | W::QueueFull { .. }
            | W::Transport { .. } => ErrorKind::Unavailable,
            _ => ErrorKind::Unknown,
        }
    }
}

#[cfg(feature = "messaging")]
#[allow(unknown_lints, clippy::unneeded_struct_pattern)]
impl Classify for ferroly::messaging::MessagingError {
    /// `Unavailable` for a full buffer, a send timeout, a closed provider or a
    /// transient backend failure or refusal; `NotFound` for no listeners;
    /// `Timeout` for a request timeout; `InvalidInput` for a bad address or
    /// options; `Unsupported` for an unsupported operation; otherwise
    /// `Unknown`.
    fn kind(&self) -> ErrorKind {
        use ferroly::messaging::MessagingError as M;
        #[allow(unreachable_patterns)]
        match self {
            M::BufferFull { .. } | M::SendTimeout { .. } | M::Closed { .. } => {
                ErrorKind::Unavailable
            }
            M::NoListeners { .. } => ErrorKind::NotFound,
            M::RequestTimeout { .. } => ErrorKind::Timeout,
            M::InvalidAddress { .. } | M::InvalidOptions { .. } => ErrorKind::InvalidInput,
            M::Unsupported { .. } => ErrorKind::Unsupported,
            M::Backend {
                transient: true, ..
            }
            | M::Refused {
                transient: true, ..
            } => ErrorKind::Unavailable,
            _ => ErrorKind::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `JwtError` variant with its kind. The exhaustive `match` (allowed
    /// inside the crate despite `#[non_exhaustive]`) stops compiling when a
    /// variant is added, so the new one has to be classified on purpose.
    #[cfg(feature = "auth")]
    #[test]
    fn every_jwt_error_variant_is_classified() {
        use ferroly::auth::JwtError as J;
        fn expected(e: &J) -> ErrorKind {
            match e {
                J::Malformed
                | J::UnsupportedAlg
                | J::InvalidSignature
                | J::Expired
                | J::NotYetValid
                | J::MissingClaim(_)
                | J::InvalidAudience
                | J::InvalidIssuer
                | J::InvalidSubject
                | J::IssuedInFuture
                | J::TooOld
                | J::InvalidType
                | J::UnsupportedCritical
                | J::UnknownKey
                | J::Replayed => ErrorKind::Unauthenticated,
                J::KeyFetch(_) => ErrorKind::Unavailable,
                J::WeakSecret => ErrorKind::InvalidInput,
                J::InvalidKey => ErrorKind::Internal,
                J::NoCryptoProvider => ErrorKind::Unsupported,
            }
        }
        let all = [
            J::Malformed,
            J::UnsupportedAlg,
            J::InvalidSignature,
            J::Expired,
            J::NotYetValid,
            J::WeakSecret,
            J::InvalidKey,
            J::MissingClaim("sub".into()),
            J::InvalidAudience,
            J::InvalidIssuer,
            J::InvalidSubject,
            J::IssuedInFuture,
            J::TooOld,
            J::InvalidType,
            J::UnsupportedCritical,
            J::UnknownKey,
            J::KeyFetch("down".into()),
            J::NoCryptoProvider,
            J::Replayed,
        ];
        for e in &all {
            assert_eq!(e.kind(), expected(e), "{e:?}");
        }
        // A local key problem is neither the caller's fault nor retryable.
        assert!(!J::InvalidKey.is_user_error());
        assert!(!J::InvalidKey.is_retryable());
    }

    /// Every `WsError` variant with its kind; exhaustive like the JWT test.
    #[cfg(feature = "ws")]
    #[test]
    fn every_ws_error_variant_is_classified() {
        use ferroly::ws::WsError as W;
        fn expected(e: &W) -> ErrorKind {
            match e {
                W::Timeout(_) => ErrorKind::Timeout,
                W::Rejected { status } => match *status {
                    401 => ErrorKind::Unauthenticated,
                    403 => ErrorKind::PermissionDenied,
                    _ => ErrorKind::Unavailable,
                },
                W::Protocol(_) => ErrorKind::Internal,
                W::Connect(_) | W::Send(_) | W::Closed | W::QueueFull | W::Transport(_) => {
                    ErrorKind::Unavailable
                }
            }
        }
        let all = [
            W::Connect("refused".into()),
            W::Rejected { status: 401 },
            W::Rejected { status: 403 },
            W::Rejected { status: 503 },
            W::Timeout("connect".into()),
            W::Send("x".into()),
            W::Closed,
            W::QueueFull,
            W::Protocol("close code 1005 may not be sent".into()),
            W::Transport("reset".into()),
        ];
        for e in &all {
            assert_eq!(e.kind(), expected(e), "{e:?}");
        }
        // A protocol violation by the application is not worth retrying.
        assert!(!W::Protocol("x".into()).is_retryable());
    }

    #[test]
    fn empty_multi_error_reports_no_errors() {
        let errs = MultiError::new();
        assert!(errs.is_empty());
        assert_eq!(errs.len(), 0);
        assert!(errs.into_result().is_ok());
    }

    #[test]
    fn collects_and_displays_errors() {
        let mut errs = MultiError::new();
        errs.push(std::io::Error::other("disk full"));
        errs.push_msg("validation failed");

        assert!(!errs.is_empty());
        assert_eq!(errs.len(), 2);

        let text = errs.to_string();
        assert!(text.contains("2 error(s)"));
        assert!(text.contains("disk full"));
        assert!(text.contains("validation failed"));
    }

    #[test]
    fn into_result_errors_when_non_empty() {
        let mut errs = MultiError::new();
        errs.push_msg("boom");
        assert!(errs.into_result().is_err());
    }

    #[test]
    fn context_display_plain_and_alternate() {
        let inner = std::io::Error::other("disk full");
        let err = ContextError::new("saving", inner);
        assert_eq!(format!("{err}"), "saving");
        assert_eq!(format!("{err:#}"), "saving: disk full");
        assert_eq!(err.root_cause().to_string(), "disk full");
    }
}

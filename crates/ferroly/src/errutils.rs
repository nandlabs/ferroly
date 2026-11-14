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

#[cfg(test)]
mod tests {
    use super::*;

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

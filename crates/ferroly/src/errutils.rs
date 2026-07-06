//! Error aggregation utilities for the Ferroly toolkit.
//!
//! Custom formatted errors and multi-error aggregation. The centerpiece is
//! [`MultiError`], which collects several errors into one value that still
//! implements [`std::error::Error`].
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

#![deny(missing_docs)]

use std::error::Error;
use std::fmt;

/// A boxed, thread-safe error — the element type held by [`MultiError`].
pub type BoxError = Box<dyn Error + Send + Sync + 'static>;

/// Aggregates multiple errors into a single error value.
///
/// Push errors as they occur, then return the aggregate. The `Display`
/// implementation joins each contained error's message on its own line.
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

impl Error for MultiError {}

impl Extend<BoxError> for MultiError {
    fn extend<T: IntoIterator<Item = BoxError>>(&mut self, iter: T) {
        self.errors.extend(iter);
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
}

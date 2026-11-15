//! HTTP/2 error codes (RFC 9113 §7) and the structured errors the engine
//! hands to stream consumers.

use std::borrow::Cow;

pub(crate) use crate::http::Http2ErrorCode as Reason;
use crate::http::{Http2Cause, Http2Error, HttpError};

/// An [`HttpError::Http2`] with `cause` and `detail`.
pub(crate) fn h2_error(cause: Http2Cause, detail: impl Into<Cow<'static, str>>) -> HttpError {
    HttpError::Http2(Http2Error::new(cause, detail))
}

/// The stream or connection is gone.
pub(crate) fn closed() -> HttpError {
    h2_error(Http2Cause::ConnectionClosed, "stream is closed")
}

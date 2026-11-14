//! The unified error type for encoding, decoding, and codec lookup.
//!
//! Hand-written (not via the derive) to keep the encoding core free of
//! bootstrapping concerns.

use std::fmt;

/// Errors raised across parsing, value conversion, and content-type resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CodecError {
    /// No codec is registered for the requested content type.
    UnsupportedContentType(String),
    /// A syntax error while parsing an input format.
    Parse(String),
    /// A value of the wrong shape was encountered (e.g. expected an object).
    Expected(&'static str),
    /// A required struct field was absent.
    MissingField(String),
    /// An enum string did not match any known variant.
    UnknownVariant(String),
    /// A numeric value did not fit the target type.
    OutOfRange(&'static str),
    /// A general message.
    Message(String),
    /// The input exceeded a configured [`Limits`](super::Limits) budget
    /// (nesting depth, input size, or entry count).
    LimitExceeded(String),
    /// A value cannot be represented in the target format (raised by the
    /// fallible `try_encode` entry points).
    Unrepresentable(String),
}

impl CodecError {
    /// A parse/syntax error.
    pub fn parse(msg: impl Into<String>) -> Self {
        CodecError::Parse(msg.into())
    }
    /// The value had the wrong shape.
    pub fn expected(what: &'static str) -> Self {
        CodecError::Expected(what)
    }
    /// A required field was missing.
    pub fn missing_field(name: impl Into<String>) -> Self {
        CodecError::MissingField(name.into())
    }
    /// An unknown enum variant string.
    pub fn unknown_variant(name: impl Into<String>) -> Self {
        CodecError::UnknownVariant(name.into())
    }
    /// A numeric out-of-range conversion for the named type.
    pub fn out_of_range(ty: &'static str) -> Self {
        CodecError::OutOfRange(ty)
    }
    /// A value the target format cannot represent.
    pub fn unrepresentable(msg: impl Into<String>) -> Self {
        CodecError::Unrepresentable(msg.into())
    }
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::UnsupportedContentType(ct) => {
                write!(f, "no codec registered for content type: {ct}")
            }
            CodecError::Parse(m) => write!(f, "parse error: {m}"),
            CodecError::Expected(w) => write!(f, "expected {w}"),
            CodecError::MissingField(n) => write!(f, "missing field: {n}"),
            CodecError::UnknownVariant(n) => write!(f, "unknown variant: {n}"),
            CodecError::OutOfRange(t) => write!(f, "value out of range for {t}"),
            CodecError::Message(m) => write!(f, "{m}"),
            CodecError::LimitExceeded(m) => write!(f, "limit exceeded: {m}"),
            CodecError::Unrepresentable(m) => write!(f, "cannot encode: {m}"),
        }
    }
}

impl std::error::Error for CodecError {}

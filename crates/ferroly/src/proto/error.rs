//! The error type for decoding protobuf wire data.

use ferroly_derive::FerrolyError;

/// Why a protobuf message could not be decoded.
///
/// Every variant describes malformed or over-budget input; decoding never
/// panics. Encoding cannot fail.
#[derive(Debug, Clone, PartialEq, Eq, FerrolyError)]
#[non_exhaustive]
pub enum ProtoError {
    /// The input ended in the middle of a key or value.
    #[error("truncated protobuf input")]
    Truncated,
    /// A varint was longer than ten bytes or did not fit in 64 bits.
    #[error("varint is longer than 10 bytes or overflows 64 bits")]
    VarintOverflow,
    /// A field key carried field number 0 or one above 2²⁹ − 1.
    #[error("invalid field number {0}")]
    InvalidTag(u64),
    /// A field key carried the unassigned wire type 6 or 7.
    #[error("invalid wire type {bits} for field {tag}")]
    InvalidWireType {
        /// The field number.
        tag: u32,
        /// The low three bits of the key.
        bits: u8,
    },
    /// A length prefix pointed past the end of the enclosing message.
    #[error("length {len} exceeds the {remaining} bytes left")]
    LengthOutOfBounds {
        /// The declared length.
        len: u64,
        /// The bytes left in the enclosing message.
        remaining: usize,
    },
    /// Embedded messages (or groups) nested deeper than
    /// [`Limits::max_depth`](super::Limits::max_depth).
    #[error("messages nested deeper than {0} levels")]
    DepthExceeded(usize),
    /// The input exceeded a [`Limits`](super::Limits) budget (size or entry
    /// count).
    #[error("limit exceeded: {0}")]
    LimitExceeded(String),
    /// A `string` field was not valid UTF-8.
    #[error("string field is not valid UTF-8")]
    InvalidUtf8,
    /// An end-group key with no matching start-group key.
    #[error("unexpected end-group key for field {0}")]
    UnexpectedEndGroup(u32),
    /// A proto2 `required` field was absent.
    #[error("missing required field {message}.{field}")]
    MissingRequired {
        /// The Rust name of the message type.
        message: &'static str,
        /// The Rust name of the field.
        field: &'static str,
    },
    /// Structurally invalid input not covered by another variant.
    #[error("malformed protobuf input: {0}")]
    Malformed(&'static str),
}

impl ProtoError {
    /// A missing proto2 `required` field (used by derived code).
    pub fn missing_required(message: &'static str, field: &'static str) -> Self {
        ProtoError::MissingRequired { message, field }
    }
}

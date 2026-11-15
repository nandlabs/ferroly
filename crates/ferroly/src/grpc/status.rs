//! [`Status`] and its [`Code`]: the outcome of every gRPC call, carried in
//! the `grpc-status`, `grpc-message` and `grpc-status-details-bin` trailers.

use std::fmt;

use super::metadata::Metadata;
use tokio::time::Instant;

use crate::http::{HeaderMap, HttpError};

/// The standard gRPC status codes.
///
/// Numbers outside the standard range decode as [`Code::Unknown`], as the
/// gRPC specification requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Code {
    /// `0` — not an error.
    Ok,
    /// `1` — the call was cancelled, typically by the caller.
    Cancelled,
    /// `2` — an unknown error (also used for unrecognised status numbers).
    Unknown,
    /// `3` — the client sent an invalid argument.
    InvalidArgument,
    /// `4` — the deadline expired before the call completed.
    DeadlineExceeded,
    /// `5` — a requested entity was not found.
    NotFound,
    /// `6` — the entity the client tried to create already exists.
    AlreadyExists,
    /// `7` — the caller may not perform the operation.
    PermissionDenied,
    /// `8` — a resource (quota, message size, …) is exhausted.
    ResourceExhausted,
    /// `9` — the system is not in a state the operation requires.
    FailedPrecondition,
    /// `10` — the operation was aborted (a concurrency conflict, say).
    Aborted,
    /// `11` — the operation went past the valid range.
    OutOfRange,
    /// `12` — the operation is not implemented (or the method is unknown).
    Unimplemented,
    /// `13` — an internal error: an invariant the system relies on broke.
    Internal,
    /// `14` — the service is currently unavailable; retrying may help.
    Unavailable,
    /// `15` — unrecoverable data loss or corruption.
    DataLoss,
    /// `16` — the request lacks valid authentication credentials.
    Unauthenticated,
}

const CODES: [Code; 17] = [
    Code::Ok,
    Code::Cancelled,
    Code::Unknown,
    Code::InvalidArgument,
    Code::DeadlineExceeded,
    Code::NotFound,
    Code::AlreadyExists,
    Code::PermissionDenied,
    Code::ResourceExhausted,
    Code::FailedPrecondition,
    Code::Aborted,
    Code::OutOfRange,
    Code::Unimplemented,
    Code::Internal,
    Code::Unavailable,
    Code::DataLoss,
    Code::Unauthenticated,
];

impl Code {
    /// The code for a status number; anything unrecognised is
    /// [`Code::Unknown`].
    pub fn from_i32(value: i32) -> Code {
        usize::try_from(value)
            .ok()
            .and_then(|i| CODES.get(i).copied())
            .unwrap_or(Code::Unknown)
    }

    /// The status number (`grpc-status` value).
    pub fn as_i32(self) -> i32 {
        CODES.iter().position(|c| *c == self).unwrap_or(2) as i32
    }

    /// The canonical upper-case name (`"NOT_FOUND"`).
    pub fn name(self) -> &'static str {
        match self {
            Code::Ok => "OK",
            Code::Cancelled => "CANCELLED",
            Code::Unknown => "UNKNOWN",
            Code::InvalidArgument => "INVALID_ARGUMENT",
            Code::DeadlineExceeded => "DEADLINE_EXCEEDED",
            Code::NotFound => "NOT_FOUND",
            Code::AlreadyExists => "ALREADY_EXISTS",
            Code::PermissionDenied => "PERMISSION_DENIED",
            Code::ResourceExhausted => "RESOURCE_EXHAUSTED",
            Code::FailedPrecondition => "FAILED_PRECONDITION",
            Code::Aborted => "ABORTED",
            Code::OutOfRange => "OUT_OF_RANGE",
            Code::Unimplemented => "UNIMPLEMENTED",
            Code::Internal => "INTERNAL",
            Code::Unavailable => "UNAVAILABLE",
            Code::DataLoss => "DATA_LOSS",
            Code::Unauthenticated => "UNAUTHENTICATED",
        }
    }

    /// The code a client reports for a response that is not gRPC, by its
    /// HTTP status — the gRPC specification's HTTP-to-gRPC mapping: `400` is
    /// `INTERNAL`, `401` `UNAUTHENTICATED`, `403` `PERMISSION_DENIED`, `404`
    /// `UNIMPLEMENTED`, `429`/`502`/`503`/`504` `UNAVAILABLE`, everything else
    /// `UNKNOWN`.
    pub fn from_http_status(status: u16) -> Code {
        match status {
            400 => Code::Internal,
            401 => Code::Unauthenticated,
            403 => Code::PermissionDenied,
            404 => Code::Unimplemented,
            429 | 502 | 503 | 504 => Code::Unavailable,
            _ => Code::Unknown,
        }
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The outcome of a gRPC call: a [`Code`], a human-readable message, optional
/// binary details (`grpc-status-details-bin`, typically an encoded
/// `google.rpc.Status`) and the trailing metadata sent with it.
///
/// Every fallible gRPC operation in this module returns `Status` as its
/// error, and handlers return it to fail a call.
///
/// ```
/// use ferroly::grpc::{Code, Status};
///
/// let status = Status::not_found("no such user: 7");
/// assert_eq!(status.code(), Code::NotFound);
/// assert_eq!(status.message(), "no such user: 7");
/// assert_eq!(status.to_string(), "NOT_FOUND: no such user: 7");
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct Status {
    code: Code,
    message: String,
    details: Option<Vec<u8>>,
    metadata: Metadata,
}

macro_rules! constructors {
    ($($(#[$doc:meta])* $name:ident => $code:ident;)*) => {
        $(
            $(#[$doc])*
            pub fn $name(message: impl Into<String>) -> Status {
                Status::new(Code::$code, message)
            }
        )*
    };
}

impl Status {
    /// A status with `code` and `message`.
    pub fn new(code: Code, message: impl Into<String>) -> Status {
        Status {
            code,
            message: message.into(),
            details: None,
            metadata: Metadata::new(),
        }
    }

    /// The `OK` status (no message).
    pub fn ok() -> Status {
        Status::new(Code::Ok, "")
    }

    constructors! {
        /// A `CANCELLED` status.
        cancelled => Cancelled;
        /// An `UNKNOWN` status.
        unknown => Unknown;
        /// An `INVALID_ARGUMENT` status.
        invalid_argument => InvalidArgument;
        /// A `DEADLINE_EXCEEDED` status.
        deadline_exceeded => DeadlineExceeded;
        /// A `NOT_FOUND` status.
        not_found => NotFound;
        /// An `ALREADY_EXISTS` status.
        already_exists => AlreadyExists;
        /// A `PERMISSION_DENIED` status.
        permission_denied => PermissionDenied;
        /// A `RESOURCE_EXHAUSTED` status.
        resource_exhausted => ResourceExhausted;
        /// A `FAILED_PRECONDITION` status.
        failed_precondition => FailedPrecondition;
        /// An `ABORTED` status.
        aborted => Aborted;
        /// An `OUT_OF_RANGE` status.
        out_of_range => OutOfRange;
        /// An `UNIMPLEMENTED` status.
        unimplemented => Unimplemented;
        /// An `INTERNAL` status.
        internal => Internal;
        /// An `UNAVAILABLE` status.
        unavailable => Unavailable;
        /// A `DATA_LOSS` status.
        data_loss => DataLoss;
        /// An `UNAUTHENTICATED` status.
        unauthenticated => Unauthenticated;
    }

    /// The status code.
    pub fn code(&self) -> Code {
        self.code
    }

    /// Whether the code is [`Code::Ok`].
    pub fn is_ok(&self) -> bool {
        self.code == Code::Ok
    }

    /// The message (empty when there is none).
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The binary details (`grpc-status-details-bin`), if any.
    pub fn details(&self) -> Option<&[u8]> {
        self.details.as_deref()
    }

    /// Returns the status with binary details attached. They are sent as
    /// `grpc-status-details-bin`; by convention an encoded
    /// `google.rpc.Status` message, but any bytes are carried unchanged.
    #[must_use]
    pub fn with_details(mut self, details: impl Into<Vec<u8>>) -> Status {
        self.details = Some(details.into());
        self
    }

    /// The trailing metadata sent (server) or received (client) with this
    /// status.
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    /// Mutable access to the trailing metadata.
    pub fn metadata_mut(&mut self) -> &mut Metadata {
        &mut self.metadata
    }

    /// Returns the status with `metadata` as its trailing metadata.
    #[must_use]
    pub fn with_metadata(mut self, metadata: Metadata) -> Status {
        self.metadata = metadata;
        self
    }

    /// The status a client reports for a response that is not gRPC: the code
    /// from [`Code::from_http_status`] and a message naming the HTTP status.
    pub fn from_http_status(status: u16) -> Status {
        Status::new(
            Code::from_http_status(status),
            format!("unexpected HTTP status code {status} (the response is not gRPC)"),
        )
    }

    /// Reads a status from a header section (trailers, or the headers of a
    /// trailers-only response): `None` when there is no `grpc-status`. An
    /// unparsable number is `UNKNOWN`; the message is percent-decoded and the
    /// details base64-decoded (undecodable details are dropped). The other
    /// fields become the status's metadata.
    pub(crate) fn from_header_map(headers: &HeaderMap) -> Option<Status> {
        let raw = headers.get("grpc-status")?;
        let code = parse_code(raw);
        let mut status = Status::new(
            code,
            headers
                .get("grpc-message")
                .map(percent_decode)
                .unwrap_or_default(),
        );
        if code == Code::Unknown && parse_number(raw).is_none() && status.message.is_empty() {
            status.message = "invalid grpc-status in the response".into();
        }
        status.details = headers
            .get("grpc-status-details-bin")
            .and_then(|v| super::metadata::decode_bin(v.trim()));
        status.metadata = Metadata::from_headers(headers);
        Some(status)
    }

    /// Writes `grpc-status`, `grpc-message` (when not empty), the details and
    /// the metadata into `headers`.
    pub(crate) fn write_to(&self, headers: &mut HeaderMap) {
        self.metadata.write_to(headers);
        headers.set("grpc-status", self.code.as_i32().to_string());
        if !self.message.is_empty() {
            headers.set("grpc-message", percent_encode(&self.message));
        }
        if let Some(details) = &self.details {
            headers.set(
                "grpc-status-details-bin",
                crate::hash::base64::encode_no_pad(details),
            );
        }
    }

    /// The status for a transport failure seen by the client — the one place
    /// an [`HttpError`] becomes a gRPC [`Code`].
    ///
    /// HTTP/2 failures are mapped on their structured
    /// [`Http2Cause`](crate::http::Http2Cause), following the gRPC
    /// specification's HTTP/2-error table and what grpc-go reports:
    ///
    /// | Failure | Code |
    /// |---|---|
    /// | RST_STREAM `CANCEL` | `CANCELLED` |
    /// | RST_STREAM `REFUSED_STREAM`, a refused stream, GOAWAY, the connection closed or reset, an I/O error, a connection timer (keep-alive, write stall), a local flood limit | `UNAVAILABLE` |
    /// | RST_STREAM `ENHANCE_YOUR_CALM` | `RESOURCE_EXHAUSTED` |
    /// | RST_STREAM `INADEQUATE_SECURITY` | `PERMISSION_DENIED` |
    /// | any other RST_STREAM code, a protocol violation detected locally | `INTERNAL` |
    /// | an HTTP timeout ([`HttpError::Timeout`]) | `DEADLINE_EXCEEDED` |
    /// | connect failure or timeout, TLS, proxy, no HTTP/2 | `UNAVAILABLE` |
    /// | a body cap | `RESOURCE_EXHAUSTED` |
    ///
    /// Whatever the failure, once the call's `deadline` has passed the call
    /// reports `DEADLINE_EXCEEDED` (a peer that resets the stream at its own
    /// copy of the deadline wins the race against the local timer).
    pub(crate) fn from_http_error(error: &HttpError, deadline: Option<Instant>) -> Status {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Status::deadline_exceeded("the deadline passed");
        }
        let (code, message) = http_error_code(error);
        Status::new(code, message)
    }

    /// `self`, or `DEADLINE_EXCEEDED` when `deadline` has passed and this is
    /// a locally detected failure (cancelled or unavailable transport).
    pub(crate) fn after_deadline(self, deadline: Option<Instant>) -> Status {
        let late = deadline.is_some_and(|d| Instant::now() >= d);
        if late && matches!(self.code, Code::Cancelled | Code::Unavailable) {
            Status::deadline_exceeded("the deadline passed")
        } else {
            self
        }
    }
}

/// The code and status message for a transport failure.
fn http_error_code(error: &HttpError) -> (Code, String) {
    match error {
        HttpError::Http2(e) => (http2_code(e.cause()), format!("HTTP/2: {e}")),
        HttpError::Timeout => (Code::DeadlineExceeded, "the HTTP request timed out".into()),
        HttpError::ConnectTimeout => (Code::Unavailable, "connecting timed out".into()),
        HttpError::Io(e) => (Code::Unavailable, format!("connection failed: {e}")),
        HttpError::Tls(e) => (Code::Unavailable, format!("TLS failed: {e}")),
        HttpError::Proxy(e) => (Code::Unavailable, format!("proxy failed: {e}")),
        HttpError::InvalidUrl(e) => (Code::Unavailable, format!("invalid target: {e}")),
        // With HTTP/2 required, what is left of the HTTP/1.1-level protocol
        // errors is "the server does not speak HTTP/2".
        HttpError::Protocol(m) => (Code::Unavailable, m.clone()),
        HttpError::BodyTooLarge(_) => (Code::ResourceExhausted, error.to_string()),
        _ => (Code::Internal, error.to_string()),
    }
}

/// The code for a structured HTTP/2 failure.
fn http2_code(cause: crate::http::Http2Cause) -> Code {
    use crate::http::{Http2Cause as C, Http2ErrorCode as E};
    match cause {
        C::Reset(E::CANCEL) => Code::Cancelled,
        // This side refused a buffered body to stay within its budget.
        C::LocalReset(E::REFUSED_STREAM) => Code::Unavailable,
        C::LocalReset(E::INTERNAL_ERROR) => Code::Internal,
        // This side gave the stream up (its reader went away, or the
        // response was already complete).
        C::LocalReset(_) => Code::Cancelled,
        C::Reset(E::REFUSED_STREAM)
        | C::Refused
        | C::GoAway { .. }
        | C::ConnectionClosed
        | C::Io(_)
        | C::Timeout
        | C::LocalLimit => Code::Unavailable,
        C::Reset(E::ENHANCE_YOUR_CALM) => Code::ResourceExhausted,
        C::Reset(E::INADEQUATE_SECURITY) => Code::PermissionDenied,
        _ => Code::Internal,
    }
}

fn parse_number(raw: &str) -> Option<i32> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 10 || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

fn parse_code(raw: &str) -> Code {
    parse_number(raw).map_or(Code::Unknown, Code::from_i32)
}

impl Default for Status {
    fn default() -> Self {
        Status::ok()
    }
}

impl fmt::Debug for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("Status");
        d.field("code", &self.code).field("message", &self.message);
        if let Some(details) = &self.details {
            d.field("details", &format_args!("{} bytes", details.len()));
        }
        if !self.metadata.is_empty() {
            d.field("metadata", &self.metadata);
        }
        d.finish()
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.message.is_empty() {
            write!(f, "{}", self.code)
        } else {
            write!(f, "{}: {}", self.code, self.message)
        }
    }
}

impl std::error::Error for Status {}

impl From<Code> for Status {
    fn from(code: Code) -> Self {
        Status::new(code, "")
    }
}

/// Percent-encodes a status message for `grpc-message`: every byte of its
/// UTF-8 form outside `0x20..=0x7E`, and `%` itself, becomes `%XX`.
pub(crate) fn percent_encode(message: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(message.len());
    for &b in message.as_bytes() {
        if (0x20..=0x7e).contains(&b) && b != b'%' {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[usize::from(b >> 4)] as char);
            out.push(HEX[usize::from(b & 0xf)] as char);
        }
    }
    out
}

/// Decodes a `grpc-message` value. As the specification asks, it never
/// fails: a `%` not followed by two hex digits is kept as is, and bytes that
/// are not UTF-8 become U+FFFD.
pub(crate) fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip_and_unknown_numbers() {
        for (i, code) in CODES.iter().enumerate() {
            assert_eq!(code.as_i32(), i as i32);
            assert_eq!(Code::from_i32(i as i32), *code);
        }
        assert_eq!(Code::from_i32(17), Code::Unknown);
        assert_eq!(Code::from_i32(-1), Code::Unknown);
        assert_eq!(Code::DeadlineExceeded.to_string(), "DEADLINE_EXCEEDED");
    }

    #[test]
    fn percent_coding() {
        assert_eq!(percent_encode("plain text"), "plain text");
        assert_eq!(percent_encode("100%"), "100%25");
        assert_eq!(percent_encode("a\nb"), "a%0Ab");
        assert_eq!(percent_encode("héllo"), "h%C3%A9llo");
        for s in ["", "100%", "a\r\nb", "héllo ✓", "%%%", "\u{7f}"] {
            assert_eq!(percent_decode(&percent_encode(s)), s);
        }
        // Lenient decoding.
        assert_eq!(percent_decode("50%"), "50%");
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
        assert_eq!(percent_decode("%41%c3%a9"), "Aé");
        assert_eq!(percent_decode("%ff"), "\u{fffd}");
    }

    #[test]
    fn header_round_trip() {
        let mut status = Status::invalid_argument("bad\tvalue 100%").with_details(vec![0, 1, 255]);
        status.metadata_mut().insert("x-reason", "test").unwrap();
        let mut headers = HeaderMap::new();
        status.write_to(&mut headers);
        assert_eq!(headers.get("grpc-status"), Some("3"));
        assert_eq!(headers.get("grpc-message"), Some("bad%09value 100%25"));
        let back = Status::from_header_map(&headers).unwrap();
        assert_eq!(back, status);

        let mut headers = HeaderMap::new();
        headers.set("grpc-status", "banana");
        let s = Status::from_header_map(&headers).unwrap();
        assert_eq!(s.code(), Code::Unknown);
        headers.set("grpc-status", "99");
        assert_eq!(
            Status::from_header_map(&headers).unwrap().code(),
            Code::Unknown
        );
        headers.set("grpc-status", "99999999999999999999");
        assert_eq!(
            Status::from_header_map(&headers).unwrap().code(),
            Code::Unknown
        );
        assert!(Status::from_header_map(&HeaderMap::new()).is_none());
    }

    #[test]
    fn http_mapping() {
        assert_eq!(Code::from_http_status(400), Code::Internal);
        assert_eq!(Code::from_http_status(401), Code::Unauthenticated);
        assert_eq!(Code::from_http_status(403), Code::PermissionDenied);
        assert_eq!(Code::from_http_status(404), Code::Unimplemented);
        for s in [429, 502, 503, 504] {
            assert_eq!(Code::from_http_status(s), Code::Unavailable);
        }
        assert_eq!(Code::from_http_status(500), Code::Unknown);
        assert_eq!(Code::from_http_status(200), Code::Unknown);
        assert_eq!(
            Status::from_http_error(&HttpError::Timeout, None).code(),
            Code::DeadlineExceeded
        );
    }

    #[test]
    fn transport_failures_map_on_the_structured_cause() {
        use crate::http::{Http2Cause as C, Http2Error, Http2ErrorCode as E};
        let h2 = |cause| HttpError::Http2(Http2Error::new(cause, "detail"));
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let cases: Vec<(HttpError, Code)> = vec![
            (h2(C::Reset(E::CANCEL)), Code::Cancelled),
            (h2(C::Reset(E::REFUSED_STREAM)), Code::Unavailable),
            (h2(C::Reset(E::ENHANCE_YOUR_CALM)), Code::ResourceExhausted),
            (h2(C::Reset(E::INADEQUATE_SECURITY)), Code::PermissionDenied),
            (h2(C::Reset(E::NO_ERROR)), Code::Internal),
            (h2(C::Reset(E::PROTOCOL_ERROR)), Code::Internal),
            (h2(C::Reset(E::INTERNAL_ERROR)), Code::Internal),
            (h2(C::Reset(E::FLOW_CONTROL_ERROR)), Code::Internal),
            (h2(C::Reset(E::HTTP_1_1_REQUIRED)), Code::Internal),
            (h2(C::Refused), Code::Unavailable),
            (
                h2(C::GoAway {
                    code: E::NO_ERROR,
                    last_stream_id: 1,
                }),
                Code::Unavailable,
            ),
            (
                h2(C::GoAway {
                    code: E::PROTOCOL_ERROR,
                    last_stream_id: 1,
                }),
                Code::Unavailable,
            ),
            (h2(C::ConnectionClosed), Code::Unavailable),
            (
                h2(C::Io(std::io::ErrorKind::ConnectionReset)),
                Code::Unavailable,
            ),
            (h2(C::Timeout), Code::Unavailable),
            (h2(C::LocalLimit), Code::Unavailable),
            (h2(C::Protocol(E::PROTOCOL_ERROR)), Code::Internal),
            (h2(C::LocalReset(E::CANCEL)), Code::Cancelled),
            (h2(C::LocalReset(E::NO_ERROR)), Code::Cancelled),
            (h2(C::LocalReset(E::INTERNAL_ERROR)), Code::Internal),
            (h2(C::LocalReset(E::REFUSED_STREAM)), Code::Unavailable),
            (HttpError::Io(io), Code::Unavailable),
            (HttpError::ConnectTimeout, Code::Unavailable),
            (HttpError::Timeout, Code::DeadlineExceeded),
            (HttpError::Tls("bad cert".into()), Code::Unavailable),
            (
                HttpError::Protocol("x did not negotiate HTTP/2".into()),
                Code::Unavailable,
            ),
            (HttpError::BodyTooLarge(10), Code::ResourceExhausted),
        ];
        for (error, want) in cases {
            let status = Status::from_http_error(&error, None);
            assert_eq!(status.code(), want, "{error:?}");
            assert!(!status.message().contains("malformed http"), "{status}");
        }
    }

    #[test]
    fn a_failure_after_the_deadline_is_deadline_exceeded() {
        use crate::http::{Http2Cause as C, Http2Error, Http2ErrorCode as E};
        let past = Some(Instant::now());
        let future = Instant::now().checked_add(std::time::Duration::from_secs(60));
        let cancel = HttpError::Http2(Http2Error::new(C::Reset(E::CANCEL), "reset"));
        assert_eq!(
            Status::from_http_error(&cancel, past).code(),
            Code::DeadlineExceeded
        );
        assert_eq!(
            Status::from_http_error(&cancel, future).code(),
            Code::Cancelled
        );
        assert_eq!(
            Status::cancelled("x").after_deadline(past).code(),
            Code::DeadlineExceeded
        );
        assert_eq!(
            Status::unavailable("x").after_deadline(past).code(),
            Code::DeadlineExceeded
        );
        // A status the server sent is kept.
        assert_eq!(
            Status::invalid_argument("x").after_deadline(past).code(),
            Code::InvalidArgument
        );
        assert_eq!(
            Status::cancelled("x").after_deadline(future).code(),
            Code::Cancelled
        );
    }
}

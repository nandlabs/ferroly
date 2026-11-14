//! The transport-agnostic WebSocket message type and the close status codes.

/// A WebSocket application message.
///
/// Ping and pong control frames are handled internally and never appear here.
/// The close handshake is also handled internally, but its result is surfaced
/// as a final [`Message::Close`] so the application can see why a connection
/// ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// A UTF-8 text message.
    Text(String),
    /// A binary message.
    Binary(Vec<u8>),
    /// The connection ended. Received as the last message before `recv`
    /// returns `None`; `code` is the peer's status code, or one of the
    /// locally generated codes described in [`close_code`]. Passing it to
    /// `send` is the same as calling `close(code, reason)`.
    Close {
        /// The close status code (see [`close_code`]).
        code: u16,
        /// The UTF-8 close reason; may be empty.
        reason: String,
    },
}

impl Message {
    /// Creates a text message.
    pub fn text(s: impl Into<String>) -> Self {
        Message::Text(s.into())
    }

    /// Creates a binary message.
    pub fn binary(b: impl Into<Vec<u8>>) -> Self {
        Message::Binary(b.into())
    }

    /// Creates a close message.
    pub fn close(code: u16, reason: impl Into<String>) -> Self {
        Message::Close {
            code,
            reason: reason.into(),
        }
    }

    /// The message as text, if it is a text message.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Message::Text(s) => Some(s),
            _ => None,
        }
    }

    /// The message as bytes, if it is a binary message.
    pub fn as_binary(&self) -> Option<&[u8]> {
        match self {
            Message::Binary(b) => Some(b),
            _ => None,
        }
    }

    /// Whether this is the final [`Message::Close`].
    pub fn is_close(&self) -> bool {
        matches!(self, Message::Close { .. })
    }

    /// The payload size in bytes (for a close message, the reason length).
    pub fn len(&self) -> usize {
        match self {
            Message::Text(s) => s.len(),
            Message::Binary(b) => b.len(),
            Message::Close { reason, .. } => reason.len(),
        }
    }

    /// Whether the payload is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Close status codes (RFC 6455 §7.4.1) used by this module.
///
/// [`NO_STATUS`](close_code::NO_STATUS) and [`ABNORMAL`](close_code::ABNORMAL)
/// never travel on the wire: they are reported
/// locally in [`Message::Close`] when the peer sent a close frame without a
/// code, or when the connection ended without a close frame at all.
pub mod close_code {
    /// `1000`: normal closure.
    pub const NORMAL: u16 = 1000;
    /// `1001`: the endpoint is going away (server shutdown, idle timeout).
    pub const GOING_AWAY: u16 = 1001;
    /// `1002`: the peer violated the protocol.
    pub const PROTOCOL_ERROR: u16 = 1002;
    /// `1003`: the endpoint received a kind of data it cannot accept.
    pub const UNSUPPORTED_DATA: u16 = 1003;
    /// `1005`: the peer's close frame carried no status code (local only).
    pub const NO_STATUS: u16 = 1005;
    /// `1006`: the connection ended without a close frame (local only).
    pub const ABNORMAL: u16 = 1006;
    /// `1007`: a text message or close reason was not valid UTF-8.
    pub const INVALID_PAYLOAD: u16 = 1007;
    /// `1008`: policy violation; also used when a peer reads too slowly and
    /// the outgoing queue overflows.
    pub const POLICY_VIOLATION: u16 = 1008;
    /// `1009`: a frame or message exceeded the configured size limit.
    pub const MESSAGE_TOO_BIG: u16 = 1009;
    /// `1011`: internal error; also used when a keepalive pong is not
    /// received in time or a server handler panics.
    pub const INTERNAL_ERROR: u16 = 1011;

    /// Whether `code` may be sent in (or accepted from) a close frame:
    /// `1000..=1003`, `1007..=1014` and `3000..=4999`.
    pub fn is_sendable(code: u16) -> bool {
        matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999)
    }
}

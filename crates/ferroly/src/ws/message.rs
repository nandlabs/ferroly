//! The transport-agnostic WebSocket message type.

/// A WebSocket application message.
///
/// Control frames (ping/pong/close) are handled internally; this type carries
/// only the text and binary payloads applications care about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// A UTF-8 text message.
    Text(String),
    /// A binary message.
    Binary(Vec<u8>),
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

    /// The message as text, if it is a text frame.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Message::Text(s) => Some(s),
            Message::Binary(_) => None,
        }
    }
}

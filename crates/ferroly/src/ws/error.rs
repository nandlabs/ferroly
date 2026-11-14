//! Error type for the WebSocket module.

use ferroly_derive::FerrolyError;

/// Errors raised by WebSocket client and server operations.
#[derive(Debug, FerrolyError)]
#[non_exhaustive]
pub enum WsError {
    /// The connection could not be established or the opening handshake
    /// failed (bad URL, refused connection, TLS failure, invalid `101`
    /// response).
    #[error("connection error: {0}")]
    Connect(String),

    /// The server answered the upgrade request with an HTTP status other than
    /// `101 Switching Protocols` (for example `401` from an auth hook or
    /// `403` from the origin check).
    #[error("upgrade rejected with HTTP status {status}")]
    Rejected {
        /// The HTTP status code of the response.
        status: u16,
    },

    /// A deadline of [`WsClient::dial`](crate::ws::WsClient::dial) elapsed.
    /// The text names it: `connect` (TCP connect plus TLS handshake) or
    /// `handshake` (the upgrade exchange).
    #[error("timed out: {0}")]
    Timeout(String),

    /// Sending a message failed for a reason other than the ones below.
    /// Kept for source compatibility: this module reports a send on a dead
    /// connection as [`Closed`](Self::Closed) and no longer produces this
    /// variant itself (an application hook may).
    #[error("send error: {0}")]
    Send(String),

    /// The connection is closed or closing, so nothing more can be sent.
    #[error("connection closed")]
    Closed,

    /// The outgoing queue is full: the peer is not reading fast enough. From
    /// [`send`](crate::ws::WsSender::send) this also means the connection was
    /// closed with code `1008`; from
    /// [`try_send`](crate::ws::WsSender::try_send) the connection is left
    /// open and the message was not queued.
    #[error("outgoing queue full")]
    QueueFull,

    /// The caller asked for something RFC 6455 does not allow, such as
    /// closing with a reserved status code.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// The underlying transport failed. Kept for source compatibility: a
    /// transport failure on an established connection ends the message
    /// stream with a `1006` close message instead of an error.
    #[error("transport error: {0}")]
    Transport(String),
}

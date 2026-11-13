//! An observer hook for metrics and diagnostics.
//!
//! The WebSocket module does not depend on `ferroly::metrics`; instead it
//! reports what happens through [`WsObserver`], which an application can
//! bridge to any metrics or logging backend.

/// Which deadline expired in a [`WsEvent::TimedOut`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TimeoutKind {
    /// No frame arrived from the peer within the idle timeout.
    Idle,
    /// A keepalive ping was not answered within the pong deadline.
    Pong,
    /// The peer did not accept written bytes within the write timeout.
    Write,
}

/// Something that happened on a WebSocket connection (or during a
/// handshake), reported to a [`WsObserver`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WsEvent {
    /// A connection completed its opening handshake.
    Opened {
        /// The connection id.
        conn_id: u64,
    },
    /// A connection ended. `code` is the close code the application also
    /// sees in the final `Message::Close` (`1006` for an abnormal end).
    Closed {
        /// The connection id.
        conn_id: u64,
        /// The close status code.
        code: u16,
    },
    /// A complete (reassembled) message was received.
    MessageReceived {
        /// The connection id.
        conn_id: u64,
        /// Payload size in bytes.
        bytes: usize,
    },
    /// A message was written to the transport.
    MessageSent {
        /// The connection id.
        conn_id: u64,
        /// Payload size in bytes.
        bytes: usize,
    },
    /// A ping was received (a pong is queued automatically).
    PingReceived {
        /// The connection id.
        conn_id: u64,
    },
    /// A keepalive ping was sent.
    PingSent {
        /// The connection id.
        conn_id: u64,
    },
    /// A pong answering a keepalive ping was received.
    PongReceived {
        /// The connection id.
        conn_id: u64,
    },
    /// The peer violated the protocol or a size limit and the connection is
    /// being failed with `code` (`1002`, `1007` or `1009`).
    ProtocolError {
        /// The connection id.
        conn_id: u64,
        /// The close code sent to the peer.
        code: u16,
        /// A short fixed description (never echoes peer input).
        reason: &'static str,
    },
    /// A message could not be queued because the outgoing queue was full.
    QueueFull {
        /// The connection id.
        conn_id: u64,
    },
    /// A deadline expired on an established connection.
    TimedOut {
        /// The connection id.
        conn_id: u64,
        /// Which deadline.
        kind: TimeoutKind,
    },
    /// A server refused an upgrade request with this HTTP status (bad
    /// handshake, origin check, or the upgrade hook).
    HandshakeRejected {
        /// The HTTP status code sent.
        status: u16,
    },
}

/// Receives [`WsEvent`]s from clients and servers.
///
/// Called inline on the connection's task, never while an internal lock is
/// held. Implementations must be fast and must not block; typically they
/// bump counters.
///
/// ```
/// use std::sync::atomic::{AtomicU64, Ordering};
/// use ferroly::ws::{WsEvent, WsObserver};
///
/// #[derive(Default)]
/// struct Counters {
///     opened: AtomicU64,
///     bytes_in: AtomicU64,
/// }
///
/// impl WsObserver for Counters {
///     fn on_event(&self, event: &WsEvent) {
///         match event {
///             WsEvent::Opened { .. } => {
///                 self.opened.fetch_add(1, Ordering::Relaxed);
///             }
///             WsEvent::MessageReceived { bytes, .. } => {
///                 self.bytes_in.fetch_add(*bytes as u64, Ordering::Relaxed);
///             }
///             _ => {}
///         }
///     }
/// }
/// ```
pub trait WsObserver: Send + Sync {
    /// Called for every event.
    fn on_event(&self, event: &WsEvent);
}

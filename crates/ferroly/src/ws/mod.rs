//! WebSocket client and server, RFC 6455, entirely in-house.
//!
//! The frame codec, the opening handshake, masking, fragmentation, the close
//! handshake and keepalive are all hand-rolled over [`ferroly::http`]'s
//! transport (plain TCP or TLS) and `tokio`; hashing, base64 and randomness
//! come from [`ferroly::hash`].
//!
//! - **Client:** [`WsClient::dial`], then [`send`](WsClient::send) /
//!   [`recv`](WsClient::recv); [`ReconnectingClient`] adds reconnection with
//!   backoff.
//! - **Server:** [`server::serve_with_config`] runs an accept loop that gives
//!   each connection to an async [`WsHandler`] as a [`WsConn`];
//!   [`WsServer`] wraps it as a lifecycle component; [`server::serve`] keeps
//!   the original reply-per-message shape.
//! - **Fan-out:** [`Hub`] broadcasts to all connections, a room, or one id.
//! - **Security:** [`OriginPolicy`] and the [`UpgradeHook`] decide who may
//!   connect, before the upgrade; [`WsServerConfig`] bounds every resource.
//!
//! Both sides enforce the protocol strictly (masking direction, reserved
//! bits, opcodes, control-frame limits, fragmentation order, UTF-8, close
//! codes, size caps) and both use bounded queues, so a peer can neither
//! confuse the parser nor grow this process's memory. See `docs/ws.md` for
//! the full list of rules and limits.

#![deny(missing_docs)]

mod client;
mod conn;
mod error;
mod frame;
mod handshake;
mod hub;
mod message;
mod observer;
mod options;
mod rand;
mod reconnect;
pub mod server;
mod upgrade;

pub use client::WsClient;
pub use conn::WsSender;
pub use error::WsError;
pub use hub::{Hub, HubMember, SlowConsumerPolicy};
pub use message::{close_code, Message};
pub use observer::{TimeoutKind, WsEvent, WsObserver};
pub use options::{WsOptions, WsServerConfig, DEFAULT_MAX_FRAME_SIZE, DEFAULT_MAX_MESSAGE_SIZE};
pub use reconnect::{ReconnectOptions, ReconnectingClient};
pub use server::{WsConn, WsHandler, WsServer};
pub use upgrade::{OriginPolicy, UpgradeAccept, UpgradeHook, UpgradeReject, UpgradeRequest};

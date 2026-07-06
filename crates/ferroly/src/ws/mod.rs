//! WebSocket client and server, RFC 6455, entirely in-house.
//!
//! Built to a zero-dependency ethos: the frame
//! codec, opening handshake (with a from-scratch SHA-1), masking, and
//! fragmentation are all hand-rolled over [`ferroly::http`]'s transport and
//! `tokio`.
//!
//! - Client: [`WsClient::dial`], then [`send`](WsClient::send) / [`recv`](WsClient::recv).
//! - Server: [`server::serve`] runs an accept loop with a per-message handler.

#![deny(missing_docs)]

mod client;
mod conn;
mod error;
mod frame;
mod handshake;
mod message;
mod options;
mod rand;
pub mod server;

pub use client::WsClient;
pub use error::WsError;
pub use message::Message;
pub use options::WsOptions;
pub use server::WsServer;

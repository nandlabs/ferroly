//! In-house HTTP/1.1 stack over `tokio` (+ `tokio-rustls` for HTTPS).
//!
//! Replaces `reqwest`/`hyper`/`http` with a small, dependency-minimal
//! implementation: URL parsing, HTTP/1.1 request/response codec (content-length
//! and chunked framing), a TLS-or-plaintext transport, and a streaming client.
//!
//! ```no_run
//! use ferroly::http::{Client, Method, Request};
//!
//! # async fn ex() -> Result<(), ferroly::http::HttpError> {
//! let client = Client::new();
//! let req = Request::builder(Method::Get, "https://example.com/")?.build();
//! let resp = client.send(req).await?;
//! println!("{} {}", resp.status().as_u16(), resp.text().await?);
//! # Ok(())
//! # }
//! ```

#![deny(missing_docs)]

mod client;
mod core;
mod download;
mod error;
pub(crate) mod io;
mod message;
mod pool;
mod server;
pub mod sse;
pub(crate) mod transport;

pub use client::Client;
pub use core::{split_target, HeaderMap, Method, StatusCode, Uri};
pub use download::download_to_file;
pub use error::HttpError;
pub use message::{Request, RequestBuilder, Response};
pub use server::{
    serve, serve_tls, serve_tls_with_config, serve_with_config, BoxFuture, HttpHandler,
    HttpResponse, ServerConfig,
};
pub use sse::{Event, SseDecoder};
pub use transport::{Conn, Io, TlsConfig};

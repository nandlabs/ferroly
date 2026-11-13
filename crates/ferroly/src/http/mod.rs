//! In-house HTTP/1.1 stack over `tokio` (+ `tokio-rustls` for HTTPS).
//!
//! Replaces `reqwest`/`hyper`/`http` with a small, dependency-minimal
//! implementation: URL parsing, HTTP/1.1 request/response codec (content-length
//! and chunked framing), a TLS-or-plaintext transport, a streaming client and a
//! server.
//!
//! Both sides parse strictly — anything that two HTTP implementations could
//! read differently is rejected — and every buffer, line, header section, body
//! and wait is bounded by a configurable limit or timeout. See
//! [`ServerConfig`] and [`ClientBuilder`] for the knobs and their defaults.
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
mod date;
mod download;
mod error;
mod form;
mod inflate;
pub(crate) mod io;
mod message;
mod pool;
mod proxy;
mod server;
pub mod sse;
pub(crate) mod transport;

pub use client::{Client, ClientBuilder, Interceptor};
pub use core::{split_target, HeaderMap, Method, StatusCode, Uri};
pub use date::format_http_date;
pub use download::download_to_file;
pub use error::HttpError;
pub use form::{form_urlencode, normalize_path, percent_decode, percent_encode, Multipart};
pub use message::{Request, RequestBuilder, Response};
pub use proxy::Proxy;
pub use server::{
    serve, serve_tls, serve_tls_reloadable, serve_tls_with_config, serve_with_config, BoxFuture,
    ErrorHook, HttpHandler, HttpResponse, ServerConfig,
};
pub use sse::{Event, SseDecoder};
pub use transport::{
    ClientTlsConfig, Conn, Io, ReloadableTlsConfig, TlsConfig, TlsConfigBuilder, TlsVersion,
};

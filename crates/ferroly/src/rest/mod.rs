//! HTTP client + server framework.
//!
//! A full HTTP client and server framework, entirely in-house. The **client** wraps
//! [`ferroly::http::Client`] with codec-aware bodies, pluggable auth, retry,
//! circuit breaking and pagination. The **server** wraps the first-class
//! [`ferroly::turbo`] router and the [`ferroly::http`] server (plain or TLS),
//! integrated with [`ferroly::lifecycle`] for graceful start/stop and
//! restart.
//!
//! ```no_run
//! use ferroly::rest::{Client, Server, ServerOptions};
//! use ferroly::http::{HttpResponse, StatusCode};
//!
//! # #[tokio::main]
//! # async fn main() {
//! let server = Server::builder(ServerOptions::default())
//!     .get("/ping", |_ctx| async move { HttpResponse::text(StatusCode::OK, "pong") })
//!     .build();
//! // register `server` with a ferroly::lifecycle::ComponentManager to run it.
//!
//! let client = Client::new();
//! // let resp = client.get("http://localhost:8080/ping").send().await.unwrap();
//! # let _ = (server, client);
//! # }
//! ```

#![deny(missing_docs)]

mod client;
mod error;
mod server;

pub use client::{Client, ClientOptions, ClientOptionsBuilder, RequestBuilder, Response};
pub use error::ClientError;
pub use server::{Server, ServerBuilder, ServerOptions, ServerTls};

// Re-export the router/handler types REST handlers use.
pub use ferroly::http::{HttpResponse, Method, StatusCode};
pub use ferroly::turbo::{Cors, Ctx, Group, IntoResponse, Route, Router, SecurityHeaders};

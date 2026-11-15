//! gRPC over the in-house HTTP/2 stack, with [`proto`](crate::proto)
//! messages — no external crate.
//!
//! - [`GrpcServer`] routes calls by path (`/package.Service/Method`) to
//!   handlers for the four RPC shapes; it is an
//!   [`HttpHandler`](crate::http::HttpHandler), served by the
//!   [`http`](crate::http) server with `ServerConfig::http2` set (h2c with
//!   prior knowledge, or TLS with ALPN `h2`).
//! - [`Channel`] makes calls over the [`http`](crate::http) client's
//!   multiplexed HTTP/2 connections.
//! - [`Request`] / [`Response`] carry a message (or a [`Streaming`] of them)
//!   with its [`Metadata`]; every failure is a [`Status`] with a [`Code`].
//! - Deadlines travel as `grpc-timeout` ([`parse_timeout`] /
//!   [`format_timeout`]) and are enforced on both sides.
//! - [`codegen::GrpcGenerator`] plugs into
//!   [`proto::codegen::Config`](crate::proto::codegen::Config) and emits,
//!   per `.proto` service, a server trait, a server adapter and a typed
//!   client.
//! - [`health`] is the standard `grpc.health.v1.Health` service.
//!
//! A unary echo, both sides:
//!
//! ```
//! use ferroly::grpc::{Channel, GrpcServer, Request, Response, Status};
//!
//! #[derive(ferroly::proto::Message, Default, PartialEq, Debug)]
//! struct Ping {
//!     #[proto(tag = 1)]
//!     n: u32,
//! }
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let server = GrpcServer::new().unary("/demo.Echo/Ping", |req: Request<Ping>| async move {
//!     Ok(Response::new(Ping { n: req.into_inner().n + 1 }))
//! });
//! let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
//! let addr = listener.local_addr()?;
//! let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
//! let serving = tokio::spawn(server.serve(listener, async { let _ = stopped.await; }));
//!
//! let channel = Channel::new(&format!("http://{addr}"))?;
//! let reply = channel.unary::<Ping, Ping>("/demo.Echo/Ping", Request::new(Ping { n: 1 })).await?;
//! assert_eq!(reply.into_inner(), Ping { n: 2 });
//!
//! let err = channel.unary::<Ping, Ping>("/demo.Echo/Nope", Request::new(Ping::default())).await;
//! assert_eq!(err.unwrap_err().code(), ferroly::grpc::Code::Unimplemented);
//! # let _ = stop.send(());
//! # serving.await??;
//! # Ok(())
//! # }
//! ```
//!
//! Messages compressed with `gzip` or `deflate` are decoded; this side
//! always sends `identity`. Calls the server refused before processing them
//! are retried transparently; there is no reflection service, no retry
//! policy or load balancing, and no gRPC-Web. The repository's
//! `docs/grpc.md` is the full guide.

#![deny(missing_docs)]

mod call;
mod client;
pub mod codegen;
mod frame;
pub mod health;
mod interceptor;
mod metadata;
mod server;
mod status;
mod streaming;
mod timeout;

pub use call::{IntoRequest, Request, Response};
pub use client::{Channel, ChannelBuilder};
pub use frame::{encode_frame, FrameDecoder, DEFAULT_MAX_MESSAGE_SIZE};
pub use interceptor::Interceptor;
pub use metadata::{Metadata, MetadataValue};
pub use server::{GrpcServer, GrpcService};
pub use status::{Code, Status};
pub use streaming::{StreamSender, Streaming};
pub use timeout::{format_timeout, parse_timeout};

/// A boxed `Send` future: the return type of the generated server traits'
/// methods (the manual `async fn`-in-trait desugaring that keeps them
/// object-safe). The same type as [`crate::http::BoxFuture`].
pub use crate::http::BoxFuture;

/// Fuzz entry point: frames, status trailers, `grpc-timeout` and metadata
/// decoding over arbitrary bytes. Not a stable API.
#[doc(hidden)]
pub fn __fuzz(data: &[u8]) {
    // Frames, with the input split at a data-dependent point.
    let split = data.first().map_or(0, |&b| usize::from(b)).min(data.len());
    let mut decoder = FrameDecoder::new(1024);
    for chunk in [&data[..split], &data[split..]] {
        decoder.push(chunk);
        while let Ok(Some(frame)) = decoder.next_frame() {
            assert!(frame.len() <= 1024);
        }
    }
    let _ = decoder.finish();

    // Header-shaped text: status, timeout and metadata decoding.
    let text = String::from_utf8_lossy(data);
    if let Some(t) = parse_timeout(&text) {
        let again = parse_timeout(&format_timeout(t)).expect("formatted timeouts parse");
        assert!(again >= t);
    }
    let mut headers = crate::http::HeaderMap::new();
    for line in text.lines().take(64) {
        if let Some((k, v)) = line.split_once(':') {
            headers.append(k.trim(), v.trim());
        }
    }
    headers.append("grpc-status", text.chars().take(12).collect::<String>());
    headers.append("grpc-message", text.as_ref());
    headers.append("x-fuzz-bin", text.as_ref());
    if let Some(status) = Status::from_header_map(&headers) {
        let _ = status.to_string();
        let mut out = crate::http::HeaderMap::new();
        status.write_to(&mut out);
        let back = Status::from_header_map(&out).expect("a written status reads back");
        assert_eq!(back.code(), status.code());
        assert_eq!(back.message(), status.message());
    }
    let md = Metadata::from_headers(&headers);
    let mut out = crate::http::HeaderMap::new();
    md.write_to(&mut out);
    let _ = Metadata::from_headers(&out);
}

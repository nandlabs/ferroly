//! # Ferroly
//!
//! A self-contained, dependency-minimal toolkit of enterprise utilities under a
//! single crate. Each area is a feature-gated module, so you compile only what
//! you enable:
//!
//! ```toml
//! ferroly = { version = "0.1", features = ["genai", "codec"] }
//! ```
//!
//! ```
//! # #[cfg(feature = "codec")]
//! use ferroly::codec::{json, Encode, Decode};
//! ```
//!
//! The only external runtime dependencies are `tokio` (and, for HTTPS,
//! `tokio-rustls`); everything else is implemented in-house. Cloud integrations
//! live in separate crates (`ferroly-aws`, `ferroly-gcp`, `ferroly-vault`).

// Let derive-generated `::ferroly::codec::…` paths resolve inside this crate.
extern crate self as ferroly;

/// Derives a typed error enum/struct: `Display` from `#[error("…")]` format
/// strings (with field interpolation), `std::error::Error::source()` chaining,
/// and `From` conversions for `#[from]` fields — so it works with `?` directly.
/// Use `#[error(transparent)]` to forward to a wrapped error.
///
/// Re-exported at the crate root so it is available with only `errutils`
/// (i.e. without enabling `codec`).
pub use ferroly_derive::FerrolyError;

#[cfg(feature = "auth")]
pub mod auth;
#[cfg(feature = "clients")]
pub mod clients;
#[cfg(feature = "codec")]
pub mod codec;
#[cfg(feature = "config")]
pub mod config;
#[cfg(feature = "errutils")]
pub mod errutils;
#[cfg(feature = "fsutils")]
pub mod fsutils;
#[cfg(feature = "genai")]
pub mod genai;
#[cfg(feature = "hash")]
pub mod hash;
#[cfg(feature = "http")]
pub mod http;
#[cfg(feature = "lifecycle")]
pub mod lifecycle;
#[cfg(feature = "log")]
pub mod log;
#[cfg(feature = "messaging")]
pub mod messaging;
#[cfg(feature = "metrics")]
pub mod metrics;
#[cfg(feature = "rest")]
pub mod rest;
#[cfg(feature = "rt")]
pub mod rt;
#[cfg(feature = "turbo")]
pub mod turbo;
#[cfg(feature = "vectorstore")]
pub mod vectorstore;
#[cfg(feature = "vfs")]
pub mod vfs;
#[cfg(feature = "ws")]
pub mod ws;

/// Top-level aggregated error wrapping every enabled module's error type.
#[derive(Debug, ferroly_derive::FerrolyError)]
#[non_exhaustive]
pub enum Error {
    /// A encoding/codec error.
    #[cfg(feature = "codec")]
    #[error(transparent)]
    Codec(#[from] codec::CodecError),

    /// A configuration error.
    #[cfg(feature = "config")]
    #[error(transparent)]
    Config(#[from] config::ConfigError),

    /// A filesystem-utility error.
    #[cfg(feature = "fsutils")]
    #[error(transparent)]
    Fs(#[from] fsutils::FsError),

    /// A component-lifecycle error.
    #[cfg(feature = "lifecycle")]
    #[error(transparent)]
    Lifecycle(#[from] lifecycle::LifecycleError),

    /// A GenAI error.
    #[cfg(feature = "genai")]
    #[error(transparent)]
    GenAi(#[from] genai::GenAiError),

    /// A REST client error.
    #[cfg(feature = "rest")]
    #[error(transparent)]
    Rest(#[from] rest::ClientError),

    /// A WebSocket error.
    #[cfg(feature = "ws")]
    #[error(transparent)]
    Ws(#[from] ws::WsError),

    /// An HTTP client/server error.
    #[cfg(feature = "http")]
    #[error(transparent)]
    Http(#[from] http::HttpError),

    /// A resilience-client error (circuit breaker open).
    #[cfg(feature = "clients")]
    #[error(transparent)]
    Breaker(#[from] clients::CircuitOpenError),

    /// A JWT/auth error.
    #[cfg(feature = "auth")]
    #[error(transparent)]
    Auth(#[from] auth::JwtError),

    /// A router path/query parameter error.
    #[cfg(feature = "turbo")]
    #[error(transparent)]
    Param(#[from] turbo::ParamError),

    /// A messaging error.
    #[cfg(feature = "messaging")]
    #[error(transparent)]
    Messaging(#[from] messaging::MessagingError),

    /// A vector-store error.
    #[cfg(feature = "vectorstore")]
    #[error(transparent)]
    VectorStore(#[from] vectorstore::VectorStoreError),
}

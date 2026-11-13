//! # Ferroly
//!
//! A self-contained, dependency-minimal toolkit of enterprise utilities under a
//! single crate. Each area is a feature-gated module, so you compile only what
//! you enable:
//!
//! ```toml
//! ferroly = { version = "0.4", features = ["genai", "codec"] }
//! ```
//!
//! ```
//! # #[cfg(feature = "codec")]
//! use ferroly::codec::{json, Encode, Decode};
//! ```
//!
//! ## Modules
//!
//! Every module sits behind the cargo feature of the same name. The defaults
//! are `codec` and `errutils`; `full` enables everything.
//!
//! | Module | What it provides |
//! |---|---|
//! | `auth` | HS256 JSON Web Token signing and verification |
//! | `cli` | Command-line parser: subcommands, flags, positionals, help, shell completion |
//! | `clients` | Resilience primitives: auth providers, retry with backoff, circuit breaker |
//! | `codec` | `Value` model, `Encode`/`Decode` traits and derives, JSON/XML/YAML/TOML, a JSON-Schema subset |
//! | `config` | Layered configuration from files, environment and command-line flags |
//! | `errutils` | `MultiError` aggregation, error context wrapping and error classification |
//! | `fsutils` | Content-type detection and read-only memory-mapped files |
//! | `genai` | Provider-agnostic LLM interface, prompt templates and a model router |
//! | `hash` | Streaming SHA-256 / SHA-1 / HMAC-SHA256 |
//! | `http` | HTTP/1.1 client and server (chunked, SSE, range downloads, TLS) |
//! | `lifecycle` | Dependency-ordered component start/stop and health probes |
//! | `log` | Structured logger |
//! | `messaging` | Provider-agnostic messaging with a local in-process bus |
//! | `metrics` | Counters, gauges, histograms and Prometheus exposition |
//! | `obs` | Span/event tracing with exporters |
//! | `rest` | REST client and server framework |
//! | `rt` | Async runtime surface (re-exported `tokio` primitives) |
//! | `turbo` | HTTP router with middleware |
//! | `vectorstore` | Vector-store trait with an in-memory backend |
//! | `vfs` | Virtual filesystem trait with a local backend |
//! | `ws` | WebSocket client and server (RFC 6455) |
//!
//! The `openai`, `claude` and `ollama` features add the matching `genai`
//! provider (`all-providers` enables the three); they have no module of their
//! own. The derive macros (`Encode`, `Decode`, [`FerrolyError`]) come from the
//! companion `ferroly-derive` crate and are re-exported here.
//!
//! [`Error`] wraps every enabled module's error type, so a function that
//! touches several modules can return `Result<_, ferroly::Error>` and use `?`
//! throughout.
//!
//! The only external runtime dependencies are `tokio` and, for TLS, the
//! `rustls` stack (`tokio-rustls`, `rustls`, `webpki-roots`); everything else
//! is implemented in-house.

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
#[cfg(feature = "cli")]
pub mod cli;
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
#[cfg(feature = "obs")]
pub mod obs;
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
///
/// Each variant is `#[from]`, so `?` converts a module error into this type.
/// With the `errutils` feature it also implements `errutils::Classify`, so
/// callers can ask whether an error is retryable or caused by bad input.
#[derive(Debug, ferroly_derive::FerrolyError)]
#[non_exhaustive]
pub enum Error {
    /// An encoding/codec error.
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

    /// A missing scope or role on otherwise valid credentials.
    #[cfg(feature = "auth")]
    #[error(transparent)]
    Scope(#[from] auth::ScopeError),

    /// A credential could not be obtained or refreshed (for example an OAuth 2.0
    /// token request failed).
    #[cfg(feature = "clients")]
    #[error(transparent)]
    Credentials(#[from] clients::AuthError),

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

    /// A virtual-filesystem error.
    #[cfg(feature = "vfs")]
    #[error(transparent)]
    Vfs(#[from] vfs::VfsError),

    /// A command-line parsing error (including help/version requests).
    #[cfg(feature = "cli")]
    #[error(transparent)]
    Cli(#[from] cli::CliError),

    /// A GenAI model-router error.
    #[cfg(feature = "genai")]
    #[error(transparent)]
    Router(#[from] genai::RouterError),

    /// A spawned task failed to complete (it panicked or was cancelled).
    #[cfg(feature = "rt")]
    #[error(transparent)]
    Join(#[from] rt::JoinError),

    /// Several errors collected into one.
    #[cfg(feature = "errutils")]
    #[error(transparent)]
    Multi(#[from] errutils::MultiError),

    /// An error wrapped with a context message.
    #[cfg(feature = "errutils")]
    #[error(transparent)]
    Context(#[from] errutils::ContextError),
}

/// Classification of the aggregated error: the wrapped module error decides
/// where its category is unambiguous, and everything else answers
/// [`ErrorKind::Unknown`](errutils::ErrorKind::Unknown).
#[cfg(feature = "errutils")]
impl errutils::Classify for Error {
    // Variants are matched with `{ .. }` (whatever their shape) and a trailing
    // wildcard, so a module adding variants or fields never breaks this
    // mapping; a new variant simply classifies as `Unknown` until it is listed
    // here. That is deliberate, hence the two lint exemptions.
    #[allow(unknown_lints, clippy::unneeded_struct_pattern)]
    fn kind(&self) -> errutils::ErrorKind {
        #[allow(unused_imports)]
        use errutils::{Classify, ErrorKind as K};

        #[allow(unreachable_patterns)]
        match self {
            #[cfg(feature = "codec")]
            Error::Codec(e) => match e {
                codec::CodecError::UnsupportedContentType { .. } => K::Unsupported,
                _ => K::InvalidInput,
            },
            #[cfg(feature = "config")]
            Error::Config(e) => e.kind(),
            #[cfg(feature = "fsutils")]
            Error::Fs(e) => classify::io_source(e).unwrap_or(K::Unknown),
            #[cfg(feature = "lifecycle")]
            Error::Lifecycle(e) => match e {
                lifecycle::LifecycleError::ComponentNotFound { .. } => K::NotFound,
                lifecycle::LifecycleError::ComponentAlreadyStarted { .. } => K::Conflict,
                lifecycle::LifecycleError::CyclicDependency { .. } => K::InvalidInput,
                lifecycle::LifecycleError::Timeout { .. } => K::Timeout,
                _ => K::Unknown,
            },
            #[cfg(feature = "genai")]
            Error::GenAi(e) => classify::genai(e),
            #[cfg(feature = "genai")]
            Error::Router(e) => {
                match e {
                    genai::RouterError::UnknownProvider { .. }
                    | genai::RouterError::Config { .. } => K::InvalidInput,
                    genai::RouterError::DeadlineExceeded { .. } => K::Timeout,
                    genai::RouterError::InvalidRequest { .. } => K::InvalidInput,
                    genai::RouterError::CircuitOpen { .. } => K::Unavailable,
                    genai::RouterError::BudgetExceeded { .. } => K::RateLimited,
                    genai::RouterError::GenAi { .. }
                    | genai::RouterError::AllRoutesFailed { .. } => std::error::Error::source(e)
                        .and_then(|s| s.downcast_ref::<genai::GenAiError>())
                        .map_or(K::Unknown, classify::genai),
                    _ => K::Unknown,
                }
            }
            #[cfg(feature = "rest")]
            Error::Rest(e) => errutils::Classify::kind(e),
            #[cfg(feature = "ws")]
            Error::Ws(_) => K::Unavailable,
            #[cfg(feature = "http")]
            Error::Http(e) => classify::http(e),
            #[cfg(feature = "clients")]
            Error::Breaker(_) => K::Unavailable,
            #[cfg(feature = "auth")]
            Error::Auth(_) => K::Unauthenticated,
            #[cfg(feature = "auth")]
            Error::Scope(_) => K::PermissionDenied,
            #[cfg(feature = "clients")]
            Error::Credentials(e) => match e {
                clients::AuthError::Transport(_) => K::Unavailable,
                clients::AuthError::Rejected { .. } => K::Unauthenticated,
                clients::AuthError::Config(_) => K::InvalidInput,
                _ => K::Unknown,
            },
            #[cfg(feature = "turbo")]
            Error::Param(_) => K::InvalidInput,
            #[cfg(feature = "messaging")]
            Error::Messaging(messaging::MessagingError::BufferFull { .. }) => K::Unavailable,
            #[cfg(feature = "vectorstore")]
            Error::VectorStore(vectorstore::VectorStoreError::DimMismatch { .. }) => {
                K::InvalidInput
            }
            #[cfg(feature = "vfs")]
            Error::Vfs(e) => match e {
                vfs::VfsError::NotFound { .. } => K::NotFound,
                vfs::VfsError::Permission { .. } => K::PermissionDenied,
                vfs::VfsError::NotDir { .. } | vfs::VfsError::IsDir { .. } => K::InvalidInput,
                vfs::VfsError::Unsupported { .. } => K::Unsupported,
                other => classify::io_source(other).unwrap_or(K::Unknown),
            },
            #[cfg(feature = "cli")]
            Error::Cli(e) => e.kind(),
            #[cfg(feature = "rt")]
            Error::Join(e) => {
                if e.is_panic() {
                    K::Internal
                } else {
                    K::Unknown
                }
            }
            Error::Multi(e) => e.kind(),
            Error::Context(e) => e.kind(),
            _ => K::Unknown,
        }
    }
}

/// Per-module classification helpers shared by several [`Error`] variants.
#[cfg(feature = "errutils")]
mod classify {
    // `{ .. }` patterns on purpose; see `Classify for Error`.
    #![allow(unknown_lints, clippy::unneeded_struct_pattern)]

    #[allow(unused_imports)]
    use super::errutils::{Classify, ErrorKind as K};

    /// The kind of the `std::io::Error` directly behind `e`, when that is what
    /// its `source()` is.
    #[cfg(any(feature = "fsutils", feature = "http", feature = "vfs"))]
    pub(super) fn io_source(e: &(dyn std::error::Error + 'static)) -> Option<K> {
        e.source()
            .and_then(|s| s.downcast_ref::<std::io::Error>())
            .map(Classify::kind)
    }

    #[cfg(feature = "http")]
    pub(super) fn http(e: &super::http::HttpError) -> K {
        use super::http::HttpError;
        #[allow(unreachable_patterns)]
        match e {
            HttpError::InvalidUrl { .. } => K::InvalidInput,
            HttpError::Timeout { .. } => K::Timeout,
            other => io_source(other).unwrap_or(K::Unknown),
        }
    }

    #[cfg(feature = "genai")]
    pub(super) fn genai(e: &super::genai::GenAiError) -> K {
        use super::genai::GenAiError;
        #[allow(unreachable_patterns)]
        match e {
            GenAiError::Template { .. } | GenAiError::Config { .. } => K::InvalidInput,
            GenAiError::TemplateNotFound { .. } => K::NotFound,
            GenAiError::Unsupported { .. } => K::Unsupported,
            GenAiError::Transport { .. } => K::Unavailable,
            GenAiError::Api { status, .. } => match *status {
                401 => K::Unauthenticated,
                403 => K::PermissionDenied,
                404 => K::NotFound,
                408 => K::Timeout,
                409 => K::Conflict,
                429 => K::RateLimited,
                400..=499 => K::InvalidInput,
                500..=599 => K::Unavailable,
                _ => K::Unknown,
            },
            GenAiError::ResponseParse { .. } => K::Internal,
            GenAiError::Timeout { .. } => K::Timeout,
            GenAiError::RateLimited { .. } => K::RateLimited,
            GenAiError::Auth { status, .. } => match *status {
                403 => K::PermissionDenied,
                _ => K::Unauthenticated,
            },
            GenAiError::ContentFilter { .. } | GenAiError::ContextLengthExceeded { .. } => {
                K::InvalidInput
            }
            GenAiError::Overloaded { .. } => K::Unavailable,
            _ => K::Unknown,
        }
    }
}

// Compile and run every example in the crate README as a doctest, so the page
// shown on crates.io cannot drift from the API. Examples marked `no_run` (they
// need a network or files) are compiled only.
#[cfg(all(doctest, feature = "full"))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

//! # Ferroly
//!
//! A self-contained, dependency-minimal toolkit of enterprise utilities under a
//! single crate. Each area is a feature-gated module, so you compile only what
//! you enable:
//!
//! ```toml
//! ferroly = { version = "0.6", features = ["genai", "codec"] }
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
//! are `codec`, `errutils` and `tls-ring`; `full` enables everything.
//!
//! | Module | What it provides |
//! |---|---|
//! | `auth` | HS256 JSON Web Token signing and verification |
//! | `cli` | Command-line parser: subcommands, flags, positionals, help, shell completion |
//! | `clients` | Resilience primitives: auth providers, retry with backoff, circuit breaker |
//! | `codec` | `Value` model, `Encode`/`Decode` traits and derives, JSON/XML/YAML/TOML, a JSON-Schema subset |
//! | `config` | Layered configuration from files, environment and command-line flags |
//! | `errutils` | `MultiError` aggregation, error context wrapping and error classification |
//! | `fsutils` | Content-type detection, read-only memory-mapped files, human-readable byte sizes |
//! | `genai` | Provider-agnostic LLM interface, prompt templates and a model router |
//! | `grpc` | gRPC over HTTP/2: unary and streaming calls, status, metadata, deadlines, a `.proto` service generator, the health service |
//! | `hash` | Streaming SHA-256 / SHA-1 / HMAC-SHA256 |
//! | `http` | HTTP/1.1 client and server (chunked, SSE, range downloads, TLS) |
//! | `http2` | HTTP/2 for the `http` server and client: ALPN `h2`, h2c prior knowledge, multiplexing, trailers, bidirectional streaming (no module of its own) |
//! | `lifecycle` | Dependency-ordered component start/stop and health probes |
//! | `log` | Structured logger |
//! | `log::bridge` | Routes records from the `log` facade crate into the structured logger (feature `log-bridge`) |
//! | `messaging` | Provider-agnostic messaging: typed attributes, filters, a delivery context, and an in-process bus |
//! | `metrics` | Counters, gauges, histograms and Prometheus exposition |
//! | `obs` | Span/event tracing with exporters |
//! | `proto` | Protocol Buffers wire codec, `Message` derive, `.proto` parser and code generator (feature `protobuf`) |
//! | `rest` | REST client and server framework |
//! | `rt` | Async runtime surface (re-exported `tokio` primitives) |
//! | `sig` | Verify-only Ed25519 signatures (RFC 8032; Ed25519ph and Ed25519ctx; streaming) |
//! | `store` | Content-addressed cache; S3 (SigV4), Hugging Face and OCI fetch adapters (`store-s3`, `store-hf`, `store-oci`) |
//! | `template` | Sandboxed, bounded Jinja2-subset template engine (LLM chat templates) |
//! | `turbo` | HTTP router with middleware |
//! | `vectorstore` | Vector-store trait with an in-memory backend |
//! | `vfs` | Virtual filesystem trait with local and in-memory backends and object-store hooks |
//! | `ws` | WebSocket client and server (RFC 6455) |
//!
//! The `openai`, `claude` and `ollama` features add the matching `genai`
//! provider (`all-providers` enables the three); they have no module of their
//! own. The derive macros (`Encode`, `Decode`, [`FerrolyError`]) come from the
//! companion `ferroly-derive` crate and are re-exported here.
//!
//! | Feature | What it does |
//! |---|---|
//! | `tls-ring` | `ring` as the `rustls` crypto backend (default; no module of its own). Used when the application has not installed a process-default `CryptoProvider`; with it off ferroly links no backend, and TLS setup and RS256 / ES256 verification return a typed error until one is installed (e.g. `aws-lc-rs`) |
//!
//! [`Error`] wraps every enabled module's error type, so a function that
//! touches several modules can return `Result<_, ferroly::Error>` and use `?`
//! throughout.
//!
//! The only external runtime dependencies are `tokio` and, for TLS, the
//! `rustls` stack (`tokio-rustls`, `rustls`, `webpki-roots`); everything else
//! is implemented in-house. The single optional exception is the opt-in
//! `log-bridge` feature, which adds the `log` facade crate (itself free of
//! dependencies).

// Let derive-generated `::ferroly::codec::…` paths resolve inside this crate.
extern crate self as ferroly;

// The one base64 implementation (public as `hash::base64`), always compiled.
mod base64;

// The plain `key=value` text format shared by `log` and `obs`'s
// `PlainExporter`.
#[cfg(any(feature = "log", feature = "obs"))]
mod textfmt;

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
#[cfg(feature = "grpc")]
pub mod grpc;
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
#[cfg(feature = "protobuf")]
pub mod proto;
#[cfg(feature = "rest")]
pub mod rest;
#[cfg(feature = "rt")]
pub mod rt;
#[cfg(feature = "sig")]
pub mod sig;
#[cfg(feature = "store")]
pub mod store;
#[cfg(feature = "template")]
pub mod template;
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

    /// A signature-verification error.
    #[cfg(feature = "sig")]
    #[error(transparent)]
    Sig(#[from] sig::SigError),

    /// An artifact-cache or fetch-adapter error.
    #[cfg(feature = "store")]
    #[error(transparent)]
    Store(#[from] store::StoreError),

    /// A command-line parsing error (including help/version requests).
    #[cfg(feature = "cli")]
    #[error(transparent)]
    Cli(#[from] cli::CliError),

    /// A GenAI model-router error.
    #[cfg(feature = "genai")]
    #[error(transparent)]
    Router(#[from] genai::RouterError),

    /// A protobuf decoding error.
    #[cfg(feature = "protobuf")]
    #[error(transparent)]
    Proto(#[from] proto::ProtoError),

    /// A template parse or render error.
    #[cfg(feature = "template")]
    #[error(transparent)]
    Template(#[from] template::TemplateError),

    /// A failed gRPC call.
    #[cfg(feature = "grpc")]
    #[error(transparent)]
    Grpc(#[from] grpc::Status),

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
            Error::Fs(e) => errutils::io_source_kind(e).unwrap_or(K::Unknown),
            #[cfg(feature = "lifecycle")]
            Error::Lifecycle(e) => match e {
                lifecycle::LifecycleError::ComponentNotFound { .. } => K::NotFound,
                lifecycle::LifecycleError::ComponentAlreadyStarted { .. } => K::Conflict,
                lifecycle::LifecycleError::CyclicDependency { .. } => K::InvalidInput,
                lifecycle::LifecycleError::Timeout { .. } => K::Timeout,
                _ => K::Unknown,
            },
            #[cfg(feature = "genai")]
            Error::GenAi(e) => e.kind(),
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
                        .map_or(K::Unknown, Classify::kind),
                    _ => K::Unknown,
                }
            }
            #[cfg(feature = "rest")]
            Error::Rest(e) => errutils::Classify::kind(e),
            #[cfg(feature = "ws")]
            Error::Ws(e) => e.kind(),
            #[cfg(feature = "http")]
            Error::Http(e) => e.kind(),
            #[cfg(feature = "clients")]
            Error::Breaker(_) => K::Unavailable,
            #[cfg(feature = "auth")]
            Error::Auth(e) => e.kind(),
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
            Error::Messaging(e) => e.kind(),
            #[cfg(feature = "vectorstore")]
            Error::VectorStore(vectorstore::VectorStoreError::DimMismatch { .. }) => {
                K::InvalidInput
            }
            #[cfg(feature = "vfs")]
            Error::Vfs(e) => e.kind(),
            #[cfg(feature = "sig")]
            Error::Sig(e) => match e {
                // A tampered signature is either of these, depending on
                // which bits changed; both are a failed authentication.
                sig::SigError::VerificationFailed | sig::SigError::MalformedSignature => {
                    K::Unauthenticated
                }
                _ => K::InvalidInput,
            },
            #[cfg(feature = "store")]
            Error::Store(e) => {
                use store::StoreError as S;
                match e {
                    S::Http(h) => h.kind(),
                    S::Io(io) => Classify::kind(io),
                    S::InvalidDigest(_) | S::Config(_) => K::InvalidInput,
                    S::DigestMismatch { .. } | S::SizeMismatch(_) => K::Internal,
                    // A short or stalled transfer is parked for resumption.
                    S::Interrupted(_) => K::Unavailable,
                    // A response the adapter cannot use (or refuses, such as
                    // an https -> http redirect) does not change on retry.
                    S::Protocol(_) => K::InvalidInput,
                    S::Auth(_) => K::Unauthenticated,
                    S::Remote { status, .. } => match *status {
                        401 => K::Unauthenticated,
                        403 => K::PermissionDenied,
                        404 => K::NotFound,
                        408 => K::Timeout,
                        409 | 412 => K::Conflict,
                        429 => K::RateLimited,
                        400..=499 => K::InvalidInput,
                        500..=599 => K::Unavailable,
                        _ => K::Unknown,
                    },
                    _ => K::Unknown,
                }
            }
            #[cfg(feature = "cli")]
            Error::Cli(e) => e.kind(),
            #[cfg(feature = "protobuf")]
            Error::Proto(_) => K::InvalidInput,
            #[cfg(feature = "template")]
            Error::Template(e) => match e.kind() {
                template::TemplateErrorKind::Unsupported(_) => K::Unsupported,
                template::TemplateErrorKind::Function { .. } => K::Unknown,
                _ => K::InvalidInput,
            },
            #[cfg(feature = "grpc")]
            Error::Grpc(status) => {
                use grpc::Code as C;
                match status.code() {
                    C::InvalidArgument | C::OutOfRange => K::InvalidInput,
                    C::Unauthenticated => K::Unauthenticated,
                    C::PermissionDenied => K::PermissionDenied,
                    C::NotFound => K::NotFound,
                    C::AlreadyExists | C::Aborted | C::FailedPrecondition => K::Conflict,
                    C::Unimplemented => K::Unsupported,
                    C::ResourceExhausted => K::RateLimited,
                    C::DeadlineExceeded => K::Timeout,
                    C::Unavailable => K::Unavailable,
                    C::Internal | C::DataLoss => K::Internal,
                    _ => K::Unknown,
                }
            }
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

// Compile and run every example in the crate README as a doctest, so the page
// shown on crates.io cannot drift from the API. Examples marked `no_run` (they
// need a network or files) are compiled only.
#[cfg(all(doctest, feature = "full"))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

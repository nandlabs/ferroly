//! Content-addressed artifact cache and fetch adapters for common artifact
//! sources.
//!
//! The core is [`Cache`]: a directory of files keyed by their SHA-256
//! [`Digest`] (`<root>/sha256/ab/abcdef…`). [`Cache::get_or_fetch`] streams a
//! source into a temporary file inside the cache while hashing it, checks the
//! digest, and only then renames the file into place — so a path returned by
//! the cache always holds exactly the bytes its digest names. A hit is a
//! no-op, a transfer interrupted by an error or by cancellation resumes from
//! where it stopped when the source supports byte ranges (one cut short by
//! the process dying does not: see [`Cache::purge_temp`]), a mismatch is
//! deleted and reported, and concurrent fetches of one digest (from tasks or
//! from processes) never corrupt each other.
//!
//! The cache is usable on its own with any byte source ([`Fetched`]). The
//! adapters are separate features:
//!
//! | Feature | Module | Source |
//! |---|---|---|
//! | `store` | [`Cache`], [`Digest`], [`Fetched`] | anything that yields bytes; plain HTTP(S) URLs |
//! | `store-s3` | `store::s3` | S3-compatible object storage, AWS Signature Version 4 |
//! | `store-hf` | `store::hf` | Hugging Face Hub files (models, datasets, spaces) |
//! | `store-oci` | `store::oci` | OCI / Docker registries (Distribution Spec v2) |
//!
//! ```no_run
//! use ferroly::store::{Cache, Digest};
//! use ferroly::http::Client;
//!
//! # async fn demo() -> Result<(), ferroly::store::StoreError> {
//! let cache = Cache::open("/var/cache/my-app")?;
//! let digest: Digest =
//!     "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".parse()?;
//! // Downloads (resuming a partial file) and verifies on a miss; no I/O on a hit.
//! let path = cache
//!     .fetch_url(&Client::new(), "https://example.com/hello.txt", &digest)
//!     .await?;
//! println!("{}", path.display());
//! # Ok(())
//! # }
//! ```

#![deny(missing_docs)]

use ferroly_derive::FerrolyError;

mod cache;
mod digest;
mod fetch;

#[cfg(feature = "store-hf")]
pub mod hf;
#[cfg(feature = "store-oci")]
pub mod oci;
#[cfg(feature = "store-s3")]
pub mod s3;

#[cfg(any(feature = "store-hf", feature = "store-oci"))]
mod redirect;

pub use cache::{Cache, Evicted, Stored};
pub use digest::Digest;
pub use fetch::{fetch_http, BoxFuture, ByteStream, Fetched};

/// Errors raised by the cache and the fetch adapters.
#[derive(Debug, FerrolyError)]
#[non_exhaustive]
pub enum StoreError {
    /// A file-system operation on the cache failed.
    #[error("store i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// The HTTP exchange failed (connect, TLS, timeout, malformed message).
    #[error(transparent)]
    Http(#[from] ferroly::http::HttpError),

    /// A digest string could not be parsed (not `sha256:<64 hex>`).
    #[error("invalid digest: {0}")]
    InvalidDigest(String),

    /// The fetched content does not hash to the expected digest. The
    /// downloaded bytes were deleted.
    #[error("digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch {
        /// The digest that was asked for.
        expected: String,
        /// The digest of what was received.
        actual: String,
    },

    /// The fetched content does not have the expected size (or exceeds the
    /// configured cap). The downloaded bytes were deleted.
    #[error("size mismatch: {0}")]
    SizeMismatch(String),

    /// The transfer ended before the declared size, or a source stalled or
    /// ran past the cache's fetch deadline. What arrived of a digest-keyed
    /// download is kept and a later fetch resumes it, so this is worth
    /// retrying (unlike [`SizeMismatch`](StoreError::SizeMismatch)).
    #[error("transfer interrupted: {0}")]
    Interrupted(String),

    /// The remote service answered with an unexpected status.
    #[error("remote error {status}: {message}")]
    Remote {
        /// The HTTP status code.
        status: u16,
        /// A service error code when the body carried one (S3 `<Code>`, OCI
        /// `errors[].code`), otherwise empty.
        code: String,
        /// A short description (taken from the response body when possible,
        /// truncated).
        message: String,
    },

    /// The remote service sent a response this module cannot use (a bad
    /// `Content-Range`, a malformed or unsupported manifest or listing, too
    /// many redirects, a refused `https` → `http` redirect). Deterministic:
    /// retrying gets the same answer.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// Credentials are missing or a token could not be obtained.
    #[error("authentication failed: {0}")]
    Auth(String),

    /// An invalid argument or configuration (a bad bucket name, reference,
    /// endpoint or region).
    #[error("invalid configuration: {0}")]
    Config(String),
}

impl StoreError {
    /// Whether this is a `404 Not Found` answer from the remote service.
    pub fn is_not_found(&self) -> bool {
        matches!(self, StoreError::Remote { status: 404, .. })
    }

    /// The HTTP status of a [`Remote`](StoreError::Remote) error.
    pub fn status(&self) -> Option<u16> {
        match self {
            StoreError::Remote { status, .. } => Some(*status),
            _ => None,
        }
    }

    pub(crate) fn remote(status: u16, code: impl Into<String>, message: impl Into<String>) -> Self {
        StoreError::Remote {
            status,
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Reads an environment variable, treating unset and empty alike.
#[cfg(any(feature = "store-s3", feature = "store-hf"))]
pub(crate) fn env_lookup(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

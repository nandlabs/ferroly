//! Content-addressed artifact storage: a local store keyed by SHA-256, an
//! async download cache on top of it, and fetch adapters for common artifact
//! sources.
//!
//! The core is [`LocalStore`] (feature `store-local`): a directory of files
//! keyed by their SHA-256 [`Digest`], with a synchronous API and no async
//! runtime or HTTP stack. Objects live at `<root>/sha256/ab/abcdef…`
//! ([`Layout::Fanout`], the default) or `<root>/<prefix>/sha256/abcdef…`
//! ([`Layout::Flat`]). Content is written to a temporary file inside the
//! store while it is hashed and only renamed into place once it is complete,
//! so a path returned by the store always holds exactly the bytes its digest
//! names. [`LocalStore::list`], [`LocalStore::resolve_prefix`] (short digests),
//! [`LocalStore::check`] (re-hash without deleting) and a cross-process
//! [`StoreLock`] make it the blob store of a local registry that keeps its
//! own name → digest references.
//!
#![cfg_attr(
    feature = "store",
    doc = r#"
[`Cache`] (feature `store`) layers downloads on that core.
[`Cache::get_or_fetch`] streams a source into a temporary file inside the
cache while hashing it, checks the digest, and only then renames the file
into place. A hit is a no-op, a transfer interrupted by an error or by
cancellation resumes from where it stopped when the source supports byte
ranges (one cut short by the process dying does not: see
[`Cache::purge_temp`]), a mismatch is deleted and reported, and concurrent
fetches of one digest (from tasks or from processes) never corrupt each
other. The cache is usable on its own with any byte source ([`Fetched`]).
"#
)]
//!
//! One feature per layer and per adapter:
//!
//! | Feature | Module | Source |
//! |---|---|---|
//! | `store-local` | `LocalStore`, `Digest`, `Layout`, `StoreLock` | local files and readers; synchronous, no tokio, no HTTP |
//! | `store` | `Cache`, `Fetched` (implies `store-local`) | anything that yields bytes; plain HTTP(S) URLs |
//! | `store-s3` | `store::s3` | S3-compatible object storage, AWS Signature Version 4 |
//! | `store-hf` | `store::hf` | Hugging Face Hub files (models, datasets, spaces) |
//! | `store-oci` | `store::oci` | OCI / Docker registries (Distribution Spec v2) |
//!
//! ```
//! use ferroly::store::{Layout, LocalStore};
//!
//! # let dir = std::env::temp_dir().join(format!("ferroly-doc-store-{}", std::process::id()));
//! let store = LocalStore::open_with(&dir, Layout::flat("blobs")?)?;
//! let blob = store.insert_bytes(b"model weights")?;
//! assert_eq!(store.get(&blob.digest), Some(blob.path));
//! # std::fs::remove_dir_all(&dir).unwrap();
//! # Ok::<(), ferroly::store::StoreError>(())
//! ```
#![cfg_attr(
    feature = "store",
    doc = r#"
```no_run
use ferroly::store::{Cache, Digest};
use ferroly::http::Client;

# async fn demo() -> Result<(), ferroly::store::StoreError> {
let cache = Cache::open("/var/cache/my-app")?;
let digest: Digest =
    "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".parse()?;
// Downloads (resuming a partial file) and verifies on a miss; no I/O on a hit.
let path = cache
    .fetch_url(&Client::new(), "https://example.com/hello.txt", &digest)
    .await?;
println!("{}", path.display());
# Ok(())
# }
```
"#
)]
#![deny(missing_docs)]

use ferroly_derive::FerrolyError;

#[cfg(feature = "store")]
mod cache;
mod digest;
#[cfg(feature = "store")]
mod fetch;
mod layout;
mod local;

#[cfg(feature = "store-hf")]
pub mod hf;
#[cfg(feature = "store-oci")]
pub mod oci;
#[cfg(feature = "store-s3")]
pub mod s3;

#[cfg(any(feature = "store-hf", feature = "store-oci"))]
mod redirect;

#[cfg(feature = "store")]
pub use cache::Cache;
pub use digest::Digest;
#[cfg(feature = "store")]
pub use fetch::{fetch_http, BoxFuture, ByteStream, Fetched};
pub use layout::Layout;
pub use local::{Evicted, LocalStore, ObjectInfo, ObjectStatus, StoreLock, Stored, MIN_PREFIX_HEX};

/// Errors raised by the store, the cache and the fetch adapters.
#[derive(Debug, FerrolyError)]
#[non_exhaustive]
pub enum StoreError {
    /// A file-system operation on the store failed.
    #[error("store i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// The HTTP exchange failed (connect, TLS, timeout, malformed message).
    #[cfg(feature = "store")]
    #[error(transparent)]
    Http(#[from] ferroly::http::HttpError),

    /// A digest string could not be parsed (not `sha256:<64 hex>`), or a
    /// short digest given to [`LocalStore::resolve_prefix`] is malformed or
    /// too short.
    #[error("invalid digest: {0}")]
    InvalidDigest(String),

    /// No stored object matches a digest or digest prefix (the payload is
    /// what was looked for). Not a remote `404`; both answer
    /// [`is_not_found`](StoreError::is_not_found).
    #[error("not found in the store: {0}")]
    NotFound(String),

    /// A short digest matches more than one stored object.
    #[error("ambiguous digest prefix {prefix}: {matches} stored objects match")]
    AmbiguousPrefix {
        /// The prefix that was looked up (`sha256:<hex>`, lower case).
        prefix: String,
        /// The first matching digests, sorted (at most 8).
        candidates: Vec<Digest>,
        /// How many stored objects match in all.
        matches: usize,
    },

    /// The store lock is held by someone else (after
    /// [`LocalStore::lock_timeout`] gave up waiting). The payload is the
    /// lock file's path.
    #[error("store is locked: {0}")]
    Locked(String),

    /// The content does not hash to the expected digest. What was received
    /// was deleted.
    #[error("digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch {
        /// The digest that was asked for.
        expected: String,
        /// The digest of what was received.
        actual: String,
    },

    /// The content does not have the expected size (or exceeds the
    /// configured cap). What was received was deleted.
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
    /// Whether this is a `404 Not Found` answer from the remote service, or
    /// a [`NotFound`](StoreError::NotFound) from the local store.
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            StoreError::Remote { status: 404, .. } | StoreError::NotFound(_)
        )
    }

    /// The HTTP status of a [`Remote`](StoreError::Remote) error.
    pub fn status(&self) -> Option<u16> {
        match self {
            StoreError::Remote { status, .. } => Some(*status),
            _ => None,
        }
    }

    #[cfg(feature = "store")]
    pub(crate) fn remote(status: u16, code: impl Into<String>, message: impl Into<String>) -> Self {
        StoreError::Remote {
            status,
            code: code.into(),
            message: message.into(),
        }
    }
}

impl From<ferroly::fsutils::FsError> for StoreError {
    fn from(e: ferroly::fsutils::FsError) -> Self {
        use ferroly::fsutils::FsError;
        match e {
            FsError::Io(io) => StoreError::Io(io),
            FsError::Locked(path) => StoreError::Locked(path),
            other => StoreError::Io(std::io::Error::other(other.to_string())),
        }
    }
}

/// Reads an environment variable, treating unset and empty alike.
#[cfg(any(feature = "store-s3", feature = "store-hf"))]
pub(crate) fn env_lookup(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

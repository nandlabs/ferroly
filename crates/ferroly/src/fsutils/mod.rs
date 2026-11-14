//! Filesystem utilities (std-only): content-type detection, read-only file
//! views, atomic writes, temporary files, and an advisory lock file.
//!
//! Identify a file's content type — a **hint, not validation**:
//! - [`detect_content_type`] / [`sniff`] — guess a MIME type from leading bytes.
//! - [`lookup_content_type`] / [`mime_for_ext`] — resolve one from a filename
//!   extension.
//!
//! Read a whole file as bytes:
//! - [`Mmap::open`] — safe; reads the file into memory.
//! - [`Mmap::map`] — `unsafe`; a real lazily paged OS mapping, sound only if
//!   the file is not modified or truncated while mapped.
//!
//! Write and coordinate:
//! - [`write_atomic`] / [`AtomicFile`] — replace a file in one step (temporary
//!   sibling, flush to disk, rename).
//! - [`TempDir`] / [`TempFile`] — uniquely named scratch space, removed on drop.
//! - [`LockFile`] — an advisory lock held by a create-new file.
//!
//! Existence checks are intentionally omitted: Rust callers use
//! [`Path::exists`](std::path::Path::exists) / [`is_file`](std::path::Path::is_file) /
//! [`is_dir`](std::path::Path::is_dir) directly. Directory watching is out of
//! scope.
//!
//! ```
//! use ferroly::fsutils::lookup_content_type;
//! assert_eq!(lookup_content_type("config.json"), Some("application/json".to_string()));
//! ```

#![deny(missing_docs)]

use ferroly_derive::FerrolyError;

mod atomic;
mod lock;
mod mime;
// The crate's single `unsafe` region: the `mmap`/`munmap` FFI behind
// `Mmap::map`. Everything else in the workspace is compiled with
// `unsafe_code = "deny"`.
#[allow(unsafe_code)]
mod mmap;
mod temp;

pub use atomic::{write_atomic, AtomicFile};
pub use lock::LockFile;
pub use mime::{detect_content_type, lookup_content_type, mime_for_ext, sniff, SNIFF_LEN};
pub use mmap::Mmap;
pub use temp::{TempDir, TempFile};

/// Errors raised by the filesystem utilities.
#[derive(Debug, FerrolyError)]
#[non_exhaustive]
pub enum FsError {
    /// An underlying I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The content type could not be determined from the file's bytes.
    #[error("could not determine content type for: {0}")]
    Undetermined(String),

    /// A [`LockFile`] at this path is already held.
    #[error("lock is held: {0}")]
    Locked(String),
}

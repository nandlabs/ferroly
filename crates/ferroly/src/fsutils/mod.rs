//! Filesystem utilities (std-only): content-type detection, read-only file
//! views, human-readable byte sizes, atomic writes, temporary files, and
//! advisory file locks.
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
//! Show and parse sizes:
//! - [`size::format_bytes`] / [`size::format_bytes_si`] — `1.5 MiB`, `2.0 GB`.
//! - [`size::ByteSize`] — a byte count that parses from `512MiB` or `2G`
//!   (and, with `codec`, decodes from a config value).
//!
//! Write and coordinate:
//! - [`write_atomic`] / [`AtomicFile`] — replace a file in one step (temporary
//!   sibling, flush to disk, rename).
//! - [`TempDir`] / [`TempFile`] — uniquely named scratch space, removed on drop.
//! - [`FileLock`] — an OS file lock (`flock` / `LockFileEx`), released by the
//!   kernel when the holder dies.
//! - [`LockFile`] — an advisory lock held by a create-new file, for platforms
//!   or filesystems without OS locks.
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
// The `unsafe` regions in `fsutils`: the `mmap`/`munmap` FFI behind
// `Mmap::map`, and the `flock` / `LockFileEx` FFI behind `FileLock` (the
// crate's others are the Windows CSPRNG call in `hash/random/windows.rs` and
// the process-metrics FFI in `metrics/process.rs`).
// Everything else in the workspace is compiled with `unsafe_code = "deny"`.
#[allow(unsafe_code)]
mod mmap;
#[allow(unsafe_code)]
mod oslock;
pub mod size;
mod temp;

pub use atomic::{write_atomic, AtomicFile};
// The store's commit makes its renames durable with the same helper.
#[cfg(feature = "store-local")]
pub(crate) use atomic::sync_dir;
pub use lock::LockFile;
pub use mime::{detect_content_type, lookup_content_type, mime_for_ext, sniff, SNIFF_LEN};
pub use mmap::Mmap;
pub use oslock::FileLock;
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

    /// A [`LockFile`] or [`FileLock`] at this path is already held.
    #[error("lock is held: {0}")]
    Locked(String),
}

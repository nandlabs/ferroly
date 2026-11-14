//! A virtual filesystem — a [`Vfs`] trait, a local backend on tokio's
//! non-blocking `fs`, an in-memory backend, and helpers that work across
//! backends.
//!
//! Design choices:
//! - **One** method per operation, each taking a `&str` path — no string-twin
//!   overloads.
//! - I/O failures surface as a typed [`VfsError`] enum (`io::Error` kinds map
//!   onto it), not sentinel error values.
//! - Walk traversal is steered by a [`WalkAction`] return value rather than
//!   sentinel control-flow errors.
//! - Async work is cancelled by dropping the future, so there are no separate
//!   context-carrying method variants.
//!
//! What is here:
//! - [`LocalFs`] — the local disk. [`LocalFs::rooted`] confines every path to
//!   one directory; [`LocalFs::new`] is unconfined.
//! - [`MemoryFs`] — an in-memory tree with the same behavior, for tests.
//! - [`VfsRegistry`] — picks a backend by URL scheme.
//! - [`copy_between`], [`glob`], [`glob_match`] and (with the `hash` feature)
//!   `checksum_sha256` — helpers written against the trait.
//!
//! The trait is the extension point: object stores (S3, GCS, Azure) are not
//! bundled, but a type implementing [`Vfs`] plugs into everything above.
//!
//! ```no_run
//! # use ferroly::vfs::{LocalFs, Vfs};
//! # async fn ex() -> Result<(), ferroly::vfs::VfsError> {
//! // Confined to one directory: `..`, absolute paths and escaping symlinks
//! // are rejected.
//! let fs = LocalFs::rooted("/var/lib/my-app")?;
//! fs.write("hello.txt", b"hi".to_vec()).await?;
//! assert_eq!(fs.read("hello.txt").await?, b"hi");
//! assert!(fs.read("../../etc/passwd").await.is_err());
//! # Ok(())
//! # }
//! ```

#![deny(missing_docs)]

use std::future::Future;
use std::pin::Pin;
use std::time::SystemTime;

use ferroly_derive::FerrolyError;
use tokio::io::{AsyncRead, AsyncReadExt};

mod glob;
mod local;
mod memory;
mod path;
mod registry;

pub use glob::{glob, glob_match};
pub use local::LocalFs;
pub use memory::MemoryFs;
pub use path::scheme_of;
pub use registry::VfsRegistry;

/// A boxed, `Send` future — the object-safe async desugaring.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Errors raised by filesystem operations.
#[derive(Debug, FerrolyError)]
#[non_exhaustive]
pub enum VfsError {
    /// The path does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// Permission was denied.
    #[error("permission denied: {0}")]
    Permission(String),
    /// Expected a directory but the path (or one of its parents) is a file.
    #[error("not a directory: {0}")]
    NotDir(String),
    /// Expected a file but the path is a directory.
    #[error("is a directory: {0}")]
    IsDir(String),
    /// The operation is not supported by this backend.
    #[error("unsupported operation: {0}")]
    Unsupported(String),
    /// The path is not acceptable to this backend: a URL scheme it does not
    /// handle, or — for a confined backend — an absolute path, a `..`
    /// component, or a symbolic link that leads outside the root.
    #[error("invalid path {path}: {reason}")]
    InvalidPath {
        /// The path as the caller gave it.
        path: String,
        /// Why it was rejected.
        reason: String,
    },
    /// A lower-level I/O error.
    #[error("io error at {path}: {source}")]
    Io {
        /// The path involved.
        path: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
}

impl VfsError {
    /// Maps an [`std::io::Error`] onto a [`VfsError`]: `NotFound` and
    /// `PermissionDenied` become their typed variants, everything else is
    /// wrapped in [`VfsError::Io`]. Public so that other [`Vfs`] backends
    /// report errors the same way.
    pub fn from_io(path: &str, e: std::io::Error) -> Self {
        use std::io::ErrorKind::*;
        match e.kind() {
            NotFound => VfsError::NotFound(path.to_string()),
            PermissionDenied => VfsError::Permission(path.to_string()),
            _ => VfsError::Io {
                path: path.to_string(),
                source: e,
            },
        }
    }
}

/// Metadata about a filesystem node.
///
/// Fields a backend cannot supply are `None` / `false`. The struct is
/// `#[non_exhaustive]`: build one with [`Metadata::file`] or [`Metadata::dir`]
/// and the `with_*` methods.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Metadata {
    /// Whether the node is a directory.
    pub is_dir: bool,
    /// The size in bytes (0 for directories).
    pub len: u64,
    /// Whether the path itself is a symbolic link. The other fields describe
    /// the link's target.
    pub is_symlink: bool,
    /// When the contents were last modified, if the backend knows.
    pub modified: Option<SystemTime>,
    /// When the node was created, if the backend and platform record it.
    pub created: Option<SystemTime>,
    /// Whether the node is marked read-only.
    pub readonly: bool,
    /// Unix permission bits (`0o7777` mask), where they exist.
    pub mode: Option<u32>,
}

impl Metadata {
    /// Metadata for a file of `len` bytes.
    pub fn file(len: u64) -> Self {
        Metadata {
            len,
            ..Metadata::default()
        }
    }

    /// Metadata for a directory.
    pub fn dir() -> Self {
        Metadata {
            is_dir: true,
            ..Metadata::default()
        }
    }

    /// Whether the node is a regular file (not a directory).
    pub fn is_file(&self) -> bool {
        !self.is_dir
    }

    /// Sets the modification time.
    pub fn with_modified(mut self, at: SystemTime) -> Self {
        self.modified = Some(at);
        self
    }

    /// Sets the creation time.
    pub fn with_created(mut self, at: SystemTime) -> Self {
        self.created = Some(at);
        self
    }

    /// Sets the read-only flag.
    pub fn with_readonly(mut self, readonly: bool) -> Self {
        self.readonly = readonly;
        self
    }

    /// Sets the Unix permission bits.
    pub fn with_mode(mut self, mode: u32) -> Self {
        self.mode = Some(mode);
        self
    }

    /// Marks the path as a symbolic link.
    pub fn with_symlink(mut self, is_symlink: bool) -> Self {
        self.is_symlink = is_symlink;
        self
    }
}

/// A directory entry produced by [`Vfs::list`] / [`Vfs::walk`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The entry's path, in a form the same backend accepts back.
    pub path: String,
    /// The final path component.
    pub name: String,
    /// Whether the entry is a directory.
    pub is_dir: bool,
    /// The size in bytes (0 for directories).
    pub len: u64,
}

/// One page of a directory listing, from [`Vfs::list_page`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListPage {
    /// The entries on this page, ordered by name.
    pub entries: Vec<Entry>,
    /// The cursor to pass as `after` for the next page, or `None` if this was
    /// the last one.
    pub next: Option<String>,
}

/// What a [`walk`](Vfs::walk) callback decides after visiting an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkAction {
    /// Keep going (descend into directories).
    Continue,
    /// Skip descending into the just-visited directory.
    SkipDir,
    /// Stop the whole walk.
    Stop,
}

/// A callback invoked once per entry during a [`walk`](Vfs::walk).
pub type WalkFn = Box<dyn FnMut(&Entry) -> WalkAction + Send>;

/// An open file — a streaming async reader with attached [`Metadata`].
pub struct Vfile {
    inner: Box<dyn AsyncRead + Send + Unpin>,
    meta: Metadata,
}

impl Vfile {
    /// Wraps any async reader as an open file. This is how a [`Vfs`] backend
    /// builds the value it returns from [`Vfs::open`].
    pub fn new(reader: impl AsyncRead + Send + Unpin + 'static, meta: Metadata) -> Self {
        Vfile {
            inner: Box::new(reader),
            meta,
        }
    }

    /// An open file over bytes already in memory.
    pub fn from_bytes(data: Vec<u8>, meta: Metadata) -> Self {
        Vfile::new(std::io::Cursor::new(data), meta)
    }

    /// The file's metadata.
    pub fn metadata(&self) -> &Metadata {
        &self.meta
    }

    /// Reads the entire file into a byte vector.
    pub async fn read_all_bytes(mut self) -> Result<Vec<u8>, std::io::Error> {
        let mut buf = Vec::new();
        self.inner.read_to_end(&mut buf).await?;
        Ok(buf)
    }

    /// Reads the entire file into a UTF-8 string.
    pub async fn read_all_string(self) -> Result<String, std::io::Error> {
        let bytes = self.read_all_bytes().await?;
        String::from_utf8(bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

impl AsyncRead for Vfile {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

/// The backend half of a [`VfsWriter`]: where streamed bytes go.
///
/// Implement this to give a custom [`Vfs`] backend a streaming
/// [`create`](Vfs::create). Nothing may become visible at the destination
/// before [`finish`](WriteSink::finish); dropping the sink without finishing
/// must discard what was written.
pub trait WriteSink: Send {
    /// Appends `data` to the pending file.
    fn write_all<'s>(&'s mut self, data: &'s [u8]) -> BoxFuture<'s, Result<(), VfsError>>;

    /// Publishes everything written so far as the file's new contents.
    fn finish<'s>(self: Box<Self>) -> BoxFuture<'s, Result<(), VfsError>>
    where
        Self: 's;
}

/// A streaming writer returned by [`Vfs::create`].
///
/// Write the contents in as many pieces as you like, then call
/// [`finish`](VfsWriter::finish): only then does the file appear (or change)
/// at its path, in one step. Dropping the writer without finishing discards
/// everything and leaves any existing file untouched.
pub struct VfsWriter<'a> {
    sink: Box<dyn WriteSink + 'a>,
    written: u64,
}

impl<'a> VfsWriter<'a> {
    /// Wraps a backend's [`WriteSink`].
    pub fn new(sink: impl WriteSink + 'a) -> Self {
        VfsWriter {
            sink: Box::new(sink),
            written: 0,
        }
    }

    /// Appends `data` to the pending file.
    pub async fn write_all(&mut self, data: &[u8]) -> Result<(), VfsError> {
        self.sink.write_all(data).await?;
        self.written += data.len() as u64;
        Ok(())
    }

    /// The number of bytes written so far.
    pub fn bytes_written(&self) -> u64 {
        self.written
    }

    /// Publishes the file and returns the total number of bytes written.
    pub async fn finish(self) -> Result<u64, VfsError> {
        self.sink.finish().await?;
        Ok(self.written)
    }
}

/// The default [`WriteSink`]: buffers in memory and hands the whole buffer to
/// [`Vfs::write`] on finish.
struct BufferSink<'a, T: ?Sized> {
    fs: &'a T,
    path: String,
    buf: Vec<u8>,
}

impl<T: Vfs + ?Sized> WriteSink for BufferSink<'_, T> {
    fn write_all<'s>(&'s mut self, data: &'s [u8]) -> BoxFuture<'s, Result<(), VfsError>> {
        Box::pin(async move {
            self.buf.try_reserve(data.len()).map_err(|_| VfsError::Io {
                path: self.path.clone(),
                source: std::io::ErrorKind::OutOfMemory.into(),
            })?;
            self.buf.extend_from_slice(data);
            Ok(())
        })
    }

    fn finish<'s>(self: Box<Self>) -> BoxFuture<'s, Result<(), VfsError>>
    where
        Self: 's,
    {
        let this = *self;
        Box::pin(async move { this.fs.write(&this.path, this.buf).await })
    }
}

/// A virtual filesystem: one method per operation, `&str` paths, async.
///
/// This trait is the extension point for other storage systems. The required
/// methods are the core operations; the provided ones
/// ([`append`](Vfs::append), [`create`](Vfs::create),
/// [`list_page`](Vfs::list_page)) have correct-but-simple defaults built on
/// them, which a backend overrides when it can do better. To write a backend:
/// return [`Vfile::new`] from `open`, build [`Metadata`] with
/// [`Metadata::file`] / [`Metadata::dir`], map errors with
/// [`VfsError::from_io`], and reject URL schemes that are not yours with
/// [`VfsError::InvalidPath`].
pub trait Vfs: Send + Sync {
    /// The URL schemes this backend handles (e.g. `["file"]`). A
    /// [`VfsRegistry`] uses them to route paths.
    fn schemes(&self) -> &[&str];
    /// Reads a whole file.
    fn read(&self, path: &str) -> BoxFuture<'_, Result<Vec<u8>, VfsError>>;
    /// Reads up to `len` bytes starting at `offset`.
    ///
    /// The result is shorter than `len` when the file ends first, and empty
    /// when `offset` is at or beyond the end — neither is an error.
    /// Implementations must size their buffer by what is actually read, never
    /// by `len`, so a huge `len` is harmless.
    fn read_range(
        &self,
        path: &str,
        offset: u64,
        len: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, VfsError>>;
    /// Writes a whole file, creating or replacing it.
    ///
    /// Backends should make the replacement atomic — readers see the old
    /// contents or the new, never a mixture — and the bundled ones do.
    fn write(&self, path: &str, data: Vec<u8>) -> BoxFuture<'_, Result<(), VfsError>>;
    /// Opens a file as a streaming reader.
    fn open(&self, path: &str) -> BoxFuture<'_, Result<Vfile, VfsError>>;
    /// Deletes a file, or a directory and its contents.
    fn delete(&self, path: &str) -> BoxFuture<'_, Result<(), VfsError>>;
    /// Whether the path exists. Never errors: a path that cannot be examined
    /// (or is not acceptable to the backend) reports `false`.
    fn exists(&self, path: &str) -> BoxFuture<'_, bool>;
    /// Metadata for the path.
    fn metadata(&self, path: &str) -> BoxFuture<'_, Result<Metadata, VfsError>>;
    /// Lists one directory level.
    fn list(&self, path: &str) -> BoxFuture<'_, Result<Vec<Entry>, VfsError>>;
    /// Creates a directory and all missing parents.
    fn mkdir_all(&self, path: &str) -> BoxFuture<'_, Result<(), VfsError>>;
    /// Copies a file within this backend.
    fn copy(&self, src: &str, dst: &str) -> BoxFuture<'_, Result<(), VfsError>>;
    /// Renames/moves a path.
    fn rename(&self, src: &str, dst: &str) -> BoxFuture<'_, Result<(), VfsError>>;
    /// Recursively visits every entry under `path`, honoring [`WalkAction`].
    ///
    /// Entries are streamed to the callback as they are found, so this is
    /// also the way to list a huge directory without building a `Vec`: return
    /// [`WalkAction::SkipDir`] for every directory to stay on one level.
    fn walk(&self, path: &str, f: WalkFn) -> BoxFuture<'_, Result<(), VfsError>>;

    /// Appends `data` to a file, creating it if it does not exist.
    ///
    /// The default reads the file, appends in memory and writes it back —
    /// correct on any backend, but it costs the whole file and is not safe
    /// against a concurrent appender. Backends with a native append override
    /// it.
    fn append(&self, path: &str, data: Vec<u8>) -> BoxFuture<'_, Result<(), VfsError>> {
        let path = path.to_string();
        Box::pin(async move {
            let mut contents = match self.read(&path).await {
                Ok(existing) => existing,
                Err(VfsError::NotFound(_)) => Vec::new(),
                Err(e) => return Err(e),
            };
            contents.extend_from_slice(&data);
            self.write(&path, contents).await
        })
    }

    /// Starts a streaming write to `path`; the file appears (or is replaced)
    /// only when the returned [`VfsWriter`] is finished.
    ///
    /// The default buffers everything in memory and calls
    /// [`write`](Vfs::write) on finish, so it bounds nothing; backends that
    /// can stream to storage override it.
    fn create(&self, path: &str) -> BoxFuture<'_, Result<VfsWriter<'_>, VfsError>> {
        let path = path.to_string();
        Box::pin(async move {
            Ok(VfsWriter::new(BufferSink {
                fs: self,
                path,
                buf: Vec::new(),
            }))
        })
    }

    /// Lists one directory level a page at a time, ordered by entry name.
    ///
    /// Returns up to `limit` entries (at least one is always allowed) whose
    /// names sort after `after`; pass the returned [`ListPage::next`] as
    /// `after` to continue, until it is `None`. Paging is by name, so entries
    /// created or removed between calls are picked up or skipped according to
    /// where they sort — it is not a snapshot.
    ///
    /// The default calls [`list`](Vfs::list) and slices the result.
    fn list_page(
        &self,
        path: &str,
        after: Option<String>,
        limit: usize,
    ) -> BoxFuture<'_, Result<ListPage, VfsError>> {
        let path = path.to_string();
        Box::pin(async move {
            let mut entries = self.list(&path).await?;
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(page_of(entries, after.as_deref(), limit))
        })
    }
}

/// Cuts one page out of entries already sorted by name.
pub(crate) fn page_of(sorted: Vec<Entry>, after: Option<&str>, limit: usize) -> ListPage {
    let limit = limit.max(1);
    let mut remaining = sorted
        .into_iter()
        .filter(|e| after.map_or(true, |a| e.name.as_str() > a));
    let entries: Vec<Entry> = remaining.by_ref().take(limit).collect();
    let next = match (remaining.next(), entries.last()) {
        (Some(_), Some(last)) => Some(last.name.clone()),
        _ => None,
    };
    ListPage { entries, next }
}

/// How much is read per step when streaming between backends.
const COPY_CHUNK: usize = 64 * 1024;

/// Copies one file from `src_path` on `src` to `dst_path` on `dst`, streaming
/// it in chunks so the file is never held in memory whole (given backends
/// whose `open` and `create` stream).
///
/// The two may be different backends — that is the point — or the same one.
/// The destination appears only once the copy has completed; on error nothing
/// is left there. Returns the number of bytes copied.
///
/// ```
/// use ferroly::vfs::{copy_between, MemoryFs, Vfs};
///
/// # async fn demo() -> Result<(), ferroly::vfs::VfsError> {
/// let a = MemoryFs::new();
/// let b = MemoryFs::new();
/// a.write("/report.csv", b"id,total\n1,10\n".to_vec()).await?;
/// let copied = copy_between(&a, "/report.csv", &b, "/inbox.csv").await?;
/// assert_eq!(copied, 14);
/// assert_eq!(b.read("/inbox.csv").await?, b"id,total\n1,10\n");
/// # Ok(())
/// # }
/// ```
pub async fn copy_between(
    src: &dyn Vfs,
    src_path: &str,
    dst: &dyn Vfs,
    dst_path: &str,
) -> Result<u64, VfsError> {
    let mut reader = src.open(src_path).await?;
    let mut writer = dst.create(dst_path).await?;
    let mut chunk = vec![0u8; COPY_CHUNK];
    loop {
        let n = reader
            .read(&mut chunk)
            .await
            .map_err(|e| VfsError::from_io(src_path, e))?;
        if n == 0 {
            break;
        }
        writer.write_all(&chunk[..n]).await?;
    }
    writer.finish().await
}

/// Computes the SHA-256 digest of a file's contents, streaming it in chunks.
///
/// Requires the `hash` feature in addition to `vfs`.
///
/// ```
/// use ferroly::vfs::{checksum_sha256, MemoryFs, Vfs};
///
/// # async fn demo() -> Result<(), ferroly::vfs::VfsError> {
/// let fs = MemoryFs::new();
/// fs.write("/a.txt", b"abc".to_vec()).await?;
/// let digest = checksum_sha256(&fs, "/a.txt").await?;
/// assert!(digest.to_hex().starts_with("ba7816bf"));
/// # Ok(())
/// # }
/// ```
#[cfg(feature = "hash")]
pub async fn checksum_sha256(
    fs: &dyn Vfs,
    path: &str,
) -> Result<ferroly::hash::Digest<32>, VfsError> {
    let mut reader = fs.open(path).await?;
    let mut hasher = ferroly::hash::Sha256::new();
    let mut chunk = vec![0u8; COPY_CHUNK];
    loop {
        let n = reader
            .read(&mut chunk)
            .await
            .map_err(|e| VfsError::from_io(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&chunk[..n]);
    }
    Ok(hasher.finalize())
}

#[cfg(test)]
pub(crate) mod testutil {
    //! A self-cleaning temp directory for the vfs tests (the `fsutils` guard
    //! lives behind a different feature).

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(crate) struct TempRoot(PathBuf);

    impl TempRoot {
        pub(crate) fn new(tag: &str) -> TempRoot {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "ferroly-vfs-{tag}-{}-{nanos}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            // Canonical on Unix, so comparisons survive a symlinked temp dir
            // (`/tmp` -> `/private/tmp` on macOS). Not on Windows, where the
            // canonical form is a `\\?\` path that does not accept the `/`
            // separators these tests build paths with.
            #[cfg(unix)]
            let path = std::fs::canonicalize(&path).unwrap();
            TempRoot(path)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }

        /// A path under the root, as a string.
        pub(crate) fn join(&self, rel: &str) -> String {
            self.0.join(rel).to_string_lossy().into_owned()
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> Entry {
        Entry {
            path: format!("/{name}"),
            name: name.to_string(),
            is_dir: false,
            len: 0,
        }
    }

    #[test]
    fn page_of_walks_every_entry_once() {
        let all: Vec<Entry> = ["a", "b", "c", "d", "e"].into_iter().map(entry).collect();
        let mut seen = Vec::new();
        let mut after: Option<String> = None;
        let mut pages = 0;
        loop {
            let page = page_of(all.clone(), after.as_deref(), 2);
            pages += 1;
            seen.extend(page.entries.iter().map(|e| e.name.clone()));
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        assert_eq!(seen, ["a", "b", "c", "d", "e"]);
        assert_eq!(pages, 3);

        // An exact multiple ends without an extra empty page.
        let page = page_of(all.clone(), Some("c"), 2);
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.next, None);
        // A zero limit is treated as one.
        let page = page_of(all.clone(), None, 0);
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.next.as_deref(), Some("a"));
        // Past the end.
        assert_eq!(page_of(all, Some("z"), 10), ListPage::default());
    }

    #[test]
    fn metadata_builders() {
        let now = SystemTime::now();
        let m = Metadata::file(7)
            .with_modified(now)
            .with_created(now)
            .with_readonly(true)
            .with_mode(0o640)
            .with_symlink(true);
        assert!(m.is_file());
        assert_eq!(m.len, 7);
        assert_eq!(m.modified, Some(now));
        assert_eq!(m.created, Some(now));
        assert!(m.readonly && m.is_symlink);
        assert_eq!(m.mode, Some(0o640));
        assert!(Metadata::dir().is_dir);
        assert!(!Metadata::dir().is_file());
    }

    #[tokio::test]
    async fn vfile_from_bytes_streams() {
        let file = Vfile::from_bytes(b"hello".to_vec(), Metadata::file(5));
        assert_eq!(file.metadata().len, 5);
        assert_eq!(file.read_all_string().await.unwrap(), "hello");
    }
}

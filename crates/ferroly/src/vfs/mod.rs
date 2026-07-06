//! A virtual filesystem — a [`Vfs`] trait plus a local backend on tokio's
//! non-blocking `fs`.
//!
//! Design choices:
//! - **One** method per operation, each taking a `&str` path (a `file://`
//!   scheme is stripped) — no string-twin overloads.
//! - I/O failures surface as a typed [`VfsError`] enum (`io::Error` kinds map
//!   onto it), not sentinel error values.
//! - Walk traversal is steered by a [`WalkAction`] return value rather than
//!   sentinel control-flow errors.
//! - Async work is cancelled by dropping the future, so there are no separate
//!   context-carrying method variants.
//!
//! Cloud backends (S3, GCS) implement the same trait in satellite crates.
//!
//! ```no_run
//! # use ferroly::vfs::{LocalFs, Vfs};
//! # async fn ex() -> Result<(), ferroly::vfs::VfsError> {
//! let fs = LocalFs::new();
//! fs.write("file:///tmp/hello.txt", b"hi".to_vec()).await?;
//! let bytes = fs.read("/tmp/hello.txt").await?;
//! assert_eq!(bytes, b"hi");
//! # Ok(())
//! # }
//! ```

#![deny(missing_docs)]

use std::future::Future;
use std::pin::Pin;

use ferroly_derive::FerrolyError;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt};

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
    /// Expected a directory but the path is a file.
    #[error("not a directory: {0}")]
    NotDir(String),
    /// Expected a file but the path is a directory.
    #[error("is a directory: {0}")]
    IsDir(String),
    /// The operation is not supported by this backend.
    #[error("unsupported operation: {0}")]
    Unsupported(String),
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
    fn from_io(path: &str, e: std::io::Error) -> Self {
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    /// Whether the node is a directory.
    pub is_dir: bool,
    /// The size in bytes (0 for directories).
    pub len: u64,
}

/// A directory entry produced by [`Vfs::list`] / [`Vfs::walk`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The full path.
    pub path: String,
    /// The final path component.
    pub name: String,
    /// Whether the entry is a directory.
    pub is_dir: bool,
    /// The size in bytes.
    pub len: u64,
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

/// A virtual filesystem: one method per operation, `&str` paths, async.
pub trait Vfs: Send + Sync {
    /// The URL schemes this backend handles (e.g. `["file"]`).
    fn schemes(&self) -> &[&str];
    /// Reads a whole file.
    fn read(&self, path: &str) -> BoxFuture<'_, Result<Vec<u8>, VfsError>>;
    /// Reads `len` bytes starting at `offset`.
    fn read_range(
        &self,
        path: &str,
        offset: u64,
        len: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, VfsError>>;
    /// Writes a whole file, creating or truncating it.
    fn write(&self, path: &str, data: Vec<u8>) -> BoxFuture<'_, Result<(), VfsError>>;
    /// Opens a file as a streaming reader.
    fn open(&self, path: &str) -> BoxFuture<'_, Result<Vfile, VfsError>>;
    /// Deletes a file, or a directory and its contents.
    fn delete(&self, path: &str) -> BoxFuture<'_, Result<(), VfsError>>;
    /// Whether the path exists.
    fn exists(&self, path: &str) -> BoxFuture<'_, bool>;
    /// Metadata for the path.
    fn metadata(&self, path: &str) -> BoxFuture<'_, Result<Metadata, VfsError>>;
    /// Lists one directory level.
    fn list(&self, path: &str) -> BoxFuture<'_, Result<Vec<Entry>, VfsError>>;
    /// Creates a directory and all missing parents.
    fn mkdir_all(&self, path: &str) -> BoxFuture<'_, Result<(), VfsError>>;
    /// Copies a file.
    fn copy(&self, src: &str, dst: &str) -> BoxFuture<'_, Result<(), VfsError>>;
    /// Renames/moves a path.
    fn rename(&self, src: &str, dst: &str) -> BoxFuture<'_, Result<(), VfsError>>;
    /// Recursively visits every entry under `path`, honoring [`WalkAction`].
    fn walk(&self, path: &str, f: WalkFn) -> BoxFuture<'_, Result<(), VfsError>>;
}

/// Strips a `file://` (or any `scheme://`) prefix, yielding the path.
fn path_of(addr: &str) -> &str {
    addr.split_once("://").map(|(_, p)| p).unwrap_or(addr)
}

// ---- local backend -------------------------------------------------------

/// A [`Vfs`] backed by the local filesystem via tokio's non-blocking `fs`.
#[derive(Debug, Default, Clone)]
pub struct LocalFs;

impl LocalFs {
    /// Creates a local filesystem backend.
    pub fn new() -> Self {
        Self
    }
}

impl Vfs for LocalFs {
    fn schemes(&self) -> &[&str] {
        &["file"]
    }

    fn read(&self, path: &str) -> BoxFuture<'_, Result<Vec<u8>, VfsError>> {
        let p = path_of(path).to_string();
        Box::pin(async move {
            tokio::fs::read(&p)
                .await
                .map_err(|e| VfsError::from_io(&p, e))
        })
    }

    fn read_range(
        &self,
        path: &str,
        offset: u64,
        len: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, VfsError>> {
        let p = path_of(path).to_string();
        Box::pin(async move {
            let mut f = tokio::fs::File::open(&p)
                .await
                .map_err(|e| VfsError::from_io(&p, e))?;
            f.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(|e| VfsError::from_io(&p, e))?;
            let mut buf = vec![0u8; len as usize];
            let mut read = 0usize;
            while read < buf.len() {
                let n = f
                    .read(&mut buf[read..])
                    .await
                    .map_err(|e| VfsError::from_io(&p, e))?;
                if n == 0 {
                    break; // reached EOF before `len`
                }
                read += n;
            }
            buf.truncate(read);
            Ok(buf)
        })
    }

    fn write(&self, path: &str, data: Vec<u8>) -> BoxFuture<'_, Result<(), VfsError>> {
        let p = path_of(path).to_string();
        Box::pin(async move {
            tokio::fs::write(&p, data)
                .await
                .map_err(|e| VfsError::from_io(&p, e))
        })
    }

    fn open(&self, path: &str) -> BoxFuture<'_, Result<Vfile, VfsError>> {
        let p = path_of(path).to_string();
        Box::pin(async move {
            let file = tokio::fs::File::open(&p)
                .await
                .map_err(|e| VfsError::from_io(&p, e))?;
            let m = file
                .metadata()
                .await
                .map_err(|e| VfsError::from_io(&p, e))?;
            Ok(Vfile {
                meta: Metadata {
                    is_dir: m.is_dir(),
                    len: m.len(),
                },
                inner: Box::new(file),
            })
        })
    }

    fn delete(&self, path: &str) -> BoxFuture<'_, Result<(), VfsError>> {
        let p = path_of(path).to_string();
        Box::pin(async move {
            let m = tokio::fs::metadata(&p)
                .await
                .map_err(|e| VfsError::from_io(&p, e))?;
            if m.is_dir() {
                tokio::fs::remove_dir_all(&p).await
            } else {
                tokio::fs::remove_file(&p).await
            }
            .map_err(|e| VfsError::from_io(&p, e))
        })
    }

    fn exists(&self, path: &str) -> BoxFuture<'_, bool> {
        let p = path_of(path).to_string();
        Box::pin(async move { tokio::fs::metadata(&p).await.is_ok() })
    }

    fn metadata(&self, path: &str) -> BoxFuture<'_, Result<Metadata, VfsError>> {
        let p = path_of(path).to_string();
        Box::pin(async move {
            let m = tokio::fs::metadata(&p)
                .await
                .map_err(|e| VfsError::from_io(&p, e))?;
            Ok(Metadata {
                is_dir: m.is_dir(),
                len: m.len(),
            })
        })
    }

    fn list(&self, path: &str) -> BoxFuture<'_, Result<Vec<Entry>, VfsError>> {
        let p = path_of(path).to_string();
        Box::pin(async move {
            let mut rd = tokio::fs::read_dir(&p)
                .await
                .map_err(|e| VfsError::from_io(&p, e))?;
            let mut out = Vec::new();
            while let Some(e) = rd
                .next_entry()
                .await
                .map_err(|e| VfsError::from_io(&p, e))?
            {
                out.push(entry_from(&e).await?);
            }
            Ok(out)
        })
    }

    fn mkdir_all(&self, path: &str) -> BoxFuture<'_, Result<(), VfsError>> {
        let p = path_of(path).to_string();
        Box::pin(async move {
            tokio::fs::create_dir_all(&p)
                .await
                .map_err(|e| VfsError::from_io(&p, e))
        })
    }

    fn copy(&self, src: &str, dst: &str) -> BoxFuture<'_, Result<(), VfsError>> {
        let s = path_of(src).to_string();
        let d = path_of(dst).to_string();
        Box::pin(async move {
            tokio::fs::copy(&s, &d)
                .await
                .map(|_| ())
                .map_err(|e| VfsError::from_io(&s, e))
        })
    }

    fn rename(&self, src: &str, dst: &str) -> BoxFuture<'_, Result<(), VfsError>> {
        let s = path_of(src).to_string();
        let d = path_of(dst).to_string();
        Box::pin(async move {
            tokio::fs::rename(&s, &d)
                .await
                .map_err(|e| VfsError::from_io(&s, e))
        })
    }

    fn walk(&self, path: &str, mut f: WalkFn) -> BoxFuture<'_, Result<(), VfsError>> {
        let root = path_of(path).to_string();
        Box::pin(async move {
            let mut stack = vec![root];
            while let Some(dir) = stack.pop() {
                let mut rd = match tokio::fs::read_dir(&dir).await {
                    Ok(rd) => rd,
                    Err(e) => return Err(VfsError::from_io(&dir, e)),
                };
                while let Some(de) = rd
                    .next_entry()
                    .await
                    .map_err(|e| VfsError::from_io(&dir, e))?
                {
                    let entry = entry_from(&de).await?;
                    match f(&entry) {
                        WalkAction::Stop => return Ok(()),
                        WalkAction::SkipDir => {}
                        WalkAction::Continue => {
                            if entry.is_dir {
                                stack.push(entry.path.clone());
                            }
                        }
                    }
                }
            }
            Ok(())
        })
    }
}

async fn entry_from(de: &tokio::fs::DirEntry) -> Result<Entry, VfsError> {
    let path = de.path().to_string_lossy().into_owned();
    let m = de
        .metadata()
        .await
        .map_err(|e| VfsError::from_io(&path, e))?;
    Ok(Entry {
        name: de.file_name().to_string_lossy().into_owned(),
        is_dir: m.is_dir(),
        len: m.len(),
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique temp directory under the OS temp dir (no external deps).
    fn temp_dir(tag: &str) -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ferroly-vfs-{tag}-{n}"));
        dir.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn write_read_range_list_walk_delete() {
        let fs = LocalFs::new();
        let base = temp_dir("main");
        fs.mkdir_all(&base).await.unwrap();

        // write + read
        let file = format!("{base}/a.txt");
        fs.write(&file, b"hello world".to_vec()).await.unwrap();
        assert_eq!(fs.read(&file).await.unwrap(), b"hello world");
        assert!(fs.exists(&file).await);
        assert_eq!(fs.metadata(&file).await.unwrap().len, 11);

        // range read
        assert_eq!(fs.read_range(&file, 6, 5).await.unwrap(), b"world");

        // open + streaming read
        let vf = fs.open(&file).await.unwrap();
        assert!(!vf.metadata().is_dir);
        assert_eq!(vf.read_all_string().await.unwrap(), "hello world");

        // subdir + list
        let sub = format!("{base}/sub");
        fs.mkdir_all(&sub).await.unwrap();
        fs.write(&format!("{sub}/b.txt"), b"x".to_vec())
            .await
            .unwrap();
        let entries = fs.list(&base).await.unwrap();
        assert_eq!(entries.len(), 2); // a.txt + sub/

        // walk with SkipDir: should not descend into `sub`.
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let s2 = seen.clone();
        fs.walk(
            &base,
            Box::new(move |e: &Entry| {
                s2.lock().unwrap().push(e.name.clone());
                if e.is_dir {
                    WalkAction::SkipDir
                } else {
                    WalkAction::Continue
                }
            }),
        )
        .await
        .unwrap();
        let names = seen.lock().unwrap().clone();
        assert!(names.contains(&"a.txt".to_string()));
        assert!(names.contains(&"sub".to_string()));
        assert!(!names.contains(&"b.txt".to_string())); // skipped

        // rename + copy
        fs.rename(&file, &format!("{base}/a2.txt")).await.unwrap();
        assert!(!fs.exists(&file).await);
        fs.copy(&format!("{base}/a2.txt"), &format!("{base}/a3.txt"))
            .await
            .unwrap();
        assert!(fs.exists(&format!("{base}/a3.txt")).await);

        // scheme-prefixed paths work too
        fs.write(&format!("file://{base}/c.txt"), b"z".to_vec())
            .await
            .unwrap();
        assert_eq!(fs.read(&format!("{base}/c.txt")).await.unwrap(), b"z");

        // delete the tree
        fs.delete(&base).await.unwrap();
        assert!(!fs.exists(&base).await);
    }

    #[tokio::test]
    async fn not_found_maps_to_sentinel() {
        let fs = LocalFs::new();
        let err = fs.read("/no/such/ferroly/path/xyz").await.unwrap_err();
        assert!(matches!(err, VfsError::NotFound(_)));
    }
}

//! [`LocalFs`] — the local-disk backend.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use super::path::strip_scheme;
use super::{
    BoxFuture, Entry, ListPage, Metadata, Vfile, Vfs, VfsError, VfsWriter, WalkAction, WalkFn,
    WriteSink,
};

/// A [`Vfs`] backed by the local filesystem via tokio's non-blocking `fs`.
///
/// There are two flavors, and the difference matters for anything that
/// handles paths it did not make up itself:
///
/// * [`LocalFs::rooted`] confines every operation to one directory. Paths are
///   relative to that root; `..` components, absolute paths, and symbolic
///   links that lead outside the root are rejected with
///   [`VfsError::InvalidPath`]. **Use this for user-supplied paths.**
/// * [`LocalFs::new`] is unconfined: a path means what it means to the
///   operating system, so a caller-controlled path can reach anything the
///   process can.
///
/// Both accept a plain path or a `file://` URL and reject every other URL
/// scheme — `s3://bucket/key` is an error, not a relative path named `s3:`.
/// `file://` URLs are not percent-decoded.
///
/// # Writes are atomic
///
/// [`write`](Vfs::write) and a finished [`create`](Vfs::create) write a
/// temporary sibling (`.<name>.<unique>.tmp`), flush it to disk, and rename it
/// over the destination, so a reader — or a crash — sees the old file or the
/// new one, never part of each. The consequences: the destination's directory
/// must be writable; the file gets a new inode (hard links keep the old
/// contents; on Unix the previous permission bits are carried over, ownership
/// is not); and a destination marked read-only is refused rather than
/// replaced. A symbolic link at the destination is followed, so the link's
/// target is what gets replaced. [`append`](Vfs::append) writes in place and
/// is not atomic.
///
/// # What confinement does not cover
///
/// The root check resolves symbolic links *before* the operation runs and the
/// operation then uses the resolved path. Between the two, another process
/// with write access inside the root could swap a directory for a symlink
/// (a time-of-check/time-of-use race). Closing that window needs
/// `openat2(RESOLVE_BENEATH)`-style system calls, which this crate — having no
/// `libc` and no `unsafe` here — does not make. So: confinement holds against
/// hostile *paths*; it is not a sandbox against a hostile process that can
/// already modify the tree. Hard links to files outside the root, created by
/// such a process, are likewise not detected.
#[derive(Debug, Clone, Default)]
pub struct LocalFs {
    /// The canonical root directory, if confined.
    root: Option<Arc<PathBuf>>,
}

/// Where an address points: the real path to operate on, and the path to
/// show in errors and entries (what the caller would recognize).
#[derive(Debug, Clone)]
struct Loc {
    real: PathBuf,
    shown: String,
}

/// What an operation needs its path to be, used to turn a raw I/O failure
/// into [`VfsError::IsDir`] / [`VfsError::NotDir`].
#[derive(Clone, Copy)]
enum Expect {
    File,
    Dir,
    Any,
}

impl LocalFs {
    /// Creates an **unconfined** local filesystem backend: paths are passed to
    /// the operating system as given. Prefer [`LocalFs::rooted`] whenever a
    /// path can be influenced by a user or a remote party.
    pub fn new() -> Self {
        Self { root: None }
    }

    /// Creates a backend confined to `dir`, which must be an existing
    /// directory. Every path is then relative to it:
    ///
    /// * `"a/b.txt"`, `"./a/b.txt"` and `"file://a/b.txt"` name
    ///   `<dir>/a/b.txt`; `""` and `"."` name the root itself;
    /// * a `..` component, an absolute path, or (on Windows) a drive prefix is
    ///   rejected, even if it would not actually leave the root;
    /// * a symbolic link is followed only if it resolves inside the root — a
    ///   link that points outside, or dangles, is rejected;
    /// * entries returned by [`list`](Vfs::list) / [`walk`](Vfs::walk) carry
    ///   root-relative paths with `/` separators, ready to pass back in.
    ///
    /// See the type-level docs for what confinement does not protect against.
    ///
    /// ```
    /// use ferroly::vfs::{LocalFs, Vfs, VfsError};
    ///
    /// # async fn demo() -> Result<(), VfsError> {
    /// # let dir = std::env::temp_dir();
    /// let fs = LocalFs::rooted(&dir)?;
    /// assert!(matches!(
    ///     fs.read("../outside.txt").await,
    ///     Err(VfsError::InvalidPath { .. })
    /// ));
    /// assert!(matches!(
    ///     fs.read("/etc/passwd").await,
    ///     Err(VfsError::InvalidPath { .. })
    /// ));
    /// # Ok(())
    /// # }
    /// ```
    pub fn rooted(dir: impl AsRef<Path>) -> Result<Self, VfsError> {
        let dir = dir.as_ref();
        let shown = dir.to_string_lossy();
        let canonical = std::fs::canonicalize(dir).map_err(|e| VfsError::from_io(&shown, e))?;
        let meta = std::fs::metadata(&canonical).map_err(|e| VfsError::from_io(&shown, e))?;
        if !meta.is_dir() {
            return Err(VfsError::NotDir(shown.into_owned()));
        }
        Ok(Self {
            root: Some(Arc::new(canonical)),
        })
    }

    /// The canonical root directory if this backend is confined.
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref().map(PathBuf::as_path)
    }

    /// Resolves an address to a location.
    ///
    /// `follow` says whether the final component may be a symbolic link that
    /// is followed (reads, writes) or names the entry itself (delete, rename).
    async fn locate(&self, addr: &str, follow: bool) -> Result<Loc, VfsError> {
        let local = local_part(addr)?;
        let Some(root) = &self.root else {
            return Ok(Loc {
                real: PathBuf::from(local),
                shown: local.to_string(),
            });
        };
        let parts = relative_parts(addr, local)?;
        let shown = if parts.is_empty() {
            ".".to_string()
        } else {
            parts.join("/")
        };
        let root = root.clone();
        let label = shown.clone();
        let real = tokio::task::spawn_blocking(move || confine(&root, &parts, follow, &label))
            .await
            .map_err(|e| VfsError::Io {
                path: shown.clone(),
                source: std::io::Error::other(e),
            })??;
        Ok(Loc { real, shown })
    }

    /// The location of `name` inside directory `dir`.
    fn child(&self, dir: &Loc, name: &str) -> Loc {
        let real = dir.real.join(name);
        let shown = if self.root.is_none() {
            real.to_string_lossy().into_owned()
        } else if dir.shown == "." {
            name.to_string()
        } else {
            format!("{}/{name}", dir.shown)
        };
        Loc { real, shown }
    }

    fn is_root(&self, loc: &Loc) -> bool {
        self.root.is_some() && loc.shown == "."
    }
}

/// Strips an optional `file://` prefix; any other scheme is an error.
fn local_part(addr: &str) -> Result<&str, VfsError> {
    let rest = strip_scheme(addr, &["file"], "LocalFs")?;
    if rest.len() == addr.len() {
        return Ok(rest); // no scheme
    }
    // `file://localhost/path` names the same file as `file:///path`.
    let rest = match rest.strip_prefix("localhost") {
        Some(after) if after.starts_with('/') => after,
        _ => rest,
    };
    // `file:///C:/dir` — drop the slash in front of a Windows drive letter.
    #[cfg(windows)]
    let rest = {
        let bytes = rest.as_bytes();
        if bytes.len() >= 3
            && bytes[0] == b'/'
            && bytes[1].is_ascii_alphabetic()
            && bytes[2] == b':'
        {
            &rest[1..]
        } else {
            rest
        }
    };
    Ok(rest)
}

/// Splits a root-relative path into its normal components, rejecting anything
/// that is not plainly "down from the root".
fn relative_parts(addr: &str, local: &str) -> Result<Vec<String>, VfsError> {
    let invalid = |reason: &str| VfsError::InvalidPath {
        path: addr.to_string(),
        reason: reason.to_string(),
    };
    if local.contains('\0') {
        return Err(invalid("path contains a NUL byte"));
    }
    let mut parts = Vec::new();
    for component in Path::new(local).components() {
        match component {
            Component::Normal(part) => match part.to_str() {
                Some(part) => parts.push(part.to_string()),
                None => return Err(invalid("path is not valid UTF-8")),
            },
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(invalid("`..` is not allowed in a rooted filesystem"))
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(invalid(
                    "absolute paths are not allowed in a rooted filesystem",
                ))
            }
        }
    }
    Ok(parts)
}

/// Maps root-relative `parts` to a real path that is guaranteed (as of this
/// call) to lie inside `root`.
///
/// The deepest ancestor that exists is canonicalized — which resolves every
/// symbolic link on the way — and must still start with the canonical root.
/// Whatever lies below that ancestor does not exist yet, so it cannot contain
/// a link. With `follow == false` the final component is left alone (it is
/// the entry being removed or renamed, not something to look through).
fn confine(root: &Path, parts: &[String], follow: bool, shown: &str) -> Result<PathBuf, VfsError> {
    let (checked, last) = match parts.split_last() {
        Some((last, parents)) if !follow => (parents, Some(last)),
        _ => (parts, None),
    };
    let mut full = root.to_path_buf();
    full.extend(checked);

    // Find the deepest ancestor that exists. `symlink_metadata` does not
    // follow the final link, so a dangling link counts as existing and is
    // then rejected by `canonicalize` instead of being written through.
    let mut existing = full.as_path();
    let mut missing: Vec<&std::ffi::OsStr> = Vec::new();
    while existing != root && std::fs::symlink_metadata(existing).is_err() {
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                missing.push(name);
                existing = parent;
            }
            _ => break,
        }
    }

    let canonical = std::fs::canonicalize(existing).map_err(|e| {
        let is_link = std::fs::symlink_metadata(existing)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        if is_link {
            VfsError::InvalidPath {
                path: shown.to_string(),
                reason: "symbolic link does not resolve inside the root".to_string(),
            }
        } else {
            VfsError::from_io(shown, e)
        }
    })?;
    if !canonical.starts_with(root) {
        return Err(VfsError::InvalidPath {
            path: shown.to_string(),
            reason: "path escapes the root through a symbolic link".to_string(),
        });
    }

    let mut real = canonical;
    real.extend(missing.iter().rev());
    if let Some(last) = last {
        real.push(last);
    }
    Ok(real)
}

/// Runs blocking filesystem work off the async executor.
async fn blocking<T, F>(f: F) -> std::io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> std::io::Result<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(e) => Err(std::io::Error::other(e)),
    }
}

/// Turns a failed operation into the most specific [`VfsError`].
///
/// The `io::ErrorKind`s for "is a directory" / "not a directory" cannot be
/// named at this crate's minimum Rust version and differ between platforms
/// (Windows reports "access denied" or "not found"), so the decision is made
/// by looking at what is actually on disk.
async fn classify(loc: &Loc, e: std::io::Error, expect: Expect) -> VfsError {
    let found = tokio::fs::metadata(&loc.real).await.ok();
    match (expect, &found) {
        (Expect::File, Some(m)) if m.is_dir() => return VfsError::IsDir(loc.shown.clone()),
        (Expect::Dir, Some(m)) if !m.is_dir() => return VfsError::NotDir(loc.shown.clone()),
        _ => {}
    }
    if found.is_none() && e.kind() != std::io::ErrorKind::PermissionDenied {
        // A parent that exists but is a file: the path can never resolve.
        for ancestor in loc.real.ancestors().skip(1) {
            if let Ok(m) = tokio::fs::metadata(ancestor).await {
                if !m.is_dir() {
                    return VfsError::NotDir(loc.shown.clone());
                }
                break;
            }
        }
    }
    VfsError::from_io(&loc.shown, e)
}

/// Converts OS metadata. `is_symlink` is what `symlink_metadata` said about
/// the path itself.
fn to_metadata(m: &std::fs::Metadata, is_symlink: bool) -> Metadata {
    let mut out = if m.is_dir() {
        Metadata::dir()
    } else {
        Metadata::file(m.len())
    };
    out.is_symlink = is_symlink;
    out.modified = m.modified().ok();
    out.created = m.created().ok();
    out.readonly = m.permissions().readonly();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        out.mode = Some(m.permissions().mode() & 0o7777);
    }
    out
}

fn entry_of(loc: Loc, name: String, m: &std::fs::Metadata) -> Entry {
    Entry {
        path: loc.shown,
        name,
        is_dir: m.is_dir(),
        len: if m.is_dir() { 0 } else { m.len() },
    }
}

// ---- atomic replacement ----------------------------------------------------

/// A temporary sibling that becomes `target` when committed.
struct Draft {
    tmp: PathBuf,
    target: PathBuf,
}

/// A token for temporary names: unpredictable (`RandomState` is seeded by the
/// OS) and distinct per call.
fn unique_token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    hasher.write_u32(std::process::id());
    format!("{:016x}", hasher.finish())
}

/// Follows a chain of symbolic links at the final component, so that the
/// rename replaces what the link points at rather than the link.
fn final_target(path: &Path) -> std::io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..40 {
        match std::fs::symlink_metadata(&current) {
            Ok(m) if m.file_type().is_symlink() => {
                let link = std::fs::read_link(&current)?;
                current = match current.parent() {
                    Some(parent) => parent.join(link),
                    None => link,
                };
            }
            _ => return Ok(current),
        }
    }
    Err(std::io::Error::other("too many levels of symbolic links"))
}

/// Creates the temporary file for an atomic write to `path`.
fn begin_draft(path: &Path) -> std::io::Result<(std::fs::File, Draft)> {
    let target = final_target(path)?;
    let existing = match std::fs::metadata(&target) {
        Ok(m) if m.is_dir() => {
            // `classify` reports this as `IsDir`.
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "destination is a directory",
            ));
        }
        // A rename would silently replace a read-only file; refuse instead,
        // as a plain in-place write would.
        Ok(m) if m.permissions().readonly() => {
            return Err(std::io::ErrorKind::PermissionDenied.into());
        }
        Ok(m) => Some(m.permissions()),
        Err(_) => None,
    };
    // Only Unix carries permission bits over (see below).
    #[cfg(not(unix))]
    let _ = &existing;
    let name = target.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination has no file name",
        )
    })?;
    let dir = match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let mut last = None;
    for _ in 0..16 {
        let tmp = dir.join(format!(
            ".{}.{}.tmp",
            name.to_string_lossy(),
            unique_token()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        // When replacing a file, start private and take on that file's
        // permission bits before any data is written; a new file gets the
        // ordinary mode of a created file.
        #[cfg(unix)]
        if existing.is_some() {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&tmp) {
            Ok(file) => {
                #[cfg(unix)]
                if let Some(permissions) = &existing {
                    if let Err(e) = std::fs::set_permissions(&tmp, permissions.clone()) {
                        let _ = std::fs::remove_file(&tmp);
                        return Err(e);
                    }
                }
                return Ok((file, Draft { tmp, target }));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("could not create a temporary file")))
}

/// Flushes the draft to disk and renames it over its target. On failure the
/// temporary file is removed.
fn commit_draft(file: std::fs::File, draft: &Draft) -> std::io::Result<()> {
    let result = (|| {
        file.sync_all()?;
        drop(file);
        std::fs::rename(&draft.tmp, &draft.target)
    })();
    match result {
        Ok(()) => {
            // Make the rename itself durable. Best-effort: the data is
            // already on disk, and not every filesystem supports this.
            #[cfg(unix)]
            if let Some(parent) = draft.target.parent() {
                if let Ok(dir) = std::fs::File::open(parent) {
                    let _ = dir.sync_all();
                }
            }
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&draft.tmp);
            Err(e)
        }
    }
}

/// The streaming writer behind [`LocalFs::create`]: bytes go to the temporary
/// sibling, and `finish` publishes it.
struct LocalSink {
    /// `None` once finished.
    file: Option<tokio::fs::File>,
    draft: Draft,
    loc: Loc,
}

impl WriteSink for LocalSink {
    fn write_all<'s>(&'s mut self, data: &'s [u8]) -> BoxFuture<'s, Result<(), VfsError>> {
        Box::pin(async move {
            let Some(file) = self.file.as_mut() else {
                return Err(VfsError::Unsupported("writer already finished".into()));
            };
            // tokio's `File` hands each write to the blocking pool and returns
            // early; flushing waits for it, so an error is reported by the
            // call that caused it and no write is still in flight if the
            // writer is dropped next.
            let written = async {
                file.write_all(data).await?;
                file.flush().await
            };
            written
                .await
                .map_err(|e| VfsError::from_io(&self.loc.shown, e))
        })
    }

    fn finish<'s>(mut self: Box<Self>) -> BoxFuture<'s, Result<(), VfsError>>
    where
        Self: 's,
    {
        Box::pin(async move {
            let Some(mut file) = self.file.take() else {
                return Err(VfsError::Unsupported("writer already finished".into()));
            };
            let draft = Draft {
                tmp: self.draft.tmp.clone(),
                target: self.draft.target.clone(),
            };
            let published = async {
                file.flush().await?;
                let file = file.into_std().await;
                blocking(move || commit_draft(file, &draft)).await
            }
            .await;
            match published {
                Ok(()) => Ok(()),
                Err(e) => {
                    // Usually already gone (`commit_draft` cleans up), but a
                    // failed flush leaves it behind.
                    let _ = std::fs::remove_file(&self.draft.tmp);
                    Err(classify(&self.loc, e, Expect::File).await)
                }
            }
        })
    }
}

impl Drop for LocalSink {
    fn drop(&mut self) {
        // Still holding the file means `finish` never ran: discard the draft.
        if self.file.take().is_some() {
            let _ = std::fs::remove_file(&self.draft.tmp);
        }
    }
}

impl Vfs for LocalFs {
    fn schemes(&self) -> &[&str] {
        &["file"]
    }

    fn read(&self, path: &str) -> BoxFuture<'_, Result<Vec<u8>, VfsError>> {
        let path = path.to_string();
        Box::pin(async move {
            let loc = self.locate(&path, true).await?;
            match tokio::fs::read(&loc.real).await {
                Ok(data) => Ok(data),
                Err(e) => Err(classify(&loc, e, Expect::File).await),
            }
        })
    }

    fn read_range(
        &self,
        path: &str,
        offset: u64,
        len: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, VfsError>> {
        let path = path.to_string();
        Box::pin(async move {
            let loc = self.locate(&path, true).await?;
            let read = async {
                let mut file = tokio::fs::File::open(&loc.real).await?;
                let meta = file.metadata().await?;
                if meta.is_dir() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "is a directory",
                    ));
                }
                // At or past the end of a regular file there is nothing to
                // read (and an offset beyond `i64::MAX` could not even be
                // seeked to).
                if meta.is_file() && offset >= meta.len() {
                    return Ok(Vec::new());
                }
                file.seek(std::io::SeekFrom::Start(offset)).await?;
                // Never size the buffer by the caller's `len`: reserve only
                // what the file can supply, and fallibly, so neither a huge
                // `len` nor a huge file can abort the process.
                let mut buf = Vec::new();
                if meta.is_file() {
                    let available = (meta.len() - offset).min(len);
                    let hint = usize::try_from(available)
                        .map_err(|_| std::io::Error::from(std::io::ErrorKind::OutOfMemory))?;
                    buf.try_reserve_exact(hint)
                        .map_err(|_| std::io::Error::from(std::io::ErrorKind::OutOfMemory))?;
                }
                file.take(len).read_to_end(&mut buf).await?;
                Ok(buf)
            };
            match read.await {
                Ok(data) => Ok(data),
                Err(e) => Err(classify(&loc, e, Expect::File).await),
            }
        })
    }

    fn write(&self, path: &str, data: Vec<u8>) -> BoxFuture<'_, Result<(), VfsError>> {
        let path = path.to_string();
        Box::pin(async move {
            let loc = self.locate(&path, true).await?;
            let real = loc.real.clone();
            let written = blocking(move || {
                let (mut file, draft) = begin_draft(&real)?;
                if let Err(e) = file.write_all(&data) {
                    drop(file);
                    let _ = std::fs::remove_file(&draft.tmp);
                    return Err(e);
                }
                commit_draft(file, &draft)
            })
            .await;
            match written {
                Ok(()) => Ok(()),
                Err(e) => Err(classify(&loc, e, Expect::File).await),
            }
        })
    }

    fn open(&self, path: &str) -> BoxFuture<'_, Result<Vfile, VfsError>> {
        let path = path.to_string();
        Box::pin(async move {
            let loc = self.locate(&path, true).await?;
            let opened = async {
                let file = tokio::fs::File::open(&loc.real).await?;
                let meta = file.metadata().await?;
                Ok::<_, std::io::Error>((file, meta))
            };
            match opened.await {
                // Unix lets a directory be opened and only fails on read;
                // report it up front on every platform.
                Ok((_, meta)) if meta.is_dir() => Err(VfsError::IsDir(loc.shown)),
                Ok((file, meta)) => Ok(Vfile::new(file, to_metadata(&meta, false))),
                Err(e) => Err(classify(&loc, e, Expect::File).await),
            }
        })
    }

    fn delete(&self, path: &str) -> BoxFuture<'_, Result<(), VfsError>> {
        let path = path.to_string();
        Box::pin(async move {
            let loc = self.locate(&path, false).await?;
            if self.is_root(&loc) {
                return Err(VfsError::InvalidPath {
                    path,
                    reason: "the root itself cannot be deleted".to_string(),
                });
            }
            let removed = async {
                // Not following the link: a symlink to a directory is removed
                // as the link it is, never by emptying its target.
                let meta = tokio::fs::symlink_metadata(&loc.real).await?;
                if meta.is_dir() {
                    tokio::fs::remove_dir_all(&loc.real).await
                } else {
                    tokio::fs::remove_file(&loc.real).await
                }
            };
            match removed.await {
                Ok(()) => Ok(()),
                Err(e) => Err(classify(&loc, e, Expect::Any).await),
            }
        })
    }

    fn exists(&self, path: &str) -> BoxFuture<'_, bool> {
        let path = path.to_string();
        Box::pin(async move {
            match self.locate(&path, true).await {
                Ok(loc) => tokio::fs::metadata(&loc.real).await.is_ok(),
                Err(_) => false,
            }
        })
    }

    fn metadata(&self, path: &str) -> BoxFuture<'_, Result<Metadata, VfsError>> {
        let path = path.to_string();
        Box::pin(async move {
            // Located twice: once as the entry itself, so a link can be
            // reported as one, and once followed — which, for a confined
            // backend, verifies the link's target before anything reads it.
            let entry = self.locate(&path, false).await?;
            let target = self.locate(&path, true).await?;
            let (entry_path, target_path) = (entry.real, target.real.clone());
            let found = blocking(move || {
                let is_symlink = std::fs::symlink_metadata(&entry_path)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false);
                Ok(to_metadata(&std::fs::metadata(&target_path)?, is_symlink))
            })
            .await;
            match found {
                Ok(meta) => Ok(meta),
                Err(e) => Err(classify(&target, e, Expect::Any).await),
            }
        })
    }

    fn list(&self, path: &str) -> BoxFuture<'_, Result<Vec<Entry>, VfsError>> {
        let path = path.to_string();
        Box::pin(async move {
            let dir = self.locate(&path, true).await?;
            let listed = async {
                let mut reader = tokio::fs::read_dir(&dir.real).await?;
                let mut out = Vec::new();
                while let Some(de) = reader.next_entry().await? {
                    let name = de.file_name().to_string_lossy().into_owned();
                    let meta = de.metadata().await?;
                    out.push(entry_of(self.child(&dir, &name), name, &meta));
                }
                Ok::<_, std::io::Error>(out)
            };
            match listed.await {
                Ok(entries) => Ok(entries),
                Err(e) => Err(classify(&dir, e, Expect::Dir).await),
            }
        })
    }

    fn list_page(
        &self,
        path: &str,
        after: Option<String>,
        limit: usize,
    ) -> BoxFuture<'_, Result<ListPage, VfsError>> {
        let path = path.to_string();
        Box::pin(async move {
            let dir = self.locate(&path, true).await?;
            let real = dir.real.clone();
            let limit = limit.max(1);
            // Names are needed in full to give a stable order, but only the
            // entries on the requested page are stat'ed.
            let paged = blocking(move || {
                let mut names = Vec::new();
                for de in std::fs::read_dir(&real)? {
                    let de = de?;
                    let name = de.file_name().to_string_lossy().into_owned();
                    if after.as_deref().map_or(true, |a| name.as_str() > a) {
                        names.push((name, de));
                    }
                }
                names.sort_by(|a, b| a.0.cmp(&b.0));
                let more = names.len() > limit;
                names.truncate(limit);
                let mut page = Vec::with_capacity(names.len());
                for (name, de) in names {
                    let meta = de.metadata()?;
                    page.push((name, meta));
                }
                Ok((page, more))
            })
            .await;
            match paged {
                Ok((page, more)) => {
                    let entries: Vec<Entry> = page
                        .into_iter()
                        .map(|(name, meta)| entry_of(self.child(&dir, &name), name, &meta))
                        .collect();
                    let next = match entries.last() {
                        Some(last) if more => Some(last.name.clone()),
                        _ => None,
                    };
                    Ok(ListPage { entries, next })
                }
                Err(e) => Err(classify(&dir, e, Expect::Dir).await),
            }
        })
    }

    fn mkdir_all(&self, path: &str) -> BoxFuture<'_, Result<(), VfsError>> {
        let path = path.to_string();
        Box::pin(async move {
            let loc = self.locate(&path, true).await?;
            match tokio::fs::create_dir_all(&loc.real).await {
                Ok(()) => Ok(()),
                Err(e) => Err(classify(&loc, e, Expect::Dir).await),
            }
        })
    }

    fn copy(&self, src: &str, dst: &str) -> BoxFuture<'_, Result<(), VfsError>> {
        let src = src.to_string();
        let dst = dst.to_string();
        Box::pin(async move {
            let from = self.locate(&src, true).await?;
            let to = self.locate(&dst, true).await?;
            match tokio::fs::copy(&from.real, &to.real).await {
                Ok(_) => Ok(()),
                // Blame the side that is actually wrong.
                Err(e) => match tokio::fs::metadata(&from.real).await {
                    Ok(m) if m.is_dir() => Err(VfsError::IsDir(from.shown)),
                    Ok(_) => Err(classify(&to, e, Expect::File).await),
                    Err(_) => Err(classify(&from, e, Expect::File).await),
                },
            }
        })
    }

    fn rename(&self, src: &str, dst: &str) -> BoxFuture<'_, Result<(), VfsError>> {
        let src = src.to_string();
        let dst = dst.to_string();
        Box::pin(async move {
            let from = self.locate(&src, false).await?;
            let to = self.locate(&dst, false).await?;
            if self.is_root(&from) || self.is_root(&to) {
                return Err(VfsError::InvalidPath {
                    path: if self.is_root(&from) { src } else { dst },
                    reason: "the root itself cannot be renamed or replaced".to_string(),
                });
            }
            match tokio::fs::rename(&from.real, &to.real).await {
                Ok(()) => Ok(()),
                Err(e) => {
                    if tokio::fs::symlink_metadata(&from.real).await.is_ok() {
                        Err(classify(&to, e, Expect::Any).await)
                    } else {
                        Err(classify(&from, e, Expect::Any).await)
                    }
                }
            }
        })
    }

    fn walk(&self, path: &str, mut f: WalkFn) -> BoxFuture<'_, Result<(), VfsError>> {
        let path = path.to_string();
        Box::pin(async move {
            let root = self.locate(&path, true).await?;
            let mut stack = vec![root];
            let mut first = true;
            while let Some(dir) = stack.pop() {
                let mut reader = match tokio::fs::read_dir(&dir.real).await {
                    Ok(reader) => reader,
                    Err(e) => {
                        let expect = if first { Expect::Dir } else { Expect::Any };
                        return Err(classify(&dir, e, expect).await);
                    }
                };
                first = false;
                loop {
                    let de = match reader.next_entry().await {
                        Ok(Some(de)) => de,
                        Ok(None) => break,
                        Err(e) => return Err(VfsError::from_io(&dir.shown, e)),
                    };
                    let name = de.file_name().to_string_lossy().into_owned();
                    let child = self.child(&dir, &name);
                    // `DirEntry::metadata` does not follow symbolic links, so
                    // a linked directory is reported as a non-directory and
                    // never descended into: no loops, and no way out of a
                    // root.
                    let meta = match de.metadata().await {
                        Ok(meta) => meta,
                        Err(e) => return Err(VfsError::from_io(&child.shown, e)),
                    };
                    let entry = entry_of(child.clone(), name, &meta);
                    match f(&entry) {
                        WalkAction::Stop => return Ok(()),
                        WalkAction::SkipDir => {}
                        WalkAction::Continue => {
                            if entry.is_dir {
                                stack.push(child);
                            }
                        }
                    }
                }
            }
            Ok(())
        })
    }

    fn append(&self, path: &str, data: Vec<u8>) -> BoxFuture<'_, Result<(), VfsError>> {
        let path = path.to_string();
        Box::pin(async move {
            let loc = self.locate(&path, true).await?;
            let appended = async {
                let mut file = tokio::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&loc.real)
                    .await?;
                file.write_all(&data).await?;
                // tokio's `File` buffers writes; make sure they are handed to
                // the OS before reporting success.
                file.flush().await
            };
            match appended.await {
                Ok(()) => Ok(()),
                Err(e) => Err(classify(&loc, e, Expect::File).await),
            }
        })
    }

    fn create(&self, path: &str) -> BoxFuture<'_, Result<VfsWriter<'_>, VfsError>> {
        let path = path.to_string();
        Box::pin(async move {
            let loc = self.locate(&path, true).await?;
            let real = loc.real.clone();
            match blocking(move || begin_draft(&real)).await {
                Ok((file, draft)) => Ok(VfsWriter::new(LocalSink {
                    file: Some(tokio::fs::File::from_std(file)),
                    draft,
                    loc,
                })),
                Err(e) => Err(classify(&loc, e, Expect::File).await),
            }
        })
    }
}

#[cfg(test)]
#[path = "local_tests.rs"]
mod tests;

# Virtual filesystem (VFS)

**Feature:** `vfs` · **Module:** `ferroly::vfs`

A virtual filesystem abstraction — a [`Vfs`](#vfs) trait, a local backend with
optional root confinement ([`LocalFs`](#localfs)), an in-memory backend
([`MemoryFs`](#memoryfs)), a scheme dispatcher ([`VfsRegistry`](#vfsregistry)),
and helpers that work across backends.

## Overview

Key pieces:

- [`Vfs`](#vfs) — the trait: `read` / `read_range` / `write` / `append` / `create` / `open` / `delete` / `exists` / `metadata` / `list` / `list_page` / `mkdir_all` / `copy` / `rename` / `walk`, all async, all taking `&str` paths.
- [`LocalFs`](#localfs) — the local disk. **`LocalFs::rooted(dir)` confines every path to one directory; `LocalFs::new()` is unconfined.**
- [`MemoryFs`](#memoryfs) — an in-memory tree with the same behavior; a test double.
- [`VfsRegistry`](#vfsregistry) — routes a URL to a backend by its scheme.
- [`copy_between`](#cross-backend-copy), [`glob` / `glob_match`](#glob), [`checksum_sha256`](#checksum) — helpers written against the trait.
- [`Vfile`](#vfile) — an open file: a streaming async reader (`AsyncRead`) with attached [`Metadata`](#metadata).
- [`VfsWriter`](#vfswriter) — a streaming writer, published atomically on `finish`.
- [`Metadata`](#metadata) / [`Entry`](#entry) / [`ListPage`](#listpage) — node info, directory entries, listing pages.
- [`WalkAction`](#walkaction) / [`WalkFn`](#walkfn) — walk control and callback.
- [`VfsError`](#error-handling) — the error enum.

Paths are plain `&str`. A backend accepts a bare path or a URL with **its own**
scheme (`file://` for `LocalFs`, `mem://` for `MemoryFs`) and rejects any other
scheme with `VfsError::InvalidPath` — `s3://bucket/key` handed to `LocalFs` is
an error, never a local path.

### Design notes

| Concern | ferroly's design |
| --- | --- |
| Path arguments | **one** method per operation, each taking a `&str` path — no string-twin overloads |
| Error reporting | a typed [`VfsError`](#error-handling) enum (`io::Error` kinds map onto it), not sentinel error values |
| Walk control flow | a [`WalkAction`](#walkaction) return value, keeping traversal control out of the error channel |
| Cancellation | drop the future — no separate context-carrying method variants |
| Extension | implement [`Vfs`](#vfs); new operations are added as provided methods so implementations keep compiling |

Object-store backends (S3, GCS, Azure) are **not bundled**. The trait is the
extension point: a type implementing [`Vfs`](#vfs) plugs into the registry,
the cross-backend copy, glob and checksum helpers — see
[Writing a backend](#writing-a-backend).

## Enabling

The `vfs` feature pulls in tokio. `checksum_sha256` additionally needs `hash`.

```toml
[dependencies]
ferroly = { version = "*", features = ["vfs"] }
```

## Quick start

```rust
use ferroly::vfs::{LocalFs, Vfs};

#[tokio::main]
async fn main() -> Result<(), ferroly::vfs::VfsError> {
    // Confined to one directory: `..`, absolute paths and escaping symlinks
    // are rejected.
    let fs = LocalFs::rooted("/var/lib/my-app")?;

    fs.mkdir_all("reports").await?;
    fs.write("reports/hello.txt", b"hi".to_vec()).await?;   // atomic
    assert_eq!(fs.read("reports/hello.txt").await?, b"hi");
    assert!(fs.read("../../etc/passwd").await.is_err());
    Ok(())
}
```

## API reference

| Item | Kind | Summary |
| --- | --- | --- |
| [`Vfs`](#vfs) | trait | The filesystem: one async method per operation |
| [`LocalFs`](#localfs) | struct | Local-disk backend over `tokio::fs`, optionally rooted |
| [`MemoryFs`](#memoryfs) | struct | In-memory backend |
| [`VfsRegistry`](#vfsregistry) | struct | Scheme → backend dispatcher |
| [`Vfile`](#vfile) | struct | An open file: `AsyncRead` + read-all helpers |
| [`VfsWriter`](#vfswriter) | struct | A streaming writer, atomic on `finish` |
| [`WriteSink`](#vfswriter) | trait | The backend half of a `VfsWriter` |
| [`Metadata`](#metadata) | struct | Kind, size, times, permissions |
| [`Entry`](#entry) | struct | `path`, `name`, `is_dir`, `len` |
| [`ListPage`](#listpage) | struct | One page of a listing |
| [`WalkAction`](#walkaction) | enum | `Continue` / `SkipDir` / `Stop` |
| [`WalkFn`](#walkfn) | type alias | `Box<dyn FnMut(&Entry) -> WalkAction + Send>` |
| [`VfsError`](#error-handling) | enum | `NotFound` / `Permission` / `NotDir` / `IsDir` / `Unsupported` / `InvalidPath` / `Io` |
| [`copy_between`](#cross-backend-copy) | fn | Streamed copy from one backend to another |
| [`glob`](#glob), [`glob_match`](#glob) | fn | `*` / `**` / `?` matching over a tree |
| [`checksum_sha256`](#checksum) | fn | Streamed SHA-256 of a file (feature `hash`) |
| `scheme_of` | fn | The URL scheme of an address, if any |
| [`BoxFuture`](#boxfuture) | type alias | `Pin<Box<dyn Future<Output = T> + Send + 'a>>` |

### BoxFuture

```rust
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
```

The object-safe async return type of every [`Vfs`](#vfs) method. You never construct it — just `.await` the result of a call.

## Vfs

```rust
pub trait Vfs: Send + Sync {
    // required
    fn schemes(&self) -> &[&str];
    fn read(&self, path: &str) -> BoxFuture<'_, Result<Vec<u8>, VfsError>>;
    fn read_range(&self, path: &str, offset: u64, len: u64) -> BoxFuture<'_, Result<Vec<u8>, VfsError>>;
    fn write(&self, path: &str, data: Vec<u8>) -> BoxFuture<'_, Result<(), VfsError>>;
    fn open(&self, path: &str) -> BoxFuture<'_, Result<Vfile, VfsError>>;
    fn delete(&self, path: &str) -> BoxFuture<'_, Result<(), VfsError>>;
    fn exists(&self, path: &str) -> BoxFuture<'_, bool>;
    fn metadata(&self, path: &str) -> BoxFuture<'_, Result<Metadata, VfsError>>;
    fn list(&self, path: &str) -> BoxFuture<'_, Result<Vec<Entry>, VfsError>>;
    fn mkdir_all(&self, path: &str) -> BoxFuture<'_, Result<(), VfsError>>;
    fn copy(&self, src: &str, dst: &str) -> BoxFuture<'_, Result<(), VfsError>>;
    fn rename(&self, src: &str, dst: &str) -> BoxFuture<'_, Result<(), VfsError>>;
    fn walk(&self, path: &str, f: WalkFn) -> BoxFuture<'_, Result<(), VfsError>>;

    // provided (overridable)
    fn append(&self, path: &str, data: Vec<u8>) -> BoxFuture<'_, Result<(), VfsError>>;
    fn create(&self, path: &str) -> BoxFuture<'_, Result<VfsWriter<'_>, VfsError>>;
    fn list_page(&self, path: &str, after: Option<String>, limit: usize) -> BoxFuture<'_, Result<ListPage, VfsError>>;
}
```

| Method | Behaviour |
| --- | --- |
| `schemes()` | The URL schemes this backend handles — `["file"]` for [`LocalFs`](#localfs), `["mem"]` for [`MemoryFs`](#memoryfs). Used by [`VfsRegistry`](#vfsregistry). |
| `read(path)` | Read a whole file into a `Vec<u8>`. |
| `read_range(path, offset, len)` | Read up to `len` bytes starting at `offset`. Short if the file ends first; **empty if `offset` is at or beyond the end** — neither is an error. The buffer is sized by what is read, never by `len`, so `u64::MAX` is a valid "to the end". |
| `write(path, data)` | Write a whole file, creating or replacing it. The bundled backends replace **atomically**. The parent directory must exist. |
| `append(path, data)` | Append to a file, creating it if missing. Default: read + write (whole file, not safe against a concurrent appender); `LocalFs` appends natively. |
| `create(path)` | Start a streaming write; returns a [`VfsWriter`](#vfswriter). Default: buffers in memory and calls `write` on finish; `LocalFs` streams to disk. |
| `open(path)` | Open a file as a streaming [`Vfile`](#vfile) reader. |
| `delete(path)` | Delete a file, or a directory and all its contents. A symbolic link is removed as a link. |
| `exists(path)` | `bool` — whether the path exists. Never errors: a path that cannot be examined, or is not acceptable to the backend, reports `false`. |
| `metadata(path)` | [`Metadata`](#metadata) for the path (following a symbolic link, and saying so). |
| `list(path)` | One directory level as a `Vec<`[`Entry`](#entry)`>`. |
| `list_page(path, after, limit)` | One directory level, a [page](#listpage) at a time, ordered by name. |
| `mkdir_all(path)` | Create a directory and all missing parents. |
| `copy(src, dst)` | Copy a file within this backend. |
| `rename(src, dst)` | Rename / move a path. |
| `walk(path, f)` | Recursively visit every entry, honoring [`WalkAction`](#walkaction). Entries are streamed to the callback. |

## Metadata

```rust
#[non_exhaustive]
pub struct Metadata {
    pub is_dir: bool,
    pub len: u64,                       // 0 for directories
    pub is_symlink: bool,               // the path itself is a symbolic link
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,    // where the platform records it
    pub readonly: bool,
    pub mode: Option<u32>,              // Unix permission bits (0o7777 mask)
}

impl Metadata {
    pub fn file(len: u64) -> Self;
    pub fn dir() -> Self;
    pub fn is_file(&self) -> bool;
    pub fn with_modified(self, at: SystemTime) -> Self;
    pub fn with_created(self, at: SystemTime) -> Self;
    pub fn with_readonly(self, readonly: bool) -> Self;
    pub fn with_mode(self, mode: u32) -> Self;
    pub fn with_symlink(self, is_symlink: bool) -> Self;
}
```

Returned by [`Vfs::metadata`](#vfs) and available on an open [`Vfile`](#vfile).
`Clone`, `PartialEq`, `Eq`, `Default`. Fields a backend cannot supply are
`None` / `false` (`MemoryFs` has no `mode`; `created` is `None` on filesystems
that do not record it). When `is_symlink` is true the other fields describe
the link's target. The struct is `#[non_exhaustive]`: read the fields freely,
but build one with `Metadata::file` / `Metadata::dir` and the `with_*` methods.

```rust
use ferroly::vfs::{LocalFs, Vfs};

# async fn ex() -> Result<(), ferroly::vfs::VfsError> {
let fs = LocalFs::rooted("/var/lib/my-app")?;
let m = fs.metadata("reports/hello.txt").await?;
println!("dir={} size={} modified={:?} mode={:?}", m.is_dir, m.len, m.modified, m.mode);
# Ok(()) }
```

## Entry

```rust
pub struct Entry {
    pub path: String,    // in a form the same backend accepts back
    pub name: String,    // final path component
    pub is_dir: bool,
    pub len: u64,        // 0 for directories
}
```

A directory entry produced by [`Vfs::list`](#vfs) / `list_page` and passed to
the [`walk`](#walkaction) callback. `Clone`, `PartialEq`, `Eq`. `path` can be
fed straight back into the backend that produced it: an unconfined `LocalFs`
gives the listed directory joined with the name, a rooted `LocalFs` gives a
root-relative `/`-separated path, `MemoryFs` gives an absolute `/a/b`.

A symbolic link is reported as a non-directory entry (it is never descended
into), with the link's own size.

## ListPage

```rust
pub struct ListPage {
    pub entries: Vec<Entry>,     // ordered by name
    pub next: Option<String>,    // pass as `after` for the next page; None at the end
}
```

```rust
use ferroly::vfs::{MemoryFs, Vfs};

# async fn ex() -> Result<(), ferroly::vfs::VfsError> {
let fs = MemoryFs::new();
let mut after = None;
loop {
    let page = fs.list_page("/inbox", after, 500).await?;
    for entry in &page.entries { /* … */ }
    match page.next {
        Some(next) => after = Some(next),
        None => break,
    }
}
# Ok(()) }
```

Paging is by name, not a snapshot: entries created or removed between calls are
picked up or skipped according to where they sort. A `limit` of 0 is treated
as 1. `LocalFs` reads the directory's names to order them but only stats the
entries on the requested page. To process a huge directory without building
any list at all, use [`walk`](#walkaction) and return `SkipDir` for every
directory — entries are streamed to the callback one at a time.

## Vfile

```rust
pub struct Vfile { /* … */ }   // impls AsyncRead

impl Vfile {
    pub fn new(reader: impl AsyncRead + Send + Unpin + 'static, meta: Metadata) -> Self;
    pub fn from_bytes(data: Vec<u8>, meta: Metadata) -> Self;
    pub fn metadata(&self) -> &Metadata;
    pub async fn read_all_bytes(self) -> Result<Vec<u8>, std::io::Error>;
    pub async fn read_all_string(self) -> Result<String, std::io::Error>;
}
```

An open file returned by [`Vfs::open`](#vfs). It is a **streaming** reader: it implements tokio's `AsyncRead`, so you can read it incrementally with `AsyncReadExt` (`read`, `read_buf`, …) or copy it with `tokio::io::copy` — ideal for large files you do not want fully in memory.

- `metadata()` — the [`Metadata`](#metadata) captured when the file was opened.
- `read_all_bytes(self)` — consume the file, returning all bytes.
- `read_all_string(self)` — consume the file, returning a UTF-8 `String` (errors with `InvalidData` on non-UTF-8).
- `new` / `from_bytes` — how a backend builds the value it returns from `open`.

The read-all helpers take `self` by value (they consume the file). The errors here are raw `std::io::Error`, not [`VfsError`](#error-handling). Opening a directory is `VfsError::IsDir`.

```rust
use ferroly::vfs::{LocalFs, Vfs};
use tokio::io::AsyncReadExt;

# async fn ex() -> Result<(), Box<dyn std::error::Error>> {
let fs = LocalFs::rooted("/var/lib/my-app")?;
let mut vf = fs.open("reports/hello.txt").await?;
println!("size = {}", vf.metadata().len);

// Streaming: read the first 4 bytes.
let mut head = [0u8; 4];
let n = vf.read(&mut head).await?;
println!("first {n} bytes: {:?}", &head[..n]);
# Ok(()) }
```

## VfsWriter

```rust
pub struct VfsWriter<'a> { /* … */ }

impl<'a> VfsWriter<'a> {
    pub fn new(sink: impl WriteSink + 'a) -> Self;
    pub async fn write_all(&mut self, data: &[u8]) -> Result<(), VfsError>;
    pub fn bytes_written(&self) -> u64;
    pub async fn finish(self) -> Result<u64, VfsError>;    // total bytes written
}

pub trait WriteSink: Send {
    fn write_all<'s>(&'s mut self, data: &'s [u8]) -> BoxFuture<'s, Result<(), VfsError>>;
    fn finish<'s>(self: Box<Self>) -> BoxFuture<'s, Result<(), VfsError>> where Self: 's;
}
```

Returned by [`Vfs::create`](#vfs). Write the contents in as many pieces as you
like, then call `finish`: **only then does the file appear (or change) at its
path, in one step.** Dropping the writer without finishing discards everything
and leaves any existing file untouched.

```rust
use ferroly::vfs::{LocalFs, Vfs};

# async fn ex() -> Result<(), ferroly::vfs::VfsError> {
let fs = LocalFs::rooted("/var/lib/my-app")?;
let mut out = fs.create("exports/big.csv").await?;
for row in 0..1_000_000 {
    out.write_all(format!("{row},ok\n").as_bytes()).await?;
}
let bytes = out.finish().await?;     // published atomically here
# let _ = bytes;
# Ok(()) }
```

`LocalFs` streams to a temporary sibling file and renames it on finish, so
memory use is constant. The trait's default (used by `MemoryFs` and by any
backend that does not override `create`) buffers the whole file in memory and
hands it to `write` on finish — atomic, but not bounded. `WriteSink` is what a
backend implements to provide its own streaming writer.

## WalkAction

```rust
pub enum WalkAction {
    Continue,   // keep going (descend into directories)
    SkipDir,    // skip descending into the just-visited directory
    Stop,       // stop the whole walk
}
```

Rather than overloading the error channel with traversal control, a
[`WalkFn`](#walkfn) returns a `WalkAction` and [`walk`](#vfs) acts on it:

- `Continue` — proceed; if the visited entry is a directory, descend into it.
- `SkipDir` — do not descend into the just-visited directory (but keep walking siblings).
- `Stop` — end the entire walk immediately, returning `Ok(())`.

Real errors stay in the `Result` where they belong. Symbolic links are reported
but never followed, so a walk cannot loop or leave a rooted filesystem.

## WalkFn

```rust
pub type WalkFn = Box<dyn FnMut(&Entry) -> WalkAction + Send>;
```

The callback [`walk`](#vfs) invokes once per [`Entry`](#entry). It is `FnMut`, so it can accumulate state, and `Send` so the walk can run across `.await` points. Wrap your closure in `Box::new`.

## LocalFs

```rust
pub struct LocalFs { /* … */ }   // Debug, Default, Clone

impl LocalFs {
    pub fn new() -> Self;                                            // unconfined
    pub fn rooted(dir: impl AsRef<Path>) -> Result<Self, VfsError>;  // confined to `dir`
    pub fn root(&self) -> Option<&Path>;
}
```

The local-disk [`Vfs`](#vfs) implementation, backed by tokio's non-blocking
`fs` (compound operations run on the blocking pool). `schemes()` returns
`["file"]`. It accepts a plain path or a `file://` URL
(`file:///abs/path`, `file://localhost/abs/path`; URLs are not
percent-decoded) and rejects every other scheme with `InvalidPath`.

### Rooted: confined to one directory

`LocalFs::rooted(dir)` requires `dir` to be an existing directory
(`NotFound` / `NotDir` otherwise) and then treats every path as relative to it:

- `"a/b.txt"`, `"./a/b.txt"` and `"file://a/b.txt"` name `<dir>/a/b.txt`; `""`
  and `"."` name the root itself.
- **`..` is rejected** — any `..` component, even one that would stay inside.
- **Absolute paths are rejected** (`/etc/passwd`, `file:///etc/passwd`, and on
  Windows a drive or UNC prefix).
- **Symbolic links are followed only if they resolve inside the root.** A link
  pointing outside is rejected, and so is a dangling link (it could otherwise
  be written through to create a file outside).
- The root itself cannot be deleted, renamed, or replaced.
- Entries from `list` / `list_page` / `walk` carry root-relative paths with `/`
  separators, ready to pass back in; errors show the relative path too.
- `delete` and `rename` act on a link itself rather than its target, so an
  escaping link can still be removed.

All rejections are `VfsError::InvalidPath { path, reason }`; `exists` reports
`false` for them.

```rust
use ferroly::vfs::{LocalFs, Vfs, VfsError};

# async fn ex() -> Result<(), VfsError> {
let fs = LocalFs::rooted("/srv/uploads")?;
assert!(matches!(fs.read("../secret").await, Err(VfsError::InvalidPath { .. })));
assert!(matches!(fs.read("/etc/passwd").await, Err(VfsError::InvalidPath { .. })));
assert!(matches!(fs.read("s3://bucket/key").await, Err(VfsError::InvalidPath { .. })));
# Ok(()) }
```

**How it is enforced, and its limits.** Each path is checked lexically, then
the deepest ancestor that exists is canonicalized (resolving every symbolic
link on the way) and must still lie under the canonical root; the operation
then runs on that resolved path. What this does **not** cover:

- **Time-of-check/time-of-use.** Between the check and the operation, another
  process with write access *inside* the root could swap a directory for a
  symlink. Closing that window needs `openat2(RESOLVE_BENEATH)`-style system
  calls, which this crate — no `libc`, no `unsafe` here — does not make.
- **Hard links** to files outside the root, created by such a process, are not
  detected.

So confinement holds against hostile *paths* (the common case: a user-supplied
file name). It is not a sandbox against a hostile *process* that can already
modify the tree; for that, add OS-level isolation.

### Unrooted

`LocalFs::new()` passes paths to the operating system as given: a
caller-controlled path can reach anything the process can. Use it for paths
your own code constructs; use `rooted` for anything a user or remote party can
influence.

### Atomic writes

`write` and a finished `create` write a temporary sibling
(`.<name>.<unique>.tmp`), flush it to disk, and rename it over the destination.
A reader — or a crash — sees the old file or the new one, never part of each.
Consequences:

- The destination's **directory** must be writable.
- The file gets a new inode: hard links keep the old contents. On Unix the
  previous permission bits are carried over (ownership is not); a new file gets
  the usual mode of a created file.
- A destination marked **read-only is refused** (`Permission`) rather than
  silently replaced.
- A symbolic link at the destination is followed, so the link's *target* is
  replaced and the link kept.
- A temporary file is briefly visible in listings while a write is in flight,
  and can be left behind if the process is killed mid-write.
- Every write costs an `fsync`. `append` writes in place and is **not** atomic.

### Other implementation notes

- `read_range` never allocates by the caller's `len`: it reserves at most what
  the file can supply and reads with a limit. An offset at or past the end
  returns an empty buffer.
- `delete` removes a directory recursively, a file or symbolic link singly.
- `exists` never errors — a failed stat is reported as `false`.
- `walk` is iterative (an explicit stack), so deep trees will not overflow the
  stack; traversal order is unspecified.
- Directory/file mismatches are reported as `IsDir` / `NotDir` on every
  platform — see [Error handling](#error-handling).

### Examples

```rust
use ferroly::vfs::{Entry, LocalFs, Vfs, WalkAction};
use std::sync::{Arc, Mutex};

# async fn ex() -> Result<(), ferroly::vfs::VfsError> {
let fs = LocalFs::rooted("/var/lib/my-app")?;
fs.mkdir_all("demo").await?;
fs.write("demo/a.txt", b"hello world".to_vec()).await?;
fs.append("demo/log.txt", b"started\n".to_vec()).await?;

// Range read: bytes [6, 11).
assert_eq!(fs.read_range("demo/a.txt", 6, 5).await?, b"world");
// To the end, whatever the size.
assert_eq!(fs.read_range("demo/a.txt", 6, u64::MAX).await?, b"world");

// List one level.
for Entry { name, is_dir, len, .. } in fs.list("demo").await? {
    println!("{name}\t{}\t{len}", if is_dir { "dir" } else { "file" });
}

// Walk, skipping directory contents.
let seen = Arc::new(Mutex::new(Vec::<String>::new()));
let sink = seen.clone();
fs.walk("demo", Box::new(move |e: &Entry| {
    sink.lock().unwrap().push(e.path.clone());
    if e.is_dir { WalkAction::SkipDir } else { WalkAction::Continue }
})).await?;

fs.rename("demo/a.txt", "demo/a2.txt").await?;
fs.copy("demo/a2.txt", "demo/a3.txt").await?;
fs.delete("demo").await?;             // recursive for directories
assert!(!fs.exists("demo").await);
# Ok(()) }
```

## MemoryFs

```rust
pub struct MemoryFs { /* … */ }   // Debug, Default, Clone

impl MemoryFs {
    pub fn new() -> Self;
}
```

A [`Vfs`](#vfs) that keeps everything in memory — a stand-in for `LocalFs` in
tests, and a scratch filesystem that leaves nothing behind. `schemes()` returns
`["mem"]`.

- It behaves like the local backend where it matters for code written against
  the trait: a file's parent directory must exist, reading a directory is
  `IsDir`, listing a file is `NotDir`, a file used as a parent is `NotDir`,
  writes replace a file in one step.
- Paths are `/`-separated and always resolved from the root: `"a/b"`, `"/a/b"`
  and `"mem://a/b"` are the same file. `.` is ignored; `..` and foreign schemes
  are `InvalidPath`. Entries report absolute paths (`/a/b`) in name order.
- `Metadata` carries `created` / `modified` times; there are no permissions or
  links.
- Cloning yields another handle to the **same** tree. Nothing is persisted.
- Operations complete immediately (no I/O) under a mutex; the `walk` callback
  runs on a snapshot, so it may call back into the filesystem.

```rust
use ferroly::vfs::{MemoryFs, Vfs};

# async fn ex() -> Result<(), ferroly::vfs::VfsError> {
let fs = MemoryFs::new();
fs.mkdir_all("/reports/2024").await?;
fs.write("/reports/2024/q1.csv", b"a,b\n".to_vec()).await?;
assert_eq!(fs.read("mem://reports/2024/q1.csv").await?, b"a,b\n");
# Ok(()) }
```

A function that takes `&dyn Vfs` (or `Arc<dyn Vfs>`) can be exercised against
`MemoryFs` in tests and run against `LocalFs::rooted(..)` in production.

## VfsRegistry

```rust
pub struct VfsRegistry { /* … */ }   // Clone, Debug, Default

impl VfsRegistry {
    pub fn new() -> Self;
    pub fn with_local(local: LocalFs) -> Self;
    pub fn register(&mut self, backend: Arc<dyn Vfs>) -> &mut Self;
    pub fn register_scheme(&mut self, scheme: &str, backend: Arc<dyn Vfs>) -> &mut Self;
    pub fn set_default(&mut self, scheme: &str) -> &mut Self;
    pub fn get(&self, scheme: &str) -> Option<&Arc<dyn Vfs>>;
    pub fn schemes(&self) -> Vec<&str>;
    pub fn resolve(&self, url: &str) -> Result<&Arc<dyn Vfs>, VfsError>;

    pub async fn read(&self, url: &str) -> Result<Vec<u8>, VfsError>;
    pub async fn write(&self, url: &str, data: Vec<u8>) -> Result<(), VfsError>;
    pub async fn open(&self, url: &str) -> Result<Vfile, VfsError>;
    pub async fn exists(&self, url: &str) -> bool;
    pub async fn metadata(&self, url: &str) -> Result<Metadata, VfsError>;
    pub async fn list(&self, url: &str) -> Result<Vec<Entry>, VfsError>;
    pub async fn mkdir_all(&self, url: &str) -> Result<(), VfsError>;
    pub async fn delete(&self, url: &str) -> Result<(), VfsError>;
    pub async fn copy(&self, src: &str, dst: &str) -> Result<(), VfsError>;
    pub async fn rename(&self, src: &str, dst: &str) -> Result<(), VfsError>;
}
```

Routes a URL to the backend registered for its scheme (matched ignoring ASCII
case).

- `register(backend)` records the backend under every scheme its
  [`schemes()`](#vfs) reports.
- `register_scheme("name", backend)` mounts a backend under a scheme of your
  choosing — for two instances of one backend side by side. Because the
  backend does not know that alias, the registry strips it before forwarding:
  `scratch://a/b` reaches the backend as `a/b`.
- Paths **without** a scheme go to the default backend chosen with
  `set_default`; with no default they are refused (`Unsupported`), not guessed
  at. `with_local(local)` registers `local` for `file://` and makes it the
  default — pass a `LocalFs::rooted(..)` to keep scheme-less paths confined.
- An unregistered scheme is `VfsError::Unsupported`.
- `copy` and `rename` work **across** backends by streaming
  ([`copy_between`](#cross-backend-copy)). A cross-backend `rename` is a copy
  followed by a delete of the source: not atomic, and single files only.

```rust
use std::sync::Arc;
use ferroly::vfs::{LocalFs, MemoryFs, VfsRegistry};

# async fn ex() -> Result<(), ferroly::vfs::VfsError> {
let mut registry = VfsRegistry::with_local(LocalFs::rooted("/var/lib/my-app")?);
registry.register(Arc::new(MemoryFs::new()));          // mem://
registry.register_scheme("scratch", Arc::new(MemoryFs::new()));

registry.write("mem://upload.bin", vec![1, 2, 3]).await?;
registry.copy("mem://upload.bin", "stored/upload.bin").await?;   // memory → disk
assert!(registry.read("s3://bucket/key").await.is_err());        // nothing registered
# Ok(()) }
```

## Cross-backend copy

```rust
pub async fn copy_between(src: &dyn Vfs, src_path: &str, dst: &dyn Vfs, dst_path: &str) -> Result<u64, VfsError>;
```

Copies one file from one backend to another (or within one), reading with
`open` and writing with `create` in 64 KiB chunks, so the file is not held in
memory whole when both backends stream. The destination appears only when the
copy has completed; on error nothing is left there. Returns the bytes copied.

## Glob

```rust
pub fn glob_match(pattern: &str, path: &str) -> bool;
pub async fn glob(fs: &dyn Vfs, root: &str, pattern: &str) -> Result<Vec<Entry>, VfsError>;
```

`glob_match` compares `/`-separated segments, case-sensitively, anchored at
both ends:

| Pattern | Matches |
| --- | --- |
| `*` | any run of characters within one segment (including none) |
| `?` | exactly one character within a segment |
| `**` (a whole segment) | any number of whole segments, including none |

There are no character classes (`[a-z]`), brace alternatives (`{a,b}`) or
escapes; `**` inside a longer segment behaves like `*`. Because `**` may match
nothing, `src/**` matches `src` itself as well as everything under it.

`glob(fs, root, pattern)` returns every entry under `root` — files and
directories — whose path **relative to `root`** matches, sorted by path. It
explores with `list`, never follows symbolic links, and stops at the pattern's
depth when there is no `**`.

```rust
use ferroly::vfs::{glob, glob_match, MemoryFs, Vfs};

# async fn ex() -> Result<(), ferroly::vfs::VfsError> {
assert!(glob_match("**/*.rs", "src/vfs/mod.rs"));
assert!(!glob_match("*.rs", "src/lib.rs"));        // `*` stays in one segment

let fs = MemoryFs::new();
fs.mkdir_all("/site/css").await?;
fs.write("/site/css/main.css", vec![]).await?;
let styles = glob(&fs, "/site", "**/*.css").await?;
assert_eq!(styles[0].path, "/site/css/main.css");
# Ok(()) }
```

## Checksum

```rust
#[cfg(feature = "hash")]
pub async fn checksum_sha256(fs: &dyn Vfs, path: &str) -> Result<ferroly::hash::Digest<32>, VfsError>;
```

Streams a file through SHA-256 in 64 KiB chunks. Available only when the
`hash` feature is enabled alongside `vfs`.

## Error handling

```rust
#[non_exhaustive]
pub enum VfsError {
    NotFound(String),                                       // path does not exist
    Permission(String),                                     // permission denied
    NotDir(String),                                         // a file where a directory is needed
    IsDir(String),                                          // a directory where a file is needed
    Unsupported(String),                                    // backend doesn't support it
    InvalidPath { path: String, reason: String },           // foreign scheme, or an escape from a root
    Io { path: String, source: std::io::Error },            // other lower-level I/O error
}
```

`VfsError` implements `std::error::Error` (with `source()` chaining through the
`Io` variant) and `Display`. `VfsError::from_io(path, err)` is public so other
backends map errors the same way.

How `LocalFs` classifies failures:

| Situation | Error |
| --- | --- |
| the path does not exist | `NotFound` |
| the OS denies access, or `write` targets a read-only file | `Permission` |
| `read` / `open` / `read_range` / `write` / `append` / `create` / `copy` source on a directory | `IsDir` |
| `list` / `list_page` / `walk` / `mkdir_all` on a file | `NotDir` |
| a parent component of the path is a file (`a.txt/x`) | `NotDir` |
| a scheme other than `file`, or (rooted) `..`, an absolute path, an escaping or dangling symlink, an operation on the root itself | `InvalidPath` |
| anything else | `Io { path, source }` |

The directory/file cases are decided by examining what is on disk after the
operation fails, not by the `io::ErrorKind` — those kinds cannot be named at
this crate's minimum Rust version and differ between platforms (Windows
reports "access denied" for a directory read) — so the result is the same
everywhere. `MemoryFs` reports the same variants for the same situations.

```rust
use ferroly::vfs::{LocalFs, Vfs, VfsError};

# async fn ex() -> Result<(), VfsError> {
let fs = LocalFs::rooted("/srv/uploads")?;
match fs.read("reports/q1.csv").await {
    Ok(bytes) => println!("{} bytes", bytes.len()),
    Err(VfsError::NotFound(p)) => eprintln!("missing: {p}"),
    Err(VfsError::IsDir(p)) => eprintln!("{p} is a directory"),
    Err(VfsError::InvalidPath { path, reason }) => eprintln!("rejected {path}: {reason}"),
    Err(e) => eprintln!("io error: {e}"),
}
# Ok(()) }
```

The [`Vfile`](#vfile) read-all helpers return raw `std::io::Error`, not `VfsError`.

## Writing a backend

Implement the required [`Vfs`](#vfs) methods; the provided ones (`append`,
`create`, `list_page`) work on top of them and can be overridden when the
storage system can do better.

- Return `Vfile::new(reader, meta)` (or `Vfile::from_bytes`) from `open`.
- Build `Metadata` with `Metadata::file(len)` / `Metadata::dir()` and the
  `with_*` methods.
- Map I/O errors with `VfsError::from_io`, and reject URL schemes that are not
  yours with `VfsError::InvalidPath` (use `scheme_of` to find the scheme).
- Never size a buffer by `read_range`'s `len`; make `write` replace atomically
  if the storage system allows.
- For a streaming `create`, implement `WriteSink` and wrap it in
  `VfsWriter::new`; nothing may become visible before `finish`.

The backend then works with [`VfsRegistry`](#vfsregistry),
[`copy_between`](#cross-backend-copy), [`glob`](#glob) and
[`checksum_sha256`](#checksum).

## Limitations

- **No bundled object-store backends.** S3, GCS and Azure are out of scope for
  this module; implement [`Vfs`](#vfs) for them.
- **Rooted confinement is path confinement**, not a sandbox against a process
  that can modify the tree concurrently — see
  [its limits](#rooted-confined-to-one-directory).
- **`file://` URLs are not percent-decoded**, and no other URL features
  (queries, fragments) are interpreted.
- **`append` is not atomic**, and the default `append` / `create` hold the
  whole file in memory; only `LocalFs` overrides them.
- **`list_page` is by name, not a snapshot**, and `LocalFs` still reads every
  name in the directory to order them.
- **No file locking and no change notification.**
- **`walk` and `glob` do not follow symbolic links**; walk order is unspecified.
- **No ownership information** in [`Metadata`](#metadata), and `mode` is Unix-only.

## See also

- [fsutils](fsutils.md) — content-type detection, atomic writes, temp files and lock files for plain `std::fs` code.
- [rt](rt.md) — `ferroly::rt::fs` is the raw tokio file surface.
- [codec](codec.md) — encode/decode file contents.
- [hash](hash.md) — the digest type returned by `checksum_sha256`.

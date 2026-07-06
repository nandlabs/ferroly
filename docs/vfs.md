# Virtual filesystem (VFS)

**Feature:** `vfs` · **Module:** `ferroly::vfs`

A virtual filesystem abstraction — a [`Vfs`](#vfs) trait plus [`LocalFs`](#localfs), a local backend built on tokio's non-blocking `fs`.

## Overview

The module gives you one trait, [`Vfs`](#vfs), describing every filesystem operation, and one ready-made implementation, [`LocalFs`](#localfs), backed by the real local disk. Because the operations are trait methods, code written against `Vfs` works unchanged over future cloud backends.

Key pieces:

- [`Vfs`](#vfs) — the trait: `read` / `read_range` / `write` / `open` / `delete` / `exists` / `metadata` / `list` / `mkdir_all` / `copy` / `rename` / `walk`, all async, all taking `&str` paths.
- [`Vfile`](#vfile) — an open file: a streaming async reader (`AsyncRead`) with attached [`Metadata`](#metadata) and read-all helpers.
- [`Metadata`](#metadata) / [`Entry`](#entry) — node info and directory entries.
- **[`WalkAction`](#walkaction)** — the `Continue` / `SkipDir` / `Stop` verdict a walk callback returns.
- [`WalkFn`](#walkfn) — the walk callback type.
- [`LocalFs`](#localfs) — the local-disk backend.
- [`VfsError`](#error-handling) — the error enum.

Paths are plain `&str`. A `file://` (or any `scheme://`) prefix is stripped to the bare path, so `file:///tmp/x` and `/tmp/x` are equivalent.

### Design notes

A few deliberate shape choices keep the trait small and the error handling honest:

| Concern | ferroly's design |
| --- | --- |
| Path arguments | **one** method per operation, each taking a `&str` path (`file://` stripped) — no string-twin overloads |
| Error reporting | a typed [`VfsError`](#error-handling) enum (`io::Error` kinds map onto it), not sentinel error values |
| Walk control flow | a [`WalkAction`](#walkaction) return value, keeping traversal control out of the error channel |
| Cancellation | drop the future — no separate context-carrying method variants |

Cloud backends (S3, GCS) are planned satellite crates that will implement the same [`Vfs`](#vfs) trait — see the [roadmap](roadmap.md).

## Enabling

The `vfs` feature pulls in tokio.

```toml
[dependencies]
ferroly = { version = "*", features = ["vfs"] }
```

## Quick start

```rust
use ferroly::vfs::{LocalFs, Vfs};

#[tokio::main]
async fn main() -> Result<(), ferroly::vfs::VfsError> {
    let fs = LocalFs::new();

    fs.write("file:///tmp/hello.txt", b"hi".to_vec()).await?;
    let bytes = fs.read("/tmp/hello.txt").await?;   // scheme optional
    assert_eq!(bytes, b"hi");
    Ok(())
}
```

## API reference

| Item | Kind | Summary |
| --- | --- | --- |
| [`Vfs`](#vfs) | trait | The filesystem: one async method per operation |
| [`LocalFs`](#localfs) | struct | Local-disk backend over `tokio::fs` |
| [`Vfile`](#vfile) | struct | An open file: `AsyncRead` + read-all helpers |
| [`Metadata`](#metadata) | struct | `is_dir`, `len` |
| [`Entry`](#entry) | struct | `path`, `name`, `is_dir`, `len` |
| [`WalkAction`](#walkaction) | enum | `Continue` / `SkipDir` / `Stop` |
| [`WalkFn`](#walkfn) | type alias | `Box<dyn FnMut(&Entry) -> WalkAction + Send>` |
| [`VfsError`](#error-handling) | enum | `NotFound` / `Permission` / `NotDir` / `IsDir` / `Unsupported` / `Io` |
| [`BoxFuture`](#boxfuture) | type alias | `Pin<Box<dyn Future<Output = T> + Send + 'a>>` |

### BoxFuture

```rust
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
```

The object-safe async return type of every [`Vfs`](#vfs) method. You never construct it — just `.await` the result of a call.

## Vfs

```rust
pub trait Vfs: Send + Sync {
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
}
```

The whole abstraction. Every method is async and takes `&str` paths. There is **one method per operation** — no string-twin overloads; every method already accepts a string path, with any `scheme://` prefix stripped.

| Method | Behaviour |
| --- | --- |
| `schemes()` | The URL schemes this backend handles — `["file"]` for [`LocalFs`](#localfs). |
| `read(path)` | Read a whole file into a `Vec<u8>`. |
| `read_range(path, offset, len)` | Read up to `len` bytes starting at `offset` (short at EOF). |
| `write(path, data)` | Write a whole file, creating or truncating it. |
| `open(path)` | Open a file as a streaming [`Vfile`](#vfile) reader. |
| `delete(path)` | Delete a file, or a directory and all its contents. |
| `exists(path)` | `bool` — whether the path exists (never errors). |
| `metadata(path)` | [`Metadata`](#metadata) for the path. |
| `list(path)` | One directory level as a `Vec<`[`Entry`](#entry)`>`. |
| `mkdir_all(path)` | Create a directory and all missing parents. |
| `copy(src, dst)` | Copy a file. |
| `rename(src, dst)` | Rename / move a path. |
| `walk(path, f)` | Recursively visit every entry, honoring [`WalkAction`](#walkaction). |

## Metadata

```rust
pub struct Metadata {
    pub is_dir: bool,
    pub len: u64,     // 0 for directories
}
```

Returned by [`Vfs::metadata`](#vfs) and available on an open [`Vfile`](#vfile). `Clone`, `PartialEq`, `Eq`.

```rust
use ferroly::vfs::{LocalFs, Vfs};

# async fn ex() -> Result<(), ferroly::vfs::VfsError> {
let fs = LocalFs::new();
let m = fs.metadata("/tmp/hello.txt").await?;
println!("dir={} size={}", m.is_dir, m.len);
# Ok(()) }
```

## Entry

```rust
pub struct Entry {
    pub path: String,    // full path
    pub name: String,    // final path component
    pub is_dir: bool,
    pub len: u64,
}
```

A directory entry produced by [`Vfs::list`](#vfs) and passed to the [`walk`](#walk) callback. `Clone`, `PartialEq`, `Eq`.

## Vfile

```rust
pub struct Vfile { /* … */ }   // impls AsyncRead

impl Vfile {
    pub fn metadata(&self) -> &Metadata;
    pub async fn read_all_bytes(self) -> Result<Vec<u8>, std::io::Error>;
    pub async fn read_all_string(self) -> Result<String, std::io::Error>;
}
```

An open file returned by [`Vfs::open`](#vfs). It is a **streaming** reader: it implements tokio's `AsyncRead`, so you can read it incrementally with `AsyncReadExt` (`read`, `read_buf`, …) or copy it with `tokio::io::copy` — ideal for large files you do not want fully in memory.

- `metadata()` — the [`Metadata`](#metadata) captured when the file was opened.
- `read_all_bytes(self)` — consume the file, returning all bytes.
- `read_all_string(self)` — consume the file, returning a UTF-8 `String` (errors with `InvalidData` on non-UTF-8).

Note the read-all helpers take `self` by value (they consume the file). The errors here are raw `std::io::Error`, not [`VfsError`](#error-handling).

```rust
use ferroly::vfs::{LocalFs, Vfs};
use tokio::io::AsyncReadExt;

# async fn ex() -> Result<(), Box<dyn std::error::Error>> {
let fs = LocalFs::new();
let mut vf = fs.open("/tmp/hello.txt").await?;
println!("size = {}", vf.metadata().len);

// Streaming: read the first 4 bytes.
let mut head = [0u8; 4];
let n = vf.read(&mut head).await?;
println!("first {n} bytes: {:?}", &head[..n]);

// Or slurp the rest / whole thing at once:
let text = fs.open("/tmp/hello.txt").await?.read_all_string().await?;
println!("{text}");
# Ok(()) }
```

## WalkAction

```rust
pub enum WalkAction {
    Continue,   // keep going (descend into directories)
    SkipDir,    // skip descending into the just-visited directory
    Stop,       // stop the whole walk
}
```

**This is the module's signature design choice.** Rather than overloading the error channel with traversal control (a callback returning a sentinel "skip" or "stop" error), ferroly makes control flow an explicit **return value**. Your [`WalkFn`](#walkfn) returns a `WalkAction`, and [`walk`](#walk) acts on it:

- `Continue` — proceed; if the visited entry is a directory, descend into it.
- `SkipDir` — do not descend into the just-visited directory (but keep walking siblings).
- `Stop` — end the entire walk immediately, returning `Ok(())`.

Real errors stay in the `Result` where they belong.

## WalkFn

```rust
pub type WalkFn = Box<dyn FnMut(&Entry) -> WalkAction + Send>;
```

The callback [`walk`](#walk) invokes once per [`Entry`](#entry). It is `FnMut`, so it can accumulate state (e.g. push names into a captured `Vec`), and `Send` so the walk can run across `.await` points. Wrap your closure in `Box::new`.

## LocalFs

```rust
pub struct LocalFs;   // Debug, Default, Clone

impl LocalFs {
    pub fn new() -> Self;
}
```

The local-disk [`Vfs`](#vfs) implementation, backed entirely by tokio's non-blocking `fs` — no operation blocks the async runtime. It is a zero-sized, `Clone` handle; `schemes()` returns `["file"]`.

Implementation notes worth knowing:

- `read_range` opens the file, seeks to `offset`, and reads up to `len` bytes, looping until it has `len` or hits EOF — so a range past the end returns a short (possibly empty) buffer rather than erroring.
- `delete` stats the path first: a directory is removed recursively (`remove_dir_all`), a file with `remove_file`.
- `exists` never errors — a failed stat is reported as `false`.
- `walk` is iterative (an explicit stack), not recursive, so deep trees will not overflow the stack.

### Write / read

```rust
use ferroly::vfs::{LocalFs, Vfs};

# async fn ex() -> Result<(), ferroly::vfs::VfsError> {
let fs = LocalFs::new();
fs.mkdir_all("/tmp/demo").await?;
fs.write("/tmp/demo/a.txt", b"hello world".to_vec()).await?;

let bytes = fs.read("/tmp/demo/a.txt").await?;
assert_eq!(bytes, b"hello world");
assert!(fs.exists("/tmp/demo/a.txt").await);
# Ok(()) }
```

### Range read

```rust
use ferroly::vfs::{LocalFs, Vfs};

# async fn ex() -> Result<(), ferroly::vfs::VfsError> {
let fs = LocalFs::new();
fs.write("/tmp/demo/a.txt", b"hello world".to_vec()).await?;

let slice = fs.read_range("/tmp/demo/a.txt", 6, 5).await?;  // bytes [6, 11)
assert_eq!(slice, b"world");
# Ok(()) }
```

### List a directory

```rust
use ferroly::vfs::{Entry, LocalFs, Vfs};

# async fn ex() -> Result<(), ferroly::vfs::VfsError> {
let fs = LocalFs::new();
for e in fs.list("/tmp/demo").await? {
    let Entry { name, is_dir, len, .. } = e;
    println!("{name}\t{}\t{len}", if is_dir { "dir" } else { "file" });
}
# Ok(()) }
```

### Walk with SkipDir

`walk` visits every entry under the root. Returning [`WalkAction::SkipDir`](#walkaction) from a directory entry stops the walk from descending into it; the directory itself is still visited.

```rust
use ferroly::vfs::{Entry, LocalFs, Vfs, WalkAction};
use std::sync::{Arc, Mutex};

# async fn ex() -> Result<(), ferroly::vfs::VfsError> {
let fs = LocalFs::new();
let seen = Arc::new(Mutex::new(Vec::<String>::new()));

let sink = seen.clone();
fs.walk("/tmp/demo", Box::new(move |e: &Entry| {
    sink.lock().unwrap().push(e.name.clone());
    if e.is_dir {
        WalkAction::SkipDir      // visit the dir, but don't recurse into it
    } else {
        WalkAction::Continue
    }
})).await?;

// Top-level files and dirs appear; children of skipped dirs do not.
println!("{:?}", seen.lock().unwrap());
# Ok(()) }
```

To stop the entire traversal early (for example after finding a target), return [`WalkAction::Stop`](#walkaction) — `walk` returns `Ok(())` immediately without visiting anything further:

```rust
use ferroly::vfs::{Entry, LocalFs, Vfs, WalkAction};
use std::sync::{Arc, Mutex};

# async fn ex() -> Result<(), ferroly::vfs::VfsError> {
let fs = LocalFs::new();
let found = Arc::new(Mutex::new(None::<String>));

let sink = found.clone();
fs.walk("/tmp/demo", Box::new(move |e: &Entry| {
    if e.name == "needle.txt" {
        *sink.lock().unwrap() = Some(e.path.clone());
        WalkAction::Stop            // stop the whole walk
    } else {
        WalkAction::Continue        // keep descending
    }
})).await?;

println!("found: {:?}", found.lock().unwrap());
# Ok(()) }
```

### Copy, rename, delete

```rust
use ferroly::vfs::{LocalFs, Vfs};

# async fn ex() -> Result<(), ferroly::vfs::VfsError> {
let fs = LocalFs::new();
fs.rename("/tmp/demo/a.txt", "/tmp/demo/a2.txt").await?;
fs.copy("/tmp/demo/a2.txt", "/tmp/demo/a3.txt").await?;
fs.delete("/tmp/demo").await?;             // recursive for directories
assert!(!fs.exists("/tmp/demo").await);
# Ok(()) }
```

## Error handling

```rust
pub enum VfsError {
    NotFound(String),                                       // path does not exist
    Permission(String),                                     // permission denied
    NotDir(String),                                         // expected a directory
    IsDir(String),                                          // expected a file
    Unsupported(String),                                    // backend doesn't support it
    Io { path: String, source: std::io::Error },            // other lower-level I/O error
}
```

`VfsError` derives ferroly's error machinery (via `ferroly_derive::FerrolyError`) — it implements `std::error::Error` (with `source()` chaining through the `Io` variant) and `Display`.

`LocalFs` maps `std::io::Error` kinds onto the sentinel-style variants: `ErrorKind::NotFound` → `NotFound`, `PermissionDenied` → `Permission`, and everything else into `Io { path, source }` (preserving the original error). Match on the variant rather than inspecting messages:

```rust
use ferroly::vfs::{LocalFs, Vfs, VfsError};

# async fn ex() {
let fs = LocalFs::new();
match fs.read("/no/such/path").await {
    Ok(bytes) => println!("{} bytes", bytes.len()),
    Err(VfsError::NotFound(p)) => eprintln!("missing: {p}"),
    Err(VfsError::Permission(p)) => eprintln!("denied: {p}"),
    Err(e) => eprintln!("io error: {e}"),
}
# }
```

`NotDir`, `IsDir`, and `Unsupported` are part of the shared vocabulary for backends; `LocalFs` primarily surfaces `NotFound`, `Permission`, and `Io`. The [`Vfile`](#vfile) read-all helpers return raw `std::io::Error`, not `VfsError`.

## Limitations

- **`LocalFs` is the only bundled backend.** Cloud filesystems (S3, GCS) are [planned satellite crates](roadmap.md) implementing the same [`Vfs`](#vfs) trait.
- **`read_range` is best-effort length.** A range extending past EOF returns a short buffer, not an error.
- **No permissions / mode / timestamps.** [`Metadata`](#metadata) exposes only `is_dir` and `len`.
- **`write` is whole-file.** There is no append or streaming-write API; use it to create or truncate. For incremental reading, use [`open`](#vfile).
- **`walk` order is not specified.** Entries are produced from a stack of directories; do not rely on a particular traversal order.

### Design rationale recap

- **One method per operation** — every method already takes a `&str` path, so there are no string-twin overloads.
- **Typed errors** — I/O failures surface as the [`VfsError`](#error-handling) enum rather than sentinel error values.
- **Walk control is a return value** — the [`WalkAction`](#walkaction) enum keeps traversal steering out of the error channel.
- **Cancellation is by dropping the future** — there are no separate context-carrying method variants.

## See also

- [fsutils](fsutils.md) — path and filesystem helper utilities.
- [codec](codec.md) — encode/decode file contents.
- [roadmap](roadmap.md) — planned cloud VFS satellites.

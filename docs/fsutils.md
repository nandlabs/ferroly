# ferroly::fsutils

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `fsutils` — module `ferroly::fsutils`. No external dependencies
(std only).

## Overview

`fsutils` is a small set of local-filesystem helpers:

- **Content-type (MIME) detection** — from a filename extension
  (`lookup_content_type`, `mime_for_ext`) or from leading bytes
  (`detect_content_type`, `sniff`). An in-house table and sniffer replace
  `mime_guess`/`infer`. **The result is a hint, not validation.**
- **Read-only file views** — [`Mmap`](#read-only-file-views-mmap): a safe
  in-memory read, or an `unsafe` zero-copy OS mapping.
- **Atomic writes** — [`write_atomic` / `AtomicFile`](#atomic-writes).
- **Temporary files and directories** — [`TempDir` / `TempFile`](#temporary-files-and-directories),
  uniquely named and removed on drop.
- **An advisory lock file** — [`LockFile`](#advisory-lock-file).

Filesystem *existence* helpers are intentionally **not** provided: use
`std::path::Path` (`exists`, `is_file`, `is_dir`). Directory watching is out of
scope. For a virtual/abstract filesystem (local with root confinement,
in-memory, pluggable backends), see the separate [vfs](vfs.md) module.

## Enabling

```toml
[dependencies]
ferroly = { version = "0.3", features = ["fsutils"] }
```

## Quick start

```rust
use ferroly::fsutils::{detect_content_type, lookup_content_type, write_atomic, TempDir};

// By extension (no disk access):
assert_eq!(lookup_content_type("config.json"), Some("application/json".to_string()));
assert_eq!(lookup_content_type("jobs.yaml"),   Some("application/yaml".to_string()));

// By content (reads the file's leading bytes):
let dir = TempDir::new("my-app")?;                 // removed when `dir` drops
let path = dir.path().join("payload");
write_atomic(&path, b"{\"ok\": true}")?;           // all-or-nothing
assert_eq!(detect_content_type(&path)?, "application/json");
# Ok::<(), ferroly::fsutils::FsError>(())
```

## API reference

| Item | Description |
|---|---|
| `lookup_content_type(filename: &str) -> Option<String>` | MIME from a filename's extension. `None` if there is no extension or it is unknown. |
| `mime_for_ext(ext: &str) -> Option<&'static str>` | MIME for an already-lowercased extension (no leading dot). |
| `detect_content_type(path: impl AsRef<Path>) -> Result<String, FsError>` | Read a file's leading bytes (up to `SNIFF_LEN`) and sniff its MIME. |
| `sniff(bytes: &[u8]) -> Option<&'static str>` | Sniff a MIME type from an in-memory header. `None` if unrecognized. |
| `SNIFF_LEN: usize` | How many leading bytes `detect_content_type` reads (512). |
| `Mmap` | A read-only view of a whole file's bytes. |
| `write_atomic(path, data) -> Result<(), FsError>` | Replace a file in one step. |
| `AtomicFile` | The streaming form of `write_atomic` (`impl std::io::Write`, then `commit`). |
| `TempDir`, `TempFile` | Uniquely named scratch directory / file, removed on drop. |
| `LockFile` | An advisory lock held by a create-new file. |

### `FsError`

```rust
#[non_exhaustive]
pub enum FsError {
    Io(std::io::Error),   // underlying I/O error (From<std::io::Error>)
    Undetermined(String), // content type not recognized for the given path
    Locked(String),       // a LockFile at this path is already held
}
```

Implements `std::error::Error`. `Io` is `#[from]`, so `?` on a filesystem call
converts automatically.

## Content types are a hint, not validation

Neither lookup proves anything about a file:

- An **extension** is whatever the uploader typed.
- A **sniffed signature** says only that the first bytes *resemble* a format. A
  file that starts with `%PDF-` is not thereby a well-formed (or harmless) PDF,
  and many formats share a container (every Office Open XML document sniffs as
  `application/zip`).

Use the result to pick a parser, set a `Content-Type` header, or choose an
icon. **Never use it to decide that untrusted input is safe** — validate by
actually parsing the content with a parser for the type you expect.

## Detection by filename extension

`lookup_content_type` takes the file name (anything after the last `/` or `\`),
then the part after its last `.`, lowercases it, and delegates to
`mime_for_ext`. Only the last extension counts, so `archive.tar.gz` resolves
via `gz` to `application/gzip`; a dot in a *directory* name is never mistaken
for an extension (`conf.d/settings` → `None`).

`mime_for_ext` is the underlying table (call it directly when you already have
a bare, lowercase extension). Recognized extensions include:

- **Text:** `txt`/`text`/`log`/`conf`/`cfg`/`ini`/`env` → `text/plain`,
  `html`/`htm`/`xhtml`, `css`, `csv`, `tsv`, `md`/`markdown`, `rtf`,
  `ics` → `text/calendar`, `vcf` → `text/vcard`, `vtt`, `srt`.
- **Code / structured data:** `js`/`mjs`/`cjs` → `application/javascript`,
  `json`/`map`, `jsonl`/`ndjson` → `application/x-ndjson`, `jsonld`, `geojson`,
  `webmanifest`, `xml`/`xsd`/`xsl` → `text/xml`, `rss`, `atom`,
  `yaml`/`yml` → `application/yaml`, `toml` → `application/toml`,
  `proto` → `text/x-protobuf`, `pb`/`protobuf`/`binpb` → `application/x-protobuf`,
  `graphql`, `sql`, `sh`, `wasm` → `application/wasm`, and common source files
  (`rs`, `py`, `go`, `java`, `c`/`h`, `cpp`, `tsx`, `jsx`).
- **Data files:** `parquet` → `application/vnd.apache.parquet`,
  `arrow`/`feather`, `avro`, `orc`, `sqlite`/`sqlite3`, `bin`/`dat`.
- **Documents:** `pdf`, `ps`/`eps`, `epub`, `doc`, `docx`, `dotx`, `xls`,
  `xlsx`, `xltx`, `xlsm`, `ppt`, `pptx`, `ppsx`, `potx`, `odt`, `ods`, `odp`,
  `odg`, `msg`, `eml`.
- **Images:** `png`, `apng`, `jpg`/`jpeg`, `gif`, `webp`, `avif`, `heic`,
  `heif`, `jxl`, `svg` → `image/svg+xml`, `ico` → `image/x-icon`, `bmp`,
  `tif`/`tiff`, `psd`.
- **Audio / video:** `mp3` → `audio/mpeg`, `wav`, `ogg`, `opus`, `flac`, `aac`,
  `m4a`, `mid`, `aiff`; `mp4`/`m4v`, `webm`, `mkv`, `mov`, `avi`, `mpeg`, `ogv`,
  `3gp`, `m3u8`, `mpd`.
- **Archives / compression:** `zip`, `gz`/`gzip`/`tgz` → `application/gzip`,
  `tar` → `application/x-tar`, `zst` → `application/zstd`, `xz`, `bz2`, `7z`,
  `rar`, `lz4`, `br`, `jar`, `apk`, `deb`, `rpm`, `dmg`, `iso`.
- **Fonts:** `woff`, `woff2`, `ttf`, `otf`, `ttc` → `font/collection`, `eot`.
- **Security:** `pem`/`crt`/`cer`, `p12`/`pfx`.

Any other extension returns `None`.

## Detection by content

`detect_content_type(path)` opens the file, reads up to `SNIFF_LEN` (512) bytes
— enough to reach a tar header's marker — and calls `sniff` on them. It returns
`FsError::Io` on a read failure and `FsError::Undetermined(path)` when nothing
is recognized (including for an empty file).

`sniff(bytes)` first matches binary magic signatures, then falls back to a few
text checks. It never indexes out of bounds: a truncated header simply does not
match.

**Binary signatures**

| Bytes / signature | MIME |
|---|---|
| `89 50 4E 47 0D 0A 1A 0A` | `image/png` |
| `FF D8 FF` | `image/jpeg` |
| `GIF87a` / `GIF89a` | `image/gif` |
| `II*\0` / `MM\0*` | `image/tiff` |
| `8BPS` | `image/vnd.adobe.photoshop` |
| `FF 0A` / JXL container box | `image/jxl` |
| `00 00 01 00` | `image/x-icon` |
| `BM` + zero reserved field (bytes 6–9) | `image/bmp` |
| `RIFF`…`WEBP` / `WAVE` / `AVI ` (bytes 0–3 and 8–11) | `image/webp` / `audio/wav` / `video/x-msvideo` |
| `FORM`…`AIFF` | `audio/aiff` |
| `ftyp` at bytes 4–7, by major brand: `avif`/`avis` · `heic`/`heix`/… · `mif1`/`msf1` · `qt  ` · `M4A ` · `M4V ` · `3gp…` · `3g2…` · anything else (`isom`, `mp42`, …) | `image/avif` · `image/heic` · `image/heif` · `video/quicktime` · `audio/mp4` · `video/x-m4v` · `video/3gpp` · `video/3gpp2` · `video/mp4` |
| `OggS` | `audio/ogg` |
| `fLaC` | `audio/flac` |
| `MThd` | `audio/midi` |
| `ID3` / `FF FB` / `FF FA` / `FF F3` / `FF F2` | `audio/mpeg` |
| `1A 45 DF A3` (EBML), doctype `webm` or not | `video/webm` / `video/x-matroska` |
| `%PDF-` | `application/pdf` |
| `%!PS` | `application/postscript` |
| `{\rtf` | `application/rtf` |
| `D0 CF 11 E0 A1 B1 1A E1` (legacy Office / OLE) | `application/x-ole-storage` |
| `PK\x03\x04` / `PK\x05\x06` / `PK\x07\x08` | `application/zip`, or the stored `mimetype` entry for EPUB / OpenDocument |
| `1F 8B` | `application/gzip` |
| `28 B5 2F FD` | `application/zstd` |
| `FD 37 7A 58 5A 00` | `application/x-xz` |
| `BZh` + digit `1`–`9` | `application/x-bzip2` |
| `37 7A BC AF 27 1C` | `application/x-7z-compressed` |
| `Rar!\x1a\x07` | `application/vnd.rar` |
| `04 22 4D 18` | `application/x-lz4` |
| `ustar` at byte 257 | `application/x-tar` |
| `\0asm` | `application/wasm` |
| `\x7fELF` | `application/x-elf` |
| `SQLite format 3\0` | `application/vnd.sqlite3` |
| `PAR1` | `application/vnd.apache.parquet` |
| `ARROW1` | `application/vnd.apache.arrow.file` |
| `Obj\x01` | `application/avro` |
| `wOFF` / `wOF2` / `OTTO` / `ttcf` / `00 01 00 00 00` | `font/woff` / `font/woff2` / `font/otf` / `font/collection` / `font/ttf` |

**Text checks** (only if no binary signature matched)

| Content | MIME |
|---|---|
| UTF-16 byte-order mark (`FF FE` / `FE FF`) | `text/plain` |
| optional UTF-8 BOM and whitespace, then `<!doctype html` or `<html` (any case) | `text/html` |
| …then `<svg`, or `<?xml` with `<svg` further on | `image/svg+xml` |
| …then `<?xml` | `text/xml` |
| …then `{` followed by `"` or `}`, or `[` followed by the start of a JSON value | `application/json` |
| otherwise valid UTF-8 with no control characters other than tab, newline, carriage return, form feed and escape (a multibyte character cut off by the end of the buffer is tolerated) | `text/plain` |

Empty input, whitespace-only input, and unrecognized binary yield `None` (and
hence `FsError::Undetermined` from `detect_content_type`).

```rust
use ferroly::fsutils::sniff;

assert_eq!(sniff(b"\x89PNG\r\n\x1a\n....."),   Some("image/png"));
assert_eq!(sniff(b"%PDF-1.7"),                Some("application/pdf"));
assert_eq!(sniff(b"\x28\xb5\x2f\xfd"),        Some("application/zstd"));
assert_eq!(sniff(b"  {\"a\": 1}"),            Some("application/json"));
assert_eq!(sniff(b"just some words"),         Some("text/plain"));
assert_eq!(sniff(b"\x00\x9c\x01\x02\xfe"),    None);
```

Limits worth knowing: Office Open XML files (`.docx`/`.xlsx`/`.pptx`) cannot be
told apart from a leading header and report `application/zip`; legacy Office
files all report `application/x-ole-storage`; YAML, TOML, CSV and source code
have no signature and report `text/plain`. Use extension lookup for those.

## Extension lookup vs. content sniffing

- **`lookup_content_type` / `mime_for_ext`** — cheap, no I/O, and cover a broad
  set of types, including ones with no signature. Trusts the filename.
- **`detect_content_type` / `sniff`** — inspect the actual bytes, so they catch
  a wrong or missing extension, but distinguish fewer types.

When both are available and they disagree, treat that as a signal in itself.

## Read-only file views: `Mmap`

`Mmap` is a read-only view of a whole file that dereferences to `&[u8]`. There
are two constructors, and the difference is soundness:

| Constructor | Safe? | Backing | Cost |
|---|---|---|---|
| `Mmap::open(path)` | **yes** | the file is read into memory | a full read; memory equal to the file size |
| `unsafe Mmap::map(path)` | **no** — caller upholds a contract | a real `PROT_READ \| MAP_PRIVATE` OS mapping, pages faulted in lazily | none up front |

```rust
use ferroly::fsutils::Mmap;
# let path = std::env::temp_dir().join("ferroly-doc-mmap.bin");
# std::fs::write(&path, b"hello mmap").unwrap();
let view = Mmap::open(&path)?;         // safe: a private copy of the bytes
assert_eq!(&view[..5], b"hello");      // Deref<Target = [u8]>
assert_eq!(view.len(), 10);
assert!(!view.is_mapped());
# std::fs::remove_file(&path).ok();
# Ok::<(), ferroly::fsutils::FsError>(())
```

| Method | Returns | Notes |
|---|---|---|
| `Mmap::open(path)` | `Result<Mmap, FsError>` | safe; reads the file; empty file → empty view |
| `unsafe Mmap::map(path)` | `Result<Mmap, FsError>` | OS mapping where supported, otherwise the same as `open` |
| `Mmap::map_supported()` | `bool` (`const`) | whether `map` creates a real mapping on this target |
| `is_mapped()` | `bool` | whether *this* view is a live mapping |
| deref / `as_bytes()` / `as_ref()` | `&[u8]` | the whole file |
| `len()` / `is_empty()` | `usize` / `bool` | view size |

A view is `Send + Sync` and releases its memory (or mapping) on drop.

### The `Mmap::map` safety contract

A memory mapping is a window onto the file, not a copy of it. `MAP_PRIVATE`
keeps *this process's writes* out of the file; it does **not** freeze the file's
contents — until a page is copied (which a read-only mapping never does) it
still reflects the file. So the caller of `Mmap::map` must guarantee that, from
the call until the `Mmap` is dropped, **no process (including this one) modifies
the mapped bytes or truncates the file**:

- a write to the file would change bytes behind a live `&[u8]` — undefined
  behavior in Rust;
- a truncation makes any access to pages past the new end raise `SIGBUS`,
  killing the process.

Ways to uphold it: map files your process created with restrictive permissions
and never rewrites in place, or immutable artifacts that are only ever replaced
by rename ([`write_atomic`](#atomic-writes) gives the path a new inode and
leaves an existing mapping untouched). If you cannot guarantee it, use
`Mmap::open`.

`Mmap::map` fails if the path cannot be opened, is not a regular file, or is
too large to address on the target. An empty file is returned as an empty,
unmapped view (a zero-length mapping is invalid).

### Where `map` really maps

The `mmap`/`munmap` FFI is hand-declared (the crate takes no `libc`
dependency), so it is compiled only for targets whose ABI was checked — `off_t`
is 64-bit, and `PROT_READ = 1`, `MAP_PRIVATE = 2`, `MAP_FAILED = (void*)-1`:

- 64-bit Linux (glibc or musl) and 64-bit Android,
- 64-bit macOS and iOS,
- 64-bit FreeBSD, DragonFly, NetBSD and OpenBSD.

The values were cross-checked against the `libc` crate's per-target
definitions; of these, only macOS is exercised by this crate's own test runs.
Everywhere else — Windows, every 32-bit target, other Unixes — `Mmap::map`
reads the file into memory exactly like `Mmap::open`, and `map_supported()`
returns `false`.

> **`unsafe` in this crate.** This module's `mmap` submodule is the workspace's
> single `unsafe` region (everything else is built with `unsafe_code = "deny"`).
> It is audited, but its soundness rests on the contract above: the safe
> constructor is `Mmap::open`, and `Mmap::map` is `unsafe` precisely because
> the compiler cannot rule out another process changing the file.

## Atomic writes

```rust
pub fn write_atomic(path: impl AsRef<Path>, data: impl AsRef<[u8]>) -> Result<(), FsError>;

pub struct AtomicFile { /* … */ }   // impl std::io::Write
impl AtomicFile {
    pub fn create(dest: impl AsRef<Path>) -> Result<AtomicFile, FsError>;
    pub fn path(&self) -> &Path;
    pub fn commit(self) -> Result<(), FsError>;
}
```

Bytes go to a uniquely named temporary file (`.<name>.<unique>.tmp`) in the
**same directory** as the destination; `commit` flushes it to disk (`fsync`),
renames it over the destination, and on Unix flushes the directory too.
Dropping an `AtomicFile` without committing deletes the temporary file and
leaves the destination untouched.

```rust
use ferroly::fsutils::{write_atomic, AtomicFile, TempDir};
use std::io::Write;

let dir = TempDir::new("my-app")?;
let path = dir.path().join("state.json");

write_atomic(&path, b"{\"generation\":1}")?;

let mut draft = AtomicFile::create(&path)?;     // streaming form
draft.write_all(b"{\"generation\":")?;
draft.write_all(b"2}")?;
// readers still see generation 1 here
draft.commit()?;
# Ok::<(), ferroly::fsutils::FsError>(())
```

What this gives you, and what it does not:

- A crash or error mid-write never leaves a truncated destination; concurrent
  readers see the old file or the new one, complete.
- The destination gets a **new inode**: hard links keep the old contents, and
  anything holding the old file open keeps reading it. On Unix an existing
  destination's permission bits are copied to the new file (ownership and
  extended attributes are not); a new destination gets the usual mode of a
  created file.
- A symbolic link at the destination is followed: its target is replaced and
  the link kept.
- Two writers committing at once do not corrupt the file, but the last rename
  wins — coordinate them with a [`LockFile`](#advisory-lock-file).
- The destination's directory must be writable, and a stray temporary file can
  be left behind if the process is killed mid-write.

## Temporary files and directories

```rust
impl TempDir {
    pub fn new(prefix: &str) -> Result<TempDir, FsError>;                          // under std::env::temp_dir()
    pub fn new_in(parent: impl AsRef<Path>, prefix: &str) -> Result<TempDir, FsError>;
    pub fn path(&self) -> &Path;
    pub fn keep(self) -> PathBuf;                 // disarm: leave it in place
    pub fn close(self) -> Result<(), FsError>;    // remove now, reporting errors
}

impl TempFile {
    pub fn new(prefix: &str) -> Result<TempFile, FsError>;
    pub fn new_in(dir: impl AsRef<Path>, prefix: &str) -> Result<TempFile, FsError>;
    pub fn with_suffix_in(dir: impl AsRef<Path>, prefix: &str, suffix: &str) -> Result<TempFile, FsError>;
    pub fn path(&self) -> &Path;
    pub fn as_file(&self) -> &File;
    pub fn as_file_mut(&mut self) -> &mut File;
    pub fn keep(self) -> PathBuf;
    pub fn persist(self, dest: impl AsRef<Path>) -> Result<(), FsError>;   // fsync + rename
}
```

- Names are `<prefix>-<16 hex digits>[<suffix>]`. The token is unpredictable
  (seeded from OS randomness), and creation uses create-new semantics with
  retries, so an existing entry is never reused or truncated.
- On Unix a `TempDir` is created with mode `0700` and a `TempFile` with `0600`.
- Dropping the guard removes the directory (recursively) or the file, ignoring
  errors; `close()` removes a directory and reports them. `keep()` disarms the
  guard.
- A prefix or suffix containing a path separator is rejected.
- Cleanup runs on drop, so it does not happen if the process is killed or
  aborts.

```rust
use ferroly::fsutils::{TempDir, TempFile};
use std::io::Write;

let dir = TempDir::new("my-app")?;
let mut report = TempFile::with_suffix_in(dir.path(), "report", ".csv")?;
report.as_file_mut().write_all(b"a,b\n")?;
report.persist(dir.path().join("report.csv"))?;   // keep it under a final name
# Ok::<(), ferroly::fsutils::FsError>(())
```

## Advisory lock file

```rust
impl LockFile {
    pub fn try_acquire(path: impl AsRef<Path>) -> Result<Option<LockFile>, FsError>;   // None if held
    pub fn acquire(path: impl AsRef<Path>) -> Result<LockFile, FsError>;               // Err(Locked) if held
    pub fn acquire_timeout(path: impl AsRef<Path>, timeout: Duration) -> Result<LockFile, FsError>;
    pub fn owner(path: impl AsRef<Path>) -> Result<Option<u32>, FsError>;              // recorded pid
    pub fn break_lock(path: impl AsRef<Path>) -> Result<bool, FsError>;
    pub fn path(&self) -> &Path;
    pub fn release(self) -> Result<(), FsError>;
}
```

A lock is held by the *existence* of a file. Acquiring creates it with
create-new semantics — the one filesystem operation that atomically fails if
the name is taken — and writes the holder's process id into it. Dropping the
guard (or `release()`) deletes the file.

```rust
use ferroly::fsutils::{LockFile, TempDir};

let dir = TempDir::new("my-app")?;
let path = dir.path().join("job.lock");
let lock = LockFile::acquire(&path)?;
assert!(LockFile::try_acquire(&path)?.is_none());   // already held
drop(lock);
assert!(LockFile::try_acquire(&path)?.is_some());
# Ok::<(), ferroly::fsutils::FsError>(())
```

Know the limits:

- **Advisory.** Nothing stops a process that ignores the lock file; every
  participant must use the same path.
- **Not crash-safe.** If the holder is killed, the file stays and the lock
  looks held forever. Stale locks are never broken automatically (a pid can be
  reused, so "is that process alive" is not a safe test); inspect
  `LockFile::owner` and call `LockFile::break_lock` deliberately.
- **Not reentrant**, and not tied to a thread.
- `acquire_timeout` **blocks the calling thread**, polling with a short growing
  sleep; from async code, run it on a blocking thread.
- Create-new is atomic on local filesystems; on some network filesystems it is
  not.

## Error handling

Every fallible function returns `Result<_, FsError>`. I/O failures are
`FsError::Io`; `detect_content_type` adds `Undetermined` for an unrecognized
signature, and `LockFile::acquire` / `acquire_timeout` add `Locked`. The
pure-function variants (`lookup_content_type`, `mime_for_ext`, `sniff`) never
fail and return `Option`.

## Limitations

- **Content types are hints** — see [above](#content-types-are-a-hint-not-validation).
- **Last-extension-only, first-512-bytes-only** — `lookup_content_type` reads
  the last extension; `detect_content_type` reads at most `SNIFF_LEN` bytes.
- **`Mmap::open` copies the file**; the zero-copy `Mmap::map` is `unsafe` and
  only a real mapping on the [listed targets](#where-map-really-maps).
- **No existence/metadata helpers** — use `std::path::Path`.
- **No directory watching, no OS-level (`flock`/`fcntl`) locks** — the lock
  file is a convention between cooperating processes.
- **Not a filesystem abstraction** — for confined or pluggable file access, use
  [vfs](vfs.md).

## See also

- [vfs](vfs.md) — the virtual filesystem module (distinct from `fsutils`).
- [codec](codec.md) — MIME types from this module map to codecs for parsing.
- [config](config.md) — infers file formats from extensions in a similar spirit.

---
**Related:** [vfs](vfs.md), [codec](codec.md), [config](config.md).

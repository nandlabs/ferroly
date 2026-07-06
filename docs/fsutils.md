# ferroly::fsutils

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `fsutils` — module `ferroly::fsutils`. No external dependencies.

## Overview

`fsutils` provides **content-type (MIME) detection**. It answers two questions with
no third-party crates (`mime_guess`/`infer` are replaced by an in-house table and a
magic-byte sniffer):

- *What MIME type does this filename suggest?* — resolved from the extension, no disk
  access (`lookup_content_type`, `mime_for_ext`).
- *What MIME type do these bytes actually contain?* — resolved by sniffing leading
  magic bytes (`detect_content_type`, `sniff`).

Filesystem *existence* helpers are intentionally **not** provided: Rust callers use
`std::path::Path` directly — `Path::exists`, `Path::is_file`, `Path::is_dir`. This
module is therefore purely about identifying content type; for a full
virtual/abstract filesystem (local, in-memory, remote backends), see the separate
[vfs](vfs.md) module.

## Enabling

```toml
[dependencies]
ferroly = { version = "0.1", features = ["fsutils"] }
```

## Quick start

```rust
use ferroly::fsutils::{lookup_content_type, detect_content_type};

// By extension (no disk access):
assert_eq!(lookup_content_type("config.json"), Some("application/json".to_string()));
assert_eq!(lookup_content_type("jobs.yaml"),   Some("application/yaml".to_string()));

// By content (reads the file's leading bytes):
let mime = detect_content_type("photo.bin")?;   // e.g. "image/png"
```

## API reference

| Function | Description |
|---|---|
| `lookup_content_type(filename: &str) -> Option<String>` | MIME from a filename's extension. `None` if there is no extension or it is unknown. |
| `mime_for_ext(ext: &str) -> Option<&'static str>` | MIME for an already-lowercased extension (no leading dot). |
| `detect_content_type(path: impl AsRef<Path>) -> Result<String, FsError>` | Read a file's leading bytes and sniff its MIME from magic bytes. |
| `sniff(bytes: &[u8]) -> Option<&'static str>` | Sniff a MIME type from an in-memory byte header. `None` if unrecognized. |

### `FsError`

```rust
pub enum FsError {
    Io(std::io::Error),   // underlying I/O error (From<std::io::Error>)
    Undetermined(String), // signature not recognized for the given path
}
```

Implements `std::error::Error`. `Io` is `#[from]`, so `?` on a filesystem call
converts automatically.

## Detection by filename extension

`lookup_content_type` takes the substring after the last `.` (requiring an actual
extension — a name with no dot yields `None`), lowercases it, and delegates to
`mime_for_ext`. Because it only reads the last extension, a compound name like
`archive.tar.gz` resolves via `gz` to `application/gzip`.

`mime_for_ext` is the underlying table (call it directly when you already have a
bare, lowercase extension). Recognized extensions include:

- **Text / data:** `txt`/`text`/`log` → `text/plain`, `html`/`htm`, `css`,
  `js`/`mjs` → `application/javascript`, `json`, `xml` → `text/xml`,
  `yaml`/`yml` → `application/yaml`, `toml`, `csv`, `md`/`markdown` → `text/markdown`.
- **Images:** `png`, `jpg`/`jpeg`, `gif`, `webp`, `svg` → `image/svg+xml`,
  `ico` → `image/x-icon`, `bmp`.
- **Audio / video:** `mp3` → `audio/mpeg`, `wav`, `ogg`, `mp4`, `webm`.
- **Archives:** `zip`, `gz`/`gzip` → `application/gzip`, `tar` → `application/x-tar`,
  `wasm`.
- **Fonts:** `woff`, `woff2`, `ttf`, `otf`.
- **Documents:** `pdf`.

Any other extension returns `None`.

## Detection by content (magic bytes)

`detect_content_type(path)` opens the file, reads up to the first 32 bytes, and calls
`sniff` on them. It returns `FsError::Io` on a read failure and
`FsError::Undetermined(path)` when the signature is not recognized.

`sniff(bytes)` matches leading magic signatures and returns a `&'static str`:

| Bytes / signature | MIME |
|---|---|
| `89 50 4E 47 0D 0A 1A 0A` | `image/png` |
| `FF D8 FF` | `image/jpeg` |
| `GIF87a` / `GIF89a` | `image/gif` |
| `BM` | `image/bmp` |
| `RIFF`…`WEBP` (bytes 0–3 and 8–11) | `image/webp` |
| `%PDF` | `application/pdf` |
| `PK\x03\x04` / `PK\x05\x06` / `PK\x07\x08` | `application/zip` |
| `1F 8B` | `application/gzip` |
| `\0asm` | `application/wasm` |
| `OggS` | `audio/ogg` |
| `ID3` / `FF FB` | `audio/mpeg` |
| `00 00 01 00` | `image/x-icon` |
| `<?xml` | `text/xml` |

Anything else yields `None` (and hence `FsError::Undetermined` from
`detect_content_type`).

```rust
use ferroly::fsutils::sniff;

assert_eq!(sniff(b"\x89PNG\r\n\x1a\n....."), Some("image/png"));
assert_eq!(sniff(b"%PDF-1.7"),              Some("application/pdf"));
assert_eq!(sniff(b"not a known header"),    None);
```

## Extension lookup vs. content sniffing

- **`lookup_content_type` / `mime_for_ext`** — cheap, no I/O, and cover a broad set
  of text/document/font types. Trusts the filename. Used internally (e.g. by
  scheduler-style file storage) to pick a [codec](codec.md) from a path such as
  `jobs.yaml`.
- **`detect_content_type` / `sniff`** — inspect the actual bytes, so they are robust
  against a wrong or missing extension, but only cover binary formats with a
  distinctive signature (mostly images, archives, media, and XML).

Use extension lookup when you control (and trust) the filename, and content sniffing
when handling uploads or opaque blobs.

## Error handling

Only the disk-reading paths are fallible. `detect_content_type` returns `Io` for a
read error and `Undetermined` for an unrecognized signature; the pure-function
variants (`lookup_content_type`, `mime_for_ext`, `sniff`) never fail and return
`Option` instead.

## Limitations

- **No existence/metadata helpers** — deliberately removed; use `std::path::Path`
  (`exists`, `is_file`, `is_dir`).
- **Sniffing covers signature-bearing binary formats only** — plain text, JSON, YAML,
  CSV, etc. have no magic bytes and return `None` from `sniff`; rely on extension
  lookup for those.
- **First-extension-only, first-32-bytes-only** — `lookup_content_type` reads the
  last extension; `detect_content_type` reads at most 32 leading bytes.
- **Not a filesystem abstraction** — for reading/writing across pluggable backends,
  use [vfs](vfs.md).

## See also

- [vfs](vfs.md) — the virtual filesystem module (distinct from `fsutils`).
- [codec](codec.md) — MIME types from this module map to codecs for parsing.
- [config](config.md) — infers file formats from extensions in a similar spirit.

---
**Related:** [vfs](vfs.md), [codec](codec.md), [config](config.md).

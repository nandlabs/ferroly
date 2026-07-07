//! Filesystem utilities: content-type detection and memory-mapped file access.
//!
//! Identify a file's content type:
//! - [`detect_content_type`] — sniffs a file's MIME type from its magic bytes.
//! - [`lookup_content_type`] — resolves a MIME type from a filename extension.
//!
//! Read a large file with zero copies:
//! - [`Mmap`] — a read-only memory-mapped view (`Deref<Target = [u8]>`), with
//!   pages faulted in lazily by the OS.
//!
//! Existence checks are intentionally omitted: Rust callers use
//! [`Path::exists`](std::path::Path::exists) / [`is_file`](std::path::Path::is_file) /
//! [`is_dir`](std::path::Path::is_dir) directly.
//!
//! ```
//! use ferroly::fsutils::lookup_content_type;
//! assert_eq!(lookup_content_type("config.json"), Some("application/json".to_string()));
//! ```

#![deny(missing_docs)]

use ferroly_derive::FerrolyError;
use std::path::Path;

/// Errors raised by content-type detection that reads from disk.
#[derive(Debug, FerrolyError)]
#[non_exhaustive]
pub enum FsError {
    /// An underlying I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The content type could not be determined from the file's bytes.
    #[error("could not determine content type for: {0}")]
    Undetermined(String),
}

/// Detects a file's MIME type by reading and sniffing its leading magic bytes.
///
/// Returns [`FsError::Undetermined`] if the signature is not recognized.
pub fn detect_content_type(path: impl AsRef<Path>) -> Result<String, FsError> {
    use std::io::Read;
    let path = path.as_ref();
    let mut header = [0u8; 32];
    let n = std::fs::File::open(path)?.read(&mut header)?;
    sniff(&header[..n])
        .map(str::to_string)
        .ok_or_else(|| FsError::Undetermined(path.display().to_string()))
}

/// Sniffs a MIME type from a byte header's magic signature, or `None`.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    let starts = |sig: &[u8]| bytes.len() >= sig.len() && &bytes[..sig.len()] == sig;
    if starts(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if starts(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if starts(b"GIF87a") || starts(b"GIF89a") {
        Some("image/gif")
    } else if starts(b"BM") {
        Some("image/bmp")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if starts(b"%PDF") {
        Some("application/pdf")
    } else if starts(b"PK\x03\x04") || starts(b"PK\x05\x06") || starts(b"PK\x07\x08") {
        Some("application/zip")
    } else if starts(b"\x1f\x8b") {
        Some("application/gzip")
    } else if starts(b"\0asm") {
        Some("application/wasm")
    } else if starts(b"OggS") {
        Some("audio/ogg")
    } else if starts(b"ID3") || starts(b"\xff\xfb") {
        Some("audio/mpeg")
    } else if starts(b"\x00\x00\x01\x00") {
        Some("image/x-icon")
    } else if starts(b"<?xml") {
        Some("text/xml")
    } else {
        None
    }
}

/// Looks up a MIME type from a filename's extension (no disk access).
///
/// Used by the scheduler's file storage to pick a codec from a path like
/// `jobs.yaml`. Returns `None` when the extension is unknown.
pub fn lookup_content_type(filename: &str) -> Option<String> {
    let ext = filename.rsplit('.').next().filter(|e| *e != filename)?;
    let ext = ext.to_ascii_lowercase();
    mime_for_ext(&ext).map(str::to_string)
}

/// Maps a lowercase file extension to a MIME type.
pub fn mime_for_ext(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "txt" | "text" | "log" => "text/plain",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "application/javascript",
        "json" => "application/json",
        "xml" => "text/xml",
        "yaml" | "yml" => "application/yaml",
        "toml" => "application/toml",
        "csv" => "text/csv",
        "md" | "markdown" => "text/markdown",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "zip" => "application/zip",
        "gz" | "gzip" => "application/gzip",
        "tar" => "application/x-tar",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        _ => return None,
    })
}

// ---- memory-mapped files -------------------------------------------------

#[cfg(unix)]
pub use unix_mmap::Mmap;

#[cfg(not(unix))]
pub use fallback_mmap::Mmap;

/// The read-only memory-map, backed by the OS on Unix. This is the crate's one
/// audited `unsafe` region: it wraps the POSIX `mmap`/`munmap` FFI. The mapping
/// is `PROT_READ | MAP_PRIVATE`, so the bytes are immutable for the lifetime of
/// the [`Mmap`] and it is sound to share across threads.
#[cfg(unix)]
#[allow(unsafe_code)]
mod unix_mmap {
    use super::FsError;
    use core::ffi::c_void;
    use std::os::fd::AsRawFd;
    use std::path::Path;

    mod sys {
        use core::ffi::c_void;
        extern "C" {
            pub fn mmap(
                addr: *mut c_void,
                len: usize,
                prot: i32,
                flags: i32,
                fd: i32,
                offset: i64,
            ) -> *mut c_void;
            pub fn munmap(addr: *mut c_void, len: usize) -> i32;
        }
    }

    const PROT_READ: i32 = 0x1;
    const MAP_PRIVATE: i32 = 0x2;
    // `mmap` returns `(void *) -1` on failure.
    const MAP_FAILED: usize = usize::MAX;

    /// A read-only memory-mapped view of a file.
    ///
    /// Dereferences to `&[u8]` covering the whole file; pages are faulted in by
    /// the OS on first access. The mapping is unmapped on drop.
    pub struct Mmap {
        ptr: *const u8,
        len: usize,
    }

    impl Mmap {
        /// Maps `path` read-only. An empty file maps to an empty slice.
        pub fn open(path: impl AsRef<Path>) -> Result<Mmap, FsError> {
            let file = std::fs::File::open(path)?;
            let len = file.metadata()?.len() as usize;
            if len == 0 {
                return Ok(Mmap {
                    ptr: std::ptr::NonNull::<u8>::dangling().as_ptr(),
                    len: 0,
                });
            }
            // SAFETY: `fd` is a valid, open file descriptor for the duration of
            // the call; a null `addr` lets the kernel choose the address; `len`
            // is the file size. The returned pointer is checked against
            // `MAP_FAILED` before use, and the `File` may be dropped after
            // `mmap` returns (the mapping keeps its own reference).
            let ptr = unsafe {
                sys::mmap(
                    std::ptr::null_mut(),
                    len,
                    PROT_READ,
                    MAP_PRIVATE,
                    file.as_raw_fd(),
                    0,
                )
            };
            if ptr as usize == MAP_FAILED {
                return Err(FsError::Io(std::io::Error::last_os_error()));
            }
            Ok(Mmap {
                ptr: ptr as *const u8,
                len,
            })
        }

        /// The mapped bytes.
        pub fn as_bytes(&self) -> &[u8] {
            self
        }

        /// The length of the mapping in bytes.
        pub fn len(&self) -> usize {
            self.len
        }

        /// Whether the mapping is empty.
        pub fn is_empty(&self) -> bool {
            self.len == 0
        }
    }

    impl std::ops::Deref for Mmap {
        type Target = [u8];
        fn deref(&self) -> &[u8] {
            if self.len == 0 {
                return &[];
            }
            // SAFETY: `ptr` points at `len` initialized, read-only bytes for the
            // lifetime of `self` (the mapping is only released in `Drop`).
            unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
        }
    }

    impl AsRef<[u8]> for Mmap {
        fn as_ref(&self) -> &[u8] {
            self
        }
    }

    impl Drop for Mmap {
        fn drop(&mut self) {
            if self.len != 0 {
                // SAFETY: `ptr`/`len` are exactly what a successful `mmap`
                // returned and have not been unmapped before.
                unsafe {
                    sys::munmap(self.ptr as *mut c_void, self.len);
                }
            }
        }
    }

    // SAFETY: the mapping is read-only and immutable for the lifetime of the
    // `Mmap`, so a shared `&[u8]` view is sound to move and share across threads.
    unsafe impl Send for Mmap {}
    unsafe impl Sync for Mmap {}
}

/// Non-Unix fallback: reads the whole file into memory and exposes it through
/// the same [`Mmap`](fallback_mmap::Mmap) API. Not a true OS mapping (no lazy
/// paging), but keeps the interface portable.
#[cfg(not(unix))]
mod fallback_mmap {
    use super::FsError;
    use std::path::Path;

    /// A read-only view of a file's bytes (in-memory fallback on non-Unix).
    pub struct Mmap {
        data: Vec<u8>,
    }

    impl Mmap {
        /// Reads `path` fully into memory.
        pub fn open(path: impl AsRef<Path>) -> Result<Mmap, FsError> {
            Ok(Mmap {
                data: std::fs::read(path)?,
            })
        }

        /// The file bytes.
        pub fn as_bytes(&self) -> &[u8] {
            &self.data
        }

        /// The length in bytes.
        pub fn len(&self) -> usize {
            self.data.len()
        }

        /// Whether it is empty.
        pub fn is_empty(&self) -> bool {
            self.data.is_empty()
        }
    }

    impl std::ops::Deref for Mmap {
        type Target = [u8];
        fn deref(&self) -> &[u8] {
            &self.data
        }
    }

    impl AsRef<[u8]> for Mmap {
        fn as_ref(&self) -> &[u8] {
            &self.data
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_up_content_types() {
        assert_eq!(
            lookup_content_type("config.json"),
            Some("application/json".to_string())
        );
        assert_eq!(
            lookup_content_type("data.xml"),
            Some("text/xml".to_string())
        );
        assert_eq!(lookup_content_type("no_extension"), None);
        assert_eq!(
            lookup_content_type("archive.tar.gz"),
            Some("application/gzip".to_string())
        );
    }

    #[test]
    fn sniffs_magic_bytes() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n....."), Some("image/png"));
        assert_eq!(sniff(b"%PDF-1.7"), Some("application/pdf"));
        assert_eq!(sniff(b"\xff\xd8\xff\xe0"), Some("image/jpeg"));
        assert_eq!(sniff(b"GIF89a"), Some("image/gif"));
        assert_eq!(sniff(b"not a known header"), None);
    }

    #[test]
    fn detects_from_file() {
        let path = std::env::temp_dir().join("ferroly-fsutils-test.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR").unwrap();
        assert_eq!(detect_content_type(&path).unwrap(), "image/png");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mmap_reads_file_contents() {
        let path = std::env::temp_dir().join("ferroly-mmap-test.bin");
        let data: Vec<u8> = (0u8..=255).cycle().take(10_000).collect();
        std::fs::write(&path, &data).unwrap();

        let map = Mmap::open(&path).unwrap();
        assert_eq!(map.len(), data.len());
        assert!(!map.is_empty());
        assert_eq!(&map[..], &data[..]); // Deref<Target = [u8]>
        assert_eq!(map.as_bytes().iter().map(|&b| b as u64).sum::<u64>(), {
            data.iter().map(|&b| b as u64).sum::<u64>()
        });
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mmap_empty_file_is_empty_slice() {
        let path = std::env::temp_dir().join("ferroly-mmap-empty.bin");
        std::fs::write(&path, b"").unwrap();
        let map = Mmap::open(&path).unwrap();
        assert!(map.is_empty());
        assert_eq!(&map[..], b"");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mmap_missing_file_errors() {
        let path = std::env::temp_dir().join("ferroly-mmap-does-not-exist.bin");
        let _ = std::fs::remove_file(&path);
        assert!(Mmap::open(&path).is_err());
    }
}

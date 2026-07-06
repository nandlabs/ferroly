//! Content-type detection.
//!
//! Two ways to identify a file's content type:
//! - [`detect_content_type`] — sniffs a file's MIME type from its magic bytes.
//! - [`lookup_content_type`] — resolves a MIME type from a filename extension.
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
}

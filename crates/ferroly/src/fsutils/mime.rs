//! Content-type (MIME) detection: an extension table and a magic-byte sniffer.
//!
//! Both are **hints**. Neither proves a file is well-formed or safe: a matching
//! signature only says the leading bytes look like a format, and an extension
//! is whatever the uploader typed. Never use the result to validate untrusted
//! input.

use std::path::Path;

use super::FsError;

/// How many leading bytes [`detect_content_type`] reads.
///
/// 512 bytes is enough to reach the `ustar` marker of a tar header (offset 257),
/// the furthest signature [`sniff`] looks at.
pub const SNIFF_LEN: usize = 512;

/// Detects a file's MIME type by reading and sniffing its leading bytes.
///
/// Reads up to [`SNIFF_LEN`] bytes and passes them to [`sniff`]. Returns
/// [`FsError::Undetermined`] if nothing is recognized (including for an empty
/// file). The result is a hint, not validation — see the [module](super) docs.
pub fn detect_content_type(path: impl AsRef<Path>) -> Result<String, FsError> {
    use std::io::Read;
    let path = path.as_ref();
    let mut file = std::fs::File::open(path)?;
    let mut header = [0u8; SNIFF_LEN];
    let mut filled = 0;
    // A single `read` may return fewer bytes than asked for; keep going until
    // the buffer is full or the file ends.
    while filled < header.len() {
        match file.read(&mut header[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(FsError::Io(e)),
        }
    }
    sniff(&header[..filled])
        .map(str::to_string)
        .ok_or_else(|| FsError::Undetermined(path.display().to_string()))
}

/// True if `bytes` holds `sig` at byte offset `at` (never indexes out of bounds).
fn has(bytes: &[u8], at: usize, sig: &[u8]) -> bool {
    at.checked_add(sig.len())
        .and_then(|end| bytes.get(at..end))
        .is_some_and(|window| window == sig)
}

/// Sniffs a MIME type from the leading bytes of some content, or `None`.
///
/// Binary formats are matched by magic signature; if none matches, a few text
/// checks run (byte-order marks, HTML / XML / SVG / JSON prefixes, and finally a
/// "looks like UTF-8 text" heuristic that yields `text/plain`). Pass as much of
/// the header as you have — up to [`SNIFF_LEN`] bytes is useful; shorter input
/// is fine and never panics.
///
/// The result is a **hint**: it says what the first bytes resemble, not that
/// the content is a valid or safe instance of that type.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.is_empty() {
        return None;
    }
    sniff_binary(bytes).or_else(|| sniff_text(bytes))
}

/// Magic-signature matching for binary formats.
fn sniff_binary(b: &[u8]) -> Option<&'static str> {
    let starts = |sig: &[u8]| has(b, 0, sig);

    // ---- images ----
    if starts(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if starts(b"\xff\xd8\xff") {
        return Some("image/jpeg");
    }
    if starts(b"GIF87a") || starts(b"GIF89a") {
        return Some("image/gif");
    }
    if starts(b"II*\0") || starts(b"MM\0*") {
        return Some("image/tiff");
    }
    if starts(b"8BPS") {
        return Some("image/vnd.adobe.photoshop");
    }
    if starts(b"\xff\x0a") || starts(b"\0\0\0\x0cJXL \r\n\x87\n") {
        return Some("image/jxl");
    }
    if starts(b"\x00\x00\x01\x00") {
        return Some("image/x-icon");
    }

    // ---- RIFF / IFF containers (form type at bytes 8..12) ----
    if starts(b"RIFF") {
        if has(b, 8, b"WEBP") {
            return Some("image/webp");
        }
        if has(b, 8, b"WAVE") {
            return Some("audio/wav");
        }
        if has(b, 8, b"AVI ") {
            return Some("video/x-msvideo");
        }
    }
    if starts(b"FORM") && (has(b, 8, b"AIFF") || has(b, 8, b"AIFC")) {
        return Some("audio/aiff");
    }

    // ---- ISO base media (`ftyp` box at bytes 4..8, major brand at 8..12) ----
    if has(b, 4, b"ftyp") {
        if let Some(brand) = b.get(8..12) {
            return Some(ftyp_brand(brand));
        }
        return Some("video/mp4");
    }

    // ---- audio / video ----
    if starts(b"OggS") {
        return Some("audio/ogg");
    }
    if starts(b"fLaC") {
        return Some("audio/flac");
    }
    if starts(b"MThd") {
        return Some("audio/midi");
    }
    if starts(b"ID3")
        || starts(b"\xff\xfb")
        || starts(b"\xff\xfa")
        || starts(b"\xff\xf3")
        || starts(b"\xff\xf2")
    {
        return Some("audio/mpeg");
    }
    if starts(b"\x1a\x45\xdf\xa3") {
        // EBML: WebM and Matroska share the header; the doctype string follows.
        let window = &b[..b.len().min(64)];
        return Some(if contains(window, b"webm") {
            "video/webm"
        } else {
            "video/x-matroska"
        });
    }

    // ---- documents ----
    if starts(b"%PDF-") {
        return Some("application/pdf");
    }
    if starts(b"%!PS") {
        return Some("application/postscript");
    }
    if starts(b"{\\rtf") {
        return Some("application/rtf");
    }
    if starts(b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1") {
        // Legacy Office (.doc/.xls/.ppt) and .msi all share this container.
        return Some("application/x-ole-storage");
    }

    // ---- archives / compression ----
    if starts(b"PK\x03\x04") || starts(b"PK\x05\x06") || starts(b"PK\x07\x08") {
        return Some(zip_flavour(b));
    }
    if starts(b"\x1f\x8b") {
        return Some("application/gzip");
    }
    if starts(b"\x28\xb5\x2f\xfd") {
        return Some("application/zstd");
    }
    if starts(b"\xfd7zXZ\0") {
        return Some("application/x-xz");
    }
    if starts(b"BZh") && b.get(3).is_some_and(|d| (b'1'..=b'9').contains(d)) {
        return Some("application/x-bzip2");
    }
    if starts(b"7z\xbc\xaf\x27\x1c") {
        return Some("application/x-7z-compressed");
    }
    if starts(b"Rar!\x1a\x07") {
        return Some("application/vnd.rar");
    }
    if starts(b"\x04\x22\x4d\x18") {
        return Some("application/x-lz4");
    }
    if has(b, 257, b"ustar") {
        return Some("application/x-tar");
    }

    // ---- code / data ----
    if starts(b"\0asm") {
        return Some("application/wasm");
    }
    if starts(b"\x7fELF") {
        return Some("application/x-elf");
    }
    if starts(b"SQLite format 3\0") {
        return Some("application/vnd.sqlite3");
    }
    if starts(b"PAR1") {
        return Some("application/vnd.apache.parquet");
    }
    if starts(b"ARROW1") {
        return Some("application/vnd.apache.arrow.file");
    }
    if starts(b"Obj\x01") {
        return Some("application/avro");
    }

    // ---- fonts ----
    if starts(b"wOFF") {
        return Some("font/woff");
    }
    if starts(b"wOF2") {
        return Some("font/woff2");
    }
    if starts(b"OTTO") {
        return Some("font/otf");
    }
    if starts(b"ttcf") {
        return Some("font/collection");
    }
    if starts(b"\x00\x01\x00\x00\x00") {
        return Some("font/ttf");
    }

    // `BM` is only two bytes, so it is checked last and needs a plausible
    // header behind it (the reserved field at bytes 6..10 is zero).
    if starts(b"BM") && has(b, 6, &[0, 0, 0, 0]) {
        return Some("image/bmp");
    }
    None
}

/// Maps an ISO base media major brand to a MIME type.
fn ftyp_brand(brand: &[u8]) -> &'static str {
    match brand {
        b"avif" | b"avis" => "image/avif",
        b"heic" | b"heix" | b"heim" | b"heis" | b"hevc" | b"hevx" | b"hevm" | b"hevs" => {
            "image/heic"
        }
        b"mif1" | b"msf1" => "image/heif",
        b"qt  " => "video/quicktime",
        b"M4A " | b"M4B " | b"M4P " => "audio/mp4",
        b"M4V " => "video/x-m4v",
        b"crx " => "image/x-canon-cr3",
        _ if brand.starts_with(b"3g2") => "video/3gpp2",
        _ if brand.starts_with(b"3gp") => "video/3gpp",
        _ => "video/mp4",
    }
}

/// Refines a ZIP container when its first entry is the uncompressed `mimetype`
/// marker that EPUB and OpenDocument files start with. Office Open XML
/// (`.docx`/`.xlsx`/`.pptx`) cannot be told apart from a leading header alone,
/// so those — like every other archive — report `application/zip`.
fn zip_flavour(b: &[u8]) -> &'static str {
    const KNOWN: &[&str] = &[
        "application/epub+zip",
        "application/vnd.oasis.opendocument.text",
        "application/vnd.oasis.opendocument.spreadsheet",
        "application/vnd.oasis.opendocument.presentation",
        "application/vnd.oasis.opendocument.graphics",
    ];
    // Local file header: name length at 26..28, extra length at 28..30, name at 30.
    if has(b, 26, &[8, 0, 0, 0]) && has(b, 30, b"mimetype") {
        // The stored value follows the name directly (no extra field).
        for known in KNOWN {
            if has(b, 38, known.as_bytes()) {
                return known;
            }
        }
    }
    "application/zip"
}

/// True if `needle` occurs anywhere in `haystack`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// True if `bytes` begins with `prefix`, ignoring ASCII case.
fn starts_ignore_case(bytes: &[u8], prefix: &[u8]) -> bool {
    bytes
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

/// Text checks: byte-order marks, markup / JSON prefixes, then a plain-text
/// heuristic.
fn sniff_text(bytes: &[u8]) -> Option<&'static str> {
    // UTF-16 byte-order marks: text, but not something the prefix checks below
    // can read.
    if has(bytes, 0, &[0xff, 0xfe]) || has(bytes, 0, &[0xfe, 0xff]) {
        return Some("text/plain");
    }
    let (had_bom, body) = match bytes.strip_prefix(&[0xef, 0xbb, 0xbf]) {
        Some(rest) => (true, rest),
        None => (false, bytes),
    };
    if !looks_like_text(body) {
        return None;
    }
    let trimmed = trim_ascii_start(body);
    if starts_ignore_case(trimmed, b"<!doctype html") || starts_ignore_case(trimmed, b"<html") {
        return Some("text/html");
    }
    if starts_ignore_case(trimmed, b"<svg") {
        return Some("image/svg+xml");
    }
    if trimmed.starts_with(b"<?xml") {
        return Some(if contains(trimmed, b"<svg") {
            "image/svg+xml"
        } else {
            "text/xml"
        });
    }
    if looks_like_json(trimmed) {
        return Some("application/json");
    }
    if trimmed.is_empty() && !had_bom {
        // Only whitespace: too little to call it anything.
        return None;
    }
    Some("text/plain")
}

/// Skips leading ASCII whitespace.
fn trim_ascii_start(bytes: &[u8]) -> &[u8] {
    let skip = bytes
        .iter()
        .take_while(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0c))
        .count();
    &bytes[skip..]
}

/// A cheap structural check: the content opens like a JSON object or array.
fn looks_like_json(trimmed: &[u8]) -> bool {
    let Some((&first, rest)) = trimmed.split_first() else {
        return false;
    };
    let next = trim_ascii_start(rest).first().copied();
    match first {
        b'{' => matches!(next, Some(b'"') | Some(b'}')),
        b'[' => match next {
            Some(b'{') | Some(b'[') | Some(b'"') | Some(b']') | Some(b'-') => true,
            Some(c) if c.is_ascii_digit() => true,
            // `true`, `false`, `null`
            Some(b't') | Some(b'f') | Some(b'n') => {
                let tail = trim_ascii_start(rest);
                tail.starts_with(b"true") || tail.starts_with(b"false") || tail.starts_with(b"null")
            }
            _ => false,
        },
        _ => false,
    }
}

/// True if `bytes` is UTF-8 (allowing a multibyte sequence cut off by the end
/// of the buffer) with no control characters other than common whitespace and
/// escape.
fn looks_like_text(bytes: &[u8]) -> bool {
    let valid = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        // `error_len() == None` means the input simply ended mid-character,
        // which is expected when sniffing a truncated header.
        Err(e) if e.error_len().is_none() => match std::str::from_utf8(&bytes[..e.valid_up_to()]) {
            Ok(s) => s,
            Err(_) => return false,
        },
        Err(_) => return false,
    };
    valid
        .bytes()
        .all(|b| (b >= 0x20 && b != 0x7f) || matches!(b, b'\t' | b'\n' | b'\r' | 0x0c | 0x1b))
}

/// Looks up a MIME type from a filename's extension (no disk access).
///
/// Only the last extension counts (`archive.tar.gz` resolves through `gz`), and
/// the comparison ignores ASCII case. Returns `None` when there is no extension
/// or it is unknown. Directory components are ignored, so a dot in a parent
/// directory name is never mistaken for an extension.
pub fn lookup_content_type(filename: &str) -> Option<String> {
    let name = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
    let (_, ext) = name.rsplit_once('.')?;
    if ext.is_empty() {
        return None;
    }
    let ext = ext.to_ascii_lowercase();
    mime_for_ext(&ext).map(str::to_string)
}

/// Maps a lowercase file extension (no leading dot) to a MIME type.
pub fn mime_for_ext(ext: &str) -> Option<&'static str> {
    Some(match ext {
        // ---- text ----
        "txt" | "text" | "log" | "conf" | "cfg" | "ini" | "env" => "text/plain",
        "html" | "htm" | "xhtml" => "text/html",
        "css" => "text/css",
        "csv" => "text/csv",
        "tsv" | "tab" => "text/tab-separated-values",
        "md" | "markdown" => "text/markdown",
        "rtf" => "application/rtf",
        "ics" | "ifb" => "text/calendar",
        "vcf" | "vcard" => "text/vcard",
        "vtt" => "text/vtt",
        "srt" => "application/x-subrip",

        // ---- code / structured data ----
        "js" | "mjs" | "cjs" => "application/javascript",
        "json" | "map" => "application/json",
        "jsonl" | "ndjson" => "application/x-ndjson",
        "jsonld" => "application/ld+json",
        "geojson" => "application/geo+json",
        "webmanifest" => "application/manifest+json",
        "xml" | "xsd" | "xsl" | "xslt" => "text/xml",
        "rss" => "application/rss+xml",
        "atom" => "application/atom+xml",
        "yaml" | "yml" => "application/yaml",
        "toml" => "application/toml",
        "proto" => "text/x-protobuf",
        "pb" | "protobuf" | "binpb" => "application/x-protobuf",
        "graphql" | "gql" => "application/graphql",
        "sql" => "application/sql",
        "sh" | "bash" => "application/x-sh",
        "wasm" => "application/wasm",
        "wat" => "text/plain",
        "tsx" => "text/tsx",
        "jsx" => "text/jsx",
        "py" => "text/x-python",
        "rs" => "text/x-rust",
        "go" => "text/x-go",
        "java" => "text/x-java",
        "c" | "h" => "text/x-c",
        "cpp" | "cc" | "cxx" | "hpp" => "text/x-c++",

        // ---- data files ----
        "parquet" => "application/vnd.apache.parquet",
        "arrow" | "feather" => "application/vnd.apache.arrow.file",
        "avro" => "application/avro",
        "orc" => "application/x-orc",
        "sqlite" | "sqlite3" => "application/vnd.sqlite3",
        "bin" | "dat" => "application/octet-stream",

        // ---- documents ----
        "pdf" => "application/pdf",
        "ps" | "eps" => "application/postscript",
        "epub" => "application/epub+zip",
        "doc" | "dot" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "dotx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.template",
        "xls" | "xlt" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "xltx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.template",
        "xlsm" => "application/vnd.ms-excel.sheet.macroenabled.12",
        "ppt" | "pps" | "pot" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "ppsx" => "application/vnd.openxmlformats-officedocument.presentationml.slideshow",
        "potx" => "application/vnd.openxmlformats-officedocument.presentationml.template",
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "odp" => "application/vnd.oasis.opendocument.presentation",
        "odg" => "application/vnd.oasis.opendocument.graphics",
        "msg" => "application/vnd.ms-outlook",
        "eml" => "message/rfc822",

        // ---- images ----
        "png" => "image/png",
        "apng" => "image/apng",
        "jpg" | "jpeg" | "jpe" | "jfif" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "jxl" => "image/jxl",
        "svg" | "svgz" => "image/svg+xml",
        "ico" | "cur" => "image/x-icon",
        "bmp" | "dib" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "psd" => "image/vnd.adobe.photoshop",

        // ---- audio ----
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        "m4a" => "audio/mp4",
        "weba" => "audio/webm",
        "mid" | "midi" => "audio/midi",
        "aif" | "aiff" => "audio/aiff",

        // ---- video ----
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "mov" | "qt" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mpeg" | "mpg" => "video/mpeg",
        "ogv" => "video/ogg",
        "3gp" => "video/3gpp",
        "3g2" => "video/3gpp2",
        "m3u8" => "application/vnd.apple.mpegurl",
        "mpd" => "application/dash+xml",

        // ---- archives / compression ----
        "zip" => "application/zip",
        "gz" | "gzip" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        "zst" | "zstd" => "application/zstd",
        "xz" | "txz" => "application/x-xz",
        "bz2" | "tbz2" | "tbz" => "application/x-bzip2",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/vnd.rar",
        "lz4" => "application/x-lz4",
        "br" => "application/x-brotli",
        "jar" | "war" | "ear" => "application/java-archive",
        "apk" => "application/vnd.android.package-archive",
        "deb" => "application/vnd.debian.binary-package",
        "rpm" => "application/x-rpm",
        "dmg" => "application/x-apple-diskimage",
        "iso" => "application/x-iso9660-image",

        // ---- fonts ----
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "ttc" => "font/collection",
        "eot" => "application/vnd.ms-fontobject",

        // ---- security ----
        "pem" | "crt" | "cer" => "application/x-pem-file",
        "p12" | "pfx" => "application/x-pkcs12",

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
    fn lookup_handles_case_dirs_and_edge_names() {
        assert_eq!(
            lookup_content_type("REPORT.PDF"),
            Some("application/pdf".to_string())
        );
        // A dot in a directory name is not an extension.
        assert_eq!(lookup_content_type("conf.d/settings"), None);
        assert_eq!(lookup_content_type("conf.json/settings"), None);
        assert_eq!(
            lookup_content_type("C:\\data.d\\rows.jsonl"),
            Some("application/x-ndjson".to_string())
        );
        assert_eq!(lookup_content_type("trailing."), None);
        assert_eq!(lookup_content_type(""), None);
        assert_eq!(
            lookup_content_type(".toml"),
            Some("application/toml".into())
        );
    }

    #[test]
    fn extension_table_covers_broadened_formats() {
        let cases = [
            (
                "docx",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ),
            (
                "xlsx",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            ),
            (
                "pptx",
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            ),
            ("avif", "image/avif"),
            ("heic", "image/heic"),
            ("jsonl", "application/x-ndjson"),
            ("ndjson", "application/x-ndjson"),
            ("wasm", "application/wasm"),
            ("parquet", "application/vnd.apache.parquet"),
            ("proto", "text/x-protobuf"),
            ("pb", "application/x-protobuf"),
            ("yaml", "application/yaml"),
            ("toml", "application/toml"),
            ("woff2", "font/woff2"),
            ("ttc", "font/collection"),
            ("zst", "application/zstd"),
            ("xz", "application/x-xz"),
            ("bz2", "application/x-bzip2"),
            ("7z", "application/x-7z-compressed"),
            ("ics", "text/calendar"),
        ];
        for (ext, mime) in cases {
            assert_eq!(mime_for_ext(ext), Some(mime), "extension {ext}");
        }
        assert_eq!(mime_for_ext("nope"), None);
    }

    #[test]
    fn sniffs_magic_bytes() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n....."), Some("image/png"));
        assert_eq!(sniff(b"%PDF-1.7"), Some("application/pdf"));
        assert_eq!(sniff(b"\xff\xd8\xff\xe0"), Some("image/jpeg"));
        assert_eq!(sniff(b"GIF89a"), Some("image/gif"));
        // Unrecognized binary stays unknown.
        assert_eq!(sniff(b"\x00\x9c\x01\x02\xfe\xfd"), None);
        assert_eq!(sniff(b""), None);
    }

    fn ftyp(brand: &[u8; 4]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 0x20];
        v.extend_from_slice(b"ftyp");
        v.extend_from_slice(brand);
        v.extend_from_slice(&[0, 0, 0, 0]);
        v
    }

    #[test]
    fn sniffs_ftyp_family() {
        assert_eq!(sniff(&ftyp(b"isom")), Some("video/mp4"));
        assert_eq!(sniff(&ftyp(b"mp42")), Some("video/mp4"));
        assert_eq!(sniff(&ftyp(b"avif")), Some("image/avif"));
        assert_eq!(sniff(&ftyp(b"heic")), Some("image/heic"));
        assert_eq!(sniff(&ftyp(b"mif1")), Some("image/heif"));
        assert_eq!(sniff(&ftyp(b"qt  ")), Some("video/quicktime"));
        assert_eq!(sniff(&ftyp(b"M4A ")), Some("audio/mp4"));
        assert_eq!(sniff(&ftyp(b"3gp5")), Some("video/3gpp"));
        // `ftyp` with the brand cut off still reads as the container.
        assert_eq!(sniff(b"\0\0\0\x20ftyp"), Some("video/mp4"));
    }

    #[test]
    fn sniffs_riff_variants() {
        assert_eq!(sniff(b"RIFF\x10\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"RIFF\x10\0\0\0WAVEfmt "), Some("audio/wav"));
        assert_eq!(sniff(b"RIFF\x10\0\0\0AVI LIST"), Some("video/x-msvideo"));
        // RIFF with an unknown form type, or cut short, is not guessed at.
        assert_eq!(sniff(b"RIFF\x10\0\0\0\x01\x02\x03\x04"), None);
        assert_eq!(sniff(b"RIFF\x10\0"), None);
    }

    #[test]
    fn sniffs_archives_and_compression() {
        let mut tar = vec![0u8; 512];
        tar[..8].copy_from_slice(b"file.txt");
        tar[257..262].copy_from_slice(b"ustar");
        assert_eq!(sniff(&tar), Some("application/x-tar"));
        // The same header truncated before offset 257 is not a tar.
        assert_ne!(sniff(&tar[..200]), Some("application/x-tar"));

        assert_eq!(sniff(b"\x28\xb5\x2f\xfd\x04\x00"), Some("application/zstd"));
        assert_eq!(sniff(b"\xfd7zXZ\0\0\x04"), Some("application/x-xz"));
        assert_eq!(sniff(b"BZh91AY&SY"), Some("application/x-bzip2"));
        assert_eq!(
            sniff(b"7z\xbc\xaf\x27\x1c\0\x04"),
            Some("application/x-7z-compressed")
        );
        assert_eq!(sniff(b"\x1f\x8b\x08\0"), Some("application/gzip"));
        assert_eq!(sniff(b"PK\x03\x04\x14\0"), Some("application/zip"));
        assert_eq!(sniff(b"\0asm\x01\0\0\0"), Some("application/wasm"));
        // "BZh" followed by a non-digit is plain text, not bzip2.
        assert_eq!(sniff(b"BZhello there"), Some("text/plain"));
    }

    #[test]
    fn sniffs_zip_mimetype_marker() {
        let mut epub = vec![0u8; 30];
        epub[..4].copy_from_slice(b"PK\x03\x04");
        epub[26] = 8; // file name length
        epub.extend_from_slice(b"mimetype");
        epub.extend_from_slice(b"application/epub+zip");
        assert_eq!(sniff(&epub), Some("application/epub+zip"));

        let mut odt = epub[..38].to_vec();
        odt.extend_from_slice(b"application/vnd.oasis.opendocument.textPK");
        assert_eq!(sniff(&odt), Some("application/vnd.oasis.opendocument.text"));

        // Marker present but the value is cut off: still a zip.
        assert_eq!(sniff(&epub[..45]), Some("application/zip"));
    }

    #[test]
    fn sniffs_text_formats() {
        assert_eq!(sniff(b"\xef\xbb\xbfhello"), Some("text/plain"));
        assert_eq!(sniff(b"\xef\xbb\xbf"), Some("text/plain"));
        assert_eq!(sniff(b"\xff\xfeh\0i\0"), Some("text/plain"));
        assert_eq!(sniff(b"just some words\n"), Some("text/plain"));
        assert_eq!(sniff(b"  \n<!DOCTYPE HTML><html>"), Some("text/html"));
        assert_eq!(sniff(b"<html lang=\"en\">"), Some("text/html"));
        assert_eq!(sniff(b"<?xml version=\"1.0\"?><a/>"), Some("text/xml"));
        assert_eq!(
            sniff(b"<?xml version=\"1.0\"?><svg xmlns=\"x\"/>"),
            Some("image/svg+xml")
        );
        assert_eq!(sniff(b"<svg viewBox=\"0 0 1 1\"/>"), Some("image/svg+xml"));
        assert_eq!(sniff(b"\xef\xbb\xbf{\"a\": 1}"), Some("application/json"));
        assert_eq!(sniff(b" [1, 2, 3]"), Some("application/json"));
        assert_eq!(sniff(b"[true]"), Some("application/json"));
        assert_eq!(sniff(b"{}"), Some("application/json"));
        // Looks bracketed but is not JSON-shaped.
        assert_eq!(sniff(b"[section]\nkey = 1\n"), Some("text/plain"));
        assert_eq!(sniff(b"{not json"), Some("text/plain"));
        // Whitespace only is too little to classify.
        assert_eq!(sniff(b"   \n"), None);
    }

    #[test]
    fn text_heuristic_tolerates_truncated_utf8_and_rejects_binary() {
        // "é" is C3 A9; cut after the lead byte.
        assert_eq!(sniff(b"caf\xc3"), Some("text/plain"));
        // An invalid sequence in the middle is not text.
        assert_eq!(sniff(b"caf\xc3(more"), None);
        // NUL and other control bytes are not text.
        assert_eq!(sniff(b"abc\0def"), None);
        assert_eq!(sniff(b"abc\x01def"), None);
    }

    #[test]
    fn sniff_never_panics_on_short_or_arbitrary_input() {
        // Every prefix of a few tricky headers, plus every single byte.
        let mut tar = vec![b'a'; 300];
        tar[257..262].copy_from_slice(b"ustar");
        let samples: [&[u8]; 6] = [
            b"RIFF\x10\0\0\0WEBP",
            b"\0\0\0\x20ftypavif",
            b"PK\x03\x04aaaaaaaaaaaaaaaaaaaaaa\x08\0\0\0mimetypeapplication/epub+zip",
            b"\x1a\x45\xdf\xa3\x01\0\0\0\0\0\0\x1f\x42\x86\x81\x01webm",
            b"BZh",
            &tar,
        ];
        for sample in samples {
            for end in 0..=sample.len() {
                let _ = sniff(&sample[..end]);
            }
        }
        for byte in 0..=255u8 {
            let _ = sniff(&[byte]);
            let _ = sniff(&[byte, byte]);
        }
    }

    #[test]
    fn detects_from_file_beyond_the_first_bytes() {
        let dir = crate::fsutils::TempDir::new("ferroly-mime").unwrap();
        let png = dir.path().join("image.bin");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR").unwrap();
        assert_eq!(detect_content_type(&png).unwrap(), "image/png");

        // A tar's marker sits at offset 257 — past the old 32-byte window.
        let mut tar = vec![0u8; 1024];
        tar[..5].copy_from_slice(b"a.txt");
        tar[257..262].copy_from_slice(b"ustar");
        let tar_path = dir.path().join("archive.bin");
        std::fs::write(&tar_path, &tar).unwrap();
        assert_eq!(detect_content_type(&tar_path).unwrap(), "application/x-tar");

        let empty = dir.path().join("empty.bin");
        std::fs::write(&empty, b"").unwrap();
        assert!(matches!(
            detect_content_type(&empty),
            Err(FsError::Undetermined(_))
        ));
        assert!(matches!(
            detect_content_type(dir.path().join("missing.bin")),
            Err(FsError::Io(_))
        ));
    }
}

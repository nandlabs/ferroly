//! Streaming, resumable file downloads over the HTTP client.
//!
//! [`download_to_file`] streams a response body straight to disk (never
//! buffering the whole payload) and **resumes** an interrupted transfer: if the
//! destination already holds a partial file, it issues a `Range` request from
//! that offset and appends. Pair it with [`ferroly::hash`](crate::hash) to
//! verify a download as it streams.
//!
//! ```no_run
//! use ferroly::http::{Client, download_to_file};
//!
//! # async fn demo() -> Result<(), ferroly::http::HttpError> {
//! let client = Client::new();
//! // Run once; run again after an interruption and it continues where it stopped.
//! let total = download_to_file(&client, "https://example.com/big.iso", "big.iso").await?;
//! println!("{total} bytes on disk");
//! # Ok(())
//! # }
//! ```

use std::path::Path;

use tokio::io::AsyncWriteExt;

use super::core::parse_decimal;
use super::{Client, HttpError, Method, Request, StatusCode};

/// Downloads `url` into `path`, resuming from a partial file if one exists.
///
/// Behavior by server response:
/// - **206 Partial Content** — the `Range` request was honored. The
///   `Content-Range` header must be present and start at exactly the offset
///   that was requested; the body is then appended to the existing file. A
///   missing, malformed or mismatched `Content-Range` is an error and the
///   file is left untouched.
/// - **416 Range Not Satisfiable** — the offset is at or past the end; the file
///   is treated as already complete and its current size is returned. If the
///   server reports the full size (`Content-Range: bytes */N`) and it differs
///   from the size on disk, that is an error instead.
/// - **200 OK** (or any other 2xx) — the server ignored the range (or there was
///   nothing to resume); the file is truncated and (re)written from the start.
/// - any non-2xx, non-416 status — returns [`HttpError::Protocol`].
///
/// A body that ends early (a connection closed before `Content-Length` bytes
/// or before the final chunk) is an error — the bytes received so far stay on
/// disk, so calling the function again resumes from there. When the server
/// states the total size, the size on disk is checked against it at the end.
///
/// The client's response-body size cap does not apply: the body goes to disk,
/// not to memory. Its timeouts do.
///
/// Returns the total number of bytes on disk after the transfer.
pub async fn download_to_file(
    client: &Client,
    url: &str,
    path: impl AsRef<Path>,
) -> Result<u64, HttpError> {
    let path = path.as_ref();
    let existing = tokio::fs::metadata(path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    // A byte range only means something for the unencoded representation, so
    // never ask for a compressed one (even on a decompressing client).
    let mut builder = Request::builder(Method::Get, url)?.header("accept-encoding", "identity");
    if existing > 0 {
        // Open-ended range: resume from the byte after what we already have.
        builder = builder.range(existing, None);
    }
    let mut resp = client.send(builder.build()).await?;
    resp.set_max_body_bytes(None);
    let status = resp.status().as_u16();

    // `expected` is the total size the server promised, when it said.
    let (mut file, mut total, expected) = if status == StatusCode::PARTIAL_CONTENT.as_u16() {
        let range = resp
            .headers()
            .get("content-range")
            .and_then(parse_content_range)
            .ok_or_else(|| {
                HttpError::Protocol("206 response without a valid Content-Range".into())
            })?;
        if range.start != Some(existing) {
            return Err(HttpError::Protocol(format!(
                "Content-Range does not start at the requested offset {existing}"
            )));
        }
        let f = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        (f, existing, range.total)
    } else if status == StatusCode::RANGE_NOT_SATISFIABLE.as_u16() {
        let remote = resp
            .headers()
            .get("content-range")
            .and_then(parse_content_range)
            .and_then(|r| r.total);
        return match remote {
            Some(size) if size != existing => Err(HttpError::Protocol(format!(
                "local file has {existing} bytes but the remote resource has {size}"
            ))),
            // Already have the whole thing.
            _ => Ok(existing),
        };
    } else if (200..300).contains(&status) {
        // Server served the full body — start the file over.
        let f = tokio::fs::File::create(path).await?;
        (f, 0, resp.headers().content_length())
    } else {
        return Err(HttpError::Protocol(format!(
            "unexpected status {status} downloading {url}"
        )));
    };

    let outcome = loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                file.write_all(&chunk).await?;
                total += chunk.len() as u64;
            }
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        }
    };
    // Keep whatever arrived, even on failure, so a retry can resume.
    file.flush().await?;
    outcome?;
    if let Some(expected) = expected {
        if total != expected {
            return Err(HttpError::Protocol(format!(
                "download incomplete: {total} of {expected} bytes"
            )));
        }
    }
    Ok(total)
}

/// A parsed `Content-Range: bytes …` header.
struct ContentRange {
    /// First byte position, or `None` for the unsatisfied form `bytes */N`.
    start: Option<u64>,
    /// Complete length, or `None` when given as `*`.
    total: Option<u64>,
}

/// Parses `bytes start-end/total`, `bytes start-end/*` and `bytes */total`.
fn parse_content_range(value: &str) -> Option<ContentRange> {
    let rest = value.trim();
    let unit = rest.get(..5)?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let rest = rest[5..].trim_start();
    let (range, total) = rest.split_once('/')?;
    let total = match total.trim() {
        "*" => None,
        n => Some(parse_decimal(n)?),
    };
    let range = range.trim();
    if range == "*" {
        return Some(ContentRange { start: None, total });
    }
    let (start, end) = range.split_once('-')?;
    let start = parse_decimal(start.trim())?;
    let end = parse_decimal(end.trim())?;
    if end < start {
        return None;
    }
    if let Some(total) = total {
        if end >= total {
            return None;
        }
    }
    Some(ContentRange {
        start: Some(start),
        total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_parsing() {
        let r = parse_content_range("bytes 10-19/20").unwrap();
        assert_eq!((r.start, r.total), (Some(10), Some(20)));
        let r = parse_content_range("Bytes 0-0/*").unwrap();
        assert_eq!((r.start, r.total), (Some(0), None));
        let r = parse_content_range("bytes */20").unwrap();
        assert_eq!((r.start, r.total), (None, Some(20)));
        for bad in [
            "",
            "items 0-1/2",
            "bytes 5-4/10",
            "bytes 0-10/10",
            "bytes -1/10",
            "bytes 0-/10",
            "bytes 0-1",
            "bytes +0-1/2",
            "bytes a-b/c",
        ] {
            assert!(parse_content_range(bad).is_none(), "accepted {bad:?}");
        }
    }
}

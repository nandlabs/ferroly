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

use super::{Client, HttpError, Method, Request, StatusCode};

/// Downloads `url` into `path`, resuming from a partial file if one exists.
///
/// Behavior by server response:
/// - **206 Partial Content** — the `Range` request was honored; the body is
///   appended to the existing file.
/// - **416 Range Not Satisfiable** — the offset is at or past the end; the file
///   is treated as already complete and its current size is returned.
/// - **200 OK** (or any other 2xx) — the server ignored the range (or there was
///   nothing to resume); the file is (re)written from the start.
/// - any non-2xx, non-416 status — returns [`HttpError::Protocol`].
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

    let mut builder = Request::builder(Method::Get, url)?;
    if existing > 0 {
        // Open-ended range: resume from the byte after what we already have.
        builder = builder.range(existing, None);
    }
    let mut resp = client.send(builder.build()).await?;
    let status = resp.status().as_u16();

    let (mut file, mut total) = if status == StatusCode::PARTIAL_CONTENT.as_u16() {
        let f = tokio::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .await?;
        (f, existing)
    } else if status == StatusCode::RANGE_NOT_SATISFIABLE.as_u16() {
        // Already have the whole thing.
        return Ok(existing);
    } else if (200..300).contains(&status) {
        // Server served the full body — start the file over.
        let f = tokio::fs::File::create(path).await?;
        (f, 0)
    } else {
        return Err(HttpError::Protocol(format!(
            "unexpected status {status} downloading {url}"
        )));
    };

    while let Some(chunk) = resp.chunk().await? {
        file.write_all(&chunk).await?;
        total += chunk.len() as u64;
    }
    file.flush().await?;
    Ok(total)
}

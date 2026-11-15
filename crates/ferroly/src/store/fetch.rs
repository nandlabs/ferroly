//! Byte sources the cache reads from, and the HTTP glue for ranged fetches.

use std::future::Future;
use std::pin::Pin;

use tokio::io::{AsyncRead, AsyncReadExt};

use ferroly::http::{Client, Request, Response, StatusCode};

use super::StoreError;

/// A boxed, `Send` future (the object-safe form of an `async fn`).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A stream of body bytes, read piece by piece.
///
/// Implemented for [`ferroly::http::Response`]; [`Fetched::from_reader`] and
/// [`Fetched::from_bytes`] adapt other sources.
pub trait ByteStream: Send {
    /// The next piece of the body, or `None` at the end. An empty piece is
    /// allowed and ignored.
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, StoreError>>;
}

impl ByteStream for Response {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, StoreError>> {
        Box::pin(async move { self.chunk().await.map_err(StoreError::from) })
    }
}

struct ReaderStream<R> {
    reader: R,
}

impl<R: AsyncRead + Send + Unpin> ByteStream for ReaderStream<R> {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, StoreError>> {
        Box::pin(async move {
            let mut buf = vec![0u8; 64 * 1024];
            let n = self.reader.read(&mut buf).await?;
            if n == 0 {
                return Ok(None);
            }
            buf.truncate(n);
            Ok(Some(buf))
        })
    }
}

struct BytesStream {
    data: Option<Vec<u8>>,
}

impl ByteStream for BytesStream {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, StoreError>> {
        let next = self.data.take();
        Box::pin(std::future::ready(Ok(next)))
    }
}

/// What a fetch callback hands to the [`Cache`](super::Cache): a byte stream
/// plus where in the content it starts.
///
/// The cache asks the callback for the content **from a byte offset** (`0`
/// for a fresh download, the size of a partial file when resuming). The
/// callback may honour the offset ([`resumed`](Fetched::resumed)) or ignore
/// it and return the whole content ([`new`](Fetched::new)); the cache copes
/// with either.
pub struct Fetched {
    pub(crate) start: u64,
    pub(crate) total: Option<u64>,
    pub(crate) body: Box<dyn ByteStream>,
}

impl std::fmt::Debug for Fetched {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fetched")
            .field("start", &self.start)
            .field("total", &self.total)
            .finish_non_exhaustive()
    }
}

impl Fetched {
    /// The whole content, from byte 0.
    pub fn new(body: impl ByteStream + 'static) -> Fetched {
        Fetched {
            start: 0,
            total: None,
            body: Box::new(body),
        }
    }

    /// The content from byte `start` on (a resumed transfer).
    pub fn resumed(start: u64, body: impl ByteStream + 'static) -> Fetched {
        Fetched {
            start,
            total: None,
            body: Box::new(body),
        }
    }

    /// Declares the total size of the **whole** content (not of this
    /// piece). The cache then rejects a body that runs past it or ends short.
    pub fn with_total(mut self, total: u64) -> Fetched {
        self.total = Some(total);
        self
    }

    /// The whole content, read from an async reader.
    pub fn from_reader(reader: impl AsyncRead + Send + Unpin + 'static) -> Fetched {
        Fetched::new(ReaderStream { reader })
    }

    /// The whole content, already in memory.
    pub fn from_bytes(data: impl Into<Vec<u8>>) -> Fetched {
        let data = data.into();
        let total = data.len() as u64;
        Fetched::new(BytesStream { data: Some(data) }).with_total(total)
    }

    /// The offset this body starts at.
    pub fn start(&self) -> u64 {
        self.start
    }

    /// The declared total size of the content, if known.
    pub fn total(&self) -> Option<u64> {
        self.total
    }

    /// Interprets an HTTP response to a request for the content from byte
    /// `offset` (with a `Range: bytes=<offset>-` header when `offset > 0`).
    ///
    /// | Status | Result |
    /// |---|---|
    /// | `206` | `Content-Range` must be valid; the body starts at its first byte, the total is its complete length |
    /// | other `2xx` | the server sent the whole content (it ignored the range); the total is `Content-Length` |
    /// | `416` when `offset > 0` | nothing more to send: an empty body at `offset`, with the total from `Content-Range: bytes */N` |
    /// | anything else | [`StoreError::Remote`] |
    ///
    /// The response-body cap is lifted (the cache writes to disk and applies
    /// its own size checks).
    pub async fn from_response(mut resp: Response, offset: u64) -> Result<Fetched, StoreError> {
        let status = resp.status().as_u16();
        if status == StatusCode::PARTIAL_CONTENT.as_u16() {
            let range = resp
                .headers()
                .get("content-range")
                .and_then(parse_content_range)
                .and_then(|r| r.start.map(|s| (s, r.total)))
                .ok_or_else(|| {
                    StoreError::Protocol("206 response without a valid Content-Range".into())
                })?;
            resp.set_max_body_bytes(None);
            let mut fetched = Fetched::resumed(range.0, resp);
            fetched.total = range.1;
            return Ok(fetched);
        }
        if status == StatusCode::RANGE_NOT_SATISFIABLE.as_u16() && offset > 0 {
            let total = resp
                .headers()
                .get("content-range")
                .and_then(parse_content_range)
                .and_then(|r| r.total);
            let mut fetched = Fetched::resumed(offset, BytesStream { data: None });
            fetched.total = total;
            return Ok(fetched);
        }
        if (200..300).contains(&status) {
            let total = resp.headers().content_length();
            resp.set_max_body_bytes(None);
            let mut fetched = Fetched::new(resp);
            fetched.total = total;
            return Ok(fetched);
        }
        Err(error_from_response(resp).await)
    }
}

/// Sends `req` for the content from byte `offset` and wraps the answer with
/// [`Fetched::from_response`].
///
/// When `offset > 0` a `Range: bytes=<offset>-` header is added. Every fetch
/// asks for `Accept-Encoding: identity`, and is sent with the client's
/// response decompression turned off, since a byte range (and the digest)
/// describes the bytes as stored: a body served with `Content-Encoding` is
/// cached exactly as sent. Redirects are followed only as far as the
/// client's own [`max_redirects`](ferroly::http::ClientBuilder::max_redirects)
/// allows. This is the building block for
/// [`Cache::get_or_fetch`](super::Cache::get_or_fetch) callbacks over HTTP;
/// add authentication headers to `req` first.
pub async fn fetch_http(
    client: &Client,
    req: &Request,
    offset: u64,
) -> Result<Fetched, StoreError> {
    let client = client.clone().with_decompress(false);
    let resp = client.send(ranged(req, offset)).await?;
    Fetched::from_response(resp, offset).await
}

/// The client the adapters send through: `client` with its own redirect
/// following and decompression turned off. The adapters follow redirects
/// themselves, with explicit credential rules, and need the body bytes
/// exactly as stored.
#[cfg(any(feature = "store-s3", feature = "store-hf", feature = "store-oci"))]
pub(crate) fn adapter_client(client: Client) -> Client {
    client.with_max_redirects(0).with_decompress(false)
}

/// Whether a response to a request for the content from `offset` carries
/// content for [`Fetched::from_response`]: a `2xx`, or a `416` when resuming
/// (nothing left to send).
#[cfg(any(feature = "store-s3", feature = "store-hf", feature = "store-oci"))]
pub(crate) fn has_content(status: u16, offset: u64) -> bool {
    (200..300).contains(&status) || (status == 416 && offset > 0)
}

/// A copy of `req` asking for the content from `offset` on.
pub(crate) fn ranged(req: &Request, offset: u64) -> Request {
    let mut req = req.clone();
    req.headers.set("accept-encoding", "identity");
    if offset > 0 {
        req.headers.set("range", format!("bytes={offset}-"));
    } else {
        req.headers.remove("range");
    }
    req
}

/// Largest error body read to build an error message.
pub(crate) const ERROR_BODY_LIMIT: u64 = 64 * 1024;

/// Reads a whole body of at most `limit` bytes; a larger one is an error.
pub(crate) async fn read_capped(mut resp: Response, limit: u64) -> Result<Vec<u8>, StoreError> {
    if let Some(len) = resp.headers().content_length() {
        if len > limit {
            return Err(StoreError::Protocol(format!(
                "response body of {len} bytes exceeds the {limit}-byte limit"
            )));
        }
    }
    resp.set_max_body_bytes(Some(limit));
    match resp.bytes().await {
        Ok(b) => Ok(b),
        Err(ferroly::http::HttpError::BodyTooLarge(_)) => Err(StoreError::Protocol(format!(
            "response body exceeds the {limit}-byte limit"
        ))),
        Err(e) => Err(e.into()),
    }
}

/// Builds a [`StoreError::Remote`] from a non-success response, reading a
/// bounded amount of its body for the message.
pub(crate) async fn error_from_response(resp: Response) -> StoreError {
    let status = resp.status().as_u16();
    let body = read_capped(resp, ERROR_BODY_LIMIT)
        .await
        .unwrap_or_default();
    let message = String::from_utf8_lossy(&body);
    StoreError::remote(status, "", truncate(message.trim(), 512))
}

/// At most `max` bytes of `s`, cut at a character boundary.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// A parsed `Content-Range: bytes …` header.
pub(crate) struct ContentRange {
    /// First byte position; `None` for the unsatisfied form `bytes */N`.
    pub(crate) start: Option<u64>,
    /// Complete length; `None` when given as `*`.
    pub(crate) total: Option<u64>,
}

fn decimal(s: &str) -> Option<u64> {
    if s.is_empty() || s.len() > 19 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Parses `bytes start-end/total`, `bytes start-end/*` and `bytes */total`.
pub(crate) fn parse_content_range(value: &str) -> Option<ContentRange> {
    let rest = value.trim();
    let unit = rest.get(..5)?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let rest = rest[5..].trim_start();
    let (range, total) = rest.split_once('/')?;
    let total = match total.trim() {
        "*" => None,
        n => Some(decimal(n)?),
    };
    let range = range.trim();
    if range == "*" {
        return Some(ContentRange { start: None, total });
    }
    let (start, end) = range.split_once('-')?;
    let start = decimal(start.trim())?;
    let end = decimal(end.trim())?;
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
    fn content_range() {
        let r = parse_content_range("bytes 10-19/20").unwrap();
        assert_eq!((r.start, r.total), (Some(10), Some(20)));
        let r = parse_content_range("bytes */20").unwrap();
        assert_eq!((r.start, r.total), (None, Some(20)));
        let r = parse_content_range("BYTES 0-0/*").unwrap();
        assert_eq!((r.start, r.total), (Some(0), None));
        for bad in [
            "",
            "bytes",
            "items 0-1/2",
            "bytes 5-4/10",
            "bytes 0-10/10",
            "bytes +1-2/3",
            "bytes 0-1",
            "bytes 99999999999999999999-1/2",
        ] {
            assert!(parse_content_range(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("ééé", 3), "é…");
    }
}

//! Request/response messages and a streaming response body.

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

use super::transport::Conn;
use super::{HeaderMap, HttpError, Method, StatusCode, Uri};

/// An HTTP request (client- or server-side).
#[derive(Debug, Clone)]
pub struct Request {
    /// The request method.
    pub method: Method,
    /// The target URI (absolute on the client, path-only after server parsing).
    pub uri: Uri,
    /// Request headers.
    pub headers: HeaderMap,
    /// The request body.
    pub body: Vec<u8>,
}

impl Request {
    /// Creates a request from a method and parsed URI.
    pub fn new(method: Method, uri: Uri) -> Self {
        Self {
            method,
            uri,
            headers: HeaderMap::new(),
            body: Vec::new(),
        }
    }

    /// Starts building a request for `url`, parsing it.
    pub fn builder(method: Method, url: &str) -> Result<RequestBuilder, HttpError> {
        Ok(RequestBuilder {
            req: Request::new(method, Uri::parse(url)?),
        })
    }
}

/// A fluent builder for a [`Request`].
#[derive(Debug)]
#[must_use]
pub struct RequestBuilder {
    req: Request,
}

impl RequestBuilder {
    /// Sets a header.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.req.headers.set(name, value);
        self
    }

    /// Sets the body.
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.req.body = body.into();
        self
    }

    /// Finalizes the request.
    pub fn build(self) -> Request {
        self.req
    }
}

/// Upper bound on a single server-declared chunk size, so a hostile peer cannot
/// force a huge up-front allocation.
const MAX_CHUNK_SIZE: u64 = 64 * 1024 * 1024; // 64 MiB

/// The body framing strategy for a response being read.
pub(crate) enum Framing {
    /// A fixed number of remaining bytes.
    Length(u64),
    /// Chunked transfer-encoding.
    Chunked,
    /// Read until the connection closes.
    Eof,
    /// No more body.
    Done,
}

/// Where to return a connection once its body is fully drained, for reuse.
pub(crate) struct Reclaim {
    pub(crate) pool: std::sync::Arc<super::pool::Pool>,
    pub(crate) key: super::pool::PoolKey,
}

/// A streaming response body over the connection.
pub(crate) struct Body {
    reader: Option<BufReader<Conn>>,
    framing: Framing,
    /// Set when the response is keep-alive-able: on clean completion the
    /// connection is returned here instead of being closed.
    reclaim: Option<Reclaim>,
}

impl Body {
    pub(crate) fn new(reader: BufReader<Conn>, framing: Framing) -> Self {
        Self {
            reader: Some(reader),
            framing,
            reclaim: None,
        }
    }

    /// A body whose connection is returned to `reclaim.pool` for reuse once it is
    /// fully and cleanly drained.
    pub(crate) fn pooled(reader: BufReader<Conn>, framing: Framing, reclaim: Reclaim) -> Self {
        Self {
            reader: Some(reader),
            framing,
            reclaim: Some(reclaim),
        }
    }

    /// Cleanly complete: hand the (drained) connection back to the pool. Only
    /// called on self-delimited completion, never after a truncation/EOF-close.
    fn reclaim(&mut self) {
        if let Some(reclaim) = self.reclaim.take() {
            if let Some(reader) = self.reader.take() {
                reclaim.pool.put(reclaim.key, reader);
            }
        }
    }

    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, HttpError> {
        // Once the reader has been reclaimed/dropped, the body is finished.
        let reader = match self.reader.as_mut() {
            Some(r) => r,
            None => return Ok(None),
        };
        match self.framing {
            Framing::Done => {
                self.reclaim();
                Ok(None)
            }
            Framing::Length(0) => {
                self.framing = Framing::Done;
                self.reclaim();
                Ok(None)
            }
            Framing::Length(n) => {
                let want = n.min(16384) as usize;
                let mut buf = vec![0u8; want];
                let read = reader.read(&mut buf).await?;
                if read == 0 {
                    // Truncated before the declared length: the connection is
                    // unusable — drop it rather than pooling.
                    self.framing = Framing::Done;
                    self.reader = None;
                    return Ok(None);
                }
                buf.truncate(read);
                self.framing = Framing::Length(n - read as u64);
                Ok(Some(buf))
            }
            Framing::Chunked => {
                let mut size_line = String::new();
                reader.read_line(&mut size_line).await?;
                let size_str = size_line.trim().split(';').next().unwrap_or("").trim();
                let size = u64::from_str_radix(size_str, 16)
                    .map_err(|_| HttpError::Protocol(format!("bad chunk size: {size_str}")))?;
                // Reject an absurd server-declared chunk size before allocating —
                // a hostile/buggy peer must not force a huge up-front allocation.
                if size > MAX_CHUNK_SIZE {
                    return Err(HttpError::Protocol("chunk size exceeds maximum".into()));
                }
                if size == 0 {
                    // Consume trailers up to the blank line.
                    loop {
                        let mut l = String::new();
                        let n = reader.read_line(&mut l).await?;
                        if n == 0 || l.trim().is_empty() {
                            break;
                        }
                    }
                    self.framing = Framing::Done;
                    self.reclaim();
                    return Ok(None);
                }
                let mut buf = vec![0u8; size as usize];
                reader.read_exact(&mut buf).await?;
                let mut crlf = [0u8; 2];
                reader.read_exact(&mut crlf).await?;
                Ok(Some(buf))
            }
            Framing::Eof => {
                let mut buf = vec![0u8; 16384];
                let read = reader.read(&mut buf).await?;
                if read == 0 {
                    // End-by-close: the connection is not reusable.
                    self.framing = Framing::Done;
                    self.reader = None;
                    return Ok(None);
                }
                buf.truncate(read);
                Ok(Some(buf))
            }
        }
    }
}

/// An HTTP response with a streaming body.
pub struct Response {
    status: StatusCode,
    headers: HeaderMap,
    body: Body,
}

impl Response {
    pub(crate) fn new(status: StatusCode, headers: HeaderMap, body: Body) -> Self {
        Self {
            status,
            headers,
            body,
        }
    }

    /// The response status code.
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// The response headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Whether the status is 2xx.
    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }

    /// Reads the next body chunk, or `None` at end of body.
    pub async fn chunk(&mut self) -> Result<Option<Vec<u8>>, HttpError> {
        self.body.next_chunk().await
    }

    /// Reads the entire remaining body into a byte vector.
    pub async fn bytes(mut self) -> Result<Vec<u8>, HttpError> {
        let mut out = Vec::new();
        while let Some(chunk) = self.body.next_chunk().await? {
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    /// Reads the entire remaining body as a UTF-8 (lossy) string.
    pub async fn text(self) -> Result<String, HttpError> {
        Ok(String::from_utf8_lossy(&self.bytes().await?).into_owned())
    }
}

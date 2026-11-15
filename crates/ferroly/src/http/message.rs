//! Request/response messages and a streaming response body.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::Instant;

use super::body::BodyStream;
use super::form::{form_urlencode, Multipart};
use super::inflate::Decoder;
use super::io::{ChunkedReader, HeadLimits, BODY_PIECE_BYTES};
use super::transport::Conn;
use super::{HeaderMap, HttpError, Method, Protocol, StatusCode, Uri};

/// What the server knows about the connection a request arrived on.
#[derive(Debug, Clone)]
pub(crate) struct ConnInfo {
    /// The remote socket address.
    pub(crate) peer_addr: Option<SocketAddr>,
    /// The local socket address the connection was accepted on.
    pub(crate) local_addr: Option<SocketAddr>,
    /// Whether the connection is TLS.
    pub(crate) tls: bool,
    /// The client's certificate chain (DER, leaf first) when it presented one
    /// during a mutual-TLS handshake.
    pub(crate) peer_certs: Arc<Vec<Vec<u8>>>,
}

/// An HTTP request (client- or server-side).
///
/// Cloning a request copies everything except a streamed body
/// ([`take_body_stream`](Self::take_body_stream)), which cannot be shared:
/// the clone has none.
#[derive(Debug)]
pub struct Request {
    /// The request method.
    pub method: Method,
    /// The target URI (absolute on the client, path-only after server parsing).
    pub uri: Uri,
    /// Request headers.
    pub headers: HeaderMap,
    /// The request body.
    pub body: Vec<u8>,
    /// Connection metadata, set by the server for the requests it parses.
    conn: Option<ConnInfo>,
    /// The protocol the request arrived over.
    protocol: Protocol,
    /// A streamed body (server, for handlers that ask for one).
    body_stream: Option<BodyStream>,
}

impl Clone for Request {
    fn clone(&self) -> Self {
        Self {
            method: self.method.clone(),
            uri: self.uri.clone(),
            headers: self.headers.clone(),
            body: self.body.clone(),
            conn: self.conn.clone(),
            protocol: self.protocol,
            body_stream: None,
        }
    }
}

impl Request {
    /// Creates a request from a method and parsed URI.
    pub fn new(method: Method, uri: Uri) -> Self {
        Self {
            method,
            uri,
            headers: HeaderMap::new(),
            body: Vec::new(),
            conn: None,
            protocol: Protocol::Http11,
            body_stream: None,
        }
    }

    /// Starts building a request for `url`, parsing it.
    pub fn builder(method: Method, url: &str) -> Result<RequestBuilder, HttpError> {
        Ok(RequestBuilder {
            req: Request::new(method, Uri::parse(url)?),
        })
    }

    /// The remote socket address of the connection this request arrived on.
    ///
    /// `Some` for every request the server hands to a handler; `None` for a
    /// request built by hand (for example on the client). This is the address
    /// of the directly connected peer — behind a reverse proxy it is the
    /// proxy's address.
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.conn.as_ref().and_then(|c| c.peer_addr)
    }

    /// The local socket address the connection was accepted on, when known.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.conn.as_ref().and_then(|c| c.local_addr)
    }

    /// Whether the request arrived over TLS (served by `serve_tls*`). For a
    /// client-side request this reflects the URL scheme instead.
    pub fn is_tls(&self) -> bool {
        match &self.conn {
            Some(c) => c.tls,
            None => self.uri.is_tls(),
        }
    }

    /// The client's certificate chain (DER-encoded, leaf first) when the
    /// connection is mutual TLS and the client presented a certificate;
    /// otherwise empty. The chain has already been verified against the
    /// configured client CA bundle.
    pub fn peer_certificates(&self) -> &[Vec<u8>] {
        match &self.conn {
            Some(c) => c.peer_certs.as_slice(),
            None => &[],
        }
    }

    /// Sets the peer address reported by [`peer_addr`](Self::peer_addr). The
    /// server does this itself; the setter exists so middleware and handlers
    /// can be unit-tested with a hand-built request.
    pub fn set_peer_addr(&mut self, addr: SocketAddr) {
        self.conn_mut().peer_addr = Some(addr);
    }

    /// Sets what [`is_tls`](Self::is_tls) reports (see
    /// [`set_peer_addr`](Self::set_peer_addr) for when this is useful).
    pub fn set_tls(&mut self, tls: bool) {
        self.conn_mut().tls = tls;
    }

    fn conn_mut(&mut self) -> &mut ConnInfo {
        let tls = self.uri.is_tls();
        self.conn.get_or_insert_with(|| ConnInfo {
            peer_addr: None,
            local_addr: None,
            tls,
            peer_certs: Arc::new(Vec::new()),
        })
    }

    pub(crate) fn set_conn_info(&mut self, info: ConnInfo) {
        self.conn = Some(info);
    }

    /// The protocol the request arrived over: `Http10`/`Http11`, or `Http2`
    /// for a request served over HTTP/2. A request built by hand reports
    /// `Http11`.
    pub fn protocol(&self) -> Protocol {
        self.protocol
    }

    pub(crate) fn set_protocol(&mut self, protocol: Protocol) {
        self.protocol = protocol;
    }

    /// Takes the streamed request body, when the handler asked for one with
    /// [`HttpHandler::wants_body_stream`](super::HttpHandler::wants_body_stream)
    /// (`None` otherwise, or once taken). [`body`](Self::body) is empty for
    /// such a request.
    ///
    /// The stream yields the body as it arrives — the handler can read it
    /// while it streams its response — and ends with the request's trailers
    /// (HTTP/2 trailing HEADERS, or the HTTP/1.1 chunked trailer section), if
    /// any. Dropping it early discards the rest of the body without affecting
    /// the response; over HTTP/1.1 the connection is then closed after the
    /// response, because the unread rest of the body is still on it.
    pub fn take_body_stream(&mut self) -> Option<BodyStream> {
        self.body_stream.take()
    }

    /// Attaches a streamed body (see
    /// [`take_body_stream`](Self::take_body_stream)). The server does this
    /// itself; the setter exists so streaming handlers can be unit-tested
    /// with a hand-built request.
    pub fn set_body_stream(&mut self, body: BodyStream) {
        self.body_stream = Some(body);
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

    /// Sets an `application/x-www-form-urlencoded` body from `(name, value)`
    /// pairs and the matching `Content-Type`.
    pub fn form<I, K, V>(self, pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let body = form_urlencode(pairs);
        self.header("content-type", "application/x-www-form-urlencoded")
            .body(body.into_bytes())
    }

    /// Sets a `multipart/form-data` body and the matching `Content-Type`
    /// (including the boundary).
    pub fn multipart(self, form: &Multipart) -> Self {
        self.header("content-type", form.content_type())
            .body(form.encode())
    }

    /// Sets a `Range: bytes=start-end` header requesting a byte range. `end` is
    /// inclusive; pass `None` for an open-ended range (`start-`), e.g. to resume
    /// a download from `start`.
    pub fn range(self, start: u64, end: Option<u64>) -> Self {
        let value = match end {
            Some(end) => format!("bytes={start}-{end}"),
            None => format!("bytes={start}-"),
        };
        self.header("range", value)
    }

    /// Finalizes the request.
    pub fn build(self) -> Request {
        self.req
    }
}

/// The body framing strategy for a response being read.
#[derive(Debug)]
pub(crate) enum Framing {
    /// A fixed number of remaining bytes.
    Length(u64),
    /// Chunked transfer-encoding.
    Chunked,
    /// Read until the connection closes.
    Eof,
    /// No more body.
    Done,
    /// An earlier read failed; the body cannot be continued.
    Failed,
}

/// Where to return a connection once its body is fully drained, for reuse.
pub(crate) struct Reclaim {
    pub(crate) pool: Arc<super::pool::Pool>,
    pub(crate) key: super::pool::PoolKey,
}

/// Limits applied while a response body is read.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BodyLimits {
    /// Cap on the body size, applied to the bytes on the wire and (when the
    /// body is decompressed) to the decoded bytes as well.
    pub(crate) max_bytes: Option<u64>,
    /// Longest wait for the next piece of the body.
    pub(crate) read_timeout: Option<Duration>,
    /// Absolute deadline for the whole exchange.
    pub(crate) deadline: Option<Instant>,
    /// Line/trailer limits for chunked framing.
    pub(crate) head: HeadLimits,
}

/// A streaming response body over the connection.
pub(crate) struct Body {
    reader: Option<BufReader<Conn>>,
    framing: Framing,
    /// Set when the response is keep-alive-able: on clean completion the
    /// connection is returned here instead of being closed.
    reclaim: Option<Reclaim>,
    limits: BodyLimits,
    chunked: ChunkedReader,
    /// Bytes of body read off the wire so far.
    received: u64,
    /// Content decoder for a compressed body, with the bytes it has produced.
    decoder: Option<(Decoder, u64)>,
    /// Trailers received at the end of the body.
    trailers: Option<HeaderMap>,
    /// The body is an HTTP/2 stream rather than bytes on `reader`.
    #[cfg(feature = "http2")]
    h2: Option<super::h2::RecvStream>,
    /// For an HTTP/2 request body streamed in the background: released when
    /// the response ends cleanly, dropped (stopping the upload) when the
    /// body fails or is dropped before its end.
    #[cfg(feature = "http2")]
    upload: Option<super::h2::client::UploadWatch>,
    /// The connection's slot under the client's per-host active-connection
    /// limit, released as soon as the connection is pooled or closed.
    slot: Option<OwnedSemaphorePermit>,
}

impl Body {
    /// Holds `slot` until the connection is released (at once when it
    /// already has been).
    pub(crate) fn holding(mut self, slot: Option<OwnedSemaphorePermit>) -> Self {
        if matches!(self.framing, Framing::Done | Framing::Length(0)) {
            // Nothing to read (HEAD, 204, 304, an empty body) on a connection
            // that is not pooled either: close it now, so its slot is free
            // even while the caller holds the response.
            self.finish();
        }
        if self.reader.is_some() {
            self.slot = slot;
        }
        self
    }

    pub(crate) fn new(reader: BufReader<Conn>, framing: Framing, limits: BodyLimits) -> Self {
        Self {
            reader: Some(reader),
            framing,
            reclaim: None,
            limits,
            chunked: ChunkedReader::new(limits.max_bytes),
            received: 0,
            decoder: None,
            trailers: None,
            #[cfg(feature = "http2")]
            h2: None,
            #[cfg(feature = "http2")]
            upload: None,
            slot: None,
        }
    }

    /// A body read from an HTTP/2 stream; `upload` watches the request body
    /// being streamed in the background, if any.
    #[cfg(feature = "http2")]
    pub(crate) fn h2(
        stream: super::h2::RecvStream,
        limits: BodyLimits,
        upload: Option<super::h2::client::UploadWatch>,
    ) -> Self {
        Self {
            reader: None,
            framing: Framing::Eof,
            reclaim: None,
            limits,
            chunked: ChunkedReader::new(limits.max_bytes),
            received: 0,
            decoder: None,
            trailers: None,
            h2: Some(stream),
            upload,
            slot: None,
        }
    }

    /// A body whose connection is returned to `reclaim.pool` for reuse once it is
    /// fully and cleanly drained.
    pub(crate) fn pooled(
        reader: BufReader<Conn>,
        framing: Framing,
        limits: BodyLimits,
        reclaim: Reclaim,
    ) -> Self {
        let mut body = Self::new(reader, framing, limits);
        body.reclaim = Some(reclaim);
        if matches!(body.framing, Framing::Done | Framing::Length(0)) {
            // Nothing to read (HEAD, 204, 304, an empty body): the connection
            // is free for the next request right away, even if the caller
            // never touches the body.
            body.finish();
        }
        body
    }

    /// Decodes the body with `decoder` (gzip / deflate) as it is read.
    pub(crate) fn with_decoder(mut self, decoder: Decoder) -> Self {
        self.decoder = Some((decoder, 0));
        self
    }

    fn set_max_bytes(&mut self, max: Option<u64>) {
        self.limits.max_bytes = max;
        self.chunked.set_max(max);
    }

    /// Cleanly complete: hand the (drained) connection back to the pool. Only
    /// called on self-delimited completion, never after a truncation/EOF-close.
    fn finish(&mut self) {
        self.framing = Framing::Done;
        // Without a pool to return to (`Connection: close`, HTTP/1.0,
        // ambiguous framing), dropping the reader closes the connection.
        // Either way the connection is released, and with it its slot.
        let reader = self.reader.take();
        if let (Some(reclaim), Some(reader)) = (self.reclaim.take(), reader) {
            reclaim.pool.put(reclaim.key, reader);
        }
        self.slot = None;
    }

    /// Marks the body unusable and closes the connection.
    fn fail(&mut self) {
        self.framing = Framing::Failed;
        self.reader = None;
        self.reclaim = None;
        self.slot = None;
        #[cfg(feature = "http2")]
        {
            // Dropping the stream resets it, and dropping the watch stops a
            // request body still being streamed on it.
            self.h2 = None;
            self.upload = None;
        }
    }

    /// The time allowed for the next read: the idle timeout, shortened to what
    /// is left of the total deadline.
    fn budget(&self) -> Result<Option<Duration>, HttpError> {
        let until_deadline = match self.limits.deadline {
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(HttpError::Timeout);
                }
                Some(left)
            }
            None => None,
        };
        Ok(match (self.limits.read_timeout, until_deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        })
    }

    /// Reads the next piece off the wire (still content-encoded), applying the
    /// timeouts. Any error makes the body unusable.
    async fn next_raw(&mut self) -> Result<Option<Vec<u8>>, HttpError> {
        let result = match self.budget() {
            Err(e) => Err(e),
            Ok(Some(limit)) => match tokio::time::timeout(limit, self.read_raw()).await {
                Ok(r) => r,
                Err(_) => Err(HttpError::Timeout),
            },
            Ok(None) => self.read_raw().await,
        };
        if result.is_err() {
            self.fail();
        }
        result
    }

    async fn read_raw(&mut self) -> Result<Option<Vec<u8>>, HttpError> {
        let max = self.limits.max_bytes;
        #[cfg(feature = "http2")]
        if let Some(stream) = self.h2.as_mut() {
            if matches!(self.framing, Framing::Failed) {
                return Err(HttpError::Protocol(
                    "response body is unusable after an earlier error".into(),
                ));
            }
            return match stream.next().await? {
                super::h2::Chunk::Data(piece) => {
                    self.received += piece.len() as u64;
                    if let Some(max) = max {
                        if self.received > max {
                            return Err(HttpError::BodyTooLarge(max));
                        }
                    }
                    Ok(Some(piece))
                }
                super::h2::Chunk::End(trailers) => {
                    if trailers.is_some() {
                        self.trailers = trailers;
                    }
                    self.framing = Framing::Done;
                    if let Some(mut upload) = self.upload.take() {
                        upload.release();
                    }
                    Ok(None)
                }
            };
        }
        match self.framing {
            Framing::Failed => Err(HttpError::Protocol(
                "response body is unusable after an earlier error".into(),
            )),
            Framing::Done | Framing::Length(0) => {
                self.finish();
                Ok(None)
            }
            Framing::Length(n) => {
                if let Some(max) = max {
                    if self.received.saturating_add(n) > max {
                        return Err(HttpError::BodyTooLarge(max));
                    }
                }
                let Some(reader) = self.reader.as_mut() else {
                    return Ok(None);
                };
                let piece = take_piece(reader, n).await?;
                if piece.is_empty() {
                    // The peer closed before the declared length: an error,
                    // never a short success.
                    return Err(HttpError::Protocol(format!(
                        "response body truncated: {n} bytes short of Content-Length"
                    )));
                }
                self.received += piece.len() as u64;
                self.framing = Framing::Length(n - piece.len() as u64);
                Ok(Some(piece))
            }
            Framing::Chunked => {
                let Some(reader) = self.reader.as_mut() else {
                    return Ok(None);
                };
                match self.chunked.next(reader, &self.limits.head).await? {
                    Some(piece) => {
                        self.received += piece.len() as u64;
                        Ok(Some(piece))
                    }
                    None => {
                        self.trailers = self.chunked.take_trailers();
                        self.finish();
                        Ok(None)
                    }
                }
            }
            Framing::Eof => {
                let Some(reader) = self.reader.as_mut() else {
                    return Ok(None);
                };
                let piece = take_piece(reader, u64::MAX).await?;
                if piece.is_empty() {
                    // End-by-close: the connection is not reusable.
                    self.framing = Framing::Done;
                    self.reader = None;
                    self.reclaim = None;
                    self.slot = None;
                    return Ok(None);
                }
                self.received += piece.len() as u64;
                if let Some(max) = max {
                    if self.received > max {
                        return Err(HttpError::BodyTooLarge(max));
                    }
                }
                Ok(Some(piece))
            }
        }
    }

    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, HttpError> {
        if self.decoder.is_none() {
            return self.next_raw().await;
        }
        loop {
            let max = self.limits.max_bytes;
            let step = {
                let Some((decoder, produced)) = self.decoder.as_mut() else {
                    return Ok(None);
                };
                match decoder.decode(BODY_PIECE_BYTES) {
                    Ok(out) => {
                        *produced += out.len() as u64;
                        match max {
                            // The cap also bounds the decoded size, so a tiny
                            // compressed body cannot expand without limit.
                            Some(max) if *produced > max => Err(HttpError::BodyTooLarge(max)),
                            _ => Ok(out),
                        }
                    }
                    Err(e) => Err(e),
                }
            };
            match step {
                Ok(out) if !out.is_empty() => return Ok(Some(out)),
                Ok(_) => {}
                Err(e) => {
                    self.fail();
                    return Err(e);
                }
            }
            match self.next_raw().await? {
                Some(raw) => {
                    if let Some((decoder, _)) = self.decoder.as_mut() {
                        decoder.feed(&raw);
                    }
                }
                None => {
                    let complete = self
                        .decoder
                        .as_ref()
                        .map(|(d, _)| d.is_complete())
                        .unwrap_or(true);
                    if !complete {
                        self.fail();
                        return Err(HttpError::Protocol(
                            "compressed response body truncated".into(),
                        ));
                    }
                    return Ok(None);
                }
            }
        }
    }

    /// Reads and discards the rest of the body if it is at most `limit` bytes,
    /// so the connection can be reused; otherwise drops the connection.
    pub(crate) async fn discard(mut self, limit: u64) {
        let mut seen = 0u64;
        while let Ok(Some(piece)) = self.next_raw().await {
            seen += piece.len() as u64;
            if seen > limit {
                break;
            }
        }
    }
}

/// Takes up to `want` buffered bytes (at most one piece) from `reader`; an
/// empty result means EOF.
async fn take_piece(reader: &mut BufReader<Conn>, want: u64) -> Result<Vec<u8>, HttpError> {
    let piece = {
        let available = reader.fill_buf().await?;
        let n = (available.len() as u64)
            .min(want)
            .min(BODY_PIECE_BYTES as u64) as usize;
        available[..n].to_vec()
    };
    reader.consume(piece.len());
    Ok(piece)
}

/// An HTTP response with a streaming body.
pub struct Response {
    status: StatusCode,
    headers: HeaderMap,
    uri: Uri,
    body: Body,
    protocol: Protocol,
}

impl std::fmt::Debug for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("uri", &self.uri)
            .finish_non_exhaustive()
    }
}

impl Response {
    pub(crate) fn new(status: StatusCode, headers: HeaderMap, uri: Uri, body: Body) -> Self {
        Self {
            status,
            headers,
            uri,
            body,
            protocol: Protocol::Http11,
        }
    }

    pub(crate) fn with_protocol(mut self, protocol: Protocol) -> Self {
        self.protocol = protocol;
        self
    }

    /// The protocol the response was received over: `Http11` (or `Http10`
    /// when the server answered as HTTP/1.0), or `Http2`.
    pub fn protocol(&self) -> Protocol {
        self.protocol
    }

    /// The trailer fields that ended the body: available once
    /// [`chunk`](Self::chunk) has returned `None`, and `None` when the
    /// response had none. HTTP/2 trailers (a trailing HEADERS frame — gRPC's
    /// `grpc-status`) and HTTP/1.1 chunked trailers are both reported.
    pub fn trailers(&self) -> Option<&HeaderMap> {
        self.body.trailers.as_ref()
    }

    /// Reads the entire remaining body (bounded by the client's
    /// response-body cap) and returns it with the trailers, if any.
    pub async fn bytes_with_trailers(mut self) -> Result<(Vec<u8>, Option<HeaderMap>), HttpError> {
        let mut out = Vec::new();
        while let Some(chunk) = self.body.next_chunk().await? {
            out.extend_from_slice(&chunk);
        }
        Ok((out, self.body.trailers.take()))
    }

    /// The response status code.
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// The response headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// The URL this response came from — the request URL, or the final URL
    /// when the client followed redirects.
    pub fn uri(&self) -> &Uri {
        &self.uri
    }

    /// Whether the status is 2xx.
    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }

    /// Overrides the client's response-body size cap for this response
    /// (`None` removes it). Call it before reading the body — for example to
    /// stream a large download to disk without buffering it.
    pub fn set_max_body_bytes(&mut self, max: Option<u64>) {
        self.body.set_max_bytes(max);
    }

    /// Reads the next body chunk, or `None` at end of body.
    ///
    /// Fails with [`HttpError::Timeout`] when the client's read or total
    /// timeout expires, [`HttpError::BodyTooLarge`] when the body exceeds the
    /// size cap, and [`HttpError::Protocol`] when the body is malformed or
    /// ends before its declared length. After an error the body cannot be
    /// read further and the connection is closed.
    pub async fn chunk(&mut self) -> Result<Option<Vec<u8>>, HttpError> {
        self.body.next_chunk().await
    }

    /// Reads the entire remaining body into a byte vector (bounded by the
    /// client's response-body cap).
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

    pub(crate) fn into_body(self) -> Body {
        self.body
    }
}

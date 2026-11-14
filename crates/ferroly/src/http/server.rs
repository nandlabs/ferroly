//! A minimal HTTP/1.1 server: accept loop, request parsing, and a handler trait.
//!
//! The routing/handler ergonomics live in [`crate::turbo`]; this module provides
//! the raw serving primitive that a [`HttpHandler`] plugs into.

use std::future::Future;
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch, Semaphore};
use tokio::time::timeout;

use super::core::{split_target, TransferCoding};
use super::date::format_http_date;
use super::form::normalize_path;
use super::io::{self, ChunkedReader, HeadError, HeadLimits, RequestHead, Version};
use super::message::{ConnInfo, Request};
use super::transport::{self, Conn, ReloadableTlsConfig};
use super::{HeaderMap, HttpError, Method, StatusCode, Uri};

/// A boxed, `Send` future — the manual `async fn`-in-trait desugaring so
/// [`HttpHandler`] stays object-safe.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Largest slice written to the socket under one write timeout.
const WRITE_SLICE_BYTES: usize = 64 * 1024;
/// After an error response, how long and how much unread request data is
/// discarded before the socket is closed (so the response is not lost to a
/// TCP reset).
const LINGER_TIMEOUT: Duration = Duration::from_secs(2);
const LINGER_BYTES: usize = 1024 * 1024;

/// A callback that receives the errors the server would otherwise have to
/// discard: failed accepts, TLS handshake failures, malformed or timed-out
/// requests, connection I/O errors, and handler panics or timeouts.
///
/// The first argument is the peer address when the error belongs to a
/// connection (`None` for a failed `accept`). The hook runs on the connection
/// task, so keep it quick and non-blocking; a panic inside it is contained.
///
/// ```
/// use ferroly::http::{ErrorHook, ServerConfig};
///
/// let config = ServerConfig {
///     on_error: Some(ErrorHook::new(|peer, err| {
///         eprintln!("http: {peer:?}: {err}");
///     })),
///     ..ServerConfig::default()
/// };
/// # let _ = config;
/// ```
#[derive(Clone)]
pub struct ErrorHook(Arc<ErrorCallback>);

type ErrorCallback = dyn Fn(Option<SocketAddr>, &HttpError) + Send + Sync;

impl ErrorHook {
    /// Wraps a callback.
    pub fn new(f: impl Fn(Option<SocketAddr>, &HttpError) + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    fn call(&self, peer: Option<SocketAddr>, error: &HttpError) {
        // A faulty hook must not take the connection (or the accept loop) down.
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| (self.0)(peer, error)));
    }
}

impl std::fmt::Debug for ErrorHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ErrorHook(..)")
    }
}

/// Resource limits and behaviour applied to each served connection. The
/// defaults are safe for direct exposure; tune them with
/// [`serve_with_config`].
///
/// New fields are added over time, so build a value with struct-update syntax
/// (`ServerConfig { max_body_bytes: 1 << 20, ..ServerConfig::default() }`)
/// rather than naming every field.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Maximum accepted request-body size (`Content-Length` or decoded chunked
    /// body); larger requests are rejected with `413 Payload Too Large` before
    /// the body is read.
    pub max_body_bytes: usize,
    /// Maximum number of connections served concurrently. Further connections
    /// are not accepted until an in-flight one completes (backpressure).
    pub max_connections: usize,
    /// Deadline for reading the request line + headers once the first byte of
    /// a request has arrived (and for a new connection to send that first
    /// byte, and for the TLS handshake). Bounds slow-loris attacks that
    /// trickle the head one byte at a time. Expiry mid-head is answered with
    /// `408 Request Timeout`.
    pub head_timeout: Duration,
    /// Deadline for reading the request body once its framing is known.
    /// Expiry is answered with `408 Request Timeout`.
    pub body_timeout: Duration,
    /// On shutdown, how long to wait for in-flight connections to finish before
    /// returning (connection draining for graceful rolling deploys).
    pub drain_timeout: Duration,
    /// Maximum number of requests served on a single kept-alive connection
    /// before it is closed. Bounds per-connection resource lifetime.
    pub max_keep_alive_requests: usize,
    /// Whether connections are kept alive between requests at all. `false`
    /// answers every request with `Connection: close`.
    pub keep_alive: bool,
    /// How long a kept-alive connection may sit idle between requests before
    /// the server closes it.
    pub idle_timeout: Duration,
    /// Deadline for the client to accept each piece of a response (up to
    /// 64 KiB at a time). A client that stops reading is disconnected when it
    /// expires, releasing its connection slot.
    pub write_timeout: Duration,
    /// Optional deadline for the handler to produce a response. When it
    /// expires the handler future is dropped and the client receives
    /// `503 Service Unavailable`. `None` (the default) lets handlers run as
    /// long as they need. It does not limit how long a streaming response
    /// body may keep producing chunks.
    pub handler_timeout: Option<Duration>,
    /// Maximum total size of the request header section, and of a chunked
    /// body's trailer section. Exceeding it is answered with `431`.
    pub max_header_bytes: usize,
    /// Maximum number of request header lines, and of trailer lines.
    /// Exceeding it is answered with `431`.
    pub max_headers: usize,
    /// Maximum length of the request line and of any single header or trailer
    /// line. An over-long request line is answered with `414`, an over-long
    /// header line with `431`.
    pub max_line_bytes: usize,
    /// Normalise the request path with
    /// [`normalize_path`](super::normalize_path) before the handler sees it:
    /// unreserved percent-escapes are decoded, `.`/`..` segments removed, and
    /// a path with an empty segment (`//`), a malformed escape or an encoded
    /// control character is rejected with `400`. Off by default, in which case
    /// handlers receive the path exactly as sent.
    pub normalize_paths: bool,
    /// Receives errors that are otherwise invisible to the application; see
    /// [`ErrorHook`].
    pub on_error: Option<ErrorHook>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_body_bytes: 8 * 1024 * 1024, // 8 MiB
            max_connections: 1024,
            head_timeout: Duration::from_secs(30),
            body_timeout: Duration::from_secs(60),
            drain_timeout: Duration::from_secs(30),
            max_keep_alive_requests: 1000,
            keep_alive: true,
            idle_timeout: Duration::from_secs(30),
            write_timeout: Duration::from_secs(30),
            handler_timeout: None,
            max_header_bytes: io::DEFAULT_MAX_HEADER_BYTES,
            max_headers: io::DEFAULT_MAX_HEADERS,
            max_line_bytes: io::DEFAULT_MAX_LINE_BYTES,
            normalize_paths: false,
            on_error: None,
        }
    }
}

impl ServerConfig {
    fn head_limits(&self) -> HeadLimits {
        HeadLimits {
            max_line_bytes: self.max_line_bytes,
            max_headers: self.max_headers,
            max_header_bytes: self.max_header_bytes,
            allow_bare_lf: false,
        }
    }

    fn report(&self, peer: Option<SocketAddr>, error: &HttpError) {
        if let Some(hook) = &self.on_error {
            hook.call(peer, error);
        }
    }
}

/// An owned outgoing HTTP response produced by a handler.
///
/// The body is either the fixed [`body`](Self::body) bytes (written with
/// `Content-Length`) or a chunked stream set via [`stream`](Self::stream) /
/// [`event_stream`](Self::event_stream) (written with `Transfer-Encoding:
/// chunked`, flushed per chunk).
///
/// The server owns the framing headers: it always writes its own
/// `Content-Length` (or `Transfer-Encoding`) for the body it sends and ignores
/// one set here, except that the response to a `HEAD` request keeps a
/// `Content-Length` you set (to advertise the size of the entity). It also
/// adds a `Date` header unless you set one.
#[derive(Debug)]
pub struct HttpResponse {
    /// The status code.
    pub status: StatusCode,
    /// Response headers.
    pub headers: HeaderMap,
    /// The fixed response body (ignored when a stream is set).
    pub body: Vec<u8>,
    /// A chunked body stream, if any.
    stream: Option<mpsc::Receiver<Vec<u8>>>,
}

impl HttpResponse {
    /// A response with the given status and an empty body.
    pub fn new(status: StatusCode) -> Self {
        Self {
            status,
            headers: HeaderMap::new(),
            body: Vec::new(),
            stream: None,
        }
    }

    /// A response whose body is streamed from `chunks` using chunked transfer
    /// encoding. Each received `Vec<u8>` is written and flushed immediately.
    pub fn stream(status: StatusCode, chunks: mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            status,
            headers: HeaderMap::new(),
            body: Vec::new(),
            stream: Some(chunks),
        }
    }

    /// A Server-Sent Events response: `text/event-stream`, with each `String`
    /// from `events` sent as one event and flushed as it arrives.
    ///
    /// Each string goes through the same frame encoder as
    /// [`sse`](Self::sse): a line break (`\r\n`, `\r` or `\n`) inside it
    /// becomes a new `data:` line of the same event, so the text can never
    /// forge another field or event. For multi-field events
    /// (`id`/`event`/`retry`) use [`sse`](Self::sse).
    pub fn event_stream(mut events: mpsc::Receiver<String>) -> Self {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(16);
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let frame = crate::http::sse::Event::new(event).to_frame().into_bytes();
                if tx.send(frame).await.is_err() {
                    break;
                }
            }
        });
        Self::stream(StatusCode::OK, rx)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
    }

    /// A Server-Sent Events response streaming structured
    /// [`Event`](crate::http::sse::Event)s.
    ///
    /// Each event is encoded with
    /// [`Event::to_frame`](crate::http::sse::Event::to_frame) — correct
    /// multi-line `data:` framing plus optional `id`/`event`/`retry` — and
    /// flushed as it arrives. The response is `text/event-stream` with
    /// `cache-control: no-cache`.
    pub fn sse(mut events: mpsc::Receiver<crate::http::sse::Event>) -> Self {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(16);
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if tx.send(event.to_frame().into_bytes()).await.is_err() {
                    break;
                }
            }
        });
        Self::stream(StatusCode::OK, rx)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
    }

    /// A 200 OK with an empty body.
    pub fn ok() -> Self {
        Self::new(StatusCode::OK)
    }

    /// Sets a response header.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.set(name, value);
        self
    }

    /// Sets the response body.
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    /// A `text/plain` response.
    pub fn text(status: StatusCode, text: impl Into<String>) -> Self {
        Self::new(status)
            .header("Content-Type", "text/plain; charset=utf-8")
            .body(text.into().into_bytes())
    }
}

/// Handles a parsed [`Request`], producing an [`HttpResponse`].
///
/// A handler that panics does not take the server (or even the connection)
/// down: the panic is caught, the client receives `500 Internal Server Error`,
/// and the panic is reported to [`ServerConfig::on_error`] as
/// [`HttpError::HandlerPanic`].
pub trait HttpHandler: Send + Sync {
    /// Handles one request.
    fn handle(&self, req: Request) -> BoxFuture<'_, HttpResponse>;
}

/// Serves connections from `listener` with `handler` until `shutdown` resolves,
/// using the default [`ServerConfig`] limits.
pub async fn serve<F>(
    listener: TcpListener,
    handler: Arc<dyn HttpHandler>,
    shutdown: F,
) -> Result<(), HttpError>
where
    F: Future<Output = ()> + Send,
{
    serve_with_config(listener, handler, shutdown, ServerConfig::default()).await
}

/// Serves connections with explicit resource [`ServerConfig`] limits.
pub async fn serve_with_config<F>(
    listener: TcpListener,
    handler: Arc<dyn HttpHandler>,
    shutdown: F,
    config: ServerConfig,
) -> Result<(), HttpError>
where
    F: Future<Output = ()> + Send,
{
    serve_inner(listener, handler, shutdown, config, None).await
}

/// Serves HTTPS: like [`serve`] but terminates TLS on each connection using the
/// certificate/key in `tls`. Build a [`TlsConfig`](super::TlsConfig) with
/// `TlsConfig::from_pem`/`from_der`, or `TlsConfig::builder` for mutual TLS
/// and a minimum protocol version.
pub async fn serve_tls<F>(
    listener: TcpListener,
    handler: Arc<dyn HttpHandler>,
    shutdown: F,
    tls: super::TlsConfig,
) -> Result<(), HttpError>
where
    F: Future<Output = ()> + Send,
{
    serve_tls_with_config(listener, handler, shutdown, ServerConfig::default(), tls).await
}

/// HTTPS variant of [`serve_with_config`].
pub async fn serve_tls_with_config<F>(
    listener: TcpListener,
    handler: Arc<dyn HttpHandler>,
    shutdown: F,
    config: ServerConfig,
    tls: super::TlsConfig,
) -> Result<(), HttpError>
where
    F: Future<Output = ()> + Send,
{
    serve_inner(listener, handler, shutdown, config, Some(tls.reloadable())).await
}

/// HTTPS with a TLS configuration that can be swapped while the server runs.
///
/// Keep a clone of `tls` and call
/// [`ReloadableTlsConfig::reload`] to rotate the certificate (or change any
/// other TLS setting); connections accepted afterwards use the new
/// configuration, established ones are unaffected.
pub async fn serve_tls_reloadable<F>(
    listener: TcpListener,
    handler: Arc<dyn HttpHandler>,
    shutdown: F,
    config: ServerConfig,
    tls: ReloadableTlsConfig,
) -> Result<(), HttpError>
where
    F: Future<Output = ()> + Send,
{
    serve_inner(listener, handler, shutdown, config, Some(tls)).await
}

async fn serve_inner<F>(
    listener: TcpListener,
    handler: Arc<dyn HttpHandler>,
    shutdown: F,
    config: ServerConfig,
    tls: Option<ReloadableTlsConfig>,
) -> Result<(), HttpError>
where
    F: Future<Output = ()> + Send,
{
    let config = Arc::new(config);
    // Cap concurrent connections: acquiring a permit *before* accept() also
    // stops us from pulling more sockets off the queue than we can serve. The
    // same semaphore drives shutdown draining below.
    let max_conns = config
        .max_connections
        .clamp(1, Semaphore::MAX_PERMITS.min(u32::MAX as usize));
    let limit = Arc::new(Semaphore::new(max_conns));
    // Signals kept-alive connections to close promptly on shutdown so draining
    // does not wait `drain_timeout` for *idle* keep-alive connections.
    let (drain_tx, drain_rx) = watch::channel(false);

    tokio::select! {
        r = accept_loop(listener, handler, config.clone(), limit.clone(), tls, drain_rx) => r,
        _ = shutdown => {
            // Tell idle keep-alive connections to stop, then wait for in-flight
            // requests to finish: acquiring *every* permit means all connection
            // tasks released their slot. Bounded by `drain_timeout`.
            let _ = drain_tx.send(true);
            let _ = timeout(
                config.drain_timeout,
                limit.acquire_many(max_conns as u32),
            )
            .await;
            Ok(())
        }
    }
}

async fn accept_loop(
    listener: TcpListener,
    handler: Arc<dyn HttpHandler>,
    config: Arc<ServerConfig>,
    limit: Arc<Semaphore>,
    tls: Option<ReloadableTlsConfig>,
    drain: watch::Receiver<bool>,
) -> Result<(), HttpError> {
    loop {
        let permit = limit
            .clone()
            .acquire_owned()
            .await
            .expect("connection semaphore never closed");
        let (sock, peer) = match listener.accept().await {
            Ok(pair) => pair,
            // A failed accept (e.g. EMFILE/ENFILE fd exhaustion, ECONNABORTED)
            // is transient: back off briefly and keep serving rather than
            // tearing down the whole listener.
            Err(e) => {
                drop(permit);
                config.report(None, &HttpError::Io(e));
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let _ = sock.set_nodelay(true);
        let handler = handler.clone();
        let config = config.clone();
        // Snapshot the TLS configuration now, so a reload affects exactly the
        // connections accepted after it.
        let tls = tls.as_ref().map(ReloadableTlsConfig::current);
        let drain = drain.clone();
        tokio::spawn(async move {
            let mut info = ConnInfo {
                peer_addr: Some(peer),
                local_addr: sock.local_addr().ok(),
                tls: tls.is_some(),
                peer_certs: Arc::new(Vec::new()),
            };
            // For HTTPS, complete the TLS handshake first (bounded by the head
            // timeout so a stalled handshake can't pin the connection).
            let conn: Conn = match &tls {
                Some(tls) => {
                    match timeout(config.head_timeout, transport::accept_tls(sock, tls)).await {
                        Ok(Ok((conn, certs))) => {
                            info.peer_certs = Arc::new(certs);
                            conn
                        }
                        Ok(Err(e)) => {
                            config.report(Some(peer), &e);
                            drop(permit);
                            return;
                        }
                        Err(_) => {
                            config.report(Some(peer), &HttpError::Timeout);
                            drop(permit);
                            return;
                        }
                    }
                }
                None => Box::new(sock) as Conn,
            };
            if let Err(e) = serve_conn(conn, handler, &config, drain, info).await {
                config.report(Some(peer), &e);
            }
            drop(permit); // release the slot only once the connection is done
        });
    }
}

/// What to do with the connection after one request.
enum After {
    /// Wait for the next request.
    KeepAlive,
    /// Close normally: the request was fully read.
    Close,
    /// Close after an early error response: request bytes may still be in
    /// flight, so drain briefly to let the response reach the client.
    Linger,
}

async fn serve_conn(
    conn: Conn,
    handler: Arc<dyn HttpHandler>,
    config: &ServerConfig,
    mut drain: watch::Receiver<bool>,
    info: ConnInfo,
) -> Result<(), HttpError> {
    let mut reader = BufReader::new(conn);
    let limits = config.head_limits();
    // HTTP/1.1 keep-alive: serve successive requests on the same connection.
    let mut served = 0usize;
    let mut linger = false;
    let result = loop {
        // If shutdown has already been signalled, close this idle connection.
        if *drain.borrow() {
            break Ok(());
        }
        // Wait for the first byte of the next request. A new connection gets
        // the head timeout; a kept-alive one the idle timeout.
        let idle = if served == 0 {
            config.head_timeout
        } else {
            config.idle_timeout
        };
        tokio::select! {
            r = timeout(idle, reader.fill_buf()) => match r {
                // Idle timeout or a clean close between requests is normal.
                Err(_) => break Ok(()),
                Ok(Ok([])) => break Ok(()),
                Ok(Ok(_)) => {}
                Ok(Err(e)) => break Err(e.into()),
            },
            // Server is draining for shutdown — close this (between-requests) idle
            // connection promptly instead of waiting out the idle timeout.
            _ = drain.changed() => break Ok(()),
        }

        let mut started = false;
        let read = io::read_request_head_with(&mut reader, &limits, &mut started);
        let head = match timeout(config.head_timeout, read).await {
            Ok(Ok(head)) => head,
            Ok(Err(HeadError::Closed)) => break Ok(()),
            Ok(Err(HeadError::Io(e))) => break Err(e.into()),
            Ok(Err(HeadError::Bad { status, reason })) => {
                // Malformed or over-limit: say why, then close. The framing of
                // whatever follows cannot be trusted.
                linger = true;
                let resp = HttpResponse::text(status, reason);
                let sent = write_response(&mut reader, resp, &CLOSE, config.write_timeout).await;
                break sent.and(Err(HttpError::Protocol(reason.into())));
            }
            Err(_) => {
                if started {
                    linger = true;
                    let resp = HttpResponse::text(StatusCode::REQUEST_TIMEOUT, "request timeout");
                    let _ = write_response(&mut reader, resp, &CLOSE, config.write_timeout).await;
                }
                break Err(HttpError::Timeout);
            }
        };
        served += 1;
        let last = served >= config.max_keep_alive_requests;
        match handle_one(&mut reader, &handler, config, &limits, head, &info, last).await {
            Ok(After::KeepAlive) => {}
            Ok(After::Close) => break Ok(()),
            Ok(After::Linger) => {
                linger = true;
                break Ok(());
            }
            Err(e) => break Err(e),
        }
    };
    // Close the write side cleanly (emits TLS `close_notify`).
    let _ = timeout(config.write_timeout, reader.shutdown()).await;
    // Closing a socket that still holds unread input makes the kernel reset
    // the connection, which can destroy a response the client has not read
    // yet. So when the client sent more than was consumed (pipelined requests
    // past the keep-alive cap, or the rest of a refused request), drain it.
    if linger || !reader.buffer().is_empty() {
        let _ = timeout(LINGER_TIMEOUT, discard_input(&mut reader)).await;
    }
    result
}

/// Reads and drops up to [`LINGER_BYTES`] of input, until EOF or an error.
async fn discard_input<R: AsyncBufRead + Unpin>(reader: &mut R) {
    let mut seen = 0usize;
    while seen <= LINGER_BYTES {
        match reader.fill_buf().await {
            Ok([]) | Err(_) => break,
            Ok(buf) => {
                let n = buf.len();
                reader.consume(n);
                seen += n;
            }
        }
    }
}

/// Sends an error response for a request that is refused before its body was
/// (fully) read, reports the refusal to the error hook, and asks for the
/// connection to be closed.
async fn refuse<W: AsyncWrite + Unpin>(
    w: &mut W,
    config: &ServerConfig,
    peer: Option<SocketAddr>,
    status: StatusCode,
    message: &str,
) -> Result<After, HttpError> {
    let error = match status.as_u16() {
        408 => HttpError::Timeout,
        413 => HttpError::BodyTooLarge(config.max_body_bytes as u64),
        _ => HttpError::Protocol(message.to_string()),
    };
    config.report(peer, &error);
    let resp = HttpResponse::text(status, message);
    write_response(w, resp, &CLOSE, config.write_timeout).await?;
    Ok(After::Linger)
}

/// Handles one request on a (possibly kept-alive) connection and says what to
/// do with the connection next. An I/O error writing the response propagates
/// and closes the connection.
async fn handle_one<R>(
    reader: &mut R,
    handler: &Arc<dyn HttpHandler>,
    config: &ServerConfig,
    limits: &HeadLimits,
    head: RequestHead,
    info: &ConnInfo,
    last: bool,
) -> Result<After, HttpError>
where
    R: AsyncBufRead + AsyncWrite + Unpin,
{
    let RequestHead {
        method,
        target,
        authority,
        version,
        headers,
    } = head;
    let http10 = version == Version::Http10;
    let bad = StatusCode::BAD_REQUEST;
    let peer = info.peer_addr;

    // RFC 9112 §3.2: an HTTP/1.1 request carries exactly one Host header.
    let hosts = headers.get_all("host").count();
    if hosts > 1 || (hosts == 0 && !http10) {
        let message = "missing or duplicate Host header";
        return refuse(reader, config, peer, bad, message).await;
    }

    // Request-smuggling guards. Every framing a front-end proxy and this
    // server could read differently is refused, and the connection closed:
    // an invalid or conflicting `Content-Length`, a `Transfer-Encoding` that
    // is not exactly one `chunked` (across *all* its header lines), both
    // headers together (CL.TE / TE.CL), and chunked from an HTTP/1.0 client.
    let content_length = match headers.content_length_checked() {
        Ok(cl) => cl,
        Err(_) => return refuse(reader, config, peer, bad, "invalid Content-Length").await,
    };
    let chunked = match headers.transfer_coding() {
        TransferCoding::Absent => false,
        TransferCoding::Chunked => true,
        TransferCoding::Unsupported => {
            let status = StatusCode::NOT_IMPLEMENTED;
            let message = "unsupported Transfer-Encoding";
            return refuse(reader, config, peer, status, message).await;
        }
    };
    if chunked && content_length.is_some() {
        let message = "conflicting Content-Length and Transfer-Encoding";
        return refuse(reader, config, peer, bad, message).await;
    }
    if chunked && http10 {
        let message = "Transfer-Encoding in an HTTP/1.0 request";
        return refuse(reader, config, peer, bad, message).await;
    }

    // Reject oversized bodies up front — before any allocation and before
    // inviting the client to send the body.
    let declared = content_length.unwrap_or(0);
    if declared > config.max_body_bytes as u64 {
        let status = StatusCode::PAYLOAD_TOO_LARGE;
        return refuse(reader, config, peer, status, "payload too large").await;
    }

    // Honor `Expect: 100-continue`: acknowledge before reading the body so a
    // client withholding a large upload knows to proceed (avoiding its stall).
    // Sent only now, after the size check, so an oversized upload is refused
    // without being invited.
    if let Some(expect) = headers.get("expect") {
        if !expect.trim().eq_ignore_ascii_case("100-continue") {
            let status = StatusCode::EXPECTATION_FAILED;
            return refuse(reader, config, peer, status, "unsupported Expect").await;
        }
        if (chunked || declared > 0) && !http10 {
            write_all_timed(
                reader,
                b"HTTP/1.1 100 Continue\r\n\r\n",
                config.write_timeout,
            )
            .await?;
            flush_timed(reader, config.write_timeout).await?;
        }
    }

    let read = async {
        if chunked {
            read_chunked_body(reader, config.max_body_bytes, limits).await
        } else {
            read_sized_body(reader, declared as usize).await
        }
    };
    let body = match timeout(config.body_timeout, read).await {
        Ok(Ok(body)) => body,
        Ok(Err(HttpError::BodyTooLarge(_))) => {
            let status = StatusCode::PAYLOAD_TOO_LARGE;
            return refuse(reader, config, peer, status, "payload too large").await;
        }
        // The reasons are fixed strings from the chunk parser, never an echo
        // of what the client sent.
        Ok(Err(HttpError::Protocol(reason))) => {
            return refuse(reader, config, peer, bad, &reason).await;
        }
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            let status = StatusCode::REQUEST_TIMEOUT;
            return refuse(reader, config, peer, status, "request timeout").await;
        }
    };

    let (mut path, query) = split_target(&target);
    if config.normalize_paths && target.starts_with('/') {
        path = match normalize_path(&path) {
            Ok(path) => path,
            // The body was read in full, so the connection stays usable.
            Err(_) => {
                config.report(peer, &HttpError::Protocol("invalid request path".into()));
                let resp = HttpResponse::text(bad, "invalid request path");
                let opts = WriteOpts {
                    head_request: matches!(method, Method::Head),
                    keep_alive: false,
                    http10,
                };
                write_response(reader, resp, &opts, config.write_timeout).await?;
                return Ok(After::Close);
            }
        };
    }
    // An absolute-form target names the host itself and overrides `Host`.
    let host = authority.unwrap_or_else(|| headers.get("host").unwrap_or("").to_string());
    let uri = Uri {
        scheme: String::new(),
        host,
        port: 0,
        path,
        query,
    };
    let wants_keep_alive = config.keep_alive
        && !last
        && if http10 {
            headers.connection_has("keep-alive") && !headers.connection_has("close")
        } else {
            !headers.connection_has("close")
        };
    let head_request = matches!(method, Method::Head);
    let mut req = Request::new(method, uri);
    req.headers = headers;
    req.body = body;
    req.set_conn_info(info.clone());

    let resp = match run_handler(handler, req, config.handler_timeout).await {
        Ok(resp) => resp,
        Err(e) => {
            config.report(info.peer_addr, &e);
            match e {
                HttpError::Timeout => {
                    HttpResponse::text(StatusCode::SERVICE_UNAVAILABLE, "handler timed out")
                }
                _ => HttpResponse::text(StatusCode::INTERNAL_SERVER_ERROR, "internal server error"),
            }
        }
    };

    // A streamed body cannot be delimited for an HTTP/1.0 client except by
    // closing, and a handler may ask for the connection to be closed.
    let keep_alive = wants_keep_alive
        && !resp.headers.connection_has("close")
        && !(http10 && resp.stream.is_some());
    let opts = WriteOpts {
        head_request,
        keep_alive,
        http10,
    };
    write_response(reader, resp, &opts, config.write_timeout).await?;
    Ok(if keep_alive {
        After::KeepAlive
    } else {
        After::Close
    })
}

/// Runs the handler with panic isolation and the optional deadline.
/// `Err(HandlerPanic)` = it panicked; `Err(Timeout)` = it ran out of time.
async fn run_handler(
    handler: &Arc<dyn HttpHandler>,
    req: Request,
    limit: Option<Duration>,
) -> Result<HttpResponse, HttpError> {
    // Creating the future can panic too (the synchronous part of `handle`).
    let future = std::panic::catch_unwind(AssertUnwindSafe(|| handler.handle(req)))
        .map_err(|payload| HttpError::HandlerPanic(panic_message(payload)))?;
    let guarded = CatchUnwind(future);
    let outcome = match limit {
        Some(limit) => match timeout(limit, guarded).await {
            Ok(outcome) => outcome,
            Err(_) => return Err(HttpError::Timeout),
        },
        None => guarded.await,
    };
    outcome.map_err(HttpError::HandlerPanic)
}

/// Polls a handler future, turning a panic into an error instead of letting
/// it unwind through the connection task.
struct CatchUnwind<'a>(BoxFuture<'a, HttpResponse>);

impl Future for CatchUnwind<'_> {
    type Output = Result<HttpResponse, String>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let future = self.0.as_mut();
        match std::panic::catch_unwind(AssertUnwindSafe(|| future.poll(cx))) {
            Ok(Poll::Ready(resp)) => Poll::Ready(Ok(resp)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(payload) => Poll::Ready(Err(panic_message(payload))),
        }
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Reads exactly `len` bytes of body. The buffer grows with the data that
/// actually arrives, so a declared-but-unsent body costs no memory.
async fn read_sized_body<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    len: usize,
) -> Result<Vec<u8>, HttpError> {
    let mut body = Vec::with_capacity(len.min(WRITE_SLICE_BYTES));
    while body.len() < len {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Err(HttpError::Io(std::io::ErrorKind::UnexpectedEof.into()));
        }
        let n = available.len().min(len - body.len());
        body.extend_from_slice(&available[..n]);
        reader.consume(n);
    }
    Ok(body)
}

/// Reads and decodes a `Transfer-Encoding: chunked` request body, capped at
/// `max` bytes ([`HttpError::BodyTooLarge`] beyond that, raised as soon as a
/// chunk-size line declares too much). Chunk-size lines and the trailer
/// section are bounded too; malformed framing is [`HttpError::Protocol`].
async fn read_chunked_body<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max: usize,
    limits: &HeadLimits,
) -> Result<Vec<u8>, HttpError> {
    let mut decoder = ChunkedReader::new(Some(max as u64));
    let mut body = Vec::new();
    while let Some(piece) = decoder.next(reader, limits).await? {
        body.extend_from_slice(&piece);
    }
    Ok(body)
}

/// How one response is framed and what it says about the connection.
struct WriteOpts {
    /// The request was `HEAD`: send the head only.
    head_request: bool,
    /// Advertise (and honour) keep-alive.
    keep_alive: bool,
    /// The client speaks HTTP/1.0: never use chunked transfer encoding.
    http10: bool,
}

/// Options for an error response that closes the connection.
const CLOSE: WriteOpts = WriteOpts {
    head_request: false,
    keep_alive: false,
    http10: false,
};

async fn write_all_timed<W: AsyncWrite + Unpin>(
    w: &mut W,
    data: &[u8],
    limit: Duration,
) -> Result<(), HttpError> {
    // In slices, so a large body sent to a slow-but-reading client is not
    // mistaken for a stalled one: the timeout bounds each slice.
    for slice in data.chunks(WRITE_SLICE_BYTES) {
        match timeout(limit, w.write_all(slice)).await {
            Ok(result) => result?,
            Err(_) => return Err(HttpError::Timeout),
        }
    }
    Ok(())
}

async fn flush_timed<W: AsyncWrite + Unpin>(w: &mut W, limit: Duration) -> Result<(), HttpError> {
    match timeout(limit, w.flush()).await {
        Ok(result) => Ok(result?),
        Err(_) => Err(HttpError::Timeout),
    }
}

async fn write_response<W: AsyncWrite + Unpin>(
    w: &mut W,
    mut resp: HttpResponse,
    opts: &WriteOpts,
    write_timeout: Duration,
) -> Result<(), HttpError> {
    // Responses to HEAD, and 1xx/204/304, carry no message body — writing one
    // would desync framing on a kept-alive connection.
    let code = resp.status.as_u16();
    let no_content = matches!(code, 204 | 304) || (100..200).contains(&code);
    let bodyless = opts.head_request || no_content;
    let streaming = resp.stream.is_some() && !bodyless;
    // Chunked needs HTTP/1.1; for a 1.0 client the stream ends at close.
    let chunked = streaming && !opts.http10;

    // Only a HEAD response may carry a handler-chosen Content-Length.
    let keep_length = opts.head_request && !no_content;

    let mut head = String::with_capacity(256);
    head.push_str(&format!("HTTP/1.1 {} {}\r\n", code, resp.status.reason()));
    for (k, v) in resp.headers.iter() {
        // The server owns the framing headers; see `HttpResponse`.
        if k.eq_ignore_ascii_case("transfer-encoding") {
            continue;
        }
        if k.eq_ignore_ascii_case("content-length") && !keep_length {
            continue;
        }
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if !resp.headers.contains("date") {
        head.push_str(&format!(
            "Date: {}\r\n",
            format_http_date(SystemTime::now())
        ));
    }
    if chunked {
        head.push_str("Transfer-Encoding: chunked\r\n");
    } else if !streaming && !no_content {
        // HEAD still advertises the entity length (with no body); 1xx/204/304
        // omit Content-Length entirely.
        let advertised = opts.head_request && resp.headers.contains("content-length");
        if !advertised {
            head.push_str(&format!("Content-Length: {}\r\n", resp.body.len()));
        }
    }
    if !resp.headers.contains("connection") {
        head.push_str(if opts.keep_alive {
            "Connection: keep-alive\r\n"
        } else {
            "Connection: close\r\n"
        });
    }
    head.push_str("\r\n");

    let mut first = head.into_bytes();
    if bodyless {
        // Headers only — drop any stream/body the handler may have set.
        write_all_timed(w, &first, write_timeout).await?;
    } else if let Some(mut chunks) = resp.stream.take() {
        write_all_timed(w, &first, write_timeout).await?;
        flush_timed(w, write_timeout).await?;
        // Waiting for the handler's next chunk is not timed (an event stream
        // may be quiet for long stretches); delivering each chunk is.
        while let Some(chunk) = chunks.recv().await {
            if chunk.is_empty() {
                continue;
            }
            if chunked {
                let mut frame = Vec::with_capacity(chunk.len() + 16);
                frame.extend_from_slice(format!("{:X}\r\n", chunk.len()).as_bytes());
                frame.extend_from_slice(&chunk);
                frame.extend_from_slice(b"\r\n");
                write_all_timed(w, &frame, write_timeout).await?;
            } else {
                write_all_timed(w, &chunk, write_timeout).await?;
            }
            // Flush per chunk so SSE clients see events live.
            flush_timed(w, write_timeout).await?;
        }
        if chunked {
            write_all_timed(w, b"0\r\n\r\n", write_timeout).await?;
        }
    } else if resp.body.len() <= WRITE_SLICE_BYTES {
        // One write for small responses: head and body in one segment.
        first.extend_from_slice(&resp.body);
        write_all_timed(w, &first, write_timeout).await?;
    } else {
        write_all_timed(w, &first, write_timeout).await?;
        write_all_timed(w, &resp.body, write_timeout).await?;
    }
    flush_timed(w, write_timeout).await
}

#[cfg(test)]
mod tls_tests {
    use super::super::TlsConfig;
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    const CERT: &str = include_str!("testdata/self_signed_cert.pem");
    const KEY: &str = include_str!("testdata/self_signed_key.pem");

    struct Ok200;
    impl HttpHandler for Ok200 {
        fn handle(&self, _req: Request) -> BoxFuture<'_, HttpResponse> {
            Box::pin(async { HttpResponse::text(StatusCode::OK, "secure ok") })
        }
    }

    #[test]
    fn from_pem_parses_and_rejects_garbage() {
        assert!(TlsConfig::from_pem(CERT.as_bytes(), KEY.as_bytes()).is_ok());
        assert!(TlsConfig::from_pem(b"not a pem", KEY.as_bytes()).is_err());
        assert!(TlsConfig::from_pem(CERT.as_bytes(), b"no key block").is_err());
    }

    async fn spawn_tls(tls: TlsConfig) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = serve_tls(listener, Arc::new(Ok200), std::future::pending::<()>(), tls).await;
        });
        addr
    }

    /// Performs one HTTPS GET with a raw rustls client that trusts the
    /// self-signed test certificate and offers only `versions`.
    async fn tls_get(
        addr: std::net::SocketAddr,
        versions: &[&'static rustls::SupportedProtocolVersion],
    ) -> Result<String, String> {
        let mut roots = rustls::RootCertStore::empty();
        for (label, der) in transport::pem_blocks(CERT.as_bytes()) {
            if label == "CERTIFICATE" {
                roots
                    .add(rustls_pki_types::CertificateDer::from(der))
                    .unwrap();
            }
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let client_cfg = Arc::new(
            rustls::ClientConfig::builder_with_provider(provider)
                .with_protocol_versions(versions)
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );
        let connector = tokio_rustls::TlsConnector::from(client_cfg);
        let tcp = TcpStream::connect(addr).await.unwrap();
        let server_name = rustls_pki_types::ServerName::try_from("localhost").unwrap();
        let mut stream = connector
            .connect(server_name, tcp)
            .await
            .map_err(|e| e.to_string())?;
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .map_err(|e| e.to_string())?;
        stream.flush().await.map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        stream
            .read_to_end(&mut buf)
            .await
            .map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    #[tokio::test]
    async fn serve_tls_completes_handshake_and_serves() {
        let tls = TlsConfig::from_pem(CERT.as_bytes(), KEY.as_bytes()).unwrap();
        let addr = spawn_tls(tls).await;
        // The default server accepts TLS 1.2 and TLS 1.3 clients.
        for versions in [
            &[&rustls::version::TLS12][..],
            &[&rustls::version::TLS13][..],
        ] {
            let resp = tls_get(addr, versions).await.unwrap();
            assert!(resp.starts_with("HTTP/1.1 200"), "resp={resp}");
            assert!(resp.trim_end().ends_with("secure ok"), "resp={resp}");
        }
    }

    #[tokio::test]
    async fn server_minimum_tls_version_is_enforced() {
        let tls = TlsConfig::builder()
            .cert_pem(CERT.as_bytes(), KEY.as_bytes())
            .unwrap()
            .min_version(super::super::TlsVersion::Tls13)
            .build()
            .unwrap();
        let addr = spawn_tls(tls).await;
        // A TLS 1.2-only client cannot complete the handshake...
        assert!(tls_get(addr, &[&rustls::version::TLS12]).await.is_err());
        // ...while a TLS 1.3 client is served.
        let resp = tls_get(addr, &[&rustls::version::TLS13]).await.unwrap();
        assert!(resp.starts_with("HTTP/1.1 200"), "resp={resp}");
    }

    #[tokio::test]
    async fn client_minimum_tls_version_is_enforced() {
        use super::super::{Client, ClientTlsConfig, TlsVersion};

        // A server that only speaks TLS 1.2, built directly on the engine.
        let certs: Vec<_> = transport::pem_blocks(CERT.as_bytes())
            .into_iter()
            .filter(|(label, _)| label == "CERTIFICATE")
            .map(|(_, der)| rustls_pki_types::CertificateDer::from(der))
            .collect();
        let key = transport::pem_blocks(KEY.as_bytes()).remove(0).1;
        let key = rustls_pki_types::PrivateKeyDer::try_from(key).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let engine = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS12])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap();
        let addr = spawn_tls(TlsConfig(Arc::new(engine))).await;
        let url = format!("https://localhost:{}/", addr.port());
        let trust = ClientTlsConfig::new()
            .without_web_pki_roots()
            .add_root_pem(CERT.as_bytes())
            .unwrap();

        // The default minimum (TLS 1.2) connects.
        let client = Client::builder().tls(trust.clone()).build().unwrap();
        let req = Request::builder(Method::Get, &url).unwrap().build();
        let body = client.send(req).await.unwrap().text().await.unwrap();
        assert_eq!(body, "secure ok");

        // Requiring TLS 1.3 refuses the same server.
        let client = Client::builder()
            .tls(trust.min_version(TlsVersion::Tls13))
            .build()
            .unwrap();
        let req = Request::builder(Method::Get, &url).unwrap().build();
        let err = client.send(req).await.unwrap_err();
        assert!(matches!(err, HttpError::Tls(_)), "{err:?}");
    }
    #[tokio::test]
    async fn response_framing_headers_are_owned_by_the_server() {
        async fn render(resp: HttpResponse, opts: WriteOpts) -> String {
            let mut out = Vec::new();
            write_response(&mut out, resp, &opts, Duration::from_secs(5))
                .await
                .unwrap();
            String::from_utf8(out).unwrap()
        }
        let plain = |head_request, keep_alive, http10| WriteOpts {
            head_request,
            keep_alive,
            http10,
        };

        // A handler-supplied Content-Length / Transfer-Encoding cannot desync
        // the response.
        let resp = HttpResponse::text(StatusCode::OK, "hello")
            .header("content-length", "999")
            .header("transfer-encoding", "chunked");
        let out = render(resp, plain(false, true, false)).await;
        assert!(out.contains("Content-Length: 5\r\n"), "{out}");
        assert!(!out.contains("999") && !out.to_lowercase().contains("transfer-encoding"));
        assert!(out.contains("Connection: keep-alive\r\n"));
        assert!(out.contains("\r\nDate: ") && out.contains(" GMT\r\n"));
        assert!(out.ends_with("\r\n\r\nhello"));

        // HEAD keeps the advertised entity length and sends no body.
        let resp = HttpResponse::new(StatusCode::OK).header("content-length", "1234");
        let out = render(resp, plain(true, false, false)).await;
        assert!(out.contains("content-length: 1234\r\n"), "{out}");
        assert!(out.ends_with("Connection: close\r\n\r\n"));

        // 204 never carries a length or a body.
        let resp = HttpResponse::new(StatusCode::NO_CONTENT).body(b"ignored".to_vec());
        let out = render(resp, plain(false, true, false)).await;
        assert!(!out.to_lowercase().contains("content-length"), "{out}");
        assert!(out.ends_with("\r\n\r\n"));

        // A stream is chunked for HTTP/1.1 and close-delimited for HTTP/1.0.
        for (http10, expect_body) in [(false, "2\r\nhi\r\n0\r\n\r\n"), (true, "hi")] {
            let (tx, rx) = mpsc::channel(4);
            tx.send(b"hi".to_vec()).await.unwrap();
            drop(tx);
            let resp = HttpResponse::stream(StatusCode::OK, rx).header("date", "fixed");
            let out = render(resp, plain(false, false, http10)).await;
            assert_eq!(out.contains("Transfer-Encoding: chunked"), !http10, "{out}");
            assert!(!out.contains("Content-Length"), "{out}");
            assert!(out.contains("date: fixed\r\n") && !out.contains("GMT"));
            assert!(out.ends_with(&format!("\r\n\r\n{expect_body}")), "{out}");
        }
    }
}

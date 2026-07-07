//! A minimal HTTP/1.1 server: accept loop, request parsing, and a handler trait.
//!
//! The routing/handler ergonomics live in [`crate::turbo`]; this module provides
//! the raw serving primitive that a [`HttpHandler`] plugs into.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch, Semaphore};
use tokio::time::timeout;

use super::core::split_target;
use super::message::Request;
use super::transport::{self, Conn};
use super::{io, HeaderMap, HttpError, Method, StatusCode, Uri};

/// A boxed, `Send` future — the manual `async fn`-in-trait desugaring so
/// [`HttpHandler`] stays object-safe.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Resource limits applied to each served connection. The defaults are safe for
/// direct exposure; tune them with [`serve_with_config`].
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Maximum accepted request-body size (`Content-Length`); larger requests
    /// are rejected with `413 Payload Too Large` before any allocation.
    pub max_body_bytes: usize,
    /// Maximum number of connections served concurrently. Further connections
    /// are not accepted until an in-flight one completes (backpressure).
    pub max_connections: usize,
    /// Deadline for reading the request line + headers. Bounds slow-loris
    /// attacks that trickle the head one byte at a time.
    pub head_timeout: Duration,
    /// Deadline for reading the request body once its length is known.
    pub body_timeout: Duration,
    /// On shutdown, how long to wait for in-flight connections to finish before
    /// returning (connection draining for graceful rolling deploys).
    pub drain_timeout: Duration,
    /// Maximum number of requests served on a single kept-alive connection
    /// before it is closed. Bounds per-connection resource lifetime. The
    /// `head_timeout` doubles as the keep-alive idle timeout between requests.
    pub max_keep_alive_requests: usize,
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
        }
    }
}

/// An owned outgoing HTTP response produced by a handler.
///
/// The body is either the fixed [`body`](Self::body) bytes (written with
/// `Content-Length`) or a chunked stream set via [`stream`](Self::stream) /
/// [`event_stream`](Self::event_stream) (written with `Transfer-Encoding:
/// chunked`, flushed per chunk).
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
    /// from `events` framed as `data: <event>\n\n` and flushed as it arrives.
    ///
    /// For multi-field events (`id`/`event`/`retry`) use [`sse`](Self::sse).
    pub fn event_stream(mut events: mpsc::Receiver<String>) -> Self {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(16);
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let frame = format!("data: {event}\n\n").into_bytes();
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
    /// Each event is serialized with
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
/// `TlsConfig::from_pem`/`from_der`.
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
    let acceptor = tokio_rustls::TlsAcceptor::from(tls.0);
    serve_inner(listener, handler, shutdown, config, Some(acceptor)).await
}

async fn serve_inner<F>(
    listener: TcpListener,
    handler: Arc<dyn HttpHandler>,
    shutdown: F,
    config: ServerConfig,
    tls: Option<tokio_rustls::TlsAcceptor>,
) -> Result<(), HttpError>
where
    F: Future<Output = ()> + Send,
{
    let config = Arc::new(config);
    // Cap concurrent connections: acquiring a permit *before* accept() also
    // stops us from pulling more sockets off the queue than we can serve. The
    // same semaphore drives shutdown draining below.
    let max_conns = config.max_connections.max(1);
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
    tls: Option<tokio_rustls::TlsAcceptor>,
    drain: watch::Receiver<bool>,
) -> Result<(), HttpError> {
    loop {
        let permit = limit
            .clone()
            .acquire_owned()
            .await
            .expect("connection semaphore never closed");
        let (sock, _addr) = match listener.accept().await {
            Ok(pair) => pair,
            // A failed accept (e.g. EMFILE/ENFILE fd exhaustion, ECONNABORTED)
            // is transient: back off briefly and keep serving rather than
            // tearing down the whole listener.
            Err(_e) => {
                drop(permit);
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let _ = sock.set_nodelay(true);
        let handler = handler.clone();
        let config = config.clone();
        let tls = tls.clone();
        let drain = drain.clone();
        tokio::spawn(async move {
            // For HTTPS, complete the TLS handshake first (bounded by the head
            // timeout so a stalled handshake can't pin the connection).
            let conn: Conn = match &tls {
                Some(acceptor) => {
                    match timeout(config.head_timeout, transport::accept_tls(sock, acceptor)).await
                    {
                        Ok(Ok(c)) => c,
                        _ => {
                            drop(permit);
                            return;
                        }
                    }
                }
                None => Box::new(sock) as Conn,
            };
            let _ = serve_conn(conn, handler, &config, drain).await;
            drop(permit); // release the slot only once the connection is done
        });
    }
}

async fn serve_conn(
    conn: Conn,
    handler: Arc<dyn HttpHandler>,
    config: &ServerConfig,
    mut drain: watch::Receiver<bool>,
) -> Result<(), HttpError> {
    let mut reader = BufReader::new(conn);
    // HTTP/1.1 keep-alive: serve successive requests on the same connection.
    // The head-read timeout doubles as the idle timeout between requests.
    let mut served = 0usize;
    loop {
        // If shutdown has already been signalled, close this idle connection.
        if *drain.borrow() {
            break;
        }
        let head = tokio::select! {
            r = timeout(config.head_timeout, io::read_request_head(&mut reader)) => match r {
                Ok(Ok(h)) => h,
                // Idle/head timeout, EOF, or a malformed request line closes the
                // connection (a peer that closed cleanly between requests is normal).
                _ => break,
            },
            // Server is draining for shutdown — close this (between-requests) idle
            // connection promptly instead of waiting out the idle timeout.
            _ = drain.changed() => break,
        };
        served += 1;
        let keep_alive = match handle_one(&mut reader, &handler, config, head, served).await? {
            true => served < config.max_keep_alive_requests,
            false => false,
        };
        if !keep_alive {
            break;
        }
    }
    // Close the write side cleanly (emits TLS `close_notify`).
    let _ = reader.shutdown().await;
    Ok(())
}

/// Handles one request on a (possibly kept-alive) connection. Returns whether
/// the *request* permits keep-alive (the caller also enforces the per-connection
/// request cap). An I/O error writing the response propagates and closes the
/// connection.
async fn handle_one<R>(
    reader: &mut R,
    handler: &Arc<dyn HttpHandler>,
    config: &ServerConfig,
    head: (Method, String, HeaderMap),
    served: usize,
) -> Result<bool, HttpError>
where
    R: AsyncBufReadExt + AsyncWriteExt + Unpin,
{
    let (method, target, headers) = head;
    let req_keep_alive = wants_keep_alive(&headers);

    // Request-smuggling guard: reject conflicting/duplicate `Content-Length`,
    // and reject a message that carries both `Transfer-Encoding: chunked` and a
    // `Content-Length` (CL.TE / TE.CL desync when fronted by a proxy). These
    // ambiguous framings force the connection closed.
    let chunked = headers.is_chunked();
    let content_length = match headers.content_length_checked() {
        Ok(cl) => cl,
        Err(_) => {
            let resp = HttpResponse::text(StatusCode::BAD_REQUEST, "invalid Content-Length");
            write_response(reader, resp, method, false).await?;
            return Ok(false);
        }
    };
    if chunked && content_length.is_some() {
        let resp = HttpResponse::text(
            StatusCode::BAD_REQUEST,
            "conflicting Content-Length and Transfer-Encoding",
        );
        write_response(reader, resp, method, false).await?;
        return Ok(false);
    }

    // Honor `Expect: 100-continue`: acknowledge before reading the body so a
    // client withholding a large upload knows to proceed (avoiding its stall).
    let has_body = chunked || content_length.unwrap_or(0) > 0;
    if has_body && expects_continue(&headers) {
        reader.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await?;
        reader.flush().await?;
    }

    let body = if chunked {
        // Dechunk the request body (bounded by the same body cap).
        match timeout(
            config.body_timeout,
            read_chunked_body(reader, config.max_body_bytes),
        )
        .await
        .map_err(|_| HttpError::Timeout)??
        {
            Some(b) => b,
            None => {
                let resp = HttpResponse::text(StatusCode::PAYLOAD_TOO_LARGE, "payload too large");
                write_response(reader, resp, method, false).await?;
                return Ok(false);
            }
        }
    } else {
        match content_length {
            Some(len) => {
                // Reject oversized bodies up front — never allocate an
                // attacker-controlled `Content-Length`.
                if len as usize > config.max_body_bytes {
                    let resp =
                        HttpResponse::text(StatusCode::PAYLOAD_TOO_LARGE, "payload too large");
                    write_response(reader, resp, method, false).await?;
                    return Ok(false);
                }
                let mut buf = vec![0u8; len as usize];
                timeout(config.body_timeout, reader.read_exact(&mut buf))
                    .await
                    .map_err(|_| HttpError::Timeout)??;
                buf
            }
            None => Vec::new(),
        }
    };

    let (path, query) = split_target(&target);
    let host = headers.get("host").unwrap_or("").to_string();
    let uri = Uri {
        scheme: String::new(),
        host,
        port: 0,
        path,
        query,
    };
    let req = Request {
        method: method.clone(),
        uri,
        headers,
        body,
    };

    let resp = handler.handle(req).await;
    // The response is written with the keep-alive decision this request allows;
    // the connection cap is applied by the caller.
    let keep = req_keep_alive && served < config.max_keep_alive_requests;
    write_response(reader, resp, method, keep).await?;
    Ok(req_keep_alive)
}

/// Whether the request carries `Expect: 100-continue`.
fn expects_continue(headers: &HeaderMap) -> bool {
    headers
        .get("expect")
        .map(|v| v.trim().eq_ignore_ascii_case("100-continue"))
        .unwrap_or(false)
}

/// Whether the request's `Connection` header permits keep-alive. Absent header
/// defaults to keep-alive (HTTP/1.1); an explicit `close` token disables it.
fn wants_keep_alive(headers: &HeaderMap) -> bool {
    match headers.get("connection") {
        Some(v) => !v
            .to_ascii_lowercase()
            .split(',')
            .any(|t| t.trim() == "close"),
        None => true,
    }
}

/// Reads and decodes a `Transfer-Encoding: chunked` request body, capped at
/// `max` bytes. Returns `Ok(Some(body))` on success, `Ok(None)` if the decoded
/// body would exceed `max` (caller replies 413), and `Err` on malformed framing
/// or I/O error.
async fn read_chunked_body<R: AsyncBufReadExt + AsyncReadExt + Unpin>(
    reader: &mut R,
    max: usize,
) -> Result<Option<Vec<u8>>, HttpError> {
    let mut body = Vec::new();
    loop {
        let mut size_line = String::new();
        let n = reader.read_line(&mut size_line).await?;
        if n == 0 {
            return Err(HttpError::Protocol("unexpected EOF in chunked body".into()));
        }
        let size_str = size_line.trim().split(';').next().unwrap_or("").trim();
        let size = u64::from_str_radix(size_str, 16)
            .map_err(|_| HttpError::Protocol(format!("bad chunk size: {size_str}")))?
            as usize;
        if size == 0 {
            // Consume any trailers up to the terminating blank line.
            loop {
                let mut l = String::new();
                let n = reader.read_line(&mut l).await?;
                if n == 0 || l.trim().is_empty() {
                    break;
                }
            }
            return Ok(Some(body));
        }
        if body.len().saturating_add(size) > max {
            return Ok(None);
        }
        let mut buf = vec![0u8; size];
        reader.read_exact(&mut buf).await?;
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf).await?;
        body.extend_from_slice(&buf);
    }
}

async fn write_response<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    mut resp: HttpResponse,
    method: Method,
    keep_alive: bool,
) -> Result<(), HttpError> {
    // Responses to HEAD, and 204/304, carry no message body — writing one would
    // desync framing on a kept-alive connection.
    let no_content = matches!(resp.status.as_u16(), 204 | 304);
    let bodyless = matches!(method, Method::Head) || no_content;
    let streaming = resp.stream.is_some() && !bodyless;

    let mut head = String::new();
    head.push_str(&format!(
        "HTTP/1.1 {} {}\r\n",
        resp.status.as_u16(),
        resp.status.reason()
    ));
    for (k, v) in resp.headers.iter() {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if streaming {
        if !resp.headers.contains("transfer-encoding") {
            head.push_str("Transfer-Encoding: chunked\r\n");
        }
    } else if !resp.headers.contains("content-length") && !no_content {
        // HEAD still advertises the entity length (with no body); 204/304 omit
        // Content-Length entirely.
        head.push_str(&format!("Content-Length: {}\r\n", resp.body.len()));
    }
    if !resp.headers.contains("connection") {
        head.push_str(if keep_alive {
            "Connection: keep-alive\r\n"
        } else {
            "Connection: close\r\n"
        });
    }
    head.push_str("\r\n");
    w.write_all(head.as_bytes()).await?;

    if bodyless {
        // Headers only — drop any stream/body the handler may have set.
    } else if let Some(mut chunks) = resp.stream.take() {
        while let Some(chunk) = chunks.recv().await {
            if chunk.is_empty() {
                continue;
            }
            w.write_all(format!("{:X}\r\n", chunk.len()).as_bytes())
                .await?;
            w.write_all(&chunk).await?;
            w.write_all(b"\r\n").await?;
            w.flush().await?; // flush per chunk so SSE clients see events live
        }
        w.write_all(b"0\r\n\r\n").await?;
    } else {
        w.write_all(&resp.body).await?;
    }
    w.flush().await?;
    Ok(())
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

    #[tokio::test]
    async fn serve_tls_completes_handshake_and_serves() {
        let tls = TlsConfig::from_pem(CERT.as_bytes(), KEY.as_bytes()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = serve_tls(
                listener,
                Arc::new(Ok200),
                async move {
                    let _ = rx.await;
                },
                tls,
            )
            .await;
        });

        // A client that trusts our self-signed certificate.
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
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );
        let connector = tokio_rustls::TlsConnector::from(client_cfg);
        let tcp = TcpStream::connect(addr).await.unwrap();
        let server_name = rustls_pki_types::ServerName::try_from("localhost").unwrap();
        let mut stream = connector.connect(server_name, tcp).await.unwrap();

        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        stream.flush().await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let resp = String::from_utf8_lossy(&buf);
        assert!(resp.starts_with("HTTP/1.1 200"), "resp={resp}");
        assert!(resp.trim_end().ends_with("secure ok"), "resp={resp}");
        let _ = tx.send(());
    }
}

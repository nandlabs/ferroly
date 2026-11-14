//! An HTTP/1.1 client over the transport layer, with keep-alive connection
//! pooling: after a response body is fully drained, its connection is returned
//! to a per-host pool and reused by the next request to the same `host:port`,
//! avoiding a fresh TCP + TLS handshake each time.
//!
//! [`Client::new`] gives safe defaults; [`Client::builder`] exposes the
//! timeouts, the response-body cap, the pool, TLS, proxies, redirects,
//! decompression and interceptors.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWriteExt, BufReader};
use tokio::time::Instant;

use super::inflate::Decoder;
use super::io::{self, BodyEncoding, HeadLimits, RequestWire, Version};
use super::message::{Body, BodyLimits, Framing, Reclaim, Request, Response};
use super::pool::{Pool, PoolKey};
use super::proxy::{Proxy, ProxyServer, Route};
use super::transport::{self, ClientTlsConfig, Conn};
use super::{HeaderMap, HttpError, Method, StatusCode, Uri};

/// The `User-Agent` sent when the request does not carry one.
const DEFAULT_USER_AGENT: &str = concat!("ferroly/", env!("CARGO_PKG_VERSION"));
/// Default cap on a response body: 64 MiB.
const DEFAULT_MAX_RESPONSE_BODY: u64 = 64 * 1024 * 1024;
/// Most 1xx interim responses skipped before the final response.
const MAX_INTERIM_RESPONSES: usize = 16;
/// A redirect response body up to this size is drained so the connection can
/// be reused; a larger one closes the connection instead.
const REDIRECT_DRAIN_BYTES: u64 = 64 * 1024;

/// A body streamed from an async reader.
type BodyStream<'a> = &'a mut (dyn AsyncRead + Send + Unpin);

/// Observes and adjusts the requests a [`Client`] sends — for example to
/// inject trace or auth headers, or to record metrics.
///
/// Hooks run in the order the interceptors were added. They are synchronous
/// and should be quick. With redirect following enabled they run once per hop.
///
/// ```
/// use ferroly::http::{Client, HeaderMap, HttpError, Interceptor, Request, StatusCode};
///
/// struct TraceId;
/// impl Interceptor for TraceId {
///     fn on_request(&self, req: &mut Request) -> Result<(), HttpError> {
///         req.headers.set("x-trace-id", "abc123");
///         Ok(())
///     }
///     fn on_response(&self, req: &Request, status: StatusCode, _headers: &HeaderMap) {
///         println!("{} {} -> {}", req.method.as_str(), req.uri, status.as_u16());
///     }
/// }
///
/// # fn ex() -> Result<(), HttpError> {
/// let client = Client::builder().interceptor(TraceId).build()?;
/// # let _ = client;
/// # Ok(())
/// # }
/// ```
pub trait Interceptor: Send + Sync {
    /// Called before a request is sent; may modify it. Returning an error
    /// aborts the request and is returned from [`Client::send`].
    fn on_request(&self, req: &mut Request) -> Result<(), HttpError> {
        let _ = req;
        Ok(())
    }

    /// Called when a final (non-1xx) response head has been received.
    fn on_response(&self, req: &Request, status: StatusCode, headers: &HeaderMap) {
        let _ = (req, status, headers);
    }

    /// Called when a request fails before a response head was received.
    fn on_error(&self, req: &Request, error: &HttpError) {
        let _ = (req, error);
    }
}

#[derive(Clone)]
struct Config {
    tls: Arc<rustls::ClientConfig>,
    timeout: Option<Duration>,
    connect_timeout: Option<Duration>,
    read_timeout: Option<Duration>,
    total_timeout: Option<Duration>,
    max_body: Option<u64>,
    user_agent: Option<String>,
    max_redirects: usize,
    proxy: Option<Proxy>,
    interceptors: Vec<Arc<dyn Interceptor>>,
    decompress: bool,
    head_limits: HeadLimits,
}

/// A low-level HTTP/1.1 client. Higher-level ergonomics (auth, retry,
/// codec bodies) are layered on top in `ferroly::clients` / `ferroly::rest`.
///
/// Cloning a `Client` shares its connection pool, so clone freely.
#[derive(Clone)]
pub struct Client {
    cfg: Arc<Config>,
    pool: Arc<Pool>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("timeout", &self.cfg.timeout)
            .field("connect_timeout", &self.cfg.connect_timeout)
            .field("read_timeout", &self.cfg.read_timeout)
            .field("total_timeout", &self.cfg.total_timeout)
            .field("max_response_body_bytes", &self.cfg.max_body)
            .field("max_redirects", &self.cfg.max_redirects)
            .field("proxy", &self.cfg.proxy)
            .field("decompress", &self.cfg.decompress)
            .finish_non_exhaustive()
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

/// Configures and builds a [`Client`]; see [`Client::builder`].
///
/// | Setting | Default |
/// |---|---|
/// | [`timeout`](Self::timeout) (connect + send + response head) | 60 s |
/// | [`connect_timeout`](Self::connect_timeout) | 30 s |
/// | [`read_timeout`](Self::read_timeout) (idle time between body reads) | 300 s |
/// | [`total_timeout`](Self::total_timeout) (whole exchange incl. body) | none |
/// | [`max_response_body_bytes`](Self::max_response_body_bytes) | 64 MiB |
/// | [`user_agent`](Self::user_agent) | `ferroly/<version>` |
/// | [`pool_max_idle_per_host`](Self::pool_max_idle_per_host) | 32 |
/// | [`pool_idle_timeout`](Self::pool_idle_timeout) | 90 s |
/// | [`max_redirects`](Self::max_redirects) | 0 (redirects not followed) |
/// | [`proxy`](Self::proxy) | none |
/// | [`decompress`](Self::decompress) | off |
/// | [`tls`](Self::tls) | web PKI roots, TLS 1.2+ |
#[must_use]
pub struct ClientBuilder {
    cfg: Config,
    tls: Option<ClientTlsConfig>,
    pool_max_idle_per_host: usize,
    pool_idle_timeout: Option<Duration>,
}

impl std::fmt::Debug for ClientBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientBuilder").finish_non_exhaustive()
    }
}

impl ClientBuilder {
    fn new() -> Self {
        Self {
            cfg: Config {
                tls: transport::tls_config(),
                timeout: Some(Duration::from_secs(60)),
                connect_timeout: Some(Duration::from_secs(30)),
                read_timeout: Some(Duration::from_secs(300)),
                total_timeout: None,
                max_body: Some(DEFAULT_MAX_RESPONSE_BODY),
                user_agent: Some(DEFAULT_USER_AGENT.to_string()),
                max_redirects: 0,
                proxy: None,
                interceptors: Vec::new(),
                decompress: false,
                head_limits: HeadLimits::client(),
            },
            tls: None,
            pool_max_idle_per_host: 32,
            pool_idle_timeout: Some(Duration::from_secs(90)),
        }
    }

    /// Deadline for getting a response **head**: it covers taking or opening a
    /// connection, writing the request and reading the status line and
    /// headers (`None` disables it). Reading the body is governed by
    /// [`read_timeout`](Self::read_timeout) and
    /// [`total_timeout`](Self::total_timeout).
    pub fn timeout(mut self, timeout: Option<Duration>) -> Self {
        self.cfg.timeout = timeout;
        self
    }

    /// Deadline for establishing a connection: DNS, TCP connect, the TLS
    /// handshake and any proxy `CONNECT`. Expiry is reported as
    /// [`HttpError::ConnectTimeout`].
    pub fn connect_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.cfg.connect_timeout = timeout;
        self
    }

    /// The longest the client waits for the next piece of a response body
    /// (an idle timeout, restarted by every read). It also bounds each step of
    /// a streamed upload. Expiry is reported as [`HttpError::Timeout`] from
    /// [`Response::chunk`] / [`bytes`](Response::bytes) /
    /// [`text`](Response::text).
    pub fn read_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.cfg.read_timeout = timeout;
        self
    }

    /// An overall deadline for the whole exchange, from the call to
    /// [`Client::send`] until the last byte of the body has been read
    /// (including any redirects). `None` (the default) means no overall limit.
    pub fn total_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.cfg.total_timeout = timeout;
        self
    }

    /// Caps the size of a response body. A response that declares a larger
    /// `Content-Length`, or whose chunked or read-to-close body grows past the
    /// cap, fails with [`HttpError::BodyTooLarge`]. With
    /// [`decompress`](Self::decompress) the cap applies to the decoded bytes
    /// too. `None` removes the cap; a single response can override it with
    /// [`Response::set_max_body_bytes`].
    pub fn max_response_body_bytes(mut self, max: Option<u64>) -> Self {
        self.cfg.max_body = max;
        self
    }

    /// Sets the `User-Agent` sent when a request does not set its own.
    pub fn user_agent(mut self, value: impl Into<String>) -> Self {
        let value: String = value.into().chars().filter(|c| !c.is_control()).collect();
        self.cfg.user_agent = Some(value);
        self
    }

    /// Sends no default `User-Agent`.
    pub fn no_user_agent(mut self) -> Self {
        self.cfg.user_agent = None;
        self
    }

    /// The most idle connections kept per target (`0` disables pooling).
    pub fn pool_max_idle_per_host(mut self, max: usize) -> Self {
        self.pool_max_idle_per_host = max;
        self
    }

    /// How long an idle pooled connection is kept before it is closed (`None`
    /// keeps it until the server closes it).
    pub fn pool_idle_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.pool_idle_timeout = timeout;
        self
    }

    /// Follows up to `max` redirects (`301`, `302`, `303`, `307`, `308`).
    /// `0`, the default, returns the redirect response to the caller.
    ///
    /// When following: `303` becomes `GET` (`HEAD` stays `HEAD`) without a
    /// body; `301`/`302` turn `POST` into `GET` without a body and leave other
    /// methods alone; `307`/`308` repeat the request unchanged. A relative
    /// `Location` is resolved against the request URL. On a redirect to a
    /// different origin (scheme, host or port) the `Authorization`, `Cookie`,
    /// `Proxy-Authorization` and `Host` headers are removed. One redirect too
    /// many fails with [`HttpError::TooManyRedirects`]. Requests sent with
    /// [`Client::send_streaming`] are never redirected.
    pub fn max_redirects(mut self, max: usize) -> Self {
        self.cfg.max_redirects = max;
        self
    }

    /// Routes requests through a forward proxy; see [`Proxy`].
    pub fn proxy(mut self, proxy: Proxy) -> Self {
        self.cfg.proxy = Some(proxy);
        self
    }

    /// Configures the proxy from the conventional environment variables
    /// ([`Proxy::from_env`]); does nothing when none is set.
    pub fn proxy_from_env(mut self) -> Self {
        if let Some(proxy) = Proxy::from_env() {
            self.cfg.proxy = Some(proxy);
        }
        self
    }

    /// Adds an [`Interceptor`].
    pub fn interceptor(mut self, interceptor: impl Interceptor + 'static) -> Self {
        self.cfg.interceptors.push(Arc::new(interceptor));
        self
    }

    /// Sets the TLS options (trusted roots, client certificate, minimum
    /// version); see [`ClientTlsConfig`].
    pub fn tls(mut self, tls: ClientTlsConfig) -> Self {
        self.tls = Some(tls);
        self
    }

    /// Enables transparent response decompression. When on, the client sends
    /// `Accept-Encoding: gzip, deflate` (unless the request sets its own) and
    /// decodes a `gzip` or `deflate` response body as it is read, removing the
    /// `Content-Encoding` and `Content-Length` headers from the response. The
    /// response-body cap bounds the decoded size. Off by default.
    pub fn decompress(mut self, enabled: bool) -> Self {
        self.cfg.decompress = enabled;
        self
    }

    /// Builds the client. Fails only when the TLS options are invalid.
    pub fn build(mut self) -> Result<Client, HttpError> {
        if let Some(tls) = &self.tls {
            self.cfg.tls = tls.build()?;
        }
        Ok(Client {
            cfg: Arc::new(self.cfg),
            pool: Pool::new(self.pool_max_idle_per_host, self.pool_idle_timeout),
        })
    }
}

/// Why an exchange on a connection failed, and what that says about a retry.
struct Failure {
    error: HttpError,
    /// No byte of this request was accepted by the transport.
    nothing_written: bool,
    /// At least one byte of a response was read.
    response_started: bool,
}

impl Failure {
    fn before_write(error: HttpError) -> Self {
        Self {
            error,
            nothing_written: true,
            response_started: false,
        }
    }

    fn after_write(error: HttpError, response_started: bool) -> Self {
        Self {
            error,
            nothing_written: false,
            response_started,
        }
    }
}

/// What goes on the wire after the connection is ready.
enum Payload<'a> {
    /// A fully encoded request: `first` holds the head (and a small body),
    /// `rest` the remainder of a large body.
    Buffered { first: &'a [u8], rest: &'a [u8] },
    /// An encoded head followed by a body read from a stream.
    Streaming {
        head: &'a [u8],
        encoding: BodyEncoding,
        body: BodyStream<'a>,
    },
}

impl Client {
    /// Creates a client with the default settings: a 60-second timeout for
    /// the response head, a 30-second connect timeout, a 300-second idle
    /// timeout on body reads, a 64 MiB response-body cap, no proxy and no
    /// redirect following. See [`ClientBuilder`] for the full list.
    pub fn new() -> Self {
        let builder = ClientBuilder::new();
        Client {
            pool: Pool::new(builder.pool_max_idle_per_host, builder.pool_idle_timeout),
            cfg: Arc::new(builder.cfg),
        }
    }

    /// Starts configuring a client.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    /// Sets the response-head timeout (`None` disables it): the deadline for
    /// connecting, sending the request and receiving the response status line
    /// and headers. Equivalent to [`ClientBuilder::timeout`]; the other
    /// timeouts keep their values.
    pub fn with_timeout(mut self, timeout: Option<Duration>) -> Self {
        Arc::make_mut(&mut self.cfg).timeout = timeout;
        self
    }

    /// Sends a request and returns the response with a streaming body.
    ///
    /// Interim `1xx` responses (such as `100 Continue` and `103 Early Hints`)
    /// are skipped; the returned response is always the final one. If a
    /// pooled connection turns out to be dead, the request is re-sent on a
    /// fresh connection only when no response byte was received and either
    /// the method is idempotent or nothing of the request had been written.
    pub async fn send(&self, req: Request) -> Result<Response, HttpError> {
        let deadline = self.deadline();
        within(deadline, self.run(req, deadline)).await
    }

    /// Sends a request whose body is read from `body` as it is uploaded,
    /// instead of from [`Request::body`] (which is ignored).
    ///
    /// The body is sent with `Transfer-Encoding: chunked`, or — when the
    /// request sets a `Content-Length` header — as exactly that many bytes (a
    /// shorter stream is an error). A streamed request always uses a fresh
    /// connection, and is never retried or redirected, because the stream
    /// cannot be replayed. The response-head timeout starts once the upload
    /// has finished; each read from `body` and write to the server is bounded
    /// by the read timeout.
    pub async fn send_streaming<B>(&self, req: Request, mut body: B) -> Result<Response, HttpError>
    where
        B: AsyncRead + Send + Unpin,
    {
        let deadline = self.deadline();
        within(deadline, async {
            let mut req = req;
            self.before(&mut req)?;
            let result = self.send_once(&req, Some(&mut body), deadline).await;
            self.after(&req, result)
        })
        .await
    }

    fn deadline(&self) -> Option<Instant> {
        self.cfg.total_timeout.map(|d| Instant::now() + d)
    }

    fn before(&self, req: &mut Request) -> Result<(), HttpError> {
        for interceptor in &self.cfg.interceptors {
            interceptor.on_request(req)?;
        }
        Ok(())
    }

    fn after(
        &self,
        req: &Request,
        result: Result<Response, HttpError>,
    ) -> Result<Response, HttpError> {
        for interceptor in &self.cfg.interceptors {
            match &result {
                Ok(resp) => interceptor.on_response(req, resp.status(), resp.headers()),
                Err(e) => interceptor.on_error(req, e),
            }
        }
        result
    }

    /// Sends `req`, following redirects when configured.
    async fn run(
        &self,
        mut req: Request,
        deadline: Option<Instant>,
    ) -> Result<Response, HttpError> {
        let mut hops = 0usize;
        loop {
            self.before(&mut req)?;
            let result = self.send_once(&req, None, deadline).await;
            let resp = self.after(&req, result)?;
            if self.cfg.max_redirects == 0 || !resp.status().is_redirection() {
                return Ok(resp);
            }
            let location = resp.headers().get("location").map(str::to_string);
            let Some(location) = location else {
                return Ok(resp);
            };
            if hops >= self.cfg.max_redirects {
                return Err(HttpError::TooManyRedirects);
            }
            hops += 1;
            let next = redirect_request(&req, resp.status(), &location)?;
            // Finish with this response so its connection can be reused.
            resp.into_body().discard(REDIRECT_DRAIN_BYTES).await;
            req = next;
        }
    }

    fn route(&self, target: &Uri) -> Route<'_> {
        match &self.cfg.proxy {
            Some(proxy) => proxy.route(target),
            None => Route::Direct,
        }
    }

    /// One request/response exchange (no redirects).
    async fn send_once(
        &self,
        req: &Request,
        stream: Option<BodyStream<'_>>,
        deadline: Option<Instant>,
    ) -> Result<Response, HttpError> {
        let route = self.route(&req.uri);
        let (absolute_form, proxy_authorization, proxy_id) = match &route {
            Route::Direct => (false, None, None),
            Route::Forward(p) => (true, p.authorization.as_deref(), Some(p.pool_id())),
            Route::Tunnel(p) => (false, None, Some(p.pool_id())),
        };
        let wire = RequestWire {
            absolute_form,
            user_agent: self.cfg.user_agent.as_deref(),
            accept_encoding: self.cfg.decompress.then_some("gzip, deflate"),
            proxy_authorization,
        };
        let key = PoolKey {
            host: req.uri.host.to_ascii_lowercase(),
            port: req.uri.port,
            tls: req.uri.is_tls(),
            proxy: proxy_id,
        };

        match stream {
            None => {
                // Encode before touching a connection: a request that cannot
                // be sent safely must not cost a pooled connection.
                let (first, rest) = io::encode_request(req, &wire)?;
                let exchange = async {
                    if let Some(reader) = self.pool.take(&key) {
                        let payload = Payload::Buffered {
                            first: &first,
                            rest,
                        };
                        match self.exchange(reader, req, payload, &key, deadline).await {
                            Ok(resp) => return Ok(resp),
                            Err(failure) => {
                                // The connection was reused, so the failure may
                                // only mean the server had closed it. Re-send
                                // on a fresh one when that cannot repeat a
                                // side effect.
                                let safe = !failure.response_started
                                    && !matches!(failure.error, HttpError::Timeout)
                                    && (failure.nothing_written || req.method.is_idempotent());
                                if !safe {
                                    return Err(failure.error);
                                }
                            }
                        }
                    }
                    let conn = self.connect(&req.uri, &route).await?;
                    let payload = Payload::Buffered {
                        first: &first,
                        rest,
                    };
                    self.exchange(BufReader::new(conn), req, payload, &key, deadline)
                        .await
                        .map_err(|f| f.error)
                };
                match self.cfg.timeout {
                    Some(limit) => match tokio::time::timeout(limit, exchange).await {
                        Ok(result) => result,
                        Err(_) => Err(HttpError::Timeout),
                    },
                    None => exchange.await,
                }
            }
            Some(body) => {
                let (head, encoding) = io::encode_request_head(req, &wire, None)?;
                let conn = match self.cfg.timeout {
                    Some(limit) => {
                        match tokio::time::timeout(limit, self.connect(&req.uri, &route)).await {
                            Ok(result) => result?,
                            Err(_) => return Err(HttpError::Timeout),
                        }
                    }
                    None => self.connect(&req.uri, &route).await?,
                };
                let payload = Payload::Streaming {
                    head: &head,
                    encoding,
                    body,
                };
                self.exchange(BufReader::new(conn), req, payload, &key, deadline)
                    .await
                    .map_err(|f| f.error)
            }
        }
    }

    /// Opens a connection for `target` along `route`, within the connect
    /// timeout.
    async fn connect(&self, target: &Uri, route: &Route<'_>) -> Result<Conn, HttpError> {
        let tls = &self.cfg.tls;
        let open = async {
            match route {
                Route::Direct => transport::connect(target, tls).await,
                Route::Forward(proxy) => transport::connect(&proxy.uri, tls).await,
                Route::Tunnel(proxy) => {
                    let conn = transport::connect(&proxy.uri, tls).await?;
                    let conn = self.open_tunnel(conn, target, proxy).await?;
                    transport::tls_handshake(conn, &target.host, tls).await
                }
            }
        };
        match self.cfg.connect_timeout {
            Some(limit) => match tokio::time::timeout(limit, open).await {
                Ok(result) => result,
                Err(_) => Err(HttpError::ConnectTimeout),
            },
            None => open.await,
        }
    }

    /// Asks `proxy` for a `CONNECT` tunnel to `target` over `conn`.
    async fn open_tunnel(
        &self,
        conn: Conn,
        target: &Uri,
        proxy: &ProxyServer,
    ) -> Result<Conn, HttpError> {
        target.validate_for_wire()?;
        let authority = target.host_port();
        let mut head = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
        if let Some(ua) = &self.cfg.user_agent {
            head.push_str(&format!("User-Agent: {ua}\r\n"));
        }
        if let Some(auth) = &proxy.authorization {
            head.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
        }
        head.push_str("\r\n");

        let mut reader = BufReader::new(conn);
        reader.write_all(head.as_bytes()).await?;
        reader.flush().await?;
        let mut started = false;
        let answer =
            io::read_response_head_with(&mut reader, &self.cfg.head_limits, &mut started).await?;
        if !answer.status.is_success() {
            return Err(HttpError::Proxy(format!(
                "CONNECT to {authority} was refused with status {}",
                answer.status.as_u16()
            )));
        }
        if !reader.buffer().is_empty() {
            return Err(HttpError::Proxy(
                "unexpected data after the CONNECT response".into(),
            ));
        }
        Ok(reader.into_inner())
    }

    /// Writes the request on `reader` and reads the response head, wiring the
    /// body to return the connection to the pool when it drains cleanly
    /// (unless the response is not keep-alive-able).
    async fn exchange(
        &self,
        mut reader: BufReader<Conn>,
        req: &Request,
        payload: Payload<'_>,
        key: &PoolKey,
        deadline: Option<Instant>,
    ) -> Result<Response, Failure> {
        let mut streamed = false;
        match payload {
            Payload::Buffered { first, rest } => {
                // If the very first write fails, the transport took none of
                // this request, so it cannot have been processed.
                let n = match reader.write(first).await {
                    Ok(n) => n,
                    Err(e) => return Err(Failure::before_write(e.into())),
                };
                let write = async {
                    reader.write_all(&first[n..]).await?;
                    reader.write_all(rest).await?;
                    reader.flush().await
                };
                if let Err(e) = write.await {
                    return Err(Failure::after_write(e.into(), false));
                }
            }
            Payload::Streaming {
                head,
                encoding,
                body,
            } => {
                streamed = true;
                let idle = self.cfg.read_timeout;
                let write = async {
                    io::bounded(idle, reader.write_all(head)).await?;
                    io::write_body_stream(&mut reader, encoding, body, idle).await
                };
                if let Err(e) = write.await {
                    return Err(Failure::after_write(e, false));
                }
            }
        }

        // Skip interim responses: the final response is the first that is not
        // 1xx (101 ends the HTTP exchange, so it is final too).
        let mut started = false;
        let mut interim = 0usize;
        let head = loop {
            let read =
                io::read_response_head_with(&mut reader, &self.cfg.head_limits, &mut started);
            let read = async {
                match (streamed, self.cfg.timeout) {
                    // A streamed request times its head from the end of the
                    // upload; a buffered one is timed as a whole by the caller.
                    (true, Some(limit)) => match tokio::time::timeout(limit, read).await {
                        Ok(result) => result,
                        Err(_) => Err(HttpError::Timeout),
                    },
                    _ => read.await,
                }
            };
            let head = match read.await {
                Ok(head) => head,
                Err(e) => return Err(Failure::after_write(e, started)),
            };
            if head.status.is_informational() && head.status != StatusCode::SWITCHING_PROTOCOLS {
                interim += 1;
                if interim > MAX_INTERIM_RESPONSES {
                    return Err(Failure::after_write(
                        HttpError::Protocol("too many interim responses".into()),
                        true,
                    ));
                }
                continue;
            }
            break head;
        };

        let status = head.status;
        let mut headers = head.headers;
        let framing = io::response_framing(&req.method, status, &headers)
            .map_err(|e| Failure::after_write(e, true))?;
        let persistent = match head.version {
            Version::Http11 => !headers.connection_has("close"),
            Version::Http10 => headers.connection_has("keep-alive"),
        };
        // A response carrying both framings is suspect; never reuse after it.
        let ambiguous = matches!(framing, Framing::Chunked) && headers.contains("content-length");
        let reusable = persistent
            && !ambiguous
            && !matches!(framing, Framing::Eof)
            && status != StatusCode::SWITCHING_PROTOCOLS
            && !matches!(req.method, Method::Connect);
        let has_body = !matches!(framing, Framing::Done);

        let limits = BodyLimits {
            max_bytes: self.cfg.max_body,
            read_timeout: self.cfg.read_timeout,
            deadline,
            head: self.cfg.head_limits,
        };
        let mut body = if reusable {
            Body::pooled(
                reader,
                framing,
                limits,
                Reclaim {
                    pool: self.pool.clone(),
                    key: key.clone(),
                },
            )
        } else {
            Body::new(reader, framing, limits)
        };

        if self.cfg.decompress && has_body {
            let decoder = {
                let mut encodings = headers.get_all("content-encoding");
                match (encodings.next(), encodings.next()) {
                    (Some(encoding), None) => Decoder::for_content_encoding(encoding),
                    _ => None,
                }
            };
            if let Some(decoder) = decoder {
                body = body.with_decoder(decoder);
                // The headers describe the encoded form, which the caller no
                // longer sees.
                headers.remove("content-encoding");
                headers.remove("content-length");
            }
        }
        Ok(Response::new(status, headers, req.uri.clone(), body))
    }
}

/// Runs `fut` under the total deadline, if there is one.
async fn within<T>(
    deadline: Option<Instant>,
    fut: impl std::future::Future<Output = Result<T, HttpError>>,
) -> Result<T, HttpError> {
    match deadline {
        Some(at) => match tokio::time::timeout_at(at, fut).await {
            Ok(result) => result,
            Err(_) => Err(HttpError::Timeout),
        },
        None => fut.await,
    }
}

/// Builds the request that follows a redirect `status` to `location`.
fn redirect_request(
    req: &Request,
    status: StatusCode,
    location: &str,
) -> Result<Request, HttpError> {
    let uri = req.uri.resolve(location.trim())?;
    if uri.scheme != "http" && uri.scheme != "https" {
        return Err(HttpError::InvalidUrl(format!(
            "redirect to unsupported scheme: {}",
            uri.scheme
        )));
    }
    let mut next = req.clone();
    let to_get = match status.as_u16() {
        303 => !matches!(req.method, Method::Head | Method::Get),
        301 | 302 => matches!(req.method, Method::Post),
        _ => false,
    };
    if to_get {
        next.method = Method::Get;
        next.body = Vec::new();
        for name in [
            "content-length",
            "content-type",
            "content-encoding",
            "content-language",
            "content-location",
            "transfer-encoding",
        ] {
            next.headers.remove(name);
        }
    }
    if !req.uri.same_origin(&uri) {
        for name in ["authorization", "cookie", "proxy-authorization", "host"] {
            next.headers.remove(name);
        }
    }
    next.uri = uri;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: Method, url: &str) -> Request {
        Request::builder(method, url)
            .unwrap()
            .header("authorization", "Bearer t")
            .header("cookie", "a=b")
            .header("content-type", "application/json")
            .header("x-keep", "1")
            .body(b"{}".to_vec())
            .build()
    }

    #[test]
    fn redirect_method_rewriting() {
        let base = "http://a.example/x/y";
        for (status, method, expect, keeps_body) in [
            (301, Method::Post, Method::Get, false),
            (302, Method::Post, Method::Get, false),
            (303, Method::Post, Method::Get, false),
            (303, Method::Put, Method::Get, false),
            (303, Method::Head, Method::Head, true),
            (301, Method::Put, Method::Put, true),
            (302, Method::Delete, Method::Delete, true),
            (307, Method::Post, Method::Post, true),
            (308, Method::Post, Method::Post, true),
            (308, Method::Get, Method::Get, true),
        ] {
            let next =
                redirect_request(&req(method.clone(), base), StatusCode(status), "/z").unwrap();
            assert_eq!(next.method, expect, "{status} {method:?}");
            assert_eq!(next.body.is_empty(), !keeps_body, "{status} {method:?}");
            assert_eq!(
                next.headers.contains("content-type"),
                keeps_body,
                "{status} {method:?}"
            );
            assert_eq!(next.uri.absolute(), "http://a.example/z");
            // Same origin: credentials stay.
            assert!(next.headers.contains("authorization"));
            assert_eq!(next.headers.get("x-keep"), Some("1"));
        }
    }

    #[test]
    fn redirect_strips_credentials_across_origins() {
        let from = req(Method::Get, "https://a.example/x");
        for location in [
            "https://b.example/x",
            "http://a.example/x",
            "https://a.example:8443/x",
            "//b.example/x",
        ] {
            let next = redirect_request(&from, StatusCode(302), location).unwrap();
            assert!(!next.headers.contains("authorization"), "{location}");
            assert!(!next.headers.contains("cookie"), "{location}");
            assert_eq!(next.headers.get("x-keep"), Some("1"));
        }
        let next = redirect_request(&from, StatusCode(302), "../y?q=1#frag").unwrap();
        assert_eq!(next.uri.absolute(), "https://a.example/y?q=1");
        assert!(next.headers.contains("authorization"));

        assert!(redirect_request(&from, StatusCode(302), "ftp://x/").is_err());
        assert!(redirect_request(&from, StatusCode(302), "/a b").is_err());
        assert!(redirect_request(&from, StatusCode(302), "ws://a.example/").is_err());
    }

    #[test]
    fn with_timeout_keeps_other_settings() {
        let client = Client::builder()
            .read_timeout(Some(Duration::from_secs(7)))
            .max_response_body_bytes(Some(10))
            .build()
            .unwrap()
            .with_timeout(None);
        assert_eq!(client.cfg.timeout, None);
        assert_eq!(client.cfg.read_timeout, Some(Duration::from_secs(7)));
        assert_eq!(client.cfg.max_body, Some(10));
        assert_eq!(
            Client::new().cfg.user_agent.as_deref(),
            Some(DEFAULT_USER_AGENT)
        );
        assert!(DEFAULT_USER_AGENT.starts_with("ferroly/"));
    }
}

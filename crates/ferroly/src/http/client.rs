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
use super::{BodyStream, HeaderMap, HttpError, Method, Protocol, StatusCode, Uri};

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
type UploadReader<'a> = &'a mut (dyn AsyncRead + Send + Unpin);

/// Where a request's body comes from.
enum Outgoing<'a> {
    /// [`Request::body`].
    Buffered,
    /// A borrowed reader ([`Client::send_streaming`]).
    Reader(UploadReader<'a>),
    /// An owned stream ([`Client::send_body_stream`]); `None` once handed to
    /// an HTTP/2 connection.
    Stream(Option<BodyStream>),
}

/// How an HTTP/2 attempt ended.
#[cfg(feature = "http2")]
enum H2Attempt {
    Done(Box<Response>),
    /// The server chose HTTP/1.1 with ALPN on this new connection.
    Http1(Conn),
    /// Not over HTTP/2 (the origin is known to speak only HTTP/1.1).
    Skip,
}

/// The client's HTTP/2 settings.
#[cfg(feature = "http2")]
#[derive(Clone)]
struct H2Settings {
    alpn: bool,
    prior_knowledge: bool,
    config: super::Http2Config,
    idle_timeout: Option<Duration>,
    /// Fail a request instead of sending it over HTTP/1.1 (set by
    /// `ferroly::grpc`, which cannot work over HTTP/1.1).
    required: bool,
}

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
    /// `None`: the shared default, resolved at the first TLS handshake.
    tls: Option<Arc<rustls::ClientConfig>>,
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
    /// HTTP/2, when enabled.
    #[cfg(feature = "http2")]
    h2: Option<H2Settings>,
}

/// A low-level HTTP/1.1 client. Higher-level ergonomics (auth, retry,
/// codec bodies) are layered on top in `ferroly::clients` / `ferroly::rest`.
///
/// Cloning a `Client` shares its connection pool, so clone freely.
#[derive(Clone)]
pub struct Client {
    cfg: Arc<Config>,
    pool: Arc<Pool>,
    #[cfg(feature = "http2")]
    h2: Arc<super::h2::client::H2Pool>,
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
/// | [`pool_max_active_per_host`](Self::pool_max_active_per_host) | unlimited |
/// | [`max_redirects`](Self::max_redirects) | 0 (redirects not followed) |
/// | [`proxy`](Self::proxy) | none |
/// | [`decompress`](Self::decompress) | off |
/// | [`tls`](Self::tls) | web PKI roots, TLS 1.2+ |
/// | `http2` / `http2_prior_knowledge` (feature `http2`) | off: HTTP/1.1 only |
#[must_use]
pub struct ClientBuilder {
    cfg: Config,
    tls: Option<ClientTlsConfig>,
    pool_max_idle_per_host: usize,
    pool_idle_timeout: Option<Duration>,
    pool_max_active_per_host: Option<usize>,
    #[cfg(feature = "http2")]
    h2: H2Settings,
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
                tls: None,
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
                #[cfg(feature = "http2")]
                h2: None,
            },
            tls: None,
            pool_max_idle_per_host: 32,
            pool_idle_timeout: Some(Duration::from_secs(90)),
            pool_max_active_per_host: None,
            #[cfg(feature = "http2")]
            h2: H2Settings {
                alpn: false,
                prior_knowledge: false,
                config: super::Http2Config::default(),
                idle_timeout: None,
                required: false,
            },
        }
    }

    /// Offers HTTP/2 to `https` servers with ALPN (`h2`, then `http/1.1`).
    /// A server that selects `h2` gets its concurrent requests multiplexed
    /// over a few connections per origin (a further one only when every
    /// existing one is saturated, up to `Http2Config::max_connections_per_origin`,
    /// default 4); a server that
    /// selects HTTP/1.1 (or does not negotiate) is spoken to as before. Off by
    /// default. Requests through a forward (non-tunnelling) proxy always use
    /// HTTP/1.1. Requires the `http2` feature.
    #[cfg(feature = "http2")]
    pub fn http2(mut self, enabled: bool) -> Self {
        self.h2.alpn = enabled;
        self
    }

    /// Speaks cleartext HTTP/2 (h2c) to `http` URLs without negotiation
    /// ("prior knowledge", RFC 9113 §3.3): use it only for servers known to
    /// support it — an HTTP/1.1-only server fails the request. The h2c
    /// `Upgrade` mechanism is not supported (RFC 9113 deprecates it). Off by
    /// default. Requires the `http2` feature.
    #[cfg(feature = "http2")]
    pub fn http2_prior_knowledge(mut self, enabled: bool) -> Self {
        self.h2.prior_knowledge = enabled;
        self
    }

    /// Sets the HTTP/2 limits and windows used by this client's connections
    /// (see [`Http2Config`](super::Http2Config)). Does not by itself enable
    /// HTTP/2; see [`http2`](Self::http2). Requires the `http2` feature.
    #[cfg(feature = "http2")]
    pub fn http2_config(mut self, config: super::Http2Config) -> Self {
        self.h2.config = config;
        self
    }

    /// Makes every request fail with [`HttpError::Protocol`] instead of
    /// going over HTTP/1.1: when HTTP/2 is not enabled for its URL (an `http`
    /// URL without prior knowledge, an `https` URL without ALPN, a forward
    /// proxy) or the server's ALPN answer is not `h2`. For protocols that
    /// exist only over HTTP/2, such as gRPC.
    #[cfg(feature = "http2")]
    #[cfg_attr(not(feature = "grpc"), allow(dead_code))]
    pub(crate) fn require_http2(mut self) -> Self {
        self.h2.required = true;
        self
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

    /// Caps the HTTP/1.1 connections to one target (host, port, scheme and
    /// proxy) that are in use at once — being opened or carrying a request
    /// and its response body — at `max` (at least 1). `None`, the default,
    /// does not limit them.
    ///
    /// A request that finds every slot taken waits, in order, for one to be
    /// released: when a response body has been read to the end (the
    /// connection goes back to the pool, or is closed when it cannot be kept
    /// alive), at once for a response without a body, or when the response
    /// is dropped. The wait
    /// counts against the response-head [`timeout`](Self::timeout) (and the
    /// [`total_timeout`](Self::total_timeout)); running out of it fails with
    /// [`HttpError::Timeout`]. Idle pooled connections do not count; they
    /// are capped by [`pool_max_idle_per_host`](Self::pool_max_idle_per_host).
    /// It does not apply to HTTP/2, whose connections per origin are capped
    /// by `Http2Config::max_connections_per_origin` instead.
    pub fn pool_max_active_per_host(mut self, max: Option<usize>) -> Self {
        self.pool_max_active_per_host = max;
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

    /// Builds the client. Fails only when the TLS options set with
    /// [`tls`](Self::tls) are invalid or no rustls crypto provider is
    /// available for them ([`HttpError::Tls`]). A client without custom TLS
    /// options needs no provider until its first `https` request.
    pub fn build(mut self) -> Result<Client, HttpError> {
        if let Some(tls) = &self.tls {
            self.cfg.tls = Some(tls.build()?);
        }
        #[cfg(feature = "http2")]
        if self.h2.alpn || self.h2.prior_knowledge {
            let mut h2 = self.h2.clone();
            h2.idle_timeout = self.pool_idle_timeout;
            self.cfg.h2 = Some(h2);
        }
        Ok(Client {
            cfg: Arc::new(self.cfg),
            pool: Pool::new(
                self.pool_max_idle_per_host,
                self.pool_idle_timeout,
                self.pool_max_active_per_host,
            ),
            #[cfg(feature = "http2")]
            h2: Arc::default(),
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
        body: UploadReader<'a>,
    },
    /// An encoded head followed by a body (and trailers) from a
    /// [`BodyStream`].
    Piped {
        head: &'a [u8],
        encoding: BodyEncoding,
        body: &'a mut BodyStream,
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
            pool: Pool::new(
                builder.pool_max_idle_per_host,
                builder.pool_idle_timeout,
                builder.pool_max_active_per_host,
            ),
            cfg: Arc::new(builder.cfg),
            #[cfg(feature = "http2")]
            h2: Arc::default(),
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

    /// Sets how many redirects are followed (`0` returns every redirect to
    /// the caller). Equivalent to [`ClientBuilder::max_redirects`]; the
    /// connection pool and the other settings are kept.
    pub fn with_max_redirects(mut self, max: usize) -> Self {
        if self.cfg.max_redirects != max {
            Arc::make_mut(&mut self.cfg).max_redirects = max;
        }
        self
    }

    /// This client with HTTP/2 required (see `ClientBuilder::require_http2`),
    /// sharing its pools; `None` when HTTP/2 is not enabled for `https`
    /// (ALPN) or `http` (prior knowledge) URLs, whichever `https` names.
    #[cfg(feature = "grpc")]
    pub(crate) fn requiring_http2(mut self, https: bool) -> Option<Self> {
        let h2 = self.cfg.h2.as_ref()?;
        let enabled = if https { h2.alpn } else { h2.prior_knowledge };
        if !enabled {
            return None;
        }
        if !h2.required {
            if let Some(h2) = Arc::make_mut(&mut self.cfg).h2.as_mut() {
                h2.required = true;
            }
        }
        Some(self)
    }

    /// Turns transparent response decompression on or off. Equivalent to
    /// [`ClientBuilder::decompress`]; the connection pool and the other
    /// settings are kept.
    pub fn with_decompress(mut self, enabled: bool) -> Self {
        if self.cfg.decompress != enabled {
            Arc::make_mut(&mut self.cfg).decompress = enabled;
        }
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
            let result = self
                .send_once(&req, Outgoing::Reader(&mut body), deadline)
                .await;
            self.after(&req, result)
        })
        .await
    }

    /// Sends a request whose body is read from `body`, instead of from
    /// [`Request::body`] (which is ignored), and returns as soon as the
    /// response head arrives.
    ///
    /// Over HTTP/2 the body is uploaded by a background task while the
    /// response is read — both directions stream at once on one stream, as
    /// gRPC's bidirectional calls need — and the trailers the body ends with
    /// ([`BodySender::send_trailers`](super::BodySender::send_trailers)) are
    /// sent as a trailing HEADERS frame. If the body fails (its sender is
    /// dropped without finishing) the stream is reset. Over HTTP/1.1 the
    /// body is uploaded first, with chunked transfer encoding and its
    /// trailers in the trailer section (or as exactly `Content-Length` bytes
    /// when the request sets one), and then the response is read.
    ///
    /// Like [`send_streaming`](Self::send_streaming), the request is never
    /// redirected, and is retried (on HTTP/2) only when the server refused
    /// it before any of the body was taken.
    pub async fn send_body_stream(
        &self,
        req: Request,
        body: BodyStream,
    ) -> Result<Response, HttpError> {
        let deadline = self.deadline();
        within(deadline, async {
            let mut req = req;
            self.before(&mut req)?;
            let result = self
                .send_once(&req, Outgoing::Stream(Some(body)), deadline)
                .await;
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
            let result = self.send_once(&req, Outgoing::Buffered, deadline).await;
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
        body: Outgoing<'_>,
        deadline: Option<Instant>,
    ) -> Result<Response, HttpError> {
        #[cfg(feature = "http2")]
        let mut body = body;
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

        // A connection opened while trying HTTP/2 whose ALPN answer was
        // HTTP/1.1: used for this request instead of a new one.
        #[allow(unused_mut)]
        let mut fresh: Option<Conn> = None;
        #[cfg(feature = "http2")]
        let required = self.cfg.h2.as_ref().is_some_and(|h2| h2.required);
        #[cfg(feature = "http2")]
        let no_h2 = || {
            HttpError::Protocol(format!(
                "{} did not negotiate HTTP/2 (or HTTP/2 is not enabled for it), \
                 which this client requires",
                req.uri.authority()
            ))
        };
        #[cfg(feature = "http2")]
        if required && self.h2_mode(req, &route).is_none() {
            return Err(no_h2());
        }
        #[cfg(feature = "http2")]
        if let Some(prior_knowledge) = self.h2_mode(req, &route) {
            let buffered = matches!(body, Outgoing::Buffered);
            let attempt = self.try_h2(
                req,
                &mut body,
                &route,
                &key,
                &wire,
                prior_knowledge,
                deadline,
            );
            let attempt = match (buffered, self.cfg.timeout) {
                (true, Some(limit)) => match tokio::time::timeout(limit, attempt).await {
                    Ok(result) => result?,
                    Err(_) => return Err(HttpError::Timeout),
                },
                _ => attempt.await?,
            };
            match attempt {
                H2Attempt::Done(resp) => return Ok(*resp),
                H2Attempt::Http1(_) | H2Attempt::Skip if required => return Err(no_h2()),
                H2Attempt::Http1(conn) => fresh = Some(conn),
                H2Attempt::Skip => {}
            }
        }

        match body {
            Outgoing::Buffered => {
                // Encode before touching a connection: a request that cannot
                // be sent safely must not cost a pooled connection.
                let (first, rest) = io::encode_request(req, &wire)?;
                let exchange = async {
                    // A slot under the per-host cap, for whichever connection
                    // ends up carrying the request (waiting is timed with it).
                    let mut slot = self.pool.acquire(&key).await;
                    if fresh.is_none() {
                        if let Some(reader) = self.pool.take(&key) {
                            let payload = Payload::Buffered {
                                first: &first,
                                rest,
                            };
                            let attempt =
                                self.exchange(reader, req, payload, &key, deadline, &mut slot);
                            match attempt.await {
                                Ok(resp) => return Ok(resp),
                                Err(failure) => {
                                    // The connection was reused, so the
                                    // failure may only mean the server had
                                    // closed it. Re-send on a fresh one when
                                    // that cannot repeat a side effect.
                                    let safe = !failure.response_started
                                        && !matches!(failure.error, HttpError::Timeout)
                                        && (failure.nothing_written || req.method.is_idempotent());
                                    if !safe {
                                        return Err(failure.error);
                                    }
                                }
                            }
                        }
                    }
                    let conn = match fresh {
                        Some(conn) => conn,
                        None => self.connect(&req.uri, &route).await?,
                    };
                    let payload = Payload::Buffered {
                        first: &first,
                        rest,
                    };
                    self.exchange(
                        BufReader::new(conn),
                        req,
                        payload,
                        &key,
                        deadline,
                        &mut slot,
                    )
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
            Outgoing::Reader(body) => {
                let (head, encoding) = io::encode_request_head(req, &wire, None)?;
                let mut slot = self.acquire_timed(&key).await?;
                let conn = self.connect_timed(&req.uri, &route, fresh).await?;
                let payload = Payload::Streaming {
                    head: &head,
                    encoding,
                    body,
                };
                self.exchange(
                    BufReader::new(conn),
                    req,
                    payload,
                    &key,
                    deadline,
                    &mut slot,
                )
                .await
                .map_err(|f| f.error)
            }
            Outgoing::Stream(stream) => {
                let mut stream = stream.unwrap_or_else(BodyStream::empty);
                let (head, encoding) = io::encode_request_head(req, &wire, None)?;
                let mut slot = self.acquire_timed(&key).await?;
                let conn = self.connect_timed(&req.uri, &route, fresh).await?;
                let payload = Payload::Piped {
                    head: &head,
                    encoding,
                    body: &mut stream,
                };
                self.exchange(
                    BufReader::new(conn),
                    req,
                    payload,
                    &key,
                    deadline,
                    &mut slot,
                )
                .await
                .map_err(|f| f.error)
            }
        }
    }

    /// A slot under the per-host active-connection cap, within the
    /// response-head timeout.
    async fn acquire_timed(
        &self,
        key: &PoolKey,
    ) -> Result<Option<tokio::sync::OwnedSemaphorePermit>, HttpError> {
        match self.cfg.timeout {
            Some(limit) => tokio::time::timeout(limit, self.pool.acquire(key))
                .await
                .map_err(|_| HttpError::Timeout),
            None => Ok(self.pool.acquire(key).await),
        }
    }

    /// `fresh`, or a new connection within the response-head timeout.
    async fn connect_timed(
        &self,
        target: &Uri,
        route: &Route<'_>,
        fresh: Option<Conn>,
    ) -> Result<Conn, HttpError> {
        if let Some(conn) = fresh {
            return Ok(conn);
        }
        match self.cfg.timeout {
            Some(limit) => match tokio::time::timeout(limit, self.connect(target, route)).await {
                Ok(result) => result,
                Err(_) => Err(HttpError::Timeout),
            },
            None => self.connect(target, route).await,
        }
    }

    /// Whether `req` goes over HTTP/2: `Some(false)` to offer `h2` with ALPN
    /// (https, direct or through a CONNECT tunnel), `Some(true)` for
    /// cleartext h2c with prior knowledge (http, direct only).
    #[cfg(feature = "http2")]
    fn h2_mode(&self, req: &Request, route: &Route<'_>) -> Option<bool> {
        let h2 = self.cfg.h2.as_ref()?;
        if matches!(req.method, Method::Connect) {
            return None;
        }
        match (req.uri.scheme.as_str(), route) {
            ("https", Route::Direct | Route::Tunnel(_)) if h2.alpn => Some(false),
            ("http", Route::Direct) if h2.prior_knowledge => Some(true),
            _ => None,
        }
    }

    /// Sends `req` over HTTP/2 when the origin speaks it: on the origin's
    /// existing connection, or on a new one (set up once even for concurrent
    /// first requests), never more than `max_connections_per_origin`. A
    /// refused stream is retried once when the body can be replayed: on
    /// another connection with a free stream slot whose server has announced
    /// its stream limit, else on a new one while the origin is below
    /// `max_connections_per_origin`, else on the least loaded one (which may
    /// be the one that refused it).
    #[cfg(feature = "http2")]
    #[allow(clippy::too_many_arguments)]
    async fn try_h2(
        &self,
        req: &Request,
        body: &mut Outgoing<'_>,
        route: &Route<'_>,
        key: &PoolKey,
        wire: &RequestWire<'_>,
        prior_knowledge: bool,
        deadline: Option<Instant>,
    ) -> Result<H2Attempt, HttpError> {
        use super::h2::client::{self as h2c, Failure, Upload};
        let Some(settings) = self.cfg.h2.as_ref() else {
            return Ok(H2Attempt::Skip);
        };
        let buffered = matches!(body, Outgoing::Buffered);
        let fields = h2c::request_fields(req, wire, buffered.then_some(req.body.len()))?;
        let head_request = matches!(req.method, Method::Head);
        // A buffered exchange is timed as a whole by the caller; a streamed
        // one times only the wait for the response head.
        let head_timeout = if buffered { None } else { self.cfg.timeout };
        use super::h2::client::Pick;
        let max_conns = settings.config.normalized().max_connections_per_origin;
        let refused = || {
            super::h2::error::h2_error(super::Http2Cause::Refused, "the server refused the request")
        };
        // The connection the first attempt failed on.
        let mut failed: Option<super::h2::client::H2Conn> = None;
        for attempt in 0..2 {
            // The first attempt takes a connection with a free stream slot; a
            // retry (after a refusal or a lost connection) takes one other
            // than the connection that failed, dialing below `max_conns` and
            // going back to the least loaded one only at the limit. A new
            // connection is dialed only under the origin's setup lock and
            // below `max_conns`, so neither a burst nor its retries open more
            // than that.
            let pick = |pool: &super::h2::client::H2Pool| match &failed {
                Some(f) => pool.get_retry(key, max_conns, f),
                None => pool.get(key, max_conns),
            };
            let existing = match pick(&self.h2) {
                Pick::Use(conn) => Some(conn),
                _ => None,
            };
            let reused = existing.is_some();
            let conn = match existing {
                Some(conn) => conn,
                None => {
                    if !prior_knowledge && self.h2.is_h1_only(key) {
                        return Ok(H2Attempt::Skip);
                    }
                    let _setup = self.h2.setup_lock(key).await;
                    // Someone may have dialed while we waited.
                    match pick(&self.h2) {
                        Pick::Use(conn) => conn,
                        _ => {
                            let (io, is_h2) =
                                self.connect_h2(&req.uri, route, prior_knowledge).await?;
                            if !is_h2 {
                                self.h2.mark_h1_only(key.clone());
                                return Ok(H2Attempt::Http1(io));
                            }
                            let conn = h2c::start(
                                io,
                                &settings.config,
                                settings.idle_timeout,
                                self.cfg.read_timeout,
                            );
                            self.h2.insert(key.clone(), conn.clone());
                            conn
                        }
                    }
                }
            };
            let (result, left) = {
                let mut upload = match body {
                    Outgoing::Buffered => Upload::Buffered(&req.body),
                    Outgoing::Reader(r) => Upload::Reader(&mut **r),
                    Outgoing::Stream(s) => Upload::Stream(s.take()),
                };
                let result = h2c::exchange(
                    &conn,
                    fields.clone(),
                    &mut upload,
                    head_request,
                    self.cfg.read_timeout,
                    head_timeout,
                )
                .await;
                let left = match upload {
                    Upload::Stream(s) => s,
                    _ => None,
                };
                (result, left)
            };
            if let (Some(left), Outgoing::Stream(slot)) = (left, &mut *body) {
                *slot = Some(left);
            }
            failed = Some(conn);
            match result {
                Ok(head) => {
                    let resp = self.h2_response(req, head, deadline);
                    return Ok(H2Attempt::Done(Box::new(resp)));
                }
                Err(Failure::Refused { sent }) => {
                    let replayable = buffered || !sent;
                    if !replayable || attempt == 1 {
                        return Err(refused());
                    }
                }
                Err(Failure::Lost { sent, error }) => {
                    // A reused connection that died without GOAWAY (a TCP
                    // reset, a NAT timeout): like the HTTP/1.1 pool, retry
                    // once on a new connection when that cannot repeat a
                    // side effect.
                    let replayable = buffered || !sent;
                    let safe = reused && replayable && req.method.is_idempotent();
                    if !safe || attempt == 1 {
                        return Err(error);
                    }
                }
                Err(Failure::Error(e)) => return Err(e),
            }
        }
        Err(refused())
    }

    /// Opens a connection for HTTP/2: TLS offering `h2` with ALPN (returning
    /// whether the server chose it), or plain TCP for prior knowledge.
    #[cfg(feature = "http2")]
    async fn connect_h2(
        &self,
        target: &Uri,
        route: &Route<'_>,
        prior_knowledge: bool,
    ) -> Result<(Conn, bool), HttpError> {
        let open = async {
            if prior_knowledge {
                let tcp = transport::connect_tcp(&target.host, target.port).await?;
                return Ok((Box::new(tcp) as Conn, true));
            }
            let tls = self.h2.alpn_config(&self.tls()?);
            let conn: Conn = match route {
                Route::Tunnel(proxy) => {
                    let conn = self.dial(&proxy.uri).await?;
                    self.open_tunnel(conn, target, proxy).await?
                }
                _ => Box::new(transport::connect_tcp(&target.host, target.port).await?),
            };
            let (conn, alpn) = transport::tls_handshake_alpn(conn, &target.host, &tls).await?;
            Ok((conn, alpn.as_deref() == Some(transport::ALPN_H2)))
        };
        match self.cfg.connect_timeout {
            Some(limit) => match tokio::time::timeout(limit, open).await {
                Ok(result) => result,
                Err(_) => Err(HttpError::ConnectTimeout),
            },
            None => open.await,
        }
    }

    /// Wraps an HTTP/2 response head and body stream in a [`Response`].
    #[cfg(feature = "http2")]
    fn h2_response(
        &self,
        req: &Request,
        head: super::h2::client::Head,
        deadline: Option<Instant>,
    ) -> Response {
        let limits = BodyLimits {
            max_bytes: self.cfg.max_body,
            read_timeout: self.cfg.read_timeout,
            deadline,
            head: self.cfg.head_limits,
        };
        let status = StatusCode(head.status);
        let mut headers = head.headers;
        let body = Body::h2(head.body, limits, head.upload);
        let has_body = !matches!(req.method, Method::Head) && !matches!(head.status, 204 | 304);
        let body = self.decoded(body, &mut headers, has_body);
        Response::new(status, headers, req.uri.clone(), body).with_protocol(Protocol::Http2)
    }

    /// Applies transparent decompression when it is on and the body is
    /// encoded with a single supported coding.
    fn decoded(&self, mut body: Body, headers: &mut HeaderMap, has_body: bool) -> Body {
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
        body
    }

    /// Opens a connection for `target` along `route`, within the connect
    /// timeout.
    async fn connect(&self, target: &Uri, route: &Route<'_>) -> Result<Conn, HttpError> {
        let open = async {
            match route {
                Route::Direct => self.dial(target).await,
                Route::Forward(proxy) => self.dial(&proxy.uri).await,
                Route::Tunnel(proxy) => {
                    let conn = self.dial(&proxy.uri).await?;
                    let conn = self.open_tunnel(conn, target, proxy).await?;
                    transport::tls_handshake(conn, &target.host, &self.tls()?).await
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

    /// The TLS configuration for a handshake: the one given to
    /// [`ClientBuilder::tls`], else the shared default. Resolved only when a
    /// handshake happens, so plain-text traffic needs no crypto provider.
    fn tls(&self) -> Result<Arc<rustls::ClientConfig>, HttpError> {
        match &self.cfg.tls {
            Some(tls) => Ok(tls.clone()),
            None => transport::tls_config(),
        }
    }

    /// Opens a connection to the URI's host, wrapping it in TLS for `https`.
    async fn dial(&self, uri: &Uri) -> Result<Conn, HttpError> {
        if uri.is_tls() {
            transport::connect(uri, &self.tls()?).await
        } else {
            Ok(Box::new(transport::connect_tcp(&uri.host, uri.port).await?))
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
        slot: &mut Option<tokio::sync::OwnedSemaphorePermit>,
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
            Payload::Piped {
                head,
                encoding,
                body,
            } => {
                streamed = true;
                let idle = self.cfg.read_timeout;
                let write = async {
                    io::bounded(idle, reader.write_all(head)).await?;
                    io::write_piped_body(&mut reader, encoding, body, idle).await
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
        let body = if reusable {
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
        let body = body.holding(slot.take());

        let body = self.decoded(body, &mut headers, has_body);
        let protocol = match head.version {
            Version::Http10 => Protocol::Http10,
            Version::Http11 => Protocol::Http11,
        };
        Ok(Response::new(status, headers, req.uri.clone(), body).with_protocol(protocol))
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

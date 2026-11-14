//! HTTP client built on the in-house `ferroly::http` client, with codec-aware
//! bodies, pluggable auth, retry, circuit breaking and pagination reused from
//! `ferroly::clients`.

mod request;
mod response;

pub use request::RequestBuilder;
pub use response::Response;

use std::sync::Arc;
use std::time::Duration;

use ferroly::clients::{AuthProvider, CircuitBreakerConfig, CircuitBreakerRegistry, Policy, Retry};
use ferroly::http::{ClientBuilder, ClientTlsConfig, Interceptor, Method, Proxy, Uri};
use ferroly::rest::ClientError;

pub(crate) struct ClientInner {
    pub(crate) http: ferroly::http::Client,
    pub(crate) auth: Option<Arc<dyn AuthProvider>>,
    pub(crate) retry: Option<Retry>,
    pub(crate) default_content_type: String,
    pub(crate) base_url: Option<String>,
    pub(crate) default_headers: Vec<(String, String)>,
    pub(crate) default_query: Vec<(String, String)>,
    pub(crate) breakers: Option<Arc<CircuitBreakerRegistry>>,
    pub(crate) policy: Option<Policy>,
    pub(crate) error_for_status: bool,
    /// Set when the options could not be turned into a working client; every
    /// request then fails with this message instead of panicking at build
    /// time.
    pub(crate) config_error: Option<String>,
}

/// Where the transport comes from: options to build one, or a ready client.
enum Transport {
    Build(Box<ClientBuilder>),
    Ready(ferroly::http::Client),
}

/// Configuration for a [`Client`]. Build it with [`ClientOptions::builder`].
pub struct ClientOptions {
    transport: Transport,
    auth: Option<Arc<dyn AuthProvider>>,
    retry: Option<Retry>,
    default_content_type: Option<String>,
    base_url: Option<String>,
    default_headers: Vec<(String, String)>,
    default_query: Vec<(String, String)>,
    breakers: Option<Arc<CircuitBreakerRegistry>>,
    policy: Option<Policy>,
    error_for_status: bool,
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions {
            transport: Transport::Build(Box::new(ferroly::http::Client::builder())),
            auth: None,
            retry: None,
            default_content_type: None,
            base_url: None,
            default_headers: Vec::new(),
            default_query: Vec::new(),
            breakers: None,
            policy: None,
            error_for_status: false,
        }
    }
}

impl ClientOptions {
    /// Starts a builder.
    pub fn builder() -> ClientOptionsBuilder {
        ClientOptionsBuilder {
            options: ClientOptions::default(),
        }
    }
}

/// Fluent builder for [`ClientOptions`].
///
/// # Timeouts and limits
///
/// Nothing is unbounded by default. A client built without touching these
/// inherits the [`ferroly::http::Client`] defaults:
///
/// | Setting | Default | Method |
/// |---|---|---|
/// | connect (DNS + TCP + TLS) | 30 s | [`connect_timeout`](Self::connect_timeout) |
/// | response head | 60 s | [`request_timeout`](Self::request_timeout) |
/// | idle time between body reads | 300 s | [`read_timeout`](Self::read_timeout) |
/// | whole exchange | none | [`total_timeout`](Self::total_timeout) |
/// | response body size | 64 MiB | [`max_response_body_bytes`](Self::max_response_body_bytes) |
/// | redirects followed | 0 | [`max_redirects`](Self::max_redirects) |
#[derive(Default)]
#[must_use]
pub struct ClientOptionsBuilder {
    options: ClientOptions,
}

impl ClientOptionsBuilder {
    fn transport(mut self, f: impl FnOnce(ClientBuilder) -> ClientBuilder) -> Self {
        self.options.transport = match self.options.transport {
            Transport::Build(builder) => Transport::Build(Box::new(f(*builder))),
            ready => ready,
        };
        self
    }

    /// Sets the deadline for the **response head**: connecting, sending the
    /// request and receiving the status line and headers (default 60 s).
    /// Use [`total_timeout`](Self::total_timeout) to bound the whole
    /// exchange including the body.
    pub fn request_timeout(self, d: Duration) -> Self {
        self.transport(|b| b.timeout(Some(d)))
    }

    /// Sets the connect timeout: DNS, TCP connect, TLS handshake and any
    /// proxy tunnel (default 30 s).
    pub fn connect_timeout(self, d: Duration) -> Self {
        self.transport(|b| b.connect_timeout(Some(d)))
    }

    /// Sets the idle timeout while reading the response body: the longest
    /// wait for the next piece (default 300 s).
    pub fn read_timeout(self, d: Duration) -> Self {
        self.transport(|b| b.read_timeout(Some(d)))
    }

    /// Sets an overall deadline for one attempt, from sending until the last
    /// body byte, redirects included (default none).
    pub fn total_timeout(self, d: Duration) -> Self {
        self.transport(|b| b.total_timeout(Some(d)))
    }

    /// Caps the size of a response body (default 64 MiB). A larger response
    /// fails with a `BodyTooLarge` transport error.
    pub fn max_response_body_bytes(self, max: u64) -> Self {
        self.transport(|b| b.max_response_body_bytes(Some(max)))
    }

    /// Routes requests through a proxy.
    pub fn proxy(self, proxy: Proxy) -> Self {
        self.transport(|b| b.proxy(proxy))
    }

    /// Takes the proxy configuration from the environment (`HTTPS_PROXY`,
    /// `NO_PROXY`, …). Never done implicitly.
    pub fn proxy_from_env(self) -> Self {
        self.transport(|b| b.proxy_from_env())
    }

    /// Sets the TLS configuration: extra or replacement trust roots, a
    /// client certificate, the minimum protocol version. Server certificates
    /// are always verified.
    pub fn tls(self, tls: ClientTlsConfig) -> Self {
        self.transport(|b| b.tls(tls))
    }

    /// Follows up to `max` redirects (default 0: a `3xx` is returned as it
    /// is). Credentials are dropped on a redirect to another origin.
    pub fn max_redirects(self, max: usize) -> Self {
        self.transport(|b| b.max_redirects(max))
    }

    /// Adds a transport-level [`Interceptor`] (it sees every attempt and
    /// every redirect hop).
    pub fn interceptor(self, interceptor: impl Interceptor + 'static) -> Self {
        self.transport(|b| b.interceptor(interceptor))
    }

    /// Sets the `User-Agent` sent when a request does not set one.
    pub fn user_agent(self, value: impl Into<String>) -> Self {
        self.transport(|b| b.user_agent(value))
    }

    /// Whether to ask for and decode `gzip` / `deflate` response bodies
    /// (default off).
    pub fn decompress(self, enabled: bool) -> Self {
        self.transport(|b| b.decompress(enabled))
    }

    /// Uses an already built [`ferroly::http::Client`] as the transport —
    /// to share one connection pool between clients. The transport settings
    /// above (timeouts, body cap, proxy, TLS, redirects, interceptors, user
    /// agent, decompression) are then taken from that client; calls to those
    /// methods on this builder have no effect.
    pub fn http_client(mut self, client: ferroly::http::Client) -> Self {
        self.options.transport = Transport::Ready(client);
        self
    }

    /// Sets the authentication provider applied to every request. Providers
    /// that refresh credentials (OAuth 2.0, [`RefreshingAuth`]) are prepared
    /// before each attempt.
    ///
    /// The credentials are sent to whatever URL the request names; do not
    /// build request URLs from untrusted input on an authenticated client.
    ///
    /// [`RefreshingAuth`]: ferroly::clients::RefreshingAuth
    pub fn auth(mut self, auth: Arc<dyn AuthProvider>) -> Self {
        self.options.auth = Some(auth);
        self
    }

    /// Enables automatic retries. Accepts a
    /// [`RetryPolicy`](ferroly::clients::RetryPolicy) (the backoff schedule)
    /// or a full [`Retry`] with jitter, a time limit and a shared retry
    /// budget:
    ///
    /// ```
    /// use std::time::Duration;
    /// use ferroly::clients::{Jitter, RetryPolicy};
    /// use ferroly::rest::ClientOptions;
    ///
    /// let options = ClientOptions::builder()
    ///     .retry_policy(
    ///         RetryPolicy::exponential(3, Duration::from_millis(100), Duration::from_secs(2))
    ///             .with_jitter(Jitter::Full)
    ///             .max_elapsed(Duration::from_secs(10)),
    ///     )
    ///     .build();
    /// # let _ = options;
    /// ```
    ///
    /// What is retried:
    ///
    /// - **Idempotent requests** (`GET`, `HEAD`, `PUT`, `DELETE`, `OPTIONS`,
    ///   `TRACE`) are retried on a transient transport error (connection
    ///   failure, timeout, malformed response) and on a `502`, `503`, `504`
    ///   or `429` response. A `Retry-After` header on such a response is
    ///   honoured — the client waits exactly that long — unless it exceeds
    ///   the retry's
    ///   [`max_retry_after`](ferroly::clients::Retry::max_retry_after) cap
    ///   (5 minutes by default), in which case the response is returned.
    /// - **Non-idempotent requests** (`POST`, `PATCH`) are **not** retried,
    ///   because the server may have acted on the first attempt — except
    ///   after a connect timeout, when nothing was sent. Opt a request in
    ///   with [`RequestBuilder::idempotency_key`] (or
    ///   [`RequestBuilder::idempotent`]) when the server deduplicates.
    /// - `500` and other statuses, TLS failures, invalid URLs and oversized
    ///   bodies are never retried.
    ///
    /// When the retries run out on a retryable status, the last response is
    /// returned (as `Ok`, like any other response).
    pub fn retry_policy(mut self, retry: impl Into<Retry>) -> Self {
        self.options.retry = Some(retry.into());
        self
    }

    /// Sets the default request content type (default `application/json`).
    pub fn default_content_type(mut self, ct: impl Into<String>) -> Self {
        self.options.default_content_type = Some(ct.into());
        self
    }

    /// Sets a base URL. A request URL without a scheme (`/users/${id}`,
    /// `users`) is appended to it; an absolute request URL is used as given.
    ///
    /// ```
    /// use ferroly::rest::{Client, ClientOptions};
    ///
    /// let client = Client::with_options(
    ///     ClientOptions::builder().base_url("https://api.example.com/v1").build(),
    /// );
    /// let _request = client.get("/users/${id}").path_param("id", "42");
    /// // -> https://api.example.com/v1/users/42
    /// ```
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.options.base_url = Some(url.into());
        self
    }

    /// Adds a header sent with every request (a request's own
    /// [`header`](RequestBuilder::header) of the same name replaces it).
    pub fn default_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.options
            .default_headers
            .push((name.into(), value.into()));
        self
    }

    /// Adds a query parameter sent with every request (an API version or
    /// key).
    pub fn default_query(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.options.default_query.push((key.into(), value.into()));
        self
    }

    /// Protects each destination with a circuit breaker: one breaker per
    /// `host:port`, created on first use with `config`.
    ///
    /// A transport failure, a `5xx` or a `429` counts as a failure of the
    /// destination. Other `4xx` responses and errors that are the caller's
    /// (an invalid URL, a failed credential refresh) are **ignored** — they
    /// cannot open the circuit. While a breaker is open, requests to that
    /// destination fail at once with [`ClientError::CircuitOpen`] and are not
    /// retried.
    pub fn circuit_breaker(mut self, config: CircuitBreakerConfig) -> Self {
        self.options.breakers = Some(Arc::new(CircuitBreakerRegistry::new(config)));
        self
    }

    /// Like [`circuit_breaker`](Self::circuit_breaker), with a registry you
    /// own — to share breakers between clients, tune them per host, or
    /// observe their state.
    pub fn circuit_breakers(mut self, registry: Arc<CircuitBreakerRegistry>) -> Self {
        self.options.breakers = Some(registry);
        self
    }

    /// Runs every request through a [`Policy`] — overall timeout, retry,
    /// circuit breaker, bulkhead, rate limiter, per-attempt timeout.
    ///
    /// The policy replaces [`retry_policy`](Self::retry_policy) (which is
    /// then not consulted): configure retries on the policy itself. The same
    /// idempotency rules apply — a non-idempotent request is never retried
    /// unless the request opted in. A per-host
    /// [`circuit_breaker`](Self::circuit_breaker) set here still applies to
    /// each attempt.
    pub fn policy(mut self, policy: Policy) -> Self {
        self.options.policy = Some(policy);
        self
    }

    /// When `true`, [`RequestBuilder::send`] fails with
    /// [`ClientError::Status`] for any response whose status is not 2xx,
    /// instead of returning it. Default `false`; see [`Client`].
    pub fn error_for_status(mut self, enabled: bool) -> Self {
        self.options.error_for_status = enabled;
        self
    }

    /// Finalizes the options.
    pub fn build(self) -> ClientOptions {
        self.options
    }
}

/// An HTTP client with codec-aware request/response bodies.
///
/// # A non-2xx response is not an error — by default
///
/// [`RequestBuilder::send`] returns `Ok(Response)` for **every** response the
/// server produced, including `404` and `500`. An `Err` means no response was
/// obtained. Check the status, or make failures impossible to overlook:
///
/// ```no_run
/// use ferroly::rest::{Client, ClientOptions};
///
/// # async fn ex() -> Result<(), ferroly::rest::ClientError> {
/// let client = Client::new();
///
/// // Per request: turn a non-2xx response into ClientError::Status.
/// let user = client.get("https://api.example.com/users/1").send().await?.error_for_status()?;
/// # let _ = user;
///
/// // Or for every request of a client:
/// let strict = Client::with_options(ClientOptions::builder().error_for_status(true).build());
/// # let _ = strict;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    /// Creates a client with default options.
    pub fn new() -> Self {
        Self::with_options(ClientOptions::default())
    }

    /// Creates a client from explicit options.
    ///
    /// This never fails. If the options cannot produce a working transport
    /// (an invalid TLS configuration), every request of the returned client
    /// fails with [`ClientError::InvalidRequest`] naming the problem; use
    /// [`try_with_options`](Self::try_with_options) to find out up front.
    pub fn with_options(options: ClientOptions) -> Self {
        Self::assemble(options).0
    }

    /// Creates a client from explicit options, reporting options that cannot
    /// produce a working transport.
    pub fn try_with_options(options: ClientOptions) -> Result<Self, ClientError> {
        match Self::assemble(options) {
            (_, Some(problem)) => Err(ClientError::InvalidRequest(problem)),
            (client, None) => Ok(client),
        }
    }

    /// Builds the client and, if the transport options were unusable, the
    /// description of the problem (which the client then also reports on
    /// every request).
    fn assemble(options: ClientOptions) -> (Self, Option<String>) {
        let mut config_error = None;
        let http = match options.transport {
            Transport::Ready(client) => client,
            Transport::Build(builder) => match builder.build() {
                Ok(client) => client,
                Err(error) => {
                    config_error = Some(format!("client configuration is invalid: {error}"));
                    ferroly::http::Client::new()
                }
            },
        };
        let client = Client {
            inner: Arc::new(ClientInner {
                http,
                auth: options.auth,
                retry: options.retry,
                default_content_type: options
                    .default_content_type
                    .unwrap_or_else(|| "application/json".to_string()),
                base_url: options.base_url,
                default_headers: options.default_headers,
                default_query: options.default_query,
                breakers: options.breakers,
                policy: options.policy,
                error_for_status: options.error_for_status,
                config_error: config_error.clone(),
            }),
        };
        (client, config_error)
    }

    /// Starts building a request for `url` with the given method.
    pub fn request(&self, url: impl Into<String>, method: Method) -> RequestBuilder {
        RequestBuilder::new(self.inner.clone(), method, url.into())
    }

    /// Shorthand for a `GET` request.
    pub fn get(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(url, Method::Get)
    }

    /// Shorthand for a `POST` request.
    pub fn post(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(url, Method::Post)
    }

    /// Shorthand for a `PUT` request.
    pub fn put(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(url, Method::Put)
    }

    /// Shorthand for a `DELETE` request.
    pub fn delete(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(url, Method::Delete)
    }

    /// Shorthand for a `PATCH` request.
    pub fn patch(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(url, Method::Patch)
    }

    /// Shorthand for a `HEAD` request.
    pub fn head(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(url, Method::Head)
    }

    /// Shorthand for an `OPTIONS` request.
    pub fn options(&self, url: impl Into<String>) -> RequestBuilder {
        self.request(url, Method::Options)
    }

    /// Fetches a paginated collection page by page: sends `first`, hands the
    /// response to `next`, sends the request it returns, and so on until
    /// `next` returns `None` or `max_pages` pages have been fetched. This is
    /// the form for cursor- and offset-based APIs:
    ///
    /// ```no_run
    /// use ferroly::codec::Value;
    /// use ferroly::rest::Client;
    ///
    /// # async fn ex() -> Result<(), ferroly::rest::ClientError> {
    /// let client = Client::new();
    /// let pages = client
    ///     .paginate(client.get("https://api.example.com/items"), 50, |page| {
    ///         let body: Value = page.decode().ok()?;
    ///         let cursor = body.get("next_cursor")?.as_str()?.to_string();
    ///         Some(client.get("https://api.example.com/items").query("cursor", cursor))
    ///     })
    ///     .await?;
    /// # let _ = pages;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// A page with a non-2xx status ends the walk with
    /// [`ClientError::Status`], so a failure part-way through cannot be
    /// mistaken for the end of the collection. `max_pages` bounds a server
    /// that never stops handing out cursors.
    pub async fn paginate<F>(
        &self,
        first: RequestBuilder,
        max_pages: usize,
        mut next: F,
    ) -> Result<Vec<Response>, ClientError>
    where
        F: FnMut(&Response) -> Option<RequestBuilder>,
    {
        let mut pages = Vec::new();
        let mut request = Some(first);
        while let Some(builder) = request.take() {
            if pages.len() >= max_pages {
                break;
            }
            let page = builder.send().await?.error_for_status()?;
            request = next(&page);
            pages.push(page);
        }
        Ok(pages)
    }

    /// Fetches a paginated collection by following `Link: <…>; rel="next"`
    /// response headers (RFC 8288), up to `max_pages` pages.
    ///
    /// Each following request repeats the method and headers of `first`. A
    /// relative link is resolved against the URL of the page it came from. A
    /// link to a **different origin** is refused with
    /// [`ClientError::InvalidRequest`] rather than followed, so a response
    /// cannot redirect the client's credentials to another host. Like
    /// [`paginate`](Self::paginate), a non-2xx page is an error.
    pub async fn paginate_links(
        &self,
        first: RequestBuilder,
        max_pages: usize,
    ) -> Result<Vec<Response>, ClientError> {
        let template = first.template();
        let mut pages = Vec::new();
        let mut request = Some(first);
        while let Some(builder) = request.take() {
            if pages.len() >= max_pages {
                break;
            }
            let page = builder.send().await?.error_for_status()?;
            if let Some(next) = page.next_link() {
                let same_origin = match (Uri::parse(page.url()), Uri::parse(&next)) {
                    (Ok(from), Ok(to)) => from.same_origin(&to),
                    _ => false,
                };
                if !same_origin {
                    return Err(ClientError::InvalidRequest(
                        "pagination link points to a different origin".to_string(),
                    ));
                }
                request = Some(template.request(self.inner.clone(), next));
            }
            pages.push(page);
        }
        Ok(pages)
    }
}

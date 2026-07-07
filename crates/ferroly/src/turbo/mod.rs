//! A first-class, in-house HTTP router and server.
//!
//! Built directly on [`ferroly::http`] — routing, path parameters, query
//! parsing, filters, and an authenticator hook, with no external web framework.
//!
//! ```no_run
//! use ferroly::turbo::Router;
//! use ferroly::http::{HttpResponse, StatusCode};
//!
//! # async fn ex() -> Result<(), ferroly::http::HttpError> {
//! let router = Router::new()
//!     .get("/greet/:name", |ctx| async move {
//!         let name = ctx.param("name").unwrap_or("world").to_string();
//!         HttpResponse::text(StatusCode::OK, format!("hi {name}"))
//!     });
//! router.serve("127.0.0.1:8080").await
//! # }
//! ```

#![deny(missing_docs)]

mod error;

pub use error::ParamError;

use std::collections::HashMap;
use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;

use tokio::net::{TcpListener, ToSocketAddrs};

use ferroly::codec::{CodecError, Decode, Encode};
use ferroly::http::{
    serve, BoxFuture, HttpError, HttpHandler, HttpResponse, Method, Request, StatusCode,
};

/// The per-request context passed to handlers.
pub struct Ctx {
    request: Request,
    params: HashMap<String, String>,
}

impl Ctx {
    /// The request method.
    pub fn method(&self) -> &Method {
        &self.request.method
    }

    /// The request path.
    pub fn path(&self) -> &str {
        &self.request.uri.path
    }

    /// A captured path parameter (e.g. `:name`).
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.get(name).map(String::as_str)
    }

    /// A path parameter parsed into `T` via [`FromStr`].
    pub fn param_as<T>(&self, name: &str) -> Result<T, ParamError>
    where
        T: FromStr,
        T::Err: std::fmt::Display,
    {
        let raw = self
            .param(name)
            .ok_or_else(|| ParamError::Missing(name.to_string()))?;
        raw.parse().map_err(|e: T::Err| ParamError::Invalid {
            name: name.to_string(),
            reason: e.to_string(),
        })
    }

    /// A query-string parameter (percent-decoded).
    pub fn query_param(&self, name: &str) -> Option<String> {
        let q = self.request.uri.query.as_deref()?;
        for pair in q.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            if k == name {
                return Some(percent_decode(v));
            }
        }
        None
    }

    /// A query parameter parsed as an integer (`None` if absent or unparseable).
    pub fn query_int(&self, name: &str) -> Option<i64> {
        self.query_param(name)?.trim().parse().ok()
    }

    /// A query parameter parsed as a float (`None` if absent or unparseable).
    pub fn query_float(&self, name: &str) -> Option<f64> {
        self.query_param(name)?.trim().parse().ok()
    }

    /// A query parameter parsed as a boolean — `true`/`1`/`yes`/`on` and
    /// `false`/`0`/`no`/`off` (case-insensitive); `None` otherwise.
    pub fn query_bool(&self, name: &str) -> Option<bool> {
        match self.query_param(name)?.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        }
    }

    /// A query parameter parsed into `T` via [`FromStr`], erroring like
    /// [`param_as`](Self::param_as) when absent or invalid.
    pub fn query_as<T>(&self, name: &str) -> Result<T, ParamError>
    where
        T: FromStr,
        T::Err: std::fmt::Display,
    {
        let raw = self
            .query_param(name)
            .ok_or_else(|| ParamError::Missing(name.to_string()))?;
        raw.trim().parse().map_err(|e: T::Err| ParamError::Invalid {
            name: name.to_string(),
            reason: e.to_string(),
        })
    }

    /// A request header value.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.request.headers.get(name)
    }

    /// The raw request body bytes.
    pub fn body(&self) -> &[u8] {
        &self.request.body
    }

    /// Decodes the body into `T`, selecting the codec from `Content-Type`
    /// (defaulting to JSON).
    pub fn read<T: Decode>(&self) -> Result<T, CodecError> {
        let ct = self.header("content-type").unwrap_or("application/json");
        ferroly::codec::decode(ct, &self.request.body)
    }

    /// Encodes `value` as the response body, picking the codec from the request's
    /// `Accept` header (JSON / XML / YAML / TOML; JSON when `Accept` is absent or
    /// `*/*`). Returns `406 Not Acceptable` if no supported type matches.
    pub fn respond<T: Encode>(&self, status: StatusCode, value: &T) -> HttpResponse {
        let accept = self.header("accept").unwrap_or("");
        match negotiate_content_type(accept) {
            Some(ct) => match ferroly::codec::encode(ct, value) {
                Ok(bytes) => HttpResponse::new(status)
                    .header("content-type", ct)
                    .body(bytes),
                Err(_) => HttpResponse::text(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "500 response encode error",
                ),
            },
            None => HttpResponse::text(StatusCode::NOT_ACCEPTABLE, "406 Not Acceptable"),
        }
    }

    /// Consumes the context, returning the underlying request.
    pub fn into_request(self) -> Request {
        self.request
    }
}

type Handler = Arc<dyn Fn(Ctx) -> BoxFuture<'static, HttpResponse> + Send + Sync>;
type Filter = Arc<dyn Fn(&Ctx) -> Option<HttpResponse> + Send + Sync>;
type Middleware = Arc<dyn Fn(Ctx, Next) -> BoxFuture<'static, HttpResponse> + Send + Sync>;

enum Seg {
    Static(String),
    Param(String),
}

struct Route {
    methods: Vec<Method>,
    segments: Vec<Seg>,
    /// Whether the registered pattern ended with `/` (used by strict-slash).
    trailing_slash: bool,
    handler: Handler,
}

/// A first-class HTTP router. Finalize it with [`Router::into_handler`] (or
/// [`Router::serve`]) into an [`HttpHandler`] to serve directly or embed in
/// [`crate::rest`]'s server.
#[derive(Default)]
#[must_use]
pub struct Router {
    routes: Vec<Route>,
    filters: Vec<Filter>,
    auth: Option<Filter>,
    middlewares: Vec<Middleware>,
    not_found: Option<Handler>,
    method_not_allowed: Option<Handler>,
    strict_slash: bool,
}

impl Router {
    /// Creates an empty router.
    pub fn new() -> Self {
        Self::default()
    }

    fn register<F, Fut>(&mut self, path: &str, handler: F, methods: Vec<Method>)
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        let handler: Handler = Arc::new(move |ctx| Box::pin(handler(ctx)));
        self.routes.push(Route {
            methods,
            segments: parse_pattern(path),
            trailing_slash: path.len() > 1 && path.ends_with('/'),
            handler,
        });
    }

    /// Registers a `GET` handler.
    pub fn get<F, Fut>(mut self, path: &str, handler: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        self.register(path, handler, vec![Method::Get]);
        self
    }

    /// Registers a `POST` handler.
    pub fn post<F, Fut>(mut self, path: &str, handler: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        self.register(path, handler, vec![Method::Post]);
        self
    }

    /// Registers a `PUT` handler.
    pub fn put<F, Fut>(mut self, path: &str, handler: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        self.register(path, handler, vec![Method::Put]);
        self
    }

    /// Registers a `DELETE` handler.
    pub fn delete<F, Fut>(mut self, path: &str, handler: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        self.register(path, handler, vec![Method::Delete]);
        self
    }

    /// Registers a `PATCH` handler.
    pub fn patch<F, Fut>(mut self, path: &str, handler: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        self.register(path, handler, vec![Method::Patch]);
        self
    }

    /// Registers a `HEAD` handler.
    pub fn head<F, Fut>(mut self, path: &str, handler: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        self.register(path, handler, vec![Method::Head]);
        self
    }

    /// Registers an `OPTIONS` handler.
    pub fn options<F, Fut>(mut self, path: &str, handler: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        self.register(path, handler, vec![Method::Options]);
        self
    }

    /// Sets the trailing-slash policy. Default `false` (lenient — `/x` and `/x/`
    /// are the same route). When `true`, they are distinct.
    pub fn strict_slash(mut self, strict: bool) -> Self {
        self.strict_slash = strict;
        self
    }

    /// Registers one handler for several methods at once.
    pub fn add<F, Fut>(mut self, path: &str, handler: F, methods: Vec<Method>) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        self.register(path, handler, methods);
        self
    }

    /// Adds a filter that runs before routing and may short-circuit with a
    /// response. Filters run in registration order.
    pub fn filter<F>(mut self, f: F) -> Self
    where
        F: Fn(&Ctx) -> Option<HttpResponse> + Send + Sync + 'static,
    {
        self.filters.push(Arc::new(f));
        self
    }

    /// Sets the authenticator, which runs *before* all filters — authentication
    /// is always checked first.
    pub fn auth<F>(mut self, f: F) -> Self
    where
        F: Fn(&Ctx) -> Option<HttpResponse> + Send + Sync + 'static,
    {
        self.auth = Some(Arc::new(f));
        self
    }

    /// Sets an HS256-JWT authenticator: requires a valid `Authorization: Bearer
    /// <token>` verified against `secret`, else responds `401`. Requires the
    /// `auth` feature. (Claims are validated; handlers wanting the claims can
    /// re-verify with [`ferroly::auth::decode_hs256`](crate::auth::decode_hs256).)
    #[cfg(feature = "auth")]
    pub fn jwt_auth(self, secret: impl Into<Vec<u8>>) -> Self {
        let secret = secret.into();
        self.auth(move |ctx| {
            let token = ctx.header("authorization").and_then(|h| {
                h.strip_prefix("Bearer ")
                    .or_else(|| h.strip_prefix("bearer "))
            });
            match token {
                Some(t) if ferroly::auth::decode_hs256(t, &secret).is_ok() => None,
                Some(_) => Some(HttpResponse::text(
                    StatusCode::UNAUTHORIZED,
                    "401 invalid token",
                )),
                None => Some(HttpResponse::text(
                    StatusCode::UNAUTHORIZED,
                    "401 missing bearer token",
                )),
            }
        })
    }

    /// Adds a token-bucket rate-limit filter. Requests are keyed by `key_of`
    /// (e.g. client IP or an API key); each key gets `burst` tokens that refill
    /// at `per_second` per second. Over-limit requests short-circuit with `429`.
    pub fn rate_limit<K>(self, per_second: f64, burst: f64, key_of: K) -> Self
    where
        K: Fn(&Ctx) -> String + Send + Sync + 'static,
    {
        use std::sync::Mutex;
        use std::time::{Duration, Instant};

        /// How often to sweep idle (fully-refilled) buckets so the map cannot
        /// grow without bound under high-cardinality / rotating keys.
        const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

        struct State {
            buckets: HashMap<String, (f64, Instant)>,
            last_sweep: Instant,
        }
        let state = Arc::new(Mutex::new(State {
            buckets: HashMap::new(),
            last_sweep: Instant::now(),
        }));
        self.filter(move |ctx| {
            let key = key_of(ctx);
            let now = Instant::now();
            let mut st = state.lock().unwrap();

            // Periodically drop fully-refilled (idle) buckets. Evicting a full
            // bucket is lossless: a returning client is re-inserted with an
            // identical full bucket, so this never grants extra allowance.
            if now.duration_since(st.last_sweep) >= SWEEP_INTERVAL {
                st.last_sweep = now;
                st.buckets.retain(|_, (tokens, last)| {
                    let refilled =
                        (*tokens + now.duration_since(*last).as_secs_f64() * per_second).min(burst);
                    refilled < burst
                });
            }

            let entry = st.buckets.entry(key).or_insert((burst, now));
            let elapsed = now.duration_since(entry.1).as_secs_f64();
            entry.1 = now;
            entry.0 = (entry.0 + elapsed * per_second).min(burst);
            if entry.0 >= 1.0 {
                entry.0 -= 1.0;
                None
            } else {
                Some(HttpResponse::text(
                    StatusCode::TOO_MANY_REQUESTS,
                    "429 Too Many Requests",
                ))
            }
        })
    }

    /// Registers a group of routes under a shared path `prefix`, with optional
    /// group-scoped filters. The closure receives a [`Group`] builder.
    ///
    /// ```
    /// use ferroly::turbo::Router;
    /// use ferroly::http::{HttpResponse, StatusCode};
    ///
    /// let _router = Router::new().group("/api/v1", |g| {
    ///     g.get("/health", |_| async { HttpResponse::text(StatusCode::OK, "ok") })
    ///      .post("/items", |_| async { HttpResponse::text(StatusCode::CREATED, "made") })
    /// });
    /// ```
    pub fn group(mut self, prefix: &str, build: impl FnOnce(Group) -> Group) -> Self {
        let group = build(Group::new(prefix));
        self.routes.extend(group.routes);
        self.filters.extend(group.filters);
        self
    }

    /// Sets a custom 404 handler.
    pub fn on_not_found<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        self.not_found = Some(Arc::new(move |ctx| Box::pin(handler(ctx))));
        self
    }

    /// Sets a custom 405 handler.
    pub fn on_method_not_allowed<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        self.method_not_allowed = Some(Arc::new(move |ctx| Box::pin(handler(ctx))));
        self
    }

    /// Adds an "onion" middleware that wraps the rest of the chain — it runs
    /// code **before and after** the inner handler and may inspect or modify the
    /// response. Middlewares run in registration order (first registered is
    /// outermost / runs first) and wrap the authenticator, filters, routing, and
    /// handlers.
    ///
    /// Call [`Next::run`] to invoke the remainder of the chain, or return without
    /// calling it to short-circuit.
    ///
    /// ```
    /// use ferroly::turbo::Router;
    /// use ferroly::http::{HttpResponse, StatusCode};
    ///
    /// let _router: Router = Router::new()
    ///     .layer(|ctx, next| async move {
    ///         let resp = next.run(ctx).await;              // run inner chain
    ///         resp.header("X-Powered-By", "ferroly")       // post-process
    ///     })
    ///     .get("/", |_ctx| async move { HttpResponse::text(StatusCode::OK, "ok") });
    /// ```
    pub fn layer<F, Fut>(mut self, middleware: F) -> Self
    where
        F: Fn(Ctx, Next) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        self.middlewares
            .push(Arc::new(move |ctx, next| Box::pin(middleware(ctx, next))));
        self
    }

    /// Adds an access-log middleware: one [`Level::Info`](ferroly::log::Level)
    /// line per request via `logger` with method, path, status, and duration.
    /// Requires the `log` feature.
    #[cfg(feature = "log")]
    pub fn access_log(self, logger: ferroly::log::Logger) -> Self {
        self.layer(move |ctx, next| {
            let logger = logger.clone();
            async move {
                let method = ctx.method().as_str().to_string();
                let path = ctx.path().to_string();
                let start = std::time::Instant::now();
                let resp = next.run(ctx).await;
                let ms = start.elapsed().as_millis() as u64;
                let status = resp.status.as_u16();
                logger.info(
                    "request",
                    &[
                        ("method", method.into()),
                        ("path", path.into()),
                        ("status", status.into()),
                        ("ms", ms.into()),
                    ],
                );
                resp
            }
        })
    }

    /// Records RED metrics for every request into the process-global
    /// [`metrics::Registry`](ferroly::metrics::global): `http_requests_total`
    /// (counter, labelled by `method` + `status`),
    /// `http_request_duration_seconds` (histogram, labelled by `method`), and
    /// `http_requests_in_flight` (gauge). Pair with
    /// [`metrics_route`](Self::metrics_route) to expose them. Requires the
    /// `metrics` feature.
    #[cfg(feature = "metrics")]
    pub fn metrics(self) -> Self {
        use ferroly::metrics::{global, DEFAULT_BUCKETS};
        self.layer(|ctx, next| async move {
            let method = ctx.method().as_str().to_string();
            let in_flight =
                global().gauge("http_requests_in_flight", "In-flight HTTP requests", &[]);
            in_flight.inc();
            let start = std::time::Instant::now();
            let resp = next.run(ctx).await;
            let elapsed = start.elapsed().as_secs_f64();
            in_flight.dec();
            let status = resp.status.as_u16().to_string();
            global()
                .counter(
                    "http_requests_total",
                    "Total HTTP requests",
                    &[("method", &method), ("status", &status)],
                )
                .inc();
            global()
                .histogram(
                    "http_request_duration_seconds",
                    "HTTP request latency in seconds",
                    &[("method", &method)],
                    DEFAULT_BUCKETS,
                )
                .observe(elapsed);
            resp
        })
    }

    /// Registers a `GET` route at `path` that serves the process-global
    /// [`metrics::Registry`](ferroly::metrics::global) in the Prometheus text
    /// exposition format. Requires the `metrics` feature.
    #[cfg(feature = "metrics")]
    pub fn metrics_route(self, path: &str) -> Self {
        self.get(path, |_ctx| async move {
            HttpResponse::text(StatusCode::OK, ferroly::metrics::global().encode())
                .header("content-type", "text/plain; version=0.0.4; charset=utf-8")
        })
    }

    /// Wraps each request in a logging context carrying a `trace_id` (taken from
    /// the `x-request-id` header, or generated), so **every** `ferroly::log`
    /// record emitted while handling that request automatically includes it —
    /// request correlation without threading a logger. Requires the `log`
    /// feature; pair with [`access_log`](Self::access_log) or the global logger.
    #[cfg(feature = "log")]
    pub fn trace_context(self) -> Self {
        // Register the provider that reads the task-local, exactly once.
        TRACE_PROVIDER_INIT.call_once(|| {
            ferroly::log::add_context_provider(|| {
                LOG_FIELDS.try_with(|f| f.clone()).unwrap_or_default()
            });
        });
        self.layer(|ctx, next| {
            let trace_id = ctx
                .header("x-request-id")
                .map(str::to_string)
                .unwrap_or_else(gen_trace_id);
            let fields = vec![("trace_id".to_string(), ferroly::codec::Value::Str(trace_id))];
            async move { LOG_FIELDS.scope(fields, next.run(ctx)).await }
        })
    }

    /// Wraps the router as a shared [`HttpHandler`].
    pub fn into_handler(self) -> Arc<dyn HttpHandler> {
        Arc::new(RouterService {
            shared: Arc::new(Shared {
                routes: self.routes,
                filters: self.filters,
                auth: self.auth,
                middlewares: self.middlewares,
                not_found: self.not_found,
                method_not_allowed: self.method_not_allowed,
                strict_slash: self.strict_slash,
            }),
        })
    }

    /// Binds `addr` and serves until the process ends.
    pub async fn serve(self, addr: impl ToSocketAddrs) -> Result<(), HttpError> {
        let listener = TcpListener::bind(addr).await?;
        serve(listener, self.into_handler(), std::future::pending()).await
    }
}

/// A builder for a [`Router::group`] — routes under a shared prefix, plus
/// filters scoped to that prefix.
pub struct Group {
    prefix: String,
    routes: Vec<Route>,
    filters: Vec<Filter>,
}

impl Group {
    fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.trim_end_matches('/').to_string(),
            routes: Vec::new(),
            filters: Vec::new(),
        }
    }

    fn full(&self, path: &str) -> String {
        format!("{}{}", self.prefix, path)
    }

    fn register<F, Fut>(&mut self, path: &str, handler: F, methods: Vec<Method>)
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        let full = self.full(path);
        let handler: Handler = Arc::new(move |ctx| Box::pin(handler(ctx)));
        self.routes.push(Route {
            methods,
            segments: parse_pattern(&full),
            trailing_slash: full.len() > 1 && full.ends_with('/'),
            handler,
        });
    }

    /// Registers a `GET` handler under the group prefix.
    pub fn get<F, Fut>(mut self, path: &str, handler: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        self.register(path, handler, vec![Method::Get]);
        self
    }

    /// Registers a `POST` handler under the group prefix.
    pub fn post<F, Fut>(mut self, path: &str, handler: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        self.register(path, handler, vec![Method::Post]);
        self
    }

    /// Registers a `PUT` handler under the group prefix.
    pub fn put<F, Fut>(mut self, path: &str, handler: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        self.register(path, handler, vec![Method::Put]);
        self
    }

    /// Registers a `DELETE` handler under the group prefix.
    pub fn delete<F, Fut>(mut self, path: &str, handler: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        self.register(path, handler, vec![Method::Delete]);
        self
    }

    /// Registers a `PATCH` handler under the group prefix.
    pub fn patch<F, Fut>(mut self, path: &str, handler: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        self.register(path, handler, vec![Method::Patch]);
        self
    }

    /// Adds a filter that runs only for requests under this group's prefix.
    pub fn filter<F>(mut self, f: F) -> Self
    where
        F: Fn(&Ctx) -> Option<HttpResponse> + Send + Sync + 'static,
    {
        let prefix = self.prefix.clone();
        self.filters.push(Arc::new(move |ctx| {
            if ctx.path().starts_with(&prefix) {
                f(ctx)
            } else {
                None
            }
        }));
        self
    }
}

/// The finalized, shareable router state.
struct Shared {
    routes: Vec<Route>,
    filters: Vec<Filter>,
    auth: Option<Filter>,
    middlewares: Vec<Middleware>,
    not_found: Option<Handler>,
    method_not_allowed: Option<Handler>,
    strict_slash: bool,
}

struct RouterService {
    shared: Arc<Shared>,
}

impl HttpHandler for RouterService {
    fn handle(&self, req: Request) -> BoxFuture<'_, HttpResponse> {
        let shared = self.shared.clone();
        Box::pin(async move {
            let ctx = Ctx {
                request: req,
                params: HashMap::new(),
            };
            Next { shared, index: 0 }.run(ctx).await
        })
    }
}

/// The continuation passed to a middleware. Running it invokes the remaining
/// middlewares and, finally, routing plus the matched handler.
pub struct Next {
    shared: Arc<Shared>,
    index: usize,
}

impl Next {
    /// Runs the remainder of the chain with `ctx`, returning its response.
    pub fn run(self, ctx: Ctx) -> BoxFuture<'static, HttpResponse> {
        Box::pin(async move {
            if self.index < self.shared.middlewares.len() {
                let mw = self.shared.middlewares[self.index].clone();
                let next = Next {
                    shared: self.shared.clone(),
                    index: self.index + 1,
                };
                mw(ctx, next).await
            } else {
                core_dispatch(&self.shared, ctx).await
            }
        })
    }
}

/// The innermost layer: authenticator, filters, routing, and the handler.
async fn core_dispatch(shared: &Shared, ctx: Ctx) -> HttpResponse {
    if let Some(auth) = &shared.auth {
        if let Some(resp) = auth(&ctx) {
            return resp;
        }
    }
    for f in &shared.filters {
        if let Some(resp) = f(&ctx) {
            return resp;
        }
    }

    let path = ctx.request.uri.path.clone();
    let req_trailing = path.len() > 1 && path.ends_with('/');
    let mut chosen: Option<(Handler, HashMap<String, String>)> = None;
    let mut path_exists = false;
    // Methods registered for this path, for the RFC 9110 `Allow` header on 405.
    let mut allowed: Vec<Method> = Vec::new();
    for route in &shared.routes {
        if shared.strict_slash && route.trailing_slash != req_trailing {
            continue;
        }
        if let Some(params) = match_path(&route.segments, &path) {
            path_exists = true;
            for m in &route.methods {
                if !allowed.contains(m) {
                    allowed.push(m.clone());
                }
            }
            if chosen.is_none() && route.methods.contains(&ctx.request.method) {
                chosen = Some((route.handler.clone(), params));
            }
        }
    }

    match chosen {
        Some((handler, params)) => {
            let ctx = Ctx {
                request: ctx.request,
                params,
            };
            handler(ctx).await
        }
        None if path_exists => {
            let allow = allowed
                .iter()
                .map(Method::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            match &shared.method_not_allowed {
                Some(h) => h(ctx).await.header("Allow", allow),
                None => {
                    HttpResponse::text(StatusCode::METHOD_NOT_ALLOWED, "405 Method Not Allowed")
                        .header("Allow", allow)
                }
            }
        }
        None => match &shared.not_found {
            Some(h) => h(ctx).await,
            None => HttpResponse::text(StatusCode::NOT_FOUND, "404 Not Found"),
        },
    }
}

// Per-request logging context (a task-local set by `trace_context`). Read by the
// `ferroly::log` provider registered on the first `trace_context` call.
#[cfg(feature = "log")]
tokio::task_local! {
    static LOG_FIELDS: Vec<(String, ferroly::codec::Value)>;
}

#[cfg(feature = "log")]
static TRACE_PROVIDER_INIT: std::sync::Once = std::sync::Once::new();

/// A process-unique-ish request id: `<nanos>-<counter>` in hex.
#[cfg(feature = "log")]
fn gen_trace_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{n:x}")
}

/// Picks a response content type from an `Accept` header (JSON default), or
/// `None` if nothing supported matches.
fn negotiate_content_type(accept: &str) -> Option<&'static str> {
    if accept.trim().is_empty() {
        return Some("application/json");
    }
    for part in accept.split(',') {
        let ct = part.split(';').next().unwrap_or("").trim();
        match ct {
            "*/*" | "application/*" | "application/json" | "text/json" => {
                return Some("application/json")
            }
            "application/xml" | "text/xml" => return Some("application/xml"),
            "application/yaml" | "text/yaml" | "application/x-yaml" | "text/x-yaml"
            | "application/yml" | "text/yml" => return Some("application/yaml"),
            "application/toml" | "text/toml" | "application/x-toml" => {
                return Some("application/toml")
            }
            _ => {}
        }
    }
    None
}

fn parse_pattern(path: &str) -> Vec<Seg> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| match s.strip_prefix(':') {
            Some(name) => Seg::Param(name.to_string()),
            None => Seg::Static(s.to_string()),
        })
        .collect()
}

fn match_path(segments: &[Seg], path: &str) -> Option<HashMap<String, String>> {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() != segments.len() {
        return None;
    }
    let mut params = HashMap::new();
    for (seg, part) in segments.iter().zip(parts) {
        match seg {
            Seg::Static(s) => {
                if s != part {
                    return None;
                }
            }
            Seg::Param(name) => {
                params.insert(name.clone(), part.to_string());
            }
        }
    }
    Some(params)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_and_captures() {
        let segs = parse_pattern("/items/:id/detail");
        let p = match_path(&segs, "/items/42/detail").unwrap();
        assert_eq!(p.get("id").map(String::as_str), Some("42"));
        assert!(match_path(&segs, "/items/42").is_none());
        assert!(match_path(&segs, "/items/42/other").is_none());
    }

    #[test]
    fn decodes_query() {
        assert_eq!(percent_decode("a%20b+c"), "a b c");
    }
}

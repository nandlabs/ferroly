//! The router: registration, resolution and dispatch.

use std::any::TypeId;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, ToSocketAddrs};

use ferroly::http::{
    percent_decode, BoxFuture, HttpError, HttpHandler, HttpResponse, Method, ReloadableTlsConfig,
    Request, ServerConfig, StatusCode, TlsConfig,
};

use super::ctx::{apply_pending, Ctx, Reject, StateMap};
use super::error::RouteError;
use super::pattern::{match_prefix, split_path, Pattern, Seg};
use super::respond::IntoResponse;

pub(crate) type Handler = Arc<dyn Fn(Ctx) -> BoxFuture<'static, HttpResponse> + Send + Sync>;
pub(crate) type Middleware =
    Arc<dyn Fn(Ctx, Next) -> BoxFuture<'static, HttpResponse> + Send + Sync>;

/// What a filter produced: an answer right away, or a future for one.
pub(crate) enum FilterOutcome {
    Ready(Option<HttpResponse>),
    Pending(BoxFuture<'static, Option<HttpResponse>>),
}

pub(crate) type FilterFn = Arc<dyn Fn(&Ctx) -> FilterOutcome + Send + Sync>;

pub(crate) fn sync_filter<F>(f: F) -> FilterFn
where
    F: Fn(&Ctx) -> Option<HttpResponse> + Send + Sync + 'static,
{
    Arc::new(move |ctx| FilterOutcome::Ready(f(ctx)))
}

pub(crate) fn async_filter<F, Fut>(f: F) -> FilterFn
where
    F: Fn(&Ctx) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Option<HttpResponse>> + Send + 'static,
{
    Arc::new(move |ctx| FilterOutcome::Pending(Box::pin(f(ctx))))
}

async fn run_filter(filter: &FilterFn, ctx: &Ctx) -> Option<HttpResponse> {
    match filter(ctx) {
        FilterOutcome::Ready(answer) => answer,
        FilterOutcome::Pending(future) => future.await,
    }
}

fn wrap_handler<F, Fut, R>(handler: F) -> Handler
where
    F: Fn(Ctx) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = R> + Send + 'static,
    R: IntoResponse,
{
    Arc::new(move |ctx| {
        let future = handler(ctx);
        Box::pin(async move { future.await.into_response() })
    })
}

fn filter_layer(filter: FilterFn) -> Middleware {
    Arc::new(move |ctx, next| {
        let filter = filter.clone();
        Box::pin(async move {
            if let Some(resp) = run_filter(&filter, &ctx).await {
                return resp;
            }
            next.run(ctx).await
        })
    })
}

static FILTER_IDS: AtomicU64 = AtomicU64::new(1);

/// A registered filter and the path prefix it is scoped to.
pub(crate) struct FilterEntry {
    id: u64,
    /// `None` runs for every request; `Some(prefix)` only for paths whose
    /// leading segments match the prefix.
    scope: Option<Vec<Seg>>,
    run: FilterFn,
}

impl FilterEntry {
    fn new(scope: Option<Vec<Seg>>, run: FilterFn) -> Arc<FilterEntry> {
        Arc::new(FilterEntry {
            id: FILTER_IDS.fetch_add(1, Ordering::Relaxed),
            scope,
            run,
        })
    }

    fn applies(&self, parts: &[&str]) -> bool {
        match &self.scope {
            None => true,
            Some(prefix) => match_prefix(prefix, parts),
        }
    }
}

/// One route under construction: a handler for one or more methods plus
/// per-route options — middleware, filters, a body-size limit, a timeout.
/// Register it with [`Router::route`] or [`Group::route`].
///
/// ```
/// use std::time::Duration;
/// use ferroly::http::{HttpResponse, StatusCode};
/// use ferroly::turbo::{Route, Router};
///
/// let _router = Router::new().route(
///     "/upload",
///     Route::post(|ctx| async move {
///         HttpResponse::text(StatusCode::CREATED, format!("{} bytes", ctx.body().len()))
///     })
///     .body_limit(64 * 1024)                 // larger bodies -> 413
///     .timeout(Duration::from_secs(5))       // slower handlers -> 503
///     .layer(|ctx, next| async move {        // runs for this route only
///         next.run(ctx).await.header("X-Route", "upload")
///     }),
/// );
/// ```
#[must_use]
pub struct Route {
    methods: Vec<Method>,
    handler: Handler,
    layers: Vec<Middleware>,
    body_limit: Option<usize>,
    timeout: Option<Duration>,
}

macro_rules! route_constructors {
    ($($(#[$doc:meta])* $name:ident => $method:expr;)*) => {
        $(
            $(#[$doc])*
            pub fn $name<F, Fut, R>(handler: F) -> Self
            where
                F: Fn(Ctx) -> Fut + Send + Sync + 'static,
                Fut: Future<Output = R> + Send + 'static,
                R: IntoResponse,
            {
                Route::new(vec![$method], handler)
            }
        )*
    };
}

impl Route {
    /// A route answering the given methods with `handler`.
    pub fn new<F, Fut, R>(methods: Vec<Method>, handler: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: IntoResponse,
    {
        Route {
            methods,
            handler: wrap_handler(handler),
            layers: Vec::new(),
            body_limit: None,
            timeout: None,
        }
    }

    route_constructors! {
        /// A `GET` route.
        get => Method::Get;
        /// A `POST` route.
        post => Method::Post;
        /// A `PUT` route.
        put => Method::Put;
        /// A `DELETE` route.
        delete => Method::Delete;
        /// A `PATCH` route.
        patch => Method::Patch;
        /// A `HEAD` route.
        head => Method::Head;
        /// An `OPTIONS` route.
        options => Method::Options;
    }

    /// Adds an onion middleware that wraps only this route's handler. It runs
    /// inside the router-wide layers, the authenticator and the filters, and
    /// only when this route was matched. Layers run in the order added.
    pub fn layer<F, Fut>(mut self, middleware: F) -> Self
    where
        F: Fn(Ctx, Next) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        self.layers
            .push(Arc::new(move |ctx, next| Box::pin(middleware(ctx, next))));
        self
    }

    /// Adds a filter that runs only for this route, after the router's and
    /// groups' filters — the place for a per-route permission check.
    pub fn filter<F>(mut self, filter: F) -> Self
    where
        F: Fn(&Ctx) -> Option<HttpResponse> + Send + Sync + 'static,
    {
        self.layers.push(filter_layer(sync_filter(filter)));
        self
    }

    /// The asynchronous form of [`filter`](Self::filter); see
    /// [`Router::filter_async`].
    pub fn filter_async<F, Fut>(mut self, filter: F) -> Self
    where
        F: Fn(&Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Option<HttpResponse>> + Send + 'static,
    {
        self.layers.push(filter_layer(async_filter(filter)));
        self
    }

    /// Limits the request body for this route: a larger body is answered with
    /// `413 Payload Too Large` and the handler is not called. Overrides
    /// [`Router::body_limit`].
    ///
    /// The server has already read the body by the time a route is known, so
    /// this can only be **lower** than the server-wide
    /// [`ServerConfig::max_body_bytes`](ferroly::http::ServerConfig) (8 MiB
    /// by default), which is what bounds memory.
    pub fn body_limit(mut self, bytes: usize) -> Self {
        self.body_limit = Some(bytes);
        self
    }

    /// Limits how long this route's handler (and its route-level layers) may
    /// take: when the time is up the handler future is dropped and the client
    /// receives `503 Service Unavailable`. Overrides
    /// [`Router::request_timeout`].
    pub fn timeout(mut self, limit: Duration) -> Self {
        self.timeout = Some(limit);
        self
    }
}

struct RouteEntry {
    methods: Vec<Method>,
    pattern: Pattern,
    template: Arc<str>,
    handler: Handler,
    layers: Vec<Middleware>,
    chain: Arc<[Middleware]>,
    /// The filters of every group this route was registered in. They are
    /// scoped by path prefix like any other filter; this list is the proof
    /// obligation checked again right before the handler runs.
    guards: Vec<Arc<FilterEntry>>,
    body_limit: Option<usize>,
    timeout: Option<Duration>,
}

impl RouteEntry {
    fn new(pattern: Pattern, route: Route) -> RouteEntry {
        RouteEntry {
            methods: route.methods,
            template: Arc::from(pattern.template()),
            pattern,
            handler: route.handler,
            layers: route.layers,
            chain: Arc::from(Vec::new()),
            guards: Vec::new(),
            body_limit: route.body_limit,
            timeout: route.timeout,
        }
    }
}

macro_rules! method_routes {
    () => {
        /// Registers a `GET` handler. `HEAD` requests are answered by it too
        /// (without the body) unless a `HEAD` route is registered or
        /// [`Router::auto_head`] is turned off.
        pub fn get<F, Fut, R>(mut self, path: &str, handler: F) -> Self
        where
            F: Fn(Ctx) -> Fut + Send + Sync + 'static,
            Fut: Future<Output = R> + Send + 'static,
            R: IntoResponse,
        {
            self.register(path, Route::get(handler));
            self
        }

        /// Registers a `POST` handler.
        pub fn post<F, Fut, R>(mut self, path: &str, handler: F) -> Self
        where
            F: Fn(Ctx) -> Fut + Send + Sync + 'static,
            Fut: Future<Output = R> + Send + 'static,
            R: IntoResponse,
        {
            self.register(path, Route::post(handler));
            self
        }

        /// Registers a `PUT` handler.
        pub fn put<F, Fut, R>(mut self, path: &str, handler: F) -> Self
        where
            F: Fn(Ctx) -> Fut + Send + Sync + 'static,
            Fut: Future<Output = R> + Send + 'static,
            R: IntoResponse,
        {
            self.register(path, Route::put(handler));
            self
        }

        /// Registers a `DELETE` handler.
        pub fn delete<F, Fut, R>(mut self, path: &str, handler: F) -> Self
        where
            F: Fn(Ctx) -> Fut + Send + Sync + 'static,
            Fut: Future<Output = R> + Send + 'static,
            R: IntoResponse,
        {
            self.register(path, Route::delete(handler));
            self
        }

        /// Registers a `PATCH` handler.
        pub fn patch<F, Fut, R>(mut self, path: &str, handler: F) -> Self
        where
            F: Fn(Ctx) -> Fut + Send + Sync + 'static,
            Fut: Future<Output = R> + Send + 'static,
            R: IntoResponse,
        {
            self.register(path, Route::patch(handler));
            self
        }

        /// Registers a `HEAD` handler.
        pub fn head<F, Fut, R>(mut self, path: &str, handler: F) -> Self
        where
            F: Fn(Ctx) -> Fut + Send + Sync + 'static,
            Fut: Future<Output = R> + Send + 'static,
            R: IntoResponse,
        {
            self.register(path, Route::head(handler));
            self
        }

        /// Registers an `OPTIONS` handler.
        pub fn options<F, Fut, R>(mut self, path: &str, handler: F) -> Self
        where
            F: Fn(Ctx) -> Fut + Send + Sync + 'static,
            Fut: Future<Output = R> + Send + 'static,
            R: IntoResponse,
        {
            self.register(path, Route::options(handler));
            self
        }

        /// Registers one handler for several methods at once.
        pub fn add<F, Fut, R>(mut self, path: &str, handler: F, methods: Vec<Method>) -> Self
        where
            F: Fn(Ctx) -> Fut + Send + Sync + 'static,
            Fut: Future<Output = R> + Send + 'static,
            R: IntoResponse,
        {
            self.register(path, Route::new(methods, handler));
            self
        }

        /// Registers a [`Route`] — a handler with per-route middleware,
        /// filters, body limit or timeout.
        pub fn route(mut self, path: &str, route: Route) -> Self {
            self.register(path, route);
            self
        }
    };
}

/// A first-class HTTP router. Finalize it with [`Router::into_handler`] (or
/// one of the `serve*` methods) into an [`HttpHandler`] to serve directly or
/// embed in the `rest` module's server.
///
/// # Path patterns
///
/// | Segment | Matches | Example |
/// |---|---|---|
/// | literal | exactly that segment (case-sensitive) | `/users/new` |
/// | `:name` | any one segment | `/users/:id` |
/// | `*name` | the rest of the path, zero or more segments (last only) | `/files/*path` |
///
/// When several routes match a path the **most specific** one wins — a
/// literal beats `:name`, which beats `*name`, compared segment by segment
/// from the left — whatever order they were registered in.
///
/// # Request path handling
///
/// Before anything else looks at a request, its path is normalised with
/// [`normalize_path`](ferroly::http::normalize_path) and its query string is
/// decoded. A path that is not absolute, has an empty segment (`//`), a
/// malformed percent-escape or an encoded control character — or a query with
/// a malformed escape or invalid UTF-8 — is answered with `400 Bad Request`.
/// Middleware, the authenticator, filters, routing and handlers then all see
/// the same canonical path.
#[must_use]
pub struct Router {
    routes: Vec<RouteEntry>,
    filters: Vec<Arc<FilterEntry>>,
    auth: Option<FilterFn>,
    middlewares: Vec<Middleware>,
    not_found: Option<Handler>,
    method_not_allowed: Option<Handler>,
    strict_slash: bool,
    auto_head: bool,
    auto_options: bool,
    body_limit: Option<usize>,
    timeout: Option<Duration>,
    state: StateMap,
    errors: Vec<RouteError>,
}

impl Default for Router {
    fn default() -> Self {
        Router {
            routes: Vec::new(),
            filters: Vec::new(),
            auth: None,
            middlewares: Vec::new(),
            not_found: None,
            method_not_allowed: None,
            strict_slash: false,
            auto_head: true,
            auto_options: true,
            body_limit: None,
            timeout: None,
            state: StateMap::new(),
            errors: Vec::new(),
        }
    }
}

impl Router {
    /// Creates an empty router.
    pub fn new() -> Self {
        Self::default()
    }

    fn register(&mut self, path: &str, route: Route) {
        match Pattern::parse(path) {
            Ok(pattern) => self.routes.push(RouteEntry::new(pattern, route)),
            Err(error) => self.errors.push(error),
        }
    }

    pub(crate) fn push_error(&mut self, error: RouteError) {
        self.errors.push(error);
    }

    method_routes!();

    /// Sets the trailing-slash policy. Default `false` (lenient — `/x` and `/x/`
    /// are the same route). When `true`, they are distinct. Routes ending in a
    /// `*name` catch-all are not affected.
    pub fn strict_slash(mut self, strict: bool) -> Self {
        self.strict_slash = strict;
        self
    }

    /// Whether `HEAD` is answered by a path's `GET` route when no `HEAD`
    /// route is registered for it (default `true`). The `GET` handler runs
    /// and the server sends its headers — including the `Content-Length` of
    /// the body it produced — without the body.
    pub fn auto_head(mut self, enabled: bool) -> Self {
        self.auto_head = enabled;
        self
    }

    /// Whether `OPTIONS` is answered automatically for a path that has routes
    /// but no `OPTIONS` route (default `true`): `204 No Content` with an
    /// `Allow` header listing the path's methods. The authenticator and the
    /// filters still run first. When `false`, such a request is a `405`.
    pub fn auto_options(mut self, enabled: bool) -> Self {
        self.auto_options = enabled;
        self
    }

    /// Sets the default request-body limit for every route that does not set
    /// its own ([`Route::body_limit`]); a larger body is answered with `413`.
    pub fn body_limit(mut self, bytes: usize) -> Self {
        self.body_limit = Some(bytes);
        self
    }

    /// Sets the default handler timeout for every route that does not set its
    /// own ([`Route::timeout`]); a handler that takes longer is dropped and
    /// the client receives `503 Service Unavailable`.
    pub fn request_timeout(mut self, limit: Duration) -> Self {
        self.timeout = Some(limit);
        self
    }

    /// Registers shared application state, available to every handler,
    /// filter and middleware through [`Ctx::state`]. One value per type;
    /// registering the same type again replaces it.
    ///
    /// ```
    /// use ferroly::http::{HttpResponse, StatusCode};
    /// use ferroly::turbo::Router;
    ///
    /// struct Config { greeting: String }
    ///
    /// let _router = Router::new()
    ///     .with_state(Config { greeting: "hello".into() })
    ///     .get("/", |ctx| async move {
    ///         let greeting = ctx.state::<Config>().map(|c| c.greeting.clone());
    ///         HttpResponse::text(StatusCode::OK, greeting.unwrap_or_default())
    ///     });
    /// ```
    pub fn with_state<T: Send + Sync + 'static>(mut self, state: T) -> Self {
        self.state.insert(TypeId::of::<T>(), Arc::new(state));
        self
    }

    /// Adds a filter that runs before the handler and may short-circuit with
    /// a response. Filters run in registration order, after the
    /// authenticator, for every request — including ones no route matches.
    pub fn filter<F>(mut self, f: F) -> Self
    where
        F: Fn(&Ctx) -> Option<HttpResponse> + Send + Sync + 'static,
    {
        self.filters.push(FilterEntry::new(None, sync_filter(f)));
        self
    }

    /// Adds a filter that can await — a database or cache lookup, a call to
    /// an introspection endpoint.
    ///
    /// The closure is called with `&Ctx`, copies out what it needs, and
    /// returns a `'static` future that resolves to `None` (continue) or a
    /// response (stop). To hand a result on to the handler, move
    /// [`ctx.extensions()`](Ctx::extensions) into the future and insert it:
    ///
    /// ```
    /// use ferroly::http::{HttpResponse, StatusCode};
    /// use ferroly::turbo::Router;
    ///
    /// struct Account(String);
    ///
    /// async fn lookup(key: String) -> Option<String> {
    ///     (key == "k1").then(|| "acme".to_string())
    /// }
    ///
    /// let _router = Router::new()
    ///     .filter_async(|ctx| {
    ///         let key = ctx.header("x-api-key").unwrap_or_default().to_string();
    ///         let extensions = ctx.extensions();
    ///         async move {
    ///             match lookup(key).await {
    ///                 Some(account) => {
    ///                     extensions.insert(Account(account));
    ///                     None
    ///                 }
    ///                 None => Some(HttpResponse::text(StatusCode::UNAUTHORIZED, "bad key")),
    ///             }
    ///         }
    ///     })
    ///     .get("/whoami", |ctx| async move {
    ///         let name = ctx.get::<Account>().map(|a| a.0.clone()).unwrap_or_default();
    ///         HttpResponse::text(StatusCode::OK, name)
    ///     });
    /// ```
    pub fn filter_async<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(&Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Option<HttpResponse>> + Send + 'static,
    {
        self.filters.push(FilterEntry::new(None, async_filter(f)));
        self
    }

    /// Sets the authenticator, which runs *before* all filters — authentication
    /// is always checked first. Setting it again replaces the previous one.
    pub fn auth<F>(mut self, f: F) -> Self
    where
        F: Fn(&Ctx) -> Option<HttpResponse> + Send + Sync + 'static,
    {
        self.auth = Some(sync_filter(f));
        self
    }

    /// Sets an authenticator that can await; see
    /// [`filter_async`](Self::filter_async) for the shape.
    pub fn auth_async<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(&Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Option<HttpResponse>> + Send + 'static,
    {
        self.auth = Some(async_filter(f));
        self
    }

    /// Registers a group of routes under a shared path `prefix`, with
    /// group-scoped filters and middleware. The closure receives a [`Group`]
    /// builder.
    ///
    /// The prefix may contain `:name` parameters (`/tenants/:tenant`) but not
    /// a `*` catch-all. An invalid prefix registers **nothing** from the
    /// group and is reported by [`validate`](Self::validate).
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
        let group = match Pattern::parse_prefix(prefix) {
            Ok(segments) => Group::new(segments),
            Err(error) => Group::invalid(error),
        };
        let (routes, filters, errors) = build(group).finish();
        self.routes.extend(routes);
        self.filters.extend(filters);
        self.errors.extend(errors);
        self
    }

    /// Mounts another router under `prefix`: every route of `router` is
    /// served at `prefix` + its own path.
    ///
    /// What the mounted router carried comes along, scoped to the prefix:
    ///
    /// - its **authenticator** and **filters** run for every request under
    ///   the prefix (its authenticator first), after the filters this router
    ///   registered before the `mount` call;
    /// - its **middleware** ([`layer`](Self::layer)) wraps each of its routes
    ///   (so it runs only when one of them matches);
    /// - its default **body limit** and **timeout** apply to its routes;
    /// - its **state** is merged in — on a type both routers registered, this
    ///   router's value wins.
    ///
    /// Its custom 404/405 handlers and its `strict_slash` / `auto_head` /
    /// `auto_options` settings are **not** carried over; this router's apply.
    ///
    /// ```
    /// use ferroly::turbo::Router;
    /// use ferroly::http::{HttpResponse, StatusCode};
    ///
    /// let v1 = Router::new()
    ///     .get("/users/:id", |_| async { HttpResponse::text(StatusCode::OK, "user") });
    /// let _app = Router::new().mount("/v1", v1); // GET /v1/users/:id
    /// ```
    pub fn mount(mut self, prefix: &str, router: Router) -> Self {
        let prefix = match Pattern::parse_prefix(prefix) {
            Ok(segments) => segments,
            Err(error) => {
                self.errors.push(error);
                return self;
            }
        };
        let scope_under = |inner: &Option<Vec<Seg>>| {
            let mut scope = prefix.clone();
            scope.extend(inner.iter().flatten().cloned());
            Some(scope)
        };

        // The mounted router's authenticator and filters become filters
        // scoped to the mount point. Each gets a new identity, so remember
        // which new entry replaces which old one.
        let mut own: Vec<Arc<FilterEntry>> = Vec::new();
        let mut replaced: HashMap<u64, Arc<FilterEntry>> = HashMap::new();
        if let Some(auth) = router.auth {
            let entry = FilterEntry::new(Some(prefix.clone()), auth);
            own.push(entry.clone());
            self.filters.push(entry);
        }
        for old in router.filters {
            let entry = FilterEntry::new(scope_under(&old.scope), old.run.clone());
            if old.scope.is_none() {
                own.push(entry.clone());
            }
            replaced.insert(old.id, entry.clone());
            self.filters.push(entry);
        }

        for mut route in router.routes {
            route.pattern = route.pattern.prefixed(&prefix);
            route.template = Arc::from(route.pattern.template());
            let mut guards = own.clone();
            guards.extend(
                route
                    .guards
                    .iter()
                    .filter_map(|g| replaced.get(&g.id).cloned()),
            );
            route.guards = guards;
            let mut layers = router.middlewares.clone();
            layers.append(&mut route.layers);
            route.layers = layers;
            route.body_limit = route.body_limit.or(router.body_limit);
            route.timeout = route.timeout.or(router.timeout);
            self.routes.push(route);
        }
        for (key, value) in router.state {
            self.state.entry(key).or_insert(value);
        }
        self.errors.extend(router.errors);
        self
    }

    /// Sets a custom 404 handler.
    pub fn on_not_found<F, Fut, R>(mut self, handler: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: IntoResponse,
    {
        self.not_found = Some(wrap_handler(handler));
        self
    }

    /// Sets a custom 405 handler. The `Allow` header is added to whatever it
    /// returns.
    pub fn on_method_not_allowed<F, Fut, R>(mut self, handler: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: IntoResponse,
    {
        self.method_not_allowed = Some(wrap_handler(handler));
        self
    }

    /// Adds an "onion" middleware that wraps the rest of the chain — it runs
    /// code **before and after** the inner handler and may inspect or modify the
    /// response. Middlewares run in registration order (first registered is
    /// outermost / runs first) and wrap the authenticator, filters, and
    /// handlers. The route has already been resolved, so
    /// [`Ctx::route`] and [`Ctx::param`] work inside a middleware.
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

    /// Every problem found in what was registered: malformed patterns and
    /// prefixes, conflicting routes, unusable middleware configuration. Empty
    /// when the router is sound.
    pub fn problems(&self) -> Vec<RouteError> {
        let mut out = self.errors.clone();
        for (i, first) in self.routes.iter().enumerate() {
            for second in &self.routes[i + 1..] {
                if !first.pattern.same_shape(&second.pattern) {
                    continue;
                }
                if self.strict_slash
                    && !first.pattern.has_wildcard()
                    && first.pattern.trailing_slash != second.pattern.trailing_slash
                {
                    continue;
                }
                if let Some(method) = first.methods.iter().find(|m| second.methods.contains(m)) {
                    out.push(RouteError::Conflict {
                        method: method.as_str().to_string(),
                        first: first.template.to_string(),
                        second: second.template.to_string(),
                    });
                }
            }
        }
        out
    }

    /// Checks what was registered, returning the first problem — see
    /// [`problems`](Self::problems) for all of them.
    ///
    /// Registration never panics and never fails, so that the builder stays
    /// chainable; call this (or [`try_into_handler`](Self::try_into_handler))
    /// at start-up to turn a routing mistake into an error instead of a
    /// surprise at request time.
    ///
    /// ```
    /// use ferroly::http::{HttpResponse, StatusCode};
    /// use ferroly::turbo::{RouteError, Router};
    ///
    /// let router = Router::new()
    ///     .get("/users/:id", |_| async { HttpResponse::text(StatusCode::OK, "a") })
    ///     .get("/users/:name", |_| async { HttpResponse::text(StatusCode::OK, "b") });
    /// assert!(matches!(router.validate(), Err(RouteError::Conflict { .. })));
    /// ```
    pub fn validate(&self) -> Result<(), RouteError> {
        match self.problems().into_iter().next() {
            Some(problem) => Err(problem),
            None => Ok(()),
        }
    }

    fn into_shared(mut self) -> Shared {
        for route in &mut self.routes {
            route.chain = Arc::from(std::mem::take(&mut route.layers));
        }
        // Most specific first; the sort is stable, so among routes of the
        // same shape the one registered first is tried first.
        self.routes.sort_by_key(|r| r.pattern.specificity());
        Shared {
            routes: self.routes,
            filters: self.filters,
            auth: self.auth,
            middlewares: self.middlewares,
            not_found: self.not_found,
            method_not_allowed: self.method_not_allowed,
            strict_slash: self.strict_slash,
            auto_head: self.auto_head,
            auto_options: self.auto_options,
            body_limit: self.body_limit,
            timeout: self.timeout,
            state: Arc::new(self.state),
        }
    }

    /// Wraps the router as a shared [`HttpHandler`].
    ///
    /// This never fails: a route with a malformed pattern is simply absent
    /// (requests for it get `404`), and of two conflicting routes the one
    /// registered first wins. Use [`try_into_handler`](Self::try_into_handler)
    /// to have such mistakes reported instead.
    pub fn into_handler(self) -> Arc<dyn HttpHandler> {
        Arc::new(RouterService {
            shared: Arc::new(self.into_shared()),
        })
    }

    /// [`validate`](Self::validate)s the router, then wraps it as a shared
    /// [`HttpHandler`].
    pub fn try_into_handler(self) -> Result<Arc<dyn HttpHandler>, RouteError> {
        self.validate()?;
        Ok(self.into_handler())
    }

    /// Binds `addr` and serves until the process ends, with the default
    /// [`ServerConfig`].
    pub async fn serve(self, addr: impl ToSocketAddrs) -> Result<(), HttpError> {
        self.serve_with_config(addr, ServerConfig::default(), std::future::pending())
            .await
    }

    /// Binds `addr` and serves until `shutdown` resolves, then drains
    /// in-flight requests (up to the default drain timeout) and returns.
    pub async fn serve_with_shutdown<S>(
        self,
        addr: impl ToSocketAddrs,
        shutdown: S,
    ) -> Result<(), HttpError>
    where
        S: Future<Output = ()> + Send,
    {
        self.serve_with_config(addr, ServerConfig::default(), shutdown)
            .await
    }

    /// Binds `addr` and serves with explicit server limits and timeouts
    /// until `shutdown` resolves.
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use ferroly::http::ServerConfig;
    /// use ferroly::turbo::Router;
    ///
    /// # async fn ex(router: Router) -> Result<(), ferroly::http::HttpError> {
    /// let config = ServerConfig {
    ///     max_body_bytes: 1024 * 1024,
    ///     handler_timeout: Some(Duration::from_secs(30)),
    ///     ..ServerConfig::default()
    /// };
    /// router
    ///     .serve_with_config("0.0.0.0:8080", config, async {
    ///         let _ = tokio::signal::ctrl_c().await;
    ///     })
    ///     .await
    /// # }
    /// ```
    pub async fn serve_with_config<S>(
        self,
        addr: impl ToSocketAddrs,
        config: ServerConfig,
        shutdown: S,
    ) -> Result<(), HttpError>
    where
        S: Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind(addr).await?;
        self.serve_listener(listener, config, shutdown).await
    }

    /// Serves on an already bound listener (for an ephemeral port, socket
    /// activation, or a listener with custom socket options).
    pub async fn serve_listener<S>(
        self,
        listener: TcpListener,
        config: ServerConfig,
        shutdown: S,
    ) -> Result<(), HttpError>
    where
        S: Future<Output = ()> + Send,
    {
        ferroly::http::serve_with_config(listener, self.into_handler(), shutdown, config).await
    }

    /// Binds `addr` and serves HTTPS with the certificate in `tls` until
    /// `shutdown` resolves. Build `tls` with
    /// [`TlsConfig::from_pem`](ferroly::http::TlsConfig::from_pem) or
    /// [`TlsConfig::builder`](ferroly::http::TlsConfig::builder) (mutual TLS,
    /// minimum version).
    pub async fn serve_tls<S>(
        self,
        addr: impl ToSocketAddrs,
        tls: TlsConfig,
        config: ServerConfig,
        shutdown: S,
    ) -> Result<(), HttpError>
    where
        S: Future<Output = ()> + Send,
    {
        self.serve_tls_reloadable(addr, tls.reloadable(), config, shutdown)
            .await
    }

    /// Like [`serve_tls`](Self::serve_tls), with a TLS configuration that can
    /// be swapped while the server runs: keep a clone of `tls` and call
    /// [`ReloadableTlsConfig::reload`] to rotate the certificate.
    pub async fn serve_tls_reloadable<S>(
        self,
        addr: impl ToSocketAddrs,
        tls: ReloadableTlsConfig,
        config: ServerConfig,
        shutdown: S,
    ) -> Result<(), HttpError>
    where
        S: Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind(addr).await?;
        self.serve_tls_listener(listener, tls, config, shutdown)
            .await
    }

    /// Serves HTTPS on an already bound listener. Pass
    /// [`TlsConfig::reloadable`](ferroly::http::TlsConfig::reloadable) for a
    /// fixed certificate.
    pub async fn serve_tls_listener<S>(
        self,
        listener: TcpListener,
        tls: ReloadableTlsConfig,
        config: ServerConfig,
        shutdown: S,
    ) -> Result<(), HttpError>
    where
        S: Future<Output = ()> + Send,
    {
        ferroly::http::serve_tls_reloadable(listener, self.into_handler(), shutdown, config, tls)
            .await
    }
}

/// A builder for a [`Router::group`] — routes under a shared prefix, plus
/// filters and middleware scoped to that prefix.
///
/// # Scoping guarantee
///
/// A group filter runs for every request whose path lies under the group's
/// prefix. "Under" is decided by the same segment matcher that matches
/// routes, on the same normalised path — literal segments compare whole,
/// `:name` matches any one segment — so there is no spelling of a path that
/// reaches a route of the group without matching the group's prefix. As a
/// second line of defence the router re-checks, immediately before calling a
/// group's handler, that every filter of the enclosing groups has run for
/// this request, and runs any that has not.
///
/// A group filter also runs for paths under the prefix that match no route
/// (so an authentication filter answers `401` rather than revealing which
/// paths exist with a `404`). Group **middleware** ([`layer`](Self::layer))
/// wraps the group's handlers, so it runs only when one of its routes
/// matched.
#[must_use]
pub struct Group {
    prefix: Vec<Seg>,
    routes: Vec<RouteEntry>,
    /// `(filter, own)`: `own` filters belong to this group and guard all of
    /// its routes; the others came from nested groups and already guard
    /// theirs.
    filters: Vec<(Arc<FilterEntry>, bool)>,
    layers: Vec<Middleware>,
    body_limit: Option<usize>,
    timeout: Option<Duration>,
    errors: Vec<RouteError>,
    invalid: bool,
}

type GroupParts = (Vec<RouteEntry>, Vec<Arc<FilterEntry>>, Vec<RouteError>);

impl Group {
    fn new(prefix: Vec<Seg>) -> Self {
        Group {
            prefix,
            routes: Vec::new(),
            filters: Vec::new(),
            layers: Vec::new(),
            body_limit: None,
            timeout: None,
            errors: Vec::new(),
            invalid: false,
        }
    }

    /// A group whose prefix could not be parsed: it accepts registrations and
    /// discards them all, so nothing from it is ever served unprotected at
    /// the wrong place.
    fn invalid(error: RouteError) -> Self {
        let mut group = Group::new(Vec::new());
        group.errors.push(error);
        group.invalid = true;
        group
    }

    fn register(&mut self, path: &str, route: Route) {
        match Pattern::parse(path) {
            Ok(mut pattern) => {
                if self.prefix.is_empty() {
                    pattern.trailing_slash = path.len() > 1 && path.ends_with('/');
                } else {
                    pattern.trailing_slash = path.ends_with('/');
                }
                let pattern = pattern.prefixed(&self.prefix);
                self.routes.push(RouteEntry::new(pattern, route));
            }
            Err(error) => self.errors.push(error),
        }
    }

    #[cfg(feature = "auth")]
    pub(crate) fn push_error(&mut self, error: RouteError) {
        self.errors.push(error);
    }

    method_routes!();

    /// Adds a filter that runs for requests under this group's prefix; see
    /// the [scoping guarantee](Group#scoping-guarantee).
    pub fn filter<F>(mut self, f: F) -> Self
    where
        F: Fn(&Ctx) -> Option<HttpResponse> + Send + Sync + 'static,
    {
        self.filters.push((
            FilterEntry::new(Some(self.prefix.clone()), sync_filter(f)),
            true,
        ));
        self
    }

    /// The asynchronous form of [`filter`](Self::filter); see
    /// [`Router::filter_async`] for the shape.
    pub fn filter_async<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(&Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Option<HttpResponse>> + Send + 'static,
    {
        self.filters.push((
            FilterEntry::new(Some(self.prefix.clone()), async_filter(f)),
            true,
        ));
        self
    }

    /// Adds an onion middleware around every handler of this group
    /// (including nested groups). It runs inside the router-wide layers, the
    /// authenticator and the filters, and only when a route of the group
    /// matched. Layers run in the order added, outer groups first.
    pub fn layer<F, Fut>(mut self, middleware: F) -> Self
    where
        F: Fn(Ctx, Next) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        self.layers
            .push(Arc::new(move |ctx, next| Box::pin(middleware(ctx, next))));
        self
    }

    /// Sets the default body limit for this group's routes; see
    /// [`Route::body_limit`].
    pub fn body_limit(mut self, bytes: usize) -> Self {
        self.body_limit = Some(bytes);
        self
    }

    /// Sets the default handler timeout for this group's routes; see
    /// [`Route::timeout`].
    pub fn request_timeout(mut self, limit: Duration) -> Self {
        self.timeout = Some(limit);
        self
    }

    /// Nests a group: its prefix is this group's prefix followed by
    /// `prefix`, and this group's filters and middleware apply to it as well
    /// as its own.
    ///
    /// ```
    /// use ferroly::turbo::Router;
    /// use ferroly::http::{HttpResponse, StatusCode};
    ///
    /// let _router = Router::new().group("/api", |api| {
    ///     api.group("/admin", |admin| {
    ///         admin.get("/stats", |_| async { HttpResponse::text(StatusCode::OK, "stats") })
    ///     })
    /// }); // GET /api/admin/stats
    /// ```
    pub fn group(mut self, prefix: &str, build: impl FnOnce(Group) -> Group) -> Self {
        let child = match Pattern::parse_prefix(prefix) {
            Ok(segments) if !self.invalid => {
                let mut full = self.prefix.clone();
                full.extend(segments);
                Group::new(full)
            }
            Ok(_) => {
                let mut child = Group::new(Vec::new());
                child.invalid = true;
                child
            }
            Err(error) => Group::invalid(error),
        };
        let (routes, filters, errors) = build(child).finish();
        self.routes.extend(routes);
        self.filters.extend(filters.into_iter().map(|f| (f, false)));
        self.errors.extend(errors);
        self
    }

    /// Closes the group: its own filters become guards of every route in it,
    /// its layers wrap every route, and its defaults fill in.
    fn finish(self) -> GroupParts {
        if self.invalid {
            return (Vec::new(), Vec::new(), self.errors);
        }
        let own: Vec<Arc<FilterEntry>> = self
            .filters
            .iter()
            .filter(|(_, own)| *own)
            .map(|(f, _)| f.clone())
            .collect();
        let mut routes = self.routes;
        for route in &mut routes {
            let mut guards = own.clone();
            guards.append(&mut route.guards);
            route.guards = guards;
            let mut layers = self.layers.clone();
            layers.append(&mut route.layers);
            route.layers = layers;
            route.body_limit = route.body_limit.or(self.body_limit);
            route.timeout = route.timeout.or(self.timeout);
        }
        let filters = self.filters.into_iter().map(|(f, _)| f).collect();
        (routes, filters, self.errors)
    }
}

/// How a request was resolved against the route table.
pub(crate) enum Resolution {
    /// The route at this index handles the request.
    Route(usize),
    /// The path has routes, none for this method, and the request is an
    /// `OPTIONS` the router answers itself.
    AutoOptions(String),
    /// The path has routes, but none for this method.
    MethodNotAllowed(String),
    /// No route matches the path.
    NotFound,
}

/// The finalized, shareable router state.
pub(crate) struct Shared {
    routes: Vec<RouteEntry>,
    filters: Vec<Arc<FilterEntry>>,
    auth: Option<FilterFn>,
    middlewares: Vec<Middleware>,
    not_found: Option<Handler>,
    method_not_allowed: Option<Handler>,
    strict_slash: bool,
    auto_head: bool,
    auto_options: bool,
    body_limit: Option<usize>,
    timeout: Option<Duration>,
    state: Arc<StateMap>,
}

fn bad_request(what: &str) -> HttpResponse {
    HttpResponse::text(StatusCode::BAD_REQUEST, format!("400 Bad Request: {what}"))
}

impl Shared {
    /// The `Allow` value for a set of registered methods, with the methods
    /// the router answers on its own.
    fn allow_header(&self, methods: &[Method]) -> String {
        let mut names: Vec<&str> = methods.iter().map(Method::as_str).collect();
        if self.auto_head && names.contains(&"GET") && !names.contains(&"HEAD") {
            names.push("HEAD");
        }
        if self.auto_options && !names.contains(&"OPTIONS") {
            names.push("OPTIONS");
        }
        names.join(", ")
    }

    /// Resolves the request against the route table and records the match —
    /// route template, decoded parameters — on the context.
    fn resolve(&self, ctx: &mut Ctx) -> Result<(), HttpResponse> {
        let method = ctx.method().clone();
        let path = ctx.path().to_string();
        let parts = split_path(&path);
        let req_trailing = path.len() > 1 && path.ends_with('/');

        let mut chosen: Option<usize> = None;
        let mut head_fallback: Option<usize> = None;
        let mut first_match: Option<usize> = None;
        let mut allowed: Vec<Method> = Vec::new();
        for (index, route) in self.routes.iter().enumerate() {
            if self.strict_slash
                && !route.pattern.has_wildcard()
                && route.pattern.trailing_slash != req_trailing
            {
                continue;
            }
            if route.pattern.match_full(&parts).is_none() {
                continue;
            }
            first_match.get_or_insert(index);
            for m in &route.methods {
                if !allowed.contains(m) {
                    allowed.push(m.clone());
                }
            }
            if chosen.is_none() && route.methods.contains(&method) {
                chosen = Some(index);
            }
            if head_fallback.is_none()
                && self.auto_head
                && method == Method::Head
                && route.methods.contains(&Method::Get)
            {
                head_fallback = Some(index);
            }
        }

        let resolution = match (chosen.or(head_fallback), first_match) {
            (Some(index), _) => {
                let route = &self.routes[index];
                let caps = route
                    .pattern
                    .match_full(&parts)
                    .ok_or_else(|| bad_request("invalid request path"))?;
                let decode = |raw: &str| -> Result<String, HttpResponse> {
                    percent_decode(raw)
                        .ok()
                        .and_then(|bytes| String::from_utf8(bytes).ok())
                        .ok_or_else(|| bad_request("path parameter is not valid UTF-8"))
                };
                let mut params = HashMap::with_capacity(caps.params.len() + 1);
                for (name, raw) in &caps.params {
                    params.insert((*name).to_string(), decode(raw)?);
                }
                let mut tail = Vec::new();
                if let Some((name, segments)) = &caps.tail {
                    for raw in segments {
                        tail.push(decode(raw)?);
                    }
                    params.insert((*name).to_string(), tail.join("/"));
                }
                ctx.set_match(Some(route.template.clone()), params, tail);
                Resolution::Route(index)
            }
            (None, Some(index)) => {
                // The path exists: label the request with its template even
                // though no handler will run.
                ctx.set_match(
                    Some(self.routes[index].template.clone()),
                    HashMap::new(),
                    Vec::new(),
                );
                let allow = self.allow_header(&allowed);
                if self.auto_options && method == Method::Options {
                    Resolution::AutoOptions(allow)
                } else {
                    Resolution::MethodNotAllowed(allow)
                }
            }
            (None, None) => {
                ctx.set_match(None, HashMap::new(), Vec::new());
                Resolution::NotFound
            }
        };
        ctx.resolution = Some(resolution);
        Ok(())
    }

    /// The answer to `OPTIONS *`: what the server as a whole supports.
    fn options_star(&self) -> HttpResponse {
        if !self.auto_options {
            return HttpResponse::text(StatusCode::NOT_FOUND, "404 Not Found");
        }
        let mut methods: Vec<Method> = Vec::new();
        for route in &self.routes {
            for m in &route.methods {
                if !methods.contains(m) {
                    methods.push(m.clone());
                }
            }
        }
        HttpResponse::new(StatusCode::NO_CONTENT).header("Allow", self.allow_header(&methods))
    }
}

struct RouterService {
    shared: Arc<Shared>,
}

impl HttpHandler for RouterService {
    fn handle(&self, req: Request) -> BoxFuture<'_, HttpResponse> {
        let shared = self.shared.clone();
        Box::pin(async move {
            if req.uri.path == "*" && req.method == Method::Options {
                return shared.options_star();
            }
            // Normalise the target once, before middleware, authentication,
            // filters and routing — all of which then see the same path.
            let mut ctx = match Ctx::prepare(req, shared.state.clone()) {
                Ok(ctx) => ctx,
                Err(Reject::Path) => return bad_request("invalid request path"),
                Err(Reject::Query) => return bad_request("invalid query string"),
            };
            if let Err(resp) = shared.resolve(&mut ctx) {
                return resp;
            }
            let pending = ctx.pending();
            let resp = Next::global(shared, 0).run(ctx).await;
            // Headers queued by a layer that then answered on its own.
            apply_pending(&pending, resp)
        })
    }
}

enum NextInner {
    /// The router-wide chain; past its end comes authentication, filters and
    /// the route.
    Global { shared: Arc<Shared>, index: usize },
    /// A route's own chain; past its end comes the handler.
    Route {
        chain: Arc<[Middleware]>,
        index: usize,
        handler: Handler,
    },
}

/// The continuation passed to a middleware. Running it invokes the remaining
/// middlewares and, finally, the authenticator, the filters and the matched
/// handler.
pub struct Next {
    inner: NextInner,
}

impl Next {
    fn global(shared: Arc<Shared>, index: usize) -> Next {
        Next {
            inner: NextInner::Global { shared, index },
        }
    }

    /// Runs the remainder of the chain with `ctx`, returning its response.
    pub fn run(self, ctx: Ctx) -> BoxFuture<'static, HttpResponse> {
        Box::pin(async move {
            match self.inner {
                NextInner::Global { shared, index } => match shared.middlewares.get(index) {
                    Some(middleware) => {
                        let middleware = middleware.clone();
                        middleware(ctx, Next::global(shared, index + 1)).await
                    }
                    None => core_dispatch(shared, ctx).await,
                },
                NextInner::Route {
                    chain,
                    index,
                    handler,
                } => match chain.get(index) {
                    Some(middleware) => {
                        let middleware = middleware.clone();
                        let next = Next {
                            inner: NextInner::Route {
                                chain,
                                index: index + 1,
                                handler,
                            },
                        };
                        middleware(ctx, next).await
                    }
                    None => handler(ctx).await,
                },
            }
        })
    }
}

/// The innermost layer: authenticator, filters, and the resolved route.
async fn core_dispatch(shared: Arc<Shared>, mut ctx: Ctx) -> HttpResponse {
    if ctx.resolution.is_none() {
        // A context a middleware built itself (a rewritten request): route it
        // now, so filters and routing still agree on the path.
        ctx.set_state(shared.state.clone());
        if let Err(resp) = shared.resolve(&mut ctx) {
            return resp;
        }
    }
    let pending = ctx.pending();
    let resp = dispatch(&shared, ctx).await;
    apply_pending(&pending, resp)
}

async fn dispatch(shared: &Shared, mut ctx: Ctx) -> HttpResponse {
    if let Some(auth) = &shared.auth {
        if let Some(resp) = run_filter(auth, &ctx).await {
            return resp;
        }
    }

    // Filter scopes are tested with the route matcher, on the same segments.
    let applicable: Vec<Arc<FilterEntry>> = {
        let parts = split_path(ctx.path());
        shared
            .filters
            .iter()
            .filter(|f| f.applies(&parts))
            .cloned()
            .collect()
    };
    let mut ran: Vec<u64> = Vec::with_capacity(applicable.len());
    for filter in &applicable {
        ran.push(filter.id);
        if let Some(resp) = run_filter(&filter.run, &ctx).await {
            return resp;
        }
    }

    match ctx.resolution.take() {
        Some(Resolution::Route(index)) => {
            let route = &shared.routes[index];
            // Defence in depth: a handler registered in a group never runs
            // unless every filter of its enclosing groups has run.
            for guard in &route.guards {
                if !ran.contains(&guard.id) {
                    ran.push(guard.id);
                    if let Some(resp) = run_filter(&guard.run, &ctx).await {
                        return resp;
                    }
                }
            }
            if let Some(limit) = route.body_limit.or(shared.body_limit) {
                if ctx.body().len() > limit {
                    return HttpResponse::text(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "413 Payload Too Large",
                    );
                }
            }
            let next = Next {
                inner: NextInner::Route {
                    chain: route.chain.clone(),
                    index: 0,
                    handler: route.handler.clone(),
                },
            };
            match route.timeout.or(shared.timeout) {
                Some(limit) => match tokio::time::timeout(limit, next.run(ctx)).await {
                    Ok(resp) => resp,
                    Err(_) => HttpResponse::text(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "503 Service Unavailable: the request timed out",
                    ),
                },
                None => next.run(ctx).await,
            }
        }
        Some(Resolution::AutoOptions(allow)) => {
            HttpResponse::new(StatusCode::NO_CONTENT).header("Allow", allow)
        }
        Some(Resolution::MethodNotAllowed(allow)) => match &shared.method_not_allowed {
            Some(h) => h(ctx).await.header("Allow", allow),
            None => HttpResponse::text(StatusCode::METHOD_NOT_ALLOWED, "405 Method Not Allowed")
                .header("Allow", allow),
        },
        Some(Resolution::NotFound) | None => match &shared.not_found {
            Some(h) => h(ctx).await,
            None => HttpResponse::text(StatusCode::NOT_FOUND, "404 Not Found"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroly::http::Uri;

    fn request(method: Method, path: &str) -> Request {
        let mut uri = Uri::parse("http://localhost/").unwrap();
        uri.path = path.to_string();
        Request::new(method, uri)
    }

    async fn call(handler: &Arc<dyn HttpHandler>, method: Method, path: &str) -> (u16, String) {
        let resp = handler.handle(request(method, path)).await;
        (
            resp.status.as_u16(),
            String::from_utf8_lossy(&resp.body).into_owned(),
        )
    }

    fn deny(_: &Ctx) -> Option<HttpResponse> {
        Some(HttpResponse::text(StatusCode::FORBIDDEN, "guard"))
    }

    /// Even if the prefix-scoped filter list were to miss a group filter, the
    /// route still carries it as a guard and the handler does not run.
    #[tokio::test]
    async fn group_filters_guard_their_routes_independently_of_scoping() {
        let mut router = Router::new().group("/api", |g| {
            g.filter(deny).get("/secret", |_| async {
                HttpResponse::text(StatusCode::OK, "secret")
            })
        });
        // Simulate a scoping failure: drop every scoped filter.
        router.filters.clear();
        let handler = router.into_handler();
        assert_eq!(
            call(&handler, Method::Get, "/api/secret").await,
            (403, "guard".to_string())
        );
    }

    #[tokio::test]
    async fn nested_and_mounted_routes_carry_every_enclosing_guard() {
        let inner = Router::new()
            .auth(deny)
            .get("/x", |_| async { HttpResponse::text(StatusCode::OK, "x") });
        let mut router = Router::new()
            .group("/a", |a| {
                a.filter(|_| None).group("/b", |b| {
                    b.filter(deny)
                        .get("/c", |_| async { HttpResponse::text(StatusCode::OK, "c") })
                })
            })
            .mount("/m", inner);
        let guards: Vec<usize> = router.routes.iter().map(|r| r.guards.len()).collect();
        // `/a/b/c` is guarded by both groups, `/m/x` by the mounted auth.
        assert_eq!(guards, vec![2, 1]);

        router.filters.clear();
        let handler = router.into_handler();
        assert_eq!(call(&handler, Method::Get, "/a/b/c").await.0, 403);
        assert_eq!(call(&handler, Method::Get, "/m/x").await.0, 403);
    }

    #[tokio::test]
    async fn the_handler_normalises_before_anything_else() {
        let handler = Router::new()
            .layer(|ctx, next| async move {
                let seen = ctx.path().to_string();
                next.run(ctx).await.header("x-seen", seen)
            })
            .get("/a/b", |_| async {
                HttpResponse::text(StatusCode::OK, "ab")
            })
            .into_handler();
        let resp = handler.handle(request(Method::Get, "/x/../a/./b")).await;
        assert_eq!(resp.status, StatusCode::OK);
        assert_eq!(resp.headers.get("x-seen"), Some("/a/b"));
        for bad in ["//a/b", "a/b", "/a//b", "/a/%zz", "/a/b%00"] {
            let resp = handler.handle(request(Method::Get, bad)).await;
            assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{bad}");
            // Rejected before any middleware ran.
            assert_eq!(resp.headers.get("x-seen"), None, "{bad}");
        }
    }
}

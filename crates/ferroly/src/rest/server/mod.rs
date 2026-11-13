//! HTTP server built on the in-house `ferroly::turbo` router and `ferroly::http`
//! server, integrated with the lifecycle component system for graceful start/stop.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ferroly::http::{
    HttpHandler, Method, ReloadableTlsConfig, ServerConfig, StatusCode, TlsConfig,
};
use ferroly::lifecycle::{
    BoxFuture, Component, ComponentState, HealthRegistry, HealthStatus, LifecycleError, ProbeKind,
};
use ferroly::turbo::{
    Cors, Ctx, Group, IntoResponse, Next, Route, RouteError, Router, SecurityHeaders,
};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use ferroly::http::HttpResponse;

/// The TLS configuration of a [`Server`].
#[derive(Debug, Clone)]
pub enum ServerTls {
    /// A fixed certificate.
    Static(TlsConfig),
    /// A configuration that can be swapped while the server runs: keep a
    /// clone of the [`ReloadableTlsConfig`] and call `reload` to rotate the
    /// certificate.
    Reloadable(ReloadableTlsConfig),
}

impl From<TlsConfig> for ServerTls {
    fn from(tls: TlsConfig) -> Self {
        ServerTls::Static(tls)
    }
}

impl From<ReloadableTlsConfig> for ServerTls {
    fn from(tls: ReloadableTlsConfig) -> Self {
        ServerTls::Reloadable(tls)
    }
}

/// Configuration for a [`Server`].
///
/// The struct gains fields over time; build it with struct-update syntax
/// (`..Default::default()`) or the chaining methods:
///
/// ```
/// use std::time::Duration;
/// use ferroly::http::ServerConfig;
/// use ferroly::rest::ServerOptions;
///
/// let options = ServerOptions {
///     listen_host: "127.0.0.1".into(),
///     listen_port: 8443,
///     ..Default::default()
/// }
/// .server_config(ServerConfig {
///     max_body_bytes: 1024 * 1024,
///     handler_timeout: Some(Duration::from_secs(30)),
///     ..ServerConfig::default()
/// });
/// # let _ = options;
/// ```
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// A unique component id.
    pub id: String,
    /// A path prefix all routes are mounted under (empty for none).
    pub path_prefix: String,
    /// The host/interface to bind.
    pub listen_host: String,
    /// The port to bind (0 selects an ephemeral port).
    pub listen_port: u16,
    /// Limits and timeouts of the underlying HTTP server: body and header
    /// sizes, connection cap, head/body/idle/write/handler timeouts, drain
    /// time, the error hook. See [`ServerConfig`].
    pub config: ServerConfig,
    /// TLS termination. `None` serves plain HTTP.
    pub tls: Option<ServerTls>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            id: "ferroly-rest".to_string(),
            path_prefix: String::new(),
            listen_host: "0.0.0.0".to_string(),
            listen_port: 8080,
            config: ServerConfig::default(),
            tls: None,
        }
    }
}

impl ServerOptions {
    /// Sets the HTTP server limits and timeouts.
    pub fn server_config(mut self, config: ServerConfig) -> Self {
        self.config = config;
        self
    }

    /// Serves HTTPS with `tls` — a [`TlsConfig`] (fixed certificate) or a
    /// [`ReloadableTlsConfig`] (rotatable while running).
    pub fn tls(mut self, tls: impl Into<ServerTls>) -> Self {
        self.tls = Some(tls.into());
        self
    }
}

struct Running {
    shutdown: watch::Sender<bool>,
    join: JoinHandle<()>,
}

/// What the serve task reports back.
#[derive(Default)]
struct Status {
    state: Mutex<Option<ComponentState>>,
    /// The serve loop is no longer running, for whatever reason.
    exited: AtomicBool,
    last_error: Mutex<Option<String>>,
}

impl Status {
    fn set_state(&self, state: ComponentState) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = Some(state);
    }

    fn state(&self) -> ComponentState {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unwrap_or(ComponentState::Unknown)
    }

    fn fail(&self, message: String) {
        *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(message);
    }

    fn error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Marks the serve loop as exited when the task ends — by returning, by an
/// error, or by a panic.
struct ExitFlag(Arc<Status>);

impl Drop for ExitFlag {
    fn drop(&mut self) {
        self.0.exited.store(true, Ordering::SeqCst);
    }
}

/// An HTTP server implementing [`ferroly::lifecycle::Component`].
///
/// # Lifecycle
///
/// - [`start`](Component::start) binds the listener and spawns the serve
///   loop. It is **cancel-safe**: if the returned future is dropped (a
///   start timeout fires), nothing is left half-started and `start` can be
///   called again.
/// - [`stop`](Component::stop) asks the serve loop to shut down, waits for
///   in-flight requests to drain (up to
///   [`ServerConfig::drain_timeout`]), and reports an error if the serve loop
///   had failed. It is cancel-safe as well: a dropped `stop` leaves the
///   shutdown in progress, and a later `stop` finishes it.
/// - The server is **restartable**: `start` after `stop` serves again, with
///   the same routes. With `listen_port: 0` each start picks a new port.
/// - A serve loop that ends on its own — the listener failed — is not
///   silent: [`state`](Self::state) becomes
///   [`Error`](ComponentState::Error), [`health`](Component::health) reports
///   [`Down`](HealthStatus::Down), [`last_error`](Self::last_error) holds the
///   reason, and the next `stop` returns it.
pub struct Server {
    id: String,
    host: String,
    port: u16,
    config: ServerConfig,
    tls: Option<ReloadableTlsConfig>,
    handler: Arc<dyn HttpHandler>,
    local_addr: Mutex<Option<SocketAddr>>,
    running: tokio::sync::Mutex<Option<Running>>,
    status: Arc<Status>,
}

impl Server {
    /// Starts building a server with the given options.
    pub fn builder(options: ServerOptions) -> ServerBuilder {
        ServerBuilder {
            options,
            router: Router::new(),
        }
    }

    /// Builds a server with default options and an empty router.
    pub fn default_server() -> ServerBuilder {
        Self::builder(ServerOptions::default())
    }

    /// The bound local address, available after [`Component::start`].
    pub fn local_addr(&self) -> Option<SocketAddr> {
        *self.local_addr.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The current component state: [`Running`](ComponentState::Running)
    /// between a successful `start` and `stop`,
    /// [`Error`](ComponentState::Error) if the serve loop ended on its own.
    pub fn state(&self) -> ComponentState {
        let state = self.status.state();
        if state == ComponentState::Running && self.status.exited.load(Ordering::SeqCst) {
            ComponentState::Error
        } else {
            state
        }
    }

    /// Why the serve loop ended, if it ended with an error or without being
    /// asked to. Cleared by the next successful `start`.
    pub fn last_error(&self) -> Option<String> {
        self.status.error()
    }

    /// Whether the server terminates TLS.
    pub fn is_tls(&self) -> bool {
        self.tls.is_some()
    }
}

impl Component for Server {
    fn id(&self) -> &str {
        &self.id
    }

    fn start(&self) -> BoxFuture<'_, Result<(), LifecycleError>> {
        Box::pin(async move {
            let mut running = self.running.lock().await;
            if let Some(current) = running.as_ref() {
                if !current.join.is_finished() {
                    return Err(LifecycleError::ComponentAlreadyStarted(self.id.clone()));
                }
                // The previous serve loop ended on its own; start afresh.
                *running = None;
            }

            // The only await between taking the lock and committing: if the
            // caller gives up here, nothing has changed.
            let listener = tokio::net::TcpListener::bind((self.host.as_str(), self.port))
                .await
                .map_err(|e| LifecycleError::failure(self.id.clone(), e))?;
            let addr = listener
                .local_addr()
                .map_err(|e| LifecycleError::failure(self.id.clone(), e))?;

            let (shutdown, mut stop) = watch::channel(false);
            let status = self.status.clone();
            status.exited.store(false, Ordering::SeqCst);
            *status.last_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
            let handler = self.handler.clone();
            let config = self.config.clone();
            let tls = self.tls.clone();
            let join = tokio::spawn(async move {
                let _exit = ExitFlag(status.clone());
                let mut signal = stop.clone();
                let until_stopped = async move {
                    // Resolves on `stop`, and when the `Server` is dropped.
                    let _ = signal.wait_for(|stop| *stop).await;
                };
                let served = match tls {
                    Some(tls) => {
                        ferroly::http::serve_tls_reloadable(
                            listener,
                            handler,
                            until_stopped,
                            config,
                            tls,
                        )
                        .await
                    }
                    None => {
                        ferroly::http::serve_with_config(listener, handler, until_stopped, config)
                            .await
                    }
                };
                let asked_to_stop = *stop.borrow_and_update() || stop.has_changed().is_err();
                match served {
                    Err(error) => status.fail(error.to_string()),
                    Ok(()) if !asked_to_stop => {
                        status.fail("the serve loop ended without being stopped".to_string())
                    }
                    Ok(()) => {}
                }
            });

            *self.local_addr.lock().unwrap_or_else(|e| e.into_inner()) = Some(addr);
            *running = Some(Running { shutdown, join });
            self.status.set_state(ComponentState::Running);
            Ok(())
        })
    }

    fn stop(&self) -> BoxFuture<'_, Result<(), LifecycleError>> {
        Box::pin(async move {
            let mut running = self.running.lock().await;
            let joined = match running.as_mut() {
                Some(current) => {
                    let _ = current.shutdown.send(true);
                    // Awaiting by reference keeps the handle in place, so a
                    // cancelled `stop` can be repeated.
                    let joined = (&mut current.join).await;
                    *running = None;
                    Some(joined)
                }
                None => None,
            };
            self.status.set_state(ComponentState::Stopped);
            // Report a failed serve loop once, to the `stop` that reaped it;
            // `last_error` keeps it until the next start.
            match (joined, self.status.error()) {
                (Some(Err(join_error)), _) => {
                    let message = format!("the serve task failed: {join_error}");
                    self.status.fail(message.clone());
                    Err(LifecycleError::failure(
                        self.id.clone(),
                        std::io::Error::other(message),
                    ))
                }
                (Some(Ok(())), Some(message)) => Err(LifecycleError::failure(
                    self.id.clone(),
                    std::io::Error::other(message),
                )),
                _ => Ok(()),
            }
        })
    }

    /// [`Down`](HealthStatus::Down) when the serve loop has ended although
    /// the server was not stopped; otherwise `None` (healthy — the manager
    /// derives the status from the lifecycle state).
    fn health(&self) -> Option<HealthStatus> {
        if self.status.exited.load(Ordering::SeqCst)
            && self.status.state() == ComponentState::Running
        {
            Some(HealthStatus::Down)
        } else {
            None
        }
    }
}

/// Builder that registers routes before constructing a [`Server`].
///
/// Route paths given to the registration methods and to
/// [`group`](Self::group) / [`route`](Self::route) are placed under
/// [`ServerOptions::path_prefix`]. Paths added through
/// [`map_router`](Self::map_router), and the health endpoints, are not.
#[must_use]
pub struct ServerBuilder {
    options: ServerOptions,
    router: Router,
}

macro_rules! prefixed_methods {
    ($($(#[$doc:meta])* $name:ident;)*) => {
        $(
            $(#[$doc])*
            pub fn $name<F, Fut, R>(mut self, path: &str, handler: F) -> Self
            where
                F: Fn(Ctx) -> Fut + Send + Sync + 'static,
                Fut: Future<Output = R> + Send + 'static,
                R: IntoResponse,
            {
                let p = self.prefixed(path);
                self.router = self.router.$name(&p, handler);
                self
            }
        )*
    };
}

impl ServerBuilder {
    fn prefixed(&self, path: &str) -> String {
        let prefix = self.options.path_prefix.trim_end_matches('/');
        if prefix.is_empty() {
            path.to_string()
        } else {
            format!("{prefix}{path}")
        }
    }

    prefixed_methods! {
        /// Registers a `GET` handler.
        get;
        /// Registers a `POST` handler.
        post;
        /// Registers a `PUT` handler.
        put;
        /// Registers a `DELETE` handler.
        delete;
        /// Registers a `PATCH` handler.
        patch;
        /// Registers a `HEAD` handler.
        head;
        /// Registers an `OPTIONS` handler.
        options;
    }

    /// Registers one handler for several methods.
    pub fn add_route<F, Fut, R>(mut self, path: &str, handler: F, methods: Vec<Method>) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: IntoResponse,
    {
        let p = self.prefixed(path);
        self.router = self.router.add(&p, handler, methods);
        self
    }

    /// Registers a [`Route`] — a handler with per-route middleware, filters,
    /// body limit or timeout.
    pub fn route(mut self, path: &str, route: Route) -> Self {
        let p = self.prefixed(path);
        self.router = self.router.route(&p, route);
        self
    }

    /// Registers a group of routes under `prefix` (itself under the server's
    /// path prefix), with group-scoped filters and middleware; see
    /// [`Router::group`].
    ///
    /// ```
    /// use ferroly::http::{HttpResponse, StatusCode};
    /// use ferroly::rest::{Server, ServerOptions};
    ///
    /// let server = Server::builder(ServerOptions {
    ///     path_prefix: "/api".into(),
    ///     ..Default::default()
    /// })
    /// .group("/admin", |g| {
    ///     g.filter(|ctx| {
    ///         ctx.header("x-admin").is_none().then(|| {
    ///             HttpResponse::text(StatusCode::FORBIDDEN, "admins only")
    ///         })
    ///     })
    ///     .get("/stats", |_| async { HttpResponse::text(StatusCode::OK, "stats") })
    /// }) // GET /api/admin/stats
    /// .build();
    /// # let _ = server;
    /// ```
    pub fn group(mut self, prefix: &str, build: impl FnOnce(Group) -> Group) -> Self {
        let p = self.prefixed(prefix);
        self.router = self.router.group(&p, build);
        self
    }

    /// Adds an onion middleware around every request; see [`Router::layer`].
    pub fn layer<F, Fut>(mut self, middleware: F) -> Self
    where
        F: Fn(Ctx, Next) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        self.router = self.router.layer(middleware);
        self
    }

    /// Installs a CORS policy; see [`Cors`]. Add it before authentication.
    pub fn cors(mut self, cors: Cors) -> Self {
        self.router = self.router.cors(cors);
        self
    }

    /// Adds security response headers (HSTS, `X-Content-Type-Options`,
    /// framing policy, `Referrer-Policy`, optional CSP); see
    /// [`SecurityHeaders`].
    pub fn security_headers(mut self, headers: SecurityHeaders) -> Self {
        self.router = self.router.security_headers(headers);
        self
    }

    /// Gives every request an id (`X-Request-Id`); see
    /// [`Router::request_id`].
    pub fn request_id(mut self) -> Self {
        self.router = self.router.request_id();
        self
    }

    /// Transforms the underlying [`ferroly::turbo::Router`] directly — the seam
    /// for turbo features not surfaced as wrappers here, such as
    /// [`filter`](Router::filter), [`auth`](Router::auth),
    /// [`rate_limit_with`](Router::rate_limit_with) and
    /// [`mount`](Router::mount). Paths added this way are **not** prefixed.
    ///
    /// ```no_run
    /// # use ferroly::rest::{Server, ServerOptions};
    /// # use ferroly::http::{HttpResponse, StatusCode};
    /// let server = Server::builder(ServerOptions::default())
    ///     .map_router(|r| r.auth(|ctx| {
    ///         if ctx.header("authorization").is_none() {
    ///             Some(HttpResponse::text(StatusCode::UNAUTHORIZED, "no auth"))
    ///         } else {
    ///             None
    ///         }
    ///     }))
    ///     .build();
    /// ```
    pub fn map_router(mut self, f: impl FnOnce(Router) -> Router) -> Self {
        self.router = f(self.router);
        self
    }

    /// Adds Kubernetes-style probes (unprefixed) backed by one registry:
    /// `GET /health` (liveness) runs the registry's
    /// [`Liveness`](ProbeKind::Liveness) checks and `GET /ready` (readiness)
    /// its [`Readiness`](ProbeKind::Readiness) checks.
    ///
    /// Register dependency checks (database, downstream services) for
    /// readiness **only**
    /// (`CheckOptions::new().probes(&[ProbeKind::Readiness])`), so that a
    /// failed dependency takes the instance out of rotation without getting
    /// it restarted. A check registered without probe options belongs to
    /// every probe, liveness included.
    pub fn health_endpoints(self, registry: HealthRegistry) -> Self {
        self.health_endpoints_split(registry.clone(), registry)
    }

    /// Adds `GET /health` (liveness) backed by `liveness` and `GET /ready`
    /// (readiness) backed by `readiness` — two registries, for when liveness
    /// and readiness are assembled separately.
    ///
    /// Both endpoints run their checks through the asynchronous registry API
    /// ([`HealthRegistry::check_probe`]), so asynchronous checks are awaited
    /// (each bounded by its timeout) rather than reported from a stale
    /// result.
    ///
    /// - `/health` answers `200` unless a liveness check is
    ///   [`Down`](HealthStatus::Down) (`Degraded` is still alive), else
    ///   `503`. The body is the JSON report.
    /// - `/ready` answers `200 ready` only when every readiness check is
    ///   [`Up`](HealthStatus::Up), else `503 not ready`.
    ///
    /// The probes go through the router's middleware, authenticator and
    /// filters like any other route; exempt their paths there if the
    /// orchestrator cannot authenticate.
    pub fn health_endpoints_split(
        self,
        liveness: HealthRegistry,
        readiness: HealthRegistry,
    ) -> Self {
        self.map_router(move |router| {
            router
                .get("/health", move |_ctx| {
                    let r = liveness.clone();
                    async move {
                        let report = r.check_probe(ProbeKind::Liveness).await;
                        let code = if report.is_live() {
                            StatusCode::OK
                        } else {
                            StatusCode::SERVICE_UNAVAILABLE
                        };
                        HttpResponse::new(code)
                            .header("content-type", "application/json")
                            .header("cache-control", "no-store")
                            .body(report.to_json().into_bytes())
                    }
                })
                .get("/ready", move |_ctx| {
                    let r = readiness.clone();
                    async move {
                        let report = r.check_probe(ProbeKind::Readiness).await;
                        let resp = if report.is_ready() {
                            HttpResponse::text(StatusCode::OK, "ready")
                        } else {
                            HttpResponse::text(StatusCode::SERVICE_UNAVAILABLE, "not ready")
                        };
                        resp.header("cache-control", "no-store")
                    }
                })
        })
    }

    /// Finalizes into a runnable [`Server`].
    ///
    /// Routing mistakes (a malformed pattern, two routes for the same path
    /// and method) do not stop the build — see
    /// [`Router::into_handler`] for what happens to such routes. Use
    /// [`try_build`](Self::try_build) to have them reported.
    pub fn build(self) -> Server {
        let handler = self.router.into_handler();
        Self::assemble(self.options, handler)
    }

    /// Validates the routes ([`Router::validate`]), then builds the server.
    pub fn try_build(self) -> Result<Server, RouteError> {
        let handler = self.router.try_into_handler()?;
        Ok(Self::assemble(self.options, handler))
    }

    fn assemble(options: ServerOptions, handler: Arc<dyn HttpHandler>) -> Server {
        Server {
            id: options.id,
            host: options.listen_host,
            port: options.listen_port,
            config: options.config,
            tls: options.tls.map(|tls| match tls {
                ServerTls::Static(tls) => tls.reloadable(),
                ServerTls::Reloadable(tls) => tls,
            }),
            handler,
            local_addr: Mutex::new(None),
            running: tokio::sync::Mutex::new(None),
            status: Arc::new(Status::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Server {
        Server::builder(ServerOptions {
            id: "unit".into(),
            listen_host: "127.0.0.1".into(),
            listen_port: 0,
            ..Default::default()
        })
        .build()
    }

    /// The serve loop cannot easily be made to fail from outside, so drive
    /// the bookkeeping it would leave behind and check that nothing about it
    /// is swallowed.
    #[tokio::test]
    async fn a_failed_serve_loop_is_visible_everywhere() {
        let server = server();
        server.start().await.unwrap();
        assert_eq!(server.state(), ComponentState::Running);
        assert_eq!(server.health(), None);
        assert_eq!(server.last_error(), None);

        // What the serve task records when the loop ends with an error.
        server
            .status
            .fail("accept failed: too many open files".to_string());
        server.status.exited.store(true, Ordering::SeqCst);

        assert_eq!(server.state(), ComponentState::Error);
        assert_eq!(server.health(), Some(HealthStatus::Down));
        assert_eq!(
            server.last_error().as_deref(),
            Some("accept failed: too many open files")
        );
        let err = server.stop().await.unwrap_err();
        assert!(
            err.to_string().contains("too many open files"),
            "stop must return the serve error, got: {err}"
        );
        assert_eq!(server.state(), ComponentState::Stopped);
        assert_eq!(server.health(), None, "only consulted while running");
        // The failure is reported once; it stays readable until the next start.
        server.stop().await.unwrap();
        assert!(server.last_error().is_some());

        // A fresh start clears the failure.
        server.start().await.unwrap();
        assert_eq!(server.state(), ComponentState::Running);
        assert_eq!(server.last_error(), None);
        assert_eq!(server.health(), None);
        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn dropping_the_server_stops_the_listener() {
        let server = server();
        server.start().await.unwrap();
        let addr = server.local_addr().unwrap();
        assert!(tokio::net::TcpStream::connect(addr).await.is_ok());
        drop(server);
        // The serve loop notices the closed channel and shuts down.
        let mut closed = false;
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(addr).await.is_err() {
                closed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(closed, "the listener outlived its server");
    }
}

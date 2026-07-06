//! HTTP server built on the in-house `ferroly::turbo` router and `ferroly::http`
//! server, integrated with the lifecycle component system for graceful start/stop.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use ferroly::http::{HttpHandler, Method, StatusCode};
use ferroly::lifecycle::{
    BoxFuture, Component, ComponentState, HealthRegistry, HealthStatus, LifecycleError,
};
use ferroly::turbo::{Ctx, Router};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use ferroly::http::HttpResponse;

/// Configuration for a [`Server`].
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
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            id: "ferroly-rest".to_string(),
            path_prefix: String::new(),
            listen_host: "0.0.0.0".to_string(),
            listen_port: 8080,
        }
    }
}

struct Running {
    shutdown: oneshot::Sender<()>,
    join: JoinHandle<()>,
}

/// An HTTP server implementing [`ferroly::lifecycle::Component`].
pub struct Server {
    id: String,
    host: String,
    port: u16,
    handler: Mutex<Option<Arc<dyn HttpHandler>>>,
    local_addr: Mutex<Option<SocketAddr>>,
    running: tokio::sync::Mutex<Option<Running>>,
    state: Mutex<ComponentState>,
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
        *self.local_addr.lock().unwrap()
    }

    /// The current component state.
    pub fn state(&self) -> ComponentState {
        *self.state.lock().unwrap()
    }
}

impl Component for Server {
    fn id(&self) -> &str {
        &self.id
    }

    fn start(&self) -> BoxFuture<'_, Result<(), LifecycleError>> {
        Box::pin(async move {
            let handler = self
                .handler
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| LifecycleError::ComponentAlreadyStarted(self.id.clone()))?;

            let listener = tokio::net::TcpListener::bind((self.host.as_str(), self.port))
                .await
                .map_err(|e| LifecycleError::failure(self.id.clone(), e))?;
            let addr = listener
                .local_addr()
                .map_err(|e| LifecycleError::failure(self.id.clone(), e))?;
            *self.local_addr.lock().unwrap() = Some(addr);

            let (tx, rx) = oneshot::channel::<()>();
            let join = tokio::spawn(async move {
                let _ = ferroly::http::serve(listener, handler, async move {
                    let _ = rx.await;
                })
                .await;
            });

            *self.running.lock().await = Some(Running { shutdown: tx, join });
            *self.state.lock().unwrap() = ComponentState::Running;
            Ok(())
        })
    }

    fn stop(&self) -> BoxFuture<'_, Result<(), LifecycleError>> {
        Box::pin(async move {
            if let Some(running) = self.running.lock().await.take() {
                let _ = running.shutdown.send(());
                let _ = running.join.await;
            }
            *self.state.lock().unwrap() = ComponentState::Stopped;
            Ok(())
        })
    }
}

/// Builder that registers routes before constructing a [`Server`].
#[must_use]
pub struct ServerBuilder {
    options: ServerOptions,
    router: Router,
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

    /// Registers a `GET` handler.
    pub fn get<F, Fut>(mut self, path: &str, handler: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        let p = self.prefixed(path);
        self.router = self.router.get(&p, handler);
        self
    }
    /// Registers a `POST` handler.
    pub fn post<F, Fut>(mut self, path: &str, handler: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        let p = self.prefixed(path);
        self.router = self.router.post(&p, handler);
        self
    }
    /// Registers a `PUT` handler.
    pub fn put<F, Fut>(mut self, path: &str, handler: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        let p = self.prefixed(path);
        self.router = self.router.put(&p, handler);
        self
    }
    /// Registers a `DELETE` handler.
    pub fn delete<F, Fut>(mut self, path: &str, handler: F) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        let p = self.prefixed(path);
        self.router = self.router.delete(&p, handler);
        self
    }
    /// Registers one handler for several methods.
    pub fn add_route<F, Fut>(mut self, path: &str, handler: F, methods: Vec<Method>) -> Self
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HttpResponse> + Send + 'static,
    {
        let p = self.prefixed(path);
        self.router = self.router.add(&p, handler, methods);
        self
    }

    /// Transforms the underlying [`ferroly::turbo::Router`] directly — the seam
    /// for turbo features not surfaced as prefixing wrappers here, such as
    /// [`layer`](Router::layer) (onion middleware), [`filter`](Router::filter),
    /// and [`auth`](Router::auth). Paths added this way are **not** prefixed.
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

    /// Adds Kubernetes-friendly probes (unprefixed) backed by a **single**
    /// registry for both liveness (`GET /health`) and readiness (`GET /ready`).
    ///
    /// Note: with one registry, a `Down` dependency fails *liveness* too, which
    /// makes k8s **restart** the pod rather than just pull it from the load
    /// balancer. For that reason prefer [`health_endpoints_split`] in production,
    /// giving liveness a minimal "process is alive" registry and readiness the
    /// dependency checks.
    ///
    /// [`health_endpoints_split`]: Self::health_endpoints_split
    pub fn health_endpoints(self, registry: HealthRegistry) -> Self {
        self.health_endpoints_split(registry.clone(), registry)
    }

    /// Adds `GET /health` (liveness) backed by `liveness` and `GET /ready`
    /// (readiness) backed by `readiness`, so a failed dependency triggers a
    /// readiness failure (LB pull) without triggering a liveness failure (pod
    /// restart). Both return `503` when unhealthy; `/health` includes a JSON
    /// report.
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
                        let code = if r.overall() == HealthStatus::Down {
                            StatusCode::SERVICE_UNAVAILABLE
                        } else {
                            StatusCode::OK
                        };
                        HttpResponse::new(code)
                            .header("content-type", "application/json")
                            .body(r.to_json().into_bytes())
                    }
                })
                .get("/ready", move |_ctx| {
                    let r = readiness.clone();
                    async move {
                        if r.is_ready() {
                            HttpResponse::text(StatusCode::OK, "ready")
                        } else {
                            HttpResponse::text(StatusCode::SERVICE_UNAVAILABLE, "not ready")
                        }
                    }
                })
        })
    }

    /// Finalizes into a runnable [`Server`].
    pub fn build(self) -> Server {
        Server {
            id: self.options.id,
            host: self.options.listen_host,
            port: self.options.listen_port,
            handler: Mutex::new(Some(self.router.into_handler())),
            local_addr: Mutex::new(None),
            running: tokio::sync::Mutex::new(None),
            state: Mutex::new(ComponentState::Unknown),
        }
    }
}

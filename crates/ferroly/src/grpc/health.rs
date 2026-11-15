//! The standard health-checking service, `grpc.health.v1.Health`
//! (`Check` and `Watch`), as used by load balancers, Kubernetes gRPC
//! probes and `grpc_health_probe`.
//!
//! ```
//! use ferroly::grpc::health::{HealthReporter, ServingStatus};
//! use ferroly::grpc::GrpcServer;
//!
//! let health = HealthReporter::new(); // "" (the whole server) is SERVING
//! health.set_status("demo.Echo", ServingStatus::Serving);
//! let server = GrpcServer::new().add_service(health.service());
//! assert!(server.has_method("/grpc.health.v1.Health/Check"));
//! // Later, while draining (`GrpcServer::serve` does this itself when its
//! // shutdown signal fires): every service NOT_SERVING, Watch streams end.
//! health.shutdown();
//! assert_eq!(health.status(""), Some(ServingStatus::NotServing));
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use super::{GrpcServer, GrpcService, Request, Response, Status, Streaming};
use crate::proto::EnumValue;

/// `grpc.health.v1.HealthCheckRequest`.
#[derive(crate::proto::Message, Clone, PartialEq, Debug, Default)]
#[non_exhaustive]
pub struct HealthCheckRequest {
    /// The service to check; empty for the server as a whole.
    #[proto(tag = 1)]
    pub service: String,
}

impl HealthCheckRequest {
    /// A request for `service` (empty for the whole server).
    pub fn new(service: impl Into<String>) -> Self {
        HealthCheckRequest {
            service: service.into(),
        }
    }
}

/// `grpc.health.v1.HealthCheckResponse.ServingStatus`.
#[derive(crate::proto::Enumeration, Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum ServingStatus {
    /// `UNKNOWN`.
    Unknown = 0,
    /// `SERVING`.
    Serving = 1,
    /// `NOT_SERVING`.
    NotServing = 2,
    /// `SERVICE_UNKNOWN` (only sent by `Watch`).
    ServiceUnknown = 3,
}

/// `grpc.health.v1.HealthCheckResponse`.
#[derive(crate::proto::Message, Clone, PartialEq, Debug, Default)]
#[non_exhaustive]
pub struct HealthCheckResponse {
    /// The serving status.
    #[proto(tag = 1)]
    pub status: EnumValue<ServingStatus>,
}

impl HealthCheckResponse {
    /// A response with `status`.
    pub fn new(status: ServingStatus) -> Self {
        HealthCheckResponse {
            status: status.into(),
        }
    }
}

/// The path of `Check`.
pub const CHECK_PATH: &str = "/grpc.health.v1.Health/Check";
/// The path of `Watch`.
pub const WATCH_PATH: &str = "/grpc.health.v1.Health/Watch";

struct Shared {
    statuses: Mutex<HashMap<String, ServingStatus>>,
    changed: watch::Sender<u64>,
    /// Set by [`HealthReporter::shutdown`]: `Watch` streams end.
    shut_down: AtomicBool,
}

/// The health state served by the health service: a serving status per
/// service name (`""` is the server as a whole). Cloning shares the state.
#[derive(Clone)]
pub struct HealthReporter {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for HealthReporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HealthReporter")
            .field("statuses", &*self.lock())
            .finish()
    }
}

impl Default for HealthReporter {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthReporter {
    /// A reporter with the server as a whole (`""`) `SERVING`.
    pub fn new() -> Self {
        let mut statuses = HashMap::new();
        statuses.insert(String::new(), ServingStatus::Serving);
        HealthReporter {
            shared: Arc::new(Shared {
                statuses: Mutex::new(statuses),
                changed: watch::channel(0).0,
                shut_down: AtomicBool::new(false),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, ServingStatus>> {
        self.shared
            .statuses
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn notify(&self) {
        self.shared.changed.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// Sets the status of `service`, telling every `Watch` of it. Ignored
    /// once [`shutdown`](Self::shutdown) was called, as in grpc-go: a
    /// draining server keeps reporting `NOT_SERVING`.
    pub fn set_status(&self, service: &str, status: ServingStatus) {
        {
            let mut statuses = self.lock();
            // Checked under the lock `shutdown` sets the flag under.
            if self.is_shut_down() {
                return;
            }
            statuses.insert(service.to_string(), status);
        }
        self.notify();
    }

    /// Forgets `service`: `Check` answers `NOT_FOUND`, `Watch` reports
    /// `SERVICE_UNKNOWN`. Ignored once [`shutdown`](Self::shutdown) was
    /// called.
    pub fn clear(&self, service: &str) {
        {
            let mut statuses = self.lock();
            if self.is_shut_down() {
                return;
            }
            statuses.remove(service);
        }
        self.notify();
    }

    /// The server is shutting down: marks every known service (and the
    /// server, `""`) `NOT_SERVING`, sends that to every `Watch`, and then
    /// ends the `Watch` streams cleanly (`OK`), so that they do not hold a
    /// graceful drain open until its timeout. Later `Watch` calls get the
    /// current status and end at once; `Check` keeps answering. Later
    /// [`set_status`](Self::set_status) and [`clear`](Self::clear) calls are
    /// ignored.
    ///
    /// [`GrpcServer::serve`] calls it when its shutdown signal fires, for
    /// every health service added to the server.
    pub fn shutdown(&self) {
        {
            let mut statuses = self.lock();
            statuses.insert(String::new(), ServingStatus::NotServing);
            for status in statuses.values_mut() {
                *status = ServingStatus::NotServing;
            }
            self.shared.shut_down.store(true, Ordering::SeqCst);
        }
        self.notify();
    }

    /// Whether [`shutdown`](Self::shutdown) was called.
    pub fn is_shut_down(&self) -> bool {
        self.shared.shut_down.load(Ordering::SeqCst)
    }

    /// The status of `service`, if it is known.
    pub fn status(&self, service: &str) -> Option<ServingStatus> {
        self.lock().get(service).copied()
    }

    /// The `grpc.health.v1.Health` service over this state, for
    /// [`GrpcServer::add_service`].
    pub fn service(&self) -> HealthService {
        HealthService {
            reporter: self.clone(),
        }
    }
}

/// The `grpc.health.v1.Health` service; see [`HealthReporter::service`].
#[derive(Debug, Clone)]
pub struct HealthService {
    reporter: HealthReporter,
}

impl GrpcService for HealthService {
    fn register(self, server: GrpcServer) -> GrpcServer {
        let check = self.reporter.clone();
        let stopper = self.reporter.clone();
        let watcher = self.reporter;
        server
            .on_shutdown(move || stopper.shutdown())
            .unary(CHECK_PATH, move |req: Request<HealthCheckRequest>| {
                let status = check.status(&req.get_ref().service);
                async move {
                    match status {
                        Some(status) => Ok(Response::new(HealthCheckResponse::new(status))),
                        None => Err(Status::not_found("unknown service")),
                    }
                }
            })
            .server_streaming(WATCH_PATH, move |req: Request<HealthCheckRequest>| {
                let reporter = watcher.clone();
                async move {
                    let service = req.into_inner().service;
                    let (tx, stream) = Streaming::channel(4);
                    let mut changed = reporter.shared.changed.subscribe();
                    tokio::spawn(async move {
                        let mut last = None;
                        loop {
                            // Read the flag first: `shutdown` sets the statuses
                            // before it, so a stream that ends has sent them.
                            let stopping = reporter.is_shut_down();
                            let now = reporter
                                .status(&service)
                                .unwrap_or(ServingStatus::ServiceUnknown);
                            if last != Some(now) {
                                if tx.send(HealthCheckResponse::new(now)).await.is_err() {
                                    return;
                                }
                                last = Some(now);
                            }
                            if stopping {
                                let _ = tx.finish().await;
                                return;
                            }
                            tokio::select! {
                                r = changed.changed() => if r.is_err() {
                                    let _ = tx.finish().await;
                                    return;
                                },
                                () = tx.closed() => return,
                            }
                        }
                    });
                    Ok(Response::new(stream))
                }
            })
    }
}

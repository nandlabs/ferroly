//! Observability middleware: access log, RED metrics, and trace / request
//! correlation. Each is gated on the feature of the module it reports to.

#[cfg(any(feature = "metrics", feature = "obs"))]
use ferroly::http::Method;

#[cfg(any(feature = "log", feature = "metrics", feature = "obs"))]
use super::router::Router;

/// The label for a request method: the standard method names, and `OTHER`
/// for anything else — so a client inventing method tokens cannot create
/// label values.
#[cfg(any(feature = "metrics", feature = "obs"))]
fn method_label(method: &Method) -> &'static str {
    match method {
        Method::Get => "GET",
        Method::Post => "POST",
        Method::Put => "PUT",
        Method::Delete => "DELETE",
        Method::Patch => "PATCH",
        Method::Head => "HEAD",
        Method::Options => "OPTIONS",
        Method::Trace => "TRACE",
        Method::Connect => "CONNECT",
        Method::Other(_) => "OTHER",
    }
}

/// The `route` label of a request that matched no route.
#[cfg(feature = "metrics")]
pub const UNMATCHED_ROUTE: &str = "unmatched";

/// The log target of [`Router::access_log`] records when the logger passed
/// in has none of its own.
#[cfg(feature = "log")]
pub const ACCESS_LOG_TARGET: &str = "ferroly::turbo::access";

#[cfg(feature = "log")]
impl Router {
    /// Adds an access-log middleware: one [`Level::Info`](ferroly::log::Level)
    /// record per request via `logger`. Requires the `log` feature.
    ///
    /// Fields: `method`, `path` (the normalised path), `route` (the matched
    /// template, when there is one), `status`, `ms`, `bytes` (response body
    /// size; 0 for a streamed body), `peer` (the connected address) and
    /// `request_id` (when [`request_id`](Self::request_id) or
    /// [`trace_context`](Self::trace_context) is registered before it).
    ///
    /// If `logger` has no target, records are logged under
    /// [`ACCESS_LOG_TARGET`], so
    /// [`set_level_for`](ferroly::log::set_level_for) can turn the access log
    /// up or down independently of everything else.
    pub fn access_log(self, logger: ferroly::log::Logger) -> Self {
        let logger = if logger.target().is_none() {
            logger.with_target(ACCESS_LOG_TARGET)
        } else {
            logger
        };
        self.layer(move |ctx, next| {
            let logger = logger.clone();
            async move {
                let method = ctx.method().as_str().to_string();
                let path = ctx.path().to_string();
                let route = ctx.route().map(str::to_string);
                let peer = ctx.peer_addr().map(|a| a.to_string());
                let request_id = ctx.request_id();
                let start = std::time::Instant::now();
                let resp = next.run(ctx).await;
                let ms = start.elapsed().as_millis() as u64;
                let mut fields: Vec<(&str, ferroly::codec::Value)> = vec![
                    ("method", method.into()),
                    ("path", path.into()),
                    ("status", resp.status.as_u16().into()),
                    ("ms", ms.into()),
                    ("bytes", (resp.body.len() as u64).into()),
                ];
                if let Some(route) = route {
                    fields.push(("route", route.into()));
                }
                if let Some(peer) = peer {
                    fields.push(("peer", peer.into()));
                }
                if let Some(id) = request_id {
                    fields.push(("request_id", id.into()));
                }
                logger.info("request", &fields);
                resp
            }
        })
    }
}

/// Counts the request when dropped — with the response status, or as
/// `cancelled` when the request future was dropped before producing one (the
/// client went away, or a timeout fired).
#[cfg(feature = "metrics")]
struct RequestRecorder {
    requests: ferroly::metrics::CounterVec,
    method: &'static str,
    route: String,
    status: Option<u16>,
}

#[cfg(feature = "metrics")]
impl RequestRecorder {
    fn finish(mut self, status: u16) {
        self.status = Some(status);
        drop(self);
    }
}

#[cfg(feature = "metrics")]
impl Drop for RequestRecorder {
    fn drop(&mut self) {
        let status = match self.status {
            Some(code) => code.to_string(),
            None => "cancelled".to_string(),
        };
        self.requests
            .with_labels(&[self.method, &self.route, &status])
            .inc();
    }
}

#[cfg(feature = "metrics")]
impl Router {
    /// Records RED metrics for every request into the process-global
    /// [`metrics::Registry`](ferroly::metrics::global). Pair with
    /// [`metrics_route`](Self::metrics_route) to expose them. Requires the
    /// `metrics` feature.
    ///
    /// | Metric | Type | Labels |
    /// |---|---|---|
    /// | `http_requests_total` | counter | `method`, `route`, `status` |
    /// | `http_request_duration_seconds` | histogram | `method`, `route` |
    /// | `http_requests_in_flight` | gauge | — |
    ///
    /// Every label value comes from a bounded set, so a client cannot grow
    /// the number of series:
    ///
    /// - `method` is one of the nine standard method names, or `OTHER`;
    /// - `route` is the matched route **template** (`/users/:id`), never the
    ///   request path, or [`UNMATCHED_ROUTE`] when no route matched;
    /// - `status` is the response status code, or `cancelled` when the
    ///   request was dropped before a response was produced.
    ///
    /// The in-flight gauge and the duration are recorded by RAII guards, so
    /// they stay correct when a request is cancelled or its handler panics.
    pub fn metrics(self) -> Self {
        self.metrics_in(ferroly::metrics::global())
    }

    /// Like [`metrics`](Self::metrics), recording into `registry` instead of
    /// the global one.
    pub fn metrics_in(self, registry: &ferroly::metrics::Registry) -> Self {
        let requests = registry.counter_vec(
            "http_requests_total",
            "Total HTTP requests",
            &["method", "route", "status"],
        );
        let duration = registry.histogram_vec(
            "http_request_duration_seconds",
            "HTTP request latency in seconds",
            &["method", "route"],
            ferroly::metrics::DEFAULT_BUCKETS,
        );
        let in_flight = registry.gauge("http_requests_in_flight", "In-flight HTTP requests", &[]);
        self.layer(move |ctx, next| {
            let method = method_label(ctx.method());
            let route = ctx.route().unwrap_or(UNMATCHED_ROUTE).to_string();
            let guard = in_flight.track();
            let timer = duration.with_labels(&[method, &route]).start_timer();
            let recorder = RequestRecorder {
                requests: requests.clone(),
                method,
                route,
                status: None,
            };
            async move {
                let resp = next.run(ctx).await;
                recorder.finish(resp.status.as_u16());
                drop(timer);
                drop(guard);
                resp
            }
        })
    }

    /// Registers a `GET` route at `path` that serves the process-global
    /// [`metrics::Registry`](ferroly::metrics::global) in the Prometheus text
    /// exposition format. Requires the `metrics` feature.
    ///
    /// The route is served to anyone who can reach it; put it behind your
    /// authenticator or a group filter, or bind it on an internal listener,
    /// if the metrics are not public.
    pub fn metrics_route(self, path: &str) -> Self {
        self.get(path, |_ctx| async move {
            ferroly::http::HttpResponse::text(
                ferroly::http::StatusCode::OK,
                ferroly::metrics::global().encode(),
            )
            .header("content-type", ferroly::metrics::PROMETHEUS_CONTENT_TYPE)
        })
    }
}

// Per-request logging context (a task-local set by `trace_context`). Read by
// the `ferroly::log` provider registered on the first `trace_context` call.
#[cfg(feature = "log")]
tokio::task_local! {
    static LOG_FIELDS: Vec<(String, ferroly::codec::Value)>;
}

#[cfg(feature = "log")]
static LOG_PROVIDER_INIT: std::sync::Once = std::sync::Once::new();

#[cfg(any(feature = "log", feature = "obs"))]
impl Router {
    /// Correlates everything that happens while a request is handled.
    /// Available with the `log` feature, the `obs` feature, or both.
    ///
    /// **Request id.** Each request gets an id — a valid inbound
    /// `X-Request-Id` (see
    /// [`is_valid_request_id`](super::is_valid_request_id)) or a generated
    /// one — available as [`Ctx::request_id`](super::Ctx::request_id) and
    /// echoed in the `X-Request-Id` response header.
    ///
    /// **Tracing** (`obs`). The W3C trace context of the request
    /// (`traceparent` / `tracestate`) is extracted and a
    /// server span (`obs::http::server_span`) is started as its child
    /// — or as the root of a new trace when there is none. The span is named
    /// after the method and the route **template**, is the current span for
    /// the whole of the request (so spans the handler starts, and outbound
    /// calls that inject the context, join the same trace), records the
    /// response status, and ends when the response is produced or the
    /// request is dropped.
    ///
    /// **Logging** (`log`). Every `ferroly::log` record emitted while the
    /// request is handled carries `request_id`, plus `trace_id` and
    /// `span_id` of the request's span when `obs` is enabled. Without `obs`
    /// (or when the span is not started because of the `obs` level),
    /// `trace_id` is the request id, as before.
    ///
    /// Register it before `access_log` so access records are correlated too.
    pub fn trace_context(self) -> Self {
        #[cfg(feature = "log")]
        LOG_PROVIDER_INIT.call_once(|| {
            ferroly::log::add_context_provider(|| {
                LOG_FIELDS.try_with(|f| f.clone()).unwrap_or_default()
            });
            // Adds trace_id / span_id of the current `obs` span (a no-op
            // provider without that feature).
            ferroly::log::add_trace_context_provider();
        });
        self.layer(|ctx, next| {
            let request_id = super::security::ensure_request_id(&ctx);

            #[cfg(feature = "obs")]
            let (span, span_ctx) = {
                let parent = ferroly::obs::extract(|name| ctx.header(name));
                let span = ferroly::obs::http::server_span(
                    method_label(ctx.method()),
                    ctx.route().unwrap_or(""),
                )
                .child_of_opt(parent.as_ref())
                .start();
                span.record("url.path", ctx.path());
                span.record("http.request.id", request_id.as_str());
                // An inert span is transparent: keep the caller's context.
                let span_ctx = span.context().or(parent);
                (span, span_ctx)
            };

            #[cfg(feature = "log")]
            let fields = {
                let mut fields = vec![(
                    "request_id".to_string(),
                    ferroly::codec::Value::Str(request_id.clone()),
                )];
                #[cfg(feature = "obs")]
                let traced = span_ctx.is_some();
                #[cfg(not(feature = "obs"))]
                let traced = false;
                if !traced {
                    fields.push((
                        "trace_id".to_string(),
                        ferroly::codec::Value::Str(request_id.clone()),
                    ));
                }
                fields
            };

            async move {
                let inner = next.run(ctx);
                #[cfg(feature = "log")]
                let inner = LOG_FIELDS.scope(fields, inner);

                #[cfg(feature = "obs")]
                let resp = {
                    let resp = match &span_ctx {
                        Some(span_ctx) => span_ctx.wrap(inner).await,
                        None => inner.await,
                    };
                    ferroly::obs::http::record_server_status(&span, resp.status.as_u16());
                    span.end();
                    resp
                };
                #[cfg(not(feature = "obs"))]
                let resp = inner.await;

                resp
            }
        })
    }
}

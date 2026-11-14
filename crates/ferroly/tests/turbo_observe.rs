#![cfg(feature = "turbo")]
//! Observability middleware: bounded metric labels (H7), cancellation-safe
//! gauges, W3C trace context, and the access log.

#![allow(unused_imports, dead_code)]

use std::net::SocketAddr;
use std::time::Duration;

use ferroly::http::{HttpResponse, ServerConfig, StatusCode};
use ferroly::turbo::Router;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn spawn_with(router: Router, config: ServerConfig) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = router
            .serve_listener(listener, config, std::future::pending())
            .await;
    });
    addr
}

async fn spawn(router: Router) -> SocketAddr {
    spawn_with(router, ServerConfig::default()).await
}

/// Sends a request written by hand and returns `(status, head, body)`.
async fn raw(
    addr: SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
) -> (u16, String, String) {
    let mut request = format!("{method} {target} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await.unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, head.to_string(), body.to_string())
}

fn ok(text: &'static str) -> HttpResponse {
    HttpResponse::text(StatusCode::OK, text)
}

/// A `Write` that collects log output.
#[derive(Clone, Default)]
struct Buf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Buf {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

// ---- metrics (H7) -----------------------------------------------------------------------

#[cfg(feature = "metrics")]
mod metrics {
    use super::*;
    use ferroly::metrics::Registry;

    /// The exposition lines of one metric family's samples.
    fn series<'a>(exposition: &'a str, prefix: &str) -> Vec<&'a str> {
        exposition
            .lines()
            .filter(|line| line.starts_with(prefix))
            .collect()
    }

    #[tokio::test]
    async fn metric_labels_come_from_bounded_sets() {
        let registry = Registry::new();
        let router = Router::new()
            .metrics_in(&registry)
            .get("/users/:id", |_| async { ok("user") })
            .get("/files/*path", |_| async { ok("file") });
        let addr = spawn(router).await;

        // Many distinct paths, one route template each.
        for id in 0..25 {
            assert_eq!(raw(addr, "GET", &format!("/users/{id}"), &[]).await.0, 200);
            assert_eq!(
                raw(addr, "GET", &format!("/files/a/{id}/b"), &[]).await.0,
                200
            );
        }
        // Many invented method tokens: a client must not be able to mint
        // label values.
        for i in 0..25 {
            let method = format!("BREW{i}");
            assert_eq!(raw(addr, &method, "/users/1", &[]).await.0, 405);
            assert_eq!(
                raw(addr, &method, &format!("/nowhere/{i}"), &[]).await.0,
                404
            );
        }
        // Many unmatched paths.
        for i in 0..25 {
            assert_eq!(raw(addr, "GET", &format!("/missing/{i}"), &[]).await.0, 404);
        }

        let exposition = registry.encode();
        let requests = series(&exposition, "http_requests_total{");
        let expected = [
            r#"http_requests_total{method="GET",route="/files/*path",status="200"} 25"#,
            r#"http_requests_total{method="GET",route="/users/:id",status="200"} 25"#,
            r#"http_requests_total{method="GET",route="unmatched",status="404"} 25"#,
            r#"http_requests_total{method="OTHER",route="/users/:id",status="405"} 25"#,
            r#"http_requests_total{method="OTHER",route="unmatched",status="404"} 25"#,
        ];
        assert_eq!(requests.len(), expected.len(), "{requests:#?}");
        for line in expected {
            assert!(requests.contains(&line), "missing {line}\n{requests:#?}");
        }
        // Nothing a client sent verbatim appears anywhere in the output.
        for leaked in [
            "BREW",
            "/users/1",
            "/users/24",
            "/missing",
            "/nowhere",
            "/files/a",
        ] {
            assert!(
                !exposition.contains(leaked),
                "{leaked} leaked:\n{exposition}"
            );
        }
        // The histogram has one series per (method, route) too.
        let counts = series(&exposition, "http_request_duration_seconds_count{");
        assert_eq!(counts.len(), 5, "{counts:#?}");
        assert!(
            exposition.contains("http_requests_in_flight 0"),
            "{exposition}"
        );
    }

    #[tokio::test]
    async fn in_flight_gauge_and_timer_survive_cancellation() {
        let registry = Registry::new();
        let router = Router::new().metrics_in(&registry).get("/slow", |_| async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            ok("never")
        });
        // The server drops the handler future — middleware included — when
        // the handler timeout fires.
        let config = ServerConfig {
            handler_timeout: Some(Duration::from_millis(50)),
            ..ServerConfig::default()
        };
        let addr = spawn_with(router, config).await;
        assert_eq!(raw(addr, "GET", "/slow", &[]).await.0, 503);

        let exposition = registry.encode();
        assert!(
            exposition.contains("http_requests_in_flight 0"),
            "the gauge leaked:\n{exposition}"
        );
        assert!(
            exposition.contains(
                r#"http_requests_total{method="GET",route="/slow",status="cancelled"} 1"#
            ),
            "{exposition}"
        );
        assert!(
            exposition
                .contains(r#"http_request_duration_seconds_count{method="GET",route="/slow"} 1"#),
            "{exposition}"
        );
    }

    #[tokio::test]
    async fn metrics_route_serves_the_prometheus_content_type() {
        let router = Router::new().metrics().metrics_route("/metrics");
        let addr = spawn(router).await;
        let (status, head, body) = raw(addr, "GET", "/metrics", &[]).await;
        assert_eq!(status, 200);
        assert!(
            head.to_ascii_lowercase()
                .contains("content-type: text/plain; version=0.0.4; charset=utf-8"),
            "{head}"
        );
        assert!(body.contains("http_requests_in_flight"), "{body}");
    }
}

// ---- access log ---------------------------------------------------------------------------

#[cfg(feature = "log")]
#[tokio::test]
async fn access_log_uses_a_target_and_the_route_template() {
    use ferroly::log::{Level, Logger};

    ferroly::log::set_max_level(Level::Info);
    let buf = Buf::default();
    let logger = Logger::json().to_writer(buf.clone());
    let router = Router::new()
        .request_id()
        .access_log(logger)
        .get("/users/:id", |_| async { ok("user") });
    let addr = spawn(router).await;

    assert_eq!(
        raw(addr, "GET", "/users/42?x=1", &[("x-request-id", "rid-1")])
            .await
            .0,
        200
    );
    assert_eq!(raw(addr, "GET", "/x/../missing", &[]).await.0, 404);

    let out = buf.text();
    let first = out.lines().next().unwrap_or_default();
    for expected in [
        r#""method":"GET""#,
        r#""path":"/users/42""#,
        r#""route":"/users/:id""#,
        r#""status":200"#,
        r#""bytes":4"#,
        r#""peer":"127.0.0.1:"#,
        r#""request_id":"rid-1""#,
        r#""target":"ferroly::turbo::access""#,
    ] {
        assert!(first.contains(expected), "missing {expected} in {first}");
    }
    // The logged path is the normalised one; an unmatched request has no route.
    let second = out.lines().nth(1).unwrap_or_default();
    assert!(second.contains(r#""path":"/missing""#), "{second}");
    assert!(second.contains(r#""status":404"#), "{second}");
    assert!(!second.contains(r#""route""#), "{second}");

    // A logger that brings its own target keeps it.
    let buf = Buf::default();
    let logger = Logger::json().to_writer(buf.clone()).with_target("edge");
    let addr = spawn(
        Router::new()
            .access_log(logger)
            .get("/", |_| async { ok("") }),
    )
    .await;
    let _ = raw(addr, "GET", "/", &[]).await;
    assert!(buf.text().contains(r#""target":"edge""#), "{}", buf.text());
}

// ---- W3C trace context --------------------------------------------------------------------

#[cfg(feature = "obs")]
mod tracing {
    use super::*;
    use ferroly::obs::{self, Exporter, FinishedSpan, SpanKind};
    use std::sync::{Arc, Mutex};

    /// Collects every finished span of the process; tests pick theirs out by
    /// trace id.
    #[derive(Clone, Default)]
    struct Collect(Arc<Mutex<Vec<FinishedSpan>>>);

    impl Exporter for Collect {
        fn export(&self, span: &FinishedSpan) {
            self.0.lock().unwrap().push(span.clone());
        }
    }

    fn collector() -> Collect {
        static SHARED: std::sync::OnceLock<Collect> = std::sync::OnceLock::new();
        SHARED
            .get_or_init(|| {
                let collect = Collect::default();
                obs::add_exporter(Box::new(collect.clone()));
                collect
            })
            .clone()
    }

    fn spans_of(collect: &Collect, trace_id: &str) -> Vec<FinishedSpan> {
        collect
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|span| span.trace_id == trace_id)
            .cloned()
            .collect()
    }

    fn field<'a>(span: &'a FinishedSpan, name: &str) -> Option<&'a ferroly::codec::Value> {
        span.fields.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    #[tokio::test]
    async fn trace_context_continues_the_callers_trace() {
        let collect = collector();
        let router = Router::new()
            .trace_context()
            .get("/orders/:id", |_| async {
                // A span started inside the handler joins the request's trace.
                let child = obs::Span::new("load order").start();
                let traceparent = obs::current().map(|ctx| ctx.to_traceparent());
                child.end();
                HttpResponse::text(StatusCode::OK, traceparent.unwrap_or_default())
            })
            .get("/boom", |_| async {
                HttpResponse::text(StatusCode::INTERNAL_SERVER_ERROR, "boom")
            });
        let addr = spawn(router).await;

        let trace_id = "4bf92f3577b34da6a3ce929d0e0e4736";
        let parent_id = "00f067aa0ba902b7";
        let inbound = format!("00-{trace_id}-{parent_id}-01");
        let (status, _, body) = raw(
            addr,
            "GET",
            "/orders/7",
            &[("traceparent", &inbound), ("tracestate", "vendor=1")],
        )
        .await;
        assert_eq!(status, 200);
        // Inside the handler the current context is the request's span: same
        // trace, a new span id.
        assert!(body.starts_with(&format!("00-{trace_id}-")), "{body}");
        assert!(!body.contains(parent_id), "{body}");

        let spans = spans_of(&collect, trace_id);
        let server = spans
            .iter()
            .find(|s| s.kind == SpanKind::Server)
            .unwrap_or_else(|| panic!("no server span in {spans:#?}"));
        // Named after the route template, never the path.
        assert_eq!(server.name, "GET /orders/:id");
        assert_eq!(server.parent_id.as_deref(), Some(parent_id));
        assert!(server.parent_remote);
        assert_eq!(server.trace_state.as_deref(), Some("vendor=1"));
        assert_eq!(
            field(server, "http.route").and_then(|v| v.as_str()),
            Some("/orders/:id")
        );
        assert_eq!(
            field(server, "http.response.status_code").and_then(|v| v.as_u64()),
            Some(200)
        );
        assert!(!server.status.is_error());
        let child = spans
            .iter()
            .find(|s| s.name == "load order")
            .expect("handler span");
        assert_eq!(child.parent_id.as_deref(), Some(server.span_id.as_str()));

        // A 5xx marks the span as an error.
        let trace_id = "aaaabbbbccccddddeeeeffff00001111";
        let inbound = format!("00-{trace_id}-{parent_id}-01");
        assert_eq!(
            raw(addr, "GET", "/boom", &[("traceparent", &inbound)])
                .await
                .0,
            500
        );
        let spans = spans_of(&collect, trace_id);
        assert_eq!(spans.len(), 1, "{spans:#?}");
        assert!(spans[0].status.is_error());

        // An invalid traceparent starts a new trace instead of being trusted.
        let (status, _, body) =
            raw(addr, "GET", "/orders/7", &[("traceparent", "00-zz-zz-01")]).await;
        assert_eq!(status, 200);
        assert!(body.starts_with("00-"), "{body}");
        assert!(!body.contains("zz"), "{body}");
    }

    #[tokio::test]
    async fn an_unmatched_request_is_named_by_method_only() {
        let collect = collector();
        let addr = spawn(Router::new().trace_context()).await;
        let trace_id = "0123456789abcdef0123456789abcdef";
        let inbound = format!("00-{trace_id}-00f067aa0ba902b7-01");
        // A hostile method token and path leave no mark on the span name.
        assert_eq!(
            raw(
                addr,
                "BREW",
                "/secret/path/123",
                &[("traceparent", &inbound)]
            )
            .await
            .0,
            404
        );
        let spans = spans_of(&collect, trace_id);
        assert_eq!(spans.len(), 1, "{spans:#?}");
        assert_eq!(spans[0].name, "_OTHER");
    }

    #[cfg(feature = "log")]
    #[tokio::test]
    async fn log_records_carry_the_trace_and_request_ids() {
        use ferroly::log::{Level, Logger};

        let buf = Buf::default();
        let logger = Logger::json().to_writer(buf.clone()).level(Level::Info);
        let router = Router::new()
            .trace_context()
            .access_log(logger.clone())
            .get("/t", move |_| {
                let logger = logger.clone();
                async move {
                    logger.info("in-handler", &[]);
                    ok("ok")
                }
            });
        let addr = spawn(router).await;

        let trace_id = "11112222333344445555666677778888";
        let inbound = format!("00-{trace_id}-00f067aa0ba902b7-01");
        let (status, head, _) = raw(
            addr,
            "GET",
            "/t",
            &[("traceparent", &inbound), ("x-request-id", "req-9")],
        )
        .await;
        assert_eq!(status, 200);
        assert!(
            head.to_ascii_lowercase().contains("x-request-id: req-9"),
            "{head}"
        );

        let out = buf.text();
        let lines: Vec<&str> = out.lines().filter(|line| line.contains(trace_id)).collect();
        // Both the handler's record and the access record are correlated.
        assert_eq!(lines.len(), 2, "{out}");
        for line in lines {
            assert!(
                line.contains(&format!(r#""trace_id":"{trace_id}""#)),
                "{line}"
            );
            assert!(line.contains(r#""span_id":""#), "{line}");
            assert!(line.contains(r#""request_id":"req-9""#), "{line}");
            assert_eq!(line.matches("\"trace_id\"").count(), 1, "{line}");
        }
    }
}

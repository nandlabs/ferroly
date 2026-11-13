//! Regression tests for the OTLP/HTTP exporter (I8) against a scripted local
//! collector: bounded queue, checked status, retries, flush and shutdown.
//!
//! The exporter is driven directly (`Exporter::export`), so these tests touch
//! no global state and run in parallel.

#![cfg(all(feature = "obs", feature = "http"))]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use ferroly::codec::{json, Value};
use ferroly::obs::{
    Exporter, FinishedSpan, Level, Link, OtlpHttpExporter, Resource, SpanEvent, SpanKind, Status,
};

// ---- a scripted collector ------------------------------------------------------

#[derive(Debug, Clone)]
struct Received {
    head: String,
    body: String,
}

impl Received {
    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().skip(1).find_map(|line| {
            let (k, v) = line.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    }

    fn span_names(&self) -> Vec<String> {
        let doc = json::from_str(&self.body).expect("the body is JSON");
        spans_of(&doc)
            .iter()
            .map(|s| s.get("name").and_then(Value::as_str).unwrap().to_string())
            .collect()
    }
}

fn spans_of(doc: &Value) -> Vec<Value> {
    doc.get("resourceSpans")
        .and_then(|v| v.get_index(0))
        .and_then(|v| v.get("scopeSpans"))
        .and_then(|v| v.get_index(0))
        .and_then(|v| v.get("spans"))
        .and_then(Value::as_array)
        .cloned()
        .expect("resourceSpans[0].scopeSpans[0].spans")
}

#[derive(Clone)]
struct Collector {
    url: String,
    received: Arc<Mutex<Vec<Received>>>,
}

impl Collector {
    /// Answers each request with the next scripted reply (then `200` forever).
    /// A reply of `0` means: read the request and never answer.
    async fn start(script: &[u16]) -> Collector {
        Self::start_with(script, "").await
    }

    async fn start_with(script: &[u16], extra_headers: &'static str) -> Collector {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/traces", listener.local_addr().unwrap());
        let received: Arc<Mutex<Vec<Received>>> = Arc::default();
        let script: Arc<Mutex<VecDeque<u16>>> =
            Arc::new(Mutex::new(script.iter().copied().collect()));
        let sink = received.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let (sink, script) = (sink.clone(), script.clone());
                tokio::spawn(async move {
                    let mut buf: Vec<u8> = Vec::new();
                    loop {
                        // Read one request: head, then `content-length` bytes.
                        let head_end = loop {
                            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                break i + 4;
                            }
                            let mut chunk = [0u8; 8192];
                            match stream.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            }
                        };
                        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                        let length: usize = head
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse().ok())?
                            })
                            .unwrap_or(0);
                        while buf.len() < head_end + length {
                            let mut chunk = [0u8; 8192];
                            match stream.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            }
                        }
                        let body =
                            String::from_utf8_lossy(&buf[head_end..head_end + length]).to_string();
                        buf.drain(..head_end + length);
                        sink.lock().unwrap().push(Received { head, body });

                        let status = script.lock().unwrap().pop_front().unwrap_or(200);
                        if status == 0 {
                            // Stall: hold the connection open without replying.
                            std::future::pending::<()>().await;
                        }
                        let reply = format!(
                            "HTTP/1.1 {status} Scripted\r\ncontent-type: application/json\r\n\
                             {extra_headers}content-length: 2\r\n\r\n{{}}"
                        );
                        if stream.write_all(reply.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Collector { url, received }
    }

    fn requests(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }
}

fn span(name: &str) -> FinishedSpan {
    FinishedSpan {
        name: name.to_string(),
        trace_id: "4bf92f3577b34da6a3ce929d0e0e4736".into(),
        span_id: "00f067aa0ba902b7".into(),
        start_unix_nanos: 1_700_000_000_000_000_000,
        duration_nanos: 1_500_000,
        ..FinishedSpan::default()
    }
}

/// Fast settings so that failure paths finish in milliseconds.
fn builder(url: &str) -> ferroly::obs::OtlpBuilder {
    OtlpHttpExporter::builder(url)
        .service_name("test-service")
        .interval(Duration::from_secs(3600)) // only explicit flushes send
        .retry_backoff(Duration::from_millis(5))
        .timeout(Duration::from_secs(5))
        .flush_timeout(Duration::from_secs(20))
}

// ---- tests ---------------------------------------------------------------------

#[tokio::test]
async fn flush_waits_until_the_collector_has_the_spans() {
    let collector = Collector::start(&[]).await;
    let exporter = builder(&collector.url)
        .resource(
            Resource::new("checkout")
                .service_version("1.4.2")
                .service_instance_id("pod-7")
                .deployment_environment("staging")
                .attribute("team", "payments"),
        )
        .header("authorization", "Bearer s3cr3t-token")
        .header("x-tenant", "acme")
        .build();

    let full = FinishedSpan {
        parent_id: Some("1111111111111111".into()),
        parent_remote: true,
        trace_state: Some("vendor=1".into()),
        level: Level::Warn,
        kind: SpanKind::Server,
        status: Status::error("upstream timeout"),
        fields: vec![
            ("http.response.status_code".into(), Value::UInt(504)),
            ("retried".into(), Value::Bool(true)),
            ("ratio".into(), Value::Float(0.25)),
            ("tags".into(), Value::Array(vec!["a".into(), "b".into()])),
        ],
        events: vec![SpanEvent {
            level: Level::Error,
            message: "gave up".into(),
            fields: vec![("attempt".into(), Value::Int(3))],
            unix_nanos: 1_700_000_000_000_500_000,
        }],
        links: vec![Link {
            trace_id: "a".repeat(32),
            span_id: "b".repeat(16),
            trace_state: None,
            fields: vec![],
        }],
        ..span("GET /orders/:id")
    };
    exporter.export(&full);
    exporter.export(&span("second"));
    assert!(collector.requests().is_empty(), "nothing is sent inline");

    // No sleep: when flush returns, the collector must already have the batch.
    assert!(exporter.flush_async().await, "the batch was accepted");
    let requests = collector.requests();
    assert_eq!(requests.len(), 1, "one batch");
    let request = &requests[0];
    assert!(
        request.head.starts_with("POST /v1/traces HTTP/1.1"),
        "{}",
        request.head
    );
    assert_eq!(
        request.header("content-type").as_deref(),
        Some("application/json")
    );
    assert_eq!(
        request.header("authorization").as_deref(),
        Some("Bearer s3cr3t-token")
    );
    assert_eq!(request.header("x-tenant").as_deref(), Some("acme"));

    // The payload is OTLP JSON with the right field names and encodings.
    let doc = json::from_str(&request.body).unwrap();
    let resource = doc
        .get("resourceSpans")
        .and_then(|v| v.get_index(0))
        .and_then(|v| v.get("resource"))
        .unwrap();
    let resource_text = json::to_string(resource);
    for attr in [
        r#"{"key":"service.name","value":{"stringValue":"checkout"}}"#,
        r#"{"key":"service.version","value":{"stringValue":"1.4.2"}}"#,
        r#"{"key":"service.instance.id","value":{"stringValue":"pod-7"}}"#,
        r#"{"key":"deployment.environment","value":{"stringValue":"staging"}}"#,
        r#"{"key":"team","value":{"stringValue":"payments"}}"#,
    ] {
        assert!(
            resource_text.contains(attr),
            "{attr} missing: {resource_text}"
        );
    }
    let spans = spans_of(&doc);
    assert_eq!(spans.len(), 2);
    let s = &spans[0];
    let text = |key: &str| s.get(key).and_then(Value::as_str).map(str::to_string);
    assert_eq!(
        text("traceId").as_deref(),
        Some("4bf92f3577b34da6a3ce929d0e0e4736")
    );
    assert_eq!(text("spanId").as_deref(), Some("00f067aa0ba902b7"));
    assert_eq!(text("parentSpanId").as_deref(), Some("1111111111111111"));
    assert_eq!(text("traceState").as_deref(), Some("vendor=1"));
    assert_eq!(text("name").as_deref(), Some("GET /orders/:id"));
    assert_eq!(
        s.get("kind").and_then(Value::as_i64),
        Some(2),
        "SPAN_KIND_SERVER"
    );
    assert_eq!(s.get("flags").and_then(Value::as_i64), Some(0x301));
    // 64-bit integers are JSON strings.
    assert_eq!(
        text("startTimeUnixNano").as_deref(),
        Some("1700000000000000000")
    );
    assert_eq!(
        text("endTimeUnixNano").as_deref(),
        Some("1700000000001500000")
    );
    let status = s.get("status").unwrap();
    assert_eq!(
        status.get("code").and_then(Value::as_i64),
        Some(2),
        "STATUS_CODE_ERROR"
    );
    assert_eq!(
        status.get("message").and_then(Value::as_str),
        Some("upstream timeout")
    );
    let attributes = json::to_string(s.get("attributes").unwrap());
    for attr in [
        r#"{"key":"http.response.status_code","value":{"intValue":"504"}}"#,
        r#"{"key":"retried","value":{"boolValue":true}}"#,
        r#"{"key":"ratio","value":{"doubleValue":0.25}}"#,
        r#"{"key":"tags","value":{"arrayValue":{"values":[{"stringValue":"a"},{"stringValue":"b"}]}}}"#,
    ] {
        assert!(attributes.contains(attr), "{attr} missing: {attributes}");
    }
    let event = s.get("events").and_then(|e| e.get_index(0)).unwrap();
    assert_eq!(event.get("name").and_then(Value::as_str), Some("gave up"));
    assert_eq!(
        event.get("timeUnixNano").and_then(Value::as_str),
        Some("1700000000000500000")
    );
    assert!(json::to_string(event).contains(r#"{"key":"attempt","value":{"intValue":"3"}}"#));
    let link = s.get("links").and_then(|l| l.get_index(0)).unwrap();
    assert_eq!(
        link.get("spanId").and_then(Value::as_str),
        Some("b".repeat(16).as_str())
    );
    // The plain span: internal kind, no parent, no status.
    assert_eq!(spans[1].get("kind").and_then(Value::as_i64), Some(1));
    assert!(spans[1].get("parentSpanId").is_none());
    assert!(spans[1].get("status").is_none());
    assert_eq!(spans[1].get("flags").and_then(Value::as_i64), Some(0x101));

    let stats = exporter.stats();
    assert_eq!(stats.exported_spans, 2);
    assert_eq!(stats.sent_batches, 1);
    assert_eq!(
        (
            stats.dropped_spans,
            stats.failed_spans,
            stats.failed_batches
        ),
        (0, 0, 0)
    );
    assert_eq!(stats.last_error, None);

    // Flushing an empty queue sends nothing and succeeds.
    assert!(exporter.flush_async().await);
    assert_eq!(collector.requests().len(), 1);
}

#[tokio::test]
async fn a_rejected_batch_is_counted_not_reported_as_sent() {
    let collector = Collector::start(&[400]).await;
    let exporter = builder(&collector.url).build();
    for i in 0..3 {
        exporter.export(&span(&format!("rejected-{i}")));
    }
    assert!(!exporter.flush_async().await, "HTTP 400 is a failure");
    assert_eq!(collector.requests().len(), 1, "a 400 is not retried");

    let stats = exporter.stats();
    assert_eq!(stats.exported_spans, 0);
    assert_eq!(stats.failed_batches, 1);
    assert_eq!(stats.failed_spans, 3);
    assert_eq!(stats.retries, 0);
    assert!(
        stats.last_error.as_deref().unwrap().contains("400"),
        "{:?}",
        stats.last_error
    );

    // The exporter keeps working afterwards.
    exporter.export(&span("accepted"));
    assert!(exporter.flush_async().await);
    assert_eq!(exporter.stats().exported_spans, 1);
    assert_eq!(collector.requests()[1].span_names(), ["accepted"]);
}

#[tokio::test]
async fn transient_failures_are_retried_with_backoff() {
    let collector = Collector::start(&[503, 429, 200]).await;
    let exporter = builder(&collector.url).max_retries(3).build();
    exporter.export(&span("eventually"));
    assert!(exporter.flush_async().await);

    let requests = collector.requests();
    assert_eq!(requests.len(), 3, "two retries, then success");
    assert!(requests.iter().all(|r| r.span_names() == ["eventually"]));
    let stats = exporter.stats();
    assert_eq!(stats.retries, 2);
    assert_eq!(stats.exported_spans, 1);
    assert_eq!(stats.failed_batches, 0);

    // Retries are bounded.
    let collector = Collector::start(&[503, 503, 503, 503, 503]).await;
    let exporter = builder(&collector.url).max_retries(2).build();
    exporter.export(&span("never"));
    assert!(!exporter.flush_async().await);
    assert_eq!(collector.requests().len(), 3, "one attempt + two retries");
    let stats = exporter.stats();
    assert_eq!(
        (stats.retries, stats.failed_batches, stats.failed_spans),
        (2, 1, 1)
    );
    assert!(stats.last_error.unwrap().contains("503"));
}

#[tokio::test]
async fn retry_after_is_honoured() {
    let collector = Collector::start_with(&[503], "retry-after: 1\r\n").await;
    let exporter = builder(&collector.url)
        .retry_backoff(Duration::from_secs(3600)) // would never finish
        .build();
    exporter.export(&span("paced"));
    let started = Instant::now();
    assert!(exporter.flush_async().await);
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(900), "waited {waited:?}");
    assert_eq!(collector.requests().len(), 2);
}

#[tokio::test]
async fn an_unreachable_collector_fails_fast_and_is_reported() {
    // Reserve a port, then close it: connections are refused.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/traces", listener.local_addr().unwrap());
    drop(listener);

    let exporter = builder(&url).max_retries(1).build();
    exporter.export(&span("lost"));
    assert!(!exporter.flush_async().await);
    let stats = exporter.stats();
    assert_eq!(
        (stats.failed_batches, stats.failed_spans, stats.retries),
        (1, 1, 1)
    );
    assert!(
        stats
            .last_error
            .as_deref()
            .unwrap()
            .contains("request failed"),
        "{:?}",
        stats.last_error
    );

    // A malformed endpoint is a permanent failure: no retries.
    let exporter = builder("not a url").build();
    exporter.export(&span("lost"));
    assert!(!exporter.flush_async().await);
    let stats = exporter.stats();
    assert_eq!((stats.failed_batches, stats.retries), (1, 0));
    assert!(stats.last_error.unwrap().contains("invalid endpoint"));
}

#[tokio::test]
async fn the_queue_is_bounded_when_the_collector_stalls() {
    let collector = Collector::start(&[0]).await; // reads, never answers
    let exporter = builder(&collector.url)
        .queue_size(8)
        .batch_size(4)
        .max_retries(0)
        .timeout(Duration::from_millis(300))
        .build();

    // `export` never blocks, however slow the collector is.
    let started = Instant::now();
    for i in 0..5_000 {
        exporter.export(&span(&format!("burst-{i}")));
    }
    assert!(started.elapsed() < Duration::from_secs(5));

    // At most the queue (8) plus one batch in flight (4) can have been kept.
    let stats = exporter.stats();
    assert!(
        stats.dropped_spans >= 5_000 - 12,
        "dropped only {} of 5000",
        stats.dropped_spans
    );

    // The stalled request times out (a configurable, short timeout), is
    // counted as failed, and the exporter recovers.
    assert!(!exporter.flush_async().await);
    let stats = exporter.stats();
    assert!(stats.failed_batches >= 1);
    assert_eq!(
        stats.dropped_spans + stats.failed_spans + stats.exported_spans,
        5_000,
        "every span is accounted for: {stats:?}"
    );
    exporter.export(&span("after"));
    assert!(exporter.flush_async().await);
}

#[tokio::test]
async fn shutdown_drains_the_queue_and_stops() {
    let collector = Collector::start(&[]).await;
    let exporter = builder(&collector.url).batch_size(3).queue_size(64).build();
    for i in 0..10 {
        exporter.export(&span(&format!("drain-{i}")));
    }
    assert!(exporter.shutdown_async().await);

    let names: Vec<String> = collector
        .requests()
        .iter()
        .flat_map(Received::span_names)
        .collect();
    let expected: Vec<String> = (0..10).map(|i| format!("drain-{i}")).collect();
    assert_eq!(names, expected, "everything queued was delivered, in order");
    assert!(collector
        .requests()
        .iter()
        .all(|r| r.span_names().len() <= 3));
    assert_eq!(exporter.stats().exported_spans, 10);

    // After shutdown spans are dropped and counted; flush reports failure.
    exporter.export(&span("too-late"));
    assert_eq!(exporter.stats().dropped_spans, 1);
    assert!(!exporter.flush_async().await);
    assert_eq!(exporter.stats().exported_spans, 10);
}

#[tokio::test]
async fn a_full_batch_and_the_interval_both_trigger_a_send() {
    let collector = Collector::start(&[]).await;
    let exporter = builder(&collector.url)
        .batch_size(2)
        .interval(Duration::from_millis(20))
        .build();
    exporter.export(&span("one"));
    exporter.export(&span("two"));
    exporter.export(&span("three"));

    // No flush: wait (bounded) for the background task to send on its own.
    let deadline = Instant::now() + Duration::from_secs(20);
    while exporter.stats().exported_spans < 3 {
        assert!(Instant::now() < deadline, "{:?}", exporter.stats());
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let requests = collector.requests();
    assert_eq!(requests[0].span_names(), ["one", "two"], "sent when full");
    assert_eq!(requests[1].span_names(), ["three"], "sent on the interval");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_blocking_flush_waits_on_a_multi_thread_runtime() {
    let collector = Collector::start(&[]).await;
    let exporter = builder(&collector.url).build();
    exporter.export(&span("sync-flush"));
    // The synchronous trait method, as called by `obs::flush()`.
    Exporter::flush(&exporter);
    assert_eq!(
        collector.requests().len(),
        1,
        "delivered before flush returned"
    );
    exporter.export(&span("sync-shutdown"));
    Exporter::shutdown(&exporter);
    assert_eq!(collector.requests().len(), 2);
    assert_eq!(exporter.stats().exported_spans, 2);
}

#[tokio::test]
async fn the_blocking_flush_does_not_stall_a_current_thread_runtime() {
    let collector = Collector::start(&[]).await;
    let exporter = builder(&collector.url).build();
    exporter.export(&span("queued"));
    // On a current-thread runtime the worker shares this thread, so a blocking
    // wait could never succeed: the call must return immediately instead.
    let started = Instant::now();
    Exporter::flush(&exporter);
    assert!(started.elapsed() < Duration::from_secs(2));
    // The request was queued and is served as soon as the task gets to run.
    assert!(exporter.flush_async().await);
    assert_eq!(collector.requests().len(), 1);
}

#[test]
fn the_blocking_flush_works_from_a_plain_thread() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (collector, exporter) = runtime.block_on(async {
        let collector = Collector::start(&[]).await;
        let exporter = builder(&collector.url).build();
        (collector, exporter)
    });
    // Outside any runtime thread (for example the end of `main`).
    exporter.export(&span("from-main"));
    Exporter::flush(&exporter);
    assert_eq!(collector.requests().len(), 1);
    assert_eq!(collector.requests()[0].span_names(), ["from-main"]);
    Exporter::shutdown(&exporter);
}

#[test]
fn spans_from_the_public_api_reach_the_collector_without_feedback() {
    use ferroly::obs::{self, Span};

    // The only test in this file that installs a global exporter.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let collector = Collector::start(&[]).await;
        let exporter = OtlpHttpExporter::new(collector.url.clone(), "legacy-ctor");
        obs::set_exporter(Box::new(exporter.clone()));

        let parent = obs::Context::from_traceparent(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        )
        .unwrap();
        obs::instrument(
            async {
                tokio::task::yield_now().await;
                obs::set_error("boom");
            },
            obs::http::server_span("GET", "/e2e").child_of(&parent),
        )
        .await;
        Span::new("e2e.internal").start().end();

        assert!(exporter.flush_async().await);
        obs::clear_exporters();

        let requests = collector.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].body.contains(r#""stringValue":"legacy-ctor""#));
        let doc = json::from_str(&requests[0].body).unwrap();
        let spans = spans_of(&doc);
        assert_eq!(spans.len(), 2, "the exporter's own request made no span");
        let server = &spans[0];
        assert_eq!(server.get("name").and_then(Value::as_str), Some("GET /e2e"));
        assert_eq!(
            server.get("traceId").and_then(Value::as_str),
            Some("4bf92f3577b34da6a3ce929d0e0e4736")
        );
        assert_eq!(
            server.get("parentSpanId").and_then(Value::as_str),
            Some("00f067aa0ba902b7")
        );
        assert_eq!(server.get("kind").and_then(Value::as_i64), Some(2));
        assert_eq!(
            server
                .get("status")
                .and_then(|s| s.get("code"))
                .and_then(Value::as_i64),
            Some(2)
        );
    });
}

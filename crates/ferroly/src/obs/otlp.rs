//! OTLP/HTTP exporter: ships finished spans to a collector as OTLP JSON.
//!
//! [`OtlpHttpExporter`] batches spans and `POST`s them to an OTLP `/v1/traces`
//! endpoint (`Content-Type: application/json`) using the in-house
//! [`http::Client`](ferroly::http::Client). Because a span ends in a
//! synchronous context, the exporter hands each span to a background task over
//! a **bounded** queue; the task batches and sends them, so `export` never
//! blocks and a slow collector cannot grow memory without limit — spans that
//! do not fit are dropped and counted.
//!
//! Available when both the `obs` and `http` features are enabled. Construct it
//! from inside a running async context (it spawns a background task):
//!
//! ```no_run
//! use std::time::Duration;
//! use ferroly::obs::{self, OtlpHttpExporter, Resource};
//!
//! # async fn setup() {
//! let exporter = OtlpHttpExporter::builder("http://localhost:4318/v1/traces")
//!     .resource(Resource::new("my-service").service_version("1.2.3"))
//!     .header("authorization", "Bearer <token>")
//!     .timeout(Duration::from_secs(5))
//!     .build();
//! obs::set_exporter(Box::new(exporter.clone()));
//!
//! // … on shutdown: wait for the queue to be delivered.
//! exporter.shutdown_async().await;
//! println!("{:?}", exporter.stats());
//! # }
//! ```

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::codec::{json, Value};
use ferroly::http::{Client, Method, Request};
use tokio::runtime::{Handle, RuntimeFlavor};
use tokio::sync::{mpsc, oneshot};

use super::{Exporter, FinishedSpan, Resource, SpanKind, Status};

/// The longest pause between two attempts to deliver one batch.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// How much of a collector's response body is read before it is abandoned.
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// Configuration for an [`OtlpHttpExporter`]; create one with
/// [`OtlpHttpExporter::builder`].
#[derive(Debug, Clone)]
pub struct OtlpBuilder {
    endpoint: String,
    resource: Resource,
    headers: Vec<(String, String)>,
    timeout: Duration,
    batch_size: usize,
    queue_size: usize,
    interval: Duration,
    max_retries: u32,
    retry_backoff: Duration,
    flush_timeout: Duration,
}

impl OtlpBuilder {
    /// Describes the service that emits the spans (default: a resource with
    /// `service.name` = `unknown_service`).
    pub fn resource(mut self, resource: Resource) -> Self {
        self.resource = resource;
        self
    }

    /// Sets only `service.name` on the resource.
    pub fn service_name(mut self, name: impl Into<String>) -> Self {
        self.resource = self.resource.attribute("service.name", name.into());
        self
    }

    /// Adds a header to every export request — typically
    /// `authorization: Bearer …` or a vendor API-key header.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// The time allowed for one export request, connect to last byte (default
    /// 10 s). A request that exceeds it counts as a transient failure.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The most spans sent in one request (default 512, minimum 1). A batch is
    /// sent as soon as it is full.
    pub fn batch_size(mut self, spans: usize) -> Self {
        self.batch_size = spans.max(1);
        self
    }

    /// The most finished spans waiting to be sent (default 2048, minimum 1).
    /// Spans that arrive while the queue is full are dropped and counted in
    /// [`OtlpStats::dropped_spans`].
    pub fn queue_size(mut self, spans: usize) -> Self {
        self.queue_size = spans.max(1);
        self
    }

    /// How often a partly filled batch is sent (default 2 s).
    pub fn interval(mut self, interval: Duration) -> Self {
        self.interval = interval.max(Duration::from_millis(1));
        self
    }

    /// How many times a batch is re-sent after a transient failure — a
    /// transport error, a timeout, or HTTP 429, 502, 503 or 504 (default 3).
    /// Other responses are not retried.
    pub fn max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    /// The pause before the first retry (default 200 ms); it doubles for each
    /// further retry, up to 30 s. A `Retry-After` header given in seconds
    /// overrides it.
    pub fn retry_backoff(mut self, backoff: Duration) -> Self {
        self.retry_backoff = backoff;
        self
    }

    /// The longest [`flush`](Exporter::flush) and [`shutdown`](Exporter::shutdown)
    /// wait for the background task (default 5 s).
    pub fn flush_timeout(mut self, timeout: Duration) -> Self {
        self.flush_timeout = timeout;
        self
    }

    /// Starts the exporter.
    ///
    /// Must be called from within a Tokio runtime — it spawns the background
    /// task that performs the HTTP sends.
    pub fn build(self) -> OtlpHttpExporter {
        let (span_tx, span_rx) = mpsc::channel::<FinishedSpan>(self.queue_size);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<Control>();
        let counters = Arc::new(Counters::default());
        let flush_timeout = self.flush_timeout;
        // The exporter's own HTTP calls must not be traced into itself.
        tokio::spawn(super::suppress(run(
            self,
            span_rx,
            ctl_rx,
            Arc::clone(&counters),
        )));
        OtlpHttpExporter {
            inner: Arc::new(Inner {
                span_tx,
                ctl_tx,
                counters,
                closed: AtomicBool::new(false),
                flush_timeout,
            }),
        }
    }
}

/// A point-in-time copy of an exporter's counters; see
/// [`OtlpHttpExporter::stats`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OtlpStats {
    /// Spans the collector accepted (a 2xx response).
    pub exported_spans: u64,
    /// Spans discarded before sending: the queue was full, or the exporter was
    /// already shut down.
    pub dropped_spans: u64,
    /// Spans in batches that could not be delivered.
    pub failed_spans: u64,
    /// Batches delivered.
    pub sent_batches: u64,
    /// Batches given up on (after retries, or on a non-retryable response).
    pub failed_batches: u64,
    /// Re-sends after a transient failure.
    pub retries: u64,
    /// A description of the most recent failed batch.
    pub last_error: Option<String>,
}

#[derive(Default)]
struct Counters {
    exported_spans: AtomicU64,
    dropped_spans: AtomicU64,
    failed_spans: AtomicU64,
    sent_batches: AtomicU64,
    failed_batches: AtomicU64,
    retries: AtomicU64,
    last_error: Mutex<Option<String>>,
}

/// Where the result of a flush or shutdown request is reported.
enum Ack {
    Blocking(std::sync::mpsc::SyncSender<bool>),
    Async(oneshot::Sender<bool>),
}

impl Ack {
    fn send(self, delivered: bool) {
        match self {
            Ack::Blocking(tx) => {
                let _ = tx.try_send(delivered);
            }
            Ack::Async(tx) => {
                let _ = tx.send(delivered);
            }
        }
    }
}

enum Control {
    /// Send everything queued so far, then acknowledge.
    Flush(Ack),
    /// Send everything queued, acknowledge, and stop.
    Shutdown(Ack),
}

struct Inner {
    span_tx: mpsc::Sender<FinishedSpan>,
    ctl_tx: mpsc::UnboundedSender<Control>,
    counters: Arc<Counters>,
    closed: AtomicBool,
    flush_timeout: Duration,
}

/// An [`Exporter`](super::Exporter) that sends spans to an OTLP/HTTP collector
/// as OTLP JSON.
///
/// - **Bounded:** finished spans wait in a queue of
///   [`queue_size`](OtlpBuilder::queue_size); overflow is dropped and counted,
///   never buffered without limit.
/// - **Checked:** a batch counts as exported only on a 2xx response. Transient
///   failures are retried with exponential backoff; everything else is counted
///   in [`stats`](Self::stats) and reported through
///   [`OtlpStats::last_error`].
/// - **Flushable:** [`flush_async`](Self::flush_async) /
///   [`shutdown_async`](Self::shutdown_async) (and the blocking
///   [`Exporter::flush`] / [`Exporter::shutdown`]) wait until the background
///   task has actually attempted delivery of everything queued.
///
/// Clones share one queue and one background task, so keep a clone to read
/// [`stats`](Self::stats) after handing another to
/// [`set_exporter`](super::set_exporter).
#[derive(Clone)]
pub struct OtlpHttpExporter {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for OtlpHttpExporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OtlpHttpExporter")
            .field("stats", &self.stats())
            .finish()
    }
}

impl OtlpHttpExporter {
    /// Creates an exporter targeting `endpoint` (a full OTLP traces URL such as
    /// `http://host:4318/v1/traces`) tagged with `service_name`, with default
    /// settings. Use [`builder`](Self::builder) to change them.
    ///
    /// Must be called from within a Tokio runtime — it spawns a background task
    /// that performs the HTTP sends.
    pub fn new(endpoint: impl Into<String>, service_name: impl Into<String>) -> OtlpHttpExporter {
        Self::builder(endpoint).service_name(service_name).build()
    }

    /// Starts configuring an exporter for `endpoint`.
    pub fn builder(endpoint: impl Into<String>) -> OtlpBuilder {
        OtlpBuilder {
            endpoint: endpoint.into(),
            resource: Resource::new("unknown_service"),
            headers: Vec::new(),
            timeout: Duration::from_secs(10),
            batch_size: 512,
            queue_size: 2048,
            interval: Duration::from_secs(2),
            max_retries: 3,
            retry_backoff: Duration::from_millis(200),
            flush_timeout: Duration::from_secs(5),
        }
    }

    /// The exporter's counters right now.
    pub fn stats(&self) -> OtlpStats {
        let c = &self.inner.counters;
        OtlpStats {
            exported_spans: c.exported_spans.load(Ordering::Relaxed),
            dropped_spans: c.dropped_spans.load(Ordering::Relaxed),
            failed_spans: c.failed_spans.load(Ordering::Relaxed),
            sent_batches: c.sent_batches.load(Ordering::Relaxed),
            failed_batches: c.failed_batches.load(Ordering::Relaxed),
            retries: c.retries.load(Ordering::Relaxed),
            last_error: c
                .last_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }

    /// Sends everything queued so far and waits for the result, up to the
    /// [flush timeout](OtlpBuilder::flush_timeout). Returns `true` when every
    /// batch was accepted by the collector, `false` on a failed batch, a
    /// timeout, or a stopped exporter.
    pub async fn flush_async(&self) -> bool {
        self.request_async(Control::Flush).await
    }

    /// Sends everything queued, then stops the background task; spans exported
    /// afterwards are dropped (and counted). Returns as
    /// [`flush_async`](Self::flush_async) does.
    pub async fn shutdown_async(&self) -> bool {
        self.inner.closed.store(true, Ordering::Relaxed);
        self.request_async(Control::Shutdown).await
    }

    async fn request_async(&self, make: fn(Ack) -> Control) -> bool {
        let (tx, rx) = oneshot::channel();
        if self.inner.ctl_tx.send(make(Ack::Async(tx))).is_err() {
            return false;
        }
        matches!(
            tokio::time::timeout(self.inner.flush_timeout, rx).await,
            Ok(Ok(true))
        )
    }

    /// The blocking form, for the synchronous [`Exporter`] methods.
    fn request_blocking(&self, make: fn(Ack) -> Control) -> bool {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        if self.inner.ctl_tx.send(make(Ack::Blocking(tx))).is_err() {
            return false;
        }
        let timeout = self.inner.flush_timeout;
        let wait = move || rx.recv_timeout(timeout).unwrap_or(false);
        match Handle::try_current() {
            // On a multi-thread runtime, let the scheduler move other tasks
            // (including the exporter's) off this worker while it waits.
            Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(wait)
            }
            // On a current-thread runtime the background task cannot run
            // while this thread blocks. The request is queued and will be
            // served; waiting here would only stall the runtime.
            Ok(_) => false,
            // Not on a runtime thread: plain blocking wait.
            Err(_) => wait(),
        }
    }
}

impl Exporter for OtlpHttpExporter {
    fn export(&self, span: &FinishedSpan) {
        // Non-blocking hand-off. A full queue (slow collector) or a stopped
        // exporter drops the span and counts it.
        let accepted = !self.inner.closed.load(Ordering::Relaxed)
            && self.inner.span_tx.try_send(span.clone()).is_ok();
        if !accepted {
            self.inner
                .counters
                .dropped_spans
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Sends everything queued and waits for the outcome, up to the
    /// [flush timeout](OtlpBuilder::flush_timeout).
    ///
    /// This blocks the calling thread. Called on a **current-thread** runtime
    /// it cannot wait (the background task shares that thread): the flush is
    /// requested and the call returns at once — use
    /// [`flush_async`](OtlpHttpExporter::flush_async) there.
    fn flush(&self) {
        self.request_blocking(Control::Flush);
    }

    /// Sends everything queued, waits as [`flush`](Exporter::flush) does, and
    /// stops the background task.
    fn shutdown(&self) {
        self.inner.closed.store(true, Ordering::Relaxed);
        self.request_blocking(Control::Shutdown);
    }
}

// ---- background task -----------------------------------------------------------

async fn run(
    cfg: OtlpBuilder,
    mut spans: mpsc::Receiver<FinishedSpan>,
    mut control: mpsc::UnboundedReceiver<Control>,
    counters: Arc<Counters>,
) {
    let client = Client::new().with_timeout(Some(cfg.timeout));
    let mut batch: Vec<FinishedSpan> = Vec::with_capacity(cfg.batch_size.min(1024));
    // The first tick is one full interval away (a plain `interval` fires at
    // once, which would send a needlessly small first batch).
    let mut ticker =
        tokio::time::interval_at(tokio::time::Instant::now() + cfg.interval, cfg.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            // In order: a flush or shutdown request is served before more
            // spans are read (it drains the queue itself), and the timer only
            // matters when nothing else is ready.
            biased;
            request = control.recv() => match request {
                Some(Control::Flush(ack)) => {
                    let ok = drain(&client, &cfg, &counters, &mut spans, &mut batch).await;
                    ack.send(ok);
                }
                Some(Control::Shutdown(ack)) => {
                    spans.close();
                    let ok = drain(&client, &cfg, &counters, &mut spans, &mut batch).await;
                    ack.send(ok);
                    break;
                }
                // Every handle is gone: deliver what is left and stop.
                None => {
                    drain(&client, &cfg, &counters, &mut spans, &mut batch).await;
                    break;
                }
            },
            span = spans.recv() => match span {
                Some(span) => {
                    batch.push(span);
                    if batch.len() >= cfg.batch_size {
                        send_batch(&client, &cfg, &counters, &mut batch).await;
                    }
                }
                None => {
                    send_batch(&client, &cfg, &counters, &mut batch).await;
                    break;
                }
            },
            _ = ticker.tick() => {
                send_batch(&client, &cfg, &counters, &mut batch).await;
            }
        }
    }
}

/// Sends everything already queued; `true` when every batch was accepted.
async fn drain(
    client: &Client,
    cfg: &OtlpBuilder,
    counters: &Counters,
    spans: &mut mpsc::Receiver<FinishedSpan>,
    batch: &mut Vec<FinishedSpan>,
) -> bool {
    let mut ok = true;
    while let Ok(span) = spans.try_recv() {
        batch.push(span);
        if batch.len() >= cfg.batch_size {
            ok &= send_batch(client, cfg, counters, batch).await;
        }
    }
    ok & send_batch(client, cfg, counters, batch).await
}

/// One failed delivery attempt.
struct Failure {
    transient: bool,
    message: String,
    /// The collector's `Retry-After`, when it gave one in seconds.
    retry_after: Option<Duration>,
}

/// Encodes and delivers `batch` (emptying it), retrying transient failures.
/// The batch is released either way, so a broken collector cannot make memory
/// grow. Returns `true` when the collector accepted it.
async fn send_batch(
    client: &Client,
    cfg: &OtlpBuilder,
    counters: &Counters,
    batch: &mut Vec<FinishedSpan>,
) -> bool {
    if batch.is_empty() {
        return true;
    }
    let count = batch.len() as u64;
    let payload = json::to_string(&otlp_document(&cfg.resource, batch)).into_bytes();
    batch.clear();

    let mut backoff = cfg.retry_backoff;
    let mut attempt = 0;
    loop {
        let failure = match post(client, cfg, payload.clone()).await {
            Ok(()) => {
                counters.exported_spans.fetch_add(count, Ordering::Relaxed);
                counters.sent_batches.fetch_add(1, Ordering::Relaxed);
                return true;
            }
            Err(failure) => failure,
        };
        if failure.transient && attempt < cfg.max_retries {
            attempt += 1;
            counters.retries.fetch_add(1, Ordering::Relaxed);
            let pause = failure.retry_after.unwrap_or(backoff).min(MAX_BACKOFF);
            tokio::time::sleep(pause).await;
            backoff = backoff.saturating_mul(2).min(MAX_BACKOFF);
            continue;
        }
        counters.failed_batches.fetch_add(1, Ordering::Relaxed);
        counters.failed_spans.fetch_add(count, Ordering::Relaxed);
        *counters
            .last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(failure.message);
        return false;
    }
}

/// One `POST`; `Ok` only for a 2xx response.
async fn post(client: &Client, cfg: &OtlpBuilder, payload: Vec<u8>) -> Result<(), Failure> {
    let mut request = Request::builder(Method::Post, &cfg.endpoint)
        .map_err(|e| Failure {
            transient: false,
            message: format!("invalid endpoint {:?}: {e}", cfg.endpoint),
            retry_after: None,
        })?
        .header("content-type", "application/json");
    for (name, value) in &cfg.headers {
        request = request.header(name.clone(), value.clone());
    }
    let mut response = client
        .send(request.body(payload).build())
        .await
        .map_err(|e| Failure {
            transient: true,
            message: format!("request failed: {e}"),
            retry_after: None,
        })?;
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    // Read the (small) body so the connection can be reused, but never more
    // than a bounded amount.
    let mut read = 0;
    while let Ok(Some(chunk)) = response.chunk().await {
        read += chunk.len();
        if read > MAX_RESPONSE_BYTES {
            break;
        }
    }
    if (200..300).contains(&status) {
        return Ok(());
    }
    Err(Failure {
        transient: matches!(status, 429 | 502 | 503 | 504),
        message: format!("collector responded with HTTP {status}"),
        retry_after,
    })
}

// ---- OTLP JSON encoding ----------------------------------------------------------

/// Builds the OTLP `ExportTraceServiceRequest` JSON document for `spans`.
fn otlp_document(resource: &Resource, spans: &[FinishedSpan]) -> Value {
    let span_values: Vec<Value> = spans.iter().map(otlp_span).collect();
    let resource = Value::Object(vec![(
        "attributes".into(),
        attributes(resource.attributes()),
    )]);
    let scope_spans = Value::Object(vec![
        (
            "scope".into(),
            Value::Object(vec![
                ("name".into(), Value::Str("ferroly".into())),
                (
                    "version".into(),
                    Value::Str(env!("CARGO_PKG_VERSION").into()),
                ),
            ]),
        ),
        ("spans".into(), Value::Array(span_values)),
    ]);
    let resource_spans = Value::Object(vec![
        ("resource".into(), resource),
        ("scopeSpans".into(), Value::Array(vec![scope_spans])),
    ]);
    Value::Object(vec![(
        "resourceSpans".into(),
        Value::Array(vec![resource_spans]),
    )])
}

/// The `SpanKind` enum number (OTLP JSON carries enums as integers).
fn kind_code(kind: SpanKind) -> i64 {
    match kind {
        SpanKind::Internal => 1,
        SpanKind::Server => 2,
        SpanKind::Client => 3,
        SpanKind::Producer => 4,
        SpanKind::Consumer => 5,
    }
}

/// Span flags: the W3C trace flags in the low byte (exported spans are
/// sampled), bit 8 = "the remote-parent bit is meaningful", bit 9 = "the parent
/// is remote".
fn span_flags(parent_remote: bool) -> i64 {
    0x01 | 0x100 | if parent_remote { 0x200 } else { 0 }
}

fn otlp_span(span: &FinishedSpan) -> Value {
    let end = span.start_unix_nanos.saturating_add(span.duration_nanos);
    let mut obj = vec![
        ("traceId".into(), Value::Str(span.trace_id.clone())),
        ("spanId".into(), Value::Str(span.span_id.clone())),
    ];
    if let Some(state) = &span.trace_state {
        obj.push(("traceState".into(), Value::Str(state.clone())));
    }
    if let Some(parent) = &span.parent_id {
        obj.push(("parentSpanId".into(), Value::Str(parent.clone())));
    }
    obj.push(("flags".into(), Value::Int(span_flags(span.parent_remote))));
    obj.push(("name".into(), Value::Str(span.name.clone())));
    obj.push(("kind".into(), Value::Int(kind_code(span.kind))));
    // 64-bit integers are strings in OTLP JSON.
    obj.push((
        "startTimeUnixNano".into(),
        Value::Str(span.start_unix_nanos.to_string()),
    ));
    obj.push(("endTimeUnixNano".into(), Value::Str(end.to_string())));
    obj.push(("attributes".into(), attributes(&span.fields)));
    if !span.events.is_empty() {
        let events: Vec<Value> = span
            .events
            .iter()
            .map(|e| {
                Value::Object(vec![
                    ("timeUnixNano".into(), Value::Str(e.unix_nanos.to_string())),
                    ("name".into(), Value::Str(e.message.clone())),
                    ("attributes".into(), attributes(&e.fields)),
                ])
            })
            .collect();
        obj.push(("events".into(), Value::Array(events)));
    }
    if !span.links.is_empty() {
        let links: Vec<Value> = span
            .links
            .iter()
            .map(|link| {
                let mut l = vec![
                    ("traceId".into(), Value::Str(link.trace_id.clone())),
                    ("spanId".into(), Value::Str(link.span_id.clone())),
                ];
                if let Some(state) = &link.trace_state {
                    l.push(("traceState".into(), Value::Str(state.clone())));
                }
                l.push(("attributes".into(), attributes(&link.fields)));
                Value::Object(l)
            })
            .collect();
        obj.push(("links".into(), Value::Array(links)));
    }
    // STATUS_CODE_UNSET (0) is the default and is left out.
    match &span.status {
        Status::Unset => {}
        Status::Ok => obj.push((
            "status".into(),
            Value::Object(vec![("code".into(), Value::Int(1))]),
        )),
        Status::Error(message) => obj.push((
            "status".into(),
            Value::Object(vec![
                ("message".into(), Value::Str(message.clone())),
                ("code".into(), Value::Int(2)),
            ]),
        )),
    }
    Value::Object(obj)
}

/// Converts field pairs into an OTLP `KeyValue` array.
fn attributes(fields: &[(String, Value)]) -> Value {
    Value::Array(fields.iter().map(|(k, v)| key_value(k, v)).collect())
}

fn key_value(key: &str, value: &Value) -> Value {
    Value::Object(vec![
        ("key".into(), Value::Str(key.to_string())),
        ("value".into(), any_value(value)),
    ])
}

/// Wraps a [`Value`] as an OTLP `AnyValue`. 64-bit integers are carried as
/// strings, non-finite doubles as `"NaN"` / `"Infinity"` / `"-Infinity"`,
/// bytes as base64, and arrays and objects as `arrayValue` / `kvlistValue`.
/// `Null` becomes an empty `AnyValue`.
fn any_value(value: &Value) -> Value {
    let (key, inner) = match value {
        Value::Null => return Value::Object(Vec::new()),
        Value::Str(s) => ("stringValue", Value::Str(s.clone())),
        Value::Bool(b) => ("boolValue", Value::Bool(*b)),
        Value::Int(i) => ("intValue", Value::Str(i.to_string())),
        Value::UInt(u) => ("intValue", Value::Str(u.to_string())),
        Value::Float(f) if f.is_finite() => ("doubleValue", Value::Float(*f)),
        Value::Float(f) => {
            let text = if f.is_nan() {
                "NaN"
            } else if *f > 0.0 {
                "Infinity"
            } else {
                "-Infinity"
            };
            ("doubleValue", Value::Str(text.into()))
        }
        Value::Bytes(bytes) => ("bytesValue", Value::Str(base64(bytes))),
        Value::Array(items) => (
            "arrayValue",
            Value::Object(vec![(
                "values".into(),
                Value::Array(items.iter().map(any_value).collect()),
            )]),
        ),
        Value::Object(entries) => (
            "kvlistValue",
            Value::Object(vec![(
                "values".into(),
                Value::Array(entries.iter().map(|(k, v)| key_value(k, v)).collect()),
            )]),
        ),
    };
    Value::Object(vec![(key.into(), inner)])
}

/// Standard base64 with padding.
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        out.push(TABLE[(b[0] >> 2) as usize] as char);
        out.push(TABLE[(((b[0] & 0x03) << 4) | (b[1] >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b[1] & 0x0f) << 2) | (b[2] >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b[2] & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroly::obs::{Level, Link, SpanEvent};

    fn sample_span() -> FinishedSpan {
        FinishedSpan {
            name: "handle".into(),
            trace_id: "0123456789abcdef0123456789abcdef".into(),
            span_id: "0123456789abcdef".into(),
            start_unix_nanos: 1_000,
            duration_nanos: 500,
            fields: vec![
                ("http.status".into(), Value::Int(200)),
                ("route".into(), Value::Str("/x".into())),
            ],
            events: vec![SpanEvent {
                level: Level::Warn,
                message: "slow".into(),
                fields: vec![("ms".into(), Value::UInt(42))],
                unix_nanos: 1_200,
            }],
            ..FinishedSpan::default()
        }
    }

    fn first_span(doc: &Value) -> &Value {
        doc.get("resourceSpans")
            .and_then(|v| v.get_index(0))
            .and_then(|v| v.get("scopeSpans"))
            .and_then(|v| v.get_index(0))
            .and_then(|v| v.get("spans"))
            .and_then(|v| v.get_index(0))
            .expect("resourceSpans[0].scopeSpans[0].spans[0]")
    }

    #[test]
    fn document_shape_is_otlp_json() {
        let doc = otlp_document(&Resource::new("svc"), &[sample_span()]);
        let text = json::to_string(&doc);
        assert!(text.contains("\"resourceSpans\""));
        assert!(text.contains("\"scopeSpans\""));
        assert!(text.contains("\"service.name\""));
        // 64-bit ints and nanos are strings in OTLP JSON.
        assert!(text.contains("\"intValue\":\"200\""));
        assert!(text.contains("\"startTimeUnixNano\":\"1000\""));
        assert!(text.contains("\"endTimeUnixNano\":\"1500\""));
        assert!(text.contains("\"kind\":1"));
        // The event is carried.
        assert!(text.contains("\"slow\""));

        let span = first_span(&doc);
        assert_eq!(
            span.get("traceId").and_then(Value::as_str),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(
            span.get("spanId").and_then(Value::as_str),
            Some("0123456789abcdef")
        );
        assert!(span.get("parentSpanId").is_none(), "root span");
        assert!(span.get("status").is_none(), "unset status is omitted");
        assert_eq!(span.get("flags").and_then(Value::as_i64), Some(0x101));
        let scope = doc
            .get("resourceSpans")
            .and_then(|v| v.get_index(0))
            .and_then(|v| v.get("scopeSpans"))
            .and_then(|v| v.get_index(0))
            .and_then(|v| v.get("scope"))
            .unwrap();
        assert_eq!(scope.get("name").and_then(Value::as_str), Some("ferroly"));
    }

    #[test]
    fn kind_status_links_and_resource_are_encoded() {
        let kinds = [
            (SpanKind::Internal, 1),
            (SpanKind::Server, 2),
            (SpanKind::Client, 3),
            (SpanKind::Producer, 4),
            (SpanKind::Consumer, 5),
        ];
        for (kind, code) in kinds {
            let v = otlp_span(&FinishedSpan {
                kind,
                ..sample_span()
            });
            assert_eq!(v.get("kind").and_then(Value::as_i64), Some(code));
        }

        let span = FinishedSpan {
            parent_id: Some("fedcba9876543210".into()),
            parent_remote: true,
            trace_state: Some("vendor=1".into()),
            status: Status::error("boom"),
            links: vec![Link {
                trace_id: "a".repeat(32),
                span_id: "b".repeat(16),
                trace_state: Some("other=2".into()),
                fields: vec![("reason".into(), Value::Str("batch".into()))],
            }],
            ..sample_span()
        };
        let resource = Resource::new("checkout")
            .service_version("1.2.3")
            .service_instance_id("i-1")
            .deployment_environment("prod")
            .attribute("replicas", 3);
        let doc = otlp_document(&resource, &[span]);
        let v = first_span(&doc);
        assert_eq!(
            v.get("parentSpanId").and_then(Value::as_str),
            Some("fedcba9876543210")
        );
        assert_eq!(v.get("flags").and_then(Value::as_i64), Some(0x301));
        assert_eq!(
            v.get("traceState").and_then(Value::as_str),
            Some("vendor=1")
        );
        let status = v.get("status").unwrap();
        assert_eq!(status.get("code").and_then(Value::as_i64), Some(2));
        assert_eq!(status.get("message").and_then(Value::as_str), Some("boom"));
        let link = v.get("links").and_then(|l| l.get_index(0)).unwrap();
        assert_eq!(
            link.get("traceId").and_then(Value::as_str),
            Some("a".repeat(32).as_str())
        );
        assert_eq!(
            link.get("traceState").and_then(Value::as_str),
            Some("other=2")
        );
        assert!(json::to_string(link).contains("\"reason\""));

        let ok = otlp_span(&FinishedSpan {
            status: Status::Ok,
            ..sample_span()
        });
        let status = ok.get("status").unwrap();
        assert_eq!(status.get("code").and_then(Value::as_i64), Some(1));
        assert!(status.get("message").is_none());

        let text = json::to_string(&doc);
        for expected in [
            r#"{"key":"service.name","value":{"stringValue":"checkout"}}"#,
            r#"{"key":"service.version","value":{"stringValue":"1.2.3"}}"#,
            r#"{"key":"service.instance.id","value":{"stringValue":"i-1"}}"#,
            r#"{"key":"deployment.environment","value":{"stringValue":"prod"}}"#,
            r#"{"key":"replicas","value":{"intValue":"3"}}"#,
        ] {
            assert!(text.contains(expected), "{expected} missing from {text}");
        }
    }

    #[test]
    fn any_value_maps_kinds() {
        let enc = |v: Value| json::to_string(&any_value(&v));
        assert_eq!(enc(Value::Bool(true)), r#"{"boolValue":true}"#);
        assert_eq!(enc(Value::Float(1.5)), r#"{"doubleValue":1.5}"#);
        assert_eq!(enc(Value::Str("s".into())), r#"{"stringValue":"s"}"#);
        assert_eq!(enc(Value::Int(-7)), r#"{"intValue":"-7"}"#);
        assert_eq!(
            enc(Value::UInt(u64::MAX)),
            r#"{"intValue":"18446744073709551615"}"#
        );
        assert_eq!(enc(Value::Null), "{}");
        assert_eq!(enc(Value::Float(f64::NAN)), r#"{"doubleValue":"NaN"}"#);
        assert_eq!(
            enc(Value::Float(f64::NEG_INFINITY)),
            r#"{"doubleValue":"-Infinity"}"#
        );
        assert_eq!(
            enc(Value::Bytes(b"hi!?".to_vec())),
            r#"{"bytesValue":"aGkhPw=="}"#
        );
        assert_eq!(
            enc(Value::Array(vec![Value::Int(1), Value::Str("a".into())])),
            r#"{"arrayValue":{"values":[{"intValue":"1"},{"stringValue":"a"}]}}"#
        );
        assert_eq!(
            enc(Value::Object(vec![("k".into(), Value::Bool(false))])),
            r#"{"kvlistValue":{"values":[{"key":"k","value":{"boolValue":false}}]}}"#
        );
    }

    #[test]
    fn base64_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}

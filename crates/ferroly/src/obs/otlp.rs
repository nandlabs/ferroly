//! OTLP/HTTP exporter: ships finished spans to a collector as OTLP JSON.
//!
//! [`OtlpHttpExporter`] batches spans and `POST`s them to an OTLP `/v1/traces`
//! endpoint (`Content-Type: application/json`) using the in-house
//! [`http::Client`](crate::http::Client). Because a span's guard drops in a
//! synchronous context, the exporter hands each span to a background task over
//! a channel; the task batches and sends them asynchronously, so `export` never
//! blocks.
//!
//! Available when both the `obs` and `http` features are enabled. Construct it
//! from inside a running async context (it spawns a background task):
//!
//! ```no_run
//! use ferroly::obs::{self, OtlpHttpExporter};
//!
//! # async fn setup() {
//! obs::set_exporter(Box::new(OtlpHttpExporter::new(
//!     "http://localhost:4318/v1/traces",
//!     "my-service",
//! )));
//! # }
//! ```

use ferroly::codec::{json, Value};
use ferroly::http::{Client, Method, Request};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use super::{Exporter, FinishedSpan};

/// Flush a batch once it reaches this many spans.
const BATCH_LIMIT: usize = 512;
/// Also flush at least this often so small trailing batches are not stranded.
const FLUSH_INTERVAL_SECS: u64 = 2;

enum Msg {
    Span(Box<FinishedSpan>),
    Flush,
}

/// An [`Exporter`](super::Exporter) that sends spans to an OTLP/HTTP collector
/// as OTLP JSON. Best-effort: transport errors are dropped rather than
/// surfaced, so telemetry never breaks the application.
pub struct OtlpHttpExporter {
    tx: UnboundedSender<Msg>,
}

impl OtlpHttpExporter {
    /// Creates an exporter targeting `endpoint` (a full OTLP traces URL such as
    /// `http://host:4318/v1/traces`) tagged with `service_name`.
    ///
    /// Must be called from within a Tokio runtime — it spawns a background task
    /// that performs the HTTP sends.
    pub fn new(endpoint: impl Into<String>, service_name: impl Into<String>) -> OtlpHttpExporter {
        let endpoint = endpoint.into();
        let service_name = service_name.into();
        let (tx, mut rx) = unbounded_channel::<Msg>();
        tokio::spawn(async move {
            let client = Client::new();
            let mut buf: Vec<FinishedSpan> = Vec::new();
            let mut ticker =
                tokio::time::interval(std::time::Duration::from_secs(FLUSH_INTERVAL_SECS));
            loop {
                tokio::select! {
                    msg = rx.recv() => match msg {
                        Some(Msg::Span(span)) => {
                            buf.push(*span);
                            if buf.len() >= BATCH_LIMIT {
                                post(&client, &endpoint, &service_name, &mut buf).await;
                            }
                        }
                        Some(Msg::Flush) => post(&client, &endpoint, &service_name, &mut buf).await,
                        None => {
                            // All senders dropped: flush and stop.
                            post(&client, &endpoint, &service_name, &mut buf).await;
                            break;
                        }
                    },
                    _ = ticker.tick() => {
                        post(&client, &endpoint, &service_name, &mut buf).await;
                    }
                }
            }
        });
        OtlpHttpExporter { tx }
    }
}

impl Exporter for OtlpHttpExporter {
    fn export(&self, span: &FinishedSpan) {
        // Non-blocking hand-off; if the background task is gone the span is
        // silently dropped (best-effort telemetry).
        let _ = self.tx.send(Msg::Span(Box::new(span.clone())));
    }

    fn flush(&self) {
        let _ = self.tx.send(Msg::Flush);
    }
}

/// Encodes and sends the buffered spans, then clears the buffer. A failed send
/// still clears the buffer so a broken collector cannot grow memory unbounded.
async fn post(client: &Client, endpoint: &str, service: &str, buf: &mut Vec<FinishedSpan>) {
    if buf.is_empty() {
        return;
    }
    let payload = json::to_string(&otlp_document(service, buf));
    buf.clear();
    if let Ok(req) = Request::builder(Method::Post, endpoint) {
        let req = req
            .header("content-type", "application/json")
            .body(payload.into_bytes())
            .build();
        let _ = client.send(req).await;
    }
}

/// Builds the OTLP `ExportTraceServiceRequest` JSON document for `spans`.
fn otlp_document(service: &str, spans: &[FinishedSpan]) -> Value {
    let span_values: Vec<Value> = spans.iter().map(otlp_span).collect();
    let resource = Value::Object(vec![(
        "attributes".into(),
        Value::Array(vec![attribute(
            "service.name",
            Value::Str(service.to_string()),
        )]),
    )]);
    let scope_spans = Value::Object(vec![
        (
            "scope".into(),
            Value::Object(vec![("name".into(), Value::Str("ferroly".into()))]),
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

fn otlp_span(span: &FinishedSpan) -> Value {
    let end = span.start_unix_nanos.saturating_add(span.duration_nanos);
    let mut obj = vec![
        ("traceId".into(), Value::Str(span.trace_id.clone())),
        ("spanId".into(), Value::Str(span.span_id.clone())),
        ("name".into(), Value::Str(span.name.clone())),
        ("kind".into(), Value::Int(1)), // SPAN_KIND_INTERNAL
        (
            "startTimeUnixNano".into(),
            Value::Str(span.start_unix_nanos.to_string()),
        ),
        ("endTimeUnixNano".into(), Value::Str(end.to_string())),
        ("attributes".into(), attributes(&span.fields)),
    ];
    if let Some(parent) = &span.parent_id {
        obj.push(("parentSpanId".into(), Value::Str(parent.clone())));
    }
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
    Value::Object(obj)
}

/// Converts field pairs into an OTLP `KeyValue` array.
fn attributes(fields: &[(String, Value)]) -> Value {
    Value::Array(
        fields
            .iter()
            .map(|(k, v)| attribute(k, v.clone()))
            .collect(),
    )
}

fn attribute(key: &str, value: Value) -> Value {
    Value::Object(vec![
        ("key".into(), Value::Str(key.to_string())),
        ("value".into(), any_value(value)),
    ])
}

/// Wraps a [`Value`] as an OTLP `AnyValue`. OTLP JSON carries 64-bit integers as
/// strings; other scalars map to their natural OTLP kind, and anything without a
/// direct mapping is rendered as a string.
fn any_value(value: Value) -> Value {
    let (key, inner) = match value {
        Value::Str(s) => ("stringValue", Value::Str(s)),
        Value::Bool(b) => ("boolValue", Value::Bool(b)),
        Value::Int(i) => ("intValue", Value::Str(i.to_string())),
        Value::UInt(u) => ("intValue", Value::Str(u.to_string())),
        Value::Float(f) => ("doubleValue", Value::Float(f)),
        other => ("stringValue", Value::Str(json::to_string(&other))),
    };
    Value::Object(vec![(key.into(), inner)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::{Level, SpanEvent};

    fn sample_span() -> FinishedSpan {
        FinishedSpan {
            name: "handle".into(),
            trace_id: "0123456789abcdef0123456789abcdef".into(),
            span_id: "0123456789abcdef".into(),
            parent_id: None,
            level: Level::Info,
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
        }
    }

    #[test]
    fn document_shape_is_otlp_json() {
        let doc = json::to_string(&otlp_document("svc", &[sample_span()]));
        assert!(doc.contains("\"resourceSpans\""));
        assert!(doc.contains("\"scopeSpans\""));
        assert!(doc.contains("\"service.name\""));
        // 64-bit ints and nanos are strings in OTLP JSON.
        assert!(doc.contains("\"intValue\":\"200\""));
        assert!(doc.contains("\"startTimeUnixNano\":\"1000\""));
        assert!(doc.contains("\"endTimeUnixNano\":\"1500\""));
        assert!(doc.contains("\"kind\":1"));
        // The event is carried.
        assert!(doc.contains("\"slow\""));
    }

    #[test]
    fn any_value_maps_kinds() {
        assert!(json::to_string(&any_value(Value::Bool(true))).contains("\"boolValue\":true"));
        assert!(json::to_string(&any_value(Value::Float(1.5))).contains("\"doubleValue\":1.5"));
        assert!(
            json::to_string(&any_value(Value::Str("s".into()))).contains("\"stringValue\":\"s\"")
        );
    }
}

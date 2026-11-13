//! Observation hooks for provider calls and routing decisions.
//!
//! [`GenAiObserver`] is a plain trait with no dependency on any other ferroly
//! feature: implement it to feed your own telemetry. Ready-made adapters are
//! compiled in when the matching feature is on:
//!
//! | Adapter | Feature | Emits |
//! |---|---|---|
//! | [`ObsObserver`] | `obs` | one client span per call (`gen_ai.*` fields), trace-context headers on the outgoing request |
//! | [`MetricsObserver`] | `metrics` | request, token, cost and latency series |
//! | [`LogObserver`] | `log` | one structured record per call and per routing decision; optional payload logging |
//!
//! Install one on a provider with
//! [`ProviderOptions::observer`](crate::genai::ProviderOptions::observer) and
//! on a router with `RouterBuilder::observer`; combine several with
//! [`MultiObserver`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ferroly::codec::Value;
use ferroly::genai::router::RouteReport;
use ferroly::genai::{FinishReason, Usage};

/// What kind of provider call is being observed.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operation {
    /// A non-streaming completion.
    Chat,
    /// A streaming completion.
    ChatStream,
    /// An embeddings call.
    Embed,
}

impl Operation {
    /// A stable label: `"chat"`, `"chat_stream"` or `"embed"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Operation::Chat => "chat",
            Operation::ChatStream => "chat_stream",
            Operation::Embed => "embed",
        }
    }
}

/// Identifies one provider call across the observer callbacks.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RequestInfo {
    /// A process-unique id, the same in every callback for one call.
    pub id: u64,
    /// The provider name.
    pub provider: String,
    /// The requested model.
    pub model: String,
    /// The kind of call.
    pub operation: Operation,
}

impl RequestInfo {
    /// A new call descriptor with a fresh id.
    pub fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
        operation: Operation,
    ) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        RequestInfo {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            provider: provider.into(),
            model: model.into(),
            operation,
        }
    }
}

/// How one provider call ended.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct RequestOutcome {
    /// Wall-clock time from the start of the call to its end (for a stream:
    /// until the last chunk).
    pub latency: Duration,
    /// For a stream, the time until the first chunk arrived.
    pub time_to_first_chunk: Option<Duration>,
    /// Token usage, if the provider reported it.
    pub usage: Option<Usage>,
    /// The cost of the call in USD, computed from `usage` and the provider's
    /// catalog prices; `None` when either is unknown.
    pub cost_usd: Option<f64>,
    /// Why generation stopped.
    pub finish_reason: Option<FinishReason>,
    /// The model that served the call, as the provider reported it.
    pub response_model: Option<String>,
    /// `None` on success; otherwise the failure's
    /// [`kind_label`](crate::genai::GenAiError::kind_label).
    pub error_kind: Option<&'static str>,
    /// The HTTP status of a failed call, when there was one.
    pub status: Option<u16>,
    /// The call was abandoned by the caller (future dropped, or stream
    /// receiver dropped) before it finished.
    pub cancelled: bool,
}

impl RequestOutcome {
    /// A label for the result: `"ok"`, `"cancelled"`, or the error kind.
    pub fn outcome_label(&self) -> &'static str {
        if self.cancelled {
            "cancelled"
        } else {
            self.error_kind.unwrap_or("ok")
        }
    }
}

/// Which body a payload callback carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadDirection {
    /// The JSON body sent to the provider.
    Request,
    /// The JSON body of a non-streaming response.
    Response,
}

/// Receives events about provider calls and routing decisions.
///
/// Every method has an empty default, so an implementation overrides only what
/// it needs. The callbacks run inline on the calling task: keep them short and
/// never block.
///
/// Credentials never reach an observer: payload callbacks carry request and
/// response **bodies** only, never headers.
pub trait GenAiObserver: Send + Sync {
    /// A provider call is about to be sent.
    fn on_request_start(&self, info: &RequestInfo) {
        let _ = info;
    }

    /// A provider call finished, failed or was abandoned. Called exactly once
    /// for every `on_request_start`.
    fn on_request_end(&self, info: &RequestInfo, outcome: &RequestOutcome) {
        let _ = (info, outcome);
    }

    /// Extra headers to add to the outgoing request — the hook trace-context
    /// propagation uses. Called after `on_request_start`.
    fn request_headers(&self, info: &RequestInfo) -> Vec<(String, String)> {
        let _ = info;
        Vec::new()
    }

    /// Whether [`on_payload`](GenAiObserver::on_payload) should be called.
    /// `false` by default: prompts and completions are not handed out unless
    /// an observer asks for them.
    fn wants_payloads(&self) -> bool {
        false
    }

    /// A request or (non-streaming) response body, verbatim. Only called when
    /// [`wants_payloads`](GenAiObserver::wants_payloads) is `true`. Streamed
    /// responses are not delivered here. Use [`redact_payload`] before storing
    /// a body anywhere prompts must not appear.
    fn on_payload(&self, info: &RequestInfo, direction: PayloadDirection, body: &Value) {
        let _ = (info, direction, body);
    }

    /// The model router finished routing one task (successfully or not).
    fn on_route(&self, report: &RouteReport) {
        let _ = report;
    }
}

/// Fans every callback out to several observers, in order.
#[derive(Clone, Default)]
pub struct MultiObserver {
    observers: Vec<Arc<dyn GenAiObserver>>,
}

impl MultiObserver {
    /// An observer forwarding to each of `observers`.
    pub fn new(observers: Vec<Arc<dyn GenAiObserver>>) -> Self {
        MultiObserver { observers }
    }

    /// Adds one more observer.
    pub fn with(mut self, observer: Arc<dyn GenAiObserver>) -> Self {
        self.observers.push(observer);
        self
    }
}

impl std::fmt::Debug for MultiObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MultiObserver")
            .field("observers", &self.observers.len())
            .finish()
    }
}

impl GenAiObserver for MultiObserver {
    fn on_request_start(&self, info: &RequestInfo) {
        for o in &self.observers {
            o.on_request_start(info);
        }
    }
    fn on_request_end(&self, info: &RequestInfo, outcome: &RequestOutcome) {
        for o in &self.observers {
            o.on_request_end(info, outcome);
        }
    }
    fn request_headers(&self, info: &RequestInfo) -> Vec<(String, String)> {
        self.observers
            .iter()
            .flat_map(|o| o.request_headers(info))
            .collect()
    }
    fn wants_payloads(&self) -> bool {
        self.observers.iter().any(|o| o.wants_payloads())
    }
    fn on_payload(&self, info: &RequestInfo, direction: PayloadDirection, body: &Value) {
        for o in &self.observers {
            if o.wants_payloads() {
                o.on_payload(info, direction, body);
            }
        }
    }
    fn on_route(&self, report: &RouteReport) {
        for o in &self.observers {
            o.on_route(report);
        }
    }
}

/// Keys whose string values describe the *shape* of a payload rather than its
/// content, and are therefore kept by [`redact_payload`].
const STRUCTURAL_KEYS: &[&str] = &[
    "model",
    "role",
    "type",
    "name",
    "id",
    "tool_call_id",
    "tool_use_id",
    "tool_name",
    "finish_reason",
    "stop_reason",
    "done_reason",
    "object",
    "media_type",
    "format",
    "effort",
    "service_tier",
    "created_at",
];

/// Returns a copy of a request/response body with every content string
/// replaced by a `"[redacted: N chars]"` marker.
///
/// The structure survives — which messages, roles, tools and options were sent
/// — but prompts, completions, tool arguments, tool results, system
/// instructions, image data and user identifiers do not. Numbers, booleans and
/// the short structural strings (`model`, `role`, `type`, tool names, ids,
/// finish reasons) are kept.
///
/// ```
/// use ferroly::codec::json;
/// use ferroly::genai::redact_payload;
///
/// let body = json::from_str(
///     r#"{"model":"m","messages":[{"role":"user","content":"my secret plan"}]}"#,
/// ).unwrap();
/// let safe = json::to_string(&redact_payload(&body));
/// assert!(!safe.contains("secret"));
/// assert!(safe.contains(r#""role":"user""#) && safe.contains("[redacted: 14 chars]"));
/// ```
pub fn redact_payload(value: &Value) -> Value {
    redact(value, false)
}

fn redact(value: &Value, structural: bool) -> Value {
    match value {
        Value::Str(s) if !structural => {
            Value::Str(format!("[redacted: {} chars]", s.chars().count()))
        }
        Value::Bytes(b) => Value::Str(format!("[redacted: {} bytes]", b.len())),
        Value::Array(items) => Value::Array(items.iter().map(|v| redact(v, false)).collect()),
        Value::Object(entries) => Value::Object(
            entries
                .iter()
                .map(|(k, v)| {
                    let keep = STRUCTURAL_KEYS.contains(&k.as_str()) && matches!(v, Value::Str(_));
                    (k.clone(), redact(v, keep))
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

// ---- obs adapter -----------------------------------------------------------

/// Records every provider call as a client span (`chat {model}`, with the
/// `gen_ai.*` semantic-convention fields from
/// [`ferroly::obs::genai`](crate::obs::genai)) and propagates the span's W3C
/// trace context on the outgoing request.
///
/// Prompt and completion text are never recorded. Requires the `obs` feature.
#[cfg(feature = "obs")]
#[derive(Default)]
pub struct ObsObserver {
    spans: std::sync::Mutex<std::collections::HashMap<u64, ferroly::obs::ActiveSpan>>,
}

#[cfg(feature = "obs")]
impl ObsObserver {
    /// A new adapter.
    pub fn new() -> Self {
        ObsObserver::default()
    }
}

#[cfg(feature = "obs")]
impl std::fmt::Debug for ObsObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObsObserver").finish_non_exhaustive()
    }
}

#[cfg(feature = "obs")]
impl GenAiObserver for ObsObserver {
    fn on_request_start(&self, info: &RequestInfo) {
        let span = match info.operation {
            Operation::Embed => ferroly::obs::Span::new(format!("embeddings {}", info.model))
                .kind(ferroly::obs::SpanKind::Client)
                .field("gen_ai.operation.name", "embeddings")
                .field("gen_ai.provider.name", info.provider.as_str())
                .field("gen_ai.request.model", info.model.as_str()),
            _ => ferroly::obs::genai::chat_span(&info.provider, &info.model),
        };
        let span = span.start();
        if let Ok(mut spans) = self.spans.lock() {
            spans.insert(info.id, span);
        }
    }

    fn request_headers(&self, info: &RequestInfo) -> Vec<(String, String)> {
        let mut headers = Vec::new();
        if let Ok(spans) = self.spans.lock() {
            if let Some(ctx) = spans.get(&info.id).and_then(|s| s.context()) {
                ferroly::obs::inject(&ctx, &mut |name, value| {
                    headers.push((name.to_string(), value.to_string()));
                });
            }
        }
        headers
    }

    fn on_request_end(&self, info: &RequestInfo, outcome: &RequestOutcome) {
        let span = match self.spans.lock() {
            Ok(mut spans) => spans.remove(&info.id),
            Err(_) => None,
        };
        let Some(span) = span else { return };
        if let Some(usage) = &outcome.usage {
            ferroly::obs::genai::record_usage(
                &span,
                u64::from(usage.prompt_tokens.unwrap_or(0)),
                u64::from(usage.completion_tokens.unwrap_or(0)),
            );
            if let Some(cached) = usage.cached_tokens {
                span.record("gen_ai.usage.cache_read.input_tokens", u64::from(cached));
            }
        }
        if let Some(model) = &outcome.response_model {
            span.record("gen_ai.response.model", model.as_str());
        }
        if let Some(reason) = outcome.finish_reason {
            span.record("gen_ai.response.finish_reason", reason.as_str());
        }
        if let Some(cost) = outcome.cost_usd {
            span.record("gen_ai.usage.cost_usd", cost);
        }
        if let Some(status) = outcome.status {
            span.record("http.response.status_code", u64::from(status));
        }
        if outcome.cancelled {
            span.record("gen_ai.cancelled", true);
        }
        if let Some(kind) = outcome.error_kind {
            span.record("error.type", kind);
            span.set_error(kind);
        }
        span.end();
    }

    fn on_route(&self, report: &RouteReport) {
        let mut span = ferroly::obs::Span::new("genai.route")
            .field("genai.route.strategy", report.decision.strategy.as_str())
            .field(
                "genai.route.chosen",
                format!(
                    "{}/{}",
                    report.decision.chosen.provider, report.decision.chosen.model
                ),
            )
            .field("genai.route.attempts", report.attempts.len() as u64)
            .field("genai.route.estimated_cost_usd", report.estimated_cost_usd);
        if let Some(served) = &report.served_by {
            span = span.field(
                "genai.route.served_by",
                format!("{}/{}", served.provider, served.model),
            );
        }
        if let Some(cost) = report.actual_cost_usd {
            span = span.field("genai.route.actual_cost_usd", cost);
        }
        let span = span.start();
        if let Some(kind) = report.error_kind {
            span.set_error(kind);
        }
        span.end();
    }
}

// ---- metrics adapter -------------------------------------------------------

/// Latency buckets suited to model calls, in seconds.
#[cfg(feature = "metrics")]
const LATENCY_BUCKETS: &[f64] = &[
    0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0,
];

/// Records provider calls and routing decisions in a
/// [`metrics::Registry`](crate::metrics::Registry). Requires the `metrics`
/// feature.
///
/// | Series | Type | Labels |
/// |---|---|---|
/// | `genai_requests_total` | counter | `provider`, `model`, `operation`, `outcome` (`ok`, `cancelled`, or the error kind) |
/// | `genai_request_duration_seconds` | histogram | `provider`, `model`, `operation` |
/// | `genai_time_to_first_chunk_seconds` | histogram | `provider`, `model` |
/// | `genai_tokens_total` | counter | `provider`, `model`, `kind` (`prompt`, `completion`, `cached`, `reasoning`) |
/// | `genai_cost_usd_total` | float counter | `provider`, `model` |
/// | `genai_route_decisions_total` | counter | `strategy`, `provider`, `model`, `outcome` |
/// | `genai_route_fallbacks_total` | counter | `provider`, `model` (the model that was abandoned) |
///
/// The `model` label is bounded by the registry's cardinality guard.
#[cfg(feature = "metrics")]
#[derive(Debug, Clone)]
pub struct MetricsObserver {
    requests: ferroly::metrics::CounterVec,
    duration: ferroly::metrics::HistogramVec,
    first_chunk: ferroly::metrics::HistogramVec,
    tokens: ferroly::metrics::CounterVec,
    cost: ferroly::metrics::FloatCounterVec,
    routes: ferroly::metrics::CounterVec,
    fallbacks: ferroly::metrics::CounterVec,
}

#[cfg(feature = "metrics")]
impl MetricsObserver {
    /// Registers the series in `registry`.
    pub fn new(registry: &ferroly::metrics::Registry) -> Self {
        MetricsObserver {
            requests: registry.counter_vec(
                "genai_requests_total",
                "GenAI provider calls",
                &["provider", "model", "operation", "outcome"],
            ),
            duration: registry.histogram_vec(
                "genai_request_duration_seconds",
                "GenAI provider call duration",
                &["provider", "model", "operation"],
                LATENCY_BUCKETS,
            ),
            first_chunk: registry.histogram_vec(
                "genai_time_to_first_chunk_seconds",
                "Time until the first chunk of a streamed completion",
                &["provider", "model"],
                LATENCY_BUCKETS,
            ),
            tokens: registry.counter_vec(
                "genai_tokens_total",
                "Tokens consumed by GenAI provider calls",
                &["provider", "model", "kind"],
            ),
            cost: registry.float_counter_vec(
                "genai_cost_usd_total",
                "Cost of GenAI provider calls in USD, from catalog prices",
                &["provider", "model"],
            ),
            routes: registry.counter_vec(
                "genai_route_decisions_total",
                "Model-router decisions",
                &["strategy", "provider", "model", "outcome"],
            ),
            fallbacks: registry.counter_vec(
                "genai_route_fallbacks_total",
                "Model-router attempts that failed over to another model",
                &["provider", "model"],
            ),
        }
    }

    /// Registers the series in the process-global registry.
    pub fn global() -> Self {
        MetricsObserver::new(ferroly::metrics::global())
    }
}

#[cfg(feature = "metrics")]
impl GenAiObserver for MetricsObserver {
    fn on_request_end(&self, info: &RequestInfo, outcome: &RequestOutcome) {
        let (p, m, op) = (
            info.provider.as_str(),
            info.model.as_str(),
            info.operation.as_str(),
        );
        self.requests
            .with_labels(&[p, m, op, outcome.outcome_label()])
            .inc();
        self.duration
            .with_labels(&[p, m, op])
            .observe(outcome.latency.as_secs_f64());
        if let Some(first) = outcome.time_to_first_chunk {
            self.first_chunk
                .with_labels(&[p, m])
                .observe(first.as_secs_f64());
        }
        if let Some(usage) = &outcome.usage {
            for (kind, count) in [
                ("prompt", usage.prompt_tokens),
                ("completion", usage.completion_tokens),
                ("cached", usage.cached_tokens),
                ("reasoning", usage.reasoning_tokens),
            ] {
                if let Some(n) = count {
                    self.tokens.with_labels(&[p, m, kind]).inc_by(u64::from(n));
                }
            }
        }
        if let Some(cost) = outcome.cost_usd {
            self.cost.with_labels(&[p, m]).inc_by(cost);
        }
    }

    fn on_route(&self, report: &RouteReport) {
        use ferroly::genai::router::AttemptOutcome;
        let target = report.served_by.as_ref().unwrap_or(&report.decision.chosen);
        self.routes
            .with_labels(&[
                report.decision.strategy.as_str(),
                target.provider.as_str(),
                target.model.as_str(),
                report.error_kind.unwrap_or("ok"),
            ])
            .inc();
        for attempt in &report.attempts {
            if attempt.outcome == AttemptOutcome::FailedOver {
                self.fallbacks
                    .with_labels(&[attempt.provider.as_str(), attempt.model.as_str()])
                    .inc();
            }
        }
    }
}

// ---- log adapter -----------------------------------------------------------

/// What [`LogObserver`] does with request and response bodies.
#[cfg(feature = "log")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PayloadLogging {
    /// Bodies are not logged (the default).
    #[default]
    Off,
    /// Bodies are logged after [`redact_payload`]: structure only, no prompt,
    /// completion or tool content.
    Redacted,
    /// Bodies are logged verbatim. Prompts and completions routinely contain
    /// personal and confidential data: use only where the log is allowed to
    /// hold it.
    Full,
}

/// Writes one structured `ferroly::log` record per provider call (at debug
/// level; failures at warn) and per routing decision. Requires the `log`
/// feature.
///
/// Request and response bodies are **not** logged unless
/// [`payloads`](LogObserver::payloads) turns that on. Headers — and with them
/// API keys — are never available to an observer.
#[cfg(feature = "log")]
#[derive(Debug, Clone, Default)]
pub struct LogObserver {
    payloads: PayloadLogging,
}

#[cfg(feature = "log")]
impl LogObserver {
    /// A logger that records call metadata only.
    pub fn new() -> Self {
        LogObserver::default()
    }

    /// Chooses whether (and how) request/response bodies are logged.
    pub fn payloads(mut self, mode: PayloadLogging) -> Self {
        self.payloads = mode;
        self
    }
}

#[cfg(feature = "log")]
impl GenAiObserver for LogObserver {
    fn on_request_end(&self, info: &RequestInfo, outcome: &RequestOutcome) {
        let mut fields: Vec<(&str, Value)> = vec![
            ("provider", Value::from(info.provider.clone())),
            ("model", Value::from(info.model.clone())),
            ("operation", Value::from(info.operation.as_str())),
            ("request_id", Value::from(info.id)),
            ("outcome", Value::from(outcome.outcome_label())),
            (
                "latency_ms",
                Value::from(outcome.latency.as_millis().min(u128::from(u64::MAX)) as u64),
            ),
        ];
        if let Some(usage) = &outcome.usage {
            if let Some(n) = usage.prompt_tokens {
                fields.push(("prompt_tokens", Value::from(n)));
            }
            if let Some(n) = usage.completion_tokens {
                fields.push(("completion_tokens", Value::from(n)));
            }
            if let Some(n) = usage.cached_tokens {
                fields.push(("cached_tokens", Value::from(n)));
            }
        }
        if let Some(cost) = outcome.cost_usd {
            fields.push(("cost_usd", Value::from(cost)));
        }
        if let Some(reason) = outcome.finish_reason {
            fields.push(("finish_reason", Value::from(reason.as_str())));
        }
        if let Some(status) = outcome.status {
            fields.push(("status", Value::from(status)));
        }
        if outcome.error_kind.is_some() {
            ferroly::log::warn("genai call failed", &fields);
        } else {
            ferroly::log::debug("genai call", &fields);
        }
    }

    fn wants_payloads(&self) -> bool {
        self.payloads != PayloadLogging::Off
    }

    fn on_payload(&self, info: &RequestInfo, direction: PayloadDirection, body: &Value) {
        let body = match self.payloads {
            PayloadLogging::Off => return,
            PayloadLogging::Redacted => redact_payload(body),
            PayloadLogging::Full => body.clone(),
        };
        let direction = match direction {
            PayloadDirection::Request => "request",
            PayloadDirection::Response => "response",
        };
        ferroly::log::debug(
            "genai payload",
            &[
                ("provider", Value::from(info.provider.clone())),
                ("model", Value::from(info.model.clone())),
                ("request_id", Value::from(info.id)),
                ("direction", Value::from(direction)),
                ("body", body),
            ],
        );
    }

    fn on_route(&self, report: &RouteReport) {
        log_route(report, true);
    }
}

/// Writes a routing decision to the structured log: at debug level, or at
/// warn level for a failed route when `warn_on_failure` is set.
#[cfg(feature = "log")]
pub(crate) fn log_route(report: &RouteReport, warn_on_failure: bool) {
    let d = &report.decision;
    let mut fields: Vec<(&str, Value)> = vec![
        ("strategy", Value::from(d.strategy.clone())),
        ("provider", Value::from(d.chosen.provider.clone())),
        ("model", Value::from(d.chosen.model.clone())),
        ("est_cost_usd", Value::from(report.estimated_cost_usd)),
        ("fallbacks", Value::from(d.fallbacks.len() as u64)),
        ("considered", Value::from(d.considered.len() as u64)),
        ("excluded", Value::from(d.excluded.len() as u64)),
        ("pinned", Value::from(d.pinned)),
        ("attempts", Value::from(report.attempts.len() as u64)),
    ];
    if let Some(served) = &report.served_by {
        fields.push((
            "served_by",
            Value::from(format!("{}/{}", served.provider, served.model)),
        ));
    }
    if let Some(cost) = report.actual_cost_usd {
        fields.push(("actual_cost_usd", Value::from(cost)));
    }
    if let Some(kind) = report.error_kind {
        fields.push(("error", Value::from(kind)));
    }
    if report.error_kind.is_some() && warn_on_failure {
        ferroly::log::warn("genai model-router decision", &fields);
    } else {
        ferroly::log::debug("genai model-router decision", &fields);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroly::codec::json;

    #[test]
    fn redaction_keeps_structure_only() {
        let body = json::from_str(
            r#"{"model":"gpt-x","stream":true,"temperature":0.5,
                "messages":[{"role":"system","content":"top secret system"},
                            {"role":"user","content":[{"type":"text","text":"my password is hunter2"}]},
                            {"role":"tool","tool_call_id":"call_1","content":"{\"ssn\":\"123\"}"}],
                "tools":[{"type":"function","function":{"name":"lookup","description":"find people"}}],
                "user":"alice@example.com"}"#,
        )
        .unwrap();
        let out = json::to_string(&redact_payload(&body));
        for leaked in ["secret", "hunter2", "ssn", "alice", "find people"] {
            assert!(!out.contains(leaked), "{leaked} leaked into {out}");
        }
        for kept in [
            r#""model":"gpt-x""#,
            r#""role":"tool""#,
            r#""tool_call_id":"call_1""#,
            r#""name":"lookup""#,
            r#""stream":true"#,
            r#""temperature":0.5"#,
        ] {
            assert!(out.contains(kept), "{kept} missing from {out}");
        }
    }

    #[test]
    fn request_ids_are_unique() {
        let a = RequestInfo::new("p", "m", Operation::Chat);
        let b = RequestInfo::new("p", "m", Operation::Chat);
        assert_ne!(a.id, b.id);
    }
}

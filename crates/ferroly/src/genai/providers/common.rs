//! Plumbing shared by the HTTP providers: client construction, request
//! sending with auth / retry / observation, error mapping, and the small JSON
//! helpers the wire encoders use.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ferroly::clients::{authorize, parse_retry_after, AuthError, AuthProvider, Retry, Secret};
use ferroly::codec::{json, Value};
use ferroly::genai::observe::{
    GenAiObserver, Operation, PayloadDirection, RequestInfo, RequestOutcome,
};
use ferroly::genai::provider::ChunkStream;
use ferroly::genai::{
    CompletionChunk, CompletionResponse, FinishReason, GenAiError, ModelInfo, RateLimitInfo,
    TimeoutPhase, Usage,
};
use ferroly::http::{Client, HeaderMap, HttpError, Method, Request, Response};

use super::sse::{pump, Framing, StreamParser};
use super::{ProviderOptions, Timeouts};

/// How much of an error response is read off the wire before giving up on it.
const ERROR_BODY_READ_LIMIT: u64 = 64 * 1024;

/// Maps a transport error. `phase` names the timeout a plain
/// [`HttpError::Timeout`] stands for at the call site.
pub(crate) fn map_http_error(err: HttpError, phase: TimeoutPhase) -> GenAiError {
    match err {
        HttpError::ConnectTimeout => GenAiError::Timeout {
            phase: TimeoutPhase::Connect,
        },
        HttpError::Timeout => GenAiError::Timeout { phase },
        HttpError::InvalidUrl(e) => GenAiError::Config(format!("invalid url: {e}")),
        other => GenAiError::Transport(other.to_string()),
    }
}

fn map_auth_error(err: AuthError) -> GenAiError {
    match err {
        AuthError::Transport(e) => GenAiError::Transport(format!("credential refresh failed: {e}")),
        AuthError::Rejected { status, .. } => GenAiError::Auth {
            status,
            message: "the token endpoint rejected the credential request".into(),
        },
        other => GenAiError::Config(format!("auth provider: {other}")),
    }
}

/// What a provider's error body says about the failure, beyond the status.
/// Each provider recognises a subset, so with a single provider enabled some
/// variants are never constructed.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorHint {
    /// Nothing more specific than the HTTP status.
    None,
    RateLimited,
    Auth,
    ContentFilter,
    ContextLength,
    Overloaded,
}

/// The facts extracted from a failed response, before they become a
/// [`GenAiError`].
pub(crate) struct ProviderFailure {
    pub(crate) status: u16,
    pub(crate) hint: ErrorHint,
    /// The provider's error type / code, if it sent one.
    pub(crate) error_type: Option<String>,
    pub(crate) message: String,
}

/// A provider's parser for its error-body shape.
pub(crate) type ErrorParser = fn(status: u16, body: &str) -> ProviderFailure;

/// Builds the most specific error for a failed response.
pub(crate) fn failure_to_error(
    failure: ProviderFailure,
    retry_after: Option<Duration>,
    limits: RateLimitInfo,
) -> GenAiError {
    let ProviderFailure {
        status,
        hint,
        error_type,
        message,
    } = failure;
    let message = match error_type {
        Some(t) if !t.is_empty() && !message.starts_with(&t) => format!("{t}: {message}"),
        _ => message,
    };
    match hint {
        ErrorHint::RateLimited => GenAiError::RateLimited {
            message,
            retry_after,
            limits,
        },
        ErrorHint::Auth => GenAiError::Auth { status, message },
        ErrorHint::ContentFilter => GenAiError::ContentFilter { message },
        ErrorHint::ContextLength => GenAiError::ContextLengthExceeded { message },
        ErrorHint::Overloaded => GenAiError::Overloaded {
            status,
            message,
            retry_after,
        },
        ErrorHint::None => match status {
            401 | 403 => GenAiError::Auth { status, message },
            429 => GenAiError::RateLimited {
                message,
                retry_after,
                limits,
            },
            503 | 529 => GenAiError::Overloaded {
                status,
                message,
                retry_after,
            },
            _ => GenAiError::Api { status, message },
        },
    }
}

/// Truncates `text` to at most `max` bytes on a character boundary.
pub(crate) fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [truncated]", &text[..end])
}

fn rate_limit_info(headers: &HeaderMap) -> RateLimitInfo {
    RateLimitInfo::new(
        headers
            .iter()
            .filter(|(name, _)| name.to_ascii_lowercase().contains("ratelimit"))
            .map(|(name, value)| (name.to_string(), value.to_string())),
    )
}

fn retry_after_of(headers: &HeaderMap) -> Option<Duration> {
    if let Some(ms) = headers
        .get("retry-after-ms")
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        return Some(Duration::from_millis(ms));
    }
    headers.get("retry-after").and_then(parse_retry_after)
}

/// The shared HTTP machinery of one provider instance.
pub(crate) struct HttpCore {
    name: String,
    /// `Err` when the HTTP client could not be built (invalid TLS options);
    /// reported as a configuration error on every call.
    client: Result<Client, String>,
    auth: Option<Arc<dyn AuthProvider>>,
    headers: Vec<(String, String)>,
    pub(crate) timeouts: Timeouts,
    retry: Option<Retry>,
    observer: Option<Arc<dyn GenAiObserver>>,
    max_error_body: usize,
    max_stream_bytes: Option<u64>,
    #[cfg(any(feature = "openai", feature = "ollama"))]
    pub(crate) embed_batch_size: Option<usize>,
    /// The built-in API key, scrubbed from any provider text that echoes it.
    secret: Option<Secret>,
}

impl HttpCore {
    /// Builds the core. `default_auth` (and the `secret` behind it) is used
    /// unless the options carry their own auth provider.
    pub(crate) fn new(
        name: &str,
        opts: &ProviderOptions,
        default_auth: Option<Arc<dyn AuthProvider>>,
        secret: Option<Secret>,
    ) -> Self {
        let t = opts.timeouts;
        let client = match &opts.http_client {
            Some(client) => Ok(client.clone()),
            None => {
                let mut builder = Client::builder()
                    .connect_timeout(t.connect)
                    .timeout(t.response)
                    .read_timeout(t.stream_idle)
                    .total_timeout(t.total)
                    .max_response_body_bytes(opts.max_response_bytes);
                if let Some(proxy) = &opts.proxy {
                    builder = builder.proxy(proxy.clone());
                }
                if let Some(tls) = &opts.tls {
                    builder = builder.tls(tls.clone());
                }
                builder.build().map_err(|e| e.to_string())
            }
        };
        let (auth, secret) = match &opts.auth {
            Some(custom) => (Some(custom.clone()), None),
            None => (default_auth, secret),
        };
        HttpCore {
            name: name.to_string(),
            client,
            auth,
            headers: opts.headers.clone(),
            timeouts: t,
            retry: opts.retry.clone(),
            observer: opts.observer.clone(),
            max_error_body: opts.max_error_body_bytes,
            max_stream_bytes: opts.max_stream_bytes,
            #[cfg(any(feature = "openai", feature = "ollama"))]
            embed_batch_size: opts.embed_batch_size,
            secret,
        }
    }

    #[cfg(feature = "openai")]
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Removes the built-in API key from provider-supplied text and caps its
    /// size.
    fn sanitize(&self, text: &str) -> String {
        let scrubbed = match self.secret.as_ref().and_then(Secret::expose_str) {
            Some(key) if key.len() >= 8 && text.contains(key) => text.replace(key, "[REDACTED]"),
            _ => text.to_string(),
        };
        truncate(&scrubbed, self.max_error_body)
    }

    /// Starts an observed call. `price` is only evaluated when an observer is
    /// installed.
    pub(crate) fn begin(
        &self,
        model: &str,
        operation: Operation,
        price: impl FnOnce() -> Option<ModelInfo>,
    ) -> CallGuard {
        let info = RequestInfo::new(self.name.clone(), model, operation);
        let price = match &self.observer {
            Some(observer) => {
                observer.on_request_start(&info);
                price()
            }
            None => None,
        };
        CallGuard {
            observer: self.observer.clone(),
            info,
            started: Instant::now(),
            first_chunk: None,
            usage: None,
            finish: None,
            model: None,
            price,
            done: false,
        }
    }

    /// POSTs a JSON body and returns the successful response, retrying under
    /// the configured policy. A non-2xx answer becomes the matching error.
    pub(crate) async fn post(
        &self,
        url: &str,
        body: &Value,
        provider_headers: &[(&str, String)],
        call: &CallGuard,
        parse_error: ErrorParser,
    ) -> Result<Response, GenAiError> {
        call.payload(PayloadDirection::Request, body);
        let payload = json::to_string(body).into_bytes();
        match &self.retry {
            None => {
                self.post_once(url, &payload, provider_headers, call, parse_error)
                    .await
            }
            Some(retry) => {
                retry
                    .run_with(
                        |e: &GenAiError, _attempt| e.error_class().retry,
                        || self.post_once(url, &payload, provider_headers, call, parse_error),
                    )
                    .await
            }
        }
    }

    async fn post_once(
        &self,
        url: &str,
        payload: &[u8],
        provider_headers: &[(&str, String)],
        call: &CallGuard,
        parse_error: ErrorParser,
    ) -> Result<Response, GenAiError> {
        let client = self
            .client
            .as_ref()
            .map_err(|e| GenAiError::Config(format!("http client: {e}")))?;
        let mut req = Request::builder(Method::Post, url)
            .map_err(|e| GenAiError::Config(e.to_string()))?
            .header("content-type", "application/json")
            .body(payload.to_vec())
            .build();
        for (name, value) in &self.headers {
            req.headers
                .try_set(name.clone(), value.clone())
                .map_err(|e| GenAiError::Config(format!("invalid header '{name}': {e}")))?;
        }
        // Propagate the caller's trace context; an observer that started its
        // own span for this call overrides it just below.
        #[cfg(feature = "obs")]
        {
            ferroly::obs::inject_current(&mut |name, value| req.headers.set(name, value));
        }
        if let Some(observer) = &self.observer {
            for (name, value) in observer.request_headers(call.info()) {
                req.headers.set(name, value);
            }
        }
        for (name, value) in provider_headers {
            req.headers.set(*name, value.clone());
        }
        if let Some(auth) = &self.auth {
            authorize(auth.as_ref(), &mut req)
                .await
                .map_err(map_auth_error)?;
        }
        let resp = client
            .send(req)
            .await
            .map_err(|e| map_http_error(e, TimeoutPhase::Response))?;
        if resp.status().is_success() {
            return Ok(resp);
        }
        Err(self.error_from_response(resp, parse_error).await)
    }

    /// Reads (a bounded part of) a failed response and classifies it.
    async fn error_from_response(
        &self,
        mut resp: Response,
        parse_error: ErrorParser,
    ) -> GenAiError {
        let status = resp.status().as_u16();
        let retry_after = retry_after_of(resp.headers());
        let limits = rate_limit_info(resp.headers());
        resp.set_max_body_bytes(Some(ERROR_BODY_READ_LIMIT));
        let mut body = Vec::new();
        while let Ok(Some(chunk)) = resp.chunk().await {
            body.extend_from_slice(&chunk);
        }
        let text = String::from_utf8_lossy(&body);
        let mut failure = parse_error(status, text.trim());
        failure.message = self.sanitize(&failure.message);
        failure_to_error(failure, retry_after, limits)
    }

    /// Reads a successful response body as JSON.
    pub(crate) async fn read_json(
        &self,
        resp: Response,
        call: &CallGuard,
    ) -> Result<Value, GenAiError> {
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| map_http_error(e, TimeoutPhase::Response))?;
        let value = json::from_slice(&bytes)
            .map_err(|e| GenAiError::ResponseParse(format!("response body is not JSON: {e}")))?;
        call.payload(PayloadDirection::Response, &value);
        Ok(value)
    }

    /// Hands a streaming response to a background reader and returns the
    /// receiving end.
    pub(crate) fn spawn_stream<P: StreamParser>(
        &self,
        mut resp: Response,
        parser: P,
        framing: Framing,
        call: CallGuard,
    ) -> ChunkStream {
        // Deliberate: the client's body cap is meant for buffered responses.
        // A stream is bounded by the idle timeout and the per-line limit, plus
        // `max_stream_bytes` when the caller sets one.
        resp.set_max_body_bytes(self.max_stream_bytes);
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(pump(
            resp,
            tx,
            parser,
            framing,
            self.timeouts.stream_idle,
            call,
        ));
        rx
    }
}

/// Tracks one provider call for the observer and guarantees that
/// `on_request_end` fires exactly once — as `cancelled` if the call is dropped
/// before it finishes.
pub(crate) struct CallGuard {
    observer: Option<Arc<dyn GenAiObserver>>,
    info: RequestInfo,
    started: Instant,
    first_chunk: Option<Duration>,
    usage: Option<Usage>,
    finish: Option<FinishReason>,
    model: Option<String>,
    price: Option<ModelInfo>,
    done: bool,
}

impl CallGuard {
    pub(crate) fn info(&self) -> &RequestInfo {
        &self.info
    }

    fn payload(&self, direction: PayloadDirection, body: &Value) {
        if let Some(observer) = &self.observer {
            if observer.wants_payloads() {
                observer.on_payload(&self.info, direction, body);
            }
        }
    }

    /// Notes what a stream chunk says about the call.
    pub(crate) fn observe_chunk(&mut self, chunk: &CompletionChunk) {
        if self.observer.is_none() {
            return;
        }
        if self.first_chunk.is_none() {
            self.first_chunk = Some(self.started.elapsed());
        }
        match chunk {
            CompletionChunk::Usage(usage) => match &mut self.usage {
                Some(total) => total.merge(usage),
                None => self.usage = Some(usage.clone()),
            },
            CompletionChunk::Done { finish_reason, .. } => self.finish = Some(*finish_reason),
            CompletionChunk::Start { model, .. } if model.is_some() => {
                self.model.clone_from(model);
            }
            _ => {}
        }
    }

    fn finish(&mut self, error: Option<&GenAiError>, cancelled: bool) {
        if std::mem::replace(&mut self.done, true) {
            return;
        }
        let Some(observer) = &self.observer else {
            return;
        };
        let mut usage = self.usage.take();
        if let Some(u) = &mut usage {
            u.fill_total();
        }
        let cost_usd = match (&usage, &self.price) {
            (Some(u), Some(price)) => price.cost_for(u),
            _ => None,
        };
        let outcome = RequestOutcome {
            latency: self.started.elapsed(),
            time_to_first_chunk: self.first_chunk,
            usage,
            cost_usd,
            finish_reason: self.finish,
            response_model: self.model.take(),
            error_kind: error.map(GenAiError::kind_label),
            status: error.and_then(GenAiError::status),
            cancelled,
        };
        observer.on_request_end(&self.info, &outcome);
    }

    pub(crate) fn end_err(&mut self, err: &GenAiError) {
        self.finish(Some(err), false);
    }

    pub(crate) fn end_stream_ok(&mut self) {
        self.finish(None, false);
    }

    pub(crate) fn end_response(&mut self, result: &Result<CompletionResponse, GenAiError>) {
        match result {
            Ok(response) => {
                self.usage.clone_from(&response.usage);
                self.finish = response.finish_reason;
                self.model = Some(response.model.clone());
                self.finish(None, false);
            }
            Err(e) => self.finish(Some(e), false),
        }
    }

    #[cfg(any(feature = "openai", feature = "ollama"))]
    pub(crate) fn end_embed(&mut self, result: &Result<ferroly::genai::EmbedResponse, GenAiError>) {
        match result {
            Ok(response) => {
                self.usage.clone_from(&response.usage);
                self.model = Some(response.model.clone());
                self.finish(None, false);
            }
            Err(e) => self.finish(Some(e), false),
        }
    }
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        self.finish(None, true);
    }
}

// ---- JSON helpers ----------------------------------------------------------

/// Builds a JSON object from `(key, value)` pairs, in order.
pub(crate) fn obj<const N: usize>(pairs: [(&str, Value); N]) -> Value {
    Value::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// A JSON string value.
pub(crate) fn s(text: &str) -> Value {
    Value::Str(text.to_string())
}

/// An `f32` option as a JSON number that keeps its decimal value (`0.7`, not
/// the `0.699999988079071` a plain widening to `f64` would print).
pub(crate) fn f32_num(value: f32) -> Value {
    let widened = value.to_string().parse::<f64>().unwrap_or(f64::from(value));
    Value::Float(widened)
}

/// How much of a provider's in-stream error text is kept.
const STREAM_ERROR_MAX: usize = 2048;

/// The error for a failure the provider reported inside an established
/// stream.
pub(crate) fn stream_failure(mut failure: ProviderFailure) -> GenAiError {
    failure.message = truncate(&failure.message, STREAM_ERROR_MAX);
    failure_to_error(failure, None, RateLimitInfo::default())
}

/// Reads a non-negative integer field as `u32` (saturating).
pub(crate) fn u32_field(value: &Value, key: &str) -> Option<u32> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .map(|v| v.min(u64::from(u32::MAX)) as u32)
}

/// Coerces a JSON number (float or int) to `f32`.
#[cfg(any(feature = "openai", feature = "ollama"))]
pub(crate) fn num_f32(v: &Value) -> Option<f32> {
    v.as_f64().map(|f| f as f32)
}

/// The text a tool result is sent as: a string as is, anything else as JSON.
pub(crate) fn tool_result_text(result: &Value) -> String {
    match result {
        Value::Str(text) => text.clone(),
        other => json::to_string(other),
    }
}

/// Merges `Options::custom` into a request body under the documented rules:
/// provider-owned keys are rejected, a typed field wins over a custom key of
/// the same name, and two objects are merged one level deep (typed entries
/// win). Keys are applied in sorted order so the body is deterministic.
pub(crate) fn merge_custom(
    fields: &mut Vec<(String, Value)>,
    custom: &HashMap<String, Value>,
    owned: &[&str],
    provider: &str,
) -> Result<(), GenAiError> {
    let mut keys: Vec<&String> = custom.keys().collect();
    keys.sort();
    for key in keys {
        if owned.contains(&key.as_str()) {
            return Err(GenAiError::Config(format!(
                "custom option '{key}' cannot override a field the {provider} provider owns"
            )));
        }
        let value = &custom[key];
        match fields.iter_mut().find(|(k, _)| k == key) {
            None => fields.push((key.clone(), value.clone())),
            Some((_, Value::Object(typed))) => {
                if let Value::Object(extra) = value {
                    for (k, v) in extra {
                        if !typed.iter().any(|(existing, _)| existing == k) {
                            typed.push((k.clone(), v.clone()));
                        }
                    }
                }
            }
            Some(_) => {} // the typed option wins
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_merge_rules() {
        let mut fields = vec![
            ("model".to_string(), s("m")),
            ("temperature".to_string(), Value::Float(0.5)),
            ("options".to_string(), obj([("seed", Value::Int(1))])),
        ];
        let mut custom = HashMap::new();
        custom.insert("temperature".to_string(), Value::Float(0.9));
        custom.insert("logprobs".to_string(), Value::Bool(true));
        custom.insert(
            "options".to_string(),
            obj([("seed", Value::Int(2)), ("mirostat", Value::Int(1))]),
        );
        merge_custom(&mut fields, &custom, &["model"], "test").unwrap();
        let body = Value::Object(fields);
        assert_eq!(body.get("temperature").unwrap().as_f64(), Some(0.5));
        assert_eq!(body.get("logprobs").unwrap().as_bool(), Some(true));
        let options = body.get("options").unwrap();
        assert_eq!(options.get("seed").unwrap().as_i64(), Some(1));
        assert_eq!(options.get("mirostat").unwrap().as_i64(), Some(1));

        let mut custom = HashMap::new();
        custom.insert("model".to_string(), s("other"));
        let mut fields = vec![("model".to_string(), s("m"))];
        let err = merge_custom(&mut fields, &custom, &["model"], "test").unwrap_err();
        assert!(matches!(err, GenAiError::Config(_)));
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        assert_eq!(truncate("short", 10), "short");
        let t = truncate("h\u{e9}llo world", 2);
        assert!(t.starts_with('h') && t.ends_with("[truncated]"));
    }

    #[test]
    fn status_fallback_classification() {
        let mk = |status| ProviderFailure {
            status,
            hint: ErrorHint::None,
            error_type: None,
            message: "m".into(),
        };
        let classify = |status| failure_to_error(mk(status), None, RateLimitInfo::default());
        assert!(matches!(
            classify(401),
            GenAiError::Auth { status: 401, .. }
        ));
        assert!(matches!(classify(429), GenAiError::RateLimited { .. }));
        assert!(matches!(
            classify(529),
            GenAiError::Overloaded { status: 529, .. }
        ));
        assert!(matches!(classify(400), GenAiError::Api { status: 400, .. }));
        assert!(matches!(classify(500), GenAiError::Api { status: 500, .. }));
    }
}

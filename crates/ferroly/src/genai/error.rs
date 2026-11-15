//! Error type for the GenAI module.

use std::fmt;
use std::time::Duration;

use ferroly::clients::{ErrorClass, Outcome, RetryDecision};
use ferroly_derive::FerrolyError;

/// Which of a provider call's timeouts elapsed.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutPhase {
    /// Connecting (DNS, TCP, TLS, proxy tunnel). **Nothing was sent**, so the
    /// call is safe to repeat.
    Connect,
    /// Waiting for the response (or the overall deadline of the exchange).
    /// The request was sent and may have been processed — and billed.
    Response,
    /// A streamed response went silent for longer than the idle timeout.
    StreamIdle,
}

impl fmt::Display for TimeoutPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TimeoutPhase::Connect => "connect",
            TimeoutPhase::Response => "response",
            TimeoutPhase::StreamIdle => "stream idle",
        })
    }
}

/// The rate-limit headers a provider sent with a response, verbatim.
///
/// Header names differ per provider (`x-ratelimit-remaining-requests`,
/// `anthropic-ratelimit-requests-remaining`, …), so they are kept as
/// lower-cased `(name, value)` pairs instead of being forced into one shape.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RateLimitInfo {
    headers: Vec<(String, String)>,
}

impl RateLimitInfo {
    /// Builds the info from `(name, value)` pairs; names are lower-cased.
    pub fn new<I, K, V>(headers: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        RateLimitInfo {
            headers: headers
                .into_iter()
                .map(|(k, v)| (k.into().to_ascii_lowercase(), v.into()))
                .collect(),
        }
    }

    /// The value of the header `name` (case-insensitive), if it was sent.
    pub fn get(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Every captured header, in the order received.
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// Whether no rate-limit header was sent.
    pub fn is_empty(&self) -> bool {
        self.headers.is_empty()
    }
}

/// Errors raised across message construction, prompt rendering, and provider I/O.
///
/// Provider HTTP failures are mapped to the most specific variant: a `429` is
/// [`RateLimited`](GenAiError::RateLimited), a `401`/`403` is
/// [`Auth`](GenAiError::Auth), an overload (`529`, `503`, or the provider's
/// overloaded error type) is [`Overloaded`](GenAiError::Overloaded), and so on;
/// everything else keeps the plain [`Api`](GenAiError::Api) shape.
/// [`status`](GenAiError::status), [`retry_after`](GenAiError::retry_after) and
/// [`error_class`](GenAiError::error_class) read the common facts without a
/// `match`.
///
/// No variant ever carries an API key or an `Authorization` value, and provider
/// error bodies are truncated before they are stored.
#[derive(Debug, FerrolyError)]
#[non_exhaustive]
pub enum GenAiError {
    /// A prompt template failed to compile or render.
    #[error("template error: {0}")]
    Template(String),

    /// The requested prompt template id was not found in the store.
    #[error("prompt template not found: {0}")]
    TemplateNotFound(String),

    /// A provider was asked for a capability it does not support.
    #[error("provider '{provider}' does not support capability: {capability:?}")]
    Unsupported {
        /// The provider name.
        provider: String,
        /// The unsupported capability.
        capability: ferroly::genai::Capability,
    },

    /// The provider's HTTP transport failed **after the connection was
    /// established** (connection reset mid-exchange, malformed or oversized
    /// response). The request may have reached the provider and been
    /// processed — and billed — so it is not retried.
    #[error("transport error: {0}")]
    Transport(String),

    /// No connection to the provider could be established (connection
    /// refused, name resolution failure, unreachable network, TLS handshake
    /// or proxy `CONNECT` failure). **Nothing was sent**, so the call is safe
    /// to repeat.
    #[error("could not connect to the provider: {0}")]
    Connect(String),

    /// The provider returned a non-success HTTP status that has no more
    /// specific variant.
    ///
    /// `#[non_exhaustive]`: build one with [`GenAiError::api`] (plus
    /// [`with_error_type`](GenAiError::with_error_type) /
    /// [`with_retry_after`](GenAiError::with_retry_after)) and match it with
    /// `GenAiError::Api { status, .. }`.
    #[error("provider returned status {status}: {message}")]
    #[non_exhaustive]
    Api {
        /// The HTTP status code.
        status: u16,
        /// The provider's error message (truncated), prefixed with the
        /// provider's error type when it sent one.
        message: String,
        /// The provider's machine-readable error type or code, when its
        /// error body carried one (`invalid_request_error`, `not_found_error`,
        /// `server_error`, …).
        error_type: Option<String>,
        /// The delay the provider asked for (`Retry-After` /
        /// `retry-after-ms`), if any.
        retry_after: Option<Duration>,
    },

    /// The provider's response could not be parsed into the normalized shape.
    #[error("failed to parse provider response: {0}")]
    ResponseParse(String),

    /// Configuration (missing API key, invalid base URL, etc.) was invalid.
    #[error("configuration error: {0}")]
    Config(String),

    /// A timeout elapsed. Distinct from [`Transport`](GenAiError::Transport)
    /// because only a [`TimeoutPhase::Connect`] timeout is safe to repeat: in
    /// every other phase the request was sent and may have been billed.
    #[error("request timed out ({phase})")]
    Timeout {
        /// Which timeout elapsed.
        phase: TimeoutPhase,
    },

    /// The provider throttled the request (HTTP `429`).
    #[error("rate limited by provider: {message}")]
    RateLimited {
        /// The provider's error message (truncated).
        message: String,
        /// The delay the provider asked for (`Retry-After`), if any.
        retry_after: Option<Duration>,
        /// The rate-limit headers sent with the response.
        limits: RateLimitInfo,
    },

    /// The credentials were missing, invalid, or not allowed (HTTP `401` /
    /// `403`) — or the account behind them has no quota or billing left
    /// (reported with the provider's status, e.g. `429` or `402`). Waiting
    /// does not help: the call is not retried.
    #[error("authentication failed (status {status}): {message}")]
    Auth {
        /// The HTTP status code.
        status: u16,
        /// The provider's error message (truncated).
        message: String,
    },

    /// The provider refused the request or blocked the output under its
    /// content policy.
    #[error("blocked by the provider's content policy: {message}")]
    ContentFilter {
        /// The provider's explanation (truncated).
        message: String,
    },

    /// The prompt (plus requested output) does not fit the model's context
    /// window.
    #[error("context length exceeded: {message}")]
    ContextLengthExceeded {
        /// The provider's error message (truncated).
        message: String,
    },

    /// The provider is temporarily overloaded.
    #[error("provider overloaded (status {status}): {message}")]
    Overloaded {
        /// The HTTP status code (`529`, `503`, …). For an overload reported
        /// inside an established stream, the status that error type
        /// corresponds to.
        status: u16,
        /// The provider's error message (truncated).
        message: String,
        /// The delay the provider asked for (`Retry-After`), if any.
        retry_after: Option<Duration>,
    },
}

impl GenAiError {
    /// An [`Api`](GenAiError::Api) error with no error type and no retry
    /// delay.
    pub fn api(status: u16, message: impl Into<String>) -> GenAiError {
        GenAiError::Api {
            status,
            message: message.into(),
            error_type: None,
            retry_after: None,
        }
    }

    /// Sets the provider's error type on an [`Api`](GenAiError::Api) error;
    /// any other variant is returned unchanged.
    pub fn with_error_type(mut self, kind: impl Into<String>) -> GenAiError {
        if let GenAiError::Api { error_type, .. } = &mut self {
            *error_type = Some(kind.into());
        }
        self
    }

    /// Sets the requested retry delay on an [`Api`](GenAiError::Api),
    /// [`RateLimited`](GenAiError::RateLimited) or
    /// [`Overloaded`](GenAiError::Overloaded) error; any other variant is
    /// returned unchanged.
    pub fn with_retry_after(mut self, delay: Duration) -> GenAiError {
        match &mut self {
            GenAiError::Api { retry_after, .. }
            | GenAiError::RateLimited { retry_after, .. }
            | GenAiError::Overloaded { retry_after, .. } => *retry_after = Some(delay),
            _ => {}
        }
        self
    }

    /// The provider's machine-readable error type, when the error came from
    /// an [`Api`](GenAiError::Api) response that carried one.
    pub fn error_type(&self) -> Option<&str> {
        match self {
            GenAiError::Api { error_type, .. } => error_type.as_deref(),
            _ => None,
        }
    }

    /// The HTTP status behind this error, when it came from an HTTP response.
    pub fn status(&self) -> Option<u16> {
        match self {
            GenAiError::Api { status, .. }
            | GenAiError::Auth { status, .. }
            | GenAiError::Overloaded { status, .. } => Some(*status),
            GenAiError::RateLimited { .. } => Some(429),
            _ => None,
        }
    }

    /// The delay the provider asked the caller to wait before retrying.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            GenAiError::RateLimited { retry_after, .. }
            | GenAiError::Overloaded { retry_after, .. }
            | GenAiError::Api { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// Whether this is a [`Timeout`](GenAiError::Timeout) of any phase.
    pub fn is_timeout(&self) -> bool {
        matches!(self, GenAiError::Timeout { .. })
    }

    /// A short, stable label for the kind of failure — suitable as a metric
    /// label or log field (`"rate_limited"`, `"auth"`, `"timeout"`, …).
    pub fn kind_label(&self) -> &'static str {
        match self {
            GenAiError::Template(_) => "template",
            GenAiError::TemplateNotFound(_) => "template_not_found",
            GenAiError::Unsupported { .. } => "unsupported",
            GenAiError::Transport(_) => "transport",
            GenAiError::Connect(_) => "connect",
            GenAiError::Api { .. } => "api",
            GenAiError::ResponseParse(_) => "response_parse",
            GenAiError::Config(_) => "config",
            GenAiError::Timeout { .. } => "timeout",
            GenAiError::RateLimited { .. } => "rate_limited",
            GenAiError::Auth { .. } => "auth",
            GenAiError::ContentFilter { .. } => "content_filter",
            GenAiError::ContextLengthExceeded { .. } => "context_length",
            GenAiError::Overloaded { .. } => "overloaded",
        }
    }

    /// Whether the request itself was at fault (bad input, bad credentials,
    /// refused content): repeating it unchanged cannot succeed, and the failure
    /// says nothing about the provider's health.
    pub fn is_caller_error(&self) -> bool {
        self.error_class().outcome == Outcome::Ignored
    }

    /// Whether the failure is temporary on the provider's side, so the same
    /// call may succeed later.
    pub fn is_transient(&self) -> bool {
        self.error_class().retry != RetryDecision::Stop
    }

    /// How a completion call that failed with this error should be treated by
    /// a retry loop and a circuit breaker — the classification the
    /// [model router](crate::genai::router) and the providers' optional retry
    /// use.
    ///
    /// | Error | Retry | Breaker |
    /// |---|---|---|
    /// | `Connect`, `Timeout { Connect }` | yes | failure |
    /// | `Api` `408`/`409`/`429`/`5xx` | yes, after `retry_after` when given | failure |
    /// | `RateLimited`, `Overloaded` | yes, after `retry_after` when given | failure |
    /// | `Transport`, `Timeout { Response \| StreamIdle }`, `ResponseParse` | **no** | failure |
    /// | other `Api` `4xx`, `Auth`, `ContentFilter`, `ContextLengthExceeded`, `Config`, `Template*`, `Unsupported` | no | ignored |
    ///
    /// A transport failure after the connection was up, and a response or
    /// stream-idle timeout, are not retried because a completion is not
    /// idempotent: the provider may have produced (and billed) the answer.
    /// Only failures where nothing was sent ([`Connect`](GenAiError::Connect),
    /// a connect timeout) and explicit refusals by the provider (a `5xx`,
    /// `429` or overload answer) are retried.
    pub fn error_class(&self) -> ErrorClass {
        match self {
            GenAiError::Connect(_) => ErrorClass::transient(),
            GenAiError::Transport(_) => ErrorClass::fatal(),
            GenAiError::Timeout { phase } => match phase {
                TimeoutPhase::Connect => ErrorClass::transient(),
                _ => ErrorClass::fatal(),
            },
            GenAiError::RateLimited { retry_after, .. }
            | GenAiError::Overloaded { retry_after, .. } => match retry_after {
                Some(delay) => ErrorClass::retry_after(*delay),
                None => ErrorClass::transient(),
            },
            GenAiError::Api {
                status,
                retry_after,
                ..
            } => match (*status, retry_after) {
                (408 | 409 | 429 | 500..=599, Some(delay)) => ErrorClass::retry_after(*delay),
                (408 | 409 | 429 | 500..=599, None) => ErrorClass::transient(),
                _ => ErrorClass::caller_error(),
            },
            GenAiError::ResponseParse(_) => ErrorClass::fatal(),
            GenAiError::Template(_)
            | GenAiError::TemplateNotFound(_)
            | GenAiError::Unsupported { .. }
            | GenAiError::Config(_)
            | GenAiError::Auth { .. }
            | GenAiError::ContentFilter { .. }
            | GenAiError::ContextLengthExceeded { .. } => ErrorClass::caller_error(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_for_retry_and_breaker() {
        let e = GenAiError::api(400, "bad");
        assert!(e.is_caller_error());
        assert!(!e.is_transient());

        let e = GenAiError::api(503, "busy");
        assert!(e.is_transient());
        assert!(!e.is_caller_error());

        let e = GenAiError::Timeout {
            phase: TimeoutPhase::Response,
        };
        assert!(!e.is_transient(), "a sent completion is not re-sent");
        assert!(!e.is_caller_error(), "but it does count against the model");

        let e = GenAiError::Timeout {
            phase: TimeoutPhase::Connect,
        };
        assert!(e.is_transient());

        let e = GenAiError::RateLimited {
            message: "slow down".into(),
            retry_after: Some(Duration::from_secs(2)),
            limits: RateLimitInfo::default(),
        };
        assert_eq!(e.status(), Some(429));
        assert_eq!(
            e.error_class().retry,
            RetryDecision::RetryAfter(Duration::from_secs(2))
        );
    }

    #[test]
    fn only_failures_before_sending_are_retried() {
        // G4: a transport failure after the connection was up may follow a
        // processed (billed) request, so it is never repeated.
        let sent = GenAiError::Transport("connection reset".into());
        assert_eq!(sent.error_class().retry, RetryDecision::Stop);
        assert_eq!(sent.error_class().outcome, Outcome::Failure);
        assert!(!sent.is_transient());
        let unsent = GenAiError::Connect("connection refused".into());
        assert_eq!(unsent.error_class().retry, RetryDecision::Retry);
        assert_eq!(unsent.kind_label(), "connect");
    }

    #[test]
    fn api_error_carries_type_and_retry_after() {
        let e = GenAiError::api(503, "busy")
            .with_error_type("server_error")
            .with_retry_after(Duration::from_secs(7));
        assert_eq!(e.error_type(), Some("server_error"));
        assert_eq!(e.retry_after(), Some(Duration::from_secs(7)));
        assert_eq!(
            e.error_class().retry,
            RetryDecision::RetryAfter(Duration::from_secs(7))
        );
        assert!(matches!(e, GenAiError::Api { status: 503, .. }));
        // A caller error is not retried whatever the header says.
        let e = GenAiError::api(400, "bad").with_retry_after(Duration::from_secs(1));
        assert_eq!(e.error_class().retry, RetryDecision::Stop);
        // Setting a type on another variant is a no-op.
        let e = GenAiError::Config("x".into()).with_error_type("t");
        assert_eq!(e.error_type(), None);
    }

    #[test]
    fn rate_limit_info_is_case_insensitive() {
        let info = RateLimitInfo::new([("X-RateLimit-Remaining-Requests", "3")]);
        assert_eq!(info.get("x-ratelimit-remaining-requests"), Some("3"));
        assert!(info.get("missing").is_none());
        assert!(!info.is_empty());
    }
}

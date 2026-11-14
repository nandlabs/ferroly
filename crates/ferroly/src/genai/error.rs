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

    /// The provider's HTTP transport failed (connection refused or reset, TLS
    /// failure, malformed response, oversized body).
    #[error("transport error: {0}")]
    Transport(String),

    /// The provider returned a non-success HTTP status that has no more
    /// specific variant.
    #[error("provider returned status {status}: {message}")]
    Api {
        /// The HTTP status code.
        status: u16,
        /// The provider's error message (truncated), prefixed with the
        /// provider's error type when it sent one.
        message: String,
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
            | GenAiError::Overloaded { retry_after, .. } => *retry_after,
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
    /// | `Transport`, `Timeout { Connect }`, `Api` `408`/`409`/`5xx` | yes | failure |
    /// | `RateLimited`, `Overloaded` | yes, after `retry_after` when given | failure |
    /// | `Timeout { Response \| StreamIdle }`, `ResponseParse` | **no** | failure |
    /// | other `Api` `4xx`, `Auth`, `ContentFilter`, `ContextLengthExceeded`, `Config`, `Template*`, `Unsupported` | no | ignored |
    ///
    /// A response or stream-idle timeout is not retried because a completion is
    /// not idempotent: the provider may have produced (and billed) the answer.
    pub fn error_class(&self) -> ErrorClass {
        match self {
            GenAiError::Transport(_) => ErrorClass::transient(),
            GenAiError::Timeout { phase } => match phase {
                TimeoutPhase::Connect => ErrorClass::transient(),
                _ => ErrorClass::fatal(),
            },
            GenAiError::RateLimited { retry_after, .. }
            | GenAiError::Overloaded { retry_after, .. } => match retry_after {
                Some(delay) => ErrorClass::retry_after(*delay),
                None => ErrorClass::transient(),
            },
            GenAiError::Api { status, .. } => match *status {
                408 | 409 | 429 | 500..=599 => ErrorClass::transient(),
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
        let e = GenAiError::Api {
            status: 400,
            message: "bad".into(),
        };
        assert!(e.is_caller_error());
        assert!(!e.is_transient());

        let e = GenAiError::Api {
            status: 503,
            message: "busy".into(),
        };
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
    fn rate_limit_info_is_case_insensitive() {
        let info = RateLimitInfo::new([("X-RateLimit-Remaining-Requests", "3")]);
        assert_eq!(info.get("x-ratelimit-remaining-requests"), Some("3"));
        assert!(info.get("missing").is_none());
        assert!(!info.is_empty());
    }
}

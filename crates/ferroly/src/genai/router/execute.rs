//! Pure execution helpers: request validation, option clamping and error
//! classification.

use ferroly::clients::{Outcome, RetryDecision};
use ferroly::genai::{Capability, CompletionRequest, GenAiError, ResponseFormat, TimeoutPhase};

use super::capability::ModelInfo;
use super::error::RouterError;
use super::task::Task;

/// Checks, before any model is chosen, that the request can satisfy what the
/// task demands. A task that requires tool use must come with tools: without
/// them the capability filter would pick a tool-capable (often more
/// expensive) model for a call that cannot use it.
pub(crate) fn validate_request(
    task: &Task,
    request: &CompletionRequest,
) -> Result<(), RouterError> {
    if task.required.contains(&Capability::ToolUse) && request.tools.is_empty() {
        return Err(RouterError::InvalidRequest(
            "the task requires the ToolUse capability but the request carries no tools".into(),
        ));
    }
    Ok(())
}

/// Reconciles caller options with the chosen model before dispatch: clamps
/// `max_tokens` to the model's limit and sets a JSON response format when the
/// task requires `JsonMode` but the caller left it unset.
pub(crate) fn clamp_options(
    mut request: CompletionRequest,
    info: &ModelInfo,
    task: &Task,
) -> CompletionRequest {
    if let Some(max) = request.options.max_tokens {
        if info.max_output_tokens > 0 && max > info.max_output_tokens {
            request.options.max_tokens = Some(info.max_output_tokens);
        }
    }
    if task.required.contains(&Capability::JsonMode) && request.response_format.is_none() {
        request.response_format = Some(ResponseFormat::Json);
    }
    request
}

/// What the router does with a failed attempt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Disposition {
    /// Whether (and when) to retry the **same** model.
    pub(crate) retry: RetryDecision,
    /// Whether to move on to the next candidate.
    pub(crate) fallover: bool,
    /// How the failure counts toward the model's circuit breaker.
    pub(crate) outcome: Outcome,
}

/// Classifies a provider error for a routed call.
///
/// | Error | Same-model retry | Fallover | Breaker |
/// |---|---|---|---|
/// | transport, connect timeout, `5xx` | yes | yes | failure |
/// | rate limited / overloaded | yes, after `Retry-After` | yes | failure |
/// | response or stream-idle timeout | no | only if `idempotent` or `fallback_on_timeout` | failure |
/// | unparseable response | no | yes | failure |
/// | context length exceeded, unsupported feature | no | yes — another model may fit | ignored |
/// | auth, validation (`4xx`), content filter, config | no | **no** | ignored |
///
/// `idempotent` marks calls that are safe to repeat (embeddings): a timeout
/// then behaves like any other transient failure.
pub(crate) fn classify(
    err: &GenAiError,
    idempotent: bool,
    fallback_on_timeout: bool,
) -> Disposition {
    let class = err.error_class();
    match err {
        GenAiError::Timeout { phase } if *phase != TimeoutPhase::Connect => {
            if idempotent {
                Disposition {
                    retry: RetryDecision::Retry,
                    fallover: true,
                    outcome: Outcome::Failure,
                }
            } else {
                Disposition {
                    retry: RetryDecision::Stop,
                    fallover: fallback_on_timeout,
                    outcome: Outcome::Failure,
                }
            }
        }
        GenAiError::ResponseParse(_)
        | GenAiError::ContextLengthExceeded { .. }
        | GenAiError::Unsupported { .. } => Disposition {
            retry: RetryDecision::Stop,
            fallover: true,
            outcome: class.outcome,
        },
        _ => Disposition {
            retry: class.retry,
            fallover: class.retry != RetryDecision::Stop,
            outcome: class.outcome,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroly::genai::{Capability, CompletionRequest, Options, RateLimitInfo, ToolDefinition};
    use std::time::Duration;

    fn model(max_out: u32) -> ModelInfo {
        ModelInfo::new("p", "m").limits(1000, max_out)
    }

    #[test]
    fn clamps_max_tokens_and_sets_json() {
        let task = Task {
            required: vec![Capability::JsonMode],
            ..Default::default()
        };
        let req = CompletionRequest::builder("m")
            .options(Options::builder().max_tokens(9999).build())
            .build();
        let out = clamp_options(req, &model(4096), &task);
        assert_eq!(out.options.max_tokens, Some(4096));
        assert!(out.response_format.is_some());
    }

    #[test]
    fn tool_use_requires_tools() {
        let task = Task::new().require(Capability::ToolUse);
        let bare = CompletionRequest::builder("m").build();
        assert!(matches!(
            validate_request(&task, &bare),
            Err(RouterError::InvalidRequest(_))
        ));
        let with_tool = CompletionRequest::builder("m")
            .tool(ToolDefinition::new("t", "d", ferroly::codec::Value::Null))
            .build();
        assert!(validate_request(&task, &with_tool).is_ok());
        assert!(validate_request(&Task::new(), &bare).is_ok());
    }

    #[test]
    fn classifies_errors() {
        let api = |status| GenAiError::Api {
            status,
            message: "x".into(),
        };
        // Transient: retry, fall over, count.
        let d = classify(&GenAiError::Transport("reset".into()), false, false);
        assert!(d.fallover && d.retry == RetryDecision::Retry && d.outcome == Outcome::Failure);
        assert!(classify(&api(503), false, false).fallover);

        // Caller errors: stop, and never count against the model.
        for status in [400, 401, 403, 404, 422] {
            let d = classify(&api(status), false, false);
            assert!(!d.fallover, "{status}");
            assert_eq!(d.retry, RetryDecision::Stop, "{status}");
            assert_eq!(d.outcome, Outcome::Ignored, "{status}");
        }
        let d = classify(
            &GenAiError::ContentFilter {
                message: "no".into(),
            },
            false,
            false,
        );
        assert!(!d.fallover && d.outcome == Outcome::Ignored);

        // Throttling honours the server's delay, then falls over.
        let d = classify(
            &GenAiError::RateLimited {
                message: "slow".into(),
                retry_after: Some(Duration::from_secs(3)),
                limits: RateLimitInfo::default(),
            },
            false,
            false,
        );
        assert_eq!(d.retry, RetryDecision::RetryAfter(Duration::from_secs(3)));
        assert!(d.fallover && d.outcome == Outcome::Failure);

        // A sent completion that timed out is not repeated anywhere …
        let timeout = GenAiError::Timeout {
            phase: TimeoutPhase::Response,
        };
        let d = classify(&timeout, false, false);
        assert!(!d.fallover && d.retry == RetryDecision::Stop && d.outcome == Outcome::Failure);
        // … unless the caller opts in, or the call is idempotent.
        assert!(classify(&timeout, false, true).fallover);
        assert!(classify(&timeout, true, false).fallover);
        // Nothing was sent on a connect timeout.
        let d = classify(
            &GenAiError::Timeout {
                phase: TimeoutPhase::Connect,
            },
            false,
            false,
        );
        assert!(d.fallover && d.retry == RetryDecision::Retry);

        // A bigger model may fit a prompt this one cannot.
        let d = classify(
            &GenAiError::ContextLengthExceeded {
                message: "too long".into(),
            },
            false,
            false,
        );
        assert!(d.fallover && d.outcome == Outcome::Ignored);
    }
}

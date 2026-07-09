//! Pure execution helpers: option clamping and error classification.

use ferroly::genai::{Capability, CompletionRequest, GenAiError, ResponseFormat};

use super::capability::ModelInfo;
use super::task::Task;

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

/// Whether an error should trigger fallover to a different model. Transient /
/// capacity errors (429, 5xx, network) are retryable; client / permanent errors
/// (4xx, bad schema, config) and content-policy blocks are not — a different
/// model would fail identically.
pub(crate) fn is_retryable(err: &GenAiError, finish_reason: Option<&str>) -> bool {
    if matches!(
        finish_reason,
        Some("content_filter") | Some("content-filter")
    ) {
        return false;
    }
    match err {
        GenAiError::Transport(_) => true,
        GenAiError::Api { status, .. } => *status == 429 || (500..=599).contains(status),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroly::genai::{Capability, CompletionRequest, Options};

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
    fn classifies_errors() {
        assert!(is_retryable(&GenAiError::Transport("reset".into()), None));
        assert!(is_retryable(
            &GenAiError::Api {
                status: 503,
                message: "x".into()
            },
            None
        ));
        assert!(!is_retryable(
            &GenAiError::Api {
                status: 400,
                message: "bad".into()
            },
            None
        ));
        // Content policy is never retryable, even on a transient-looking error.
        assert!(!is_retryable(
            &GenAiError::Transport("x".into()),
            Some("content_filter")
        ));
    }
}

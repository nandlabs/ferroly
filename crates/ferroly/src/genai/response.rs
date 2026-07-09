//! Completion response and streaming chunk types.

use ferroly::genai::Message;

/// Normalized token-usage accounting for cross-provider cost tracking.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    /// Tokens in the prompt/input.
    pub prompt_tokens: Option<u32>,
    /// Tokens in the completion/output.
    pub completion_tokens: Option<u32>,
    /// Total tokens, if the provider reports it directly.
    pub total_tokens: Option<u32>,
}

/// A non-streaming completion result.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletionResponse {
    /// The model that produced the response.
    pub model: String,
    /// The assistant message (may contain text and/or tool-call parts).
    pub message: Message,
    /// The provider's finish reason (e.g. `stop`, `length`, `tool_calls`).
    pub finish_reason: Option<String>,
    /// Token usage, if reported.
    pub usage: Option<Usage>,
}

impl CompletionResponse {
    /// Convenience accessor for the concatenated text of the response message.
    pub fn text(&self) -> String {
        self.message.text_content()
    }

    /// Decodes the response text as JSON into `T` — the structured-output path,
    /// pairing with `ResponseFormat::Json` / `JsonSchema`. Errors as
    /// `GenAiError::ResponseParse` if the text is not valid JSON for `T`.
    pub fn decode<T: ferroly::codec::Decode>(&self) -> Result<T, crate::genai::GenAiError> {
        ferroly::codec::json::decode(&self.text())
            .map_err(|e| crate::genai::GenAiError::ResponseParse(e.to_string()))
    }
}

/// One chunk of a streaming completion.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompletionChunk {
    /// Incremental text appended by this chunk (may be empty).
    pub delta: String,
    /// The finish reason, present on the terminal chunk.
    pub finish_reason: Option<String>,
    /// Token usage, present on the terminal chunk for providers that report it.
    pub usage: Option<Usage>,
}

/// A capability a provider or model may or may not support.
///
/// Used both for provider-level [`GenAiProvider::supports`](crate::genai::GenAiProvider::supports)
/// and for per-model [`ModelInfo`](crate::genai::ModelInfo) metadata consumed by
/// the model router. `#[non_exhaustive]` so new capabilities can be added without
/// breaking downstream `match`es.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// Plain text completion.
    Text,
    /// Multi-turn chat.
    Chat,
    /// Streaming completions.
    Streaming,
    /// Image / multimodal input.
    Vision,
    /// Audio input/output.
    Audio,
    /// Tool / function calling.
    ToolUse,
    /// Constrained JSON output.
    JsonMode,
    /// Text embeddings.
    Embeddings,
    /// Extended reasoning / chain-of-thought models.
    Reasoning,
}

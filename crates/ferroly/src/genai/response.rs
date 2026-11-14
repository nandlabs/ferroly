//! Completion response, streaming chunk and stream-aggregation types.

use ferroly::codec::{json, Value};
use ferroly::genai::{GenAiError, Message, MessagePart, Role};

/// Normalized token-usage accounting for cross-provider cost tracking.
///
/// `prompt_tokens` is always the **total** input, including any tokens served
/// from or written to a prompt cache; [`cached_tokens`](Usage::cached_tokens)
/// and [`cache_write_tokens`](Usage::cache_write_tokens) are subsets of it.
/// Providers that report the cached part separately (Claude) are normalized to
/// this shape.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    /// Tokens in the prompt/input (cached tokens included).
    pub prompt_tokens: Option<u32>,
    /// Tokens in the completion/output (reasoning tokens included).
    pub completion_tokens: Option<u32>,
    /// Total tokens, if the provider reports it or both parts are known.
    pub total_tokens: Option<u32>,
    /// Prompt tokens served from the provider's prompt cache.
    pub cached_tokens: Option<u32>,
    /// Prompt tokens written to the provider's prompt cache by this call.
    pub cache_write_tokens: Option<u32>,
    /// Output tokens spent on reasoning / thinking.
    pub reasoning_tokens: Option<u32>,
}

impl Usage {
    /// Usage with the given prompt and completion counts (and their sum as the
    /// total).
    pub fn new(prompt_tokens: u32, completion_tokens: u32) -> Self {
        Usage {
            prompt_tokens: Some(prompt_tokens),
            completion_tokens: Some(completion_tokens),
            total_tokens: Some(prompt_tokens.saturating_add(completion_tokens)),
            ..Usage::default()
        }
    }

    /// Overlays `other` on `self`: every count `other` reports replaces the
    /// one here. Streams report usage in pieces (input first, output last);
    /// this folds them together.
    pub fn merge(&mut self, other: &Usage) {
        fn take(dst: &mut Option<u32>, src: Option<u32>) {
            if src.is_some() {
                *dst = src;
            }
        }
        take(&mut self.prompt_tokens, other.prompt_tokens);
        take(&mut self.completion_tokens, other.completion_tokens);
        take(&mut self.total_tokens, other.total_tokens);
        take(&mut self.cached_tokens, other.cached_tokens);
        take(&mut self.cache_write_tokens, other.cache_write_tokens);
        take(&mut self.reasoning_tokens, other.reasoning_tokens);
    }

    /// Fills `total_tokens` from the two parts when the provider did not
    /// report it.
    pub(crate) fn fill_total(&mut self) {
        if self.total_tokens.is_none() {
            if let (Some(p), Some(c)) = (self.prompt_tokens, self.completion_tokens) {
                self.total_tokens = Some(p.saturating_add(c));
            }
        }
    }
}

/// Why the model stopped generating, normalized across providers.
///
/// The provider's own string is kept next to it
/// ([`CompletionResponse::raw_finish_reason`]).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FinishReason {
    /// The model finished its answer (or hit a stop sequence).
    Stop,
    /// The output-token limit was reached: the answer is **truncated**.
    Length,
    /// The model asked for one or more tool calls.
    ToolCalls,
    /// The provider's content policy blocked or cut the output (including a
    /// model refusal).
    ContentFilter,
    /// The provider reported that generation failed.
    Error,
    /// A reason this enum does not model; read the raw string.
    Other,
}

impl FinishReason {
    /// A stable lower-case label (`"stop"`, `"length"`, `"tool_calls"`,
    /// `"content_filter"`, `"error"`, `"other"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            FinishReason::Stop => "stop",
            FinishReason::Length => "length",
            FinishReason::ToolCalls => "tool_calls",
            FinishReason::ContentFilter => "content_filter",
            FinishReason::Error => "error",
            FinishReason::Other => "other",
        }
    }

    /// Maps a provider's finish/stop-reason string. Understands the OpenAI
    /// (`stop`, `length`, `tool_calls`, `function_call`, `content_filter`),
    /// Claude (`end_turn`, `stop_sequence`, `max_tokens`, `tool_use`,
    /// `refusal`, …) and Ollama (`stop`, `length`) vocabularies; anything else
    /// is [`FinishReason::Other`].
    pub fn from_provider(raw: &str) -> FinishReason {
        match raw {
            "stop" | "end_turn" | "stop_sequence" | "eos" => FinishReason::Stop,
            "length" | "max_tokens" | "model_context_window_exceeded" => FinishReason::Length,
            "tool_calls" | "function_call" | "tool_use" => FinishReason::ToolCalls,
            "content_filter" | "content-filter" | "refusal" | "safety" => {
                FinishReason::ContentFilter
            }
            "error" => FinishReason::Error,
            _ => FinishReason::Other,
        }
    }
}

impl std::fmt::Display for FinishReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One source the model cited for (part of) its answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Citation {
    /// The quoted source text, when the provider returns it.
    pub cited_text: Option<String>,
    /// The source's title.
    pub title: Option<String>,
    /// The source's URL (web citations).
    pub url: Option<String>,
    /// The index of the cited document among the request's documents.
    pub document_index: Option<u32>,
    /// Start of the cited span: a character or page offset in the source
    /// (Claude) or a character offset in the answer (OpenAI).
    pub start: Option<u32>,
    /// End of the cited span, in the same unit as `start`.
    pub end: Option<u32>,
}

/// Grounding metadata: what the answer was based on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grounding {
    /// The citations, in the order the provider returned them.
    pub citations: Vec<Citation>,
}

/// A non-streaming completion result.
///
/// The struct is `#[non_exhaustive]`: read the fields freely, and build one
/// with [`CompletionResponse::new`] and the `with_*` setters.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct CompletionResponse {
    /// The provider's id for this response, if it sent one.
    pub id: Option<String>,
    /// The model that produced the response.
    pub model: String,
    /// The assistant message (may contain text and/or tool-call parts).
    pub message: Message,
    /// Why generation stopped, normalized. [`FinishReason::Length`] means the
    /// answer was truncated by the token limit.
    pub finish_reason: Option<FinishReason>,
    /// The provider's own finish/stop-reason string (`stop`, `end_turn`, …).
    pub raw_finish_reason: Option<String>,
    /// Token usage, if reported.
    pub usage: Option<Usage>,
    /// Reasoning / thinking text the provider returned, if any. Never part of
    /// [`text`](CompletionResponse::text).
    pub reasoning: Option<String>,
    /// Citations for the answer, where the provider returns them.
    pub grounding: Option<Grounding>,
}

impl CompletionResponse {
    /// A response from `model` carrying `message`, with everything else unset.
    pub fn new(model: impl Into<String>, message: Message) -> Self {
        CompletionResponse {
            id: None,
            model: model.into(),
            message,
            finish_reason: None,
            raw_finish_reason: None,
            usage: None,
            reasoning: None,
            grounding: None,
        }
    }

    /// A plain-text assistant response that finished normally — handy in tests
    /// and custom providers.
    pub fn from_text(model: impl Into<String>, text: impl Into<String>) -> Self {
        CompletionResponse::new(
            model,
            Message {
                role: Role::Assistant,
                id: None,
                parts: vec![MessagePart::Text(text.into())],
            },
        )
        .with_finish_reason(FinishReason::Stop)
    }

    /// Sets the normalized finish reason (and its label as the raw string when
    /// none is set yet).
    pub fn with_finish_reason(mut self, reason: FinishReason) -> Self {
        self.finish_reason = Some(reason);
        if self.raw_finish_reason.is_none() {
            self.raw_finish_reason = Some(reason.as_str().to_string());
        }
        self
    }

    /// Sets the finish reason from a provider string, keeping the string.
    pub fn with_raw_finish_reason(mut self, raw: impl Into<String>) -> Self {
        let raw = raw.into();
        self.finish_reason = Some(FinishReason::from_provider(&raw));
        self.raw_finish_reason = Some(raw);
        self
    }

    /// Sets the token usage.
    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = Some(usage);
        self
    }

    /// Sets the provider response id.
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Sets the reasoning text.
    pub fn with_reasoning(mut self, reasoning: impl Into<String>) -> Self {
        self.reasoning = Some(reasoning.into());
        self
    }

    /// Sets the grounding metadata.
    pub fn with_grounding(mut self, grounding: Grounding) -> Self {
        self.grounding = Some(grounding);
        self
    }

    /// Convenience accessor for the concatenated text of the response message.
    pub fn text(&self) -> String {
        self.message.text_content()
    }

    /// Whether the output was cut off by the token limit
    /// ([`FinishReason::Length`]).
    pub fn is_truncated(&self) -> bool {
        self.finish_reason == Some(FinishReason::Length)
    }

    /// The tool calls the model requested, as `(id, name, arguments)`.
    pub fn tool_calls(&self) -> Vec<(&str, &str, &Value)> {
        self.message.tool_calls()
    }

    /// Decodes the response text as JSON into `T` — the structured-output path,
    /// pairing with `ResponseFormat::Json` / `JsonSchema`.
    ///
    /// Surrounding whitespace is ignored, and so is one Markdown code fence
    /// around the document (```` ```json … ``` ````), which models add even in
    /// JSON mode. Anything else — prose before or after the JSON, an empty
    /// answer, JSON of the wrong shape — is a
    /// [`GenAiError::ResponseParse`] saying what was found.
    pub fn decode<T: ferroly::codec::Decode>(&self) -> Result<T, GenAiError> {
        let text = self.text();
        let doc = json_document(&text)?;
        json::decode(doc).map_err(|e| {
            GenAiError::ResponseParse(format!(
                "response text is not valid JSON for the requested type: {e}{}",
                self.truncation_hint()
            ))
        })
    }

    /// Like [`decode`](CompletionResponse::decode) but returns the untyped
    /// [`Value`].
    pub fn decode_value(&self) -> Result<Value, GenAiError> {
        let text = self.text();
        let doc = json_document(&text)?;
        json::from_str(doc).map_err(|e| {
            GenAiError::ResponseParse(format!(
                "response text is not valid JSON: {e}{}",
                self.truncation_hint()
            ))
        })
    }

    fn truncation_hint(&self) -> &'static str {
        if self.is_truncated() {
            " (the response was truncated by the output-token limit)"
        } else {
            ""
        }
    }
}

/// Extracts the JSON document from a model answer: trims whitespace and
/// removes one enclosing Markdown code fence.
fn json_document(text: &str) -> Result<&str, GenAiError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(GenAiError::ResponseParse(
            "response text is empty; expected a JSON document".into(),
        ));
    }
    let Some(rest) = trimmed.strip_prefix("```") else {
        return Ok(trimmed);
    };
    // The opening fence line may carry an info string (`json`, `JSON`, …).
    let Some(newline) = rest.find('\n') else {
        return Err(GenAiError::ResponseParse(
            "response text is an unterminated code fence; expected a JSON document".into(),
        ));
    };
    let info = rest[..newline].trim();
    if info.chars().any(char::is_whitespace) {
        return Err(GenAiError::ResponseParse(
            "response text starts with a malformed code fence; expected a JSON document".into(),
        ));
    }
    let body = rest[newline + 1..].trim_end();
    let Some(inner) = body.strip_suffix("```") else {
        return Err(GenAiError::ResponseParse(
            "response text has text after its code fence (or the fence is not closed); \
             expected only a JSON document"
                .into(),
        ));
    };
    let inner = inner.trim();
    if inner.is_empty() {
        return Err(GenAiError::ResponseParse(
            "response code fence is empty; expected a JSON document".into(),
        ));
    }
    Ok(inner)
}

/// A fragment of one tool call in a streamed response.
///
/// A provider announces a call with its `id` and `name` once and then sends
/// the JSON `arguments` in pieces; fragments of the same call share an
/// `index`. Parallel calls have different indexes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCallDelta {
    /// Which tool call of the response this fragment belongs to.
    pub index: u32,
    /// The provider-assigned call id (on the fragment that opens the call).
    pub id: Option<String>,
    /// The tool name (on the fragment that opens the call).
    pub name: Option<String>,
    /// A fragment of the JSON-encoded arguments; concatenate the fragments of
    /// one index to get the whole document.
    pub arguments: String,
}

/// One event of a streaming completion.
///
/// A stream is a sequence of `Result<CompletionChunk, GenAiError>`: content
/// arrives as deltas, usage and the finish reason as their own events, and a
/// failure — including one the provider reports in the middle of the stream —
/// as an `Err`. Simple consumers call [`text`](CompletionChunk::text) on every
/// chunk; [`StreamAccumulator`] / [`collect_stream`] fold a stream into a
/// [`CompletionResponse`].
///
/// ```
/// use ferroly::genai::{CompletionChunk, FinishReason};
///
/// let chunks = [
///     CompletionChunk::text_delta("Hel"),
///     CompletionChunk::text_delta("lo"),
///     CompletionChunk::done(FinishReason::Stop),
/// ];
/// let text: String = chunks.iter().map(|c| c.text()).collect();
/// assert_eq!(text, "Hello");
/// assert!(chunks[2].is_done());
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum CompletionChunk {
    /// The stream opened: the provider's response id and serving model, where
    /// it sends them up front.
    Start {
        /// The provider's response id.
        id: Option<String>,
        /// The model serving the response.
        model: Option<String>,
    },
    /// Incremental answer text.
    TextDelta(String),
    /// Incremental reasoning / thinking text (not part of the answer).
    ReasoningDelta(String),
    /// A fragment of a tool call.
    ToolCallDelta(ToolCallDelta),
    /// A complete provider-specific content block that has no normalized form
    /// (for example a Claude thinking block with its signature). It must be
    /// sent back unchanged in the next request to the same provider, which
    /// [`StreamAccumulator`] arranges by storing it as a
    /// [`MessagePart::ProviderData`].
    ProviderData {
        /// The provider that produced (and understands) the block.
        provider: String,
        /// The block, verbatim.
        data: Value,
    },
    /// Token usage. May arrive more than once (input counts first, output
    /// counts last); later reports overlay earlier ones.
    Usage(Usage),
    /// The model stopped generating.
    Done {
        /// The normalized reason.
        finish_reason: FinishReason,
        /// The provider's own reason string.
        raw_finish_reason: Option<String>,
    },
}

impl CompletionChunk {
    /// A [`TextDelta`](CompletionChunk::TextDelta).
    pub fn text_delta(delta: impl Into<String>) -> Self {
        CompletionChunk::TextDelta(delta.into())
    }

    /// A [`Done`](CompletionChunk::Done) with the reason's label as the raw
    /// string.
    pub fn done(finish_reason: FinishReason) -> Self {
        CompletionChunk::Done {
            finish_reason,
            raw_finish_reason: Some(finish_reason.as_str().to_string()),
        }
    }

    /// A [`Done`](CompletionChunk::Done) built from a provider's reason string.
    pub fn done_raw(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        CompletionChunk::Done {
            finish_reason: FinishReason::from_provider(&raw),
            raw_finish_reason: Some(raw),
        }
    }

    /// The answer text this chunk adds — empty for every chunk that is not a
    /// [`TextDelta`](CompletionChunk::TextDelta). Lets a consumer that only
    /// wants the text ignore the other events:
    /// `out.push_str(chunk.text())`.
    pub fn text(&self) -> &str {
        match self {
            CompletionChunk::TextDelta(t) => t,
            _ => "",
        }
    }

    /// The tool-call fragment, if this chunk is one.
    pub fn tool_call(&self) -> Option<&ToolCallDelta> {
        match self {
            CompletionChunk::ToolCallDelta(d) => Some(d),
            _ => None,
        }
    }

    /// The usage report, if this chunk is one.
    pub fn usage(&self) -> Option<&Usage> {
        match self {
            CompletionChunk::Usage(u) => Some(u),
            _ => None,
        }
    }

    /// The finish reason, if this chunk ends the generation.
    pub fn finish_reason(&self) -> Option<FinishReason> {
        match self {
            CompletionChunk::Done { finish_reason, .. } => Some(*finish_reason),
            _ => None,
        }
    }

    /// Whether this chunk ends the generation.
    pub fn is_done(&self) -> bool {
        matches!(self, CompletionChunk::Done { .. })
    }
}

/// A part of the message being assembled, in arrival order.
#[derive(Debug)]
enum Building {
    Text(String),
    Tool {
        index: u32,
        id: String,
        name: String,
        arguments: String,
    },
    Provider {
        provider: String,
        data: Value,
    },
}

/// Folds the chunks of a stream into a [`CompletionResponse`].
///
/// Feed it every `Ok` chunk in order with [`push`](StreamAccumulator::push),
/// then call [`finish`](StreamAccumulator::finish). Text deltas are
/// concatenated, tool-call fragments are joined per index and their arguments
/// parsed, usage reports are merged, and the parts keep the order in which the
/// provider produced them.
///
/// ```
/// use ferroly::genai::{CompletionChunk, FinishReason, StreamAccumulator, ToolCallDelta};
///
/// let mut acc = StreamAccumulator::new();
/// acc.push(CompletionChunk::text_delta("Checking. "));
/// acc.push(CompletionChunk::ToolCallDelta(ToolCallDelta {
///     index: 0,
///     id: Some("call_1".into()),
///     name: Some("get_weather".into()),
///     arguments: "{\"city\":".into(),
/// }));
/// acc.push(CompletionChunk::ToolCallDelta(ToolCallDelta {
///     index: 0,
///     arguments: "\"Paris\"}".into(),
///     ..Default::default()
/// }));
/// acc.push(CompletionChunk::done(FinishReason::ToolCalls));
///
/// let response = acc.finish().unwrap();
/// assert_eq!(response.text(), "Checking. ");
/// let calls = response.tool_calls();
/// assert_eq!(calls[0].1, "get_weather");
/// assert_eq!(calls[0].2.get("city").unwrap().as_str(), Some("Paris"));
/// ```
#[derive(Debug, Default)]
pub struct StreamAccumulator {
    id: Option<String>,
    model: Option<String>,
    parts: Vec<Building>,
    reasoning: String,
    usage: Option<Usage>,
    finish: Option<(FinishReason, Option<String>)>,
}

impl StreamAccumulator {
    /// An empty accumulator.
    pub fn new() -> Self {
        StreamAccumulator::default()
    }

    /// An accumulator whose response reports `model` unless the stream names
    /// the serving model itself.
    pub fn for_model(model: impl Into<String>) -> Self {
        StreamAccumulator {
            model: Some(model.into()),
            ..StreamAccumulator::default()
        }
    }

    /// Adds one chunk.
    pub fn push(&mut self, chunk: CompletionChunk) {
        match chunk {
            CompletionChunk::Start { id, model } => {
                if id.is_some() {
                    self.id = id;
                }
                if model.as_deref().is_some_and(|m| !m.is_empty()) {
                    self.model = model;
                }
            }
            CompletionChunk::TextDelta(delta) => {
                if delta.is_empty() {
                    return;
                }
                if let Some(Building::Text(text)) = self.parts.last_mut() {
                    text.push_str(&delta);
                } else {
                    self.parts.push(Building::Text(delta));
                }
            }
            CompletionChunk::ReasoningDelta(delta) => self.reasoning.push_str(&delta),
            CompletionChunk::ToolCallDelta(delta) => {
                let existing = self.parts.iter_mut().find_map(|p| match p {
                    Building::Tool {
                        index,
                        id,
                        name,
                        arguments,
                    } if *index == delta.index => Some((id, name, arguments)),
                    _ => None,
                });
                match existing {
                    Some((id, name, arguments)) => {
                        if let Some(new_id) = delta.id.filter(|s| !s.is_empty()) {
                            *id = new_id;
                        }
                        if let Some(new_name) = delta.name.filter(|s| !s.is_empty()) {
                            *name = new_name;
                        }
                        arguments.push_str(&delta.arguments);
                    }
                    None => self.parts.push(Building::Tool {
                        index: delta.index,
                        id: delta.id.unwrap_or_default(),
                        name: delta.name.unwrap_or_default(),
                        arguments: delta.arguments,
                    }),
                }
            }
            CompletionChunk::ProviderData { provider, data } => {
                self.parts.push(Building::Provider { provider, data });
            }
            CompletionChunk::Usage(usage) => match &mut self.usage {
                Some(total) => total.merge(&usage),
                None => self.usage = Some(usage),
            },
            CompletionChunk::Done {
                finish_reason,
                raw_finish_reason,
            } => self.finish = Some((finish_reason, raw_finish_reason)),
        }
    }

    /// The answer text accumulated so far.
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                Building::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Whether a [`CompletionChunk::Done`] has been seen.
    pub fn is_done(&self) -> bool {
        self.finish.is_some()
    }

    /// Builds the response.
    ///
    /// Fails with [`GenAiError::ResponseParse`] when a tool call's accumulated
    /// arguments are not valid JSON — which is what a call cut off by the
    /// token limit looks like; the message says so when the finish reason is
    /// [`FinishReason::Length`]. A stream that never sent a `Done` yields a
    /// response whose `finish_reason` is `None`.
    pub fn finish(self) -> Result<CompletionResponse, GenAiError> {
        let (finish_reason, raw_finish_reason) = match self.finish {
            Some((reason, raw)) => (Some(reason), raw),
            None => (None, None),
        };
        let mut parts = Vec::with_capacity(self.parts.len());
        for part in self.parts {
            match part {
                Building::Text(text) => parts.push(MessagePart::Text(text)),
                Building::Provider { provider, data } => {
                    parts.push(MessagePart::ProviderData { provider, data });
                }
                Building::Tool {
                    id,
                    name,
                    arguments,
                    ..
                } => {
                    let parsed = if arguments.trim().is_empty() {
                        Value::Object(Vec::new())
                    } else {
                        json::from_str(&arguments).map_err(|e| {
                            let hint = if finish_reason == Some(FinishReason::Length) {
                                " (the stream was cut off by the output-token limit)"
                            } else {
                                ""
                            };
                            GenAiError::ResponseParse(format!(
                                "arguments of tool call '{name}' are not valid JSON: {e}{hint}"
                            ))
                        })?
                    };
                    parts.push(MessagePart::ToolCall {
                        id,
                        name,
                        arguments: parsed,
                    });
                }
            }
        }
        let mut usage = self.usage;
        if let Some(u) = &mut usage {
            u.fill_total();
        }
        Ok(CompletionResponse {
            id: self.id,
            model: self.model.unwrap_or_default(),
            message: Message {
                role: Role::Assistant,
                id: None,
                parts,
            },
            finish_reason,
            raw_finish_reason,
            usage,
            reasoning: if self.reasoning.is_empty() {
                None
            } else {
                Some(self.reasoning)
            },
            grounding: None,
        })
    }
}

/// Drains a stream and folds it into a [`CompletionResponse`].
///
/// Returns the first `Err` the stream carries (a mid-stream provider error, a
/// transport failure, an idle timeout) instead of a partial response.
pub async fn collect_stream(
    mut stream: ferroly::genai::ChunkStream,
) -> Result<CompletionResponse, GenAiError> {
    let mut acc = StreamAccumulator::new();
    while let Some(item) = stream.recv().await {
        acc.push(item?);
    }
    acc.finish()
}

/// Splits a complete response into the chunks a stream of it would carry —
/// the inverse of [`StreamAccumulator`]. Used by the mock provider and by
/// custom providers that only implement `complete`.
pub fn response_to_chunks(response: &CompletionResponse) -> Vec<CompletionChunk> {
    let mut chunks = vec![CompletionChunk::Start {
        id: response.id.clone(),
        model: Some(response.model.clone()),
    }];
    if let Some(reasoning) = &response.reasoning {
        chunks.push(CompletionChunk::ReasoningDelta(reasoning.clone()));
    }
    let mut tool_index = 0u32;
    for part in &response.message.parts {
        match part {
            MessagePart::Text(text) => chunks.push(CompletionChunk::TextDelta(text.clone())),
            MessagePart::ToolCall {
                id,
                name,
                arguments,
            } => {
                chunks.push(CompletionChunk::ToolCallDelta(ToolCallDelta {
                    index: tool_index,
                    id: Some(id.clone()),
                    name: Some(name.clone()),
                    arguments: json::to_string(arguments),
                }));
                tool_index += 1;
            }
            MessagePart::ProviderData { provider, data } => {
                chunks.push(CompletionChunk::ProviderData {
                    provider: provider.clone(),
                    data: data.clone(),
                });
            }
            _ => {}
        }
    }
    if let Some(usage) = &response.usage {
        chunks.push(CompletionChunk::Usage(usage.clone()));
    }
    if let Some(reason) = response.finish_reason {
        chunks.push(CompletionChunk::Done {
            finish_reason: reason,
            raw_finish_reason: response.raw_finish_reason.clone(),
        });
    }
    chunks
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

#[cfg(test)]
mod tests {
    use super::*;

    fn response(text: &str) -> CompletionResponse {
        CompletionResponse::from_text("m", text)
    }

    #[test]
    fn decode_tolerates_fences_and_whitespace() {
        for text in [
            "{\"a\":1}",
            "  \n{\"a\":1}\n ",
            "```json\n{\"a\":1}\n```",
            "```\n{\"a\":1}\n```\n",
            "\n```JSON\n  {\"a\":1}\n```",
        ] {
            let v = response(text).decode_value().unwrap();
            assert_eq!(v.get("a").unwrap().as_i64(), Some(1), "input: {text:?}");
        }
    }

    #[test]
    fn decode_reports_clear_errors() {
        for (text, needle) in [
            ("", "empty"),
            ("   ", "empty"),
            ("```json\n{\"a\":1}", "after its code fence"),
            (
                "```json\n{\"a\":1}\n```\nHope that helps!",
                "after its code fence",
            ),
            ("```json", "unterminated"),
            ("```\n\n```", "empty"),
            ("Sure! {\"a\":1}", "not valid JSON"),
        ] {
            let err = response(text).decode_value().unwrap_err();
            let msg = err.to_string();
            assert!(
                matches!(err, GenAiError::ResponseParse(_)) && msg.contains(needle),
                "input {text:?} gave {msg:?}"
            );
        }
    }

    #[test]
    fn accumulator_orders_parts_and_merges_usage() {
        let mut acc = StreamAccumulator::for_model("fallback-model");
        acc.push(CompletionChunk::Start {
            id: Some("r1".into()),
            model: Some("served".into()),
        });
        acc.push(CompletionChunk::Usage(Usage {
            prompt_tokens: Some(10),
            cached_tokens: Some(4),
            ..Usage::default()
        }));
        acc.push(CompletionChunk::text_delta("a"));
        acc.push(CompletionChunk::text_delta("b"));
        acc.push(CompletionChunk::ToolCallDelta(ToolCallDelta {
            index: 1,
            id: Some("c2".into()),
            name: Some("two".into()),
            arguments: String::new(),
        }));
        acc.push(CompletionChunk::ToolCallDelta(ToolCallDelta {
            index: 0,
            id: Some("c1".into()),
            name: Some("one".into()),
            arguments: "{\"x\":".into(),
        }));
        acc.push(CompletionChunk::ToolCallDelta(ToolCallDelta {
            index: 0,
            arguments: "1}".into(),
            ..Default::default()
        }));
        acc.push(CompletionChunk::Usage(Usage {
            completion_tokens: Some(5),
            ..Usage::default()
        }));
        acc.push(CompletionChunk::done_raw("tool_use"));
        assert!(acc.is_done());

        let r = acc.finish().unwrap();
        assert_eq!(r.id.as_deref(), Some("r1"));
        assert_eq!(r.model, "served");
        assert_eq!(r.text(), "ab");
        assert_eq!(r.finish_reason, Some(FinishReason::ToolCalls));
        assert_eq!(r.raw_finish_reason.as_deref(), Some("tool_use"));
        let usage = r.usage.clone().unwrap();
        assert_eq!(usage.prompt_tokens, Some(10));
        assert_eq!(usage.completion_tokens, Some(5));
        assert_eq!(usage.total_tokens, Some(15));
        assert_eq!(usage.cached_tokens, Some(4));
        let calls = r.tool_calls();
        assert_eq!(calls.len(), 2);
        // Arrival order is kept: index 1 opened first.
        assert_eq!((calls[0].0, calls[0].1), ("c2", "two"));
        assert_eq!(calls[0].2, &Value::Object(Vec::new()));
        assert_eq!(calls[1].2.get("x").unwrap().as_i64(), Some(1));
    }

    #[test]
    fn accumulator_rejects_truncated_tool_arguments() {
        let mut acc = StreamAccumulator::new();
        acc.push(CompletionChunk::ToolCallDelta(ToolCallDelta {
            index: 0,
            id: Some("c".into()),
            name: Some("t".into()),
            arguments: "{\"x\":".into(),
        }));
        acc.push(CompletionChunk::done(FinishReason::Length));
        let err = acc.finish().unwrap_err().to_string();
        assert!(
            err.contains("not valid JSON") && err.contains("token limit"),
            "{err}"
        );
    }

    #[test]
    fn chunks_round_trip_through_the_accumulator() {
        let original = CompletionResponse::new(
            "m",
            Message {
                role: Role::Assistant,
                id: None,
                parts: vec![
                    MessagePart::Text("hi".into()),
                    MessagePart::ToolCall {
                        id: "c1".into(),
                        name: "t".into(),
                        arguments: Value::Object(vec![("k".into(), Value::Int(1))]),
                    },
                ],
            },
        )
        .with_raw_finish_reason("tool_calls")
        .with_usage(Usage::new(3, 4))
        .with_id("r");
        let mut acc = StreamAccumulator::new();
        for chunk in response_to_chunks(&original) {
            acc.push(chunk);
        }
        assert_eq!(acc.finish().unwrap(), original);
    }

    #[test]
    fn finish_reason_maps_provider_strings() {
        assert_eq!(FinishReason::from_provider("end_turn"), FinishReason::Stop);
        assert_eq!(
            FinishReason::from_provider("max_tokens"),
            FinishReason::Length
        );
        assert_eq!(
            FinishReason::from_provider("tool_use"),
            FinishReason::ToolCalls
        );
        assert_eq!(
            FinishReason::from_provider("refusal"),
            FinishReason::ContentFilter
        );
        assert_eq!(
            FinishReason::from_provider("pause_turn"),
            FinishReason::Other
        );
    }
}

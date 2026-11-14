//! Ollama provider (`/api/chat`, `/api/embed`).

use std::fmt;

use ferroly::codec::{json, Value};
use ferroly::hash::base64;

use super::common::{
    f32_num, merge_custom, num_f32, obj, s, stream_failure, tool_result_text, u32_field, ErrorHint,
    HttpCore, ProviderFailure,
};
use super::sse::{Framing, StreamParser};
use super::ProviderOptions;
use ferroly::genai::embedder::embed_in_batches;
use ferroly::genai::observe::Operation;
use ferroly::genai::provider::{BoxFuture, ChunkStream, GenAiProvider};
use ferroly::genai::{
    Capability, CompletionChunk, CompletionRequest, CompletionResponse, EmbedRequest,
    EmbedResponse, Embedder, FinishReason, GenAiError, Message, MessagePart, ModelInfo,
    ResponseFormat, Role, ToolCallDelta, Usage,
};

const PROVIDER: &str = "ollama";
const DEFAULT_BASE: &str = "http://localhost:11434";
const DEFAULT_EMBED_BATCH: usize = 128;
/// Body keys the provider builds itself; `Options::custom` may not set them.
const OWNED_KEYS: &[&str] = &["model", "messages", "stream"];

/// A [`GenAiProvider`] backed by a local (or remote) Ollama server.
///
/// Ollama needs no credentials by default; for a server behind an
/// authenticating proxy, set [`ProviderOptions::auth`].
pub struct OllamaProvider {
    core: HttpCore,
    base_url: String,
}

impl fmt::Debug for OllamaProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OllamaProvider")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl OllamaProvider {
    /// Creates a provider pointing at `http://localhost:11434` by default.
    pub fn new(opts: Option<ProviderOptions>) -> Self {
        let opts = opts.unwrap_or_default();
        let base_url = opts
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE.to_string());
        Self {
            core: HttpCore::new(PROVIDER, &opts, None, None),
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/api/chat", self.base_url)
    }

    async fn embed_once(&self, request: EmbedRequest) -> Result<EmbedResponse, GenAiError> {
        let mut call = self.core.begin(&request.model, Operation::Embed, || {
            self.model_info_for(&request.model)
        });
        let result = async {
            // POST /api/embed { model, input: [..] } -> { model, embeddings }.
            let mut fields = vec![
                ("model".to_string(), s(&request.model)),
                (
                    "input".to_string(),
                    Value::Array(request.input.iter().map(|t| s(t)).collect()),
                ),
            ];
            if let Some(d) = request.dimensions {
                fields.push(("dimensions".to_string(), d.into()));
            }
            let resp = self
                .core
                .post(
                    &format!("{}/api/embed", self.base_url),
                    &Value::Object(fields),
                    &[],
                    &call,
                    parse_error,
                )
                .await?;
            let value = self.core.read_json(resp, &call).await?;
            parse_embeddings(&request.model, &value)
        }
        .await;
        call.end_embed(&result);
        result
    }
}

impl GenAiProvider for OllamaProvider {
    fn name(&self) -> &str {
        PROVIDER
    }

    fn description(&self) -> &str {
        "Local Ollama chat provider"
    }

    fn supports(&self, capability: Capability) -> bool {
        matches!(
            capability,
            Capability::Text
                | Capability::Chat
                | Capability::Streaming
                | Capability::JsonMode
                | Capability::ToolUse
                | Capability::Vision
                | Capability::Embeddings
        )
    }

    fn model_catalog(&self) -> Vec<ModelInfo> {
        use Capability::{Chat, Embeddings, JsonMode, Streaming, Text};
        // Local models: zero marginal cost. Which models exist depends on what
        // the server has pulled, so these two entries are placeholders to
        // override; limits are typical defaults.
        vec![
            ModelInfo::new(PROVIDER, "llama3")
                .display_name("Llama 3 (local)")
                .capabilities([Text, Chat, Streaming, JsonMode])
                .limits(8_192, 4_096)
                .cost(0.0, 0.0)
                .ranks(30, 50),
            ModelInfo::new(PROVIDER, "nomic-embed-text")
                .display_name("nomic-embed-text (local)")
                .capabilities([Embeddings])
                .limits(8_192, 0)
                .cost(0.0, 0.0),
        ]
    }

    fn complete(
        &self,
        request: CompletionRequest,
    ) -> BoxFuture<'_, Result<CompletionResponse, GenAiError>> {
        Box::pin(async move {
            let mut call = self.core.begin(&request.model, Operation::Chat, || {
                self.model_info_for(&request.model)
            });
            let result = async {
                let body = build_body(&request, false)?;
                let resp = self
                    .core
                    .post(&self.endpoint(), &body, &[], &call, parse_error)
                    .await?;
                let value = self.core.read_json(resp, &call).await?;
                parse_response(&value)
            }
            .await;
            call.end_response(&result);
            result
        })
    }

    fn complete_stream(
        &self,
        request: CompletionRequest,
    ) -> BoxFuture<'_, Result<ChunkStream, GenAiError>> {
        Box::pin(async move {
            let mut call = self.core.begin(&request.model, Operation::ChatStream, || {
                self.model_info_for(&request.model)
            });
            let established = async {
                let body = build_body(&request, true)?;
                self.core
                    .post(&self.endpoint(), &body, &[], &call, parse_error)
                    .await
            }
            .await;
            match established {
                // Ollama streams newline-delimited JSON objects (not SSE).
                Ok(resp) => {
                    Ok(self
                        .core
                        .spawn_stream(resp, OllamaStream::default(), Framing::NdJson, call))
                }
                Err(e) => {
                    call.end_err(&e);
                    Err(e)
                }
            }
        })
    }
}

impl Embedder for OllamaProvider {
    fn embed(&self, request: EmbedRequest) -> BoxFuture<'_, Result<EmbedResponse, GenAiError>> {
        let batch = self.core.embed_batch_size.unwrap_or(DEFAULT_EMBED_BATCH);
        Box::pin(embed_in_batches(request, batch, move |part| {
            Box::pin(self.embed_once(part))
        }))
    }
}

/// Parses Ollama's error body: `{"error": "message"}`.
fn parse_error(status: u16, body: &str) -> ProviderFailure {
    let message = json::from_str(body)
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| body.to_string());
    ProviderFailure {
        status,
        hint: ErrorHint::None,
        error_type: None,
        message,
    }
}

/// Parses Ollama's `{ model, embeddings: [[..], ..], prompt_eval_count }`
/// response.
fn parse_embeddings(model: &str, value: &Value) -> Result<EmbedResponse, GenAiError> {
    let rows = value
        .get("embeddings")
        .and_then(Value::as_array)
        .ok_or_else(|| GenAiError::ResponseParse("missing 'embeddings' array".into()))?;
    let mut embeddings = Vec::with_capacity(rows.len());
    for row in rows {
        let vec = row
            .as_array()
            .ok_or_else(|| GenAiError::ResponseParse("embedding is not an array".into()))?
            .iter()
            .map(num_f32)
            .collect::<Option<Vec<f32>>>()
            .ok_or_else(|| GenAiError::ResponseParse("non-numeric embedding value".into()))?;
        embeddings.push(vec);
    }
    let usage = u32_field(value, "prompt_eval_count").map(|n| Usage {
        prompt_tokens: Some(n),
        total_tokens: Some(n),
        ..Usage::default()
    });
    Ok(EmbedResponse {
        model: value
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(model)
            .to_string(),
        embeddings,
        usage,
    })
}

/// Builds the JSON request body. Pure and unit-tested.
fn build_body(request: &CompletionRequest, stream: bool) -> Result<Value, GenAiError> {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(sys) = &request.options.system_instructions {
        messages.push(obj([("role", s("system")), ("content", s(sys))]));
    }
    for (position, message) in request.messages.iter().enumerate() {
        encode_message(message, &request.messages[..position], &mut messages);
    }

    let o = &request.options;
    let mut fields: Vec<(String, Value)> = vec![
        ("model".into(), s(&request.model)),
        ("messages".into(), Value::Array(messages)),
        ("stream".into(), stream.into()),
    ];
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                obj([
                    ("type", s("function")),
                    (
                        "function",
                        obj([
                            ("name", s(&t.name)),
                            ("description", s(&t.description)),
                            ("parameters", t.parameters.clone()),
                        ]),
                    ),
                ])
            })
            .collect();
        fields.push(("tools".into(), Value::Array(tools)));
    }
    match &request.response_format {
        Some(ResponseFormat::Json) => fields.push(("format".into(), s("json"))),
        Some(format) => {
            if let Some((_name, _description, schema, _strict)) = format.schema_parts() {
                fields.push(("format".into(), schema.clone()));
            }
        }
        None => {}
    }

    let mut options: Vec<(String, Value)> = Vec::new();
    if let Some(t) = o.temperature {
        options.push(("temperature".into(), f32_num(t)));
    }
    if let Some(p) = o.top_p {
        options.push(("top_p".into(), f32_num(p)));
    }
    if let Some(k) = o.top_k {
        options.push(("top_k".into(), k.into()));
    }
    if let Some(m) = o.max_tokens {
        options.push(("num_predict".into(), m.into()));
    }
    if let Some(seed) = o.seed {
        options.push(("seed".into(), seed.into()));
    }
    if !o.stop.is_empty() {
        options.push((
            "stop".into(),
            Value::Array(o.stop.iter().map(|t| s(t)).collect()),
        ));
    }
    if let Some(p) = o.presence_penalty {
        options.push(("presence_penalty".into(), f32_num(p)));
    }
    if let Some(p) = o.frequency_penalty {
        options.push(("frequency_penalty".into(), f32_num(p)));
    }
    if !options.is_empty() {
        fields.push(("options".into(), Value::Object(options)));
    }
    merge_custom(&mut fields, &o.custom, OWNED_KEYS, PROVIDER)?;
    Ok(Value::Object(fields))
}

/// The name of the tool that call `call_id` invoked, looked up in the
/// conversation so far. Ollama matches a tool result to its call by name.
fn tool_name_for<'a>(call_id: &str, earlier: &'a [Message]) -> Option<&'a str> {
    earlier.iter().rev().find_map(|m| {
        m.parts.iter().find_map(|p| match p {
            MessagePart::ToolCall { id, name, .. } if id == call_id => Some(name.as_str()),
            _ => None,
        })
    })
}

/// Encodes one message in Ollama's chat shape:
///
/// - every tool result becomes `{role: "tool", content, tool_name}`;
/// - an assistant message with tool calls carries
///   `tool_calls: [{function: {name, arguments: <object>}}]`;
/// - images go in `images` as base64 strings.
fn encode_message(msg: &Message, earlier: &[Message], out: &mut Vec<Value>) {
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut images: Vec<Value> = Vec::new();
    let mut has_result = false;
    for part in &msg.parts {
        match part {
            MessagePart::Image { data, .. } => images.push(s(&base64::encode(data))),
            MessagePart::ToolCall {
                name, arguments, ..
            } => {
                let arguments = match arguments {
                    Value::Object(_) => arguments.clone(),
                    _ => Value::Object(Vec::new()),
                };
                tool_calls.push(obj([(
                    "function",
                    obj([("name", s(name)), ("arguments", arguments)]),
                )]));
            }
            MessagePart::ToolResult { call_id, result } => {
                has_result = true;
                let mut fields = vec![
                    ("role".to_string(), s("tool")),
                    ("content".to_string(), s(&tool_result_text(result))),
                ];
                if let Some(name) = tool_name_for(call_id, earlier) {
                    fields.push(("tool_name".to_string(), s(name)));
                }
                out.push(Value::Object(fields));
            }
            _ => {}
        }
    }
    let text = msg.text_content();
    if has_result && text.is_empty() && tool_calls.is_empty() && images.is_empty() {
        return;
    }
    let role = if has_result && msg.role == Role::Tool {
        "user"
    } else {
        msg.role.as_str()
    };
    let mut fields = vec![
        ("role".to_string(), s(role)),
        ("content".to_string(), s(&text)),
    ];
    if !images.is_empty() {
        fields.push(("images".to_string(), Value::Array(images)));
    }
    if !tool_calls.is_empty() {
        fields.push(("tool_calls".to_string(), Value::Array(tool_calls)));
    }
    out.push(Value::Object(fields));
}

/// Reads the tool calls of an Ollama message as `(id, name, arguments)`.
/// Ollama may not assign ids; `call_<n>` (counting from `offset`) is used
/// then, so results can still be matched to calls.
fn tool_calls_of(message: Option<&Value>, offset: u32) -> Vec<(String, String, Value)> {
    let Some(calls) = message
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    calls
        .iter()
        .enumerate()
        .map(|(i, call)| {
            let func = call.get("function");
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("call_{}", offset + i as u32));
            let name = func
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let arguments = match func.and_then(|f| f.get("arguments")) {
                Some(Value::Object(o)) => Value::Object(o.clone()),
                // Tolerate servers that send the arguments as a JSON string.
                Some(Value::Str(text)) => {
                    json::from_str(text).unwrap_or_else(|_| Value::Object(Vec::new()))
                }
                _ => Value::Object(Vec::new()),
            };
            (id, name, arguments)
        })
        .collect()
}

fn parse_usage(value: &Value) -> Option<Usage> {
    let prompt = u32_field(value, "prompt_eval_count");
    let completion = u32_field(value, "eval_count");
    if prompt.is_none() && completion.is_none() {
        return None;
    }
    let mut usage = Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        ..Usage::default()
    };
    usage.fill_total();
    Some(usage)
}

/// The normalized and raw finish reason of a final (`done: true`) object.
fn finish_of(value: &Value, has_tool_calls: bool) -> Option<(FinishReason, String)> {
    let done = value.get("done").and_then(Value::as_bool).unwrap_or(false);
    let raw = value.get("done_reason").and_then(Value::as_str);
    if !done && raw.is_none() {
        return None;
    }
    let raw = raw.unwrap_or("stop").to_string();
    // Ollama reports "stop" for a turn that ends in tool calls.
    let reason = if has_tool_calls && raw == "stop" {
        FinishReason::ToolCalls
    } else {
        FinishReason::from_provider(&raw)
    };
    Some((reason, raw))
}

/// Parses a non-streaming response body. Pure and unit-tested.
fn parse_response(value: &Value) -> Result<CompletionResponse, GenAiError> {
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return Err(stream_failure(ProviderFailure {
            status: 500,
            hint: ErrorHint::None,
            error_type: None,
            message: error.to_string(),
        }));
    }
    let message = value.get("message");
    let mut parts: Vec<MessagePart> = Vec::new();
    let content = message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let calls = tool_calls_of(message, 0);
    if !content.is_empty() || calls.is_empty() {
        parts.push(MessagePart::Text(content.to_string()));
    }
    let has_calls = !calls.is_empty();
    for (id, name, arguments) in calls {
        parts.push(MessagePart::ToolCall {
            id,
            name,
            arguments,
        });
    }
    let mut response = CompletionResponse::new(
        value.get("model").and_then(Value::as_str).unwrap_or(""),
        Message {
            role: Role::Assistant,
            id: None,
            parts,
        },
    );
    if let Some((reason, raw)) = finish_of(value, has_calls) {
        response.finish_reason = Some(reason);
        response.raw_finish_reason = Some(raw);
    }
    response.usage = parse_usage(value);
    response.reasoning = message
        .and_then(|m| m.get("thinking"))
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    Ok(response)
}

/// Stateful decoder of Ollama's NDJSON chat stream.
#[derive(Default)]
struct OllamaStream {
    started: bool,
    tool_calls: u32,
    done: bool,
}

impl StreamParser for OllamaStream {
    fn on_data(
        &mut self,
        _event: Option<&str>,
        data: &str,
        out: &mut Vec<Result<CompletionChunk, GenAiError>>,
    ) {
        let value = match json::from_str(data) {
            Ok(v) => v,
            Err(e) => {
                out.push(Err(GenAiError::ResponseParse(format!(
                    "stream line is not JSON: {e}"
                ))));
                return;
            }
        };
        // A failure after the stream started arrives as `{"error": "..."}`.
        if let Some(error) = value.get("error") {
            let message = error
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| json::to_string(error));
            out.push(Err(stream_failure(ProviderFailure {
                status: 500,
                hint: ErrorHint::None,
                error_type: None,
                message,
            })));
            return;
        }
        if !self.started {
            self.started = true;
            if let Some(model) = value.get("model").and_then(Value::as_str) {
                out.push(Ok(CompletionChunk::Start {
                    id: None,
                    model: Some(model.to_string()),
                }));
            }
        }
        let message = value.get("message");
        if let Some(thinking) = message
            .and_then(|m| m.get("thinking"))
            .and_then(Value::as_str)
        {
            if !thinking.is_empty() {
                out.push(Ok(CompletionChunk::ReasoningDelta(thinking.to_string())));
            }
        }
        if let Some(text) = message
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
        {
            if !text.is_empty() {
                out.push(Ok(CompletionChunk::TextDelta(text.to_string())));
            }
        }
        // Tool calls arrive whole, not as fragments.
        for (id, name, arguments) in tool_calls_of(message, self.tool_calls) {
            out.push(Ok(CompletionChunk::ToolCallDelta(ToolCallDelta {
                index: self.tool_calls,
                id: Some(id),
                name: Some(name),
                arguments: json::to_string(&arguments),
            })));
            self.tool_calls += 1;
        }
        if value.get("done").and_then(Value::as_bool).unwrap_or(false) {
            self.done = true;
            if let Some(usage) = parse_usage(&value) {
                out.push(Ok(CompletionChunk::Usage(usage)));
            }
            if let Some((finish_reason, raw)) = finish_of(&value, self.tool_calls > 0) {
                out.push(Ok(CompletionChunk::Done {
                    finish_reason,
                    raw_finish_reason: Some(raw),
                }));
            }
        }
    }

    fn is_complete(&self) -> bool {
        self.done
    }

    fn is_terminal(&self) -> bool {
        self.done
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroly::genai::Options;

    #[test]
    fn builds_body_with_options() {
        let req = CompletionRequest {
            model: "llama3".into(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            tool_choice: None,
            response_format: None,
            options: Options::builder().temperature(0.5).max_tokens(64).build(),
        };
        let body = build_body(&req, true).unwrap();
        assert_eq!(
            json::to_string(&body),
            r#"{"model":"llama3","messages":[{"role":"user","content":"hi"}],"stream":true,"options":{"temperature":0.5,"num_predict":64}}"#
        );
    }

    #[test]
    fn parses_response() {
        let value = json::from_str(
            r#"{"model":"llama3","message":{"role":"assistant","content":"hello"},"done":true,"done_reason":"length","prompt_eval_count":8,"eval_count":3}"#,
        )
        .unwrap();
        let resp = parse_response(&value).unwrap();
        assert_eq!(resp.text(), "hello");
        assert_eq!(resp.finish_reason, Some(FinishReason::Length));
        assert_eq!(resp.raw_finish_reason.as_deref(), Some("length"));
        let usage = resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, Some(8));
        assert_eq!(usage.total_tokens, Some(11));
    }

    #[test]
    fn stream_lines_carry_text_error_and_done() {
        let mut p = OllamaStream::default();
        let mut out = Vec::new();
        p.on_data(
            None,
            "{\"model\":\"llama3\",\"message\":{\"content\":\"He\"},\"done\":false}",
            &mut out,
        );
        assert!(!p.is_complete());
        p.on_data(
            None,
            "{\"message\":{\"content\":\"\"},\"done\":true,\"done_reason\":\"stop\",\"eval_count\":2}",
            &mut out,
        );
        assert!(p.is_complete());
        let chunks: Vec<CompletionChunk> = out.into_iter().map(Result::unwrap).collect();
        assert_eq!(chunks[1].text(), "He");
        assert_eq!(
            chunks.last().unwrap().finish_reason(),
            Some(FinishReason::Stop)
        );

        let mut out = Vec::new();
        OllamaStream::default().on_data(None, "{\"error\":\"model crashed\"}", &mut out);
        assert!(
            matches!(&out[0], Err(GenAiError::Api { status: 500, message }) if message == "model crashed")
        );
    }
}

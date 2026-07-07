//! OpenAI provider (Chat Completions API).

use std::sync::Arc;

use ferroly::clients::{AuthProvider, BearerAuth};
use ferroly::codec::{json, Value};
use ferroly::http::{Client, Method, Request};

use super::sse::{pump, sse_data};
use super::util::to_base64;
use super::ProviderOptions;
use ferroly::genai::provider::{BoxFuture, ChunkStream, GenAiProvider};
use ferroly::genai::{
    Capability, CompletionChunk, CompletionRequest, CompletionResponse, EmbedRequest,
    EmbedResponse, Embedder, GenAiError, Message, MessagePart, ModelInfo, ResponseFormat, Role,
    ToolChoice, Usage,
};

const DEFAULT_BASE: &str = "https://api.openai.com";

/// A [`GenAiProvider`] backed by the OpenAI Chat Completions API.
pub struct OpenAiProvider {
    client: Client,
    base_url: String,
    auth: Arc<dyn AuthProvider>,
}

impl OpenAiProvider {
    /// Creates a provider authenticated with a bearer API key.
    pub fn new(api_key: impl Into<String>, opts: Option<ProviderOptions>) -> Self {
        let base_url = opts
            .and_then(|o| o.base_url)
            .unwrap_or_else(|| DEFAULT_BASE.to_string());
        Self {
            client: Client::new(),
            base_url,
            auth: Arc::new(BearerAuth::new(api_key.into())),
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/chat/completions", self.base_url)
    }

    fn request(&self, body: &Value) -> Result<Request, GenAiError> {
        let mut req = Request::builder(Method::Post, &self.endpoint())
            .map_err(|e| GenAiError::Config(e.to_string()))?
            .header("content-type", "application/json")
            .body(json::to_string(body).into_bytes())
            .build();
        self.auth.apply(&mut req);
        Ok(req)
    }
}

impl GenAiProvider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai"
    }

    fn description(&self) -> &str {
        "OpenAI Chat Completions provider"
    }

    fn supports(&self, capability: Capability) -> bool {
        matches!(
            capability,
            Capability::Streaming | Capability::ToolUse | Capability::Vision | Capability::JsonMode
        )
    }

    fn model_catalog(&self) -> Vec<ModelInfo> {
        use Capability::{Chat, Embeddings, JsonMode, Streaming, Text, ToolUse, Vision};
        vec![
            ModelInfo::new("openai", "gpt-4o")
                .display_name("GPT-4o")
                .capabilities([Text, Chat, Streaming, Vision, ToolUse, JsonMode])
                .limits(128_000, 16_384)
                .cost(2.5, 10.0),
            ModelInfo::new("openai", "gpt-4o-mini")
                .display_name("GPT-4o mini")
                .capabilities([Text, Chat, Streaming, Vision, ToolUse, JsonMode])
                .limits(128_000, 16_384)
                .cost(0.15, 0.60),
            ModelInfo::new("openai", "text-embedding-3-small")
                .display_name("text-embedding-3-small")
                .capabilities([Embeddings])
                .limits(8_191, 0)
                .cost(0.02, 0.0),
        ]
    }

    fn complete(
        &self,
        request: CompletionRequest,
    ) -> BoxFuture<'_, Result<CompletionResponse, GenAiError>> {
        Box::pin(async move {
            let body = build_body(&request, false);
            let req = self.request(&body)?;
            let resp = self
                .client
                .send(req)
                .await
                .map_err(|e| GenAiError::Transport(e.to_string()))?;
            let status = resp.status();
            let text = resp
                .text()
                .await
                .map_err(|e| GenAiError::Transport(e.to_string()))?;
            if !status.is_success() {
                return Err(GenAiError::Api {
                    status: status.as_u16(),
                    message: text,
                });
            }
            let value =
                json::from_str(&text).map_err(|e| GenAiError::ResponseParse(e.to_string()))?;
            parse_response(&value)
        })
    }

    fn complete_stream(
        &self,
        request: CompletionRequest,
    ) -> BoxFuture<'_, Result<ChunkStream, GenAiError>> {
        Box::pin(async move {
            let body = build_body(&request, true);
            let req = self.request(&body)?;
            let resp = self
                .client
                .send(req)
                .await
                .map_err(|e| GenAiError::Transport(e.to_string()))?;
            let status = resp.status();
            if !status.is_success() {
                let message = resp.text().await.unwrap_or_default();
                return Err(GenAiError::Api {
                    status: status.as_u16(),
                    message,
                });
            }
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            tokio::spawn(pump(resp, tx, parse_sse_line));
            Ok(rx)
        })
    }
}

impl Embedder for OpenAiProvider {
    fn embed(&self, request: EmbedRequest) -> BoxFuture<'_, Result<EmbedResponse, GenAiError>> {
        Box::pin(async move {
            let body = Value::Object(vec![
                ("model".into(), request.model.clone().into()),
                (
                    "input".into(),
                    Value::Array(
                        request
                            .input
                            .iter()
                            .map(|s| Value::Str(s.clone()))
                            .collect(),
                    ),
                ),
            ]);
            let endpoint = format!("{}/v1/embeddings", self.base_url);
            let mut req = Request::builder(Method::Post, &endpoint)
                .map_err(|e| GenAiError::Config(e.to_string()))?
                .header("content-type", "application/json")
                .body(json::to_string(&body).into_bytes())
                .build();
            self.auth.apply(&mut req);
            let resp = self
                .client
                .send(req)
                .await
                .map_err(|e| GenAiError::Transport(e.to_string()))?;
            let status = resp.status();
            let text = resp
                .text()
                .await
                .map_err(|e| GenAiError::Transport(e.to_string()))?;
            if !status.is_success() {
                return Err(GenAiError::Api {
                    status: status.as_u16(),
                    message: text,
                });
            }
            let value =
                json::from_str(&text).map_err(|e| GenAiError::ResponseParse(e.to_string()))?;
            parse_embeddings(&request.model, &value)
        })
    }
}

/// Parses OpenAI's `{ data: [{ embedding: [..] }], usage }` embeddings response.
fn parse_embeddings(model: &str, value: &Value) -> Result<EmbedResponse, GenAiError> {
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| GenAiError::ResponseParse("missing 'data' array".into()))?;
    let mut embeddings = Vec::with_capacity(data.len());
    for item in data {
        let vec = item
            .get("embedding")
            .and_then(Value::as_array)
            .ok_or_else(|| GenAiError::ResponseParse("missing 'embedding'".into()))?
            .iter()
            .map(num_f32)
            .collect::<Option<Vec<f32>>>()
            .ok_or_else(|| GenAiError::ResponseParse("non-numeric embedding value".into()))?;
        embeddings.push(vec);
    }
    let usage = value.get("usage").map(|u| Usage {
        prompt_tokens: u
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        completion_tokens: None,
        total_tokens: u
            .get("total_tokens")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
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

/// Coerces a JSON number (float or int) to `f32`.
fn num_f32(v: &Value) -> Option<f32> {
    v.as_f64()
        .or_else(|| v.as_i64().map(|i| i as f64))
        .or_else(|| v.as_u64().map(|u| u as f64))
        .map(|f| f as f32)
}

/// Builds the JSON request body as a [`Value`]. Pure and unit-tested.
fn build_body(request: &CompletionRequest, stream: bool) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(sys) = &request.options.system_instructions {
        messages.push(Value::Object(vec![
            ("role".into(), "system".into()),
            ("content".into(), sys.clone().into()),
        ]));
    }
    messages.extend(request.messages.iter().map(encode_message));

    let mut fields: Vec<(String, Value)> = vec![
        ("model".into(), request.model.clone().into()),
        ("messages".into(), Value::Array(messages)),
        ("stream".into(), stream.into()),
    ];
    if let Some(t) = request.options.temperature {
        fields.push(("temperature".into(), (t as f64).into()));
    }
    if let Some(m) = request.options.max_tokens {
        fields.push(("max_tokens".into(), m.into()));
    }
    if let Some(p) = request.options.top_p {
        fields.push(("top_p".into(), (p as f64).into()));
    }
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                Value::Object(vec![
                    ("type".into(), "function".into()),
                    (
                        "function".into(),
                        Value::Object(vec![
                            ("name".into(), t.name.clone().into()),
                            ("description".into(), t.description.clone().into()),
                            ("parameters".into(), t.parameters.clone()),
                        ]),
                    ),
                ])
            })
            .collect();
        fields.push(("tools".into(), Value::Array(tools)));
    }
    if let Some(choice) = &request.tool_choice {
        // OpenAI: "auto" | "none" | "required" | {type:function, function:{name}}.
        let v = match choice {
            ToolChoice::Auto => "auto".into(),
            ToolChoice::None => "none".into(),
            ToolChoice::Required => "required".into(),
            ToolChoice::Named(name) => Value::Object(vec![
                ("type".into(), "function".into()),
                (
                    "function".into(),
                    Value::Object(vec![("name".into(), name.clone().into())]),
                ),
            ]),
        };
        fields.push(("tool_choice".into(), v));
    }
    match &request.response_format {
        Some(ResponseFormat::Json) => fields.push((
            "response_format".into(),
            Value::Object(vec![("type".into(), "json_object".into())]),
        )),
        Some(ResponseFormat::JsonSchema(schema)) => fields.push((
            "response_format".into(),
            Value::Object(vec![
                ("type".into(), "json_schema".into()),
                (
                    "json_schema".into(),
                    Value::Object(vec![
                        ("name".into(), "response".into()),
                        ("schema".into(), schema.clone()),
                    ]),
                ),
            ]),
        )),
        _ => {}
    }
    Value::Object(fields)
}

fn encode_message(msg: &Message) -> Value {
    let all_text = msg.parts.iter().all(|p| matches!(p, MessagePart::Text(_)));
    if all_text {
        return Value::Object(vec![
            ("role".into(), msg.role.as_str().into()),
            ("content".into(), msg.text_content().into()),
        ]);
    }
    let content: Vec<Value> = msg
        .parts
        .iter()
        .filter_map(|p| match p {
            MessagePart::Text(t) => Some(Value::Object(vec![
                ("type".into(), "text".into()),
                ("text".into(), t.clone().into()),
            ])),
            MessagePart::Image { data, mime_type } => {
                let url = format!("data:{mime_type};base64,{}", to_base64(data));
                Some(Value::Object(vec![
                    ("type".into(), "image_url".into()),
                    (
                        "image_url".into(),
                        Value::Object(vec![("url".into(), url.into())]),
                    ),
                ]))
            }
            MessagePart::FileRef { uri, .. } => Some(Value::Object(vec![
                ("type".into(), "image_url".into()),
                (
                    "image_url".into(),
                    Value::Object(vec![("url".into(), uri.clone().into())]),
                ),
            ])),
            _ => None,
        })
        .collect();
    Value::Object(vec![
        ("role".into(), msg.role.as_str().into()),
        ("content".into(), Value::Array(content)),
    ])
}

/// Parses a non-streaming response body. Pure and unit-tested.
fn parse_response(value: &Value) -> Result<CompletionResponse, GenAiError> {
    let model = value
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let choice = value
        .get("choices")
        .and_then(|c| c.get_index(0))
        .ok_or_else(|| GenAiError::ResponseParse("missing choices".into()))?;

    let msg = choice.get("message");
    let mut parts: Vec<MessagePart> = Vec::new();
    if let Some(text) = msg.and_then(|m| m.get("content")).and_then(Value::as_str) {
        if !text.is_empty() {
            parts.push(MessagePart::Text(text.to_string()));
        }
    }
    if let Some(calls) = msg
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array)
    {
        for call in calls {
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let func = call.get("function");
            let name = func
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let arguments = func
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .and_then(|s| json::from_str(s).ok())
                .unwrap_or(Value::Null);
            parts.push(MessagePart::ToolCall {
                id,
                name,
                arguments,
            });
        }
    }

    let message = Message {
        role: Role::Assistant,
        id: None,
        parts,
    };
    let finish_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .map(str::to_string);
    let usage = value.get("usage").map(|u| Usage {
        prompt_tokens: u
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        completion_tokens: u
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        total_tokens: u
            .get("total_tokens")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
    });
    Ok(CompletionResponse {
        model,
        message,
        finish_reason,
        usage,
    })
}

/// Parses one SSE line into a chunk, or `None` to skip. Pure and unit-tested.
fn parse_sse_line(line: &str) -> Option<Result<CompletionChunk, GenAiError>> {
    let data = sse_data(line)?;
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    let value = match json::from_str(data) {
        Ok(v) => v,
        Err(e) => return Some(Err(GenAiError::ResponseParse(e.to_string()))),
    };
    let choice = value.get("choices").and_then(|c| c.get_index(0));
    let delta = choice
        .and_then(|c| c.get("delta"))
        .and_then(|d| d.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let finish_reason = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(Ok(CompletionChunk {
        delta,
        finish_reason,
        usage: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroly::genai::{Options, Role};

    #[test]
    fn builds_body_with_options_and_system() {
        let req = CompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![Message::text(Role::User, "1", "hi")],
            tools: vec![],
            tool_choice: None,
            response_format: Some(ResponseFormat::Json),
            options: Options::builder()
                .temperature(0.5)
                .max_tokens(100)
                .system_instructions("sys")
                .build(),
        };
        let body = build_body(&req, false);
        assert_eq!(body.get("model").unwrap().as_str(), Some("gpt-4o"));
        assert_eq!(body.get("stream").unwrap().as_bool(), Some(false));
        assert_eq!(body.get("temperature").unwrap().as_f64(), Some(0.5));
        assert_eq!(body.get("max_tokens").unwrap().as_u64(), Some(100));
        assert_eq!(
            body.get("response_format")
                .unwrap()
                .get("type")
                .unwrap()
                .as_str(),
            Some("json_object")
        );
        let messages = body.get("messages").unwrap().as_array().unwrap();
        assert_eq!(messages[0].get("role").unwrap().as_str(), Some("system"));
        assert_eq!(messages[1].get("role").unwrap().as_str(), Some("user"));
        assert_eq!(messages[1].get("content").unwrap().as_str(), Some("hi"));
    }

    #[test]
    fn parses_response_text_and_usage() {
        let value = json::from_str(
            r#"{"model":"gpt-4o","choices":[{"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}"#,
        )
        .unwrap();
        let resp = parse_response(&value).unwrap();
        assert_eq!(resp.text(), "hello");
        assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
        assert_eq!(resp.usage.unwrap().total_tokens, Some(4));
    }

    #[test]
    fn parses_tool_call_response() {
        let value = json::from_str(
            r#"{"model":"gpt-4o","choices":[{"message":{"role":"assistant","tool_calls":[{"id":"call_1","function":{"name":"get_weather","arguments":"{\"city\":\"NYC\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        )
        .unwrap();
        let resp = parse_response(&value).unwrap();
        match &resp.message.parts[0] {
            MessagePart::ToolCall {
                id,
                name,
                arguments,
            } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "get_weather");
                assert_eq!(arguments.get("city").unwrap().as_str(), Some("NYC"));
            }
            other => panic!("expected tool call, got {other:?}"),
        }
    }

    #[test]
    fn parses_sse_delta_and_skips_done() {
        let chunk = parse_sse_line("data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}")
            .unwrap()
            .unwrap();
        assert_eq!(chunk.delta, "Hi");
        assert!(parse_sse_line("data: [DONE]").is_none());
        assert!(parse_sse_line(": comment").is_none());
    }
}

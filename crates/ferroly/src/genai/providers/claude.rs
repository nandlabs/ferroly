//! Anthropic Claude provider (Messages API).

use std::sync::Arc;

use ferroly::clients::{ApiKeyAuth, AuthProvider};
use ferroly::codec::{json, Value};
use ferroly::http::{Client, Method, Request};

use super::sse::{pump, sse_data};
use super::util::to_base64;
use super::ProviderOptions;
use ferroly::genai::provider::{BoxFuture, ChunkStream, GenAiProvider};
use ferroly::genai::{
    Capability, CompletionChunk, CompletionRequest, CompletionResponse, GenAiError, Message,
    MessagePart, ModelInfo, Role, ToolChoice, Usage,
};

const DEFAULT_BASE: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 1024;

/// Configuration for constructing a [`ClaudeProvider`] with a custom auth scheme.
pub struct ClaudeProviderConfig {
    /// The auth provider to apply (e.g. a Vault-backed credential source).
    pub auth: Arc<dyn AuthProvider>,
}

/// A [`GenAiProvider`] backed by the Anthropic Messages API.
pub struct ClaudeProvider {
    client: Client,
    base_url: String,
    auth: Arc<dyn AuthProvider>,
}

impl ClaudeProvider {
    /// Creates a provider authenticated with an `x-api-key` header.
    pub fn new(api_key: impl Into<String>, opts: Option<ProviderOptions>) -> Self {
        Self::with_config(
            ClaudeProviderConfig {
                auth: Arc::new(ApiKeyAuth::new("x-api-key", api_key.into())),
            },
            opts,
        )
    }

    /// Creates a provider with a caller-supplied auth provider.
    pub fn with_config(config: ClaudeProviderConfig, opts: Option<ProviderOptions>) -> Self {
        let base_url = opts
            .and_then(|o| o.base_url)
            .unwrap_or_else(|| DEFAULT_BASE.to_string());
        Self {
            client: Client::new(),
            base_url,
            auth: config.auth,
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/messages", self.base_url)
    }

    fn request(&self, body: &Value) -> Result<Request, GenAiError> {
        let mut req = Request::builder(Method::Post, &self.endpoint())
            .map_err(|e| GenAiError::Config(e.to_string()))?
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .body(json::to_string(body).into_bytes())
            .build();
        self.auth.apply(&mut req);
        Ok(req)
    }
}

impl GenAiProvider for ClaudeProvider {
    fn name(&self) -> &str {
        "claude"
    }

    fn description(&self) -> &str {
        "Anthropic Claude Messages provider"
    }

    fn supports(&self, capability: Capability) -> bool {
        matches!(
            capability,
            Capability::Streaming | Capability::ToolUse | Capability::Vision
        )
    }

    fn model_catalog(&self) -> Vec<ModelInfo> {
        use Capability::{Chat, Reasoning, Streaming, Text, ToolUse, Vision};
        vec![
            ModelInfo::new("claude", "claude-sonnet-5")
                .display_name("Claude Sonnet 5")
                .capabilities([Text, Chat, Streaming, Vision, ToolUse, Reasoning])
                .limits(200_000, 64_000)
                .cost(3.0, 15.0),
            ModelInfo::new("claude", "claude-3-5-haiku")
                .display_name("Claude 3.5 Haiku")
                .capabilities([Text, Chat, Streaming, ToolUse])
                .limits(200_000, 8_192)
                .cost(0.8, 4.0),
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

/// Builds the JSON request body as a [`Value`]. Pure and unit-tested.
fn build_body(request: &CompletionRequest, stream: bool) -> Value {
    let messages: Vec<Value> = request
        .messages
        .iter()
        .filter(|m| m.role != Role::System)
        .map(encode_message)
        .collect();

    let max_tokens = request.options.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    let mut fields: Vec<(String, Value)> = vec![
        ("model".into(), request.model.clone().into()),
        ("max_tokens".into(), max_tokens.into()),
        ("messages".into(), Value::Array(messages)),
        ("stream".into(), stream.into()),
    ];

    let system = request.options.system_instructions.clone().or_else(|| {
        let sys: String = request
            .messages
            .iter()
            .filter(|m| m.role == Role::System)
            .map(|m| m.text_content())
            .collect::<Vec<_>>()
            .join("\n");
        if sys.is_empty() {
            None
        } else {
            Some(sys)
        }
    });
    if let Some(sys) = system {
        fields.push(("system".into(), sys.into()));
    }
    if let Some(t) = request.options.temperature {
        fields.push(("temperature".into(), (t as f64).into()));
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
                    ("name".into(), t.name.clone().into()),
                    ("description".into(), t.description.clone().into()),
                    ("input_schema".into(), t.parameters.clone()),
                ])
            })
            .collect();
        fields.push(("tools".into(), Value::Array(tools)));
    }
    if let Some(choice) = &request.tool_choice {
        // Claude: {type:auto} | {type:any} (required) | {type:tool,name} | {type:none}.
        let v = match choice {
            ToolChoice::Auto => Value::Object(vec![("type".into(), "auto".into())]),
            ToolChoice::Required => Value::Object(vec![("type".into(), "any".into())]),
            ToolChoice::None => Value::Object(vec![("type".into(), "none".into())]),
            ToolChoice::Named(name) => Value::Object(vec![
                ("type".into(), "tool".into()),
                ("name".into(), name.clone().into()),
            ]),
        };
        fields.push(("tool_choice".into(), v));
    }
    Value::Object(fields)
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::Assistant => "assistant",
        _ => "user",
    }
}

fn encode_message(msg: &Message) -> Value {
    let content: Vec<Value> = msg
        .parts
        .iter()
        .map(|p| match p {
            MessagePart::Text(t) => Value::Object(vec![
                ("type".into(), "text".into()),
                ("text".into(), t.clone().into()),
            ]),
            MessagePart::Image { data, mime_type } => Value::Object(vec![
                ("type".into(), "image".into()),
                (
                    "source".into(),
                    Value::Object(vec![
                        ("type".into(), "base64".into()),
                        ("media_type".into(), mime_type.clone().into()),
                        ("data".into(), to_base64(data).into()),
                    ]),
                ),
            ]),
            MessagePart::FileRef { uri, .. } => Value::Object(vec![
                ("type".into(), "text".into()),
                ("text".into(), uri.clone().into()),
            ]),
            MessagePart::ToolCall {
                id,
                name,
                arguments,
            } => Value::Object(vec![
                ("type".into(), "tool_use".into()),
                ("id".into(), id.clone().into()),
                ("name".into(), name.clone().into()),
                ("input".into(), arguments.clone()),
            ]),
            MessagePart::ToolResult { call_id, result } => Value::Object(vec![
                ("type".into(), "tool_result".into()),
                ("tool_use_id".into(), call_id.clone().into()),
                ("content".into(), json::to_string(result).into()),
            ]),
        })
        .collect();
    Value::Object(vec![
        ("role".into(), role_str(msg.role).into()),
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
    let blocks = value
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| GenAiError::ResponseParse("missing content".into()))?;

    let mut parts: Vec<MessagePart> = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    parts.push(MessagePart::Text(t.to_string()));
                }
            }
            Some("tool_use") => parts.push(MessagePart::ToolCall {
                id: block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                name: block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                arguments: block.get("input").cloned().unwrap_or(Value::Null),
            }),
            _ => {}
        }
    }

    let message = Message {
        role: Role::Assistant,
        id: None,
        parts,
    };
    let finish_reason = value
        .get("stop_reason")
        .and_then(Value::as_str)
        .map(str::to_string);
    let usage = value.get("usage").map(|u| {
        let input = u
            .get("input_tokens")
            .and_then(Value::as_u64)
            .map(|v| v as u32);
        let output = u
            .get("output_tokens")
            .and_then(Value::as_u64)
            .map(|v| v as u32);
        Usage {
            prompt_tokens: input,
            completion_tokens: output,
            total_tokens: match (input, output) {
                (Some(i), Some(o)) => Some(i + o),
                _ => None,
            },
        }
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
    if data.is_empty() {
        return None;
    }
    let value = match json::from_str(data) {
        Ok(v) => v,
        Err(e) => return Some(Err(GenAiError::ResponseParse(e.to_string()))),
    };
    match value.get("type").and_then(Value::as_str) {
        Some("content_block_delta") => {
            let delta = value
                .get("delta")
                .and_then(|d| d.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Some(Ok(CompletionChunk {
                delta,
                finish_reason: None,
                usage: None,
            }))
        }
        Some("message_delta") => {
            let finish_reason = value
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(Value::as_str)
                .map(str::to_string);
            Some(Ok(CompletionChunk {
                delta: String::new(),
                finish_reason,
                usage: None,
            }))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroly::genai::Options;

    #[test]
    fn builds_body_with_system_and_defaults_max_tokens() {
        let req = CompletionRequest {
            model: "claude-sonnet-5".into(),
            messages: vec![
                Message::system("be helpful"),
                Message::text(Role::User, "1", "hello"),
            ],
            tools: vec![],
            tool_choice: None,
            response_format: None,
            options: Options::new(),
        };
        let body = build_body(&req, false);
        assert_eq!(body.get("model").unwrap().as_str(), Some("claude-sonnet-5"));
        assert_eq!(
            body.get("max_tokens").unwrap().as_u64(),
            Some(DEFAULT_MAX_TOKENS as u64)
        );
        assert_eq!(body.get("system").unwrap().as_str(), Some("be helpful"));
        let messages = body.get("messages").unwrap().as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].get("role").unwrap().as_str(), Some("user"));
        assert_eq!(
            messages[0]
                .get("content")
                .unwrap()
                .get_index(0)
                .unwrap()
                .get("text")
                .unwrap()
                .as_str(),
            Some("hello")
        );
    }

    #[test]
    fn parses_response_text_and_usage() {
        let value = json::from_str(
            r#"{"model":"claude-sonnet-5","content":[{"type":"text","text":"hi there"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}}"#,
        )
        .unwrap();
        let resp = parse_response(&value).unwrap();
        assert_eq!(resp.text(), "hi there");
        assert_eq!(resp.finish_reason.as_deref(), Some("end_turn"));
        assert_eq!(resp.usage.unwrap().total_tokens, Some(15));
    }

    #[test]
    fn parses_sse_content_delta() {
        let chunk = parse_sse_line(
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}",
        )
        .unwrap()
        .unwrap();
        assert_eq!(chunk.delta, "Hel");
        assert!(parse_sse_line("event: ping").is_none());
    }
}

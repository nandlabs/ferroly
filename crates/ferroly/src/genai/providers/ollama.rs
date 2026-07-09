//! Ollama provider (local `/api/chat`).

use ferroly::codec::{json, Value};
use ferroly::http::{Client, Method, Request};

use super::sse::pump;
use super::ProviderOptions;
use ferroly::genai::provider::{BoxFuture, ChunkStream, GenAiProvider};
use ferroly::genai::{
    Capability, CompletionChunk, CompletionRequest, CompletionResponse, EmbedRequest,
    EmbedResponse, Embedder, GenAiError, Message, MessagePart, ModelInfo, ResponseFormat, Role,
    Usage,
};

const DEFAULT_BASE: &str = "http://localhost:11434";

/// A [`GenAiProvider`] backed by a local (or remote) Ollama server.
pub struct OllamaProvider {
    client: Client,
    base_url: String,
}

impl OllamaProvider {
    /// Creates a provider pointing at `http://localhost:11434` by default.
    pub fn new(opts: Option<ProviderOptions>) -> Self {
        let base_url = opts
            .and_then(|o| o.base_url)
            .unwrap_or_else(|| DEFAULT_BASE.to_string());
        Self {
            client: Client::new(),
            base_url,
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/api/chat", self.base_url)
    }

    fn request(&self, body: &Value) -> Result<Request, GenAiError> {
        Ok(Request::builder(Method::Post, &self.endpoint())
            .map_err(|e| GenAiError::Config(e.to_string()))?
            .header("content-type", "application/json")
            .body(json::to_string(body).into_bytes())
            .build())
    }
}

impl GenAiProvider for OllamaProvider {
    fn name(&self) -> &str {
        "ollama"
    }

    fn description(&self) -> &str {
        "Local Ollama chat provider"
    }

    fn supports(&self, capability: Capability) -> bool {
        matches!(capability, Capability::Streaming | Capability::JsonMode)
    }

    fn model_catalog(&self) -> Vec<ModelInfo> {
        use Capability::{Chat, Embeddings, JsonMode, Streaming, Text};
        // Local models: zero marginal cost. Limits are typical defaults.
        vec![
            ModelInfo::new("ollama", "llama3")
                .display_name("Llama 3 (local)")
                .capabilities([Text, Chat, Streaming, JsonMode])
                .limits(8_192, 4_096)
                .cost(0.0, 0.0),
            ModelInfo::new("ollama", "nomic-embed-text")
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
            // Ollama streams newline-delimited JSON objects (not SSE).
            tokio::spawn(pump(resp, tx, |line| {
                if line.trim().is_empty() {
                    None
                } else {
                    Some(parse_ndjson_line(line))
                }
            }));
            Ok(rx)
        })
    }
}

impl Embedder for OllamaProvider {
    fn embed(&self, request: EmbedRequest) -> BoxFuture<'_, Result<EmbedResponse, GenAiError>> {
        Box::pin(async move {
            // Ollama's batch embeddings API: POST /api/embed { model, input:[..] }
            // -> { model, embeddings: [[..], ..] }.
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
            let endpoint = format!("{}/api/embed", self.base_url);
            let req = Request::builder(Method::Post, &endpoint)
                .map_err(|e| GenAiError::Config(e.to_string()))?
                .header("content-type", "application/json")
                .body(json::to_string(&body).into_bytes())
                .build();
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

/// Parses Ollama's `{ model, embeddings: [[..], ..] }` response.
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
    Ok(EmbedResponse {
        model: value
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(model)
            .to_string(),
        embeddings,
        usage: None,
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

    let mut options: Vec<(String, Value)> = Vec::new();
    if let Some(t) = request.options.temperature {
        options.push(("temperature".into(), (t as f64).into()));
    }
    if let Some(p) = request.options.top_p {
        options.push(("top_p".into(), (p as f64).into()));
    }
    if let Some(m) = request.options.max_tokens {
        options.push(("num_predict".into(), m.into()));
    }
    if !options.is_empty() {
        fields.push(("options".into(), Value::Object(options)));
    }
    if matches!(request.response_format, Some(ResponseFormat::Json)) {
        fields.push(("format".into(), "json".into()));
    }
    Value::Object(fields)
}

fn encode_message(msg: &Message) -> Value {
    Value::Object(vec![
        ("role".into(), msg.role.as_str().into()),
        ("content".into(), msg.text_content().into()),
    ])
}

/// Parses a non-streaming response body. Pure and unit-tested.
fn parse_response(value: &Value) -> Result<CompletionResponse, GenAiError> {
    let model = value
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let content = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let message = Message {
        role: Role::Assistant,
        id: None,
        parts: vec![MessagePart::Text(content)],
    };
    let finish_reason = value
        .get("done_reason")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            if value.get("done").and_then(Value::as_bool).unwrap_or(false) {
                Some("stop".to_string())
            } else {
                None
            }
        });
    let usage = Usage {
        prompt_tokens: value
            .get("prompt_eval_count")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        completion_tokens: value
            .get("eval_count")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        total_tokens: None,
    };
    Ok(CompletionResponse {
        model,
        message,
        finish_reason,
        usage: Some(usage),
    })
}

/// Parses one NDJSON stream line into a chunk. Pure and unit-tested.
fn parse_ndjson_line(line: &str) -> Result<CompletionChunk, GenAiError> {
    let value = json::from_str(line).map_err(|e| GenAiError::ResponseParse(e.to_string()))?;
    let delta = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let done = value.get("done").and_then(Value::as_bool).unwrap_or(false);
    Ok(CompletionChunk {
        delta,
        finish_reason: if done { Some("stop".into()) } else { None },
        usage: None,
    })
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
        let body = build_body(&req, true);
        assert_eq!(body.get("model").unwrap().as_str(), Some("llama3"));
        assert_eq!(body.get("stream").unwrap().as_bool(), Some(true));
        let options = body.get("options").unwrap();
        assert_eq!(options.get("temperature").unwrap().as_f64(), Some(0.5));
        assert_eq!(options.get("num_predict").unwrap().as_u64(), Some(64));
        assert_eq!(
            body.get("messages")
                .unwrap()
                .get_index(0)
                .unwrap()
                .get("content")
                .unwrap()
                .as_str(),
            Some("hi")
        );
    }

    #[test]
    fn parses_response() {
        let value = json::from_str(
            r#"{"model":"llama3","message":{"role":"assistant","content":"hello"},"done":true,"prompt_eval_count":8,"eval_count":3}"#,
        )
        .unwrap();
        let resp = parse_response(&value).unwrap();
        assert_eq!(resp.text(), "hello");
        assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
        assert_eq!(resp.usage.unwrap().prompt_tokens, Some(8));
    }

    #[test]
    fn parses_ndjson_stream_line() {
        let chunk = parse_ndjson_line("{\"message\":{\"content\":\"He\"},\"done\":false}").unwrap();
        assert_eq!(chunk.delta, "He");
        assert!(chunk.finish_reason.is_none());
        let last = parse_ndjson_line("{\"message\":{\"content\":\"\"},\"done\":true}").unwrap();
        assert_eq!(last.finish_reason.as_deref(), Some("stop"));
    }
}

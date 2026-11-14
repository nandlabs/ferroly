//! OpenAI provider (Chat Completions API), also usable against Azure OpenAI
//! and OpenAI-compatible servers.

use std::fmt;
use std::sync::Arc;

use ferroly::clients::{ApiKeyAuth, AuthProvider, BearerAuth, Secret};
use ferroly::codec::{json, Value};
use ferroly::hash::base64;
use ferroly::http::percent_encode;

use super::common::{
    f32_num, merge_custom, num_f32, obj, s, stream_failure, tool_result_text, u32_field, ErrorHint,
    HttpCore, ProviderFailure,
};
use super::sse::{Framing, StreamParser};
use super::ProviderOptions;
use ferroly::genai::embedder::embed_in_batches;
use ferroly::genai::observe::Operation;
use ferroly::genai::provider::{BoxFuture, ChunkStream, GenAiProvider};
use ferroly::genai::router::AsOf;
use ferroly::genai::{
    Capability, Citation, CompletionChunk, CompletionRequest, CompletionResponse, EmbedRequest,
    EmbedResponse, Embedder, FinishReason, GenAiError, Grounding, Message, MessagePart, ModelInfo,
    ReasoningEffort, ResponseFormat, Role, ToolCallDelta, ToolChoice, Usage,
};

const DEFAULT_BASE: &str = "https://api.openai.com";
/// The Embeddings API accepts at most this many inputs per request.
const DEFAULT_EMBED_BATCH: usize = 2048;
/// Body keys the provider builds itself; `Options::custom` may not set them.
const OWNED_KEYS: &[&str] = &["model", "messages", "stream"];

/// How an OpenAI-style endpoint expects the API key.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthStyle {
    /// `Authorization: Bearer <key>` — OpenAI and most compatible servers.
    Bearer,
    /// The key in a named header, e.g. `api-key` for Azure OpenAI.
    Header(String),
    /// No credentials (a local server).
    None,
}

/// Endpoint configuration for [`OpenAiProvider`]: where the Chat Completions
/// and Embeddings endpoints live and how to authenticate.
///
/// ```
/// use ferroly::genai::OpenAiConfig;
///
/// // api.openai.com
/// let openai = OpenAiConfig::new("sk-...");
///
/// // An OpenAI-compatible gateway with its own paths.
/// let gateway = OpenAiConfig::compatible("https://llm.internal", Some("token"))
///     .name("gateway")
///     .chat_path("/chat/completions")
///     .legacy_max_tokens(true);
///
/// // Azure OpenAI: one provider per deployment.
/// let azure = OpenAiConfig::azure(
///     "https://my-resource.openai.azure.com",
///     "my-gpt-4o-deployment",
///     "2024-10-21",
///     "azure-key",
/// );
/// # let _ = (openai, gateway, azure);
/// ```
#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    name: String,
    api_key: Option<Secret>,
    auth_style: AuthStyle,
    base_url: Option<String>,
    chat_path: String,
    embeddings_path: String,
    query: Vec<(String, String)>,
    legacy_max_tokens: bool,
    stream_usage: bool,
    catalog: Option<Vec<ModelInfo>>,
}

impl OpenAiConfig {
    /// The OpenAI API (`https://api.openai.com`) with a bearer API key.
    pub fn new(api_key: impl Into<Secret>) -> Self {
        OpenAiConfig {
            name: "openai".to_string(),
            api_key: Some(api_key.into()),
            auth_style: AuthStyle::Bearer,
            base_url: None,
            chat_path: "/v1/chat/completions".to_string(),
            embeddings_path: "/v1/embeddings".to_string(),
            query: Vec::new(),
            legacy_max_tokens: false,
            stream_usage: true,
            catalog: None,
        }
    }

    /// An OpenAI-compatible server at `base_url` (no trailing slash; the
    /// default paths `/v1/chat/completions` and `/v1/embeddings` are appended).
    /// The model catalog starts empty: add the served models with
    /// [`catalog`](Self::catalog) or through router configuration.
    pub fn compatible<K: Into<Secret>>(base_url: impl Into<String>, api_key: Option<K>) -> Self {
        let api_key: Option<Secret> = api_key.map(Into::into);
        let auth_style = if api_key.is_some() {
            AuthStyle::Bearer
        } else {
            AuthStyle::None
        };
        OpenAiConfig {
            api_key,
            auth_style,
            base_url: Some(base_url.into()),
            catalog: Some(Vec::new()),
            ..OpenAiConfig::new("")
        }
    }

    /// Azure OpenAI: requests go to
    /// `{endpoint}/openai/deployments/{deployment}/chat/completions?api-version={api_version}`
    /// (and `…/embeddings`) with the key in the `api-key` header.
    ///
    /// On Azure the deployment selects the model, so one provider serves one
    /// deployment and the request's `model` is informational. The provider is
    /// named `azure-openai`; its catalog starts empty.
    pub fn azure(
        endpoint: impl Into<String>,
        deployment: &str,
        api_version: impl Into<String>,
        api_key: impl Into<Secret>,
    ) -> Self {
        let deployment = percent_encode(deployment);
        OpenAiConfig {
            name: "azure-openai".to_string(),
            api_key: Some(api_key.into()),
            auth_style: AuthStyle::Header("api-key".to_string()),
            base_url: Some(endpoint.into()),
            chat_path: format!("/openai/deployments/{deployment}/chat/completions"),
            embeddings_path: format!("/openai/deployments/{deployment}/embeddings"),
            query: vec![("api-version".to_string(), api_version.into())],
            legacy_max_tokens: false,
            stream_usage: true,
            catalog: Some(Vec::new()),
        }
    }

    /// The name the provider registers under (default `openai`). Give each
    /// endpoint its own name when a router uses several.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// The API base URL (no trailing slash). `ProviderOptions::base_url`, when
    /// set, takes precedence.
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    /// How the API key is sent.
    pub fn auth_style(mut self, style: AuthStyle) -> Self {
        self.auth_style = style;
        self
    }

    /// The path of the chat-completions endpoint, appended to the base URL.
    pub fn chat_path(mut self, path: impl Into<String>) -> Self {
        self.chat_path = path.into();
        self
    }

    /// The path of the embeddings endpoint, appended to the base URL.
    pub fn embeddings_path(mut self, path: impl Into<String>) -> Self {
        self.embeddings_path = path.into();
        self
    }

    /// Adds a query parameter sent with every request (e.g. `api-version`).
    pub fn query_param(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.query.push((name.into(), value.into()));
        self
    }

    /// Sends the output-token limit as the legacy `max_tokens` instead of
    /// `max_completion_tokens`. OpenAI deprecated `max_tokens` and its
    /// reasoning models reject it, but many compatible servers only know the
    /// old name.
    pub fn legacy_max_tokens(mut self, legacy: bool) -> Self {
        self.legacy_max_tokens = legacy;
        self
    }

    /// Whether streamed requests ask for usage with
    /// `stream_options: {include_usage: true}` (default on). Turn it off for
    /// servers that reject the field.
    pub fn stream_usage(mut self, include: bool) -> Self {
        self.stream_usage = include;
        self
    }

    /// Replaces the model catalog this provider advertises to the router.
    /// Entries are re-labelled with the provider's name.
    pub fn catalog(mut self, models: Vec<ModelInfo>) -> Self {
        self.catalog = Some(models);
        self
    }
}

/// A [`GenAiProvider`] backed by the OpenAI Chat Completions API.
pub struct OpenAiProvider {
    core: HttpCore,
    base_url: String,
    config: OpenAiConfig,
}

impl fmt::Debug for OpenAiProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiProvider")
            .field("name", &self.config.name)
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl OpenAiProvider {
    /// Creates a provider for `https://api.openai.com` authenticated with a
    /// bearer API key.
    pub fn new(api_key: impl Into<String>, opts: Option<ProviderOptions>) -> Self {
        Self::with_config(OpenAiConfig::new(api_key.into()), opts)
    }

    /// Creates a provider from an endpoint configuration — Azure OpenAI, an
    /// OpenAI-compatible server, or OpenAI itself with non-default settings.
    pub fn with_config(config: OpenAiConfig, opts: Option<ProviderOptions>) -> Self {
        let opts = opts.unwrap_or_default();
        let base_url = opts
            .base_url
            .clone()
            .or_else(|| config.base_url.clone())
            .unwrap_or_else(|| DEFAULT_BASE.to_string());
        let key = config.api_key.as_ref().and_then(Secret::expose_str);
        let default_auth: Option<Arc<dyn AuthProvider>> = match (&config.auth_style, key) {
            (AuthStyle::Bearer, Some(key)) if !key.is_empty() => {
                Some(Arc::new(BearerAuth::new(key)))
            }
            (AuthStyle::Header(header), Some(key)) if !key.is_empty() => {
                Some(Arc::new(ApiKeyAuth::new(header.clone(), key)))
            }
            _ => None,
        };
        let core = HttpCore::new(&config.name, &opts, default_auth, config.api_key.clone());
        Self {
            core,
            base_url: base_url.trim_end_matches('/').to_string(),
            config,
        }
    }

    fn url(&self, path: &str) -> String {
        let mut url = format!("{}{}", self.base_url, path);
        for (i, (name, value)) in self.config.query.iter().enumerate() {
            url.push(if i == 0 { '?' } else { '&' });
            url.push_str(&percent_encode(name));
            url.push('=');
            url.push_str(&percent_encode(value));
        }
        url
    }

    async fn embed_once(&self, request: EmbedRequest) -> Result<EmbedResponse, GenAiError> {
        let mut call = self.core.begin(&request.model, Operation::Embed, || {
            self.model_info_for(&request.model)
        });
        let result = async {
            let body = build_embed_body(&request);
            let resp = self
                .core
                .post(
                    &self.url(&self.config.embeddings_path),
                    &body,
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

impl GenAiProvider for OpenAiProvider {
    fn name(&self) -> &str {
        self.core.name()
    }

    fn description(&self) -> &str {
        "OpenAI Chat Completions provider"
    }

    fn supports(&self, capability: Capability) -> bool {
        matches!(
            capability,
            Capability::Text
                | Capability::Chat
                | Capability::Streaming
                | Capability::ToolUse
                | Capability::Vision
                | Capability::JsonMode
                | Capability::Embeddings
        )
    }

    fn model_catalog(&self) -> Vec<ModelInfo> {
        let mut models = match &self.config.catalog {
            Some(models) => models.clone(),
            None => default_catalog(),
        };
        for model in &mut models {
            model.provider = self.config.name.clone();
        }
        models
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
                let body = build_body(&request, false, &self.config)?;
                let resp = self
                    .core
                    .post(
                        &self.url(&self.config.chat_path),
                        &body,
                        &[],
                        &call,
                        parse_error,
                    )
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
                let body = build_body(&request, true, &self.config)?;
                self.core
                    .post(
                        &self.url(&self.config.chat_path),
                        &body,
                        &[],
                        &call,
                        parse_error,
                    )
                    .await
            }
            .await;
            match established {
                Ok(resp) => {
                    Ok(self
                        .core
                        .spawn_stream(resp, OpenAiStream::default(), Framing::Sse, call))
                }
                Err(e) => {
                    call.end_err(&e);
                    Err(e)
                }
            }
        })
    }
}

impl Embedder for OpenAiProvider {
    fn embed(&self, request: EmbedRequest) -> BoxFuture<'_, Result<EmbedResponse, GenAiError>> {
        let batch = self.core.embed_batch_size.unwrap_or(DEFAULT_EMBED_BATCH);
        Box::pin(embed_in_batches(request, batch, move |part| {
            Box::pin(self.embed_once(part))
        }))
    }
}

/// The compiled-in OpenAI catalog. Ids, limits and prices are **defaults to
/// override**: they were correct on the `as_of` date and are not updated
/// between releases.
fn default_catalog() -> Vec<ModelInfo> {
    use Capability::{Chat, Embeddings, JsonMode, Streaming, Text, ToolUse, Vision};
    let as_of = AsOf::new(2024, 10, 1);
    vec![
        ModelInfo::new("openai", "gpt-4o")
            .display_name("GPT-4o")
            .capabilities([Text, Chat, Streaming, Vision, ToolUse, JsonMode])
            .limits(128_000, 16_384)
            .cost(2.5, 10.0)
            .ranks(70, 60)
            .as_of(as_of),
        ModelInfo::new("openai", "gpt-4o-mini")
            .display_name("GPT-4o mini")
            .capabilities([Text, Chat, Streaming, Vision, ToolUse, JsonMode])
            .limits(128_000, 16_384)
            .cost(0.15, 0.60)
            .ranks(45, 85)
            .as_of(as_of),
        ModelInfo::new("openai", "text-embedding-3-small")
            .display_name("text-embedding-3-small")
            .capabilities([Embeddings])
            .limits(8_191, 0)
            .cost(0.02, 0.0)
            .as_of(as_of),
    ]
}

/// Parses OpenAI's error body:
/// `{"error": {"message", "type", "param", "code"}}`.
fn parse_error(status: u16, body: &str) -> ProviderFailure {
    let parsed = json::from_str(body).ok();
    let error = parsed.as_ref().and_then(|v| v.get("error"));
    let (message, error_type, code) = match error {
        Some(Value::Object(_)) => {
            let e = error.expect("matched Some");
            (
                e.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or(body)
                    .to_string(),
                e.get("type").and_then(Value::as_str).map(str::to_string),
                e.get("code").and_then(Value::as_str).map(str::to_string),
            )
        }
        // Some compatible servers send `{"error": "text"}`.
        Some(Value::Str(text)) => (text.clone(), None, None),
        _ => (body.to_string(), None, None),
    };
    let code_str = code.as_deref().unwrap_or("");
    let type_str = error_type.as_deref().unwrap_or("");
    let hint =
        if code_str == "context_length_exceeded" || message.contains("maximum context length") {
            ErrorHint::ContextLength
        } else if code_str == "content_filter"
            || code_str == "content_policy_violation"
            || type_str == "content_filter"
        {
            ErrorHint::ContentFilter
        } else if code_str == "insufficient_quota" || type_str == "insufficient_quota" {
            // A 429 that no amount of waiting fixes: the account is out of quota.
            ErrorHint::Auth
        } else {
            ErrorHint::None
        };
    ProviderFailure {
        status,
        hint,
        error_type: code.or(error_type),
        message,
    }
}

fn build_embed_body(request: &EmbedRequest) -> Value {
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
    if let Some(user) = &request.user {
        fields.push(("user".to_string(), s(user)));
    }
    Value::Object(fields)
}

/// Parses OpenAI's `{ data: [{ index, embedding: [..] }], usage }` embeddings
/// response, ordering the vectors by `index`.
fn parse_embeddings(model: &str, value: &Value) -> Result<EmbedResponse, GenAiError> {
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| GenAiError::ResponseParse("missing 'data' array".into()))?;
    let mut rows: Vec<(u64, Vec<f32>)> = Vec::with_capacity(data.len());
    for (position, item) in data.iter().enumerate() {
        let vec = item
            .get("embedding")
            .and_then(Value::as_array)
            .ok_or_else(|| GenAiError::ResponseParse("missing 'embedding'".into()))?
            .iter()
            .map(num_f32)
            .collect::<Option<Vec<f32>>>()
            .ok_or_else(|| GenAiError::ResponseParse("non-numeric embedding value".into()))?;
        let index = item
            .get("index")
            .and_then(Value::as_u64)
            .unwrap_or(position as u64);
        rows.push((index, vec));
    }
    rows.sort_by_key(|(index, _)| *index);
    let usage = value.get("usage").map(|u| Usage {
        prompt_tokens: u32_field(u, "prompt_tokens"),
        total_tokens: u32_field(u, "total_tokens"),
        ..Usage::default()
    });
    Ok(EmbedResponse {
        model: value
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(model)
            .to_string(),
        embeddings: rows.into_iter().map(|(_, v)| v).collect(),
        usage,
    })
}

/// Builds the JSON request body. Pure and unit-tested.
fn build_body(
    request: &CompletionRequest,
    stream: bool,
    config: &OpenAiConfig,
) -> Result<Value, GenAiError> {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(sys) = &request.options.system_instructions {
        messages.push(obj([("role", s("system")), ("content", s(sys))]));
    }
    for message in &request.messages {
        encode_message(message, &mut messages);
    }

    let o = &request.options;
    let mut fields: Vec<(String, Value)> = vec![
        ("model".into(), s(&request.model)),
        ("messages".into(), Value::Array(messages)),
        ("stream".into(), stream.into()),
    ];
    if stream && config.stream_usage {
        fields.push((
            "stream_options".into(),
            obj([("include_usage", true.into())]),
        ));
    }
    if let Some(t) = o.temperature {
        fields.push(("temperature".into(), f32_num(t)));
    }
    if let Some(p) = o.top_p {
        fields.push(("top_p".into(), f32_num(p)));
    }
    if let Some(m) = o.max_tokens {
        let key = if config.legacy_max_tokens {
            "max_tokens"
        } else {
            "max_completion_tokens"
        };
        fields.push((key.into(), m.into()));
    }
    if !o.stop.is_empty() {
        fields.push((
            "stop".into(),
            Value::Array(o.stop.iter().map(|t| s(t)).collect()),
        ));
    }
    if let Some(seed) = o.seed {
        fields.push(("seed".into(), seed.into()));
    }
    if let Some(p) = o.presence_penalty {
        fields.push(("presence_penalty".into(), f32_num(p)));
    }
    if let Some(p) = o.frequency_penalty {
        fields.push(("frequency_penalty".into(), f32_num(p)));
    }
    if let Some(user) = &o.user {
        fields.push(("user".into(), s(user)));
    }
    if let Some(effort) = o.reasoning_effort {
        // The Chat Completions API defines low / medium / high.
        let label = match effort {
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            _ => "high",
        };
        fields.push(("reasoning_effort".into(), s(label)));
    }
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                let mut function = vec![
                    ("name".to_string(), s(&t.name)),
                    ("description".to_string(), s(&t.description)),
                    ("parameters".to_string(), t.parameters.clone()),
                ];
                if t.strict {
                    function.push(("strict".to_string(), true.into()));
                }
                obj([
                    ("type", s("function")),
                    ("function", Value::Object(function)),
                ])
            })
            .collect();
        fields.push(("tools".into(), Value::Array(tools)));
        if let Some(parallel) = o.parallel_tool_calls {
            fields.push(("parallel_tool_calls".into(), parallel.into()));
        }
    }
    if let Some(choice) = &request.tool_choice {
        // "auto" | "none" | "required" | {type: function, function: {name}}.
        let v = match choice {
            ToolChoice::Auto => s("auto"),
            ToolChoice::None => s("none"),
            ToolChoice::Required => s("required"),
            ToolChoice::Named(name) => obj([
                ("type", s("function")),
                ("function", obj([("name", s(name))])),
            ]),
        };
        fields.push(("tool_choice".into(), v));
    }
    match &request.response_format {
        Some(ResponseFormat::Json) => {
            fields.push(("response_format".into(), obj([("type", s("json_object"))])));
        }
        Some(format) => {
            if let Some((name, description, schema, strict)) = format.schema_parts() {
                let mut json_schema = vec![("name".to_string(), s(name))];
                if let Some(d) = description {
                    json_schema.push(("description".to_string(), s(d)));
                }
                json_schema.push(("schema".to_string(), schema.clone()));
                if strict {
                    json_schema.push(("strict".to_string(), true.into()));
                }
                fields.push((
                    "response_format".into(),
                    obj([
                        ("type", s("json_schema")),
                        ("json_schema", Value::Object(json_schema)),
                    ]),
                ));
            }
        }
        None => {}
    }
    merge_custom(&mut fields, &o.custom, OWNED_KEYS, "openai")?;
    Ok(Value::Object(fields))
}

/// Encodes one message as one or more Chat Completions messages:
///
/// - every tool result becomes its own `{role: "tool", tool_call_id, content}`;
/// - an assistant message with tool calls carries them in `tool_calls`
///   (`arguments` as a JSON string) with `content: null` when it has no text;
/// - anything else is `{role, content}` with a string, or an array of parts
///   when it has images.
fn encode_message(msg: &Message, out: &mut Vec<Value>) {
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut content: Vec<Value> = Vec::new();
    let mut text_only = true;
    let mut has_result = false;
    for part in &msg.parts {
        match part {
            MessagePart::Text(t) => content.push(obj([("type", s("text")), ("text", s(t))])),
            MessagePart::Image { data, mime_type } => {
                text_only = false;
                let url = format!("data:{mime_type};base64,{}", base64::encode(data));
                content.push(obj([
                    ("type", s("image_url")),
                    ("image_url", obj([("url", s(&url))])),
                ]));
            }
            MessagePart::FileRef { uri, .. } => {
                text_only = false;
                content.push(obj([
                    ("type", s("image_url")),
                    ("image_url", obj([("url", s(uri))])),
                ]));
            }
            MessagePart::ToolCall {
                id,
                name,
                arguments,
            } => tool_calls.push(obj([
                ("id", s(id)),
                ("type", s("function")),
                (
                    "function",
                    obj([
                        ("name", s(name)),
                        ("arguments", s(&json::to_string(arguments))),
                    ]),
                ),
            ])),
            MessagePart::ToolResult { call_id, result } => {
                has_result = true;
                out.push(obj([
                    ("role", s("tool")),
                    ("tool_call_id", s(call_id)),
                    ("content", s(&tool_result_text(result))),
                ]));
            }
            // Blocks owned by another provider have no meaning here.
            _ => {}
        }
    }
    if content.is_empty() && tool_calls.is_empty() {
        if !has_result {
            // An empty message: keep the turn so roles still alternate.
            out.push(obj([("role", s(msg.role.as_str())), ("content", s(""))]));
        }
        return;
    }
    // A tool-role message that carried results is fully encoded above; any
    // other content it has is sent as a user turn.
    let role = if has_result && msg.role == Role::Tool {
        "user"
    } else {
        msg.role.as_str()
    };
    let content_value = if content.is_empty() {
        Value::Null
    } else if text_only {
        s(&msg.text_content())
    } else {
        Value::Array(content)
    };
    let mut fields = vec![
        ("role".to_string(), s(role)),
        ("content".to_string(), content_value),
    ];
    if !tool_calls.is_empty() {
        fields.push(("tool_calls".to_string(), Value::Array(tool_calls)));
    }
    out.push(Value::Object(fields));
}

fn parse_usage(u: &Value) -> Usage {
    Usage {
        prompt_tokens: u32_field(u, "prompt_tokens"),
        completion_tokens: u32_field(u, "completion_tokens"),
        total_tokens: u32_field(u, "total_tokens"),
        cached_tokens: u
            .get("prompt_tokens_details")
            .and_then(|d| u32_field(d, "cached_tokens")),
        cache_write_tokens: None,
        reasoning_tokens: u
            .get("completion_tokens_details")
            .and_then(|d| u32_field(d, "reasoning_tokens")),
    }
}

/// Parses the JSON arguments string of a tool call (`""` is `{}`).
fn parse_arguments(name: &str, raw: &str) -> Result<Value, GenAiError> {
    if raw.trim().is_empty() {
        return Ok(Value::Object(Vec::new()));
    }
    json::from_str(raw).map_err(|e| {
        GenAiError::ResponseParse(format!(
            "arguments of tool call '{name}' are not valid JSON: {e}"
        ))
    })
}

/// Parses a non-streaming response body. Pure and unit-tested.
fn parse_response(value: &Value) -> Result<CompletionResponse, GenAiError> {
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
    // A structured-output refusal arrives in `refusal` instead of `content`.
    let refusal = msg
        .and_then(|m| m.get("refusal"))
        .and_then(Value::as_str)
        .filter(|r| !r.is_empty());
    if let Some(text) = refusal {
        parts.push(MessagePart::Text(text.to_string()));
    }
    if let Some(calls) = msg
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array)
    {
        for call in calls {
            let func = call.get("function");
            let name = func
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let raw = func
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let arguments = parse_arguments(&name, raw)?;
            parts.push(MessagePart::ToolCall {
                id: call
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                name,
                arguments,
            });
        }
    }

    let citations: Vec<Citation> = msg
        .and_then(|m| m.get("annotations"))
        .and_then(Value::as_array)
        .map(|annotations| {
            annotations
                .iter()
                .filter_map(|a| a.get("url_citation"))
                .map(|c| Citation {
                    cited_text: None,
                    title: c.get("title").and_then(Value::as_str).map(str::to_string),
                    url: c.get("url").and_then(Value::as_str).map(str::to_string),
                    document_index: None,
                    start: u32_field(c, "start_index"),
                    end: u32_field(c, "end_index"),
                })
                .collect()
        })
        .unwrap_or_default();

    let mut response = CompletionResponse::new(
        value.get("model").and_then(Value::as_str).unwrap_or(""),
        Message {
            role: Role::Assistant,
            id: None,
            parts,
        },
    );
    response.id = value.get("id").and_then(Value::as_str).map(str::to_string);
    if let Some(raw) = choice.get("finish_reason").and_then(Value::as_str) {
        response = response.with_raw_finish_reason(raw);
    }
    if refusal.is_some() {
        response.finish_reason = Some(FinishReason::ContentFilter);
    }
    response.usage = value.get("usage").filter(|u| !u.is_null()).map(parse_usage);
    if !citations.is_empty() {
        response.grounding = Some(Grounding { citations });
    }
    Ok(response)
}

/// Stateful decoder of a Chat Completions event stream.
#[derive(Default)]
struct OpenAiStream {
    started: bool,
    refused: bool,
    finished: bool,
    done: bool,
}

impl StreamParser for OpenAiStream {
    fn on_data(
        &mut self,
        _event: Option<&str>,
        data: &str,
        out: &mut Vec<Result<CompletionChunk, GenAiError>>,
    ) {
        let data = data.trim();
        if data.is_empty() {
            return;
        }
        if data == "[DONE]" {
            self.done = true;
            return;
        }
        let value = match json::from_str(data) {
            Ok(v) => v,
            Err(e) => {
                out.push(Err(GenAiError::ResponseParse(format!(
                    "stream event is not JSON: {e}"
                ))));
                return;
            }
        };
        if value.get("error").is_some_and(|e| !e.is_null()) {
            // An error reported after the stream was established.
            out.push(Err(stream_failure(parse_error(500, data))));
            return;
        }
        if !self.started {
            self.started = true;
            let id = value.get("id").and_then(Value::as_str).map(str::to_string);
            let model = value
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string);
            if id.is_some() || model.is_some() {
                out.push(Ok(CompletionChunk::Start { id, model }));
            }
        }
        if let Some(choice) = value.get("choices").and_then(|c| c.get_index(0)) {
            if let Some(delta) = choice.get("delta") {
                if let Some(text) = delta.get("content").and_then(Value::as_str) {
                    if !text.is_empty() {
                        out.push(Ok(CompletionChunk::TextDelta(text.to_string())));
                    }
                }
                if let Some(text) = delta.get("refusal").and_then(Value::as_str) {
                    if !text.is_empty() {
                        self.refused = true;
                        out.push(Ok(CompletionChunk::TextDelta(text.to_string())));
                    }
                }
                if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                    for (position, call) in calls.iter().enumerate() {
                        let func = call.get("function");
                        out.push(Ok(CompletionChunk::ToolCallDelta(ToolCallDelta {
                            index: u32_field(call, "index").unwrap_or(position as u32),
                            id: call.get("id").and_then(Value::as_str).map(str::to_string),
                            name: func
                                .and_then(|f| f.get("name"))
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            arguments: func
                                .and_then(|f| f.get("arguments"))
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        })));
                    }
                }
            }
            if let Some(raw) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finished = true;
                let finish_reason = if self.refused {
                    FinishReason::ContentFilter
                } else {
                    FinishReason::from_provider(raw)
                };
                out.push(Ok(CompletionChunk::Done {
                    finish_reason,
                    raw_finish_reason: Some(raw.to_string()),
                }));
            }
        }
        // With `stream_options.include_usage`, a final event carries the usage
        // (and an empty `choices`).
        if let Some(usage) = value.get("usage").filter(|u| !u.is_null()) {
            out.push(Ok(CompletionChunk::Usage(parse_usage(usage))));
        }
    }

    fn is_complete(&self) -> bool {
        self.finished || self.done
    }

    fn is_terminal(&self) -> bool {
        self.done
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroly::genai::Options;

    fn cfg() -> OpenAiConfig {
        OpenAiConfig::new("sk-test")
    }

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
        let body = build_body(&req, false, &cfg()).unwrap();
        assert_eq!(
            json::to_string(&body),
            r#"{"model":"gpt-4o","messages":[{"role":"system","content":"sys"},{"role":"user","content":"hi"}],"stream":false,"temperature":0.5,"max_completion_tokens":100,"response_format":{"type":"json_object"}}"#
        );
        // Legacy servers get the old token-limit name.
        let legacy = build_body(&req, false, &cfg().legacy_max_tokens(true)).unwrap();
        assert_eq!(legacy.get("max_tokens").unwrap().as_u64(), Some(100));
        assert!(legacy.get("max_completion_tokens").is_none());
    }

    #[test]
    fn f32_options_keep_their_decimal_value() {
        let req = CompletionRequest::builder("m")
            .options(Options::builder().temperature(0.7).top_p(0.9).build())
            .build();
        let body = json::to_string(&build_body(&req, false, &cfg()).unwrap());
        assert!(body.contains(r#""temperature":0.7,"top_p":0.9"#), "{body}");
    }

    #[test]
    fn parses_response_text_and_usage() {
        let value = json::from_str(
            r#"{"id":"chatcmpl-1","model":"gpt-4o","choices":[{"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4,"prompt_tokens_details":{"cached_tokens":2},"completion_tokens_details":{"reasoning_tokens":1}}}"#,
        )
        .unwrap();
        let resp = parse_response(&value).unwrap();
        assert_eq!(resp.text(), "hello");
        assert_eq!(resp.id.as_deref(), Some("chatcmpl-1"));
        assert_eq!(resp.finish_reason, Some(FinishReason::Stop));
        assert_eq!(resp.raw_finish_reason.as_deref(), Some("stop"));
        let usage = resp.usage.unwrap();
        assert_eq!(usage.total_tokens, Some(4));
        assert_eq!(usage.cached_tokens, Some(2));
        assert_eq!(usage.reasoning_tokens, Some(1));
    }

    #[test]
    fn parses_tool_call_response() {
        let value = json::from_str(
            r#"{"model":"gpt-4o","choices":[{"message":{"role":"assistant","tool_calls":[{"id":"call_1","function":{"name":"get_weather","arguments":"{\"city\":\"NYC\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        )
        .unwrap();
        let resp = parse_response(&value).unwrap();
        assert_eq!(resp.finish_reason, Some(FinishReason::ToolCalls));
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
    fn invalid_tool_arguments_are_an_error() {
        let value = json::from_str(
            r#"{"model":"m","choices":[{"message":{"tool_calls":[{"id":"c","function":{"name":"t","arguments":"{\"a\":"}}]},"finish_reason":"length"}]}"#,
        )
        .unwrap();
        assert!(matches!(
            parse_response(&value),
            Err(GenAiError::ResponseParse(_))
        ));
    }

    #[test]
    fn classifies_error_bodies() {
        let f = parse_error(
            400,
            r#"{"error":{"message":"This model's maximum context length is 8192 tokens.","type":"invalid_request_error","code":"context_length_exceeded"}}"#,
        );
        assert_eq!(f.hint, ErrorHint::ContextLength);
        let f = parse_error(
            429,
            r#"{"error":{"message":"You exceeded your current quota","type":"insufficient_quota","code":"insufficient_quota"}}"#,
        );
        assert_eq!(f.hint, ErrorHint::Auth);
        let f = parse_error(502, "<html>bad gateway</html>");
        assert_eq!(f.hint, ErrorHint::None);
        assert_eq!(f.message, "<html>bad gateway</html>");
    }

    #[test]
    fn stream_parser_tracks_completion() {
        let mut p = OpenAiStream::default();
        let mut out = Vec::new();
        p.on_data(
            None,
            r#"{"choices":[{"delta":{"content":"Hi"}}]}"#,
            &mut out,
        );
        assert!(!p.is_complete());
        p.on_data(
            None,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            &mut out,
        );
        assert!(p.is_complete() && !p.is_terminal());
        p.on_data(None, "[DONE]", &mut out);
        assert!(p.is_terminal());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].as_ref().unwrap().text(), "Hi");
    }

    #[test]
    fn azure_urls_and_auth_header() {
        let p = OpenAiProvider::with_config(
            OpenAiConfig::azure("https://r.openai.azure.com/", "my dep", "2024-10-21", "k"),
            None,
        );
        assert_eq!(
            p.url(&p.config.chat_path),
            "https://r.openai.azure.com/openai/deployments/my%20dep/chat/completions?api-version=2024-10-21"
        );
        assert_eq!(p.name(), "azure-openai");
        assert!(p.model_catalog().is_empty());
        // The key never shows up in Debug output.
        let debug = format!("{p:?} {:?}", p.config);
        assert!(!debug.contains("\"k\""), "{debug}");
    }
}

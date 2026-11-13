//! Anthropic Claude provider (Messages API).

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use ferroly::clients::{ApiKeyAuth, AuthProvider, Secret};
use ferroly::codec::{json, Value};
use ferroly::hash::base64;

use super::common::{
    f32_num, merge_custom, obj, s, stream_failure, tool_result_text, u32_field, ErrorHint,
    HttpCore, ProviderFailure,
};
use super::sse::{Framing, StreamParser};
use super::ProviderOptions;
use ferroly::genai::observe::Operation;
use ferroly::genai::provider::{BoxFuture, ChunkStream, GenAiProvider};
use ferroly::genai::router::AsOf;
use ferroly::genai::{
    CacheTtl, Capability, Citation, CompletionChunk, CompletionRequest, CompletionResponse,
    GenAiError, Grounding, Message, MessagePart, ModelInfo, ResponseFormat, Role, ToolCallDelta,
    ToolChoice, Usage,
};

const PROVIDER: &str = "claude";
const DEFAULT_BASE: &str = "https://api.anthropic.com";
const DEFAULT_API_VERSION: &str = "2023-06-01";
/// `max_tokens` is required by the Messages API. When the caller sets none,
/// the model's catalog output limit is used, capped so the request stays
/// within what the kind of call can reasonably carry.
const NON_STREAMING_MAX_TOKENS_CAP: u32 = 16_000;
const STREAMING_MAX_TOKENS_CAP: u32 = 64_000;
/// The default for a model that is not in the catalog.
const UNKNOWN_MODEL_MAX_TOKENS: u32 = 4_096;
/// Body keys the provider builds itself; `Options::custom` may not set them.
const OWNED_KEYS: &[&str] = &["model", "messages", "stream", "system"];

/// Configuration for constructing a [`ClaudeProvider`].
///
/// ```
/// use ferroly::genai::ClaudeProviderConfig;
///
/// let config = ClaudeProviderConfig::new("sk-ant-...")
///     .beta("server-side-fallback-2026-07-01")
///     .default_max_tokens(8_000);
/// # let _ = config;
/// ```
#[derive(Clone)]
pub struct ClaudeProviderConfig {
    /// The auth provider applied to every request. [`ClaudeProviderConfig::new`]
    /// sets an `x-api-key` header; use
    /// [`with_auth`](ClaudeProviderConfig::with_auth) for anything else (a
    /// Vault-backed key, an OAuth bearer token).
    pub auth: Arc<dyn AuthProvider>,
    api_key: Option<Secret>,
    api_version: String,
    betas: Vec<String>,
    omit_sampling_params: bool,
    default_max_tokens: Option<u32>,
    eager_tool_streaming: Option<bool>,
}

impl fmt::Debug for ClaudeProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeProviderConfig")
            .field("api_version", &self.api_version)
            .field("betas", &self.betas)
            .field("omit_sampling_params", &self.omit_sampling_params)
            .field("default_max_tokens", &self.default_max_tokens)
            .field("eager_tool_streaming", &self.eager_tool_streaming)
            .finish_non_exhaustive()
    }
}

impl ClaudeProviderConfig {
    /// Authenticates with an API key in the `x-api-key` header.
    pub fn new(api_key: impl Into<Secret>) -> Self {
        let key: Secret = api_key.into();
        let auth: Arc<dyn AuthProvider> = Arc::new(ApiKeyAuth::new(
            "x-api-key",
            key.expose_str().unwrap_or_default(),
        ));
        ClaudeProviderConfig {
            api_key: Some(key),
            ..Self::with_auth(auth)
        }
    }

    /// Authenticates with a caller-supplied auth provider.
    pub fn with_auth(auth: Arc<dyn AuthProvider>) -> Self {
        ClaudeProviderConfig {
            auth,
            api_key: None,
            api_version: DEFAULT_API_VERSION.to_string(),
            betas: Vec::new(),
            omit_sampling_params: false,
            default_max_tokens: None,
            eager_tool_streaming: None,
        }
    }

    /// The `anthropic-version` header value. Default `2023-06-01`.
    ///
    /// From API version `2026-10-21` on, the Messages API rejects
    /// `temperature`, `top_p` and `top_k` on every model: combine that version
    /// with [`omit_sampling_params`](Self::omit_sampling_params).
    pub fn api_version(mut self, version: impl Into<String>) -> Self {
        self.api_version = version.into();
        self
    }

    /// Adds a value to the `anthropic-beta` header.
    pub fn beta(mut self, beta: impl Into<String>) -> Self {
        self.betas.push(beta.into());
        self
    }

    /// Do not send `temperature`, `top_p` and `top_k`.
    ///
    /// Current Claude models reject non-default sampling parameters (and API
    /// version `2026-10-21` rejects them on every model), which would make a
    /// request that sets `Options::temperature` fail with a `400`. With this
    /// switch on the provider leaves the three options out, so the same
    /// request can be routed to Claude and to other providers.
    pub fn omit_sampling_params(mut self, omit: bool) -> Self {
        self.omit_sampling_params = omit;
        self
    }

    /// The `max_tokens` sent when a request sets none, instead of the
    /// catalog-derived default.
    pub fn default_max_tokens(mut self, max_tokens: u32) -> Self {
        self.default_max_tokens = Some(max_tokens);
        self
    }

    /// Whether streamed requests mark their tools `eager_input_streaming`, so
    /// that large tool inputs arrive as they are generated instead of in one
    /// burst. With it on the API no longer validates tool inputs: validate
    /// them against the tool's schema before running the tool.
    ///
    /// Default: on for `https://api.anthropic.com`, off for any other base URL
    /// (a gateway may reject the field).
    pub fn eager_tool_streaming(mut self, eager: bool) -> Self {
        self.eager_tool_streaming = Some(eager);
        self
    }
}

/// A [`GenAiProvider`] backed by the Anthropic Messages API.
pub struct ClaudeProvider {
    core: HttpCore,
    base_url: String,
    config: ClaudeProviderConfig,
    eager_tools: bool,
}

impl fmt::Debug for ClaudeProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeProvider")
            .field("base_url", &self.base_url)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl ClaudeProvider {
    /// Creates a provider authenticated with an `x-api-key` header.
    pub fn new(api_key: impl Into<String>, opts: Option<ProviderOptions>) -> Self {
        Self::with_config(ClaudeProviderConfig::new(api_key.into()), opts)
    }

    /// Creates a provider from a configuration (custom auth, API version,
    /// beta flags, …).
    pub fn with_config(config: ClaudeProviderConfig, opts: Option<ProviderOptions>) -> Self {
        let opts = opts.unwrap_or_default();
        let custom_base = opts.base_url.is_some();
        let base_url = opts
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE.to_string());
        let core = HttpCore::new(
            PROVIDER,
            &opts,
            Some(config.auth.clone()),
            config.api_key.clone(),
        );
        let eager_tools = config.eager_tool_streaming.unwrap_or(!custom_base);
        Self {
            core,
            base_url: base_url.trim_end_matches('/').to_string(),
            config,
            eager_tools,
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/messages", self.base_url)
    }

    fn headers(&self) -> Vec<(&'static str, String)> {
        let mut headers = vec![("anthropic-version", self.config.api_version.clone())];
        if !self.config.betas.is_empty() {
            headers.push(("anthropic-beta", self.config.betas.join(",")));
        }
        headers
    }

    fn body(&self, request: &CompletionRequest, stream: bool) -> Result<Value, GenAiError> {
        let max_output = self
            .model_info_for(&request.model)
            .map(|m| m.max_output_tokens);
        build_body(
            request,
            &BodyOptions {
                stream,
                max_output,
                default_max_tokens: self.config.default_max_tokens,
                omit_sampling: self.config.omit_sampling_params,
                eager_tools: self.eager_tools,
            },
        )
    }
}

impl GenAiProvider for ClaudeProvider {
    fn name(&self) -> &str {
        PROVIDER
    }

    fn description(&self) -> &str {
        "Anthropic Claude Messages provider"
    }

    fn supports(&self, capability: Capability) -> bool {
        matches!(
            capability,
            Capability::Text
                | Capability::Chat
                | Capability::Streaming
                | Capability::ToolUse
                | Capability::Vision
                | Capability::Reasoning
        )
    }

    fn model_catalog(&self) -> Vec<ModelInfo> {
        default_catalog()
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
                let body = self.body(&request, false)?;
                let headers = self.headers();
                let resp = self
                    .core
                    .post(&self.endpoint(), &body, &headers, &call, parse_error)
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
                let body = self.body(&request, true)?;
                let headers = self.headers();
                self.core
                    .post(&self.endpoint(), &body, &headers, &call, parse_error)
                    .await
            }
            .await;
            match established {
                Ok(resp) => {
                    Ok(self
                        .core
                        .spawn_stream(resp, ClaudeStream::default(), Framing::Sse, call))
                }
                Err(e) => {
                    call.end_err(&e);
                    Err(e)
                }
            }
        })
    }
}

/// The compiled-in Claude catalog. Ids, limits and prices are **defaults to
/// override**: they were taken from Anthropic's published model table on the
/// `as_of` date and are not updated between releases.
fn default_catalog() -> Vec<ModelInfo> {
    use Capability::{Chat, Reasoning, Streaming, Text, ToolUse, Vision};
    let caps = [Text, Chat, Streaming, Vision, ToolUse, Reasoning];
    let as_of = AsOf::new(2026, 11, 3);
    vec![
        ModelInfo::new(PROVIDER, "claude-fable-5-2")
            .display_name("Claude Fable 5.2")
            .capabilities(caps)
            .limits(1_000_000, 128_000)
            .cost(8.0, 40.0)
            .cache_cost(Some(0.20), Some(10.0))
            .ranks(100, 30)
            .as_of(as_of),
        ModelInfo::new(PROVIDER, "claude-opus-5-5")
            .display_name("Claude Opus 5.5")
            .capabilities(caps)
            .limits(1_000_000, 128_000)
            .cost(4.0, 20.0)
            .cache_cost(Some(0.20), None)
            .ranks(90, 45)
            .as_of(as_of),
        ModelInfo::new(PROVIDER, "claude-sonnet-5-5")
            .display_name("Claude Sonnet 5.5")
            .capabilities(caps)
            .limits(1_000_000, 128_000)
            .cost(2.0, 10.0)
            .cache_cost(Some(0.10), None)
            .ranks(75, 65)
            .as_of(as_of),
        ModelInfo::new(PROVIDER, "claude-haiku-5-5")
            .display_name("Claude Haiku 5.5")
            .capabilities(caps)
            .limits(1_000_000, 128_000)
            // The rate for prompts up to 100K tokens; longer prompts cost more.
            .cost(0.10, 0.50)
            .meta("pricing_note", "rate for prompts up to 100K tokens")
            .ranks(55, 90)
            .as_of(as_of),
        ModelInfo::new(PROVIDER, "claude-haiku-4-5")
            .display_name("Claude Haiku 4.5")
            .capabilities(caps)
            .limits(200_000, 64_000)
            .cost(1.0, 5.0)
            .ranks(45, 85)
            .as_of(as_of),
    ]
}

/// The status a Claude error type corresponds to, for errors reported inside
/// a stream (where there is no HTTP status of their own).
fn status_for_error_type(error_type: &str) -> u16 {
    match error_type {
        "invalid_request_error" => 400,
        "authentication_error" => 401,
        "billing_error" => 402,
        "permission_error" => 403,
        "not_found_error" => 404,
        "request_too_large" => 413,
        "rate_limit_error" => 429,
        "overloaded_error" => 529,
        _ => 500,
    }
}

/// Parses Claude's error body:
/// `{"type": "error", "error": {"type", "message"}, "request_id"}`.
fn parse_error(status: u16, body: &str) -> ProviderFailure {
    let parsed = json::from_str(body).ok();
    let error = parsed.as_ref().and_then(|v| v.get("error"));
    let error_type = error
        .and_then(|e| e.get("type"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let message = error
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or(body)
        .to_string();
    let hint = match error_type.as_deref() {
        Some("overloaded_error") => ErrorHint::Overloaded,
        Some("rate_limit_error") => ErrorHint::RateLimited,
        Some("authentication_error" | "permission_error" | "billing_error") => ErrorHint::Auth,
        Some("request_too_large") => ErrorHint::ContextLength,
        _ if status == 413 || message.contains("prompt is too long") => ErrorHint::ContextLength,
        _ => ErrorHint::None,
    };
    ProviderFailure {
        status,
        hint,
        error_type,
        message,
    }
}

struct BodyOptions {
    stream: bool,
    /// The model's catalog output limit, when the model is known.
    max_output: Option<u32>,
    default_max_tokens: Option<u32>,
    omit_sampling: bool,
    eager_tools: bool,
}

fn cache_control(ttl: CacheTtl) -> Value {
    match ttl {
        CacheTtl::OneHour => obj([("type", s("ephemeral")), ("ttl", s("1h"))]),
        _ => obj([("type", s("ephemeral"))]),
    }
}

/// Builds the JSON request body. Pure and unit-tested.
fn build_body(request: &CompletionRequest, opts: &BodyOptions) -> Result<Value, GenAiError> {
    let o = &request.options;

    // The Messages API alternates user and assistant turns: consecutive
    // messages of the same wire role are merged, which also puts every
    // tool_result of one round into a single user message.
    let mut messages: Vec<(&'static str, Vec<Value>)> = Vec::new();
    for message in request.messages.iter().filter(|m| m.role != Role::System) {
        let content = encode_content(message);
        if content.is_empty() {
            continue;
        }
        let role = role_str(message.role);
        match messages.last_mut() {
            Some((last_role, blocks)) if *last_role == role => blocks.extend(content),
            _ => messages.push((role, content)),
        }
    }
    let messages: Vec<Value> = messages
        .into_iter()
        .map(|(role, content)| obj([("role", s(role)), ("content", Value::Array(content))]))
        .collect();

    let max_tokens =
        o.max_tokens
            .or(opts.default_max_tokens)
            .unwrap_or_else(|| match opts.max_output {
                Some(limit) if limit > 0 => limit.min(if opts.stream {
                    STREAMING_MAX_TOKENS_CAP
                } else {
                    NON_STREAMING_MAX_TOKENS_CAP
                }),
                _ => UNKNOWN_MODEL_MAX_TOKENS,
            });
    let mut fields: Vec<(String, Value)> = vec![
        ("model".into(), s(&request.model)),
        ("max_tokens".into(), max_tokens.into()),
        ("messages".into(), Value::Array(messages)),
        ("stream".into(), opts.stream.into()),
    ];

    // System prompt: `system_instructions` first, then every `Message::system`
    // in order, separated by a blank line. Neither source is dropped.
    let mut system: Vec<String> = Vec::new();
    if let Some(instructions) = o.system_instructions.as_deref().filter(|t| !t.is_empty()) {
        system.push(instructions.to_string());
    }
    for message in request.messages.iter().filter(|m| m.role == Role::System) {
        let text = message.text_content();
        if !text.is_empty() {
            system.push(text);
        }
    }
    if !system.is_empty() {
        let text = system.join("\n\n");
        let value = match o.prompt_cache {
            Some(cache) if cache.system => Value::Array(vec![obj([
                ("type", s("text")),
                ("text", s(&text)),
                ("cache_control", cache_control(cache.ttl)),
            ])]),
            _ => s(&text),
        };
        fields.push(("system".into(), value));
    }
    if let Some(cache) = o.prompt_cache.filter(|c| c.conversation) {
        // Top-level cache_control: the API places the breakpoint on the last
        // cacheable block.
        fields.push(("cache_control".into(), cache_control(cache.ttl)));
    }

    if !opts.omit_sampling {
        if let Some(t) = o.temperature {
            fields.push(("temperature".into(), f32_num(t)));
        }
        if let Some(p) = o.top_p {
            fields.push(("top_p".into(), f32_num(p)));
        }
        if let Some(k) = o.top_k {
            fields.push(("top_k".into(), k.into()));
        }
    }
    if !o.stop.is_empty() {
        fields.push((
            "stop_sequences".into(),
            Value::Array(o.stop.iter().map(|t| s(t)).collect()),
        ));
    }
    if let Some(user) = &o.user {
        fields.push(("metadata".into(), obj([("user_id", s(user))])));
    }
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                let mut tool = vec![
                    ("name".to_string(), s(&t.name)),
                    ("description".to_string(), s(&t.description)),
                ];
                if t.strict {
                    tool.push(("strict".to_string(), true.into()));
                }
                if opts.stream && opts.eager_tools {
                    tool.push(("eager_input_streaming".to_string(), true.into()));
                }
                tool.push(("input_schema".to_string(), t.parameters.clone()));
                Value::Object(tool)
            })
            .collect();
        fields.push(("tools".into(), Value::Array(tools)));
    }
    // {type: auto} | {type: any} (required) | {type: tool, name} | {type: none},
    // each optionally with disable_parallel_tool_use.
    let serial = o.parallel_tool_calls == Some(false) && !request.tools.is_empty();
    let choice = match &request.tool_choice {
        Some(ToolChoice::Auto) => Some(vec![("type".to_string(), s("auto"))]),
        Some(ToolChoice::Required) => Some(vec![("type".to_string(), s("any"))]),
        Some(ToolChoice::None) => Some(vec![("type".to_string(), s("none"))]),
        Some(ToolChoice::Named(name)) => Some(vec![
            ("type".to_string(), s("tool")),
            ("name".to_string(), s(name)),
        ]),
        None if serial => Some(vec![("type".to_string(), s("auto"))]),
        None => None,
    };
    if let Some(mut choice) = choice {
        let is_none = matches!(request.tool_choice, Some(ToolChoice::None));
        if serial && !is_none {
            choice.push(("disable_parallel_tool_use".to_string(), true.into()));
        }
        fields.push(("tool_choice".into(), Value::Object(choice)));
    }

    // Structured output and reasoning effort both live in `output_config`.
    let mut output_config: Vec<(String, Value)> = Vec::new();
    match &request.response_format {
        Some(ResponseFormat::Json) => {
            // The Messages API has no schema-less JSON mode; pretending it
            // does would silently return free-form text.
            return Err(GenAiError::Unsupported {
                provider: PROVIDER.to_string(),
                capability: Capability::JsonMode,
            });
        }
        Some(format) => {
            if let Some((_name, _description, schema, _strict)) = format.schema_parts() {
                output_config.push((
                    "format".to_string(),
                    obj([("type", s("json_schema")), ("schema", schema.clone())]),
                ));
            }
        }
        None => {}
    }
    if let Some(effort) = o.reasoning_effort {
        output_config.push(("effort".to_string(), s(effort.as_str())));
    }
    if !output_config.is_empty() {
        fields.push(("output_config".into(), Value::Object(output_config)));
    }

    merge_custom(&mut fields, &o.custom, OWNED_KEYS, PROVIDER)?;
    Ok(Value::Object(fields))
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::Assistant => "assistant",
        _ => "user",
    }
}

/// Encodes a message's parts as Messages API content blocks.
fn encode_content(msg: &Message) -> Vec<Value> {
    // tool_result blocks must lead the user message that answers a tool call.
    let mut results: Vec<Value> = Vec::new();
    let mut blocks: Vec<Value> = Vec::new();
    for part in &msg.parts {
        match part {
            MessagePart::Text(t) => {
                if !t.is_empty() {
                    blocks.push(obj([("type", s("text")), ("text", s(t))]));
                }
            }
            MessagePart::Image { data, mime_type } => blocks.push(obj([
                ("type", s("image")),
                (
                    "source",
                    obj([
                        ("type", s("base64")),
                        ("media_type", s(mime_type)),
                        ("data", s(&base64::encode(data))),
                    ]),
                ),
            ])),
            MessagePart::FileRef { uri, mime_type } => {
                let is_http = uri.starts_with("https://") || uri.starts_with("http://");
                let source = obj([("type", s("url")), ("url", s(uri))]);
                if is_http && mime_type.starts_with("image/") {
                    blocks.push(obj([("type", s("image")), ("source", source)]));
                } else if is_http && mime_type == "application/pdf" {
                    blocks.push(obj([("type", s("document")), ("source", source)]));
                } else {
                    // Not fetchable by the API: pass the reference as text.
                    blocks.push(obj([("type", s("text")), ("text", s(uri))]));
                }
            }
            MessagePart::ToolCall {
                id,
                name,
                arguments,
            } => {
                let input = match arguments {
                    Value::Object(_) => arguments.clone(),
                    _ => Value::Object(Vec::new()),
                };
                blocks.push(obj([
                    ("type", s("tool_use")),
                    ("id", s(id)),
                    ("name", s(name)),
                    ("input", input),
                ]));
            }
            MessagePart::ToolResult { call_id, result } => results.push(obj([
                ("type", s("tool_result")),
                ("tool_use_id", s(call_id)),
                ("content", s(&tool_result_text(result))),
            ])),
            // A block this provider returned earlier (thinking, server tool
            // use, …) goes back exactly as it came.
            MessagePart::ProviderData { provider, data } if provider == PROVIDER => {
                blocks.push(data.clone());
            }
            _ => {}
        }
    }
    results.extend(blocks);
    results
}

/// Normalizes a `usage` object. `complete` is false for the partial usage
/// objects of a stream, whose total is only known once they are merged.
fn parse_usage(u: &Value, complete: bool) -> Usage {
    let input = u32_field(u, "input_tokens");
    let cache_write = u32_field(u, "cache_creation_input_tokens");
    let cache_read = u32_field(u, "cache_read_input_tokens");
    let output = u32_field(u, "output_tokens");
    // `input_tokens` is the uncached remainder; the normalized prompt count is
    // the whole prompt.
    let prompt = input.map(|i| {
        i.saturating_add(cache_write.unwrap_or(0))
            .saturating_add(cache_read.unwrap_or(0))
    });
    let mut usage = Usage {
        prompt_tokens: prompt,
        completion_tokens: output,
        total_tokens: None,
        cached_tokens: cache_read,
        cache_write_tokens: cache_write,
        reasoning_tokens: None,
    };
    if complete {
        usage.fill_total();
    }
    usage
}

fn parse_citations(block: &Value, out: &mut Vec<Citation>) {
    let Some(citations) = block.get("citations").and_then(Value::as_array) else {
        return;
    };
    for c in citations {
        let text = |key: &str| c.get(key).and_then(Value::as_str).map(str::to_string);
        out.push(Citation {
            cited_text: text("cited_text"),
            title: text("document_title").or_else(|| text("title")),
            url: text("url"),
            document_index: u32_field(c, "document_index"),
            start: u32_field(c, "start_char_index")
                .or_else(|| u32_field(c, "start_page_number"))
                .or_else(|| u32_field(c, "start_block_index")),
            end: u32_field(c, "end_char_index")
                .or_else(|| u32_field(c, "end_page_number"))
                .or_else(|| u32_field(c, "end_block_index")),
        });
    }
}

/// Parses a non-streaming response body. Pure and unit-tested.
fn parse_response(value: &Value) -> Result<CompletionResponse, GenAiError> {
    let blocks = value
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| GenAiError::ResponseParse("missing content".into()))?;

    let mut parts: Vec<MessagePart> = Vec::new();
    let mut reasoning = String::new();
    let mut citations: Vec<Citation> = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    parts.push(MessagePart::Text(t.to_string()));
                }
                parse_citations(block, &mut citations);
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
                arguments: block
                    .get("input")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Vec::new())),
            }),
            other => {
                if other == Some("thinking") {
                    if let Some(t) = block.get("thinking").and_then(Value::as_str) {
                        reasoning.push_str(t);
                    }
                }
                // Thinking, redacted thinking, server-tool and any future
                // block type: keep it verbatim so the next request can send
                // it back unchanged.
                parts.push(MessagePart::ProviderData {
                    provider: PROVIDER.to_string(),
                    data: block.clone(),
                });
            }
        }
    }

    let mut response = CompletionResponse::new(
        value.get("model").and_then(Value::as_str).unwrap_or(""),
        Message {
            role: Role::Assistant,
            id: None,
            parts,
        },
    );
    response.id = value.get("id").and_then(Value::as_str).map(str::to_string);
    if let Some(raw) = value.get("stop_reason").and_then(Value::as_str) {
        response = response.with_raw_finish_reason(raw);
    }
    response.usage = value
        .get("usage")
        .filter(|u| !u.is_null())
        .map(|u| parse_usage(u, true));
    if !reasoning.is_empty() {
        response.reasoning = Some(reasoning);
    }
    if !citations.is_empty() {
        response.grounding = Some(Grounding { citations });
    }
    Ok(response)
}

/// A content block being assembled from stream events.
enum Block {
    Text,
    /// A client tool call; the value is its index among the response's calls.
    Tool(u32),
    Thinking {
        thinking: String,
        signature: String,
    },
    /// Any other block: its start value plus the JSON input streamed for it.
    Other {
        block: Value,
        partial_json: String,
    },
}

/// Stateful decoder of a Messages API event stream.
#[derive(Default)]
struct ClaudeStream {
    blocks: HashMap<u64, Block>,
    next_tool: u32,
    finished: bool,
    stopped: bool,
}

impl StreamParser for ClaudeStream {
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
        let value = match json::from_str(data) {
            Ok(v) => v,
            Err(e) => {
                out.push(Err(GenAiError::ResponseParse(format!(
                    "stream event is not JSON: {e}"
                ))));
                return;
            }
        };
        let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
        match value.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                let message = value.get("message");
                let text = |key: &str| {
                    message
                        .and_then(|m| m.get(key))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                };
                out.push(Ok(CompletionChunk::Start {
                    id: text("id"),
                    model: text("model"),
                }));
                if let Some(usage) = message
                    .and_then(|m| m.get("usage"))
                    .filter(|u| !u.is_null())
                {
                    // The output count here is a placeholder; the final one
                    // arrives with `message_delta`.
                    let mut usage = parse_usage(usage, false);
                    usage.completion_tokens = None;
                    out.push(Ok(CompletionChunk::Usage(usage)));
                }
            }
            Some("content_block_start") => {
                let block = value.get("content_block").cloned().unwrap_or(Value::Null);
                let text = |key: &str| block.get(key).and_then(Value::as_str).unwrap_or("");
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if !text("text").is_empty() {
                            out.push(Ok(CompletionChunk::TextDelta(text("text").to_string())));
                        }
                        self.blocks.insert(index, Block::Text);
                    }
                    Some("tool_use") => {
                        let tool_index = self.next_tool;
                        self.next_tool += 1;
                        out.push(Ok(CompletionChunk::ToolCallDelta(ToolCallDelta {
                            index: tool_index,
                            id: Some(text("id").to_string()),
                            name: Some(text("name").to_string()),
                            arguments: String::new(),
                        })));
                        self.blocks.insert(index, Block::Tool(tool_index));
                    }
                    Some("thinking") => {
                        self.blocks.insert(
                            index,
                            Block::Thinking {
                                thinking: text("thinking").to_string(),
                                signature: text("signature").to_string(),
                            },
                        );
                    }
                    _ => {
                        self.blocks.insert(
                            index,
                            Block::Other {
                                block,
                                partial_json: String::new(),
                            },
                        );
                    }
                }
            }
            Some("content_block_delta") => {
                let delta = value.get("delta");
                let text = |key: &str| {
                    delta
                        .and_then(|d| d.get(key))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                };
                match delta.and_then(|d| d.get("type")).and_then(Value::as_str) {
                    Some("text_delta") => {
                        if !text("text").is_empty() {
                            out.push(Ok(CompletionChunk::TextDelta(text("text").to_string())));
                        }
                    }
                    Some("input_json_delta") => match self.blocks.get_mut(&index) {
                        Some(Block::Tool(tool_index)) => {
                            out.push(Ok(CompletionChunk::ToolCallDelta(ToolCallDelta {
                                index: *tool_index,
                                id: None,
                                name: None,
                                arguments: text("partial_json").to_string(),
                            })));
                        }
                        Some(Block::Other { partial_json, .. }) => {
                            partial_json.push_str(text("partial_json"));
                        }
                        _ => {}
                    },
                    Some("thinking_delta") => {
                        if let Some(Block::Thinking { thinking, .. }) = self.blocks.get_mut(&index)
                        {
                            thinking.push_str(text("thinking"));
                        }
                        if !text("thinking").is_empty() {
                            out.push(Ok(CompletionChunk::ReasoningDelta(
                                text("thinking").to_string(),
                            )));
                        }
                    }
                    Some("signature_delta") => {
                        if let Some(Block::Thinking { signature, .. }) = self.blocks.get_mut(&index)
                        {
                            signature.push_str(text("signature"));
                        }
                    }
                    // citations_delta and future delta types carry nothing the
                    // normalized stream models.
                    _ => {}
                }
            }
            Some("content_block_stop") => match self.blocks.remove(&index) {
                Some(Block::Thinking {
                    thinking,
                    signature,
                }) => out.push(Ok(CompletionChunk::ProviderData {
                    provider: PROVIDER.to_string(),
                    data: obj([
                        ("type", s("thinking")),
                        ("thinking", s(&thinking)),
                        ("signature", s(&signature)),
                    ]),
                })),
                Some(Block::Other {
                    mut block,
                    partial_json,
                }) => {
                    if !partial_json.trim().is_empty() {
                        if let (Value::Object(entries), Ok(input)) =
                            (&mut block, json::from_str(&partial_json))
                        {
                            entries.retain(|(k, _)| k != "input");
                            entries.push(("input".to_string(), input));
                        }
                    }
                    if !block.is_null() {
                        out.push(Ok(CompletionChunk::ProviderData {
                            provider: PROVIDER.to_string(),
                            data: block,
                        }));
                    }
                }
                _ => {}
            },
            Some("message_delta") => {
                if let Some(usage) = value.get("usage").filter(|u| !u.is_null()) {
                    out.push(Ok(CompletionChunk::Usage(parse_usage(usage, false))));
                }
                if let Some(raw) = value
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.finished = true;
                    out.push(Ok(CompletionChunk::done_raw(raw)));
                }
            }
            Some("message_stop") => self.stopped = true,
            Some("error") => {
                let error_type = value
                    .get("error")
                    .and_then(|e| e.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                out.push(Err(stream_failure(parse_error(
                    status_for_error_type(error_type),
                    data,
                ))));
            }
            // `ping` keep-alives, and event types added to the API later.
            _ => {}
        }
    }

    fn is_complete(&self) -> bool {
        self.finished || self.stopped
    }

    fn is_terminal(&self) -> bool {
        self.stopped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferroly::genai::{FinishReason, Options};

    fn opts(stream: bool) -> BodyOptions {
        BodyOptions {
            stream,
            max_output: Some(128_000),
            default_max_tokens: None,
            omit_sampling: false,
            eager_tools: false,
        }
    }

    #[test]
    fn builds_body_with_system_and_default_max_tokens() {
        let req = CompletionRequest {
            model: "claude-opus-5-5".into(),
            messages: vec![
                Message::system("be helpful"),
                Message::text(Role::User, "1", "hello"),
            ],
            tools: vec![],
            tool_choice: None,
            response_format: None,
            options: Options::new(),
        };
        let body = build_body(&req, &opts(false)).unwrap();
        assert_eq!(
            json::to_string(&body),
            r#"{"model":"claude-opus-5-5","max_tokens":16000,"messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}],"stream":false,"system":"be helpful"}"#
        );
        // Streaming may use more of the model's output limit.
        let body = build_body(&req, &opts(true)).unwrap();
        assert_eq!(body.get("max_tokens").unwrap().as_u64(), Some(64_000));
        // A model that is not in the catalog gets the conservative default.
        let unknown = BodyOptions {
            max_output: None,
            ..opts(false)
        };
        let body = build_body(&req, &unknown).unwrap();
        assert_eq!(body.get("max_tokens").unwrap().as_u64(), Some(4_096));
    }

    #[test]
    fn parses_response_text_and_usage() {
        let value = json::from_str(
            r#"{"id":"msg_1","model":"claude-opus-5-5","content":[{"type":"text","text":"hi there"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"cache_read_input_tokens":90,"cache_creation_input_tokens":20,"output_tokens":5}}"#,
        )
        .unwrap();
        let resp = parse_response(&value).unwrap();
        assert_eq!(resp.text(), "hi there");
        assert_eq!(resp.finish_reason, Some(FinishReason::Stop));
        assert_eq!(resp.raw_finish_reason.as_deref(), Some("end_turn"));
        let usage = resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, Some(120));
        assert_eq!(usage.cached_tokens, Some(90));
        assert_eq!(usage.cache_write_tokens, Some(20));
        assert_eq!(usage.total_tokens, Some(125));
    }

    #[test]
    fn classifies_error_bodies() {
        let f = parse_error(
            529,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        );
        assert_eq!(f.hint, ErrorHint::Overloaded);
        let f = parse_error(
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 1200000 tokens > 1000000 maximum"}}"#,
        );
        assert_eq!(f.hint, ErrorHint::ContextLength);
        let f = parse_error(
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages: roles must alternate"}}"#,
        );
        assert_eq!(f.hint, ErrorHint::None);
        assert_eq!(f.error_type.as_deref(), Some("invalid_request_error"));
    }

    #[test]
    fn catalog_uses_current_model_ids() {
        let ids: Vec<String> = default_catalog().into_iter().map(|m| m.name).collect();
        assert!(ids.contains(&"claude-opus-5-5".to_string()));
        assert!(ids.iter().all(|id| !id.starts_with("claude-3")));
    }
}

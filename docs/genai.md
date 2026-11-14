# ferroly::genai

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `genai` (implies `codec` + `clients` + `http` + `tokio`) · **Module:** `ferroly::genai`

A provider-agnostic GenAI / LLM interface. Application code depends on the [`GenAiProvider`](#the-genaiprovider-trait)
trait, never on a vendor SDK, so back-ends are interchangeable: construct the
provider you want, or hold an `Arc<dyn GenAiProvider>` when you need runtime
indirection.

The module gives you:

- A normalized [message model](#messages-role-messagepart) — role-tagged,
  multi-part (text, images, file references, tool calls, tool results).
- A typed [request/options](#options-and-optionsbuilder) surface plus fluent builders,
  with a pass-through for anything provider-specific.
- [Function calling](#function-calling-tooldefinition--toolchoice) (including
  parallel calls) on all three providers, and
  [structured output](#structured-output-responseformat--decode) that decodes a JSON
  reply straight into your own type.
- [Streaming](#streaming) as a sequence of typed events — text, tool-call
  fragments, usage, finish reason, errors — plus an aggregator.
- A specific [error taxonomy](#error-handling) (rate limited, auth, timeout,
  content filter, context length, overloaded) with `Retry-After` parsed.
- Configurable [timeouts, retry, proxy, TLS and auth](#provideroptions) per provider.
- [Usage and cost](#usage-and-cost) accounting, including cached tokens.
- [Observation hooks](#observability) with ready-made adapters for `obs`,
  `metrics` and `log`.
- [Embeddings](#embeddings) with automatic batching, and a small
  [RAG helper](#rag-helper-vectorstore-feature) over [vectorstore](vectorstore.md).
- [Prompt templates](#prompt-templates) with strict rendering, sections,
  partials, versions and directory loading.
- Three built-in providers — [OpenAI](#openaiprovider) (also Azure OpenAI and
  OpenAI-compatible servers), [Claude](#claudeprovider), [Ollama](#ollamaprovider) —
  and a scripted [`MockProvider`](#mockprovider) for your tests.

## Overview

Everything is built on one object-safe trait. A `CompletionRequest` (model +
messages + tools + options) goes in; a `CompletionResponse` (or a `ChunkStream`
of `CompletionChunk` events) comes out. Each provider translates that neutral
shape to and from its wire protocol using [`ferroly::codec::Value`](codec.md) as
the JSON DOM, sends it over [`ferroly::http::Client`](http.md), and applies
credentials through the auth traits in [`clients`](clients.md).

The design deliberately avoids a few patterns that add indirection without value:

- **No registry.** You construct the provider directly (`OpenAiProvider::new(...)`)
  or store an `Arc<dyn GenAiProvider>`. There is no global name→provider lookup.
- **Typed `Options`.** Rather than a stringly-typed map, options are a struct of
  typed fields; `custom: HashMap<String, Value>` carries anything else into the
  request body.
- **No hidden behaviour.** An option or response format a provider cannot honour
  is documented as not sent, or is an error — it is never silently dropped
  where that would change what you get back.

## Enabling

The umbrella `genai` feature pulls in the trait surface, message model, options,
requests, responses, embeddings, the router, prompt templates and the mock
provider. It implies `codec`, `clients`, `http`, and `tokio`. The concrete
providers are each gated behind their own feature so you only compile the vendor
you use:

```toml
[dependencies]
# Just the OpenAI provider (turns on `genai` transitively):
ferroly = { version = "*", features = ["openai"] }

# Several providers at once:
ferroly = { version = "*", features = ["openai", "claude", "ollama"] }
# or the convenience group:
ferroly = { version = "*", features = ["all-providers"] }
```

| Feature          | Turns on                                                    |
| ---------------- | ----------------------------------------------------------- |
| `genai`          | `codec`, `clients`, `http`, `tokio` — the vendor-neutral API |
| `openai`         | `genai` + [`OpenAiProvider`](#openaiprovider) (+ embeddings) |
| `claude`         | `genai` + [`ClaudeProvider`](#claudeprovider)               |
| `ollama`         | `genai` + [`OllamaProvider`](#ollamaprovider) (+ embeddings) |
| `all-providers`  | `openai` + `claude` + `ollama`                              |

Optional integrations, each compiled in only when you also enable that feature:

| With feature   | You also get |
| -------------- | ------------ |
| `obs`          | `ObsObserver` (spans + trace-context propagation) |
| `metrics`      | `MetricsObserver` |
| `log`          | `LogObserver`, router decision logging, stale-price warnings |
| `vectorstore`  | `genai::rag` |

## Quick start

```rust
use ferroly::genai::{CompletionRequest, GenAiProvider, Message, OpenAiProvider};

#[tokio::main]
async fn main() -> Result<(), ferroly::genai::GenAiError> {
    let provider = OpenAiProvider::new("sk-...", None);

    let request = CompletionRequest::builder("gpt-4o")
        .message(Message::system("You are terse."))
        .message(Message::user("Say hello in French."))
        .build();

    let response = provider.complete(request).await?;
    println!("{}", response.text());
    Ok(())
}
```

## API reference

Re-exported from `ferroly::genai`:

| Item | Kind | Summary |
| ---- | ---- | ------- |
| [`GenAiProvider`](#the-genaiprovider-trait) | trait | The vendor-neutral LLM backend. |
| [`BoxFuture`](#boxfuture-chunkstream-capability) | type alias | Boxed `Send` future returned by the async trait methods. |
| [`ChunkStream`](#boxfuture-chunkstream-capability) | type alias | `mpsc::Receiver` of streaming events. |
| [`Capability`](#boxfuture-chunkstream-capability) | enum | `Text` / `Chat` / `Streaming` / `Vision` / `Audio` / `ToolUse` / `JsonMode` / `Embeddings` / `Reasoning`. |
| [`Message`](#messages-role-messagepart) / [`Role`](#messages-role-messagepart) / [`MessagePart`](#messages-role-messagepart) | struct / enum / enum | Role-tagged, multi-part message. |
| [`Options`](#options-and-optionsbuilder) / `OptionsBuilder` | struct | Typed generation options + `custom` pass-through. |
| `ReasoningEffort`, `PromptCache`, `CacheTtl` | enum / struct / enum | Reasoning and prompt-caching controls. |
| [`CompletionRequest`](#completionrequest-and-its-builder) / `CompletionRequestBuilder` | struct | Model + messages + tools + options. |
| [`ToolDefinition`](#function-calling-tooldefinition--toolchoice) / [`ToolChoice`](#function-calling-tooldefinition--toolchoice) | struct / enum | A callable tool; how the model may use tools. |
| [`ResponseFormat`](#structured-output-responseformat--decode) / `JsonSchemaFormat` | enum / struct | `Text` / `Json` / `JsonSchema` / `Schema`. |
| [`CompletionResponse`](#completionresponse-finishreason-grounding) | struct | Non-streaming result; `.text()`, `.tool_calls()`, `.decode::<T>()`. |
| [`FinishReason`](#completionresponse-finishreason-grounding) | enum | Normalized stop reason. |
| [`Grounding`](#completionresponse-finishreason-grounding) / `Citation` | struct | Citations, where the provider returns them. |
| [`Usage`](#usage-and-cost) | struct | Token accounting, cached tokens included. |
| [`CompletionChunk`](#streaming) / `ToolCallDelta` | enum / struct | One streaming event. |
| [`StreamAccumulator`](#folding-a-stream-into-a-response) / `collect_stream` / `response_to_chunks` | struct / fn / fn | Stream ⇄ response conversion. |
| [`Embedder`](#embeddings) / `EmbedRequest` / `EmbedResponse` | trait / struct | Text → vector embedding. |
| [`PromptTemplate`](#prompt-templates) / `PromptStore` / `InMemoryPromptStore` | struct / trait / struct | Named, versioned prompts. |
| [`template`](#prompt-templates) | module | The template engine (`Template`, `RenderOptions`). |
| [`GenAiError`](#error-handling) / `TimeoutPhase` / `RateLimitInfo` | enum / enum / struct | The error type. |
| [`GenAiObserver`](#observability) / `MultiObserver` / `RequestInfo` / `RequestOutcome` / `Operation` / `PayloadDirection` / `redact_payload` | trait / … | Observation hooks. |
| `ObsObserver` / `MetricsObserver` / `LogObserver` / `PayloadLogging` | struct | Adapters behind `obs` / `metrics` / `log`. |
| [`MockProvider`](#mockprovider) / `MockReply` | struct / enum | Scripted provider for tests. |
| `OpenAiProvider` / `OpenAiConfig` / `AuthStyle` / `ClaudeProvider` / `ClaudeProviderConfig` / `OllamaProvider` | struct | [Concrete providers](#providers), each behind its feature. |
| [`ProviderOptions`](#provideroptions) / `Timeouts` | struct | Common provider settings. |
| [`rag`](#rag-helper-vectorstore-feature) | module | `RagIndex` (feature `vectorstore`). |
| [`router`](model-router.md) | module | The model router. `ModelInfo`, `ModelRouter`, `RouterError` are re-exported at the top level. |

The auth traits `ApiKeyAuth`, `AuthProvider`, `BasicAuth`, `BearerAuth` are also
re-exported from `ferroly::genai` for convenience (they live in
[clients](clients.md)).

## The `GenAiProvider` trait

```rust
pub trait GenAiProvider: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str { "" } // defaulted

    fn complete(
        &self,
        request: CompletionRequest,
    ) -> BoxFuture<'_, Result<CompletionResponse, GenAiError>>;

    fn complete_stream(
        &self,
        request: CompletionRequest,
    ) -> BoxFuture<'_, Result<ChunkStream, GenAiError>>;

    fn supports(&self, capability: Capability) -> bool;

    fn model_catalog(&self) -> Vec<ModelInfo> { Vec::new() }                 // defaulted
    fn model_info_for(&self, model: &str) -> Option<ModelInfo> { /* scans the catalog */ }
}
```

- `name()` — the provider id (`"openai"`, `"claude"`, `"ollama"`, or the name you
  gave an OpenAI-compatible endpoint).
- `complete()` — one non-streaming turn.
- `complete_stream()` — a streaming turn; see [Streaming](#streaming).
- `supports(cap)` — feature probe used to guard optional behavior.
- `model_catalog()` / `model_info_for(model)` — per-model metadata (capabilities,
  limits, prices, ranks) consumed by the [model router](model-router.md) and by
  cost accounting. See [Model catalogs](#model-catalogs).

The trait is object-safe so `Arc<dyn GenAiProvider>` works. The async methods
return a [`BoxFuture`](#boxfuture-chunkstream-capability) — the manual desugaring
of an `async fn` — which is why implementations wrap `Box::pin(async move { ... })`
instead of pulling in the `async-trait` crate.

```rust
use std::sync::Arc;
use ferroly::genai::{ClaudeProvider, GenAiProvider, OpenAiProvider};

// Pick a backend at runtime behind the trait object — no registry needed.
fn choose(vendor: &str) -> Arc<dyn GenAiProvider> {
    match vendor {
        "claude" => Arc::new(ClaudeProvider::new("sk-ant-...", None)),
        _ => Arc::new(OpenAiProvider::new("sk-...", None)),
    }
}
```

## `BoxFuture`, `ChunkStream`, `Capability`

```rust
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// A stream of streaming events, delivered over a tokio channel.
pub type ChunkStream = tokio::sync::mpsc::Receiver<Result<CompletionChunk, GenAiError>>;

#[non_exhaustive]
pub enum Capability { Text, Chat, Streaming, Vision, Audio, ToolUse, JsonMode, Embeddings, Reasoning }
```

`supports()` reports these per provider (per-model detail is in the catalog):

| Provider | `Text`/`Chat` | `Streaming` | `ToolUse` | `Vision` | `JsonMode` | `Embeddings` | `Reasoning` |
| -------- | :--: | :--: | :--: | :--: | :--: | :--: | :--: |
| OpenAI   | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | — |
| Claude   | ✅ | ✅ | ✅ | ✅ | ❌ (schema only) | ❌ | ✅ |
| Ollama   | ✅ | ✅ | ✅ (model-dependent) | ✅ (model-dependent) | ✅ | ✅ | — |

## Messages: `Role`, `MessagePart`

```rust
pub struct Message {
    pub role: Role,
    pub id: Option<String>,
    pub parts: Vec<MessagePart>,
}

pub enum Role { System, User, Assistant, Tool }

#[non_exhaustive]
pub enum MessagePart {
    Text(String),
    Image { data: Vec<u8>, mime_type: String },
    FileRef { uri: String, mime_type: String },
    ToolCall { id: String, name: String, arguments: Value },
    ToolResult { call_id: String, result: Value },
    ProviderData { provider: String, data: Value },
}
```

A message carries an ordered list of parts, so one message can mix, say, text
and an image, or text and several tool calls.

| Method | Builds |
| ------ | ------ |
| `Message::user(text)` / `system(text)` / `assistant(text)` | a text message, no id |
| `Message::text(role, id, text)` | a single `Text` part with an id |
| `Message::binary(role, id, bytes, mime_type)` | a single `Image` part |
| `Message::file_ref(role, id, uri, mime_type)` | a single `FileRef` part |
| `Message::json(role, id, &val)` | text part = the JSON [`Encode`](codec.md) of `val` |
| `Message::tool_call(id, name, arguments)` | an assistant message requesting one tool call |
| `Message::tool_result(call_id, result)` | a `Role::Tool` message answering a call |
| `msg.add_text_part(..)` / `add_binary_part(..)` / `add_tool_call(..)` / `add_tool_result(..)` | append a part (returns `&mut Self`) |
| `msg.text_content()` | concatenates all `Text` parts |
| `msg.tool_calls()` | the tool calls as `(id, name, arguments)` |

**`ProviderData`** is a provider-specific content block that has no normalized
form — a Claude thinking block and its signature, a server-tool block, and so
on. Providers return such blocks so that a multi-turn conversation can send them
back **unchanged**: append `response.message` to your history as it is. A block
is re-sent only to the provider named in it and skipped by every other.

`FileRef` is sent as an image URL to OpenAI; to Claude as a URL image or PDF
source when the URI is `http(s)://` with an `image/*` or `application/pdf` MIME
type, and as text otherwise; Ollama ignores it.

## `Options` and `OptionsBuilder`

```rust
pub struct Options {
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub top_k: Option<u32>,
    pub stop: Vec<String>,
    pub seed: Option<i64>,
    pub presence_penalty: Option<f32>,
    pub frequency_penalty: Option<f32>,
    pub user: Option<String>,
    pub parallel_tool_calls: Option<bool>,
    pub reasoning_effort: Option<ReasoningEffort>, // Low | Medium | High | XHigh | Max
    pub prompt_cache: Option<PromptCache>,
    pub system_instructions: Option<String>,
    pub custom: HashMap<String, Value>,
}
```

Build it with a struct literal and `..Default::default()`, direct field
assignment, or the fluent `OptionsBuilder` (`Options::builder()`), which has a
setter per field plus `stop(seq)` (adds one) and `stop_sequences(iter)`.

What each provider receives for each typed option ("—" = the provider has no
equivalent and the option is **not sent**):

| Option | OpenAI | Claude | Ollama |
| ------ | ------ | ------ | ------ |
| `max_tokens` | `max_completion_tokens` (`max_tokens` with [`legacy_max_tokens`](#openaiprovider)) | `max_tokens` ([defaulted](#claudeprovider) when unset) | `options.num_predict` |
| `temperature`, `top_p` | same names | same names (unless [`omit_sampling_params`](#claudeprovider)) | `options.*` |
| `top_k` | — | `top_k` (same switch) | `options.top_k` |
| `stop` | `stop` | `stop_sequences` | `options.stop` |
| `seed` | `seed` | — | `options.seed` |
| `presence_penalty`, `frequency_penalty` | same names | — | `options.*` |
| `user` | `user` | `metadata.user_id` | — |
| `parallel_tool_calls` | `parallel_tool_calls` (only with tools) | `false` → `tool_choice.disable_parallel_tool_use: true` | — |
| `reasoning_effort` | `reasoning_effort` (`XHigh`/`Max` are sent as `high`) | `output_config.effort` | — (use `custom`, e.g. `think`) |
| `prompt_cache` | — (caching is automatic) | `cache_control` (see below) | — |
| `system_instructions` | a leading `system` message | merged into `system` | a leading `system` message |

### `custom`: anything else

`custom` keys are merged into the **top level of the request body**, so every
provider parameter is reachable without waiting for a typed field:

```rust
use ferroly::genai::Options;

let opts = Options::builder()
    .seed(7)
    .custom("logprobs", true)                 // OpenAI
    .custom("service_tier", "flex")
    .custom("seed", 99i64)                    // ignored: the typed `seed` wins
    .build();
```

The precedence rules:

1. **A typed option wins** over a custom key of the same name.
2. When both are JSON **objects**, the custom entries are added to the typed
   ones (typed entries win). This is how you add `options.num_ctx` on Ollama or
   `output_config.task_budget` on Claude next to what the provider built.
3. **Provider-owned keys cannot be set**: `model`, `messages`, `stream` (and
   `system` on Claude). A request that tries fails with `GenAiError::Config`
   before anything is sent.

Keys are applied in sorted order, so the body is deterministic.

### Prompt caching

`PromptCache` matters for providers where caching is opt-in (Claude):

```rust
use ferroly::genai::{CacheTtl, Options, PromptCache};

// Cache the whole conversation prefix (the usual choice for multi-turn chats):
let opts = Options::builder().prompt_cache(PromptCache::auto()).build();

// Cache only a large, stable system prompt, for one hour:
let opts = Options::builder()
    .system_instructions("...long instructions...")
    .prompt_cache(PromptCache::system().ttl(CacheTtl::OneHour))
    .build();
```

`conversation` sends a top-level `cache_control` (the API places the breakpoint
on the last cacheable block); `system` sends the system prompt as a block with
its own `cache_control`. Cached-token counts come back in [`Usage`](#usage-and-cost)
on every provider that reports them.

## `CompletionRequest` and its builder

```rust
pub struct CompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub tool_choice: Option<ToolChoice>,
    pub response_format: Option<ResponseFormat>,
    pub options: Options,
}
```

`CompletionRequest::new(model, messages)` for the bare case, or the builder:
`.message(m)`, `.messages(vec)`, `.tool(t)`, `.tool_choice(c)`,
`.response_format(f)`, `.options(o)`, `.build()`.

## Function calling: `ToolDefinition` + `ToolChoice`

```rust
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value, // JSON Schema
    pub strict: bool,      // provider-enforced schema (OpenAI, Claude)
}

pub enum ToolChoice {
    Auto,          // model decides (default when tools are present)
    None,          // model must not call a tool
    Required,      // model must call some tool
    Named(String), // model must call this specific tool
}
```

`ToolDefinition::new(name, description, parameters)` builds a non-strict tool;
chain `.strict(true)` to ask the provider to guarantee the arguments match the
schema (the schema must then follow the provider's strict-schema rules).

All three providers send tools, encode the assistant's tool calls and your
tool results, and parse tool calls — including **parallel** calls — from the
answer:

| | OpenAI | Claude | Ollama |
| --- | --- | --- | --- |
| Assistant tool call | `tool_calls: [{id, type: "function", function: {name, arguments: "<JSON string>"}}]`, `content: null` when there is no text | `tool_use` content blocks | `tool_calls: [{function: {name, arguments: {…}}}]` |
| Tool result | one `{role: "tool", tool_call_id, content}` message per result | `tool_result` blocks, all results of a round in **one** user message | `{role: "tool", content, tool_name}` |
| `ToolChoice` | `"auto"` / `"none"` / `"required"` / `{type: function, …}` | `{type: auto}` / `{type: none}` / `{type: any}` / `{type: tool, name}` | not sent |

A tool result that is a string is sent as is; any other value is sent as its
JSON encoding. Ollama does not assign call ids: the provider synthesizes
`call_0`, `call_1`, … and matches results to calls by tool name.

> Current Claude models reject forced tool use (`Required` / `Named`) with a
> `400`. Use `Auto`, name the tool in the prompt, and set `strict(true)`.

The loop: send tools, read the tool calls, run them, append the assistant
message **as returned** and one result per call, and ask again.

```rust
use ferroly::codec::json;
use ferroly::genai::{
    CompletionRequest, FinishReason, GenAiProvider, Message, OpenAiProvider, ToolDefinition,
};

#[tokio::main]
async fn main() -> Result<(), ferroly::genai::GenAiError> {
    let provider = OpenAiProvider::new("sk-...", None);
    let weather = ToolDefinition::new(
        "get_weather",
        "Look up the current weather for a city",
        json::from_str(
            r#"{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}"#,
        )
        .unwrap(),
    );

    let mut messages = vec![Message::user("What's the weather in Paris and Rome?")];
    loop {
        let request = CompletionRequest::builder("gpt-4o")
            .messages(messages.clone())
            .tool(weather.clone())
            .build();
        let response = provider.complete(request).await?;
        if response.finish_reason != Some(FinishReason::ToolCalls) {
            println!("{}", response.text());
            return Ok(());
        }
        // Run every requested call (they may be parallel) …
        let results: Vec<Message> = response
            .tool_calls()
            .into_iter()
            .map(|(id, name, arguments)| {
                let city = arguments.get("city").and_then(|c| c.as_str()).unwrap_or("?");
                println!("calling {name}({city})");
                Message::tool_result(id, format!("18C and sunny in {city}"))
            })
            .collect();
        // … then send the assistant turn back unchanged, followed by the results.
        messages.push(response.message);
        messages.extend(results);
    }
}
```

Tool arguments are parsed strictly: arguments that are not valid JSON (what a
call cut off by the token limit looks like) are a `GenAiError::ResponseParse`,
never a silent `null`. Always validate arguments against your schema before
running a tool.

## Structured output: `ResponseFormat` + `decode`

```rust
#[non_exhaustive]
pub enum ResponseFormat {
    Text,                     // free-form (default)
    Json,                     // provider "JSON mode", no schema
    JsonSchema(Value),        // shorthand: name "response", not strict
    Schema(JsonSchemaFormat), // { name, description, schema, strict }
}
// ResponseFormat::strict_schema(name, schema)  -> Schema { strict: true, .. }
```

| Variant | OpenAI | Claude | Ollama |
| ------- | ------ | ------ | ------ |
| `Json` | `response_format: {type: "json_object"}` | **`GenAiError::Unsupported`** — the Messages API has no schema-less JSON mode; pass a schema | `format: "json"` |
| `JsonSchema` / `Schema` | `response_format: {type: "json_schema", json_schema: {name, schema, strict?}}` | `output_config.format: {type: "json_schema", schema}` | `format: <schema>` |

`strict: true` is sent to OpenAI when requested. Claude's structured outputs are
always enforced and require `additionalProperties: false` on every object.

`CompletionResponse::decode::<T>()` JSON-[`Decode`](codec.md)s the response text
(`decode_value()` returns the untyped `Value`). It ignores surrounding
whitespace and **one Markdown code fence** around the document
(```` ```json … ``` ````), which models add even in JSON mode. Anything else —
prose before or after the JSON, an empty answer, JSON of the wrong shape — is a
`GenAiError::ResponseParse` that says what was found, and mentions truncation
when the finish reason is `Length`.

```rust
use ferroly::codec::json;
use ferroly::genai::{CompletionRequest, GenAiProvider, Message, OpenAiProvider, ResponseFormat};

#[derive(ferroly::codec::Decode)]
struct Sentiment { label: String, score: f32 }

#[tokio::main]
async fn main() -> Result<(), ferroly::genai::GenAiError> {
    let provider = OpenAiProvider::new("sk-...", None);
    let schema = json::from_str(
        r#"{"type":"object","properties":{"label":{"type":"string"},"score":{"type":"number"}},
            "required":["label","score"],"additionalProperties":false}"#,
    )
    .unwrap();

    let request = CompletionRequest::builder("gpt-4o")
        .message(Message::user("Classify: I absolutely love this crate!"))
        .response_format(ResponseFormat::strict_schema("sentiment", schema))
        .build();

    let parsed: Sentiment = provider.complete(request).await?.decode()?;
    println!("{} ({:.2})", parsed.label, parsed.score);
    Ok(())
}
```

## `CompletionResponse`, `FinishReason`, `Grounding`

```rust
#[non_exhaustive]
pub struct CompletionResponse {
    pub id: Option<String>,                 // the provider's response id
    pub model: String,                      // the model that served the call
    pub message: Message,                   // assistant reply
    pub finish_reason: Option<FinishReason>,
    pub raw_finish_reason: Option<String>,  // the provider's own string
    pub usage: Option<Usage>,
    pub reasoning: Option<String>,          // thinking text, if returned
    pub grounding: Option<Grounding>,       // citations, if returned
}

#[non_exhaustive]
pub enum FinishReason { Stop, Length, ToolCalls, ContentFilter, Error, Other }
```

The struct is `#[non_exhaustive]`: build one (in a custom provider or a test)
with `CompletionResponse::new(model, message)` or `from_text(model, text)` and
the `with_finish_reason` / `with_raw_finish_reason` / `with_usage` / `with_id` /
`with_reasoning` / `with_grounding` setters.

`FinishReason` is mapped from each provider's vocabulary; the original string is
kept in `raw_finish_reason`:

| `FinishReason` | OpenAI | Claude | Ollama |
| --- | --- | --- | --- |
| `Stop` | `stop` | `end_turn`, `stop_sequence` | `stop` |
| `Length` | `length` | `max_tokens` | `length` |
| `ToolCalls` | `tool_calls`, `function_call` | `tool_use` | `stop` with tool calls present |
| `ContentFilter` | `content_filter`, or a `refusal` message | `refusal` | — |
| `Other` | anything else | `pause_turn`, … | `load`, … |

**`Length` means the answer is truncated.** `response.is_truncated()` tests for
it; check it before trusting or decoding the text.

Helpers: `text()`, `tool_calls()`, `is_truncated()`, `decode::<T>()`,
`decode_value()`.

`Grounding { citations: Vec<Citation> }` is populated from Claude citation
blocks and OpenAI `url_citation` annotations; each `Citation` has optional
`cited_text`, `title`, `url`, `document_index`, `start`, `end`.

## Usage and cost

```rust
pub struct Usage {
    pub prompt_tokens: Option<u32>,      // total input, cached tokens included
    pub completion_tokens: Option<u32>,  // total output, reasoning tokens included
    pub total_tokens: Option<u32>,
    pub cached_tokens: Option<u32>,      // prompt tokens read from the cache
    pub cache_write_tokens: Option<u32>, // prompt tokens written to the cache
    pub reasoning_tokens: Option<u32>,   // output tokens spent reasoning
}
```

The counts are normalized: `prompt_tokens` is always the **whole** prompt.
Claude reports the uncached remainder separately from cache reads and writes;
the provider adds them up. `Usage::new(prompt, completion)` and `usage.merge(&other)`
are available for custom providers.

`ModelInfo::cost_for(&usage)` turns usage into USD at a catalog entry's prices,
billing cached and cache-write tokens at their own prices when the entry has
them:

```rust
use ferroly::genai::{GenAiProvider, OpenAiProvider};

# fn demo(provider: &OpenAiProvider, response: &ferroly::genai::CompletionResponse) {
if let (Some(info), Some(usage)) = (provider.model_info_for(&response.model), &response.usage) {
    println!("this call cost about ${:.5}", info.cost_for(usage).unwrap_or(0.0));
}
# }
```

The same figure is handed to [observers](#observability) and, with the
registry's (overridable) prices, to the [router's report](model-router.md#cost-budgets-and-reports).

### Model catalogs

Each built-in provider ships a small catalog of `ModelInfo` entries.
**They are defaults to override, not a source of truth**: model ids, limits
and prices change faster than a library is released.

- Every entry carries the date it was correct on (`ModelInfo::as_of`).
- The Claude entries (`claude-fable-5-2`, `claude-opus-5-5`, `claude-sonnet-5-5`,
  `claude-haiku-5-5`, `claude-haiku-4-5`) are dated 2026-11-03.
- The OpenAI entries (`gpt-4o`, `gpt-4o-mini`, `text-embedding-3-small`) are
  dated 2024-10-01 and will be reported as stale — override them.
- The Ollama entries are placeholders: what exists depends on what your server
  has pulled.

Override prices, ranks and capabilities, and add models, through the
[router configuration](model-router.md#configuration-yaml) (`cost_overrides`,
`rank_overrides`, `custom_models`) or `OpenAiConfig::catalog`. When the router's
registry is built, any entry whose compiled-in price is older than 180 days and
has not been overridden is [reported once](model-router.md#stale-prices).

## Streaming

`complete_stream()` returns a `ChunkStream` — a channel of
`Result<CompletionChunk, GenAiError>`. A background task reads the HTTP body,
reassembles it, decodes each event and forwards it.

```rust
#[non_exhaustive]
pub enum CompletionChunk {
    Start { id: Option<String>, model: Option<String> },
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallDelta(ToolCallDelta),               // { index, id, name, arguments }
    ProviderData { provider: String, data: Value },
    Usage(Usage),
    Done { finish_reason: FinishReason, raw_finish_reason: Option<String> },
}
```

| Event | Meaning |
| ----- | ------- |
| `Start` | the stream opened; response id and serving model where the provider sends them |
| `TextDelta` | more answer text |
| `ReasoningDelta` | more thinking text (never part of the answer) |
| `ToolCallDelta` | a fragment of a tool call. Fragments of one call share an `index`; `id` and `name` arrive on the first one; `arguments` are pieces of one JSON document |
| `ProviderData` | a complete provider-specific block (e.g. a Claude thinking block with its signature) |
| `Usage` | token counts; may arrive more than once (input first, output last) — later reports overlay earlier ones |
| `Done` | the model stopped, with the reason |
| `Err(_)` | the stream failed; nothing follows |

Accessors keep simple consumers simple: `chunk.text()` is the text of a
`TextDelta` and `""` for everything else; `tool_call()`, `usage()`,
`finish_reason()` and `is_done()` pick out the other events.

```rust
use ferroly::genai::{CompletionRequest, GenAiProvider, Message, OpenAiProvider};

#[tokio::main]
async fn main() -> Result<(), ferroly::genai::GenAiError> {
    let provider = OpenAiProvider::new("sk-...", None);
    let request = CompletionRequest::builder("gpt-4o")
        .message(Message::user("Write a haiku about iron."))
        .build();

    let mut stream = provider.complete_stream(request).await?;
    while let Some(item) = stream.recv().await {
        let chunk = item?;                 // a mid-stream failure is an Err
        print!("{}", chunk.text());        // "" for non-text events
        if let Some(reason) = chunk.finish_reason() {
            println!("\n[done: {reason}]");
        }
    }
    Ok(())
}
```

What each provider's stream is decoded from:

- **OpenAI** — SSE. `delta.content`, `delta.tool_calls` (with indexes),
  `delta.refusal`, `finish_reason`, and — because the request sends
  `stream_options: {include_usage: true}` — the final usage event. An
  `{"error": …}` event is an `Err`.
- **Claude** — SSE. `message_start` (id, model, input usage),
  `content_block_start` / `content_block_delta` (`text_delta`,
  `input_json_delta`, `thinking_delta`, `signature_delta`) /
  `content_block_stop`, `message_delta` (stop reason, output usage),
  `message_stop`, `ping` (ignored) and `error` (an `Err` of the matching
  variant, e.g. `Overloaded`).
- **Ollama** — NDJSON. `message.content`, `message.thinking`,
  `message.tool_calls` (whole calls), `done` / `done_reason` with the token
  counts, and `{"error": …}` lines (an `Err`).

### Guarantees

- **A failed stream never looks like a short success.** A provider error event,
  a transport failure, an idle timeout, and a body that ends before the provider
  signalled completion all arrive as an `Err` item.
- **Byte-exact reassembly.** The body is buffered as bytes and only complete
  lines are decoded, so a multi-byte UTF-8 character, a line, or an SSE event
  split across network chunks comes out intact. `\n`, `\r\n` and `\r` line
  endings, comment lines and multi-line `data:` fields are handled. A single
  line longer than 8 MiB fails the stream.
- **Cancellation.** Drop the receiver and the reader task stops and closes its
  connection.
- **Idle timeout.** A stream that goes silent for longer than
  `Timeouts::stream_idle` (default 5 minutes) ends with
  `GenAiError::Timeout { phase: StreamIdle }`. (A `Timeouts::total` deadline
  that passes while a stream is being read is reported with the same phase.)
- **No size cap by default.** The HTTP client's response-body cap is meant for
  buffered responses and is lifted for streams; a stream is bounded by the idle
  timeout, the per-line limit, and `ProviderOptions::max_stream_bytes` if you
  set it.

### Folding a stream into a response

```rust
use ferroly::genai::{collect_stream, StreamAccumulator};

# async fn demo(provider: &dyn ferroly::genai::GenAiProvider, request: ferroly::genai::CompletionRequest)
# -> Result<(), ferroly::genai::GenAiError> {
// All at once:
let response = collect_stream(provider.complete_stream(request.clone()).await?).await?;

// Or while you render the text as it arrives:
let mut acc = StreamAccumulator::new();
let mut stream = provider.complete_stream(request).await?;
while let Some(item) = stream.recv().await {
    let chunk = item?;
    print!("{}", chunk.text());
    acc.push(chunk);
}
let response = acc.finish()?; // text, tool calls (arguments parsed), usage, finish reason
# let _ = response; Ok(()) }
```

`StreamAccumulator` concatenates text, joins tool-call fragments per index and
parses their arguments, merges usage reports, and keeps the parts in the order
the provider produced them — so `response.message` can go straight back into
the conversation. `finish()` fails if a tool call's arguments are not valid
JSON. `response_to_chunks(&response)` is the inverse.

## Error handling

```rust
#[non_exhaustive]
pub enum GenAiError {
    Template(String),
    TemplateNotFound(String),
    Unsupported { provider: String, capability: Capability },
    Transport(String),                                   // connection, TLS, protocol
    Api { status: u16, message: String },                // any other non-2xx
    ResponseParse(String),
    Config(String),
    Timeout { phase: TimeoutPhase },                     // Connect | Response | StreamIdle
    RateLimited { message: String, retry_after: Option<Duration>, limits: RateLimitInfo },
    Auth { status: u16, message: String },               // 401 / 403 / out of quota
    ContentFilter { message: String },
    ContextLengthExceeded { message: String },
    Overloaded { status: u16, message: String, retry_after: Option<Duration> },
}
```

Provider failures are mapped from the HTTP status **and** the provider's error
body to the most specific variant:

| Variant | OpenAI | Claude | Ollama |
| ------- | ------ | ------ | ------ |
| `RateLimited` | `429` | `429` / `rate_limit_error` | `429` |
| `Auth` | `401`, `403`, `insufficient_quota` | `401`, `403`, `billing_error` | `401`, `403` |
| `Overloaded` | `503`, `529` | `529` / `overloaded_error`, `503` | `503` |
| `ContextLengthExceeded` | `context_length_exceeded` | "prompt is too long", `413` | — |
| `ContentFilter` | `content_filter`, `content_policy_violation` | — (a refusal is a `ContentFilter` finish reason) | — |
| `Api` | everything else | everything else | everything else |

- `retry_after` is parsed from `Retry-After` (seconds or an HTTP date) or
  `retry-after-ms`.
- `RateLimitInfo` holds every response header whose name contains `ratelimit`,
  verbatim: `limits.get("x-ratelimit-remaining-requests")`.
- `Api::message` is `"<provider error type>: <message>"` when the provider sent
  a type.

Helpers on `GenAiError`:

| Method | Returns |
| ------ | ------- |
| `status()` | the HTTP status, when there was one |
| `retry_after()` | the delay the provider asked for |
| `is_timeout()` | any `Timeout` |
| `is_transient()` | the same call may succeed later |
| `is_caller_error()` | the request itself is at fault |
| `kind_label()` | a stable label (`"rate_limited"`, `"auth"`, `"timeout"`, …) for metrics and logs |
| `error_class()` | the `clients::ErrorClass` (retry decision + breaker outcome) used by retry and the router |

```rust
use ferroly::genai::GenAiError;

# async fn demo(provider: &dyn ferroly::genai::GenAiProvider, request: ferroly::genai::CompletionRequest) {
match provider.complete(request).await {
    Ok(response) => println!("{}", response.text()),
    Err(GenAiError::RateLimited { retry_after, .. }) => eprintln!("throttled; retry in {retry_after:?}"),
    Err(GenAiError::Auth { status, .. }) => eprintln!("credentials rejected ({status})"),
    Err(GenAiError::ContextLengthExceeded { .. }) => eprintln!("prompt too long for this model"),
    Err(e) if e.is_timeout() => eprintln!("timed out: {e}"),
    Err(e) if e.is_transient() => eprintln!("temporary failure: {e}"),
    Err(other) => eprintln!("{other}"),
}
# }
```

### Timeouts are not retried

A completion is **not idempotent**: once the request has been sent, the
provider may have generated — and billed — the answer even if you never see it.
So a timeout is its own variant, and only `TimeoutPhase::Connect` (nothing was
sent) counts as transient. A `Response` or `StreamIdle` timeout is not retried
by the provider's optional retry, and the router neither retries it nor fails
it over unless you opt in.

### Secrets

API keys are held in a [`Secret`](hash.md#secret). They never appear in an
error, in `Debug` output of a provider, config or `ProviderOptions` (header
values are not printed either), in observer callbacks, or in logs. A provider
error body is truncated to `ProviderOptions::max_error_body_bytes` (2048) before
it is stored, and the built-in API key is scrubbed from it should the provider
echo it back.

## Providers

### `ProviderOptions`

Common settings of the three HTTP providers. Every field has a working default.

```rust
pub struct ProviderOptions {
    pub base_url: Option<String>,
    pub auth: Option<Arc<dyn AuthProvider>>,      // replaces the built-in key header
    pub headers: Vec<(String, String)>,           // extra default headers
    pub timeouts: Timeouts,
    pub proxy: Option<http::Proxy>,
    pub tls: Option<http::ClientTlsConfig>,
    pub http_client: Option<http::Client>,        // bring your own client
    pub retry: Option<clients::Retry>,            // off by default
    pub observer: Option<Arc<dyn GenAiObserver>>,
    pub max_error_body_bytes: usize,              // 2048
    pub max_response_bytes: Option<u64>,          // 64 MiB (non-streaming)
    pub max_stream_bytes: Option<u64>,            // none
    pub embed_batch_size: Option<usize>,          // provider default
}

pub struct Timeouts {
    pub connect: Option<Duration>,     // 10 s   — DNS + TCP + TLS (+ proxy tunnel)
    pub response: Option<Duration>,    // 10 min — until the response starts
    pub stream_idle: Option<Duration>, // 5 min  — longest silence while reading
    pub total: Option<Duration>,       // none   — the whole exchange
}
```

```text
call ──► [ connect ]──►[ send request ]──►[ wait for response ]──►[ read body / stream … ]
          connect
         └────────────────────── response ────────────────────┘   stream_idle per read
         └──────────────────────────────── total ───────────────────────────────────────┘
```

The defaults suit model calls: a non-streaming completion sends nothing until
the whole answer is generated, so `response` is long. For long outputs prefer
streaming, where `stream_idle` watches for a stalled connection instead.

```rust
use std::sync::Arc;
use std::time::Duration;
use ferroly::clients::{Jitter, RetryPolicy};
use ferroly::genai::{BearerAuth, OpenAiProvider, ProviderOptions};

let opts = ProviderOptions::with_base_url("https://llm-gateway.internal")
    .auth(Arc::new(BearerAuth::new("gateway-token")))   // any AuthProvider, incl. OAuth2
    .header("x-team", "search")
    .connect_timeout(Duration::from_secs(5))
    .response_timeout(Duration::from_secs(120))
    .stream_idle_timeout(Duration::from_secs(60))
    .retry(RetryPolicy::default().with_jitter(Jitter::Full));
let provider = OpenAiProvider::new("unused-key", Some(opts));
```

- **`auth`** — providers call [`clients::authorize`](clients.md#the-authprovider-trait),
  so refreshing credentials (`RefreshingAuth`, `OAuth2ClientCredentials`) work.
- **`proxy` / `tls`** — passed to the [HTTP client](http.md#clientbuilder-timeouts-limits-pool)
  (`http::Proxy`, `http::ClientTlsConfig`). An invalid TLS configuration surfaces
  as `GenAiError::Config` on the first call.
- **`http_client`** — use your own `http::Client` (shared pool, interceptors).
  Its own timeouts apply then; only `stream_idle` is still enforced.
- **`retry`** — retries inside the provider with the given
  [`Retry`](clients.md#retry): transient failures after the backoff, throttling
  and overload after the provider's `Retry-After`. A response timeout is never
  retried. For a stream, only establishing it is retried.
- Setter methods mirror the fields: `base_url`, `auth`, `header`, `timeouts`,
  `connect_timeout`, `response_timeout`, `stream_idle_timeout`, `total_timeout`,
  `proxy`, `tls`, `http_client`, `retry`, `observer`, `embed_batch_size`.

### `OpenAiProvider`

Backs the OpenAI Chat Completions API (`POST {base}/v1/chat/completions`) and the
Embeddings API. Default base `https://api.openai.com`, bearer authentication.

```rust
use ferroly::genai::{AuthStyle, OpenAiConfig, OpenAiProvider, ProviderOptions};

// api.openai.com:
let provider = OpenAiProvider::new("sk-...", None);

// An OpenAI-compatible server (vLLM, LiteLLM, a gateway …):
let gateway = OpenAiProvider::with_config(
    OpenAiConfig::compatible("https://llm.internal", Some("token"))
        .name("gateway")                 // the provider name (for the router)
        .chat_path("/chat/completions")  // default /v1/chat/completions
        .legacy_max_tokens(true)         // send `max_tokens`
        .stream_usage(false),            // do not send `stream_options`
    None,
);

// Azure OpenAI — one provider per deployment:
let azure = OpenAiProvider::with_config(
    OpenAiConfig::azure(
        "https://my-resource.openai.azure.com",
        "my-gpt-4o-deployment",
        "2024-10-21",                    // api-version
        "azure-key",
    ),
    None,
);
```

`OpenAiConfig`:

| Method | Effect |
| ------ | ------ |
| `new(api_key)` | api.openai.com, bearer key |
| `compatible(base_url, Option<key>)` | a compatible server; empty catalog; `None::<&str>` for no auth |
| `azure(endpoint, deployment, api_version, api_key)` | `…/openai/deployments/{deployment}/chat/completions?api-version=…` (and `/embeddings`), key in the `api-key` header; provider name `azure-openai`; empty catalog |
| `name(n)` | the provider's name (default `openai`) |
| `base_url(u)` | the base URL (`ProviderOptions::base_url` takes precedence) |
| `auth_style(AuthStyle)` | `Bearer`, `Header(name)` or `None` |
| `chat_path(p)` / `embeddings_path(p)` | endpoint paths |
| `query_param(k, v)` | a query parameter on every request |
| `legacy_max_tokens(bool)` | send `max_tokens` instead of `max_completion_tokens` |
| `stream_usage(bool)` | whether streams ask for usage (default on) |
| `catalog(Vec<ModelInfo>)` | the models advertised to the router |

Notes:

- The token limit is sent as **`max_completion_tokens`**; OpenAI deprecated
  `max_tokens` and its reasoning models reject it. Turn on `legacy_max_tokens`
  for servers that only know the old name.
- On Azure the deployment selects the model; the request's `model` is
  informational.
- A structured-output refusal (`message.refusal`) becomes the response text with
  `finish_reason = ContentFilter`.

### `ClaudeProvider`

Backs the Anthropic Messages API (`POST {base}/v1/messages`). Default base
`https://api.anthropic.com`, `x-api-key` authentication.

```rust
use std::sync::Arc;
use ferroly::genai::{BearerAuth, ClaudeProvider, ClaudeProviderConfig, ProviderOptions};

let provider = ClaudeProvider::new("sk-ant-...", None);

let custom = ClaudeProvider::with_config(
    ClaudeProviderConfig::new("sk-ant-...")
        .beta("server-side-fallback-2026-07-01") // anthropic-beta
        .omit_sampling_params(true)
        .default_max_tokens(8_000),
    None,
);

// A different credential scheme (Vault-backed key, OAuth bearer token):
let oauth = ClaudeProvider::with_config(
    ClaudeProviderConfig::with_auth(Arc::new(BearerAuth::new("token"))),
    Some(ProviderOptions::with_base_url("https://claude-proxy.internal")),
);
```

`ClaudeProviderConfig`:

| Method | Effect |
| ------ | ------ |
| `new(api_key)` | `x-api-key` header |
| `with_auth(Arc<dyn AuthProvider>)` | any other scheme |
| `api_version(v)` | the `anthropic-version` header (default `2023-06-01`) |
| `beta(b)` | adds a value to `anthropic-beta` |
| `omit_sampling_params(bool)` | never send `temperature` / `top_p` / `top_k` |
| `default_max_tokens(n)` | the `max_tokens` used when a request sets none |
| `eager_tool_streaming(bool)` | mark tools `eager_input_streaming` on streamed requests |

Behaviour to know:

- **System prompt.** `options.system_instructions` and every `Message::system(..)`
  are **merged**: instructions first, then the system messages in order,
  separated by a blank line. Neither is dropped.
- **Messages are merged per role.** Consecutive messages with the same wire role
  become one, so several tool-result messages form the single user message the
  API expects.
- **`max_tokens`** is required by the API. When a request sets none, the
  provider uses the model's catalog output limit, capped at 16 000 for
  non-streaming calls and 64 000 for streams (long non-streaming generations
  run into HTTP timeouts); a model that is not in the catalog gets 4 096.
  `default_max_tokens` overrides all of that. If the answer is still cut off,
  `finish_reason` is `Length`.
- **Structured output** uses `output_config.format`; `ResponseFormat::Json` is
  `GenAiError::Unsupported` (see [above](#structured-output-responseformat--decode)).
- **Thinking.** Current models think by default. `reasoning_effort` sets
  `output_config.effort`; set anything else through `custom`
  (e.g. `custom("thinking", …)`). Thinking blocks come back as
  `MessagePart::ProviderData` and must be sent back unchanged — append
  `response.message` as it is. Thinking text, when the API returns it, is in
  `CompletionResponse::reasoning`.
- **Sampling parameters.** Current Claude models reject non-default
  `temperature` / `top_p` / `top_k`, and API version `2026-10-21` rejects them
  on every model. A request that sets them then fails with a `400`. Set
  `omit_sampling_params(true)` to have the provider leave them out — useful
  when the same request is routed to several providers. The default API version
  stays `2023-06-01`; moving to `2026-10-21` is your decision
  (`api_version("2026-10-21")` together with `omit_sampling_params(true)`).
- **Eager tool-input streaming** is on by default against `api.anthropic.com`
  and off for any other base URL (a gateway may reject the field). With it on
  the API does not validate tool inputs: validate them before running a tool.
- **No embeddings API.**

### `OllamaProvider`

Backs an Ollama server's `POST {base}/api/chat` and `/api/embed`. Default base
`http://localhost:11434`, no auth (set `ProviderOptions::auth` for a server
behind an authenticating proxy).

```rust
use ferroly::codec::json;
use ferroly::genai::{CompletionRequest, Message, OllamaProvider, Options, ProviderOptions};

let provider = OllamaProvider::new(Some(ProviderOptions::with_base_url("http://gpu-box:11434")));

let request = CompletionRequest::builder("llama3.1")
    .message(Message::user("Explain iron oxidation briefly."))
    .options(
        Options::builder()
            .temperature(0.2)
            .custom("keep_alive", "10m")                                // top-level field
            .custom("options", json::from_str(r#"{"num_ctx":8192}"#).unwrap()) // merged into `options`
            .build(),
    )
    .build();
```

Generation options are nested under `options` on the wire. Images are sent in
`images` as base64. Streaming is NDJSON. Tool calling and vision depend on the
model you run.

### `MockProvider`

A scripted, in-memory `GenAiProvider` and `Embedder` for **your** tests — no
network, no API key.

```rust
use ferroly::genai::{CompletionRequest, GenAiError, GenAiProvider, Message, MockProvider};

#[tokio::test]
async fn summarizer_retries_once() {
    let mock = MockProvider::new("mock");
    mock.push_error(GenAiError::Api { status: 503, message: "busy".into() });
    mock.push_text("a summary");

    let request = CompletionRequest::builder("any").message(Message::user("text")).build();
    assert!(mock.complete(request.clone()).await.is_err());
    assert_eq!(mock.complete(request).await.unwrap().text(), "a summary");

    assert_eq!(mock.call_count(), 2);
    assert_eq!(mock.requests()[0].messages[0].text_content(), "text");
}
```

| Method | Effect |
| ------ | ------ |
| `new(name)` | empty script and catalog |
| `with_catalog(models)` | the catalog advertised to a router |
| `with_default_text(t)` | answer with `t` when the script is empty (otherwise an unscripted call fails with `GenAiError::Config`) |
| `with_delay(d)` / `set_delay(Option<d>)` | wait before answering (timeouts, cancellation) |
| `push_text(t)` / `push_response(r)` / `push_error(e)` | queue a reply |
| `push_stream(items)` / `push_stream_text(deltas)` | queue a stream, item by item (an `Err` in the middle is allowed) |
| `push(MockReply)` | the general form |
| `push_embeddings(vectors)` / `push_embed_error(e)` | queue an embeddings reply |
| `requests()` / `last_request()` / `call_count()` / `remaining()` / `embed_requests()` | what was received |

Replies are served in order, one per call, whether the call is `complete` or
`complete_stream`: a scripted response is split into chunks for a stream, and a
scripted stream is folded into a response for `complete`. Unscripted `embed`
calls return deterministic pseudo-embeddings (the same text always gives the
same unit vector), so retrieval code can be exercised without scripting.

## Embeddings

```rust
pub trait Embedder: Send + Sync {
    fn embed(&self, request: EmbedRequest) -> BoxFuture<'_, Result<EmbedResponse, GenAiError>>;
    fn embed_batched(&self, request: EmbedRequest, max_batch: usize)
        -> BoxFuture<'_, Result<EmbedResponse, GenAiError>>; // defaulted
}

pub struct EmbedRequest {
    pub model: String,
    pub input: Vec<String>,
    pub dimensions: Option<u32>, // shorten the vectors, where the model can
    pub user: Option<String>,    // OpenAI
}
pub struct EmbedResponse { pub model: String, pub embeddings: Vec<Vec<f32>>, pub usage: Option<Usage> }
```

`EmbedRequest::new(model, inputs)` / `single(model, text)`, then `.dimensions(n)`
and `.user(u)`. OpenAI calls `/v1/embeddings`; Ollama calls `/api/embed`. Claude
exposes no embeddings API.

**Batching is automatic.** A request with more inputs than the provider's batch
size is split into consecutive requests and the vectors are joined back **in
input order**, with usage summed. The batch size is
`ProviderOptions::embed_batch_size` (default 2048 for OpenAI — the API's limit —
and 128 for Ollama). OpenAI's answers are ordered by their `index` field, and a
batch that returns a different number of vectors than it was given is a
`GenAiError::ResponseParse`, so a short answer cannot shift every later vector
onto the wrong input. `embed_batched(request, max_batch)` gives the same
behaviour over any `Embedder`.

```rust
use ferroly::genai::{EmbedRequest, Embedder, OpenAiProvider};

#[tokio::main]
async fn main() -> Result<(), ferroly::genai::GenAiError> {
    let provider = OpenAiProvider::new("sk-...", None);
    let request = EmbedRequest::new(
        "text-embedding-3-small",
        vec!["iron is a metal".into(), "rust is oxidation".into()],
    )
    .dimensions(256);
    let response = provider.embed(request).await?;
    println!("{} vectors, dim {}", response.embeddings.len(), response.embeddings[0].len());
    Ok(())
}
```

## RAG helper (`vectorstore` feature)

`genai::rag::RagIndex` ties an `Embedder` to a
[`VectorStore`](vectorstore.md): it embeds text and upserts the vectors, and it
embeds a question and searches. Using one object for both guarantees the same
model and dimension on each side.

```rust
use std::sync::Arc;
use ferroly::genai::rag::{RagDocument, RagIndex};
use ferroly::genai::OpenAiProvider;
use ferroly::vectorstore::{Filter, MemoryStore};

# async fn demo() -> Result<(), ferroly::genai::rag::RagError> {
let index = RagIndex::new(
    Arc::new(OpenAiProvider::new("sk-...", None)),
    Arc::new(MemoryStore::new()),
    "text-embedding-3-small",
)
.dimensions(256)
.batch_size(64);

index.upsert(vec![
    RagDocument::new("doc-1", "Rust is a systems programming language."),
    RagDocument::new("doc-2", "Paris is the capital of France."),
]).await?;

for hit in index.query("Which language is good for systems work?", 3).await? {
    println!("{:.3} {} {:?}", hit.score, hit.id, hit.text);
}

// Scope a search with a metadata filter (for example to one tenant):
let _ = index.query_filtered("capital cities", 3, Filter::eq("tenant", "acme")).await?;
# Ok(()) }
```

- `RagDocument::new(id, text)` + `.with_metadata(object)`. The text is stored in
  the document's metadata under `text` (change the key with `text_key`), so a
  hit can return it.
- `upsert` embeds in batches (`batch_size`, default 64) and stores all documents
  in one all-or-nothing upsert; nothing is stored if embedding fails.
- `RagHit { id, score, text, metadata }`.
- `RagError` is `Embed(GenAiError)` or `Store(VectorStoreError)`.

Chunking is the vector store's concern — see
[`vectorstore::Chunker`](vectorstore.md).

## Observability

`GenAiObserver` is a plain trait with no dependency on other ferroly features.
Every method has an empty default.

```rust
pub trait GenAiObserver: Send + Sync {
    fn on_request_start(&self, info: &RequestInfo) {}
    fn on_request_end(&self, info: &RequestInfo, outcome: &RequestOutcome) {}
    fn request_headers(&self, info: &RequestInfo) -> Vec<(String, String)> { vec![] }
    fn wants_payloads(&self) -> bool { false }
    fn on_payload(&self, info: &RequestInfo, direction: PayloadDirection, body: &Value) {}
    fn on_route(&self, report: &router::RouteReport) {}
}
```

- `RequestInfo { id, provider, model, operation }` — `operation` is `Chat`,
  `ChatStream` or `Embed`; `id` ties the callbacks of one call together.
- `RequestOutcome { latency, time_to_first_chunk, usage, cost_usd,
  finish_reason, response_model, error_kind, status, cancelled }`.
  `on_request_end` fires **exactly once** per `on_request_start` — also when the
  call is dropped or a stream is abandoned (`cancelled = true`). For a stream
  it fires when the stream ends, so usage and cost are complete.
- `request_headers` lets an observer add headers to the outgoing request (trace
  propagation).
- `on_route` receives the router's [`RouteReport`](model-router.md#cost-budgets-and-reports).

Install an observer on a provider with `ProviderOptions::observer(..)` and on a
router with `RouterBuilder::observer(..)`; combine several with
`MultiObserver::new(vec![..])`.

### Payloads and redaction

Prompts and completions are **not** handed to observers unless one asks:
`on_payload` is called only when `wants_payloads()` returns `true`, with the
JSON request body and — for non-streaming calls — the response body. Headers are
never passed, so credentials cannot leak this way.

`redact_payload(&body)` returns a copy with every content string replaced by
`"[redacted: N chars]"`, keeping the structure (roles, model, tool names, ids,
numeric options):

```rust
use ferroly::codec::json;
use ferroly::genai::redact_payload;

let body = json::from_str(
    r#"{"model":"m","messages":[{"role":"user","content":"my secret plan"}]}"#,
).unwrap();
let safe = json::to_string(&redact_payload(&body));
assert!(!safe.contains("secret") && safe.contains(r#""role":"user""#));
```

### Built-in adapters

| Adapter | Feature | What it does |
| ------- | ------- | ------------ |
| `ObsObserver::new()` | `obs` | One client span per call via [`obs::genai::chat_span`](obs.md#semantic-convention-helpers) (`chat {model}`; `embeddings {model}` for embeddings) with `gen_ai.provider.name`, `gen_ai.request.model`, `gen_ai.response.model`, `gen_ai.usage.*`, finish reason, cost and `error.type`; sends the span's W3C `traceparent` to the provider; a `genai.route` span per routed call. Prompt and completion text are never recorded. |
| `MetricsObserver::new(&registry)` / `::global()` | `metrics` | `genai_requests_total{provider,model,operation,outcome}`, `genai_request_duration_seconds`, `genai_time_to_first_chunk_seconds`, `genai_tokens_total{…,kind}`, `genai_cost_usd_total`, `genai_route_decisions_total{strategy,provider,model,outcome}`, `genai_route_fallbacks_total`. |
| `LogObserver::new()` | `log` | One structured record per call (debug; failures at warn) and per routing decision. `.payloads(PayloadLogging::Redacted)` or `::Full` turns body logging on; the default is `Off`. |

With the `obs` feature the providers also propagate the **caller's** current
trace context on every request, whether or not an observer is installed.

```rust
use std::sync::Arc;
use ferroly::genai::{GenAiObserver, LogObserver, MetricsObserver, MultiObserver, ObsObserver,
    OpenAiProvider, PayloadLogging, ProviderOptions};

let observer = MultiObserver::new(vec![
    Arc::new(ObsObserver::new()) as Arc<dyn GenAiObserver>,
    Arc::new(MetricsObserver::global()),
    Arc::new(LogObserver::new().payloads(PayloadLogging::Redacted)),
]);
let provider = OpenAiProvider::new(
    "sk-...",
    Some(ProviderOptions::new().observer(Arc::new(observer))),
);
```

## Prompt templates

The `ferroly::genai::template` module is a small template engine for prompts.

| Syntax | Meaning |
| ------ | ------- |
| `{{ name }}`, `{{ user.name }}`, `{{ items.0 }}` | substitute a variable (dot path; numeric segments index arrays) |
| `{{ .name }}` | the same — the Go `text/template` leading dot is tolerated |
| `{{#if x}}…{{else}}…{{/if}}` | conditional on the truthiness of `x` |
| `{{#unless x}}…{{/unless}}` | the inverse |
| `{{#each xs}}…{{else}}…{{/each}}` | repeat for every element of an array, or value of an object; `{{else}}` renders when it is empty |
| `{{ this }}` / `{{ . }}`, `{{ this.field }}` | the current element inside `#each` |
| `{{ @index }}`, `{{ @first }}`, `{{ @last }}`, `{{ @key }}` | loop metadata inside `#each` |
| `{{> name }}` | include the partial template `name` |
| `{{! note }}` | a comment; renders nothing |
| `\{{` | a literal `{{` |

False values are `null`, `false`, `0`, `""`, and empty arrays and objects.
Inside `#each`, a name is looked up in the current element first and then in
the enclosing scopes. Arrays and objects substituted directly render as JSON.

**Substituted values are never parsed as template text.** A variable containing
`{{ secret }}` or `{{#each …}}` is inserted verbatim; it cannot read other
variables or inject logic.

### Strict by default

```rust
use ferroly::genai::template::{RenderOptions, Template};

#[derive(ferroly::codec::Encode)]
struct Vars { name: String, items: Vec<String>, vip: bool }

let t = Template::parse(
    "Hi {{ name }}{{#if vip}} (VIP){{/if}}.\n{{#each items}}{{ @index }}: {{ this }}\n{{/each}}",
).unwrap();
let out = t.render(&Vars {
    name: "Ada".into(),
    items: vec!["tea".into(), "cake".into()],
    vip: true,
}).unwrap();
assert_eq!(out, "Hi Ada (VIP).\n0: tea\n1: cake\n");
```

`Template` — the compiled form and the entry point for new code — renders
**strictly**: a variable that is not defined is an error
(`undefined variable 'nmae'`) instead of a prompt with a silent hole in it. A
variable that is defined but `null` renders as empty, `{{#if x}}` treats an
undefined `x` as false (testing for presence is what it is for), and
`{{#each x}}` over an undefined or non-iterable `x` is an error.

| Entry point | Mode |
| ----------- | ---- |
| `Template::parse(src)?.render(&vars)` / `.render_value(&value)` | strict |
| `Template::render_with(&value, &RenderOptions)` | as configured |
| `template::render_strict(src, &vars)` | strict |
| `PromptTemplate::render_strict(&vars)` | strict |
| `InMemoryPromptStore::render(id, &vars)` / `render_version(id, version, &vars)` | strict, with the store's templates as partials |
| `Message::from_prompt(role, &store, id, &vars)` | strict, with partials |
| `template::render(src, &vars)` / `render_value(src, &value)` | **lenient** (existing behaviour: undefined → empty) |
| `PromptTemplate::render(&vars)`, `Message::from_prompt_id(..)` | **lenient** (existing behaviour) |

`RenderOptions`: `strict()` (default) / `lenient()`, `.allow_root(bool)`,
`.partials(&source)`, `.max_output_bytes(n)` (default 8 MiB).

In **both** modes, `{{ . }}` or an empty tag at the top level no longer dumps
the whole variables object — a prompt must not leak every variable by accident.
It is an error when strict and empty when lenient, unless `allow_root(true)`.

Other limits: blocks nest at most 32 deep, partials at most 16 deep (so a
partial that includes itself is an error), and output is capped.
`Template::variables()` lists the variable names a template refers to.

### Stores, versions, directories

```rust
pub struct PromptTemplate {
    pub id: String,
    pub name: String,
    pub template: String,
    pub version: Option<String>,
}
pub trait PromptStore: Send + Sync {
    fn get(&self, id: &str) -> Option<PromptTemplate>;                          // latest version
    fn get_version(&self, id: &str, version: &str) -> Option<PromptTemplate>;   // defaulted
}
```

`InMemoryPromptStore` keeps every version of every template. `get(id)` returns
the latest: the highest version string, where dot-separated numeric parts
compare as numbers (`1.10` > `1.9`) and an unversioned template ranks lowest.
`add` replaces a template with the same id **and** version.

```rust
use ferroly::genai::{InMemoryPromptStore, Message, PromptStore, PromptTemplate, Role};

#[derive(ferroly::codec::Encode)]
struct Vars { name: String }

let mut store = InMemoryPromptStore::new();
store.add(PromptTemplate::new("signature", "Signature", "-- the team"));
store.add(PromptTemplate::new("greet", "Greeting", "Hi {{ name }}").with_version("1"));
store.add(
    PromptTemplate::new("greet", "Greeting", "Hello {{ name }}!\n{{> signature }}").with_version("2"),
);

let vars = Vars { name: "Ada".into() };
assert_eq!(store.render("greet", &vars).unwrap(), "Hello Ada!\n-- the team");
assert_eq!(store.render_version("greet", "1", &vars).unwrap(), "Hi Ada");
let message = Message::from_prompt(Role::User, &store, "greet", &vars).unwrap();
# let _ = message;
```

Load templates from files with `InMemoryPromptStore::load_dir(path)` (or
`add_dir` on an existing store):

```text
prompts/
  greet.prompt            -> id "greet", unversioned
  greet@2.prompt          -> id "greet", version "2"
  shared/signature.tmpl   -> id "shared/signature"
  support/reply@1.3.md    -> id "support/reply", version "1.3"
```

Files ending in `.prompt`, `.tmpl`, `.txt` or `.md` are read; other files,
hidden files and symbolic links are skipped; sub-directories (up to 8 levels)
become part of the id. Every file is compiled while loading, so a syntax error
is reported there with the file's path. A file larger than 1 MiB or not valid
UTF-8 is an error.

## Limitations

- **Providers:** OpenAI (including Azure OpenAI and compatible servers), Claude
  and Ollama. There are no Bedrock, Vertex AI or Gemini providers.
- **APIs not covered:** the OpenAI Responses API, batch APIs, moderation, image
  and audio generation, file uploads, token counting, and server-side tools
  (their blocks are passed through as `ProviderData`, not modelled).
- **Audio and document input** have no message part of their own; `FileRef`
  covers URL images and PDFs on Claude and URL images on OpenAI.
- **No conversation memory, guardrails framework or agent loop** — the building
  blocks are here; the loop is yours.
- **Tool results have no error flag**; describe a failure in the result text.
- **The compiled-in catalogs go stale.** Treat ids and prices as defaults and
  override them (see [Model catalogs](#model-catalogs)).
- **Claude:** no schema-less JSON mode, no embeddings; forced tool choice and
  non-default sampling parameters are rejected by current models (the API's
  rules, surfaced as `400` errors).
- **Ollama:** `user`, `parallel_tool_calls`, `reasoning_effort` and
  `prompt_cache` have no equivalent and are not sent; `dimensions` for
  embeddings needs a recent server.
- **Streaming:** OpenAI-compatible servers that omit both a finish reason and
  `[DONE]` are reported as a truncated stream.
- **Templates:** no filters, helpers or whitespace control.

## See also

- [model-router](model-router.md) — capability/cost-based selection over these
  providers with automatic fallback, budgets and circuit breaking.
- [codec](codec.md) — the `Encode`/`Decode` traits and `Value` JSON DOM every
  provider builds on.
- [clients](clients.md) — auth providers, retry, circuit breakers.
- [http](http.md) — the `Client` that carries every request and streams responses.
- [obs](obs.md), [metrics](metrics.md), [log](log.md) — where the observer
  adapters send their data.
- [vectorstore](vectorstore.md) — where [embeddings](#embeddings) go for RAG.

---
**Related:** [codec](codec.md), [clients](clients.md), [http](http.md), [vectorstore](vectorstore.md), [model-router](model-router.md).

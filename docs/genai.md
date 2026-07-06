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
- A typed [request/options](#options-and-optionsbuilder) surface plus fluent builders.
- [Function calling](#function-calling-tooldefinition--toolchoice) and
  [structured output](#structured-output-responseformat--decode) that decode a JSON
  reply straight into your own type.
- [Streaming](#streaming) over a tokio channel, normalized from OpenAI/Claude SSE
  and Ollama NDJSON.
- [Embeddings](#embeddings) that pair with [vectorstore](vectorstore.md) for RAG.
- [Prompt templates](#prompt-templates) with a tiny in-house substitution engine.
- Three built-in providers: [OpenAI](#openaiprovider), [Claude](#claudeprovider),
  and [Ollama](#ollamaprovider).

## Overview

Everything is built on one object-safe trait. A `CompletionRequest` (model +
messages + tools + options) goes in; a `CompletionResponse` (or a `ChunkStream`
of `CompletionChunk`s) comes out. Each provider translates that neutral shape to
and from its wire protocol using [`ferroly::codec::Value`](codec.md) as the JSON
DOM, sends it over [`ferroly::http::Client`](http.md), and applies credentials
through the auth traits in [`clients`](clients.md).

The design deliberately avoids a few patterns that add indirection without value:

- **No registry.** You construct the provider directly (`OpenAiProvider::new(...)`)
  or store an `Arc<dyn GenAiProvider>`. There is no global name→provider lookup.
- **No `version()` / `models()` methods** on the trait — they would be unused
  indirection. The trait is `name` / `description` / `complete` / `complete_stream`
  / `supports`.
- **Typed `Options`.** Rather than a stringly-typed `HashMap<String, String>`,
  options are a struct of typed fields, with a small `custom: HashMap<String, Value>`
  escape hatch for the rare provider-specific knob.

## Enabling

The umbrella `genai` feature pulls in the trait surface, message model, options,
requests, responses, embeddings, and prompt templates. It implies `codec`,
`clients`, `http`, and `tokio`. The concrete providers are each gated behind their
own feature so you only compile the vendor you use:

```toml
[dependencies]
# Just the OpenAI provider (turns on `genai` transitively):
ferroly = { version = "*", features = ["openai"] }

# Several providers at once:
ferroly = { version = "*", features = ["openai", "claude", "ollama"] }
# or the convenience group:
ferroly = { version = "*", features = ["all-providers"] }
```

Feature map (from `Cargo.toml`):

| Feature          | Turns on                                                    |
| ---------------- | ----------------------------------------------------------- |
| `genai`          | `codec`, `clients`, `http`, `tokio` — the vendor-neutral API |
| `openai`         | `genai` + [`OpenAiProvider`](#openaiprovider) (+ embeddings) |
| `claude`         | `genai` + [`ClaudeProvider`](#claudeprovider)               |
| `ollama`         | `genai` + [`OllamaProvider`](#ollamaprovider) (+ embeddings) |
| `all-providers`  | `openai` + `claude` + `ollama`                              |

Enabling `genai` without any provider feature still gives you the whole
trait/message/request/response/template surface — useful for writing your own
`GenAiProvider` implementation.

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
| [`ChunkStream`](#boxfuture-chunkstream-capability) | type alias | `mpsc::Receiver` of streaming chunks. |
| [`Capability`](#boxfuture-chunkstream-capability) | enum | `Streaming` / `ToolUse` / `Vision` / `JsonMode`. |
| [`Message`](#messages-role-messagepart) | struct | Role-tagged, multi-part message. |
| [`Role`](#messages-role-messagepart) | enum | `System` / `User` / `Assistant` / `Tool`. |
| [`MessagePart`](#messages-role-messagepart) | enum | `Text` / `Image` / `FileRef` / `ToolCall` / `ToolResult`. |
| [`Options`](#options-and-optionsbuilder) | struct | Typed generation options + `custom` escape hatch. |
| [`OptionsBuilder`](#options-and-optionsbuilder) | struct | Fluent builder for `Options`. |
| [`CompletionRequest`](#completionrequest-and-its-builder) | struct | Model + messages + tools + options. |
| [`CompletionRequestBuilder`](#completionrequest-and-its-builder) | struct | Fluent builder for the request. |
| [`ToolDefinition`](#function-calling-tooldefinition--toolchoice) | struct | A callable tool (name + description + JSON-Schema params). |
| [`ToolChoice`](#function-calling-tooldefinition--toolchoice) | enum | `Auto` / `None` / `Required` / `Named`. |
| [`ResponseFormat`](#structured-output-responseformat--decode) | enum | `Text` / `Json` / `JsonSchema`. |
| [`CompletionResponse`](#completionresponse-completionchunk-usage) | struct | Non-streaming result; `.text()`, `.decode::<T>()`. |
| [`CompletionChunk`](#completionresponse-completionchunk-usage) | struct | One streaming delta. |
| [`Usage`](#completionresponse-completionchunk-usage) | struct | Token accounting. |
| [`Embedder`](#embeddings) | trait | Text → vector embedding. |
| [`EmbedRequest`](#embeddings) / [`EmbedResponse`](#embeddings) | struct | Embedding request / result. |
| [`PromptTemplate`](#prompt-templates) | struct | Named, reusable prompt. |
| [`PromptStore`](#prompt-templates) / [`InMemoryPromptStore`](#prompt-templates) | trait / struct | Template lookup surface. |
| [`template`](#prompt-templates) | module | `{{ dot.path }}` substitution engine. |
| [`GenAiError`](#error-handling) | enum | The crate's error type. |
| `OpenAiProvider` / `ClaudeProvider` / `ClaudeProviderConfig` / `OllamaProvider` / `ProviderOptions` | struct | [Concrete providers](#providers), each behind its feature. |

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
}
```

- `name()` — the provider id (`"openai"`, `"claude"`, `"ollama"`).
- `description()` — a short human string; defaulted to `""`.
- `complete()` — one non-streaming turn.
- `complete_stream()` — a streaming turn; see [Streaming](#streaming).
- `supports(cap)` — feature probe used to guard optional behavior.

The trait is deliberately object-safe so `Arc<dyn GenAiProvider>` works. Because
Rust has no stable `async fn` in object-safe traits, the async methods return a
[`BoxFuture`](#boxfuture-chunkstream-capability) — the manual desugaring of an
`async fn`, which is why implementations wrap `Box::pin(async move { ... })`
instead of pulling in the `async-trait` crate (in keeping with the crate's
[dependency-minimal policy](../README.md)).

There is no provider registry and no `version()`/`models()`: you hold the concrete
provider (or an `Arc<dyn GenAiProvider>`) directly.

```rust
use std::sync::Arc;
use ferroly::genai::{
    CompletionRequest, GenAiProvider, Message, OpenAiProvider, ClaudeProvider,
};

// Pick a backend at runtime behind the trait object — no registry needed.
fn choose(vendor: &str) -> Arc<dyn GenAiProvider> {
    match vendor {
        "claude" => Arc::new(ClaudeProvider::new("sk-ant-...", None)),
        _ => Arc::new(OpenAiProvider::new("sk-...", None)),
    }
}

#[tokio::main]
async fn main() -> Result<(), ferroly::genai::GenAiError> {
    let provider = choose("openai");
    println!("using provider: {}", provider.name());

    let request = CompletionRequest::builder("gpt-4o")
        .message(Message::user("One word: the capital of France."))
        .build();
    let response = provider.complete(request).await?;
    println!("{}", response.text());
    Ok(())
}
```

## `BoxFuture`, `ChunkStream`, `Capability`

```rust
// A boxed, Send future — the object-safe async-fn desugaring.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// A stream of streaming chunks, delivered over a tokio channel.
pub type ChunkStream = tokio::sync::mpsc::Receiver<Result<CompletionChunk, GenAiError>>;

pub enum Capability { Streaming, ToolUse, Vision, JsonMode }
```

`supports()` reports these per provider:

| Provider | `Streaming` | `ToolUse` | `Vision` | `JsonMode` |
| -------- | :---------: | :-------: | :------: | :--------: |
| OpenAI   | ✅ | ✅ | ✅ | ✅ |
| Claude   | ✅ | ✅ | ✅ | ❌ |
| Ollama   | ✅ | ❌ | ❌ | ✅ |

```rust
use ferroly::genai::{Capability, GenAiProvider, OpenAiProvider};

let provider = OpenAiProvider::new("sk-...", None);
if provider.supports(Capability::Vision) {
    // safe to attach an image part
}
```

## Messages: `Role`, `MessagePart`

```rust
pub struct Message {
    pub role: Role,
    pub id: Option<String>,
    pub parts: Vec<MessagePart>,
}

pub enum Role { System, User, Assistant, Tool }

pub enum MessagePart {
    Text(String),
    Image { data: Vec<u8>, mime_type: String },
    FileRef { uri: String, mime_type: String },
    ToolCall { id: String, name: String, arguments: Value },
    ToolResult { call_id: String, result: Value },
}
```

`Role::as_str()` returns the lowercase wire name (`"user"`, `"system"`,
`"assistant"`, `"tool"`). A message carries an ordered list of parts, so one
message can mix, say, text and an image.

Constructors and helpers:

| Method | Builds |
| ------ | ------ |
| `Message::text(role, id, text)` | a single `Text` part with an id |
| `Message::binary(role, id, bytes, mime_type)` | a single `Image` part |
| `Message::file_ref(role, id, uri, mime_type)` | a single `FileRef` part |
| `Message::json(role, id, &val)` | text part = the JSON [`Encode`](codec.md) of `val` (infallible) |
| `Message::user(text)` | a `User` text message, no id |
| `Message::system(text)` | a `System` text message, no id |
| `msg.add_text_part(text)` | appends a `Text` part (returns `&mut Self`) |
| `msg.add_binary_part(bytes, mime_type)` | appends an `Image` part |
| `msg.text_content()` | concatenates all `Text` parts, ignoring the rest |

`text_content()` is what you use to read an assistant reply, and it also backs
`CompletionResponse::text()`.

```rust
use ferroly::genai::{Message, Role};

// Multi-part: a prompt plus an inline PNG for a vision model.
let mut msg = Message::text(Role::User, "m1", "What's in this image?");
msg.add_binary_part(std::fs::read("photo.png").unwrap(), "image/png");
assert_eq!(msg.parts.len(), 2);
assert_eq!(msg.text_content(), "What's in this image?");

// A file reference (not inlined):
let doc = Message::file_ref(Role::User, "m2", "gs://bucket/report.pdf", "application/pdf");

// JSON-encode a value straight into a message body:
#[derive(ferroly::codec::Encode)]
struct Ctx { user_id: u64 }
let ctx = Message::json(Role::System, "m3", &Ctx { user_id: 42 });
assert_eq!(ctx.text_content(), "{\"user_id\":42}");
```

`ToolCall` and `ToolResult` parts appear in the [function-calling
loop](#function-calling-tooldefinition--toolchoice): the model emits `ToolCall`
parts in its reply, and you feed results back as `ToolResult` parts on a follow-up
turn.

## `Options` and `OptionsBuilder`

`Options` is a plain typed struct rather than a stringly-typed map. The four
common knobs are typed fields; anything provider-specific goes in `custom`.

```rust
pub struct Options {
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub system_instructions: Option<String>,
    pub custom: HashMap<String, Value>, // extensibility escape hatch, empty by default
}
```

Build it three ways — a struct literal, direct field assignment, or the fluent
`OptionsBuilder`:

```rust
use ferroly::genai::Options;

// Fluent builder:
let opts = Options::builder()
    .max_tokens(256)
    .temperature(0.7)
    .top_p(0.9)
    .system_instructions("Answer in one sentence.")
    .custom("seed", 42i64) // provider-specific knob -> custom map
    .build();

// Struct literal is equally valid:
let opts2 = Options { max_tokens: Some(64), ..Options::new() };
```

`system_instructions` is honored by every provider (OpenAI/Ollama prepend a
`system` message, Claude sets the top-level `system` field). Note the built-in
providers translate the typed fields; the `custom` map is available to your own
provider implementations and is not automatically forwarded by the built-ins.

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

Construct with `CompletionRequest::new(model, messages)` for the bare case, or the
builder for anything richer:

```rust
use ferroly::genai::{CompletionRequest, Message, Options, ResponseFormat};

let request = CompletionRequest::builder("gpt-4o")
    .message(Message::system("You are a helpful assistant."))
    .message(Message::user("Summarize Rust ownership in one line."))
    .response_format(ResponseFormat::Text)
    .options(Options::builder().max_tokens(80).temperature(0.3).build())
    .build();
```

Builder methods: `.message(m)`, `.messages(vec)`, `.tool(t)`, `.tool_choice(c)`,
`.response_format(f)`, `.options(o)`, `.build()`.

## Function calling: `ToolDefinition` + `ToolChoice`

Declare tools with a JSON-Schema parameter spec, then let the model call them.

```rust
pub struct ToolDefinition { pub name: String, pub description: String, pub parameters: Value }

pub enum ToolChoice {
    Auto,          // model decides (default when tools are present)
    None,          // model must not call a tool
    Required,      // model must call some tool
    Named(String), // model must call this specific tool
}
```

Each provider maps `ToolChoice` to its own wire form — OpenAI uses
`"auto"`/`"none"`/`"required"`/`{type:function,...}`; Claude uses
`{type:auto}`/`{type:none}`/`{type:any}`/`{type:tool,name}`. Ollama's
`/api/chat` provider does not send tools (it reports `supports(ToolUse) == false`).

The loop: send tools, read `MessagePart::ToolCall` parts from the reply, run the
tool, then send the result back as a `MessagePart::ToolResult`.

```rust
use ferroly::codec::Value;
use ferroly::genai::{
    CompletionRequest, GenAiProvider, Message, MessagePart, OpenAiProvider, Role,
    ToolChoice, ToolDefinition,
};

#[tokio::main]
async fn main() -> Result<(), ferroly::genai::GenAiError> {
    let provider = OpenAiProvider::new("sk-...", None);

    // JSON Schema for the tool's parameters, built as a codec Value.
    let params = Value::Object(vec![
        ("type".into(), "object".into()),
        ("properties".into(), Value::Object(vec![
            ("city".into(), Value::Object(vec![("type".into(), "string".into())])),
        ])),
        ("required".into(), Value::Array(vec!["city".into()])),
    ]);
    let weather = ToolDefinition::new("get_weather", "Look up current weather", params);

    let request = CompletionRequest::builder("gpt-4o")
        .message(Message::user("What's the weather in Paris?"))
        .tool(weather)
        .tool_choice(ToolChoice::Auto)
        .build();

    let response = provider.complete(request).await?;

    // Inspect tool calls the model requested.
    for part in &response.message.parts {
        if let MessagePart::ToolCall { id, name, arguments } = part {
            println!("call {id}: {name}({arguments:?})");
            // ... run the tool, then reply on the next turn:
            let _result = Message {
                role: Role::Tool,
                id: None,
                parts: vec![MessagePart::ToolResult {
                    call_id: id.clone(),
                    result: Value::Object(vec![("temp_c".into(), 18i64.into())]),
                }],
            };
        }
    }
    Ok(())
}
```

## Structured output: `ResponseFormat` + `decode`

```rust
pub enum ResponseFormat {
    Text,               // free-form (default)
    Json,               // provider "JSON mode"
    JsonSchema(Value),  // JSON conforming to the given schema
}
```

Ask for JSON, then decode the reply straight into your own type with
`CompletionResponse::decode::<T>()`, which JSON-[`Decode`](codec.md)s the response
text. (OpenAI honors both `Json` and `JsonSchema`; Ollama honors `Json` via its
`format:"json"` flag; Claude has no JSON mode — `supports(JsonMode) == false` — so
prompt for JSON explicitly there.)

```rust
use ferroly::genai::{CompletionRequest, GenAiProvider, Message, OpenAiProvider, ResponseFormat};

#[derive(ferroly::codec::Decode)]
struct Sentiment { label: String, score: f32 }

#[tokio::main]
async fn main() -> Result<(), ferroly::genai::GenAiError> {
    let provider = OpenAiProvider::new("sk-...", None);

    let request = CompletionRequest::builder("gpt-4o")
        .message(Message::system(
            "Classify sentiment. Reply as JSON: {\"label\":..., \"score\":...}",
        ))
        .message(Message::user("I absolutely love this crate!"))
        .response_format(ResponseFormat::Json)
        .build();

    let response = provider.complete(request).await?;
    let parsed: Sentiment = response.decode()?; // GenAiError::ResponseParse on bad JSON
    println!("{} ({:.2})", parsed.label, parsed.score);
    Ok(())
}
```

## `CompletionResponse`, `CompletionChunk`, `Usage`

```rust
pub struct CompletionResponse {
    pub model: String,
    pub message: Message,          // assistant reply (text and/or tool-call parts)
    pub finish_reason: Option<String>, // "stop", "length", "tool_calls", ...
    pub usage: Option<Usage>,
}

pub struct CompletionChunk {
    pub delta: String,             // incremental text (may be empty)
    pub finish_reason: Option<String>, // present on the terminal chunk
    pub usage: Option<Usage>,          // present on the terminal chunk for some providers
}

pub struct Usage {
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
}
```

- `CompletionResponse::text()` → concatenated text of the reply message.
- `CompletionResponse::decode::<T>()` → typed [structured output](#structured-output-responseformat--decode).

```rust
let response = provider.complete(request).await?;
println!("model: {}", response.model);
println!("text:  {}", response.text());
if let Some(u) = &response.usage {
    println!("tokens: {:?} in / {:?} out", u.prompt_tokens, u.completion_tokens);
}
```

## Streaming

`complete_stream()` returns a `ChunkStream` — a
`tokio::sync::mpsc::Receiver<Result<CompletionChunk, GenAiError>>`. A background
task pumps the HTTP body, buffers lines across network chunk boundaries, parses
each into a `CompletionChunk`, and forwards it. Consume with `recv().await`:

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
        let chunk = item?;                 // each item is a Result
        print!("{}", chunk.delta);         // append the incremental text
        if let Some(reason) = &chunk.finish_reason {
            println!("\n[done: {reason}]");
        }
    }
    Ok(())
}
```

OpenAI and Claude stream Server-Sent Events (`data:` lines, `[DONE]` sentinel for
OpenAI; `content_block_delta` / `message_delta` events for Claude). Ollama streams
newline-delimited JSON (NDJSON) objects. All three normalize to the same
`CompletionChunk` shape, so consumer code is provider-agnostic. If the transport
fails mid-stream, the error arrives as a terminal `Err(GenAiError::Transport(..))`
item on the channel.

## Embeddings

Turn text into vectors for semantic search / RAG. `Embedder` is a separate trait
implemented by the OpenAI and Ollama providers (Claude exposes no embeddings API).

```rust
pub trait Embedder: Send + Sync {
    fn embed(&self, request: EmbedRequest) -> BoxFuture<'_, Result<EmbedResponse, GenAiError>>;
}

pub struct EmbedRequest { pub model: String, pub input: Vec<String> }
pub struct EmbedResponse { pub model: String, pub embeddings: Vec<Vec<f32>>, pub usage: Option<Usage> }
```

Build a request for one input (`EmbedRequest::single`) or a batch
(`EmbedRequest::new`). OpenAI calls `/v1/embeddings`; Ollama calls `/api/embed`.
The result is one `Vec<f32>` per input, in request order — feed those straight
into a [vectorstore](vectorstore.md).

```rust
use ferroly::genai::{EmbedRequest, Embedder, OpenAiProvider};

#[tokio::main]
async fn main() -> Result<(), ferroly::genai::GenAiError> {
    let provider = OpenAiProvider::new("sk-...", None);

    let request = EmbedRequest::new(
        "text-embedding-3-small",
        vec!["iron is a metal".into(), "rust is oxidation".into()],
    );
    let response = provider.embed(request).await?;
    println!("{} vectors, dim {}", response.embeddings.len(), response.embeddings[0].len());
    // -> store response.embeddings in a ferroly::vectorstore and search later
    Ok(())
}
```

For a local model with Ollama, swap in `OllamaProvider::new(None)` and a model id
like `nomic-embed-text`.

## Providers

All three providers build their request body as a [`ferroly::codec::Value`](codec.md),
send it via [`ferroly::http::Client`](http.md), and (for OpenAI/Claude) apply
credentials through the [clients](clients.md) auth traits. `ProviderOptions`
overrides the base URL — for self-hosting, gateways, proxies, or test servers.

```rust
pub struct ProviderOptions { pub base_url: Option<String> }
// ProviderOptions::with_base_url("https://gateway.internal/openai")
```

### `OpenAiProvider`

Backs the OpenAI Chat Completions API (`POST {base}/v1/chat/completions`),
authenticated with a bearer key. Default base `https://api.openai.com`. Supports
`Streaming`, `ToolUse`, `Vision`, and `JsonMode`, and also implements
[`Embedder`](#embeddings).

```rust
use ferroly::genai::{OpenAiProvider, ProviderOptions};

// Default endpoint:
let provider = OpenAiProvider::new("sk-...", None);

// Point at an OpenAI-compatible gateway:
let via_proxy = OpenAiProvider::new(
    "sk-...",
    Some(ProviderOptions::with_base_url("https://gateway.internal/openai")),
);
```

### `ClaudeProvider`

Backs the Anthropic Messages API (`POST {base}/v1/messages`), authenticated with an
`x-api-key` header and the `anthropic-version: 2023-06-01` header. Default base
`https://api.anthropic.com`. Supports `Streaming`, `ToolUse`, `Vision` (no JSON
mode). If you set no `max_tokens`, it defaults to `1024` (the Messages API requires
the field). System instructions come from `options.system_instructions`, falling
back to any `Role::System` messages joined together.

`ClaudeProvider::new(api_key, opts)` is the common path. For a custom credential
source (e.g. a Vault-backed `AuthProvider`), use `with_config`:

```rust
use std::sync::Arc;
use ferroly::genai::{ClaudeProvider, ClaudeProviderConfig, BearerAuth, ProviderOptions};

// Simple API-key auth:
let provider = ClaudeProvider::new("sk-ant-...", None);

// Pluggable auth via ClaudeProviderConfig { auth: Arc<dyn AuthProvider> }:
let custom = ClaudeProvider::with_config(
    ClaudeProviderConfig { auth: Arc::new(BearerAuth::new("token-from-vault".to_string())) },
    Some(ProviderOptions::with_base_url("https://claude-proxy.internal")),
);
```

### `OllamaProvider`

Backs a local (or remote) Ollama server's `POST {base}/api/chat`. Default base
`http://localhost:11434`; no auth. Supports `Streaming` and `JsonMode`. Generation
options are nested under `options` on the wire (`temperature`, `top_p`,
`num_predict` for max tokens), and streaming is NDJSON rather than SSE. Also
implements [`Embedder`](#embeddings) via `/api/embed`.

```rust
use ferroly::genai::{CompletionRequest, GenAiProvider, Message, OllamaProvider, ProviderOptions};

#[tokio::main]
async fn main() -> Result<(), ferroly::genai::GenAiError> {
    // Local default, or point at a remote host:
    let provider = OllamaProvider::new(
        Some(ProviderOptions::with_base_url("http://gpu-box:11434")),
    );

    let request = CompletionRequest::builder("llama3")
        .message(Message::user("Explain iron oxidation briefly."))
        .build();
    println!("{}", provider.complete(request).await?.text());
    Ok(())
}
```

## Prompt templates

Reusable, named prompts rendered by a tiny in-house engine (replacing minijinja).
The `ferroly::genai::template` module does `{{ name }}` / `{{ a.b }}` dot-path
substitution against any [`Encode`](codec.md) value, tolerating the Go
`text/template` `{{ .name }}` leading-dot form. Undefined variables render as the
empty string. It is substitution-only — no conditionals or loops.

```rust
pub struct PromptTemplate { pub id: String, pub name: String, pub template: String }
pub trait PromptStore: Send + Sync { fn get(&self, id: &str) -> Option<PromptTemplate>; }
pub struct InMemoryPromptStore { /* ... */ }
```

- `PromptTemplate::new(id, name, source)` and `.render(&vars)` → `Result<String, GenAiError>`.
- `InMemoryPromptStore::new()` / `.add(template)` and `PromptStore::get(id)`.
- `Message::from_prompt_id(role, store, id, &vars)` renders a stored template
  directly into a `Message` (errors `TemplateNotFound` if the id is absent).
- `ferroly::genai::template::render(src, &vars)` / `render_value(src, &value)` for
  ad-hoc rendering.

```rust
use ferroly::genai::{InMemoryPromptStore, Message, PromptStore, PromptTemplate, Role};

#[derive(ferroly::codec::Encode)]
struct Vars { name: String, topic: String }

// Render a template directly:
let t = PromptTemplate::new("greet", "Greeting", "Hello {{ name }}, about {{ topic }}?");
let text = t.render(&Vars { name: "Ada".into(), topic: "Rust".into() }).unwrap();
assert_eq!(text, "Hello Ada, about Rust?");

// Or store templates and build messages by id:
let mut store = InMemoryPromptStore::new();
store.add(PromptTemplate::new("greet", "Greeting", "Hi {{ name }}"));
let msg = Message::from_prompt_id(
    Role::User, &store, "greet",
    &Vars { name: "Bob".into(), topic: "x".into() },
).unwrap();
assert_eq!(msg.text_content(), "Hi Bob");
```

## Error handling

Every fallible call returns `Result<_, GenAiError>`:

```rust
pub enum GenAiError {
    Template(String),                                   // template failed to compile/render
    TemplateNotFound(String),                           // prompt id absent from the store
    Unsupported { provider: String, capability: Capability },
    Transport(String),                                  // HTTP/network failure
    Api { status: u16, message: String },               // non-2xx from the provider
    ResponseParse(String),                              // couldn't parse the provider reply / decode<T>
    Config(String),                                     // bad base URL, missing key, ...
}
```

`GenAiError` derives the crate's `FerrolyError` (so it implements `Display` and
the standard error trait). Typical handling:

```rust
use ferroly::genai::GenAiError;

match provider.complete(request).await {
    Ok(response) => println!("{}", response.text()),
    Err(GenAiError::Api { status, message }) => eprintln!("provider HTTP {status}: {message}"),
    Err(GenAiError::Transport(e)) => eprintln!("network error: {e}"),
    Err(GenAiError::ResponseParse(e)) => eprintln!("bad reply shape: {e}"),
    Err(other) => eprintln!("{other}"),
}
```

`CompletionResponse::decode::<T>()` surfaces bad structured output as
`GenAiError::ResponseParse`.

## Limitations

- **Providers:** only OpenAI, Claude, and Ollama ship here. Bedrock / Vertex are
  intended for the `ferroly-aws` / `ferroly-gcp` extension crates.
- **Claude has no JSON mode** (`supports(JsonMode) == false`) and **no embeddings**
  — prompt for JSON explicitly and use OpenAI/Ollama for vectors.
- **Ollama chat** does not forward tools (`supports(ToolUse) == false`).
- **Streaming tool calls** are not reassembled: `CompletionChunk` carries text
  deltas plus a terminal finish reason. Use `complete()` when you need structured
  tool-call parts.
- **The `custom` options map** is an escape hatch for your own providers; the
  built-ins translate only the typed `Options` fields.
- **The template engine** is substitution-only — no conditionals, loops, or
  filters.
- **No provider registry / `version()` / `models()`** — construct providers
  directly, by design.

## See also

- [codec](codec.md) — the `Encode`/`Decode` traits and `Value` JSON DOM every
  provider builds on.
- [clients](clients.md) — the `AuthProvider` / `BearerAuth` / `ApiKeyAuth` traits
  used for provider credentials.
- [http](http.md) — the `Client` that carries every request and streams responses.
- [vectorstore](vectorstore.md) — where [embeddings](#embeddings) go for RAG.

---
**Related:** [codec](codec.md), [clients](clients.md), [http](http.md), [vectorstore](vectorstore.md).

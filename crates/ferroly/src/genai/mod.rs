//! Provider-agnostic GenAI / LLM interface.
//!
//! Application code depends
//! on the [`GenAiProvider`] trait, not on any vendor, so providers are
//! interchangeable: construct the one you want (or hold an
//! `Arc<dyn GenAiProvider>` when runtime indirection is wanted).
//!
//! ```no_run
//! use ferroly::genai::{CompletionRequest, GenAiProvider, Message};
//! # #[cfg(feature = "openai")]
//! use ferroly::genai::OpenAiProvider;
//!
//! # #[cfg(feature = "openai")]
//! # #[tokio::main]
//! # async fn main() -> Result<(), ferroly::genai::GenAiError> {
//! let provider = OpenAiProvider::new("sk-...", None);
//!
//! let request = CompletionRequest::builder("gpt-4o")
//!     .message(Message::user("Say hello in French."))
//!     .build();
//! let response = provider.complete(request).await?;
//! println!("{}", response.text());
//! # Ok(())
//! # }
//! # #[cfg(not(feature = "openai"))]
//! # fn main() {}
//! ```
//!
//! Providers are gated behind cargo features (`openai`, `claude`, `ollama`).
//!
//! # What is here
//!
//! | Area | Start at |
//! |---|---|
//! | Requests and responses | [`CompletionRequest`], [`Options`], [`CompletionResponse`], [`FinishReason`], [`Usage`] |
//! | Tool calling | [`ToolDefinition`], [`ToolChoice`], [`Message::tool_result`], [`CompletionResponse::tool_calls`] |
//! | Streaming | [`CompletionChunk`], [`StreamAccumulator`], [`collect_stream`] |
//! | Errors | [`GenAiError`] (rate limits, auth, timeouts, overload, context length) |
//! | Embeddings | [`Embedder`], [`EmbedRequest`] |
//! | Routing, budgets, fallback | [`router`] |
//! | Prompt templates | [`template`], [`PromptTemplate`], [`InMemoryPromptStore`] |
//! | Telemetry | [`GenAiObserver`] |
//! | Testing your code | [`MockProvider`] |
//!
//! The model ids and prices compiled into the built-in providers' catalogs are
//! **defaults to override** — see [`ModelInfo`] and the router configuration.

#![deny(missing_docs)]

mod embedder;
mod error;
mod message;
mod mock;
mod observe;
mod options;
mod prompt;
mod provider;
#[cfg(feature = "vectorstore")]
pub mod rag;
mod request;
mod response;
pub mod router;
pub mod template;

pub mod providers;

pub use embedder::{EmbedRequest, EmbedResponse, Embedder};
pub use error::{GenAiError, RateLimitInfo, TimeoutPhase};
pub use message::{Message, MessagePart, Role};
pub use mock::{MockProvider, MockReply};
#[cfg(feature = "metrics")]
pub use observe::MetricsObserver;
#[cfg(feature = "obs")]
pub use observe::ObsObserver;
pub use observe::{
    redact_payload, GenAiObserver, MultiObserver, Operation, PayloadDirection, RequestInfo,
    RequestOutcome,
};
#[cfg(feature = "log")]
pub use observe::{LogObserver, PayloadLogging};
pub use options::{CacheTtl, Options, OptionsBuilder, PromptCache, ReasoningEffort};
pub use prompt::{InMemoryPromptStore, PromptStore, PromptTemplate};
pub use provider::{BoxFuture, ChunkStream, GenAiProvider};
pub use request::{
    CompletionRequest, CompletionRequestBuilder, JsonSchemaFormat, ResponseFormat, ToolChoice,
    ToolDefinition,
};
pub use response::{
    collect_stream, response_to_chunks, Capability, Citation, CompletionChunk, CompletionResponse,
    FinishReason, Grounding, StreamAccumulator, ToolCallDelta, Usage,
};
pub use router::{ModelInfo, ModelRouter, RouterError};

// Re-export the auth traits GenAI providers build on, for convenience.
pub use ferroly::clients::{ApiKeyAuth, AuthProvider, BasicAuth, BearerAuth};

#[cfg(feature = "ollama")]
pub use providers::OllamaProvider;
#[cfg(feature = "openai")]
pub use providers::{AuthStyle, OpenAiConfig, OpenAiProvider};
#[cfg(feature = "claude")]
pub use providers::{ClaudeProvider, ClaudeProviderConfig};
#[cfg(any(feature = "openai", feature = "claude", feature = "ollama"))]
pub use providers::{ProviderOptions, Timeouts};

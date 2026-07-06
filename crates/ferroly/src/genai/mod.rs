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

#![deny(missing_docs)]

mod embedder;
mod error;
mod message;
mod options;
mod prompt;
mod provider;
mod request;
mod response;
pub mod template;

pub mod providers;

pub use embedder::{EmbedRequest, EmbedResponse, Embedder};
pub use error::GenAiError;
pub use message::{Message, MessagePart, Role};
pub use options::{Options, OptionsBuilder};
pub use prompt::{InMemoryPromptStore, PromptStore, PromptTemplate};
pub use provider::{BoxFuture, ChunkStream, GenAiProvider};
pub use request::{
    CompletionRequest, CompletionRequestBuilder, ResponseFormat, ToolChoice, ToolDefinition,
};
pub use response::{Capability, CompletionChunk, CompletionResponse, Usage};

// Re-export the auth traits GenAI providers build on, for convenience.
pub use ferroly::clients::{ApiKeyAuth, AuthProvider, BasicAuth, BearerAuth};

#[cfg(feature = "ollama")]
pub use providers::OllamaProvider;
#[cfg(feature = "openai")]
pub use providers::OpenAiProvider;
#[cfg(any(feature = "openai", feature = "claude", feature = "ollama"))]
pub use providers::ProviderOptions;
#[cfg(feature = "claude")]
pub use providers::{ClaudeProvider, ClaudeProviderConfig};

//! The [`GenAiProvider`] trait.

use std::future::Future;
use std::pin::Pin;

use ferroly::genai::{
    Capability, CompletionChunk, CompletionRequest, CompletionResponse, GenAiError,
};

/// A boxed, `Send` future — the manual `async fn`-in-trait desugaring that keeps
/// [`GenAiProvider`] object-safe without the `async-trait` dependency.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A stream of streaming completion chunks, delivered over a tokio channel.
///
/// Consume with `while let Some(chunk) = stream.recv().await { ... }`.
pub type ChunkStream = tokio::sync::mpsc::Receiver<Result<CompletionChunk, GenAiError>>;

/// A provider-agnostic LLM backend.
///
/// Application code depends on this trait, not on any vendor. Providers are
/// held directly (or as `Arc<dyn GenAiProvider>` when runtime indirection is
/// wanted). The async methods return a [`BoxFuture`]; the idiomatic
/// implementation wraps `async move { ... }` in `Box::pin`.
pub trait GenAiProvider: Send + Sync {
    /// Provider identifier, e.g. `"openai"`, `"claude"`, `"ollama"`.
    fn name(&self) -> &str;

    /// A short human-readable description.
    fn description(&self) -> &str {
        ""
    }

    /// Runs a non-streaming completion.
    fn complete(
        &self,
        request: CompletionRequest,
    ) -> BoxFuture<'_, Result<CompletionResponse, GenAiError>>;

    /// Runs a streaming completion, yielding [`CompletionChunk`]s over a channel.
    fn complete_stream(
        &self,
        request: CompletionRequest,
    ) -> BoxFuture<'_, Result<ChunkStream, GenAiError>>;

    /// Reports whether the provider supports a capability.
    fn supports(&self, capability: Capability) -> bool;
}

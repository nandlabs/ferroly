//! The [`GenAiProvider`] trait.

use std::future::Future;
use std::pin::Pin;

use ferroly::genai::{
    Capability, CompletionChunk, CompletionRequest, CompletionResponse, GenAiError, ModelInfo,
};

/// A boxed, `Send` future — the manual `async fn`-in-trait desugaring that keeps
/// [`GenAiProvider`] object-safe without the `async-trait` dependency.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A stream of streaming completion events, delivered over a tokio channel.
///
/// Consume with `while let Some(chunk) = stream.recv().await { ... }`, or fold
/// it into a [`CompletionResponse`] with
/// [`collect_stream`](crate::genai::collect_stream). Each item is a
/// [`CompletionChunk`] event or the error that ended the stream. Dropping the
/// receiver cancels the stream: the provider's reader task stops and closes
/// its connection.
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

    /// Structured metadata for the models this provider serves, consumed by the
    /// [model router](crate::genai::router).
    ///
    /// The default is empty — "capabilities unknown": such a provider is
    /// invisible to capability-based routing but still reachable by an explicit
    /// rule or pin. Providers override this to advertise their catalog.
    fn model_catalog(&self) -> Vec<ModelInfo> {
        Vec::new()
    }

    /// The catalog entry for `model`, if this provider advertises one.
    ///
    /// The default scans [`model_catalog`](GenAiProvider::model_catalog);
    /// providers with an indexed catalog may override it.
    fn model_info_for(&self, model: &str) -> Option<ModelInfo> {
        self.model_catalog().into_iter().find(|m| m.name == model)
    }
}

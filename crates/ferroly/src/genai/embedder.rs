//! Text embeddings — turning text into vectors for search / RAG.
//!
//! Pairs with [`ferroly::vectorstore`](crate::vectorstore): embed text with a
//! provider, store the vectors, then search.

use ferroly::genai::provider::BoxFuture;
use ferroly::genai::{GenAiError, Usage};

/// A request to embed one or more inputs with a given model.
#[derive(Debug, Clone)]
pub struct EmbedRequest {
    /// The embedding model id (e.g. `text-embedding-3-small`, `nomic-embed-text`).
    pub model: String,
    /// The inputs to embed, in order.
    pub input: Vec<String>,
}

impl EmbedRequest {
    /// Creates a request for a batch of inputs.
    pub fn new(model: impl Into<String>, input: Vec<String>) -> Self {
        Self {
            model: model.into(),
            input,
        }
    }

    /// Creates a request for a single input.
    pub fn single(model: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            input: vec![text.into()],
        }
    }
}

/// The embeddings for each input, in the same order as the request.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbedResponse {
    /// The model that produced the embeddings.
    pub model: String,
    /// One vector per input.
    pub embeddings: Vec<Vec<f32>>,
    /// Token usage, if the provider reports it.
    pub usage: Option<Usage>,
}

/// A provider that can embed text into vectors.
///
/// Implemented by the OpenAI and Ollama providers (Claude exposes no embeddings
/// API). The async method returns a [`BoxFuture`].
pub trait Embedder: Send + Sync {
    /// Embeds the request's inputs.
    fn embed(&self, request: EmbedRequest) -> BoxFuture<'_, Result<EmbedResponse, GenAiError>>;
}

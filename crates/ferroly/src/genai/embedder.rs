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
    /// The number of dimensions the vectors should have, for models that can
    /// shorten their output (OpenAI `text-embedding-3-*`, recent Ollama).
    pub dimensions: Option<u32>,
    /// An opaque end-user identifier (OpenAI `user`).
    pub user: Option<String>,
}

impl EmbedRequest {
    /// Creates a request for a batch of inputs.
    pub fn new(model: impl Into<String>, input: Vec<String>) -> Self {
        Self {
            model: model.into(),
            input,
            dimensions: None,
            user: None,
        }
    }

    /// Creates a request for a single input.
    pub fn single(model: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(model, vec![text.into()])
    }

    /// Asks for vectors of `dimensions` components.
    pub fn dimensions(mut self, dimensions: u32) -> Self {
        self.dimensions = Some(dimensions);
        self
    }

    /// Sets the opaque end-user identifier.
    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
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
/// API). The async methods return a [`BoxFuture`].
pub trait Embedder: Send + Sync {
    /// Embeds the request's inputs, one vector per input, in order.
    ///
    /// The built-in providers split a request that exceeds their batch limit
    /// into several calls themselves (see `ProviderOptions::embed_batch_size`).
    fn embed(&self, request: EmbedRequest) -> BoxFuture<'_, Result<EmbedResponse, GenAiError>>;

    /// Embeds the inputs in consecutive batches of at most `max_batch`, and
    /// returns one response with the vectors **in input order** and the usage
    /// summed. `max_batch` of zero means one call.
    ///
    /// The batches are sent one after the other; the first error ends the
    /// call. A batch that returns a different number of vectors than it was
    /// given is a [`GenAiError::ResponseParse`], so a short answer can never
    /// shift every later vector onto the wrong input.
    fn embed_batched(
        &self,
        request: EmbedRequest,
        max_batch: usize,
    ) -> BoxFuture<'_, Result<EmbedResponse, GenAiError>> {
        Box::pin(embed_in_batches(request, max_batch, move |part| {
            self.embed(part)
        }))
    }
}

/// Runs `call` over consecutive batches of `request.input` and stitches the
/// answers together in input order. Shared by [`Embedder::embed_batched`] and
/// the built-in providers' automatic batching.
pub(crate) async fn embed_in_batches<'a, F>(
    request: EmbedRequest,
    max_batch: usize,
    call: F,
) -> Result<EmbedResponse, GenAiError>
where
    F: Fn(EmbedRequest) -> BoxFuture<'a, Result<EmbedResponse, GenAiError>>,
{
    let total = request.input.len();
    let batch = if max_batch == 0 {
        total.max(1)
    } else {
        max_batch
    };
    if total <= batch {
        let response = call(request).await?;
        check_count(response.embeddings.len(), total)?;
        return Ok(response);
    }
    let mut model = request.model.clone();
    let mut embeddings = Vec::with_capacity(total);
    let mut usage: Option<Usage> = None;
    for chunk in request.input.chunks(batch) {
        let part = EmbedRequest {
            model: request.model.clone(),
            input: chunk.to_vec(),
            dimensions: request.dimensions,
            user: request.user.clone(),
        };
        let response = call(part).await?;
        check_count(response.embeddings.len(), chunk.len())?;
        model = response.model;
        embeddings.extend(response.embeddings);
        if let Some(u) = response.usage {
            usage = Some(match usage {
                Some(total) => add_usage(total, &u),
                None => u,
            });
        }
    }
    Ok(EmbedResponse {
        model,
        embeddings,
        usage,
    })
}

fn check_count(got: usize, expected: usize) -> Result<(), GenAiError> {
    if got == expected {
        Ok(())
    } else {
        Err(GenAiError::ResponseParse(format!(
            "provider returned {got} embeddings for {expected} inputs"
        )))
    }
}

fn add_usage(mut total: Usage, part: &Usage) -> Usage {
    fn add(a: Option<u32>, b: Option<u32>) -> Option<u32> {
        match (a, b) {
            (Some(x), Some(y)) => Some(x.saturating_add(y)),
            (x, None) => x,
            (None, y) => y,
        }
    }
    total.prompt_tokens = add(total.prompt_tokens, part.prompt_tokens);
    total.completion_tokens = add(total.completion_tokens, part.completion_tokens);
    total.total_tokens = add(total.total_tokens, part.total_tokens);
    total.cached_tokens = add(total.cached_tokens, part.cached_tokens);
    total
}

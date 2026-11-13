//! A small retrieval helper tying an [`Embedder`] to a
//! [`VectorStore`]: embed-and-upsert, and query-by-text. Requires the
//! `vectorstore` feature.
//!
//! ```
//! use std::sync::Arc;
//! use ferroly::genai::rag::{RagDocument, RagIndex};
//! use ferroly::genai::MockProvider;
//! use ferroly::vectorstore::MemoryStore;
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> Result<(), ferroly::genai::rag::RagError> {
//! // Any `Embedder` works; the mock returns deterministic vectors.
//! let index = RagIndex::new(
//!     Arc::new(MockProvider::new("mock")),
//!     Arc::new(MemoryStore::new()),
//!     "embedding-model",
//! );
//! index
//!     .upsert(vec![
//!         RagDocument::new("a", "Rust is a systems programming language."),
//!         RagDocument::new("b", "Paris is the capital of France."),
//!     ])
//!     .await?;
//!
//! let hits = index.query("Rust is a systems programming language.", 1).await?;
//! assert_eq!(hits[0].id, "a");
//! assert_eq!(hits[0].text.as_deref(), Some("Rust is a systems programming language."));
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;

use ferroly::codec::Value;
use ferroly::genai::{EmbedRequest, Embedder, GenAiError};
use ferroly::vectorstore::{Doc, Filter, Query, VectorStore, VectorStoreError};

/// Errors from a [`RagIndex`] operation.
#[derive(Debug, ferroly::FerrolyError)]
#[non_exhaustive]
pub enum RagError {
    /// Embedding the text failed.
    #[error("embedding failed: {0}")]
    Embed(#[from] GenAiError),
    /// The vector store failed.
    #[error("vector store failed: {0}")]
    Store(#[from] VectorStoreError),
}

/// A piece of text to index.
#[derive(Debug, Clone, PartialEq)]
pub struct RagDocument {
    /// The id the document is stored under (re-using an id replaces it).
    pub id: String,
    /// The text that is embedded — and stored, so a hit can return it.
    pub text: String,
    /// Extra metadata stored with the document (an object; used for
    /// filtering).
    pub metadata: Value,
}

impl RagDocument {
    /// A document with no extra metadata.
    pub fn new(id: impl Into<String>, text: impl Into<String>) -> Self {
        RagDocument {
            id: id.into(),
            text: text.into(),
            metadata: Value::Null,
        }
    }

    /// Attaches metadata (an object).
    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = metadata;
        self
    }
}

/// One retrieval result.
#[derive(Debug, Clone, PartialEq)]
pub struct RagHit {
    /// The document id.
    pub id: String,
    /// The similarity score (higher is more similar).
    pub score: f32,
    /// The stored text, when the document was indexed through a `RagIndex`.
    pub text: Option<String>,
    /// The document's full metadata (including the text entry).
    pub metadata: Value,
}

/// Embeds text with one model and keeps the vectors in one store.
///
/// The same embedding model (and dimension) must be used for indexing and for
/// querying, which is why both go through this one object.
pub struct RagIndex {
    embedder: Arc<dyn Embedder>,
    store: Arc<dyn VectorStore>,
    model: String,
    dimensions: Option<u32>,
    batch_size: usize,
    text_key: String,
}

impl RagIndex {
    /// An index that embeds with `model` on `embedder` and stores in `store`.
    pub fn new(
        embedder: Arc<dyn Embedder>,
        store: Arc<dyn VectorStore>,
        model: impl Into<String>,
    ) -> Self {
        RagIndex {
            embedder,
            store,
            model: model.into(),
            dimensions: None,
            batch_size: 64,
            text_key: "text".to_string(),
        }
    }

    /// Asks the embedding model for vectors of this many dimensions.
    pub fn dimensions(mut self, dimensions: u32) -> Self {
        self.dimensions = Some(dimensions);
        self
    }

    /// How many texts are embedded per request while indexing (default 64).
    pub fn batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = batch_size.max(1);
        self
    }

    /// The metadata key the document text is stored under (default `text`).
    pub fn text_key(mut self, key: impl Into<String>) -> Self {
        self.text_key = key.into();
        self
    }

    fn request(&self, input: Vec<String>) -> EmbedRequest {
        let mut request = EmbedRequest::new(self.model.clone(), input);
        request.dimensions = self.dimensions;
        request
    }

    /// Embeds the documents and upserts them, returning how many were stored.
    /// The store's upsert is all-or-nothing, and nothing is stored if
    /// embedding fails.
    pub async fn upsert(&self, documents: Vec<RagDocument>) -> Result<usize, RagError> {
        if documents.is_empty() {
            return Ok(0);
        }
        let texts: Vec<String> = documents.iter().map(|d| d.text.clone()).collect();
        let response = self
            .embedder
            .embed_batched(self.request(texts), self.batch_size)
            .await?;
        if response.embeddings.len() != documents.len() {
            return Err(RagError::Embed(GenAiError::ResponseParse(format!(
                "embedder returned {} vectors for {} documents",
                response.embeddings.len(),
                documents.len()
            ))));
        }
        let count = documents.len();
        let docs: Vec<Doc> = documents
            .into_iter()
            .zip(response.embeddings)
            .map(|(document, vector)| {
                let text = (self.text_key.clone(), Value::Str(document.text));
                let metadata = match document.metadata {
                    Value::Object(mut entries) => {
                        entries.retain(|(k, _)| *k != self.text_key);
                        entries.push(text);
                        Value::Object(entries)
                    }
                    Value::Null => Value::Object(vec![text]),
                    other => Value::Object(vec![("metadata".to_string(), other), text]),
                };
                Doc::new(document.id, vector).with_metadata(metadata)
            })
            .collect();
        self.store.upsert(docs).await?;
        Ok(count)
    }

    /// Embeds `text` and returns the `top_k` most similar documents.
    pub async fn query(&self, text: &str, top_k: usize) -> Result<Vec<RagHit>, RagError> {
        self.search(text, top_k, None).await
    }

    /// Like [`query`](Self::query), restricted to documents whose metadata
    /// matches `filter` — for example a tenant or access-control field.
    pub async fn query_filtered(
        &self,
        text: &str,
        top_k: usize,
        filter: Filter,
    ) -> Result<Vec<RagHit>, RagError> {
        self.search(text, top_k, Some(filter)).await
    }

    async fn search(
        &self,
        text: &str,
        top_k: usize,
        filter: Option<Filter>,
    ) -> Result<Vec<RagHit>, RagError> {
        let response = self
            .embedder
            .embed(self.request(vec![text.to_string()]))
            .await?;
        let vector = response.embeddings.into_iter().next().ok_or_else(|| {
            RagError::Embed(GenAiError::ResponseParse(
                "embedder returned no vector for the query".into(),
            ))
        })?;
        let mut query = Query::new(vector, top_k);
        if let Some(filter) = filter {
            query = query.filter(filter);
        }
        let hits = self.store.search(query).await?;
        Ok(hits
            .into_iter()
            .map(|hit| RagHit {
                text: hit
                    .doc
                    .metadata
                    .get(&self.text_key)
                    .and_then(Value::as_str)
                    .map(str::to_string),
                id: hit.id,
                score: hit.score,
                metadata: hit.doc.metadata,
            })
            .collect())
    }
}

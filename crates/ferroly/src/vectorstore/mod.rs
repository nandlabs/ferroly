//! Vector search abstraction — a [`VectorStore`] trait, an in-memory backend,
//! and similarity metrics.
//!
//! Application code depends on the trait; construct the backend you want (or
//! hold an `Arc<dyn VectorStore>` for runtime indirection). Cloud backends
//! (Pinecone, Qdrant, pgvector) live in satellite crates.
//!
//! ```no_run
//! # use ferroly::vectorstore::{Doc, MemoryStore, Query, VectorStore};
//! # async fn example() {
//! let store = MemoryStore::new();
//! store.upsert(vec![
//!     Doc::new("a", vec![1.0, 0.0]),
//!     Doc::new("b", vec![0.0, 1.0]),
//! ]).await.unwrap();
//!
//! let hits = store.search(Query::new(vec![0.9, 0.1], 1)).await.unwrap();
//! assert_eq!(hits[0].id, "a");
//! # }
//! ```

#![deny(missing_docs)]

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::RwLock;

use ferroly::codec::Value;
use ferroly_derive::FerrolyError;

/// A boxed, `Send` future — the manual `async fn`-in-trait desugaring that keeps
/// [`VectorStore`] object-safe without `async-trait`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A dense embedding vector.
pub type Vector = Vec<f32>;

/// The similarity / distance metric used to rank search results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Metric {
    /// Cosine similarity (angle); scale-invariant. The default.
    #[default]
    Cosine,
    /// Dot product; sensitive to magnitude.
    Dot,
    /// Euclidean (L2) distance, ranked as `-distance` so nearest ranks first.
    Euclidean,
}

/// A stored document: an id, its embedding, and optional metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct Doc {
    /// Caller-assigned unique id.
    pub id: String,
    /// The embedding vector.
    pub vector: Vector,
    /// Arbitrary metadata (e.g. the source text, tags).
    pub metadata: Value,
}

impl Doc {
    /// Creates a document with no metadata.
    pub fn new(id: impl Into<String>, vector: Vector) -> Self {
        Self {
            id: id.into(),
            vector,
            metadata: Value::Null,
        }
    }

    /// Attaches metadata.
    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = metadata;
        self
    }
}

/// A nearest-neighbor query.
#[derive(Debug, Clone)]
pub struct Query {
    /// The query embedding.
    pub vector: Vector,
    /// Maximum number of hits to return.
    pub top_k: usize,
    /// The ranking metric.
    pub metric: Metric,
}

impl Query {
    /// Creates a cosine-similarity query for the `top_k` nearest docs.
    pub fn new(vector: Vector, top_k: usize) -> Self {
        Self {
            vector,
            top_k,
            metric: Metric::Cosine,
        }
    }

    /// Overrides the ranking metric.
    pub fn metric(mut self, metric: Metric) -> Self {
        self.metric = metric;
        self
    }
}

/// A search hit: the matched document and its score (higher = more similar).
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// The document id.
    pub id: String,
    /// The score under the query's [`Metric`] (descending = best first).
    pub score: f32,
    /// The full matched document.
    pub doc: Doc,
}

/// Errors raised by a vector store.
#[derive(Debug, FerrolyError)]
#[non_exhaustive]
pub enum VectorStoreError {
    /// A vector's dimension did not match the collection's.
    #[error("vector dimension mismatch: expected {expected}, got {got}")]
    DimMismatch {
        /// The collection's established dimension.
        expected: usize,
        /// The offending vector's dimension.
        got: usize,
    },
    /// A backend-specific failure (network, etc.).
    #[error("vector store backend error: {0}")]
    Backend(String),
}

/// A vector store: upsert documents, search by nearest neighbor, delete by id.
///
/// Async methods return a [`BoxFuture`]; cloud backends await network I/O, the
/// in-memory backend resolves immediately.
pub trait VectorStore: Send + Sync {
    /// Inserts or replaces documents by id. All vectors must share the
    /// collection's dimension (established by the first upsert).
    fn upsert(&self, docs: Vec<Doc>) -> BoxFuture<'_, Result<(), VectorStoreError>>;

    /// Returns up to `query.top_k` hits, most similar first.
    fn search(&self, query: Query) -> BoxFuture<'_, Result<Vec<Hit>, VectorStoreError>>;

    /// Removes documents by id. Unknown ids are silently skipped (idempotent).
    fn delete(&self, ids: Vec<String>) -> BoxFuture<'_, Result<(), VectorStoreError>>;
}

// ---- similarity metrics --------------------------------------------------

/// Cosine similarity of two equal-length vectors, in `[-1, 1]` (0 if either is
/// a zero vector).
///
/// The vectors should share a dimension; on a length mismatch these functions
/// operate over the shorter length (a `debug_assert` flags it in debug builds).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "cosine: vector length mismatch");
    let dot = dot(a, b);
    let na = dot_self(a).sqrt();
    let nb = dot_self(b).sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// Dot product of two equal-length vectors.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "dot: vector length mismatch");
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Euclidean (L2) distance between two equal-length vectors.
pub fn euclidean(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "euclidean: vector length mismatch");
    a.iter()
        .zip(b)
        .map(|(x, y)| {
            let d = x - y;
            d * d
        })
        .sum::<f32>()
        .sqrt()
}

fn dot_self(a: &[f32]) -> f32 {
    a.iter().map(|x| x * x).sum()
}

/// The rank score for `metric` — always "higher is more similar" so callers can
/// sort descending regardless of metric.
fn score(metric: Metric, a: &[f32], b: &[f32]) -> f32 {
    match metric {
        Metric::Cosine => cosine(a, b),
        Metric::Dot => dot(a, b),
        Metric::Euclidean => -euclidean(a, b),
    }
}

// ---- in-memory backend ---------------------------------------------------

/// An in-memory [`VectorStore`] — brute-force search over a hash map. Suitable
/// for tests and small collections.
#[derive(Default)]
pub struct MemoryStore {
    inner: RwLock<Inner>,
}

#[derive(Default)]
struct Inner {
    docs: HashMap<String, Doc>,
    /// Collection dimension, set by the first upserted vector.
    dim: Option<usize>,
}

impl MemoryStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of stored documents.
    pub fn len(&self) -> usize {
        self.inner.read().unwrap().docs.len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl VectorStore for MemoryStore {
    fn upsert(&self, docs: Vec<Doc>) -> BoxFuture<'_, Result<(), VectorStoreError>> {
        Box::pin(async move {
            let mut inner = self.inner.write().unwrap();
            for doc in &docs {
                let expected = *inner.dim.get_or_insert(doc.vector.len());
                if doc.vector.len() != expected {
                    return Err(VectorStoreError::DimMismatch {
                        expected,
                        got: doc.vector.len(),
                    });
                }
            }
            for doc in docs {
                inner.docs.insert(doc.id.clone(), doc);
            }
            Ok(())
        })
    }

    fn search(&self, query: Query) -> BoxFuture<'_, Result<Vec<Hit>, VectorStoreError>> {
        Box::pin(async move {
            let inner = self.inner.read().unwrap();
            if let Some(dim) = inner.dim {
                if query.vector.len() != dim {
                    return Err(VectorStoreError::DimMismatch {
                        expected: dim,
                        got: query.vector.len(),
                    });
                }
            }
            let mut hits: Vec<Hit> = inner
                .docs
                .values()
                .map(|doc| Hit {
                    id: doc.id.clone(),
                    score: score(query.metric, &query.vector, &doc.vector),
                    doc: doc.clone(),
                })
                .collect();
            // Descending by score; non-finite scores (NaN) sink to the bottom,
            // and ties break deterministically by id so `top_k` is stable across
            // the underlying `HashMap`'s iteration order.
            hits.sort_by(|a, b| {
                let by_score = match b.score.partial_cmp(&a.score) {
                    Some(ord) => ord,
                    None => match (a.score.is_nan(), b.score.is_nan()) {
                        (true, false) => std::cmp::Ordering::Greater, // a (NaN) after b
                        (false, true) => std::cmp::Ordering::Less,    // b (NaN) after a
                        _ => std::cmp::Ordering::Equal,
                    },
                };
                by_score.then_with(|| a.id.cmp(&b.id))
            });
            hits.truncate(query.top_k);
            Ok(hits)
        })
    }

    fn delete(&self, ids: Vec<String>) -> BoxFuture<'_, Result<(), VectorStoreError>> {
        Box::pin(async move {
            let mut inner = self.inner.write().unwrap();
            for id in &ids {
                inner.docs.remove(id);
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn upsert_search_delete_roundtrip() {
        let store = MemoryStore::new();
        store
            .upsert(vec![
                Doc::new("a", vec![1.0, 0.0, 0.0]),
                Doc::new("b", vec![0.0, 1.0, 0.0]),
                Doc::new("c", vec![0.0, 0.0, 1.0]),
            ])
            .await
            .unwrap();
        assert_eq!(store.len(), 3);

        let hits = store
            .search(Query::new(vec![0.9, 0.1, 0.0], 2))
            .await
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, "a"); // closest by cosine

        store.delete(vec!["a".into()]).await.unwrap();
        let hits = store
            .search(Query::new(vec![0.9, 0.1, 0.0], 2))
            .await
            .unwrap();
        assert!(hits.iter().all(|h| h.id != "a"));
    }

    #[tokio::test]
    async fn rejects_dimension_mismatch() {
        let store = MemoryStore::new();
        store
            .upsert(vec![Doc::new("a", vec![1.0, 2.0])])
            .await
            .unwrap();
        let err = store.upsert(vec![Doc::new("b", vec![1.0, 2.0, 3.0])]).await;
        assert!(matches!(
            err,
            Err(VectorStoreError::DimMismatch {
                expected: 2,
                got: 3
            })
        ));
    }

    #[test]
    fn metrics() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert!((dot(&[1.0, 2.0], &[3.0, 4.0]) - 11.0).abs() < 1e-6);
        assert!((euclidean(&[0.0, 0.0], &[3.0, 4.0]) - 5.0).abs() < 1e-6);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[tokio::test]
    async fn metric_selection_ranks_differently() {
        let store = MemoryStore::new();
        store
            .upsert(vec![
                Doc::new("near", vec![1.0, 1.0]),
                Doc::new("far", vec![5.0, 5.0]),
            ])
            .await
            .unwrap();
        // Euclidean: "near" is closest to (1,1).
        let hits = store
            .search(Query::new(vec![1.0, 1.0], 1).metric(Metric::Euclidean))
            .await
            .unwrap();
        assert_eq!(hits[0].id, "near");
    }
}

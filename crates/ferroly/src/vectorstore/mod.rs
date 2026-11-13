//! Vector search abstraction — a [`VectorStore`] trait, an in-memory backend,
//! similarity metrics, metadata filters, and a text chunker.
//!
//! Application code depends on the trait; construct the backend you want (or
//! hold an `Arc<dyn VectorStore>` for runtime indirection). The trait is the
//! extension point: approximate-nearest-neighbor indexes (HNSW) and remote
//! backends (Pinecone, Qdrant, pgvector) are not bundled — implement
//! [`VectorStore`] for them.
//!
//! ```no_run
//! # use ferroly::vectorstore::{Doc, Filter, MemoryStore, Query, VectorStore};
//! # use ferroly::codec::Value;
//! # async fn example() {
//! let store = MemoryStore::new();
//! store.upsert(vec![
//!     Doc::new("a", vec![1.0, 0.0])
//!         .with_metadata(Value::Object(vec![("lang".into(), "en".into())])),
//!     Doc::new("b", vec![0.0, 1.0]),
//! ]).await.unwrap();
//!
//! let query = Query::new(vec![0.9, 0.1], 1).filter(Filter::eq("lang", "en"));
//! let hits = store.search(query).await.unwrap();
//! assert_eq!(hits[0].id, "a");
//! # }
//! ```
//!
//! # Scores, per metric
//!
//! [`Hit::score`] is always "higher is more similar", but its range depends on
//! the [`Metric`], and [`Query::min_score`] is compared against that raw value:
//!
//! | Metric | Score | Range | Identical vectors |
//! |---|---|---|---|
//! | [`Metric::Cosine`] | cosine of the angle | `[-1, 1]` | `1` |
//! | [`Metric::Dot`] | dot product | unbounded | depends on magnitude |
//! | [`Metric::Euclidean`] | **negated** L2 distance | `(-∞, 0]` | `0` |
//!
//! [`Metric::to_unit_interval`] maps a cosine or Euclidean score onto `[0, 1]`
//! when a comparable "similarity" is wanted.
//!
//! # Runtime
//!
//! This module depends on no async runtime. [`MemoryStore`] does its work —
//! including a full scan per search — synchronously inside the returned
//! future, so a large search occupies the executor thread that polls it. For
//! big collections call the `*_sync` methods from your runtime's blocking pool
//! instead (see [`MemoryStore`]).

#![deny(missing_docs)]

use std::future::Future;
use std::pin::Pin;

use ferroly::codec::Value;
use ferroly_derive::FerrolyError;

mod chunk;
mod filter;
mod memory;

pub use chunk::{split_paragraphs, split_sentences, Boundary, Chunk, Chunker};
pub use filter::Filter;
pub use memory::MemoryStore;

/// A boxed, `Send` future — the manual `async fn`-in-trait desugaring that keeps
/// [`VectorStore`] object-safe without `async-trait`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A dense embedding vector.
pub type Vector = Vec<f32>;

/// The similarity / distance metric used to rank search results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Metric {
    /// Cosine similarity (angle); scale-invariant. Scores lie in `[-1, 1]`.
    /// The default.
    #[default]
    Cosine,
    /// Dot product; sensitive to magnitude. Scores are unbounded.
    Dot,
    /// Euclidean (L2) distance, ranked as `-distance` so nearest ranks first.
    /// Scores lie in `(-∞, 0]`.
    Euclidean,
}

impl Metric {
    /// Maps a score produced under this metric onto `[0, 1]`, where 1 means
    /// identical:
    ///
    /// * [`Cosine`](Metric::Cosine): `(1 + score) / 2`;
    /// * [`Euclidean`](Metric::Euclidean): `1 / (1 + distance)`;
    /// * [`Dot`](Metric::Dot): `None` — a dot product has no fixed range, so
    ///   there is no meaningful normalization (for unit-length vectors it
    ///   equals the cosine; use that metric instead).
    ///
    /// ```
    /// use ferroly::vectorstore::Metric;
    ///
    /// assert_eq!(Metric::Cosine.to_unit_interval(1.0), Some(1.0));
    /// assert_eq!(Metric::Cosine.to_unit_interval(-1.0), Some(0.0));
    /// assert_eq!(Metric::Euclidean.to_unit_interval(0.0), Some(1.0));
    /// assert_eq!(Metric::Euclidean.to_unit_interval(-3.0), Some(0.25));
    /// assert_eq!(Metric::Dot.to_unit_interval(42.0), None);
    /// ```
    pub fn to_unit_interval(self, score: f32) -> Option<f32> {
        match self {
            Metric::Cosine => Some(((1.0 + score) / 2.0).clamp(0.0, 1.0)),
            Metric::Euclidean => Some(1.0 / (1.0 + score.abs())),
            Metric::Dot => None,
        }
    }
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
///
/// The struct is `#[non_exhaustive]`: build one with [`Query::new`] and the
/// builder methods.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Query {
    /// The query embedding.
    pub vector: Vector,
    /// Maximum number of hits to return.
    pub top_k: usize,
    /// The ranking metric.
    pub metric: Metric,
    /// Only documents whose metadata matches are considered.
    pub filter: Option<Filter>,
    /// Hits scoring below this are dropped. Compared against the raw score of
    /// the query's [`Metric`] (see the [module docs](self#scores-per-metric)).
    pub min_score: Option<f32>,
}

impl Query {
    /// Creates a cosine-similarity query for the `top_k` nearest docs.
    pub fn new(vector: Vector, top_k: usize) -> Self {
        Self {
            vector,
            top_k,
            metric: Metric::Cosine,
            filter: None,
            min_score: None,
        }
    }

    /// Overrides the ranking metric.
    pub fn metric(mut self, metric: Metric) -> Self {
        self.metric = metric;
        self
    }

    /// Restricts the search to documents whose metadata matches `filter`.
    pub fn filter(mut self, filter: Filter) -> Self {
        self.filter = Some(filter);
        self
    }

    /// Drops hits scoring below `min_score` — a raw score under the query's
    /// metric, so for [`Metric::Euclidean`] pass a negated distance
    /// (`min_score(-0.5)` keeps documents within distance 0.5).
    pub fn min_score(mut self, min_score: f32) -> Self {
        self.min_score = Some(min_score);
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

/// A request for one page of documents, for [`VectorStore::list`].
///
/// `#[non_exhaustive]`: build one with [`ListRequest::new`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ListRequest {
    /// Return documents whose id sorts after this one (the previous page's
    /// [`DocPage::next`]); `None` starts from the beginning.
    pub after: Option<String>,
    /// The maximum number of documents to return (at least 1).
    pub limit: usize,
    /// Only documents whose metadata matches are returned.
    pub filter: Option<Filter>,
}

impl ListRequest {
    /// The first page of up to `limit` documents.
    pub fn new(limit: usize) -> Self {
        Self {
            after: None,
            limit,
            filter: None,
        }
    }

    /// Continues after the document with this id.
    pub fn after(mut self, id: impl Into<String>) -> Self {
        self.after = Some(id.into());
        self
    }

    /// Restricts the listing to documents whose metadata matches `filter`.
    pub fn filter(mut self, filter: Filter) -> Self {
        self.filter = Some(filter);
        self
    }
}

/// One page of documents, from [`VectorStore::list`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DocPage {
    /// The documents on this page, ordered by id.
    pub docs: Vec<Doc>,
    /// The id to pass to [`ListRequest::after`] for the next page, or `None`
    /// if this was the last one.
    pub next: Option<String>,
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
    /// A vector or query parameter was unusable: an empty vector, a NaN or
    /// infinite component, or a NaN score threshold.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// The backend does not implement this operation.
    #[error("unsupported operation: {0}")]
    Unsupported(String),
    /// A backend-specific failure (network, etc.).
    #[error("vector store backend error: {0}")]
    Backend(String),
    /// Reading or writing a snapshot file failed.
    #[error("vector store io error: {0}")]
    Io(#[from] std::io::Error),
    /// A snapshot could not be decoded.
    #[error("vector store snapshot error: {0}")]
    Codec(#[from] ferroly::codec::CodecError),
}

fn unsupported<T: Send + 'static>(op: &str) -> BoxFuture<'static, Result<T, VectorStoreError>> {
    let op = op.to_string();
    Box::pin(async move { Err(VectorStoreError::Unsupported(op)) })
}

/// A vector store: upsert documents, search by nearest neighbor, delete by id.
///
/// Async methods return a [`BoxFuture`]; remote backends await network I/O, the
/// in-memory backend does its work inside the first poll.
///
/// # Implementing a backend
///
/// `upsert`, `search` and `delete` are required. The remaining methods have
/// defaults that return [`VectorStoreError::Unsupported`], so adding them here
/// did not break existing implementations — override the ones your backend
/// can serve.
///
/// A [`Query`] carries an optional [`filter`](Query::filter) and
/// [`min_score`](Query::min_score). A backend that cannot honor one of them
/// **must return [`VectorStoreError::Unsupported`]** rather than ignore it:
/// a filter is often an access-control boundary, and silently returning
/// unfiltered results would leak across it. ([`Filter::matches`] is public
/// for backends that filter candidates themselves.)
///
/// Tenant or collection isolation is a property of the *handle*, not of each
/// call: [`MemoryStore::namespace`] returns a store bound to one namespace,
/// and a remote backend would bind a handle to one index or collection in the
/// same way.
pub trait VectorStore: Send + Sync {
    /// Inserts or replaces documents by id. All vectors must share the
    /// collection's dimension (established by the first successful upsert).
    ///
    /// The batch is all-or-nothing: if any document is rejected, none is
    /// stored and the collection is left exactly as it was.
    fn upsert(&self, docs: Vec<Doc>) -> BoxFuture<'_, Result<(), VectorStoreError>>;

    /// Returns up to `query.top_k` hits, most similar first.
    fn search(&self, query: Query) -> BoxFuture<'_, Result<Vec<Hit>, VectorStoreError>>;

    /// Removes documents by id. Unknown ids are silently skipped (idempotent).
    fn delete(&self, ids: Vec<String>) -> BoxFuture<'_, Result<(), VectorStoreError>>;

    /// Fetches documents by id, in the order asked; unknown ids are skipped.
    fn get(&self, ids: Vec<String>) -> BoxFuture<'_, Result<Vec<Doc>, VectorStoreError>> {
        let _ = ids;
        unsupported("get")
    }

    /// Counts the documents matching `filter` (all of them for `None`).
    fn count(&self, filter: Option<Filter>) -> BoxFuture<'_, Result<usize, VectorStoreError>> {
        let _ = filter;
        unsupported("count")
    }

    /// Removes every document matching `filter`, returning how many were
    /// removed.
    fn delete_where(&self, filter: Filter) -> BoxFuture<'_, Result<usize, VectorStoreError>> {
        let _ = filter;
        unsupported("delete_where")
    }

    /// Returns one page of documents ordered by id. Pass each page's
    /// [`DocPage::next`] to [`ListRequest::after`] until it is `None`.
    fn list(&self, request: ListRequest) -> BoxFuture<'_, Result<DocPage, VectorStoreError>> {
        let _ = request;
        unsupported("list")
    }
}

// ---- similarity metrics --------------------------------------------------

/// Cosine similarity of two equal-length vectors, in `[-1, 1]` (0 if either is
/// a zero vector).
///
/// Sums are accumulated in `f64`, so vectors with very large or very small
/// components do not overflow to infinity or underflow to zero along the way,
/// and the result is clamped to `[-1, 1]` against rounding.
///
/// The vectors should share a dimension; on a length mismatch these functions
/// operate over the shorter length (a `debug_assert` flags it in debug builds).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "cosine: vector length mismatch");
    cosine_with_norms(dot64(a, b), norm64(a), norm64(b)) as f32
}

/// Dot product of two equal-length vectors.
///
/// Accumulated in `f64`; the result saturates to `±inf` only if the true value
/// is outside the `f32` range.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "dot: vector length mismatch");
    dot64(a, b) as f32
}

/// Euclidean (L2) distance between two equal-length vectors.
///
/// Accumulated in `f64`; the result saturates to `inf` only if the true
/// distance is outside the `f32` range.
pub fn euclidean(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "euclidean: vector length mismatch");
    euclidean64(a, b) as f32
}

// The squares and products of `f32` values span roughly 1e-90..1e77, well
// inside `f64`'s range, so none of the sums below can overflow or lose a
// non-zero term to underflow for any finite input.

pub(crate) fn dot64(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| f64::from(*x) * f64::from(*y))
        .sum()
}

pub(crate) fn norm64(a: &[f32]) -> f64 {
    a.iter()
        .map(|x| f64::from(*x) * f64::from(*x))
        .sum::<f64>()
        .sqrt()
}

pub(crate) fn euclidean64(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| {
            let d = f64::from(*x) - f64::from(*y);
            d * d
        })
        .sum::<f64>()
        .sqrt()
}

/// Cosine from a precomputed dot product and the two vector norms.
pub(crate) fn cosine_with_norms(dot: f64, norm_a: f64, norm_b: f64) -> f64 {
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    let cosine = dot / (norm_a * norm_b);
    if cosine.is_nan() {
        0.0
    } else {
        cosine.clamp(-1.0, 1.0)
    }
}

/// Checks that a vector is usable: non-empty, every component finite.
pub(crate) fn check_vector(what: &str, vector: &[f32]) -> Result<(), VectorStoreError> {
    if vector.is_empty() {
        return Err(VectorStoreError::InvalidInput(format!(
            "{what}: vector is empty"
        )));
    }
    match vector.iter().position(|x| !x.is_finite()) {
        Some(at) => Err(VectorStoreError::InvalidInput(format!(
            "{what}: component {at} is {}",
            vector[at]
        ))),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert!((cosine(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-6);
        assert!((dot(&[1.0, 2.0], &[3.0, 4.0]) - 11.0).abs() < 1e-6);
        assert!((euclidean(&[0.0, 0.0], &[3.0, 4.0]) - 5.0).abs() < 1e-6);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
        assert_eq!(cosine(&[1.0, 1.0], &[0.0, 0.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[0.0, 0.0]), 0.0);
        assert_eq!(cosine(&[], &[]), 0.0);
    }

    #[test]
    fn cosine_survives_extreme_magnitudes() {
        // In f32, the squared norm of these overflows to infinity, which used
        // to turn the result into 0 or NaN.
        let huge = [3.0e38_f32, 3.0e38, -3.0e38];
        assert!((cosine(&huge, &huge) - 1.0).abs() < 1e-6);
        let scaled = [1.0_f32, 1.0, -1.0];
        assert!((cosine(&huge, &scaled) - 1.0).abs() < 1e-6);
        let opposite = [-3.0e38_f32, -3.0e38, 3.0e38];
        assert!((cosine(&huge, &opposite) + 1.0).abs() < 1e-6);
        assert!(cosine(&[f32::MAX, 0.0], &[0.0, f32::MAX]).abs() < 1e-6);

        // In f32, the squared norm of these underflows to zero, which used
        // to make them look like zero vectors.
        let tiny = [1.0e-30_f32, 2.0e-30, 2.0e-30];
        assert!((cosine(&tiny, &tiny) - 1.0).abs() < 1e-6);
        assert!((cosine(&tiny, &[1.0, 2.0, 2.0]) - 1.0).abs() < 1e-6);
        let subnormal = [f32::MIN_POSITIVE / 4.0, 0.0];
        assert!((cosine(&subnormal, &subnormal) - 1.0).abs() < 1e-6);

        // Mixed scales, and the result always stays in range.
        let c = cosine(&huge, &tiny);
        assert!((-1.0..=1.0).contains(&c) && c.is_finite());
        for v in [&huge[..], &tiny[..], &scaled[..]] {
            let c = cosine(v, v);
            assert!((1.0 - 1e-6..=1.0).contains(&c));
        }
    }

    #[test]
    fn dot_and_euclidean_do_not_overflow_prematurely() {
        // Each product overflows f32, but the terms cancel.
        assert_eq!(dot(&[3.0e38, 3.0e38], &[2.0, -2.0]), 0.0);
        // The squared differences overflow f32; the distance itself fits.
        let d = euclidean(&[2.0e38, 0.0], &[-1.0e38, 0.0]);
        assert!((d / 3.0e38 - 1.0).abs() < 1e-6);
        // Tiny differences are not flushed to zero.
        let d = euclidean(&[1.0e-30, 0.0], &[0.0, 0.0]);
        assert!(d > 0.0 && (d / 1.0e-30 - 1.0).abs() < 1e-6);
        // A true result beyond f32 saturates rather than going NaN.
        assert_eq!(
            dot(&[f32::MAX, f32::MAX], &[f32::MAX, f32::MAX]),
            f32::INFINITY
        );
    }

    #[test]
    fn check_vector_rejects_unusable_input() {
        assert!(check_vector("v", &[0.0, -1.5, f32::MAX]).is_ok());
        for bad in [
            &[][..],
            &[1.0, f32::NAN][..],
            &[f32::INFINITY][..],
            &[0.0, f32::NEG_INFINITY, 1.0][..],
        ] {
            assert!(matches!(
                check_vector("doc `a`", bad),
                Err(VectorStoreError::InvalidInput(_))
            ));
        }
        let message = check_vector("doc `a`", &[1.0, f32::NAN])
            .unwrap_err()
            .to_string();
        assert!(message.contains("doc `a`") && message.contains("component 1"));
    }

    #[test]
    fn unit_interval_mapping() {
        assert_eq!(Metric::Cosine.to_unit_interval(0.0), Some(0.5));
        // Out-of-range input (rounding) is clamped.
        assert_eq!(Metric::Cosine.to_unit_interval(1.0001), Some(1.0));
        assert_eq!(Metric::Euclidean.to_unit_interval(-1.0), Some(0.5));
        assert_eq!(Metric::Dot.to_unit_interval(0.0), None);
    }

    /// A backend written before the trait grew still compiles, and reports
    /// the new operations as unsupported.
    struct Legacy;

    impl VectorStore for Legacy {
        fn upsert(&self, _docs: Vec<Doc>) -> BoxFuture<'_, Result<(), VectorStoreError>> {
            Box::pin(async { Ok(()) })
        }
        fn search(&self, _query: Query) -> BoxFuture<'_, Result<Vec<Hit>, VectorStoreError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn delete(&self, _ids: Vec<String>) -> BoxFuture<'_, Result<(), VectorStoreError>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn defaulted_trait_methods_report_unsupported() {
        let store: std::sync::Arc<dyn VectorStore> = std::sync::Arc::new(Legacy);
        store.upsert(vec![Doc::new("a", vec![1.0])]).await.unwrap();
        assert!(matches!(
            store.get(vec!["a".into()]).await,
            Err(VectorStoreError::Unsupported(op)) if op == "get"
        ));
        assert!(matches!(
            store.count(None).await,
            Err(VectorStoreError::Unsupported(_))
        ));
        assert!(matches!(
            store.delete_where(Filter::exists("x")).await,
            Err(VectorStoreError::Unsupported(_))
        ));
        assert!(matches!(
            store.list(ListRequest::new(10)).await,
            Err(VectorStoreError::Unsupported(_))
        ));
    }
}

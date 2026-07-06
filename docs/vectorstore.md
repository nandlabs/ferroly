# ferroly::vectorstore

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `vectorstore` · **Module:** `ferroly::vectorstore`

## Overview

`vectorstore` is a small abstraction for nearest-neighbor search over dense
embedding vectors. Application code
programs against the [`VectorStore`] trait: `upsert` documents, `search` by
similarity, `delete` by id. A batteries-included in-memory backend
([`MemoryStore`]) does brute-force scans over a hash map, which is ideal for
tests, prototypes, and small collections.

It pairs naturally with the [genai](genai.md) embeddings API: run text through
an embedding provider to get vectors, store them here, then answer queries by
embedding the query and searching. See the end-to-end sketch below.

Design is deliberately minimal and dependency-free. Cloud-scale backends
(Pinecone, Qdrant, pgvector) are **planned satellite crates**, not part of this
module — the trait is the stable seam so you can swap `MemoryStore` for a hosted
index later without touching call sites.

### Design notes

- **No `Close`/`Shutdown` method.** Cleanup is Rust's `Drop`; the in-memory
  store owns nothing that needs an explicit close.
- **No context parameter.** Methods take just their arguments;
  cancellation/timeouts are the caller's concern (e.g. wrap the returned future
  with `tokio::time::timeout` if you need a deadline against a network backend).

## Enabling

This is an **optional, non-default** feature. It is **std-only** — it does *not*
pull in `tokio`. Async methods return a hand-rolled boxed future ([`BoxFuture`])
that resolves immediately for the in-memory backend, so you can `.await` them
under any executor (or a trivial block-on) without a runtime dependency.

```toml
[dependencies]
ferroly = { version = "*", features = ["vectorstore"] }
```

The feature implies [`codec`](codec.md) (used for document metadata as
[`Value`](codec.md)).

## Quick start

```rust
use ferroly::vectorstore::{Doc, MemoryStore, Query, VectorStore};

# async fn run() {
let store = MemoryStore::new();

store.upsert(vec![
    Doc::new("a", vec![1.0, 0.0]),
    Doc::new("b", vec![0.0, 1.0]),
]).await.unwrap();

let hits = store.search(Query::new(vec![0.9, 0.1], 1)).await.unwrap();
assert_eq!(hits[0].id, "a");   // nearest by cosine similarity
# }
```

## API reference

### Type aliases

| Alias | Definition | Purpose |
|---|---|---|
| `Vector` | `Vec<f32>` | A dense embedding vector. |
| `BoxFuture<'a, T>` | `Pin<Box<dyn Future<Output = T> + Send + 'a>>` | The boxed, `Send` future returned by trait methods — the manual "`async fn` in trait" desugaring that keeps [`VectorStore`] object-safe (usable as `dyn VectorStore`) without an `async-trait` dependency. |

### `Metric`

The similarity/distance metric used to rank results.

```rust
pub enum Metric {
    Cosine,     // default: cosine similarity (angle), scale-invariant
    Dot,        // dot product, sensitive to magnitude
    Euclidean,  // L2 distance, ranked as -distance so nearest ranks first
}
```

`Metric` derives `Debug, Clone, Copy, PartialEq, Eq, Default`. `Cosine` is the
`Default`.

### `Doc`

A stored document: id, embedding, and optional metadata.

| Field | Type | Notes |
|---|---|---|
| `id` | `String` | Caller-assigned unique id (the upsert/delete key). |
| `vector` | `Vector` | The embedding. |
| `metadata` | [`Value`](codec.md) | Arbitrary metadata (source text, tags, …). `Value::Null` when unset. |

Constructors:

- `Doc::new(id: impl Into<String>, vector: Vector) -> Doc` — no metadata.
- `Doc::with_metadata(self, metadata: Value) -> Doc` — builder that attaches
  metadata and returns `self`.

Derives `Debug, Clone, PartialEq`.

```rust
use ferroly::vectorstore::Doc;
use ferroly::codec::Value;

let doc = Doc::new("doc-42", vec![0.1, 0.2, 0.3])
    .with_metadata(Value::Object(vec![
        ("title".into(), "Intro to vectors".into()),
        ("source".into(), "wiki".into()),
    ]));
```

### `Query`

A nearest-neighbor query.

| Field | Type | Notes |
|---|---|---|
| `vector` | `Vector` | The query embedding. |
| `top_k` | `usize` | Maximum number of hits to return. |
| `metric` | [`Metric`] | The ranking metric. |

Constructors:

- `Query::new(vector: Vector, top_k: usize) -> Query` — defaults `metric` to
  `Metric::Cosine`.
- `Query::metric(self, metric: Metric) -> Query` — builder overriding the
  metric.

Derives `Debug, Clone`.

```rust
use ferroly::vectorstore::{Metric, Query};

let q = Query::new(vec![0.1, 0.2, 0.3], 5).metric(Metric::Dot);
```

### `Hit`

A single search result.

| Field | Type | Notes |
|---|---|---|
| `id` | `String` | The matched document id. |
| `score` | `f32` | Rank score under the query's metric — **always "higher is more similar"**, so results are sorted descending regardless of metric. |
| `doc` | [`Doc`] | The full matched document (including its metadata). |

Derives `Debug, Clone, PartialEq`. Because Euclidean is ranked as `-distance`,
its `score` is `≤ 0` (0 = identical); cosine scores land in `[-1, 1]`; dot
scores are unbounded.

### `VectorStore` trait

```rust
pub trait VectorStore: Send + Sync {
    fn upsert(&self, docs: Vec<Doc>) -> BoxFuture<'_, Result<(), VectorStoreError>>;
    fn search(&self, query: Query) -> BoxFuture<'_, Result<Vec<Hit>, VectorStoreError>>;
    fn delete(&self, ids: Vec<String>) -> BoxFuture<'_, Result<(), VectorStoreError>>;
}
```

- **`upsert`** — inserts or replaces documents by id. All vectors must share the
  collection's dimension, which is established by the very first vector ever
  upserted. A mismatched vector yields [`VectorStoreError::DimMismatch`].
- **`search`** — returns up to `query.top_k` hits, most similar first.
- **`delete`** — removes documents by id. Unknown ids are silently skipped, so
  delete is **idempotent**.

The trait is `Send + Sync`, so you can hold an `Arc<dyn VectorStore>` for
runtime backend indirection:

```rust
use std::sync::Arc;
use ferroly::vectorstore::{MemoryStore, VectorStore};

let store: Arc<dyn VectorStore> = Arc::new(MemoryStore::new());
```

### `MemoryStore`

An in-memory [`VectorStore`] implementation: a `RwLock`-guarded hash map with a
brute-force linear scan on every search.

- `MemoryStore::new() -> MemoryStore` — an empty store. (Also `Default`.)
- `MemoryStore::len(&self) -> usize` — number of stored documents.
- `MemoryStore::is_empty(&self) -> bool` — whether the store is empty.

The collection dimension is fixed by the first upserted vector; subsequent
upserts and searches with a different dimension fail with `DimMismatch`.
Searches sort descending by score with NaN sinking to the bottom, then truncate
to `top_k`.

### `VectorStoreError`

```rust
pub enum VectorStoreError {
    DimMismatch { expected: usize, got: usize },
    Backend(String),
}
```

| Variant | Meaning |
|---|---|
| `DimMismatch { expected, got }` | A vector's dimension did not match the collection's established dimension. Raised by `MemoryStore` on upsert or search. |
| `Backend(String)` | A backend-specific failure (network, remote service error) — reserved for the planned cloud backends; `MemoryStore` never returns it. |

Derives `Debug` and the crate's `FerrolyError` (so it implements
`std::error::Error` + `Display`).

### Free similarity functions

Exposed for direct use (e.g. re-ranking outside a store):

| Function | Returns |
|---|---|
| `cosine(a: &[f32], b: &[f32]) -> f32` | Cosine similarity in `[-1, 1]`; **0** if either input is a zero vector. |
| `dot(a: &[f32], b: &[f32]) -> f32` | Dot product. |
| `euclidean(a: &[f32], b: &[f32]) -> f32` | Euclidean (L2) distance. |

All three assume equal-length slices.

```rust
use ferroly::vectorstore::{cosine, dot, euclidean};

assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
assert!((dot(&[1.0, 2.0], &[3.0, 4.0]) - 11.0).abs() < 1e-6);
assert!((euclidean(&[0.0, 0.0], &[3.0, 4.0]) - 5.0).abs() < 1e-6);
```

## In depth

### Choosing a metric

The same collection can be queried under different metrics per query, and they
can rank differently:

```rust
use ferroly::vectorstore::{Doc, MemoryStore, Metric, Query, VectorStore};

# async fn run() {
let store = MemoryStore::new();
store.upsert(vec![
    Doc::new("near", vec![1.0, 1.0]),
    Doc::new("far",  vec![5.0, 5.0]),
]).await.unwrap();

// Cosine can't distinguish (1,1) from (5,5) — they point the same way.
// Euclidean does: "near" is closest to the query (1,1).
let hits = store
    .search(Query::new(vec![1.0, 1.0], 1).metric(Metric::Euclidean))
    .await
    .unwrap();
assert_eq!(hits[0].id, "near");
# }
```

Rule of thumb: use **Cosine** for normalized text embeddings (direction, not
magnitude), **Dot** when magnitude is meaningful (or vectors are pre-normalized
and you want speed), **Euclidean** for spatial/coordinate-like data.

### Upsert semantics and dimensions

`upsert` replaces on id collision, so re-upserting a doc with the same id
updates it in place. Because the first vector fixes the collection dimension,
mixing dimensions is a hard error:

```rust
use ferroly::vectorstore::{Doc, MemoryStore, VectorStore, VectorStoreError};

# async fn run() {
let store = MemoryStore::new();
store.upsert(vec![Doc::new("a", vec![1.0, 2.0])]).await.unwrap(); // dim = 2

let err = store.upsert(vec![Doc::new("b", vec![1.0, 2.0, 3.0])]).await;
assert!(matches!(err, Err(VectorStoreError::DimMismatch { expected: 2, got: 3 })));
# }
```

### End-to-end: embed → store → search

Combined with [genai](genai.md) embeddings, `vectorstore` is the storage half of
a retrieval pipeline. Embeddings require a configured provider, so this sketch is
**conceptual** — substitute the real genai embedding call for `embed`:

```rust
use ferroly::vectorstore::{Doc, MemoryStore, Query, VectorStore};

// Pretend this calls a genai embedding provider and returns a Vec<f32>.
async fn embed(text: &str) -> Vec<f32> { /* provider.embed(text).await */
    todo!()
}

# async fn run() {
let store = MemoryStore::new();

// 1. Embed and store your corpus.
for (id, text) in [("d1", "cats are great"), ("d2", "rust is fast")] {
    let vector = embed(text).await;
    store.upsert(vec![
        Doc::new(id, vector).with_metadata(text.into()),
    ]).await.unwrap();
}

// 2. Embed the query and search.
let qvec = embed("tell me about felines").await;
let hits = store.search(Query::new(qvec, 3)).await.unwrap();

for hit in hits {
    println!("{} (score {:.3}): {:?}", hit.id, hit.score, hit.doc.metadata);
}
# }
```

## Error handling

Every method returns `Result<_, VectorStoreError>`. For `MemoryStore` the only
error is `DimMismatch`; `Backend` is reserved for future networked backends.
Because the trait's errors implement `std::error::Error`, they compose with the
crate's [errutils](errutils.md) helpers and `?` in `Box<dyn Error>`-returning
functions.

## Limitations

- `MemoryStore` is **brute force** — search is O(n · dim) per query and the whole
  corpus lives in RAM. Fine for tests and small collections; not an ANN index.
- No persistence: the store is dropped with the process. There is no
  snapshot/load.
- No metadata filtering in `search` (no "where" clause); filter the returned
  `Hit`s yourself, or wait for a richer backend.
- Metrics assume equal-length inputs and do not validate that; only the store's
  `upsert`/`search` enforce dimensions.
- Cloud backends (Pinecone/Qdrant/pgvector) are not yet implemented — they are
  planned satellite crates that will implement the same `VectorStore` trait.

## See also

- [genai](genai.md) — embedding providers that produce the vectors stored here.
- [codec](codec.md) — the [`Value`](codec.md) type used for `Doc::metadata`.
- [errutils](errutils.md) — error helpers `VectorStoreError` composes with.

---
**Related:** [genai](genai.md), [codec](codec.md).

# ferroly::vectorstore

[← Docs index](README.md) · [← Project README](../README.md)

**Feature:** `vectorstore` · **Module:** `ferroly::vectorstore`

## Overview

`vectorstore` is a small abstraction for nearest-neighbor search over dense
embedding vectors. Application code programs against the [`VectorStore`] trait:
`upsert` documents, `search` by similarity (optionally filtered by metadata),
`get` / `list` / `count` / `delete` them. A batteries-included in-memory backend
([`MemoryStore`]) does exact brute-force scans — ideal for tests, prototypes,
and small collections — with namespaces for tenant isolation and a JSON
snapshot for persistence.

Also here:

- [`Filter`](#filter) — metadata predicates (equality, membership, ranges,
  and/or/not).
- [`Chunker`](#text-chunking) — splits text into overlapping, size-bounded
  chunks at character, sentence or paragraph boundaries.
- Free similarity functions (`cosine`, `dot`, `euclidean`).

It pairs naturally with the [genai](genai.md) embeddings API: chunk a document,
run each chunk through an embedding provider, store the vectors here, then
answer queries by embedding the query and searching.

**Out of scope:** approximate-nearest-neighbor indexes (HNSW and friends) and
remote backends (Pinecone, Qdrant, pgvector). The [`VectorStore`] trait is the
extension point for both — see [Implementing a backend](#implementing-a-backend).

### Design notes

- **No `Close`/`Shutdown` method.** Cleanup is Rust's `Drop`.
- **No context parameter.** Cancellation/timeouts are the caller's concern
  (e.g. wrap the returned future in a timeout against a network backend).
- **Isolation by handle.** A tenant or collection is a property of the store
  handle, not an argument to each call — see [Namespaces](#namespaces).

## Enabling

This is an **optional, non-default** feature. It is **std-only** — it does *not*
pull in `tokio` or any async runtime. Async methods return a hand-rolled boxed
future ([`BoxFuture`]) that completes on its first poll for the in-memory
backend, so you can `.await` them under any executor.

```toml
[dependencies]
ferroly = { version = "*", features = ["vectorstore"] }
```

The feature implies [`codec`](codec.md) (document metadata is a
[`Value`](codec.md), and snapshots use the JSON codec).

## Quick start

```rust
use ferroly::codec::Value;
use ferroly::vectorstore::{Doc, Filter, MemoryStore, Query, VectorStore};

# async fn run() {
let store = MemoryStore::new();

store.upsert(vec![
    Doc::new("a", vec![1.0, 0.0])
        .with_metadata(Value::Object(vec![("lang".into(), "en".into())])),
    Doc::new("b", vec![0.0, 1.0]),
]).await.unwrap();

let hits = store
    .search(Query::new(vec![0.9, 0.1], 1).filter(Filter::eq("lang", "en")))
    .await
    .unwrap();
assert_eq!(hits[0].id, "a");   // nearest by cosine similarity
# }
```

## API reference

### Type aliases

| Alias | Definition | Purpose |
|---|---|---|
| `Vector` | `Vec<f32>` | A dense embedding vector. |
| `BoxFuture<'a, T>` | `Pin<Box<dyn Future<Output = T> + Send + 'a>>` | The boxed, `Send` future returned by trait methods — the manual "`async fn` in trait" desugaring that keeps [`VectorStore`] object-safe (usable as `dyn VectorStore`) without an `async-trait` dependency. |

### `Metric` and scores

```rust
pub enum Metric {
    Cosine,     // default: cosine similarity (angle), scale-invariant
    Dot,        // dot product, sensitive to magnitude
    Euclidean,  // L2 distance, ranked as -distance so nearest ranks first
}

impl Metric {
    pub fn to_unit_interval(self, score: f32) -> Option<f32>;
}
```

`Metric` derives `Debug, Clone, Copy, PartialEq, Eq, Default` (`Cosine` is the
default).

A [`Hit`](#hit)'s `score` is always "higher is more similar", but its **range
depends on the metric** — and `Query::min_score` is compared against that raw
value:

| Metric | Score | Range | Identical vectors |
|---|---|---|---|
| `Cosine` | cosine of the angle | `[-1, 1]` | `1` |
| `Dot` | dot product | unbounded | depends on magnitude |
| `Euclidean` | **negated** L2 distance | `(-∞, 0]` | `0` |

`Metric::to_unit_interval(score)` maps a score onto `[0, 1]` (1 = identical)
when a comparable "similarity" is wanted: `(1 + score) / 2` for cosine,
`1 / (1 + distance)` for Euclidean, and `None` for dot — a dot product has no
fixed range, so there is nothing meaningful to normalize to (for unit-length
vectors it equals the cosine; use that metric).

Zero vectors score `0` under cosine, by definition here (the angle is
undefined).

### `Doc`

A stored document: id, embedding, and optional metadata.

| Field | Type | Notes |
|---|---|---|
| `id` | `String` | Caller-assigned unique id (the upsert/delete key). |
| `vector` | `Vector` | The embedding. Must be non-empty with finite components. |
| `metadata` | [`Value`](codec.md) | Arbitrary metadata (source text, tags, …). `Value::Null` when unset. |

- `Doc::new(id: impl Into<String>, vector: Vector) -> Doc` — no metadata.
- `Doc::with_metadata(self, metadata: Value) -> Doc` — attaches metadata.

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

A nearest-neighbor query. `#[non_exhaustive]`: build it with `Query::new` and
the builder methods (the fields are public for reading).

| Field | Type | Notes |
|---|---|---|
| `vector` | `Vector` | The query embedding. |
| `top_k` | `usize` | Maximum number of hits to return. |
| `metric` | [`Metric`](#metric-and-scores) | The ranking metric. |
| `filter` | `Option<`[`Filter`](#filter)`>` | Only matching documents are considered. |
| `min_score` | `Option<f32>` | Hits scoring below this are dropped (raw score under the metric). |

- `Query::new(vector, top_k)` — cosine, no filter, no threshold.
- `.metric(Metric)`, `.filter(Filter)`, `.min_score(f32)` — builders.

```rust
use ferroly::vectorstore::{Filter, Metric, Query};

let q = Query::new(vec![0.1, 0.2, 0.3], 5)
    .filter(Filter::eq("tenant", "acme").and(Filter::gte("year", 2023)))
    .min_score(0.75);

// Euclidean scores are negated distances: keep everything within 0.5.
let near = Query::new(vec![0.1, 0.2, 0.3], 5)
    .metric(Metric::Euclidean)
    .min_score(-0.5);
```

### `Hit`

| Field | Type | Notes |
|---|---|---|
| `id` | `String` | The matched document id. |
| `score` | `f32` | Rank score under the query's metric — see [scores](#metric-and-scores). |
| `doc` | [`Doc`] | The full matched document (including its metadata). |

Derives `Debug, Clone, PartialEq`.

### `Filter`

A predicate over a document's metadata.

```rust
#[non_exhaustive]
pub enum Filter {
    Eq(String, Value), Ne(String, Value), In(String, Vec<Value>),
    Gt(String, Value), Gte(String, Value), Lt(String, Value), Lte(String, Value),
    Exists(String),
    And(Vec<Filter>), Or(Vec<Filter>), Not(Box<Filter>),
}
```

| Constructor | Meaning |
|---|---|
| `Filter::eq(field, value)` | `field == value`, or `field` is an array containing `value` |
| `Filter::ne(field, value)` | the negation of `eq` — true when the field is missing |
| `Filter::is_in(field, values)` | `field` equals one of `values` (or shares an element with them) |
| `Filter::gt` / `gte` / `lt` / `lte(field, value)` | ordering comparisons |
| `Filter::between(field, low, high)` | `low <= field <= high` |
| `Filter::exists(field)` | the field is present (even if null) |
| `a.and(b)`, `a.or(b)`, `!a` / `a.negate()` | boolean combinators (`and`/`or` chains flatten) |
| `Filter::all(iter)`, `Filter::any(iter)` | n-ary and / or (`all([])` matches everything, `any([])` nothing) |
| `filter.matches(&metadata) -> bool` | evaluate against one document's metadata |

Semantics:

- **Fields** are keys of the metadata object. A dotted name (`"source.lang"`)
  descends into nested objects; a key that literally contains a dot is found
  too, and wins.
- **Numbers** compare by value across integer and float representations
  (`1`, `1u64` and `1.0` are equal; large integers compare exactly). Strings
  compare lexicographically, booleans as `false < true`. Values of different
  kinds are never equal and never ordered — such a comparison is `false`.
- **Arrays**: `eq` and `is_in` also match when the field is an array
  *containing* a matching element (`tags` contains `"legal"`). Ordering
  comparisons apply to scalar fields only.
- **Missing fields** fail every condition except `ne` (and a negation). Use
  `exists` to require presence. Metadata that is not an object has no fields.

```rust
use ferroly::codec::Value;
use ferroly::vectorstore::Filter;

let metadata = Value::Object(vec![
    ("tenant".into(), "acme".into()),
    ("year".into(), 2024.into()),
    ("tags".into(), Value::Array(vec!["draft".into(), "legal".into()])),
]);

let filter = Filter::eq("tenant", "acme")
    .and(Filter::between("year", 2020, 2025))
    .and(!Filter::eq("tags", "archived"));
assert!(filter.matches(&metadata));
```

In `MemoryStore` the filter is applied **before** ranking, so `top_k` counts
matching documents only.

### `ListRequest` and `DocPage`

```rust
#[non_exhaustive]
pub struct ListRequest { pub after: Option<String>, pub limit: usize, pub filter: Option<Filter> }
impl ListRequest {
    pub fn new(limit: usize) -> Self;
    pub fn after(self, id: impl Into<String>) -> Self;
    pub fn filter(self, filter: Filter) -> Self;
}

pub struct DocPage { pub docs: Vec<Doc>, pub next: Option<String> }
```

Documents are listed in id order. Pass each page's `next` to
`ListRequest::after` until it is `None`. Paging is by id, not a snapshot:
documents added or removed between calls appear or not according to where
their ids sort. A `limit` of 0 is treated as 1.

### `VectorStore` trait

```rust
pub trait VectorStore: Send + Sync {
    // required
    fn upsert(&self, docs: Vec<Doc>) -> BoxFuture<'_, Result<(), VectorStoreError>>;
    fn search(&self, query: Query) -> BoxFuture<'_, Result<Vec<Hit>, VectorStoreError>>;
    fn delete(&self, ids: Vec<String>) -> BoxFuture<'_, Result<(), VectorStoreError>>;

    // provided — default to Err(VectorStoreError::Unsupported)
    fn get(&self, ids: Vec<String>) -> BoxFuture<'_, Result<Vec<Doc>, VectorStoreError>>;
    fn count(&self, filter: Option<Filter>) -> BoxFuture<'_, Result<usize, VectorStoreError>>;
    fn delete_where(&self, filter: Filter) -> BoxFuture<'_, Result<usize, VectorStoreError>>;
    fn list(&self, request: ListRequest) -> BoxFuture<'_, Result<DocPage, VectorStoreError>>;
}
```

- **`upsert`** — inserts or replaces documents by id. All vectors must share
  the collection's dimension, established by the first *successful* upsert.
  The batch is **all-or-nothing**: if any document is rejected, none is stored
  and the collection — including its dimension — is exactly as it was.
- **`search`** — up to `query.top_k` hits, most similar first.
- **`delete`** — removes documents by id; unknown ids are skipped (idempotent).
- **`get`** — documents by id, in the order asked; unknown ids are skipped.
- **`count`** — how many documents match a filter (all, for `None`).
- **`delete_where`** — removes every match, returning how many.
- **`list`** — one [page](#listrequest-and-docpage) of documents in id order.

The trait is `Send + Sync`, so you can hold an `Arc<dyn VectorStore>` for
runtime backend indirection:

```rust
use std::sync::Arc;
use ferroly::vectorstore::{MemoryStore, VectorStore};

let store: Arc<dyn VectorStore> = Arc::new(MemoryStore::new());
```

### `MemoryStore`

An in-memory [`VectorStore`]: exact, brute-force search over a `RwLock`-guarded
map. `Clone` (a cheap handle on shared data), `Default`, `Debug`,
`Send + Sync + 'static`.

| Method | Notes |
|---|---|
| `MemoryStore::new()` | an empty store; the handle is bound to the `"default"` namespace |
| `namespace(&self, name) -> MemoryStore` | a handle on the same store bound to another namespace |
| `namespace_name()`, `namespaces()`, `drop_namespace(name)` | inspect / remove namespaces |
| `len()`, `is_empty()`, `dimension()` | this namespace's size and vector dimension |
| `clear()` | remove everything in this namespace and forget its dimension |
| `upsert_sync`, `search_sync`, `delete_sync`, `get_sync`, `count_sync`, `delete_where_sync`, `list_sync` | synchronous forms of the trait methods |
| `save(path)`, `MemoryStore::load(path)` | snapshot to / from a JSON file |
| `to_snapshot_json()`, `MemoryStore::from_snapshot_json(text)` | the same, to / from a string |

Behavior worth knowing:

- **Validation.** Every vector is checked on `upsert` — and the query vector on
  `search` — to be non-empty with all components finite. A NaN, an infinity or
  an empty vector is `VectorStoreError::InvalidInput`; so is a NaN `min_score`.
- **Dimension.** Set by the first successful upsert into a namespace; later
  upserts and searches with a different dimension fail with `DimMismatch`. It
  stays fixed even if every document is deleted — call `clear()` to reset it.
  If two documents in one batch share an id, the later one wins.
- **Search cost.** O(n · dim) per query. Scores are computed against the
  stored vectors in place; a bounded heap keeps only the `top_k` best
  candidates, and only those are cloned into the result. Equal scores are
  ordered by id, so results do not depend on insertion order.
- **Numerics.** Sums are accumulated in `f64`, so vectors with very large or
  very small components neither overflow to infinity nor underflow to zero
  along the way; cosine is clamped to `[-1, 1]`. The returned `f32` score
  saturates to `±inf` only if the true dot product or distance exceeds the
  `f32` range.
- **Poisoned locks.** A panic in another thread cannot make a handle unusable:
  lock poisoning is recovered from (writes validate first and then apply, so
  the data is consistent whenever the lock is released).

#### Namespaces

A store holds any number of namespaces, each a separate collection with its
own documents and its own dimension. A handle is bound to exactly one, and
every operation through it — search, upsert, get, list, count, delete — sees
only that namespace. Giving a tenant its own handle isolates it by
construction; there is no per-call namespace argument to forget.

```rust
use ferroly::vectorstore::{Doc, MemoryStore, Query};

let store = MemoryStore::new();
let acme = store.namespace("tenant-acme");
let globex = store.namespace("tenant-globex");

acme.upsert_sync(vec![Doc::new("a", vec![1.0, 0.0])]).unwrap();
globex.upsert_sync(vec![Doc::new("a", vec![1.0, 0.0, 0.0])]).unwrap();   // own dimension

assert_eq!(acme.search_sync(&Query::new(vec![1.0, 0.0], 10)).unwrap().len(), 1);
assert_eq!(store.len(), 0);                       // the default namespace is untouched
assert_eq!(store.namespaces(), ["tenant-acme", "tenant-globex"]);
```

Namespaces are created on first write; `namespaces()` lists those that hold
data.

#### Blocking work and the `*_sync` methods

This module depends on no async runtime, so `MemoryStore` cannot hand work to a
blocking pool by itself: the async trait methods do everything inside the
future's first poll, with no `.await` inside. For small collections that is
fine. For a large one, a search holds an executor thread for the whole scan —
run the synchronous form on your runtime's blocking pool instead (the handle is
`Clone + Send + 'static`):

```rust
use ferroly::vectorstore::{MemoryStore, Query};

# async fn demo(store: MemoryStore, query: Query) -> Result<(), Box<dyn std::error::Error>> {
// With tokio; any runtime's blocking pool works the same way.
let handle = store.clone();
let hits = tokio::task::spawn_blocking(move || handle.search_sync(&query)).await??;
# let _ = hits;
# Ok(()) }
```

`save` and `load` block on file I/O and should be called the same way.

#### Snapshots

`save(path)` writes **every namespace** of the store to one JSON file,
atomically: the data goes to a temporary file beside `path`, is flushed to
disk, and is renamed into place — a crash never leaves a half-written snapshot,
and an existing one is replaced in a single step. `MemoryStore::load(path)`
reads it back into a new store (bound to the `"default"` namespace).

```rust
use ferroly::vectorstore::{Doc, MemoryStore};

# fn demo() -> Result<(), ferroly::vectorstore::VectorStoreError> {
let store = MemoryStore::new();
store.upsert_sync(vec![Doc::new("a", vec![1.0, 0.0])])?;
store.save("index.json")?;

let restored = MemoryStore::load("index.json")?;
assert_eq!(restored.len(), 1);
# Ok(()) }
```

- Nothing is saved automatically, and a snapshot is a full rewrite — there is
  no append-only log. The whole store is encoded in memory before it is
  written.
- **Vectors survive exactly** (bit for bit). **Metadata** survives as far as
  JSON can carry it: integers, floats, strings, booleans, null, arrays and
  objects are preserved; a `Value::Bytes` comes back as an array of integers
  and a non-finite float as null.
- A snapshot is validated on load like fresh input: an unknown format or
  version, a non-finite or empty vector, or a dimension mismatch is an error,
  never a partly loaded store.
- The format is a plain JSON object: `format`
  (`"ferroly.vectorstore.memory"`), `version` (`1`), and `namespaces`, each
  with `name`, `dimension` and `docs` (`id`, `vector`, `metadata`).

### `VectorStoreError`

```rust
#[non_exhaustive]
pub enum VectorStoreError {
    DimMismatch { expected: usize, got: usize },
    InvalidInput(String),
    Unsupported(String),
    Backend(String),
    Io(std::io::Error),
    Codec(ferroly::codec::CodecError),
}
```

| Variant | Meaning |
|---|---|
| `DimMismatch { expected, got }` | A vector's dimension did not match the collection's. Raised on upsert, search, and snapshot load. |
| `InvalidInput(String)` | An unusable vector or parameter: empty, NaN or infinite component, NaN `min_score`, unrecognized snapshot format/version. |
| `Unsupported(String)` | The backend does not implement the operation (the default for the provided trait methods). |
| `Backend(String)` | A backend-specific failure (network, remote service) — for other backends; `MemoryStore` never returns it. |
| `Io` | Reading or writing a snapshot file failed. |
| `Codec` | A snapshot could not be decoded. |

Implements `std::error::Error` + `Display`, and converts into `ferroly::Error`.

### Free similarity functions

Exposed for direct use (e.g. re-ranking outside a store):

| Function | Returns |
|---|---|
| `cosine(a: &[f32], b: &[f32]) -> f32` | Cosine similarity in `[-1, 1]`; **0** if either input is a zero vector. |
| `dot(a: &[f32], b: &[f32]) -> f32` | Dot product. |
| `euclidean(a: &[f32], b: &[f32]) -> f32` | Euclidean (L2) distance. |

All three assume equal-length slices (a `debug_assert` flags a mismatch; in
release builds they operate over the shorter length) and accumulate in `f64`,
so extreme magnitudes do not overflow or underflow part-way. They do not
validate their inputs: a NaN in gives a NaN out.

```rust
use ferroly::vectorstore::{cosine, dot, euclidean};

assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
assert!((cosine(&[3.0e38, 3.0e38], &[1.0, 1.0]) - 1.0).abs() < 1e-6);   // no overflow
assert!((dot(&[1.0, 2.0], &[3.0, 4.0]) - 11.0).abs() < 1e-6);
assert!((euclidean(&[0.0, 0.0], &[3.0, 4.0]) - 5.0).abs() < 1e-6);
```

## Text chunking

Embedding models take bounded input, so documents are split into chunks before
they are embedded and stored.

```rust
pub struct Chunker { /* … */ }
impl Chunker {
    pub fn new(max_chars: usize) -> Self;          // sentence boundaries, no overlap
    pub fn overlap(self, chars: usize) -> Self;
    pub fn boundary(self, boundary: Boundary) -> Self;
    pub fn chunk<'a>(&self, text: &'a str) -> Vec<Chunk<'a>>;
}

#[non_exhaustive]
pub enum Boundary { Char, Sentence /* default */, Paragraph }

pub struct Chunk<'a> { pub text: &'a str, pub start: usize, pub end: usize, pub index: usize }

pub fn split_sentences(text: &str) -> Vec<&str>;
pub fn split_paragraphs(text: &str) -> Vec<&str>;
```

- Sizes are counted in **characters** (Unicode scalar values), not bytes, and
  every cut falls on a character boundary. A `Chunk` is a slice of the input
  (`text`), with its byte range (`start..end`) and position (`index`).
  Characters are only a proxy for a model's tokens — leave headroom.
- No chunk exceeds `max_chars` (0 is treated as 1).
- **`Boundary::Char`** cuts every `max_chars` characters; consecutive chunks
  share exactly `overlap` characters.
- **`Boundary::Sentence`** keeps sentences whole and packs as many as fit.
  A sentence longer than `max_chars` is cut into fixed-size pieces.
- **`Boundary::Paragraph`** keeps paragraphs whole and packs as many as fit.
  A paragraph that is too long is cut at its sentence boundaries (and those,
  if necessary, into fixed-size pieces).
- With the sentence and paragraph boundaries, `overlap` repeats **whole**
  trailing sentences/paragraphs at the start of the next chunk, as long as
  they fit within the overlap; an overlap smaller than the last unit repeats
  nothing. The overlap is capped below `max_chars`, so chunking always
  advances.
- Whitespace between units is dropped at chunk edges; empty or whitespace-only
  text yields no chunks (except with `Boundary::Char`, which cuts whatever it
  is given).

```rust
use ferroly::vectorstore::{Boundary, Chunker};

let text = "Rust is fast. Rust is safe. Rust is fun.";
let chunks = Chunker::new(30).chunk(text);
let texts: Vec<&str> = chunks.iter().map(|c| c.text).collect();
assert_eq!(texts, ["Rust is fast. Rust is safe.", "Rust is fun."]);

let fixed = Chunker::new(4).overlap(2).boundary(Boundary::Char).chunk("abcdefghij");
let texts: Vec<&str> = fixed.iter().map(|c| c.text).collect();
assert_eq!(texts, ["abcd", "cdef", "efgh", "ghij"]);
```

The splitters are punctuation heuristics, not language models:

- `split_sentences` ends a sentence at `.`, `!`, `?`, `…` or their full-width
  forms (`。`, `！`, `？`) — plus any closing quotes or brackets right after —
  when followed by whitespace or the end of the text; a blank line also ends
  one. Abbreviations ("Dr. Smith") are therefore split after the period, while
  decimals and dotted names ("3.14", "example.com") are not. Text without
  spaces after its terminators (common in CJK prose) is not split there.
- `split_paragraphs` splits at one or more blank (empty or whitespace-only)
  lines, with `\n` or `\r\n` line endings.

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
updates it in place. A batch is validated as a whole before anything is stored:

```rust
use ferroly::vectorstore::{Doc, MemoryStore, VectorStore, VectorStoreError};

# async fn run() {
let store = MemoryStore::new();

// A mixed-dimension batch is rejected — and leaves the store untouched, so
// its dimension is still open.
let err = store.upsert(vec![
    Doc::new("a", vec![1.0, 2.0]),
    Doc::new("b", vec![1.0, 2.0, 3.0]),
]).await;
assert!(matches!(err, Err(VectorStoreError::DimMismatch { expected: 2, got: 3 })));
assert_eq!(store.dimension(), None);

store.upsert(vec![Doc::new("b", vec![1.0, 2.0, 3.0])]).await.unwrap();   // dim = 3
assert!(store.upsert(vec![Doc::new("c", vec![f32::NAN, 0.0, 0.0])]).await.is_err());
# }
```

### End-to-end: chunk → embed → store → search

Combined with [genai](genai.md) embeddings, `vectorstore` is the storage half of
a retrieval pipeline. Embeddings require a configured provider, so this sketch is
**conceptual** — substitute the real genai embedding call for `embed`. (There
is no built-in helper tying an `Embedder` to a store; that glue would span two
features.)

```rust
use ferroly::codec::Value;
use ferroly::vectorstore::{Boundary, Chunker, Doc, Filter, MemoryStore, Query, VectorStore};

// Pretend this calls a genai embedding provider and returns a Vec<f32>.
async fn embed(text: &str) -> Vec<f32> { /* provider.embed(text).await */
    todo!()
}

# async fn run(handbook: &str) {
let store = MemoryStore::new().namespace("tenant-acme");

// 1. Chunk, embed and store the corpus.
let chunker = Chunker::new(800).overlap(100).boundary(Boundary::Paragraph);
let mut docs = Vec::new();
for chunk in chunker.chunk(handbook) {
    docs.push(
        Doc::new(format!("handbook#{}", chunk.index), embed(chunk.text).await)
            .with_metadata(Value::Object(vec![
                ("source".into(), "handbook".into()),
                ("text".into(), chunk.text.into()),
            ])),
    );
}
store.upsert(docs).await.unwrap();

// 2. Embed the query and search, restricted to one source.
let qvec = embed("how do I request leave?").await;
let hits = store
    .search(Query::new(qvec, 3).filter(Filter::eq("source", "handbook")).min_score(0.3))
    .await
    .unwrap();

for hit in hits {
    println!("{} (score {:.3}): {:?}", hit.id, hit.score, hit.doc.metadata.get("text"));
}
# }
```

### Implementing a backend

Implement `upsert`, `search` and `delete`; override the provided methods your
backend can serve (the rest report `Unsupported`). Two rules:

- A [`Query`](#query) may carry a `filter` and a `min_score`. A backend that
  cannot honor one **must return `VectorStoreError::Unsupported`** rather than
  ignore it — a filter is often an access-control boundary, and returning
  unfiltered results would leak across it. `Filter::matches` is public for
  backends that post-filter candidates themselves.
- Bind tenant/collection isolation to the handle, as `MemoryStore::namespace`
  does, rather than expecting callers to pass it on every call.

An ANN index (HNSW, IVF) fits the same trait: it changes how `search` finds
candidates, not the interface.

## Error handling

Every fallible method returns `Result<_, VectorStoreError>`. `MemoryStore`
returns `DimMismatch` and `InvalidInput` for bad vectors and queries, and `Io`
/ `Codec` / `InvalidInput` from `save` / `load`; it never panics on bad input
or on a poisoned lock. `VectorStoreError` implements `std::error::Error`, so it
composes with the crate's [errutils](errutils.md) helpers and `?` in
`Box<dyn Error>`-returning functions.

## Limitations

- `MemoryStore` is **brute force** — search is O(n · dim) per query and the whole
  corpus lives in RAM. Fine for tests and small collections; it is not an ANN
  index, and none is bundled.
- **No async runtime integration.** Scans run on the polling thread; use the
  `*_sync` methods on a blocking pool for large collections.
- **Persistence is a whole-store snapshot**, written on demand. No incremental
  log, no automatic save, and metadata is limited to what JSON represents.
- **No hybrid (keyword + vector) search**, and no built-in embedding glue.
- **Chunking is heuristic** — character counts, not tokens; punctuation-based
  sentence detection.
- The free metric functions do not validate inputs; only the store does.
- Remote backends (Pinecone/Qdrant/pgvector) are not implemented — implement
  the `VectorStore` trait for them.

## See also

- [genai](genai.md) — embedding providers that produce the vectors stored here.
- [codec](codec.md) — the [`Value`](codec.md) type used for `Doc::metadata` and the JSON codec used for snapshots.
- [errutils](errutils.md) — error helpers `VectorStoreError` composes with.

---
**Related:** [genai](genai.md), [codec](codec.md).

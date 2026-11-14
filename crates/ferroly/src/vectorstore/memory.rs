//! [`MemoryStore`] — the in-memory, brute-force backend.

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BinaryHeap, HashMap};
use std::io::Write as _;
use std::ops::Bound;
use std::path::Path;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use ferroly::codec::{json, Decode, Encode, Value};

use super::{
    check_vector, cosine_with_norms, dot64, euclidean64, norm64, BoxFuture, Doc, DocPage, Filter,
    Hit, ListRequest, Metric, Query, VectorStore, VectorStoreError,
};

/// The namespace a [`MemoryStore::new`] handle is bound to.
const DEFAULT_NAMESPACE: &str = "default";

/// Identifies the snapshot file format.
const SNAPSHOT_FORMAT: &str = "ferroly.vectorstore.memory";
/// The snapshot layout version this build reads and writes.
const SNAPSHOT_VERSION: u32 = 1;

/// An in-memory [`VectorStore`] — exact, brute-force search. Suitable for
/// tests, prototypes and collections small enough to scan (every search reads
/// every vector in the namespace: O(n · dim)).
///
/// # Namespaces
///
/// A store holds any number of **namespaces**, each a separate collection
/// with its own documents and its own vector dimension. A handle is bound to
/// exactly one: [`MemoryStore::new`] to `"default"`, and
/// [`namespace`](MemoryStore::namespace) returns another handle on the same
/// underlying store bound to a different one. Every operation through a
/// handle — search, upsert, delete, count, list — sees only that namespace,
/// so handing a tenant its own handle isolates it by construction.
///
/// Cloning a handle is cheap and shares the data.
///
/// # Blocking work
///
/// The async trait methods do their work inside the future's first poll, with
/// no `.await` inside. That is fine for small collections; for a large one a
/// search would hold an executor thread for the whole scan. Each operation
/// therefore also exists as a plain function (`search_sync`, `upsert_sync`,
/// …) to run on a blocking thread — the handle is `Clone + Send + 'static`:
///
/// ```no_run
/// # use ferroly::vectorstore::{MemoryStore, Query};
/// # async fn demo(store: MemoryStore, query: Query) -> Result<(), Box<dyn std::error::Error>> {
/// // With tokio; any runtime's blocking pool works the same way.
/// let handle = store.clone();
/// let hits = tokio::task::spawn_blocking(move || handle.search_sync(&query)).await??;
/// # let _ = hits;
/// # Ok(())
/// # }
/// ```
///
/// # Persistence
///
/// [`save`](MemoryStore::save) writes every namespace to one JSON file,
/// atomically; [`load`](MemoryStore::load) reads it back. Nothing is saved
/// automatically.
///
/// # Robustness
///
/// A panic in another thread cannot leave a handle unusable: lock poisoning
/// is recovered from (writes validate first and then apply, so the data is
/// consistent whenever the lock is released).
#[derive(Clone)]
pub struct MemoryStore {
    shared: Arc<RwLock<Collections>>,
    namespace: Arc<str>,
}

type Collections = HashMap<String, Collection>;

#[derive(Default)]
struct Collection {
    /// Ordered by id, which gives `list` a stable order and a cheap cursor.
    docs: BTreeMap<String, Stored>,
    /// The dimension every vector here has, set by the first upsert.
    dim: Option<usize>,
}

struct Stored {
    doc: Doc,
    /// The vector's L2 norm, computed once so cosine search does not redo it.
    norm: f64,
}

impl Stored {
    fn new(doc: Doc) -> Stored {
        let norm = norm64(&doc.vector);
        Stored { doc, norm }
    }
}

/// A search candidate, ordered so that "greater" means "better": higher
/// score first, and — for equal scores — the smaller id, so results do not
/// depend on iteration order.
struct Ranked<'a> {
    score: f64,
    doc: &'a Doc,
}

impl Ord for Ranked<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .total_cmp(&other.score)
            .then_with(|| other.doc.id.cmp(&self.doc.id))
    }
}

impl PartialOrd for Ranked<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Ranked<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Ranked<'_> {}

fn matches(filter: Option<&Filter>, doc: &Doc) -> bool {
    filter.map_or(true, |f| f.matches(&doc.metadata))
}

impl MemoryStore {
    /// Creates an empty store; the handle is bound to the `"default"`
    /// namespace.
    pub fn new() -> Self {
        Self {
            shared: Arc::new(RwLock::new(HashMap::new())),
            namespace: Arc::from(DEFAULT_NAMESPACE),
        }
    }

    /// Returns a handle on the same store bound to namespace `name`.
    ///
    /// Namespaces are created on first write and are fully separate: each has
    /// its own documents and its own dimension, and nothing done through one
    /// handle is visible through a handle on another.
    ///
    /// ```
    /// use ferroly::vectorstore::{Doc, MemoryStore};
    ///
    /// let store = MemoryStore::new();
    /// let acme = store.namespace("tenant-acme");
    /// let globex = store.namespace("tenant-globex");
    /// acme.upsert_sync(vec![Doc::new("a", vec![1.0, 0.0])]).unwrap();
    /// // A different dimension is fine: it is a different collection.
    /// globex.upsert_sync(vec![Doc::new("a", vec![1.0, 0.0, 0.0])]).unwrap();
    /// assert_eq!(acme.len(), 1);
    /// assert_eq!(store.len(), 0); // the default namespace is untouched
    /// ```
    pub fn namespace(&self, name: &str) -> MemoryStore {
        MemoryStore {
            shared: self.shared.clone(),
            namespace: Arc::from(name),
        }
    }

    /// The namespace this handle is bound to.
    pub fn namespace_name(&self) -> &str {
        &self.namespace
    }

    /// The names of all namespaces that currently hold data, sorted.
    pub fn namespaces(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .read()
            .iter()
            .filter(|(_, c)| !c.docs.is_empty())
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names
    }

    /// Removes namespace `name` and everything in it. Returns whether it
    /// existed.
    pub fn drop_namespace(&self, name: &str) -> bool {
        self.write().remove(name).is_some()
    }

    /// Removes every document in this handle's namespace and forgets its
    /// dimension, so the next upsert may establish a new one.
    pub fn clear(&self) {
        self.write().remove(&*self.namespace);
    }

    /// The number of documents in this handle's namespace.
    pub fn len(&self) -> usize {
        self.read()
            .get(&*self.namespace)
            .map_or(0, |c| c.docs.len())
    }

    /// Whether this handle's namespace is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The vector dimension of this handle's namespace, once an upsert has
    /// established it. It stays fixed until [`clear`](MemoryStore::clear),
    /// even if every document is deleted.
    pub fn dimension(&self) -> Option<usize> {
        self.read().get(&*self.namespace).and_then(|c| c.dim)
    }

    // A poisoned lock means another thread panicked while holding it. Writers
    // below validate everything before touching the map and then only perform
    // infallible inserts and removals, so the data is consistent at every
    // point a panic could unwind from; recovering the guard is sound and
    // keeps one failed caller from taking the store down for everyone.
    fn read(&self) -> RwLockReadGuard<'_, Collections> {
        self.shared.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Collections> {
        self.shared.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Synchronous [`VectorStore::upsert`]: inserts or replaces `docs`.
    ///
    /// All-or-nothing. Every vector is checked — non-empty, finite, and of
    /// one common dimension that matches the namespace's — **before**
    /// anything is stored; on error the namespace, including its dimension,
    /// is exactly as it was. If two documents in the batch share an id, the
    /// later one wins.
    pub fn upsert_sync(&self, docs: Vec<Doc>) -> Result<(), VectorStoreError> {
        for doc in &docs {
            check_vector(&format!("document `{}`", doc.id), &doc.vector)?;
        }
        let Some(first) = docs.first() else {
            return Ok(());
        };
        let mut collections = self.write();
        let established = collections.get(&*self.namespace).and_then(|c| c.dim);
        let expected = established.unwrap_or(first.vector.len());
        if let Some(odd) = docs.iter().find(|d| d.vector.len() != expected) {
            return Err(VectorStoreError::DimMismatch {
                expected,
                got: odd.vector.len(),
            });
        }
        // Validation is complete; from here on nothing can fail.
        let collection = collections.entry(self.namespace.to_string()).or_default();
        collection.dim = Some(expected);
        for doc in docs {
            collection.docs.insert(doc.id.clone(), Stored::new(doc));
        }
        Ok(())
    }

    /// Synchronous [`VectorStore::search`]: the `top_k` best matches, best
    /// first.
    ///
    /// Scores are computed against borrowed vectors and only the hits that
    /// are returned are cloned; a bounded heap keeps memory at O(`top_k`)
    /// however large the namespace is. Equal scores are ordered by id.
    ///
    /// Fails if the query vector is empty or has a non-finite component, if
    /// `min_score` is NaN, or if the vector's dimension differs from the
    /// namespace's. An empty namespace yields no hits.
    pub fn search_sync(&self, query: &Query) -> Result<Vec<Hit>, VectorStoreError> {
        check_vector("query", &query.vector)?;
        if query.min_score.is_some_and(f32::is_nan) {
            return Err(VectorStoreError::InvalidInput(
                "query: min_score is NaN".to_string(),
            ));
        }
        let collections = self.read();
        let Some(collection) = collections.get(&*self.namespace) else {
            return Ok(Vec::new());
        };
        if let Some(dim) = collection.dim {
            if query.vector.len() != dim {
                return Err(VectorStoreError::DimMismatch {
                    expected: dim,
                    got: query.vector.len(),
                });
            }
        }
        if query.top_k == 0 {
            return Ok(Vec::new());
        }

        let query_norm = norm64(&query.vector);
        let min_score = query.min_score.map(f64::from);
        let filter = query.filter.as_ref();
        // A min-heap of the best candidates so far: its top is the weakest
        // one kept, which is the one to beat (and to evict).
        let mut best: BinaryHeap<Reverse<Ranked<'_>>> =
            BinaryHeap::with_capacity(query.top_k.min(collection.docs.len()) + 1);
        for stored in collection.docs.values() {
            if !matches(filter, &stored.doc) {
                continue;
            }
            let score = match query.metric {
                Metric::Cosine => cosine_with_norms(
                    dot64(&query.vector, &stored.doc.vector),
                    query_norm,
                    stored.norm,
                ),
                Metric::Dot => dot64(&query.vector, &stored.doc.vector),
                Metric::Euclidean => -euclidean64(&query.vector, &stored.doc.vector),
            };
            // Finite inputs cannot produce NaN in f64, but if one ever
            // appeared it must rank last rather than first.
            let score = if score.is_nan() {
                f64::NEG_INFINITY
            } else {
                score
            };
            if min_score.is_some_and(|min| score < min) {
                continue;
            }
            let candidate = Ranked {
                score,
                doc: &stored.doc,
            };
            if best.len() < query.top_k {
                best.push(Reverse(candidate));
            } else if best.peek().is_some_and(|weakest| candidate > weakest.0) {
                best.pop();
                best.push(Reverse(candidate));
            }
        }
        // Ascending `Reverse` order is descending rank: best first.
        Ok(best
            .into_sorted_vec()
            .into_iter()
            .map(|Reverse(ranked)| Hit {
                id: ranked.doc.id.clone(),
                score: ranked.score as f32,
                doc: ranked.doc.clone(),
            })
            .collect())
    }

    /// Synchronous [`VectorStore::delete`]: removes documents by id, skipping
    /// unknown ones.
    pub fn delete_sync(&self, ids: &[String]) {
        let mut collections = self.write();
        if let Some(collection) = collections.get_mut(&*self.namespace) {
            for id in ids {
                collection.docs.remove(id);
            }
        }
    }

    /// Synchronous [`VectorStore::get`]: the documents with these ids, in the
    /// order asked, skipping unknown ones.
    pub fn get_sync(&self, ids: &[String]) -> Vec<Doc> {
        let collections = self.read();
        let Some(collection) = collections.get(&*self.namespace) else {
            return Vec::new();
        };
        ids.iter()
            .filter_map(|id| collection.docs.get(id))
            .map(|stored| stored.doc.clone())
            .collect()
    }

    /// Synchronous [`VectorStore::count`]: how many documents match `filter`
    /// (all of them for `None`).
    pub fn count_sync(&self, filter: Option<&Filter>) -> usize {
        let collections = self.read();
        let Some(collection) = collections.get(&*self.namespace) else {
            return 0;
        };
        match filter {
            None => collection.docs.len(),
            Some(_) => collection
                .docs
                .values()
                .filter(|stored| matches(filter, &stored.doc))
                .count(),
        }
    }

    /// Synchronous [`VectorStore::delete_where`]: removes every document
    /// matching `filter` and returns how many were removed.
    pub fn delete_where_sync(&self, filter: &Filter) -> usize {
        let mut collections = self.write();
        let Some(collection) = collections.get_mut(&*self.namespace) else {
            return 0;
        };
        let before = collection.docs.len();
        collection
            .docs
            .retain(|_, stored| !filter.matches(&stored.doc.metadata));
        before - collection.docs.len()
    }

    /// Synchronous [`VectorStore::list`]: one page of documents in id order.
    ///
    /// Paging is by id, not a snapshot: documents added or removed between
    /// calls appear or not according to where their ids sort.
    pub fn list_sync(&self, request: &ListRequest) -> DocPage {
        let collections = self.read();
        let Some(collection) = collections.get(&*self.namespace) else {
            return DocPage::default();
        };
        let limit = request.limit.max(1);
        let lower = match &request.after {
            Some(id) => Bound::Excluded(id.as_str()),
            None => Bound::Unbounded,
        };
        let mut matching = collection
            .docs
            .range::<str, _>((lower, Bound::Unbounded))
            .map(|(_, stored)| &stored.doc)
            .filter(|doc| matches(request.filter.as_ref(), doc));
        let docs: Vec<Doc> = matching.by_ref().take(limit).cloned().collect();
        // Only promise another page if a further match actually exists.
        let next = match (matching.next(), docs.last()) {
            (Some(_), Some(last)) => Some(last.id.clone()),
            _ => None,
        };
        DocPage { docs, next }
    }

    // ---- snapshots ----------------------------------------------------------

    /// Encodes every namespace of the store as a JSON snapshot string.
    ///
    /// The format is a plain JSON object (`format`, `version`, and a list of
    /// namespaces with their dimension and documents). Vectors survive
    /// exactly. Metadata survives as far as JSON can carry it: integers and
    /// floats, strings, booleans, null, arrays and objects are preserved,
    /// while a `Value::Bytes` comes back as an array of integers and a
    /// non-finite float as null.
    pub fn to_snapshot_json(&self) -> String {
        let collections = self.read();
        let mut names: Vec<&String> = collections.keys().collect();
        names.sort();
        let snapshot = Snapshot {
            format: SNAPSHOT_FORMAT.to_string(),
            version: SNAPSHOT_VERSION,
            namespaces: names
                .into_iter()
                .map(|name| {
                    let collection = &collections[name];
                    NamespaceSnapshot {
                        name: name.clone(),
                        dimension: collection.dim.map(|d| d as u64),
                        docs: collection
                            .docs
                            .values()
                            .map(|stored| DocSnapshot {
                                id: stored.doc.id.clone(),
                                vector: stored.doc.vector.clone(),
                                metadata: stored.doc.metadata.clone(),
                            })
                            .collect(),
                    }
                })
                .collect(),
        };
        json::encode(&snapshot)
    }

    /// Rebuilds a store from a snapshot produced by
    /// [`to_snapshot_json`](MemoryStore::to_snapshot_json). The returned
    /// handle is bound to the `"default"` namespace.
    ///
    /// The snapshot is validated like fresh input — every vector must be
    /// finite and match its namespace's dimension — so a damaged or
    /// hand-edited file is rejected rather than loaded into an inconsistent
    /// store.
    pub fn from_snapshot_json(text: &str) -> Result<MemoryStore, VectorStoreError> {
        let snapshot: Snapshot = json::decode(text)?;
        if snapshot.format != SNAPSHOT_FORMAT {
            return Err(VectorStoreError::InvalidInput(format!(
                "snapshot: unrecognized format `{}`",
                snapshot.format
            )));
        }
        if snapshot.version != SNAPSHOT_VERSION {
            return Err(VectorStoreError::InvalidInput(format!(
                "snapshot: unsupported version {} (this build reads version {SNAPSHOT_VERSION})",
                snapshot.version
            )));
        }
        let mut collections = Collections::new();
        for namespace in snapshot.namespaces {
            let dim = match namespace.dimension {
                Some(d) => Some(usize::try_from(d).map_err(|_| {
                    VectorStoreError::InvalidInput(format!(
                        "snapshot: namespace `{}` has an impossible dimension",
                        namespace.name
                    ))
                })?),
                None => None,
            };
            let mut collection = Collection {
                docs: BTreeMap::new(),
                dim,
            };
            for doc in namespace.docs {
                check_vector(&format!("snapshot: document `{}`", doc.id), &doc.vector)?;
                let expected = *collection.dim.get_or_insert(doc.vector.len());
                if doc.vector.len() != expected {
                    return Err(VectorStoreError::DimMismatch {
                        expected,
                        got: doc.vector.len(),
                    });
                }
                let doc = Doc {
                    id: doc.id,
                    vector: doc.vector,
                    metadata: doc.metadata,
                };
                collection.docs.insert(doc.id.clone(), Stored::new(doc));
            }
            collections.insert(namespace.name, collection);
        }
        Ok(MemoryStore {
            shared: Arc::new(RwLock::new(collections)),
            namespace: Arc::from(DEFAULT_NAMESPACE),
        })
    }

    /// Writes a snapshot of every namespace to `path`, atomically: the data
    /// goes to a temporary file beside `path`, is flushed to disk, and is
    /// then renamed into place, so a crash never leaves a half-written
    /// snapshot and an existing one is replaced in a single step.
    ///
    /// This **blocks** on file I/O (and encodes the whole store in memory
    /// first); from async code, call it on a blocking thread. See
    /// [`to_snapshot_json`](MemoryStore::to_snapshot_json) for what the
    /// format preserves.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), VectorStoreError> {
        let text = self.to_snapshot_json();
        write_atomic(path.as_ref(), text.as_bytes())?;
        Ok(())
    }

    /// Reads a snapshot written by [`save`](MemoryStore::save). Blocks on
    /// file I/O. The returned handle is bound to the `"default"` namespace.
    pub fn load(path: impl AsRef<Path>) -> Result<MemoryStore, VectorStoreError> {
        let text = std::fs::read_to_string(path)?;
        MemoryStore::from_snapshot_json(&text)
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryStore")
            .field("namespace", &&*self.namespace)
            .field("len", &self.len())
            .field("dimension", &self.dimension())
            .finish()
    }
}

/// The on-disk snapshot layout (JSON, via the codec derives).
#[derive(Encode, Decode)]
struct Snapshot {
    format: String,
    version: u32,
    namespaces: Vec<NamespaceSnapshot>,
}

#[derive(Encode, Decode)]
struct NamespaceSnapshot {
    name: String,
    dimension: Option<u64>,
    docs: Vec<DocSnapshot>,
}

#[derive(Encode, Decode)]
struct DocSnapshot {
    id: String,
    vector: Vec<f32>,
    metadata: Value,
}

/// Writes `data` to a temporary sibling of `path`, flushes it, and renames it
/// over `path`.
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "snapshot path has no file name",
        )
    })?;
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(
        ".{}.{}-{nanos}-{}.tmp",
        name.to_string_lossy(),
        std::process::id(),
        COUNTER.fetch_add(1, AtomicOrdering::Relaxed)
    ));
    let written = (|| {
        // `create_new`: never write through something already at this name.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return written;
    }
    // Make the rename durable too. Best-effort: the data is already safe.
    #[cfg(unix)]
    if let Ok(handle) = std::fs::File::open(dir) {
        let _ = handle.sync_all();
    }
    Ok(())
}

impl VectorStore for MemoryStore {
    fn upsert(&self, docs: Vec<Doc>) -> BoxFuture<'_, Result<(), VectorStoreError>> {
        Box::pin(async move { self.upsert_sync(docs) })
    }

    fn search(&self, query: Query) -> BoxFuture<'_, Result<Vec<Hit>, VectorStoreError>> {
        Box::pin(async move { self.search_sync(&query) })
    }

    fn delete(&self, ids: Vec<String>) -> BoxFuture<'_, Result<(), VectorStoreError>> {
        Box::pin(async move {
            self.delete_sync(&ids);
            Ok(())
        })
    }

    fn get(&self, ids: Vec<String>) -> BoxFuture<'_, Result<Vec<Doc>, VectorStoreError>> {
        Box::pin(async move { Ok(self.get_sync(&ids)) })
    }

    fn count(&self, filter: Option<Filter>) -> BoxFuture<'_, Result<usize, VectorStoreError>> {
        Box::pin(async move { Ok(self.count_sync(filter.as_ref())) })
    }

    fn delete_where(&self, filter: Filter) -> BoxFuture<'_, Result<usize, VectorStoreError>> {
        Box::pin(async move { Ok(self.delete_where_sync(&filter)) })
    }

    fn list(&self, request: ListRequest) -> BoxFuture<'_, Result<DocPage, VectorStoreError>> {
        Box::pin(async move { Ok(self.list_sync(&request)) })
    }
}

#[cfg(test)]
#[path = "memory_tests.rs"]
mod tests;

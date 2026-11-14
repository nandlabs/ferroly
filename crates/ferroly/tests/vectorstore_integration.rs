#![cfg(feature = "vectorstore")]
//! `ferroly::vectorstore` through its public API: chunk a document, store the
//! chunks per tenant, search with filters, snapshot and restore.

use std::sync::Arc;

use ferroly::codec::Value;
use ferroly::vectorstore::{
    Boundary, Chunker, Doc, Filter, ListRequest, MemoryStore, Metric, Query, VectorStore,
    VectorStoreError,
};

/// A toy embedding: letter frequencies. Deterministic and good enough to make
/// "similar text ranks higher" true.
fn embed(text: &str) -> Vec<f32> {
    let mut v = vec![0.0f32; 26];
    for c in text.chars().filter(char::is_ascii_alphabetic) {
        v[(c.to_ascii_lowercase() as u8 - b'a') as usize] += 1.0;
    }
    v
}

fn meta(source: &str, index: usize, text: &str) -> Value {
    Value::Object(vec![
        ("source".into(), source.into()),
        ("chunk".into(), index.into()),
        ("text".into(), text.into()),
    ])
}

const HANDBOOK: &str = "Zebras graze on the savanna. Zebras have striped coats.\n\n\
    Penguins swim in cold seas. Penguins huddle for warmth.\n\n\
    Compilers translate source code. Compilers optimize loops.";

async fn index(store: &dyn VectorStore, source: &str, text: &str) -> usize {
    let chunks = Chunker::new(60).boundary(Boundary::Paragraph).chunk(text);
    let docs: Vec<Doc> = chunks
        .iter()
        .map(|c| {
            Doc::new(format!("{source}#{}", c.index), embed(c.text))
                .with_metadata(meta(source, c.index, c.text))
        })
        .collect();
    let count = docs.len();
    store.upsert(docs).await.unwrap();
    count
}

#[tokio::test]
async fn chunk_store_search_snapshot_restore() {
    let root = MemoryStore::new();
    let acme: Arc<dyn VectorStore> = Arc::new(root.namespace("acme"));
    let globex: Arc<dyn VectorStore> = Arc::new(root.namespace("globex"));

    assert_eq!(index(acme.as_ref(), "handbook", HANDBOOK).await, 3);
    index(
        acme.as_ref(),
        "memo",
        "Quarterly budget review. Budget is tight.",
    )
    .await;
    index(
        globex.as_ref(),
        "handbook",
        "Globex secret roadmap. Do not share.",
    )
    .await;

    // Nearest chunk to a question, restricted to one source.
    let question = embed("which animals swim in the cold sea");
    let hits = acme
        .search(Query::new(question.clone(), 1).filter(Filter::eq("source", "handbook")))
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    let text = hits[0].doc.metadata.get("text").unwrap().as_str().unwrap();
    assert!(text.starts_with("Penguins"), "got: {text}");
    assert!(hits[0].score > 0.5 && hits[0].score <= 1.0);
    assert!(Metric::Cosine.to_unit_interval(hits[0].score).unwrap() > 0.75);

    // A threshold that only near-identical text passes.
    let strict = acme
        .search(Query::new(question.clone(), 10).min_score(0.999))
        .await
        .unwrap();
    assert!(strict.is_empty());

    // Tenants never see each other's documents.
    let all_acme = acme
        .search(Query::new(question.clone(), 100))
        .await
        .unwrap();
    assert_eq!(all_acme.len(), 4);
    assert!(all_acme.iter().all(|h| !h
        .doc
        .metadata
        .get("text")
        .unwrap()
        .as_str()
        .unwrap()
        .contains("Globex")));
    assert_eq!(globex.count(None).await.unwrap(), 1);
    assert_eq!(root.count(None).await.unwrap(), 0);

    // Page through one source, then drop it.
    let page = acme
        .list(ListRequest::new(2).filter(Filter::eq("source", "handbook")))
        .await
        .unwrap();
    assert_eq!(page.docs.len(), 2);
    let rest = acme
        .list(
            ListRequest::new(2)
                .filter(Filter::eq("source", "handbook"))
                .after(page.next.unwrap()),
        )
        .await
        .unwrap();
    assert_eq!(rest.docs.len(), 1);
    assert_eq!(rest.next, None);
    assert_eq!(
        acme.get(vec!["memo#0".into()]).await.unwrap()[0].id,
        "memo#0"
    );

    // Snapshot to disk and restore into a fresh store.
    let dir = std::env::temp_dir().join(format!(
        "ferroly-vectorstore-it-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("index.json");
    root.save(&path).unwrap();
    let restored = MemoryStore::load(&path).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();

    assert_eq!(restored.namespaces(), ["acme", "globex"]);
    let again = restored
        .namespace("acme")
        .search(Query::new(question, 1).filter(Filter::eq("source", "handbook")))
        .await
        .unwrap();
    assert_eq!(again[0].id, hits[0].id);
    assert_eq!(again[0].score, hits[0].score);

    assert_eq!(
        acme.delete_where(Filter::eq("source", "handbook"))
            .await
            .unwrap(),
        3
    );
    assert_eq!(acme.count(None).await.unwrap(), 1);
    // The restored copy is independent.
    assert_eq!(restored.namespace("acme").len(), 4);
}

#[tokio::test]
async fn bad_input_is_an_error_not_a_panic() {
    let store = MemoryStore::new();
    let mixed = store
        .upsert(vec![
            Doc::new("a", vec![1.0, 0.0]),
            Doc::new("b", vec![1.0, 0.0, 0.0]),
        ])
        .await;
    assert!(matches!(mixed, Err(VectorStoreError::DimMismatch { .. })));
    assert!(store.is_empty());
    assert_eq!(store.dimension(), None);

    store
        .upsert(vec![Doc::new("b", vec![1.0, 0.0, 0.0])])
        .await
        .unwrap();
    assert!(matches!(
        store.search(Query::new(vec![f32::NAN, 0.0, 0.0], 1)).await,
        Err(VectorStoreError::InvalidInput(_))
    ));
    assert!(matches!(
        store
            .upsert(vec![Doc::new("c", vec![f32::INFINITY, 0.0, 0.0])])
            .await,
        Err(VectorStoreError::InvalidInput(_))
    ));

    // The error converts into the crate-level error type.
    fn as_crate_error(e: VectorStoreError) -> ferroly::Error {
        e.into()
    }
    let err = store.search(Query::new(vec![1.0], 1)).await.unwrap_err();
    assert!(matches!(
        as_crate_error(err),
        ferroly::Error::VectorStore(VectorStoreError::DimMismatch {
            expected: 3,
            got: 1
        })
    ));
}

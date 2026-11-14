//! Tests for [`MemoryStore`].

use super::*;
use std::path::PathBuf;

fn meta(fields: Vec<(&str, Value)>) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

fn ids(hits: &[Hit]) -> Vec<&str> {
    hits.iter().map(|h| h.id.as_str()).collect()
}

/// A deterministic pseudo-random stream in `[-1, 1)` (no `rand` dependency).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }

    fn vector(&mut self, dim: usize) -> Vec<f32> {
        (0..dim).map(|_| self.next()).collect()
    }
}

/// A unique scratch directory, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "ferroly-vectorstore-{tag}-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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
    let err = store.search(Query::new(vec![1.0], 1)).await;
    assert!(matches!(
        err,
        Err(VectorStoreError::DimMismatch {
            expected: 2,
            got: 1
        })
    ));
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
    assert_eq!(hits[0].score, 0.0);
    // Dot: the longer vector wins.
    let hits = store
        .search(Query::new(vec![1.0, 1.0], 2).metric(Metric::Dot))
        .await
        .unwrap();
    assert_eq!(ids(&hits), ["far", "near"]);
    assert_eq!(hits[0].score, 10.0);
}

/// A failed first upsert must not fix the store's dimension.
#[test]
fn failed_mixed_batch_leaves_the_store_untouched() {
    let store = MemoryStore::new();
    let err = store.upsert_sync(vec![
        Doc::new("a", vec![1.0, 2.0]),
        Doc::new("b", vec![1.0, 2.0, 3.0]),
    ]);
    assert!(matches!(
        err,
        Err(VectorStoreError::DimMismatch {
            expected: 2,
            got: 3
        })
    ));
    assert_eq!(store.len(), 0);
    assert_eq!(store.dimension(), None, "dimension must not be fixed");
    assert!(store.namespaces().is_empty());

    // The store accepts either dimension afterwards — here, the "other" one.
    store
        .upsert_sync(vec![Doc::new("b", vec![1.0, 2.0, 3.0])])
        .unwrap();
    assert_eq!(store.dimension(), Some(3));
}

#[test]
fn failed_batch_on_a_populated_store_changes_nothing() {
    let store = MemoryStore::new();
    store
        .upsert_sync(vec![Doc::new("keep", vec![1.0, 0.0])])
        .unwrap();

    // Bad document last: the good ones before it must not be stored.
    for bad in [
        Doc::new("bad", vec![1.0, 2.0, 3.0]), // wrong dimension
        Doc::new("bad", vec![f32::NAN, 0.0]),
        Doc::new("bad", vec![0.0, f32::INFINITY]),
        Doc::new("bad", vec![]),
    ] {
        let result = store.upsert_sync(vec![
            Doc::new("new", vec![0.0, 1.0]),
            Doc::new("keep", vec![9.0, 9.0]), // would overwrite
            bad,
        ]);
        assert!(result.is_err());
        assert_eq!(store.len(), 1);
        assert_eq!(store.get_sync(&["keep".into()])[0].vector, [1.0, 0.0]);
        assert!(store.get_sync(&["new".into()]).is_empty());
    }
    assert_eq!(store.dimension(), Some(2));

    // An empty batch is a no-op and establishes nothing.
    let empty = MemoryStore::new();
    empty.upsert_sync(Vec::new()).unwrap();
    assert_eq!(empty.dimension(), None);
}

#[test]
fn non_finite_and_empty_vectors_are_rejected() {
    let store = MemoryStore::new();
    for vector in [
        vec![f32::NAN, 1.0],
        vec![1.0, f32::INFINITY],
        vec![f32::NEG_INFINITY, 1.0],
        vec![],
    ] {
        assert!(matches!(
            store.upsert_sync(vec![Doc::new("x", vector.clone())]),
            Err(VectorStoreError::InvalidInput(_))
        ));
        // Rejected at search too — even against an empty store.
        assert!(matches!(
            store.search_sync(&Query::new(vector, 3)),
            Err(VectorStoreError::InvalidInput(_))
        ));
    }
    assert_eq!(store.dimension(), None);

    store
        .upsert_sync(vec![Doc::new("a", vec![1.0, 0.0])])
        .unwrap();
    assert!(matches!(
        store.search_sync(&Query::new(vec![1.0, 0.0], 1).min_score(f32::NAN)),
        Err(VectorStoreError::InvalidInput(_))
    ));
    let message = store
        .upsert_sync(vec![Doc::new("doc-7", vec![0.0, f32::NAN])])
        .unwrap_err()
        .to_string();
    assert!(message.contains("doc-7") && message.contains("component 1"));
}

#[test]
fn zero_vectors_score_zero_under_cosine() {
    let store = MemoryStore::new();
    store
        .upsert_sync(vec![
            Doc::new("zero", vec![0.0, 0.0]),
            Doc::new("up", vec![0.0, 1.0]),
            Doc::new("down", vec![0.0, -1.0]),
        ])
        .unwrap();
    let hits = store.search_sync(&Query::new(vec![0.0, 2.0], 3)).unwrap();
    assert_eq!(ids(&hits), ["up", "zero", "down"]);
    assert_eq!(hits[1].score, 0.0);

    // A zero query scores everything 0; ties are broken by id.
    let hits = store.search_sync(&Query::new(vec![0.0, 0.0], 3)).unwrap();
    assert_eq!(ids(&hits), ["down", "up", "zero"]);
    assert!(hits.iter().all(|h| h.score == 0.0));
}

#[test]
fn extreme_magnitudes_rank_correctly_under_cosine() {
    let store = MemoryStore::new();
    store
        .upsert_sync(vec![
            Doc::new("huge-aligned", vec![3.0e38, 3.0e38]),
            Doc::new("huge-orthogonal", vec![3.0e38, -3.0e38]),
            Doc::new("tiny-aligned", vec![1.0e-30, 1.0e-30]),
            Doc::new("tiny-opposite", vec![-1.0e-30, -1.0e-30]),
        ])
        .unwrap();
    let hits = store.search_sync(&Query::new(vec![1.0, 1.0], 4)).unwrap();
    assert!(hits.iter().all(|h| h.score.is_finite()));
    assert!((hits[0].score - 1.0).abs() < 1e-6);
    assert!((hits[1].score - 1.0).abs() < 1e-6);
    assert_eq!(hits[2].id, "huge-orthogonal");
    assert!(hits[2].score.abs() < 1e-6);
    assert_eq!(hits[3].id, "tiny-opposite");
    assert!((hits[3].score + 1.0).abs() < 1e-6);

    // Same with a huge query vector.
    let hits = store
        .search_sync(&Query::new(vec![3.0e38, 3.0e38], 4))
        .unwrap();
    assert!((hits[0].score - 1.0).abs() < 1e-6);
    assert_eq!(hits[3].id, "tiny-opposite");
}

/// The bounded heap must return exactly what a full sort would.
#[test]
fn top_k_matches_a_full_sort() {
    let mut rng = Lcg(7);
    let store = MemoryStore::new();
    let docs: Vec<Doc> = (0..500)
        .map(|i| Doc::new(format!("doc-{i:04}"), rng.vector(16)))
        .collect();
    store.upsert_sync(docs.clone()).unwrap();

    for metric in [Metric::Cosine, Metric::Dot, Metric::Euclidean] {
        let query = rng.vector(16);
        let mut expected: Vec<(f64, &str)> = docs
            .iter()
            .map(|d| {
                let score = match metric {
                    Metric::Cosine => cosine_with_norms(
                        dot64(&query, &d.vector),
                        norm64(&query),
                        norm64(&d.vector),
                    ),
                    Metric::Dot => dot64(&query, &d.vector),
                    Metric::Euclidean => -euclidean64(&query, &d.vector),
                };
                (score, d.id.as_str())
            })
            .collect();
        expected.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));

        for top_k in [1, 7, 100, 500, 10_000] {
            let hits = store
                .search_sync(&Query::new(query.clone(), top_k).metric(metric))
                .unwrap();
            assert_eq!(hits.len(), top_k.min(500));
            for (hit, want) in hits.iter().zip(&expected) {
                assert_eq!(hit.id, want.1, "{metric:?} top_k={top_k}");
                assert_eq!(hit.score, want.0 as f32);
                assert_eq!(hit.doc.id, hit.id);
            }
            assert!(hits.windows(2).all(|w| w[0].score >= w[1].score));
        }
    }
    assert!(store
        .search_sync(&Query::new(rng.vector(16), 0))
        .unwrap()
        .is_empty());
}

#[test]
fn ties_are_ordered_by_id_whatever_the_insertion_order() {
    let store = MemoryStore::new();
    for id in ["m", "c", "x", "a", "q"] {
        store
            .upsert_sync(vec![Doc::new(id, vec![1.0, 1.0])])
            .unwrap();
    }
    let hits = store.search_sync(&Query::new(vec![2.0, 2.0], 3)).unwrap();
    assert_eq!(ids(&hits), ["a", "c", "m"]);
    let hits = store.search_sync(&Query::new(vec![2.0, 2.0], 5)).unwrap();
    assert_eq!(ids(&hits), ["a", "c", "m", "q", "x"]);
}

#[test]
fn last_duplicate_id_in_a_batch_wins() {
    let store = MemoryStore::new();
    store
        .upsert_sync(vec![
            Doc::new("a", vec![1.0, 0.0]),
            Doc::new("a", vec![0.0, 1.0]),
        ])
        .unwrap();
    assert_eq!(store.len(), 1);
    assert_eq!(store.get_sync(&["a".into()])[0].vector, [0.0, 1.0]);
}

fn library() -> MemoryStore {
    let store = MemoryStore::new();
    store
        .upsert_sync(vec![
            Doc::new("en-1", vec![1.0, 0.0]).with_metadata(meta(vec![
                ("lang", "en".into()),
                ("year", 2021.into()),
                ("tags", Value::Array(vec!["intro".into()])),
            ])),
            Doc::new("en-2", vec![0.9, 0.1])
                .with_metadata(meta(vec![("lang", "en".into()), ("year", 2024.into())])),
            Doc::new("de-1", vec![0.8, 0.2])
                .with_metadata(meta(vec![("lang", "de".into()), ("year", 2023.into())])),
            Doc::new("fr-1", vec![0.0, 1.0])
                .with_metadata(meta(vec![("lang", "fr".into()), ("year", 2019.into())])),
            Doc::new("bare", vec![0.7, 0.3]),
        ])
        .unwrap();
    store
}

#[test]
fn search_applies_metadata_filters() {
    let store = library();
    let query = || Query::new(vec![1.0, 0.0], 10);

    let hits = store
        .search_sync(&query().filter(Filter::eq("lang", "en")))
        .unwrap();
    assert_eq!(ids(&hits), ["en-1", "en-2"]);

    let hits = store
        .search_sync(&query().filter(Filter::is_in("lang", ["de", "fr"])))
        .unwrap();
    assert_eq!(ids(&hits), ["de-1", "fr-1"]);

    let hits = store
        .search_sync(&query().filter(Filter::gte("year", 2023)))
        .unwrap();
    assert_eq!(ids(&hits), ["en-2", "de-1"]);

    let recent_not_english = Filter::gt("year", 2020).and(!Filter::eq("lang", "en"));
    let hits = store
        .search_sync(&query().filter(recent_not_english))
        .unwrap();
    assert_eq!(ids(&hits), ["de-1"]);

    let hits = store
        .search_sync(&query().filter(Filter::eq("tags", "intro").or(Filter::eq("lang", "fr"))))
        .unwrap();
    assert_eq!(ids(&hits), ["en-1", "fr-1"]);

    // Documents without metadata only match negative conditions.
    let hits = store
        .search_sync(&query().filter(Filter::exists("lang").negate()))
        .unwrap();
    assert_eq!(ids(&hits), ["bare"]);

    // The filter applies before top-k, not after.
    let hits = store
        .search_sync(&Query::new(vec![1.0, 0.0], 1).filter(Filter::eq("lang", "fr")))
        .unwrap();
    assert_eq!(ids(&hits), ["fr-1"]);

    let hits = store
        .search_sync(&query().filter(Filter::eq("lang", "ja")))
        .unwrap();
    assert!(hits.is_empty());
}

#[test]
fn min_score_drops_weak_hits() {
    let store = library();
    let hits = store
        .search_sync(&Query::new(vec![1.0, 0.0], 10).min_score(0.95))
        .unwrap();
    assert_eq!(ids(&hits), ["en-1", "en-2", "de-1"]);
    assert!(hits.iter().all(|h| h.score >= 0.95));

    // With a filter as well.
    let hits = store
        .search_sync(
            &Query::new(vec![1.0, 0.0], 10)
                .min_score(0.95)
                .filter(Filter::ne("lang", "en")),
        )
        .unwrap();
    assert_eq!(ids(&hits), ["de-1"]);

    // Euclidean scores are negated distances: keep everything within 0.2.
    let hits = store
        .search_sync(
            &Query::new(vec![1.0, 0.0], 10)
                .metric(Metric::Euclidean)
                .min_score(-0.2),
        )
        .unwrap();
    assert_eq!(ids(&hits), ["en-1", "en-2"]);

    // An unreachable threshold returns nothing; -inf keeps everything.
    assert!(store
        .search_sync(&Query::new(vec![1.0, 0.0], 10).min_score(1.5))
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .search_sync(&Query::new(vec![1.0, 0.0], 10).min_score(f32::NEG_INFINITY))
            .unwrap()
            .len(),
        5
    );
}

#[test]
fn get_count_delete_where() {
    let store = library();
    let found = store.get_sync(&["fr-1".into(), "missing".into(), "bare".into()]);
    assert_eq!(
        found.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
        ["fr-1", "bare"]
    );
    assert_eq!(found[0].vector, [0.0, 1.0]);
    assert!(found[1].metadata.is_null());

    assert_eq!(store.count_sync(None), 5);
    assert_eq!(store.count_sync(Some(&Filter::eq("lang", "en"))), 2);
    assert_eq!(store.count_sync(Some(&Filter::lt("year", 2000))), 0);

    assert_eq!(store.delete_where_sync(&Filter::lt("year", 2022)), 2);
    assert_eq!(store.len(), 3);
    assert!(store.get_sync(&["en-1".into(), "fr-1".into()]).is_empty());
    assert_eq!(store.delete_where_sync(&Filter::lt("year", 2022)), 0);
    // The dimension outlives the documents.
    assert_eq!(store.delete_where_sync(&Filter::all([])), 3);
    assert!(store.is_empty());
    assert_eq!(store.dimension(), Some(2));
    assert!(store
        .upsert_sync(vec![Doc::new("x", vec![1.0, 2.0, 3.0])])
        .is_err());
    store.clear();
    assert_eq!(store.dimension(), None);
    store
        .upsert_sync(vec![Doc::new("x", vec![1.0, 2.0, 3.0])])
        .unwrap();
}

#[test]
fn list_pages_through_everything_in_id_order() {
    let store = MemoryStore::new();
    let docs: Vec<Doc> = (0..23)
        .map(|i| {
            Doc::new(format!("doc-{i:02}"), vec![i as f32, 1.0])
                .with_metadata(meta(vec![("even", (i % 2 == 0).into())]))
        })
        .collect();
    store.upsert_sync(docs).unwrap();

    let mut seen = Vec::new();
    let mut request = ListRequest::new(5);
    let mut pages = 0;
    loop {
        let page = store.list_sync(&request);
        assert!(page.docs.len() <= 5);
        pages += 1;
        seen.extend(page.docs.iter().map(|d| d.id.clone()));
        match page.next {
            Some(next) => request = ListRequest::new(5).after(next),
            None => break,
        }
    }
    let expected: Vec<String> = (0..23).map(|i| format!("doc-{i:02}")).collect();
    assert_eq!(seen, expected);
    assert_eq!(pages, 5);

    // With a filter: 12 even documents, 4 per page — and no phantom page.
    let mut seen = Vec::new();
    let mut after: Option<String> = None;
    let mut pages = 0;
    loop {
        let mut request = ListRequest::new(4).filter(Filter::eq("even", true));
        if let Some(id) = &after {
            request = request.after(id.clone());
        }
        let page = store.list_sync(&request);
        pages += 1;
        seen.extend(page.docs.iter().map(|d| d.id.clone()));
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert_eq!(seen.len(), 12);
    assert!(seen
        .iter()
        .all(|id| id[4..].parse::<u32>().unwrap() % 2 == 0));
    assert_eq!(pages, 3);

    // Edges: zero limit is treated as one; a cursor past the end is empty.
    let page = store.list_sync(&ListRequest::new(0));
    assert_eq!(page.docs.len(), 1);
    assert_eq!(page.next.as_deref(), Some("doc-00"));
    assert_eq!(
        store.list_sync(&ListRequest::new(5).after("zzz")),
        DocPage::default()
    );
    assert_eq!(
        MemoryStore::new().list_sync(&ListRequest::new(5)),
        DocPage::default()
    );
}

#[test]
fn namespaces_isolate_tenants() {
    let store = MemoryStore::new();
    let acme = store.namespace("acme");
    let globex = store.namespace("globex");
    assert_eq!(store.namespace_name(), "default");
    assert_eq!(acme.namespace_name(), "acme");

    acme.upsert_sync(vec![
        Doc::new("shared-id", vec![1.0, 0.0]).with_metadata(meta(vec![("owner", "acme".into())])),
        Doc::new("acme-only", vec![0.0, 1.0]),
    ])
    .unwrap();
    // The same id, and a different dimension, in another namespace.
    globex
        .upsert_sync(vec![Doc::new("shared-id", vec![0.0, 0.0, 1.0])
            .with_metadata(meta(vec![("owner", "globex".into())]))])
        .unwrap();

    assert_eq!((acme.len(), globex.len(), store.len()), (2, 1, 0));
    assert_eq!((acme.dimension(), globex.dimension()), (Some(2), Some(3)));
    assert_eq!(store.namespaces(), ["acme", "globex"]);

    // Searches, gets, counts and lists never cross the boundary.
    let hits = acme.search_sync(&Query::new(vec![1.0, 0.0], 10)).unwrap();
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|h| h
        .doc
        .metadata
        .get("owner")
        .map_or(true, |o| o.as_str() == Some("acme"))));
    let hits = globex
        .search_sync(&Query::new(vec![0.0, 0.0, 1.0], 10))
        .unwrap();
    assert_eq!(ids(&hits), ["shared-id"]);
    assert_eq!(
        hits[0].doc.metadata.get("owner").unwrap().as_str(),
        Some("globex")
    );
    assert!(globex.get_sync(&["acme-only".into()]).is_empty());
    assert_eq!(globex.count_sync(None), 1);
    assert_eq!(globex.list_sync(&ListRequest::new(10)).docs.len(), 1);
    assert!(store
        .search_sync(&Query::new(vec![1.0, 0.0], 10))
        .unwrap()
        .is_empty());
    // A query shaped for another tenant's vectors is an error here.
    assert!(globex.search_sync(&Query::new(vec![1.0, 0.0], 1)).is_err());

    // Deletes are scoped too.
    globex.delete_sync(&["shared-id".into()]);
    assert_eq!(acme.get_sync(&["shared-id".into()]).len(), 1);
    assert_eq!(globex.delete_where_sync(&Filter::all([])), 0);
    assert_eq!(acme.len(), 2);

    // A handle obtained again later sees the same data.
    assert_eq!(store.namespace("acme").len(), 2);
    assert!(store.drop_namespace("acme"));
    assert!(!store.drop_namespace("acme"));
    assert!(acme.is_empty());
    assert_eq!(acme.dimension(), None);
}

#[test]
fn snapshot_round_trips_everything() {
    let store = MemoryStore::new();
    let tricky = vec![
        0.1_f32,
        -0.0,
        f32::MAX,
        f32::MIN_POSITIVE,
        1.0e-45, // subnormal
        -123.456,
        16_777_217.0,
    ];
    store
        .upsert_sync(vec![
            Doc::new("plain", tricky.iter().map(|x| x * 0.5).collect()),
            Doc::new("tricky", tricky.clone()).with_metadata(meta(vec![
                ("title", "Ünïcödé \"quoted\" \\ and\nnewline".into()),
                ("year", 2024.into()),
                ("ratio", 0.25.into()),
                ("public", true.into()),
                ("nothing", Value::Null),
                ("tags", Value::Array(vec!["a".into(), "b".into()])),
                ("nested", meta(vec![("depth", 2.into())])),
            ])),
        ])
        .unwrap();
    let other = store.namespace("other tenant/β");
    other
        .upsert_sync(vec![Doc::new("o", vec![1.0, 2.0])])
        .unwrap();
    // A namespace emptied by deletes keeps its dimension across a snapshot.
    let emptied = store.namespace("emptied");
    emptied
        .upsert_sync(vec![Doc::new("gone", vec![1.0, 2.0, 3.0])])
        .unwrap();
    emptied.delete_sync(&["gone".into()]);

    let text = store.to_snapshot_json();
    let restored = MemoryStore::from_snapshot_json(&text).unwrap();
    assert_eq!(restored.namespace_name(), "default");
    assert_eq!(restored.len(), 2);
    assert_eq!(restored.dimension(), Some(tricky.len()));

    let before = store.get_sync(&["tricky".into()]).remove(0);
    let after = restored.get_sync(&["tricky".into()]).remove(0);
    // Bit-for-bit, including -0.0 and subnormals.
    let bits = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<u32>>();
    assert_eq!(bits(&after.vector), bits(&before.vector));
    assert_eq!(after.metadata, before.metadata);
    assert_eq!(
        restored.get_sync(&["plain".into()]),
        store.get_sync(&["plain".into()])
    );

    assert_eq!(restored.namespace("other tenant/β").len(), 1);
    assert_eq!(restored.namespace("emptied").len(), 0);
    assert_eq!(restored.namespace("emptied").dimension(), Some(3));
    assert_eq!(restored.namespaces(), ["default", "other tenant/β"]);

    // Searching the restored store gives the same answer.
    let query = Query::new(tricky.iter().map(|x| x.signum()).collect(), 2);
    let a = store.search_sync(&query).unwrap();
    let b = restored.search_sync(&query).unwrap();
    assert_eq!(ids(&a), ids(&b));
    assert_eq!(a[0].score, b[0].score);

    // A second snapshot of the restored store is identical text.
    assert_eq!(restored.to_snapshot_json(), text);
    // An empty store round-trips too.
    let empty = MemoryStore::from_snapshot_json(&MemoryStore::new().to_snapshot_json()).unwrap();
    assert!(empty.is_empty() && empty.namespaces().is_empty());
}

#[test]
fn save_is_atomic_and_load_restores() {
    let scratch = Scratch::new("save");
    let path = scratch.0.join("index.json");
    let store = library();
    store.save(&path).unwrap();
    assert_eq!(scratch.files(), ["index.json"], "no temporary left behind");

    let restored = MemoryStore::load(&path).unwrap();
    assert_eq!(restored.len(), 5);
    assert_eq!(
        ids(&restored
            .search_sync(&Query::new(vec![1.0, 0.0], 2).filter(Filter::eq("lang", "en")))
            .unwrap()),
        ["en-1", "en-2"]
    );

    // Saving again replaces the file in one step.
    store.delete_where_sync(&Filter::eq("lang", "en"));
    store.save(&path).unwrap();
    assert_eq!(scratch.files(), ["index.json"]);
    assert_eq!(MemoryStore::load(&path).unwrap().len(), 3);

    // A failed save leaves the previous snapshot intact.
    let missing_dir = scratch.0.join("no-such-dir").join("index.json");
    assert!(matches!(
        store.save(&missing_dir),
        Err(VectorStoreError::Io(_))
    ));
    assert_eq!(MemoryStore::load(&path).unwrap().len(), 3);
    assert!(matches!(
        MemoryStore::load(scratch.0.join("missing.json")),
        Err(VectorStoreError::Io(_))
    ));
}

#[test]
fn damaged_snapshots_are_rejected() {
    let good = library().to_snapshot_json();
    assert!(MemoryStore::from_snapshot_json(&good).is_ok());

    // Not JSON, or truncated.
    assert!(matches!(
        MemoryStore::from_snapshot_json("not json"),
        Err(VectorStoreError::Codec(_))
    ));
    assert!(matches!(
        MemoryStore::from_snapshot_json(&good[..good.len() / 2]),
        Err(VectorStoreError::Codec(_))
    ));
    // Someone else's JSON.
    assert!(MemoryStore::from_snapshot_json("{\"hello\":1}").is_err());

    let wrap = |namespaces: &str| {
        format!(
            "{{\"format\":\"{SNAPSHOT_FORMAT}\",\"version\":{SNAPSHOT_VERSION},\
             \"namespaces\":[{namespaces}]}}"
        )
    };
    let ok = wrap(
        "{\"name\":\"n\",\"dimension\":2,\"docs\":[{\"id\":\"a\",\"vector\":[1,2],\"metadata\":null}]}",
    );
    assert_eq!(
        MemoryStore::from_snapshot_json(&ok)
            .unwrap()
            .namespace("n")
            .len(),
        1
    );

    // Wrong format marker or a future version.
    let foreign = ok.replace(SNAPSHOT_FORMAT, "something.else");
    assert!(matches!(
        MemoryStore::from_snapshot_json(&foreign),
        Err(VectorStoreError::InvalidInput(m)) if m.contains("format")
    ));
    let future = ok.replace("\"version\":1", "\"version\":99");
    assert!(matches!(
        MemoryStore::from_snapshot_json(&future),
        Err(VectorStoreError::InvalidInput(m)) if m.contains("version")
    ));

    // A vector that contradicts the namespace's dimension.
    let mismatch = wrap(
        "{\"name\":\"n\",\"dimension\":2,\"docs\":[{\"id\":\"a\",\"vector\":[1,2,3],\"metadata\":null}]}",
    );
    assert!(matches!(
        MemoryStore::from_snapshot_json(&mismatch),
        Err(VectorStoreError::DimMismatch {
            expected: 2,
            got: 3
        })
    ));
    // Two documents that disagree with each other.
    let ragged = wrap(
        "{\"name\":\"n\",\"dimension\":null,\"docs\":[\
         {\"id\":\"a\",\"vector\":[1,2],\"metadata\":null},\
         {\"id\":\"b\",\"vector\":[1],\"metadata\":null}]}",
    );
    assert!(matches!(
        MemoryStore::from_snapshot_json(&ragged),
        Err(VectorStoreError::DimMismatch { .. })
    ));
    // An empty vector, and a component too large for f32 (it would load as
    // infinity).
    let empty_vector =
        wrap("{\"name\":\"n\",\"dimension\":null,\"docs\":[{\"id\":\"a\",\"vector\":[],\"metadata\":null}]}");
    assert!(matches!(
        MemoryStore::from_snapshot_json(&empty_vector),
        Err(VectorStoreError::InvalidInput(_))
    ));
    let overflow = wrap(
        "{\"name\":\"n\",\"dimension\":null,\"docs\":[{\"id\":\"a\",\"vector\":[1e300],\"metadata\":null}]}",
    );
    assert!(matches!(
        MemoryStore::from_snapshot_json(&overflow),
        Err(VectorStoreError::InvalidInput(_))
    ));
}

/// Lock poisoning must not panic callers.
#[test]
fn a_poisoned_lock_does_not_break_the_store() {
    let store = library();

    let writer = store.clone();
    let _ = std::thread::spawn(move || {
        let _guard = writer.shared.write().unwrap();
        panic!("poison the write lock");
    })
    .join();
    assert!(store.shared.is_poisoned());

    // Every operation still works, through this handle and others.
    assert_eq!(store.len(), 5);
    assert_eq!(store.dimension(), Some(2));
    assert_eq!(store.namespaces(), ["default"]);
    assert_eq!(
        store.search_sync(&Query::new(vec![1.0, 0.0], 1)).unwrap()[0].id,
        "en-1"
    );
    store
        .upsert_sync(vec![Doc::new("after", vec![0.5, 0.5])])
        .unwrap();
    assert_eq!(store.get_sync(&["after".into()]).len(), 1);
    assert_eq!(store.count_sync(Some(&Filter::eq("lang", "en"))), 2);
    assert_eq!(store.list_sync(&ListRequest::new(2)).docs.len(), 2);
    store.delete_sync(&["after".into()]);
    assert_eq!(store.delete_where_sync(&Filter::eq("lang", "fr")), 1);
    assert!(!store.to_snapshot_json().is_empty());
    let tenant = store.namespace("t");
    tenant.upsert_sync(vec![Doc::new("x", vec![1.0])]).unwrap();
    assert!(store.drop_namespace("t"));
    store.clear();
    assert!(store.is_empty());
}

#[tokio::test]
async fn trait_object_serves_the_extended_operations() {
    let store: Arc<dyn VectorStore> = Arc::new(library());

    let got = store.get(vec!["de-1".into(), "nope".into()]).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(store.count(None).await.unwrap(), 5);
    assert_eq!(
        store.count(Some(Filter::eq("lang", "en"))).await.unwrap(),
        2
    );

    let page = store.list(ListRequest::new(2)).await.unwrap();
    assert_eq!(page.docs.len(), 2);
    assert_eq!(page.next.as_deref(), Some("de-1"));
    let page = store
        .list(ListRequest::new(10).after("de-1"))
        .await
        .unwrap();
    assert_eq!(page.docs.len(), 3);
    assert_eq!(page.next, None);

    let hits = store
        .search(
            Query::new(vec![1.0, 0.0], 5)
                .filter(Filter::between("year", 2020, 2023))
                .min_score(0.5),
        )
        .await
        .unwrap();
    assert_eq!(ids(&hits), ["en-1", "de-1"]);

    assert_eq!(
        store.delete_where(Filter::eq("lang", "en")).await.unwrap(),
        2
    );
    assert_eq!(store.count(None).await.unwrap(), 3);
}

#[tokio::test]
async fn sync_methods_run_on_a_blocking_thread() {
    let store = library();
    let handle = store.clone();
    let hits =
        tokio::task::spawn_blocking(move || handle.search_sync(&Query::new(vec![1.0, 0.0], 2)))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(ids(&hits), ["en-1", "en-2"]);

    fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<MemoryStore>();
}

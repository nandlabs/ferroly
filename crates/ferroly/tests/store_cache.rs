#![cfg(feature = "store")]
//! The content-addressed cache: hit / miss, verification, size caps,
//! resumption (by HTTP range and after cancellation), concurrency, and
//! housekeeping. Every server is a loopback one.

mod store_common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferroly::http::{Client, HttpResponse, StatusCode};
use ferroly::store::{
    BoxFuture, ByteStream, Cache, Digest, Fetched, Layout, LocalStore, ObjectStatus, StoreError,
};
use store_common::{content, files_in, serve_range, spawn, Scratch};

/// Yields `data` in `piece`-byte chunks, then fails (or ends) after `limit`
/// bytes; optionally pauses before each chunk.
struct Pieces {
    data: Vec<u8>,
    pos: usize,
    piece: usize,
    fail_at: Option<usize>,
    pause: Option<Duration>,
    hang_at: Option<usize>,
}

impl Pieces {
    fn new(data: Vec<u8>, piece: usize) -> Self {
        Pieces {
            data,
            pos: 0,
            piece,
            fail_at: None,
            pause: None,
            hang_at: None,
        }
    }
}

impl ByteStream for Pieces {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, StoreError>> {
        Box::pin(async move {
            if let Some(p) = self.pause {
                tokio::time::sleep(p).await;
            }
            if self.hang_at.is_some_and(|h| self.pos >= h) {
                std::future::pending::<()>().await;
            }
            if self.fail_at.is_some_and(|f| self.pos >= f) {
                return Err(StoreError::Protocol("connection reset (simulated)".into()));
            }
            if self.pos >= self.data.len() {
                return Ok(None);
            }
            let mut end = (self.pos + self.piece).min(self.data.len());
            if let Some(f) = self.fail_at.or(self.hang_at) {
                end = end.min(f.max(self.pos + 1));
            }
            let chunk = self.data[self.pos..end].to_vec();
            self.pos = end;
            Ok(Some(chunk))
        })
    }
}

/// Parks a partial download of `data[..n]` for `digest` by failing a fetch
/// after `n` bytes.
async fn park_partial(cache: &Cache, digest: &Digest, data: &[u8], n: usize) {
    let data = data.to_vec();
    let err = cache
        .get_or_fetch(digest, move |_| {
            let mut s = Pieces::new(data.clone(), 100);
            s.fail_at = Some(n);
            async move { Ok(Fetched::new(s)) }
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Protocol(_)), "{err:?}");
    assert_eq!(cache.partial_len(digest), Some(n as u64));
}

/// A fetch callback result for paths that must not fetch.
fn must_not_fetch() -> Result<Fetched, StoreError> {
    panic!("the fetch callback was called")
}

/// A range-aware file server that records each request's `Range` header.
async fn range_server(
    body: Vec<u8>,
    ignore_range: bool,
) -> (String, Arc<Mutex<Vec<Option<String>>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    let base = spawn(move |req| {
        seen.lock()
            .unwrap()
            .push(req.headers.get("range").map(str::to_string));
        if req.uri.path != "/blob" {
            return HttpResponse::text(StatusCode::NOT_FOUND, "no such blob");
        }
        if ignore_range {
            HttpResponse::new(StatusCode::OK).body(body.clone())
        } else {
            serve_range(&req, &body)
        }
    })
    .await;
    (base, log)
}

#[tokio::test]
async fn miss_then_hit() {
    let dir = Scratch::new("hit");
    let cache = Cache::open(dir.path()).unwrap();
    let data = content(10_000);
    let digest = Digest::of(&data);
    assert!(cache.get(&digest).is_none());

    let calls = AtomicUsize::new(0);
    let fetch = |offset: u64| {
        calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(offset, 0);
        let data = data.clone();
        async move { Ok(Fetched::new(Pieces::new(data, 777))) }
    };
    let path = cache.get_or_fetch(&digest, fetch).await.unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), data);
    assert_eq!(path, cache.path_for(&digest));
    let hex = digest.hex();
    assert!(path.ends_with(std::path::Path::new("sha256").join(&hex[..2]).join(&hex)));

    // A hit never calls the fetcher.
    let again = cache
        .get_or_fetch(&digest, |_| async { must_not_fetch() })
        .await
        .unwrap();
    assert_eq!(again, path);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(cache.get(&digest), Some(path));
    assert_eq!(files_in(&dir.path().join("tmp")), 0);
}

#[tokio::test]
async fn mismatch_is_rejected_and_deleted() {
    let dir = Scratch::new("mismatch");
    let cache = Cache::open(dir.path()).unwrap();
    let digest = Digest::of(b"the real content");
    let err = cache
        .get_or_fetch(&digest, |_| async {
            Ok(Fetched::from_reader(&b"tampered content"[..]))
        })
        .await
        .unwrap_err();
    match err {
        StoreError::DigestMismatch { expected, actual } => {
            assert_eq!(expected, digest.to_string());
            assert_eq!(actual, Digest::of(b"tampered content").to_string());
        }
        other => panic!("{other:?}"),
    }
    assert!(cache.get(&digest).is_none());
    assert_eq!(
        cache.partial_len(&digest),
        None,
        "bad content must not be parked"
    );
    assert_eq!(files_in(&dir.path().join("tmp")), 0);
}

#[tokio::test]
async fn size_checks() {
    let dir = Scratch::new("size");
    let cache = Cache::open(dir.path()).unwrap();
    let data = content(5000);
    let digest = Digest::of(&data);

    // Declared size smaller than the stream: stopped and deleted.
    let err = cache
        .get_or_fetch_sized(&digest, 4000, |_| {
            let d = data.clone();
            async move { Ok(Fetched::new(Pieces::new(d, 1000))) }
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::SizeMismatch(_)), "{err:?}");

    // A source total that contradicts the expected size.
    let err = cache
        .get_or_fetch_sized(&digest, 4000, |_| {
            let d = data.clone();
            async move { Ok(Fetched::from_bytes(d)) }
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::SizeMismatch(_)), "{err:?}");

    // The cache-wide cap.
    let capped = Cache::open(dir.path())
        .unwrap()
        .with_max_object_bytes(Some(1024));
    let err = capped
        .get_or_fetch(&digest, |_| {
            let d = data.clone();
            async move { Ok(Fetched::new(Pieces::new(d, 100))) }
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::SizeMismatch(_)), "{err:?}");
    let err = capped
        .get_or_fetch_sized(&digest, 5000, |_| async { must_not_fetch() })
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::SizeMismatch(_)), "{err:?}");

    // The right size works.
    let path = cache
        .get_or_fetch_sized(&digest, 5000, |_| {
            let d = data.clone();
            async move { Ok(Fetched::new(Pieces::new(d, 1000))) }
        })
        .await
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), data);
    assert_eq!(files_in(&dir.path().join("tmp")), 0);
    assert_eq!(files_in(&dir.path().join("partial")), 0);
}

#[tokio::test]
async fn interrupted_download_resumes_with_a_range_request() {
    let dir = Scratch::new("resume");
    let cache = Cache::open(dir.path()).unwrap();
    let data = content(50_000);
    let digest = Digest::of(&data);
    park_partial(&cache, &digest, &data, 20_000).await;

    let (base, log) = range_server(data.clone(), false).await;
    let path = cache
        .fetch_url(&Client::new(), &format!("{base}/blob"), &digest)
        .await
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), data);
    assert_eq!(
        *log.lock().unwrap(),
        vec![Some("bytes=20000-".to_string())],
        "only the missing tail is requested"
    );
    assert_eq!(cache.partial_len(&digest), None);
    assert_eq!(files_in(&dir.path().join("tmp")), 0);
}

#[tokio::test]
async fn resume_when_the_server_ignores_ranges() {
    let dir = Scratch::new("norange");
    let cache = Cache::open(dir.path()).unwrap();
    let data = content(30_000);
    let digest = Digest::of(&data);
    park_partial(&cache, &digest, &data, 10_000).await;

    let (base, log) = range_server(data.clone(), true).await;
    let path = cache
        .fetch_url(&Client::new(), &format!("{base}/blob"), &digest)
        .await
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), data);
    assert_eq!(log.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn stale_partial_is_retried_from_scratch() {
    let dir = Scratch::new("stale");
    let cache = Cache::open(dir.path()).unwrap();
    let data = content(20_000);
    let digest = Digest::of(&data);
    // A parked prefix of *different* content (e.g. an older version).
    let mut other = data.clone();
    other[5] ^= 0xff;
    park_partial(&cache, &digest, &other, 8_000).await;

    let (base, log) = range_server(data.clone(), false).await;
    let path = cache
        .fetch_url(&Client::new(), &format!("{base}/blob"), &digest)
        .await
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), data);
    assert_eq!(
        *log.lock().unwrap(),
        vec![Some("bytes=8000-".to_string()), None],
        "the resumed attempt fails verification, then one full fetch"
    );
}

#[tokio::test]
async fn complete_partial_is_committed_on_416() {
    let dir = Scratch::new("416");
    let cache = Cache::open(dir.path()).unwrap();
    let data = content(4096);
    let digest = Digest::of(&data);
    // Every byte arrived but the process died before committing.
    park_partial(&cache, &digest, &data, 4096).await;

    let (base, log) = range_server(data.clone(), false).await;
    let path = cache
        .fetch_url(&Client::new(), &format!("{base}/blob"), &digest)
        .await
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), data);
    assert_eq!(*log.lock().unwrap(), vec![Some("bytes=4096-".to_string())]);
}

#[tokio::test]
async fn cancelled_fetch_is_parked_for_resumption() {
    let dir = Scratch::new("cancel");
    let cache = Cache::open(dir.path()).unwrap();
    let data = content(10_000);
    let digest = Digest::of(&data);

    let fut = cache.get_or_fetch(&digest, |_| {
        let mut s = Pieces::new(data.clone(), 1000);
        s.hang_at = Some(3000);
        async move { Ok(Fetched::new(s)) }
    });
    assert!(tokio::time::timeout(Duration::from_millis(200), fut)
        .await
        .is_err());
    assert_eq!(cache.partial_len(&digest), Some(3000));
    assert_eq!(files_in(&dir.path().join("tmp")), 0);

    let offsets = Mutex::new(Vec::new());
    let path = cache
        .get_or_fetch(&digest, |offset| {
            offsets.lock().unwrap().push(offset);
            let rest = data[offset as usize..].to_vec();
            async move { Ok(Fetched::resumed(offset, Pieces::new(rest, 999)).with_total(10_000)) }
        })
        .await
        .unwrap();
    assert_eq!(*offsets.lock().unwrap(), vec![3000]);
    assert_eq!(std::fs::read(path).unwrap(), data);
}

#[tokio::test]
async fn http_errors_are_reported() {
    let dir = Scratch::new("404");
    let cache = Cache::open(dir.path()).unwrap();
    let (base, _) = range_server(Vec::new(), false).await;
    let err = cache
        .fetch_url(
            &Client::new(),
            &format!("{base}/missing"),
            &Digest::of(b"x"),
        )
        .await
        .unwrap_err();
    assert!(err.is_not_found(), "{err:?}");
    assert!(err.to_string().contains("no such blob"), "{err}");
}

#[tokio::test]
async fn concurrent_fetches_in_one_process_download_once() {
    let dir = Scratch::new("conc");
    let cache = Cache::open(dir.path()).unwrap();
    let data = Arc::new(content(64 * 1024));
    let digest = Digest::of(&data);
    let calls = Arc::new(AtomicUsize::new(0));

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let (cache, data, calls) = (cache.clone(), data.clone(), calls.clone());
        tasks.push(tokio::spawn(async move {
            cache
                .get_or_fetch(&digest, |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let mut s = Pieces::new(data.to_vec(), 4096);
                    s.pause = Some(Duration::from_millis(1));
                    async move { Ok(Fetched::new(s)) }
                })
                .await
                .unwrap()
        }));
    }
    let mut paths = Vec::new();
    for t in tasks {
        paths.push(t.await.unwrap());
    }
    assert!(paths.windows(2).all(|w| w[0] == w[1]));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read(&paths[0]).unwrap(), *data);
}

#[tokio::test]
async fn concurrent_fetches_from_separate_caches_never_corrupt() {
    // Two `Cache` values on one directory behave like two processes: no
    // shared lock, so both may download; each writes its own temp file.
    let dir = Scratch::new("procs");
    let data = Arc::new(content(80 * 1024));
    let digest = Digest::of(&data);
    let mut tasks = Vec::new();
    for i in 0..4 {
        let cache = Cache::open(dir.path()).unwrap();
        let data = data.clone();
        tasks.push(tokio::spawn(async move {
            cache
                .get_or_fetch(&digest, |_| {
                    let mut s = Pieces::new(data.to_vec(), 2048 + i * 512);
                    s.pause = Some(Duration::from_millis(1));
                    async move { Ok(Fetched::new(s)) }
                })
                .await
        }));
    }
    for t in tasks {
        let path = t.await.unwrap().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), *data);
    }
    assert_eq!(files_in(&dir.path().join("tmp")), 0);
    let cache = Cache::open(dir.path()).unwrap();
    assert!(cache.verify(&digest).await.unwrap());
}

#[tokio::test]
async fn insert_and_verify() {
    let dir = Scratch::new("insert");
    let cache = Cache::open(dir.path()).unwrap();
    let stored = cache.insert_bytes(b"hello world".to_vec()).await.unwrap();
    assert_eq!(stored.digest, Digest::of(b"hello world"));
    assert_eq!(std::fs::read(&stored.path).unwrap(), b"hello world");
    // Inserting the same content again is harmless.
    let again = cache
        .insert(|| async { Ok(Fetched::from_reader(&b"hello world"[..])) })
        .await
        .unwrap();
    assert_eq!(again, stored);
    assert!(cache.verify(&stored.digest).await.unwrap());

    // Corrupt it on disk: verify notices and evicts it.
    std::fs::write(&stored.path, b"hello w0rld").unwrap();
    assert!(!cache.verify(&stored.digest).await.unwrap());
    assert!(!cache.contains(&stored.digest));
    assert!(!cache.verify(&stored.digest).await.unwrap());

    // An insert source that declares a size it does not deliver.
    let err = cache
        .insert(|| async { Ok(Fetched::from_reader(&b"abc"[..]).with_total(10)) })
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Interrupted(_)), "{err:?}");
    assert_eq!(files_in(&dir.path().join("tmp")), 0);

    assert!(cache.remove(&Digest::of(b"hello world")).await.is_ok());
}

#[tokio::test]
async fn purge_removes_stale_temporaries() {
    let dir = Scratch::new("purge");
    let cache = Cache::open(dir.path()).unwrap();
    std::fs::write(dir.path().join("tmp").join("leftover.tmp"), b"x").unwrap();
    let data = content(1000);
    let digest = Digest::of(&data);
    park_partial(&cache, &digest, &data, 500).await;
    let kept = cache.insert_bytes(data.clone()).await.unwrap();

    assert_eq!(
        cache.purge_temp(Duration::from_secs(3600)).await.unwrap(),
        0
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        cache.purge_temp(Duration::from_millis(10)).await.unwrap(),
        2
    );
    assert!(cache.contains(&kept.digest), "objects are never purged");
}

/// A raw TCP server whose first response promises the whole body but the
/// connection drops after `cut` bytes; later requests get proper ranges.
async fn truncating_server(body: Vec<u8>, cut: usize) -> (String, Arc<Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    tokio::spawn(async move {
        let mut first = true;
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut head = Vec::new();
            let mut buf = [0u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                head.extend_from_slice(&buf[..n]);
            }
            let head = String::from_utf8_lossy(&head).to_lowercase();
            let range = head
                .lines()
                .find_map(|l| l.strip_prefix("range: bytes="))
                .map(|r| r.trim_end_matches('-').trim().parse::<usize>().unwrap());
            seen.lock().unwrap().push(format!("{range:?}"));
            let out = match (first, range) {
                (true, _) => {
                    let mut r = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    r.extend_from_slice(&body[..cut]);
                    r
                }
                (false, Some(start)) => {
                    let mut r = format!(
                        "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes {start}-{}/{}\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n",
                        body.len() - 1,
                        body.len(),
                        body.len() - start
                    )
                    .into_bytes();
                    r.extend_from_slice(&body[start..]);
                    r
                }
                (false, None) => {
                    let mut r = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    r.extend_from_slice(&body);
                    r
                }
            };
            first = false;
            let _ = sock.write_all(&out).await;
            let _ = sock.shutdown().await;
        }
    });
    (format!("http://{addr}"), log)
}

#[tokio::test]
async fn dropped_connection_resumes_on_the_next_call() {
    let dir = Scratch::new("dropped");
    let cache = Cache::open(dir.path()).unwrap();
    let data = content(100_000);
    let digest = Digest::of(&data);
    let (base, log) = truncating_server(data.clone(), 40_000).await;
    let url = format!("{base}/blob");
    let client = Client::new();

    let err = cache.fetch_url(&client, &url, &digest).await.unwrap_err();
    assert!(matches!(err, StoreError::Http(_)), "{err:?}");
    assert!(cache.get(&digest).is_none());
    let parked = cache.partial_len(&digest).unwrap();
    assert!(parked > 0 && parked <= 40_000, "{parked}");

    let path = cache.fetch_url(&client, &url, &digest).await.unwrap();
    assert_eq!(std::fs::read(path).unwrap(), data);
    assert_eq!(
        *log.lock().unwrap(),
        vec!["None".to_string(), format!("Some({parked})")]
    );
}

/// Makes every file in `dir` look `age` old.
fn age_files(dir: &std::path::Path, age: Duration) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(std::time::SystemTime::now() - age)
            .unwrap();
    }
}

#[tokio::test]
async fn purge_does_not_remove_a_partial_being_resumed() {
    // ST-5: claiming keeps the parked file's old mtime, so a purge running
    // during the resume used to delete it and fail the commit.
    let dir = Scratch::new("purge-resume");
    let cache = Cache::open(dir.path()).unwrap();
    let data = content(100_000);
    let digest = Digest::of(&data);
    park_partial(&cache, &digest, &data, 50_000).await;
    let day = Duration::from_secs(86_400);
    age_files(&dir.path().join("partial"), 2 * day);

    // A janitor on another `Cache` over the same directory.
    let janitor = Cache::open(dir.path()).unwrap();
    let purged = Arc::new(AtomicUsize::new(usize::MAX));
    let seen = purged.clone();
    let path = cache
        .get_or_fetch(&digest, |offset| {
            let janitor = janitor.clone();
            let seen = seen.clone();
            let rest = data[offset as usize..].to_vec();
            async move {
                let n = janitor.purge_temp(day).await.unwrap();
                seen.store(n, Ordering::SeqCst);
                Ok(Fetched::resumed(offset, Pieces::new(rest, 10_000)).with_total(100_000))
            }
        })
        .await
        .unwrap();
    assert_eq!(
        purged.load(Ordering::SeqCst),
        0,
        "the live partial was purged"
    );
    assert_eq!(std::fs::read(path).unwrap(), data);

    // Once nothing uses them, old leftovers do go.
    std::fs::write(dir.path().join("tmp").join("leftover.tmp"), b"x").unwrap();
    age_files(&dir.path().join("tmp"), 2 * day);
    assert_eq!(janitor.purge_temp(day).await.unwrap(), 1);
}

#[tokio::test]
async fn a_short_transfer_is_retryable_and_resumes() {
    // ST-6: a body that ends before its declared total keeps its bytes and
    // reports `Interrupted`, not `SizeMismatch`.
    let dir = Scratch::new("short");
    let cache = Cache::open(dir.path()).unwrap();
    let data = content(10_000);
    let digest = Digest::of(&data);
    let head = data[..4_000].to_vec();
    let err = cache
        .get_or_fetch(&digest, |_| {
            let head = head.clone();
            async move { Ok(Fetched::from_bytes(head).with_total(10_000)) }
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Interrupted(_)), "{err:?}");
    assert_eq!(cache.partial_len(&digest), Some(4_000));
    #[cfg(feature = "errutils")]
    {
        use ferroly::errutils::{Classify, ErrorKind};
        let kind = ferroly::Error::from(err).kind();
        assert_eq!(kind, ErrorKind::Unavailable);
        assert!(kind.is_retryable());
        let refused = ferroly::Error::from(StoreError::Protocol(
            "refusing a redirect from https to http".into(),
        ));
        assert_eq!(refused.kind(), ErrorKind::InvalidInput);
        assert!(!refused.kind().is_retryable());
    }
    let path = cache
        .get_or_fetch(&digest, |offset| {
            let rest = data[offset as usize..].to_vec();
            async move { Ok(Fetched::resumed(offset, Pieces::new(rest, 1_000)).with_total(10_000)) }
        })
        .await
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), data);
}

#[tokio::test]
async fn wrong_content_after_an_ignored_range_is_not_fetched_twice() {
    // ST-12: the source ignored the range (the attempt started over at 0)
    // and the content is wrong: the "retry from 0" for a stale prefix used
    // to download the same wrong object a second time.
    let dir = Scratch::new("ignored-range");
    let cache = Cache::open(dir.path()).unwrap();
    let data = content(20_000);
    let digest = Digest::of(&data);
    park_partial(&cache, &digest, &data, 5_000).await;
    let mut wrong = data.clone();
    wrong[15_000] ^= 0xff;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let err = cache
        .get_or_fetch(&digest, |_offset| {
            counter.fetch_add(1, Ordering::SeqCst);
            let wrong = wrong.clone();
            async move { Ok(Fetched::from_bytes(wrong)) }
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::DigestMismatch { .. }), "{err:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(files_in(&dir.path().join("tmp")), 0);
}

#[tokio::test]
async fn a_stalled_source_is_cut_off_and_parked() {
    let dir = Scratch::new("stall");
    let cache = Cache::open(dir.path())
        .unwrap()
        .with_stall_timeout(Some(Duration::from_millis(100)));
    let data = content(10_000);
    let digest = Digest::of(&data);
    let source = data.clone();
    let started = std::time::Instant::now();
    let err = cache
        .get_or_fetch(&digest, |_| {
            let mut s = Pieces::new(source.clone(), 1_000);
            s.hang_at = Some(3_000);
            async move { Ok(Fetched::new(s).with_total(10_000)) }
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Interrupted(_)), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(cache.partial_len(&digest), Some(3_000));

    // The overall deadline: a source that keeps dripping is still stopped.
    let deadline = Cache::open(dir.path())
        .unwrap()
        .with_fetch_timeout(Some(Duration::from_millis(300)));
    let other = content(50_000);
    let other_digest = Digest::of(&other);
    let err = deadline
        .get_or_fetch(&other_digest, |_| {
            let mut s = Pieces::new(other.clone(), 100);
            s.pause = Some(Duration::from_millis(20));
            async move { Ok(Fetched::new(s)) }
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Interrupted(_)), "{err:?}");
    assert!(deadline.partial_len(&other_digest).unwrap_or(0) > 0);
    // A fetch callback that never answers is bounded too.
    let err = deadline
        .insert(std::future::pending::<Result<Fetched, StoreError>>)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Interrupted(_)), "{err:?}");
}

#[tokio::test]
async fn progress_is_reported() {
    let dir = Scratch::new("progress");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let cache = Cache::open(dir.path())
        .unwrap()
        .with_progress(move |_, done, total| log.lock().unwrap().push((done, total)));
    let data = content(3_000);
    let digest = Digest::of(&data);
    park_partial(&cache, &digest, &data, 1_000).await;
    seen.lock().unwrap().clear();
    cache
        .get_or_fetch_sized(&digest, 3_000, |offset| {
            let rest = data[offset as usize..].to_vec();
            async move { Ok(Fetched::resumed(offset, Pieces::new(rest, 1_000))) }
        })
        .await
        .unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            (1_000, Some(3_000)),
            (2_000, Some(3_000)),
            (3_000, Some(3_000))
        ]
    );
}

#[tokio::test]
async fn eviction_keeps_the_newest_objects_within_the_budget() {
    let dir = Scratch::new("evict");
    let cache = Cache::open(dir.path()).unwrap();
    let mut stored = Vec::new();
    for i in 0..4u8 {
        let s = cache.insert_bytes(vec![i; 1_000]).await.unwrap();
        // Distinct, ordered modification times.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&s.path)
            .unwrap();
        file.set_modified(std::time::SystemTime::now() - Duration::from_secs(100 - u64::from(i)))
            .unwrap();
        stored.push(s);
    }
    assert_eq!(cache.usage().await.unwrap(), 4_000);
    let evicted = cache.evict_to(2_500).await.unwrap();
    assert_eq!(
        (evicted.removed, evicted.freed, evicted.remaining),
        (2, 2_000, 2_000)
    );
    assert!(!cache.contains(&stored[0].digest) && !cache.contains(&stored[1].digest));
    assert!(cache.contains(&stored[2].digest) && cache.contains(&stored[3].digest));
    assert_eq!(cache.evict_to(10_000).await.unwrap().removed, 0);
    assert_eq!(cache.evict_to(0).await.unwrap().remaining, 0);
}

#[tokio::test]
async fn fetch_http_does_not_decode_the_body() {
    // ST-13: `Accept-Encoding: identity` alone does not stop a decompressing
    // client from decoding a body served with `Content-Encoding`.
    let dir = Scratch::new("encoded");
    let cache = Cache::open(dir.path()).unwrap();
    let stored = b"\x1f\x8b\x08 stored as gzip, cached as stored".to_vec();
    let body = stored.clone();
    let base = spawn(move |req| serve_range(&req, &body).header("content-encoding", "gzip")).await;
    let client = Client::builder().decompress(true).build().unwrap();
    let path = cache
        .fetch_url(&client, &format!("{base}/blob"), &Digest::of(&stored))
        .await
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), stored);
}

// ------------------------------------------------ layouts and the sync core

#[tokio::test]
async fn layout_fanout_is_the_default() {
    let dir = Scratch::new("layout-default");
    let a = Cache::open(dir.path()).unwrap();
    let b = Cache::open_with(dir.path(), Layout::Fanout).unwrap();
    assert_eq!(a.layout(), &Layout::Fanout);
    let d = Digest::of(b"x");
    let hex = d.hex();
    assert_eq!(a.path_for(&d), b.path_for(&d));
    assert!(a
        .path_for(&d)
        .ends_with(std::path::Path::new("sha256").join(&hex[..2]).join(&hex)));
    let stored = a.insert_bytes(b"x".to_vec()).await.unwrap();
    assert_eq!(stored.path, a.path_for(&d));
}

#[tokio::test]
async fn flat_layout_fetch_resumes() {
    let dir = Scratch::new("resume-flat");
    let cache = Cache::open_with(dir.path(), Layout::flat("blobs").unwrap()).unwrap();
    let data = content(50_000);
    let digest = Digest::of(&data);
    park_partial(&cache, &digest, &data, 20_000).await;
    assert_eq!(files_in(&dir.path().join("partial")), 1);

    let (base, log) = range_server(data.clone(), false).await;
    let path = cache
        .fetch_url(&Client::new(), &format!("{base}/blob"), &digest)
        .await
        .unwrap();
    assert_eq!(
        path,
        dir.path().join("blobs").join("sha256").join(digest.hex())
    );
    assert_eq!(std::fs::read(path).unwrap(), data);
    assert_eq!(
        *log.lock().unwrap(),
        vec![Some("bytes=20000-".to_string())],
        "only the missing tail is requested"
    );
    assert_eq!(cache.partial_len(&digest), None);
    assert_eq!(files_in(&dir.path().join("tmp")), 0);
    assert!(!dir.path().join("sha256").exists());
}

#[tokio::test]
async fn local_and_async_share_a_store() {
    let dir = Scratch::new("shared-core");
    let store = LocalStore::open_with(dir.path(), Layout::flat("blobs").unwrap()).unwrap();
    let synced = store.insert_bytes(b"from the sync side").unwrap();

    let cache = Cache::from_local(store.clone());
    assert_eq!(cache.local().root(), store.root());
    let path = cache
        .get_or_fetch(&synced.digest, |_| async { must_not_fetch() })
        .await
        .unwrap();
    assert_eq!(path, synced.path);

    let from_async = cache
        .insert_bytes(b"from the async side".to_vec())
        .await
        .unwrap();
    let listed: Vec<Digest> = store.list().unwrap().iter().map(|o| o.digest).collect();
    assert!(listed.contains(&from_async.digest) && listed.contains(&synced.digest));
    assert_eq!(cache.list().await.unwrap().len(), 2);

    // `From` and the shared size cap.
    let capped: Cache = store.clone().with_max_object_bytes(Some(4)).into();
    let err = capped.insert_bytes(b"too long".to_vec()).await.unwrap_err();
    assert!(matches!(err, StoreError::SizeMismatch(_)), "{err:?}");
    let capped = Cache::from_local(store).with_max_object_bytes(Some(4));
    assert_eq!(capped.local().max_object_bytes(), Some(4));
    assert!(capped.local().insert_bytes(b"too long").is_err());
}

#[tokio::test]
async fn async_list_resolve_and_check() {
    let dir = Scratch::new("async-mirrors");
    let cache = Cache::open(dir.path()).unwrap();
    let a = cache.insert_bytes(b"alpha".to_vec()).await.unwrap();
    let b = cache.insert_bytes(b"beta".to_vec()).await.unwrap();
    assert_eq!(cache.list().await.unwrap().len(), 2);
    assert_eq!(
        cache.resolve_prefix(&a.digest.hex()[..12]).await.unwrap(),
        a.digest
    );
    assert_eq!(
        cache
            .resolve_prefix_min(&b.digest.hex()[..6], 6)
            .await
            .unwrap(),
        b.digest
    );
    assert!(cache
        .resolve_prefix(&Digest::of(b"gamma").hex()[..12])
        .await
        .unwrap_err()
        .is_not_found());

    std::fs::write(&b.path, b"BETA").unwrap();
    assert_eq!(
        cache.check(&a.digest).await.unwrap(),
        ObjectStatus::Intact { size: 5 }
    );
    assert!(matches!(
        cache.check(&b.digest).await.unwrap(),
        ObjectStatus::Corrupt { size: 4, .. }
    ));
    let all = cache.check_all().await.unwrap();
    assert_eq!(all.len(), 2);
    // `check` never deletes; `verify` still does, under the store lock.
    assert!(cache.contains(&b.digest));
    assert!(!cache.verify(&b.digest).await.unwrap());
    assert!(!cache.contains(&b.digest));
    assert!(dir.path().join(".lock").is_file());
    assert_eq!(cache.check(&b.digest).await.unwrap(), ObjectStatus::Missing);
}

#[tokio::test]
async fn cache_lock_async() {
    let dir = Scratch::new("async-lock");
    let cache = Cache::open(dir.path()).unwrap();
    let stored = cache.insert_bytes(b"locked".to_vec()).await.unwrap();
    let guard = cache.lock().await.unwrap();

    let remover = {
        let cache = cache.clone();
        let digest = stored.digest;
        tokio::spawn(async move { cache.remove(&digest).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!remover.is_finished(), "remove ran while the lock was held");
    assert!(stored.path.exists());

    // The guard's own methods work while it is held.
    assert!(!guard.remove(&Digest::of(b"absent")).unwrap());
    drop(guard);
    let removed = tokio::time::timeout(Duration::from_secs(10), remover)
        .await
        .expect("remove did not finish after the lock was released")
        .unwrap()
        .unwrap();
    assert!(removed);
    assert!(!stored.path.exists());
}

#[tokio::test]
async fn cancelled_destructive_calls_delete_nothing() {
    let dir = Scratch::new("async-cancel");
    let cache = Cache::open(dir.path()).unwrap();
    let a = cache.insert_bytes(b"alpha".to_vec()).await.unwrap();
    let b = cache.insert_bytes(b"beta".to_vec()).await.unwrap();
    let c = cache.insert_bytes(b"gamma".to_vec()).await.unwrap();
    std::fs::write(&c.path, b"GAMMA").unwrap(); // corrupt: `verify` would delete it

    let held = cache.local().lock().unwrap();
    let short = Duration::from_millis(100);
    assert!(tokio::time::timeout(short, cache.remove(&a.digest))
        .await
        .is_err());
    assert!(tokio::time::timeout(short, cache.evict_to(0))
        .await
        .is_err());
    assert!(tokio::time::timeout(short, cache.verify(&c.digest))
        .await
        .is_err());
    drop(held);
    // Long enough for any attempt still in flight to see the lock free.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(a.path.exists(), "a dropped remove deleted the object");
    assert!(b.path.exists(), "a dropped evict_to deleted an object");
    assert!(c.path.exists(), "a dropped verify deleted the object");

    // Not cancelled, they still do their work.
    assert!(cache.remove(&a.digest).await.unwrap());
    assert!(!cache.verify(&c.digest).await.unwrap());
    assert!(!c.path.exists());
    assert_eq!(cache.evict_to(0).await.unwrap().removed, 1);
    assert!(!b.path.exists());
}

#[test]
fn a_cancelled_remove_does_not_hold_up_runtime_shutdown() {
    let dir = Scratch::new("async-cancel-shutdown");
    let store = LocalStore::open(dir.path()).unwrap();
    let stored = store.insert_bytes(b"kept").unwrap();
    let held = store.lock().unwrap();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let cache = Cache::from_local(store.clone());
    let timed_out = rt.block_on(async {
        tokio::time::timeout(Duration::from_millis(100), cache.remove(&stored.digest))
            .await
            .is_err()
    });
    assert!(timed_out);
    let started = std::time::Instant::now();
    drop(rt); // waits for the blocking pool
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "shutdown waited {:?} for the lock holder",
        started.elapsed()
    );
    drop(held);
    std::thread::sleep(Duration::from_millis(200));
    assert!(stored.path.exists());
}

#[tokio::test]
async fn nothing_to_delete_takes_no_lock() {
    let dir = Scratch::new("async-no-lock");
    let cache = Cache::open(dir.path()).unwrap();
    let a = cache.insert_bytes(b"alpha".to_vec()).await.unwrap();
    let absent = Digest::of(b"absent");
    // Even with the lock held elsewhere, these return at once.
    let held = cache.local().lock().unwrap();
    let quick = Duration::from_secs(5);
    assert!(!tokio::time::timeout(quick, cache.remove(&absent))
        .await
        .unwrap()
        .unwrap());
    let fits = tokio::time::timeout(quick, cache.evict_to(u64::MAX))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((fits.removed, fits.freed, fits.remaining), (0, 0, 5));
    assert!(!tokio::time::timeout(quick, cache.verify(&absent))
        .await
        .unwrap()
        .unwrap());
    drop(held);
    assert!(a.path.exists());
}

#[tokio::test]
async fn nothing_to_delete_creates_no_lock_file() {
    let dir = Scratch::new("async-no-lock-file");
    let cache = Cache::open(dir.path()).unwrap();
    cache.insert_bytes(b"alpha".to_vec()).await.unwrap();
    assert!(!cache.remove(&Digest::of(b"absent")).await.unwrap());
    assert_eq!(cache.evict_to(u64::MAX).await.unwrap().removed, 0);
    assert!(
        !dir.path().join(".lock").exists(),
        "the lock file was created with nothing to delete"
    );
}

/// On a store root this user may not write to, `remove` of an absent object
/// and an `evict_to` with nothing to do succeed (as in 0.6) instead of
/// failing to create the lock file.
#[cfg(unix)]
#[tokio::test]
async fn read_only_root_allows_no_op_deletions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Scratch::new("async-read-only");
    let cache = Cache::open(dir.path()).unwrap();
    cache.insert_bytes(b"alpha".to_vec()).await.unwrap();
    let set_mode =
        |mode| std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode));
    set_mode(0o555).unwrap();
    if std::fs::write(dir.path().join("probe"), b"x").is_ok() {
        // Running as root, which ignores the mode: nothing to test.
        set_mode(0o755).unwrap();
        return;
    }
    let removed = cache.remove(&Digest::of(b"absent")).await;
    let evicted = cache.evict_to(u64::MAX).await;
    set_mode(0o755).unwrap();
    assert!(!removed.unwrap());
    assert_eq!(evicted.unwrap().remaining, 5);
}

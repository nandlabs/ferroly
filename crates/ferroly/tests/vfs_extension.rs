#![cfg(feature = "vfs")]
//! The `ferroly::vfs` extension surface: richer metadata, typed backend
//! errors, and the operations an external (object-store) backend needs.

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use ferroly::vfs::{
    copy_between, strip_scheme, BoxFuture, Entry, LocalFs, MemoryFs, Metadata, Precondition,
    SignedUrlMethod, Vfile, Vfs, VfsError, VfsRegistry, VfsWriter, WalkAction, WalkFn,
    WriteOptions, WriteSink,
};
use tokio::io::{AsyncRead, ReadBuf};

// ---- an external backend ---------------------------------------------------

/// A fake object store: flat keys, multipart-style streaming writes whose
/// aborts are counted, and one failure switch.
#[derive(Default)]
struct FakeBucket {
    objects: Mutex<HashMap<String, Vec<u8>>>,
    open_uploads: Arc<AtomicUsize>,
    aborted_uploads: Arc<AtomicUsize>,
    unavailable: AtomicBool,
}

impl FakeBucket {
    fn stored(&self, key: &str) -> Option<Vec<u8>> {
        self.objects.lock().unwrap().get(key).cloned()
    }

    fn is_empty(&self) -> bool {
        self.objects.lock().unwrap().is_empty()
    }
}

/// The object key for a `fake://` address (or a bare key).
fn key_of(path: &str) -> Result<String, VfsError> {
    Ok(strip_scheme(path, &["fake"], "FakeBucket")?
        .trim_start_matches('/')
        .to_string())
}

/// A ready `Unsupported` future, for the operations a fake does not need.
fn unsupported<'a, T: Send + 'a>(op: &str) -> BoxFuture<'a, Result<T, VfsError>> {
    let what = op.to_string();
    Box::pin(async move { Err(VfsError::Unsupported(what)) })
}

impl Vfs for FakeBucket {
    fn schemes(&self) -> &[&str] {
        &["fake"]
    }

    fn read(&self, path: &str) -> BoxFuture<'_, Result<Vec<u8>, VfsError>> {
        let key = key_of(path);
        Box::pin(async move {
            let key = key?;
            self.stored(&key).ok_or(VfsError::NotFound(key))
        })
    }

    fn read_range(&self, _: &str, _: u64, _: u64) -> BoxFuture<'_, Result<Vec<u8>, VfsError>> {
        unsupported("read_range")
    }

    fn write(&self, path: &str, data: Vec<u8>) -> BoxFuture<'_, Result<(), VfsError>> {
        let key = key_of(path);
        Box::pin(async move {
            self.objects.lock().unwrap().insert(key?, data);
            Ok(())
        })
    }

    fn open(&self, path: &str) -> BoxFuture<'_, Result<Vfile, VfsError>> {
        let key = key_of(path);
        Box::pin(async move {
            let key = key?;
            let data = self.stored(&key).ok_or(VfsError::NotFound(key))?;
            let meta = Metadata::file(data.len() as u64);
            Ok(Vfile::from_bytes(data, meta))
        })
    }

    fn delete(&self, path: &str) -> BoxFuture<'_, Result<(), VfsError>> {
        let key = key_of(path);
        Box::pin(async move {
            let key = key?;
            let removed = self.objects.lock().unwrap().remove(&key);
            removed.map(|_| ()).ok_or(VfsError::NotFound(key))
        })
    }

    fn exists(&self, path: &str) -> BoxFuture<'_, bool> {
        let key = key_of(path);
        Box::pin(async move {
            // `exists` never errors, so an outage looks like "absent".
            !self.unavailable.load(Ordering::SeqCst)
                && key.is_ok_and(|key| self.stored(&key).is_some())
        })
    }

    fn metadata(&self, path: &str) -> BoxFuture<'_, Result<Metadata, VfsError>> {
        let shown = path.to_string();
        let key = key_of(path);
        Box::pin(async move {
            if self.unavailable.load(Ordering::SeqCst) {
                return Err(VfsError::Unavailable(shown));
            }
            let key = key?;
            let data = self.stored(&key).ok_or(VfsError::NotFound(key))?;
            Ok(Metadata::file(data.len() as u64))
        })
    }

    fn list(&self, _: &str) -> BoxFuture<'_, Result<Vec<Entry>, VfsError>> {
        unsupported("list")
    }

    fn mkdir_all(&self, _: &str) -> BoxFuture<'_, Result<(), VfsError>> {
        unsupported("mkdir_all")
    }

    fn copy(&self, _: &str, _: &str) -> BoxFuture<'_, Result<(), VfsError>> {
        unsupported("copy")
    }

    fn rename(&self, _: &str, _: &str) -> BoxFuture<'_, Result<(), VfsError>> {
        unsupported("rename")
    }

    fn walk(&self, _: &str, _: WalkFn) -> BoxFuture<'_, Result<(), VfsError>> {
        unsupported("walk")
    }

    fn create(&self, path: &str) -> BoxFuture<'_, Result<VfsWriter<'_>, VfsError>> {
        let key = key_of(path);
        Box::pin(async move {
            let key = key?;
            self.open_uploads.fetch_add(1, Ordering::SeqCst);
            Ok(VfsWriter::new(FakeUpload {
                bucket: self,
                key,
                parts: Vec::new(),
            }))
        })
    }

    fn signed_url(
        &self,
        path: &str,
        _method: SignedUrlMethod,
        _expires_in: Duration,
    ) -> BoxFuture<'_, Result<String, VfsError>> {
        let key = key_of(path);
        Box::pin(async move { Ok(format!("https://fake.example/{}?sig=1", key?)) })
    }

    fn as_any(&self) -> Option<&(dyn std::any::Any + Send + Sync)> {
        Some(self)
    }
}

/// A multipart-style upload: nothing is stored until `finish`.
struct FakeUpload<'a> {
    bucket: &'a FakeBucket,
    key: String,
    parts: Vec<u8>,
}

impl WriteSink for FakeUpload<'_> {
    fn write_all<'s>(&'s mut self, data: &'s [u8]) -> BoxFuture<'s, Result<(), VfsError>> {
        Box::pin(async move {
            self.parts.extend_from_slice(data);
            Ok(())
        })
    }

    fn finish<'s>(self: Box<Self>) -> BoxFuture<'s, Result<(), VfsError>>
    where
        Self: 's,
    {
        Box::pin(async move {
            let this = *self;
            this.bucket
                .objects
                .lock()
                .unwrap()
                .insert(this.key, this.parts);
            this.bucket.open_uploads.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        })
    }

    fn abort<'s>(self: Box<Self>) -> BoxFuture<'s, Result<(), VfsError>>
    where
        Self: 's,
    {
        Box::pin(async move {
            self.bucket.aborted_uploads.fetch_add(1, Ordering::SeqCst);
            self.bucket.open_uploads.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

/// A source whose only file yields a few bytes and then fails.
struct BrokenSource;

/// Hands out `data` once, then errors.
struct BreaksAfter {
    data: Vec<u8>,
    sent: bool,
}

impl AsyncRead for BreaksAfter {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.sent {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "the source went away",
            )));
        }
        self.sent = true;
        buf.put_slice(&self.data);
        Poll::Ready(Ok(()))
    }
}

impl Vfs for BrokenSource {
    fn schemes(&self) -> &[&str] {
        &["broken"]
    }
    fn read(&self, _: &str) -> BoxFuture<'_, Result<Vec<u8>, VfsError>> {
        unsupported("read")
    }
    fn read_range(&self, _: &str, _: u64, _: u64) -> BoxFuture<'_, Result<Vec<u8>, VfsError>> {
        unsupported("read_range")
    }
    fn write(&self, _: &str, _: Vec<u8>) -> BoxFuture<'_, Result<(), VfsError>> {
        unsupported("write")
    }
    fn open(&self, _: &str) -> BoxFuture<'_, Result<Vfile, VfsError>> {
        Box::pin(async move {
            let reader = BreaksAfter {
                data: b"first bytes".to_vec(),
                sent: false,
            };
            Ok(Vfile::new(reader, Metadata::file(1024)))
        })
    }
    fn delete(&self, _: &str) -> BoxFuture<'_, Result<(), VfsError>> {
        unsupported("delete")
    }
    fn exists(&self, _: &str) -> BoxFuture<'_, bool> {
        Box::pin(async move { true })
    }
    fn metadata(&self, _: &str) -> BoxFuture<'_, Result<Metadata, VfsError>> {
        Box::pin(async move { Ok(Metadata::file(1024)) })
    }
    fn list(&self, _: &str) -> BoxFuture<'_, Result<Vec<Entry>, VfsError>> {
        unsupported("list")
    }
    fn mkdir_all(&self, _: &str) -> BoxFuture<'_, Result<(), VfsError>> {
        unsupported("mkdir_all")
    }
    fn copy(&self, _: &str, _: &str) -> BoxFuture<'_, Result<(), VfsError>> {
        unsupported("copy")
    }
    fn rename(&self, _: &str, _: &str) -> BoxFuture<'_, Result<(), VfsError>> {
        unsupported("rename")
    }
    fn walk(&self, _: &str, _: WalkFn) -> BoxFuture<'_, Result<(), VfsError>> {
        unsupported("walk")
    }
}

/// A unique directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ferroly-vfs-ext-{tag}-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }

    fn file_count(&self) -> usize {
        std::fs::read_dir(&self.0).unwrap().count()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---- metadata (V3) ---------------------------------------------------------

#[test]
fn metadata_builders_set_the_new_fields() {
    let m = Metadata::file(3)
        .with_etag("\"abc\"")
        .with_version("v7")
        .with_content_type("text/plain")
        .with_user_metadata("k", "first")
        .with_user_metadata("other", "x")
        .with_user_metadata("k", "second");
    assert_eq!(m.etag.as_deref(), Some("\"abc\""));
    assert_eq!(m.version.as_deref(), Some("v7"));
    assert_eq!(m.content_type.as_deref(), Some("text/plain"));
    assert_eq!(
        m.user_metadata,
        vec![
            ("k".to_string(), "first".to_string()),
            ("other".to_string(), "x".to_string()),
            ("k".to_string(), "second".to_string()),
        ]
    );
    // The first pair with the name wins.
    assert_eq!(m.user("k"), Some("first"));
    assert_eq!(m.user("other"), Some("x"));
    assert_eq!(m.user("missing"), None);

    // Untouched by default.
    let plain = Metadata::file(0);
    assert!(plain.etag.is_none() && plain.version.is_none() && plain.content_type.is_none());
    assert!(plain.user_metadata.is_empty());
}

#[tokio::test]
async fn memory_fs_reports_an_etag_that_changes_on_write() {
    let fs = MemoryFs::new();
    fs.write("/a.txt", b"one".to_vec()).await.unwrap();
    let first = fs.metadata("/a.txt").await.unwrap();
    assert!(first.etag.is_some());
    assert_eq!(first.version, first.etag);

    fs.write("/a.txt", b"two".to_vec()).await.unwrap();
    let second = fs.metadata("/a.txt").await.unwrap();
    assert!(second.etag.is_some());
    assert_ne!(second.etag, first.etag);
    assert_eq!(second.version, second.etag);

    // An append stores new contents too.
    fs.append("/a.txt", b"!".to_vec()).await.unwrap();
    let third = fs.metadata("/a.txt").await.unwrap();
    assert_ne!(third.etag, second.etag);

    // Open reports the same metadata.
    let opened = fs.open("/a.txt").await.unwrap();
    assert_eq!(opened.metadata().etag, third.etag);

    // Directories have none.
    fs.mkdir_all("/d").await.unwrap();
    assert!(fs.metadata("/d").await.unwrap().etag.is_none());
}

#[tokio::test]
async fn memory_fs_rename_keeps_the_etag_and_copy_gets_a_new_one() {
    let fs = MemoryFs::new();
    fs.write("/a.txt", b"data".to_vec()).await.unwrap();
    let before = fs.metadata("/a.txt").await.unwrap().etag;

    fs.rename("/a.txt", "/b.txt").await.unwrap();
    assert_eq!(fs.metadata("/b.txt").await.unwrap().etag, before);

    fs.copy("/b.txt", "/c.txt").await.unwrap();
    let copied = fs.metadata("/c.txt").await.unwrap().etag;
    assert!(copied.is_some());
    assert_ne!(copied, before);
    // The source is unchanged by the copy.
    assert_eq!(fs.metadata("/b.txt").await.unwrap().etag, before);
}

// ---- typed errors (V4) -----------------------------------------------------

#[test]
fn new_error_variants_display_and_classify_retryable() {
    let path = "s3://bucket/key";
    let retryable = [
        VfsError::Unavailable(path.to_string()),
        VfsError::Timeout(path.to_string()),
    ];
    let permanent = [
        VfsError::AlreadyExists(path.to_string()),
        VfsError::PreconditionFailed(path.to_string()),
        VfsError::Unauthenticated(path.to_string()),
        VfsError::NotFound(path.to_string()),
    ];
    for e in &retryable {
        assert!(e.is_retryable(), "{e:?} should be retryable");
        assert!(e.to_string().contains(path), "{e}");
    }
    for e in &permanent {
        assert!(!e.is_retryable(), "{e:?} should not be retryable");
        assert!(e.to_string().contains(path), "{e}");
    }
    assert_eq!(
        VfsError::AlreadyExists("p".into()).to_string(),
        "already exists: p"
    );
    assert_eq!(
        VfsError::PreconditionFailed("p".into()).to_string(),
        "precondition failed: p"
    );
    assert_eq!(
        VfsError::Unavailable("p".into()).to_string(),
        "backend unavailable: p"
    );
    assert_eq!(VfsError::Timeout("p".into()).to_string(), "timed out: p");
    assert_eq!(
        VfsError::Unauthenticated("p".into()).to_string(),
        "unauthenticated: p"
    );
}

#[cfg(feature = "errutils")]
#[test]
fn root_error_classifies_the_new_vfs_errors() {
    use ferroly::errutils::{Classify, ErrorKind};

    let kind = |e: VfsError| ferroly::Error::from(e).kind();
    assert_eq!(
        kind(VfsError::AlreadyExists("p".into())),
        ErrorKind::Conflict
    );
    assert_eq!(
        kind(VfsError::PreconditionFailed("p".into())),
        ErrorKind::Conflict
    );
    assert_eq!(
        kind(VfsError::Unavailable("p".into())),
        ErrorKind::Unavailable
    );
    assert_eq!(kind(VfsError::Timeout("p".into())), ErrorKind::Timeout);
    assert_eq!(
        kind(VfsError::Unauthenticated("p".into())),
        ErrorKind::Unauthenticated
    );
}

// ---- async abort (V1) ------------------------------------------------------

#[tokio::test]
async fn abort_discards_a_streaming_write() {
    let fs = MemoryFs::new();
    let mut writer = fs.create("/pending.bin").await.unwrap();
    writer.write_all(b"never published").await.unwrap();
    writer.abort().await.unwrap();
    assert!(!fs.exists("/pending.bin").await);

    // An existing file is left untouched.
    fs.write("/kept.bin", b"old".to_vec()).await.unwrap();
    let mut writer = fs.create("/kept.bin").await.unwrap();
    writer.write_all(b"new").await.unwrap();
    writer.abort().await.unwrap();
    assert_eq!(fs.read("/kept.bin").await.unwrap(), b"old");
}

#[tokio::test]
async fn abort_removes_the_local_draft() {
    let dir = TempDir::new("abort");
    let fs = LocalFs::rooted(&dir.0).unwrap();
    let mut writer = fs.create("out.bin").await.unwrap();
    writer.write_all(b"partial contents").await.unwrap();
    // The temporary sibling exists while the write is pending.
    assert_eq!(dir.file_count(), 1);
    writer.abort().await.unwrap();
    assert_eq!(dir.file_count(), 0, "no leftover temporary file");
    assert!(!fs.exists("out.bin").await);
    assert!(!dir.0.join("out.bin").exists());
}

#[tokio::test]
async fn abort_reaches_an_external_sink() {
    let bucket = FakeBucket::default();
    let mut writer = bucket.create("fake://big.bin").await.unwrap();
    assert_eq!(bucket.open_uploads.load(Ordering::SeqCst), 1);
    writer.write_all(b"part one").await.unwrap();
    writer.abort().await.unwrap();
    assert_eq!(bucket.aborted_uploads.load(Ordering::SeqCst), 1);
    assert_eq!(bucket.open_uploads.load(Ordering::SeqCst), 0);
    assert!(bucket.is_empty());

    // A finished upload is stored and not counted as aborted.
    let mut writer = bucket.create("fake://done.bin").await.unwrap();
    writer.write_all(b"ok").await.unwrap();
    writer.finish().await.unwrap();
    assert_eq!(bucket.stored("done.bin").as_deref(), Some(&b"ok"[..]));
    assert_eq!(bucket.aborted_uploads.load(Ordering::SeqCst), 1);
    assert_eq!(bucket.open_uploads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn copy_between_aborts_the_destination_when_the_source_fails() {
    let bucket = FakeBucket::default();
    let err = copy_between(&BrokenSource, "any", &bucket, "fake://copy.bin")
        .await
        .unwrap_err();
    assert!(
        matches!(&err, VfsError::Io { path, .. } if path == "any"),
        "{err:?}"
    );
    assert_eq!(bucket.aborted_uploads.load(Ordering::SeqCst), 1);
    assert_eq!(bucket.open_uploads.load(Ordering::SeqCst), 0);
    assert!(bucket.is_empty());
}

// ---- write options (V2) ----------------------------------------------------

#[tokio::test]
async fn if_absent_refuses_to_overwrite() {
    let fs = MemoryFs::new();
    fs.write_with(
        "/once.txt",
        b"first".to_vec(),
        WriteOptions::new().if_absent(),
    )
    .await
    .unwrap();
    let err = fs
        .write_with(
            "/once.txt",
            b"second".to_vec(),
            WriteOptions::new().if_absent(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, VfsError::AlreadyExists(_)), "{err:?}");
    assert_eq!(fs.read("/once.txt").await.unwrap(), b"first");

    // A directory counts as "something exists".
    fs.mkdir_all("/dir").await.unwrap();
    assert!(matches!(
        fs.write_with("/dir", vec![], WriteOptions::new().if_absent())
            .await,
        Err(VfsError::AlreadyExists(_))
    ));
}

#[tokio::test]
async fn if_match_requires_the_current_etag() {
    let fs = MemoryFs::new();
    fs.write("/doc.txt", b"v1".to_vec()).await.unwrap();
    let old = fs.metadata("/doc.txt").await.unwrap().etag.unwrap();

    fs.write_with(
        "/doc.txt",
        b"v2".to_vec(),
        WriteOptions::new().if_match(old.clone()),
    )
    .await
    .unwrap();
    assert_eq!(fs.read("/doc.txt").await.unwrap(), b"v2");
    let current = fs.metadata("/doc.txt").await.unwrap().etag.unwrap();
    assert_ne!(current, old);

    // The old etag no longer matches.
    let err = fs
        .write_with(
            "/doc.txt",
            b"v3".to_vec(),
            WriteOptions::new().if_match(old),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, VfsError::PreconditionFailed(_)), "{err:?}");
    assert_eq!(fs.read("/doc.txt").await.unwrap(), b"v2");

    // A missing path never matches.
    let err = fs
        .write_with(
            "/missing.txt",
            vec![],
            WriteOptions::new().if_match(current),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, VfsError::PreconditionFailed(_)), "{err:?}");
    assert!(!fs.exists("/missing.txt").await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_if_absent_writers_have_one_winner() {
    let fs = MemoryFs::new();
    let tasks: Vec<_> = (0..16u8)
        .map(|i| {
            let fs = fs.clone();
            tokio::spawn(async move {
                fs.write_with("/race.txt", vec![i], WriteOptions::new().if_absent())
                    .await
            })
        })
        .collect();
    let mut winners = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(()) => winners += 1,
            Err(VfsError::AlreadyExists(_)) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
    assert_eq!(winners, 1);
    assert_eq!(fs.read("/race.txt").await.unwrap().len(), 1);
}

#[tokio::test]
async fn write_options_store_content_type_and_user_metadata() {
    let opts = WriteOptions::new()
        .content_type("text/csv")
        .user_metadata("owner", "reports")
        .user_metadata("run", "42");
    assert!(!opts.is_plain());
    assert!(WriteOptions::new().is_plain());
    assert_eq!(opts.precondition, None);
    assert_eq!(
        WriteOptions::new().if_match("e").precondition,
        Some(Precondition::IfMatch("e".to_string()))
    );

    let fs = MemoryFs::new();
    fs.write_with("/r.csv", b"a,b\n".to_vec(), opts)
        .await
        .unwrap();
    let meta = fs.metadata("/r.csv").await.unwrap();
    assert_eq!(meta.content_type.as_deref(), Some("text/csv"));
    assert_eq!(meta.user("owner"), Some("reports"));
    assert_eq!(meta.user("run"), Some("42"));
    assert_eq!(fs.open("/r.csv").await.unwrap().metadata(), &meta);

    // `append` keeps them, `copy` copies them.
    fs.append("/r.csv", b"c,d\n".to_vec()).await.unwrap();
    assert_eq!(
        fs.metadata("/r.csv").await.unwrap().content_type.as_deref(),
        Some("text/csv")
    );
    fs.copy("/r.csv", "/copy.csv").await.unwrap();
    assert_eq!(
        fs.metadata("/copy.csv").await.unwrap().user("owner"),
        Some("reports")
    );

    // A plain write replaces the file, extras included.
    fs.write("/r.csv", b"x".to_vec()).await.unwrap();
    let meta = fs.metadata("/r.csv").await.unwrap();
    assert_eq!(meta.content_type, None);
    assert!(meta.user_metadata.is_empty());
}

#[tokio::test]
async fn create_with_applies_options_on_finish() {
    let fs = MemoryFs::new();
    fs.write("/exists.txt", b"original".to_vec()).await.unwrap();

    // Creating the writer succeeds; the precondition is checked on finish.
    let mut writer = fs
        .create_with("/exists.txt", WriteOptions::new().if_absent())
        .await
        .unwrap();
    writer.write_all(b"replacement").await.unwrap();
    let err = writer.finish().await.unwrap_err();
    assert!(matches!(err, VfsError::AlreadyExists(_)), "{err:?}");
    assert_eq!(fs.read("/exists.txt").await.unwrap(), b"original");

    // The content type reaches the stored file.
    let mut writer = fs
        .create_with("/new.txt", WriteOptions::new().content_type("text/plain"))
        .await
        .unwrap();
    writer.write_all(b"hello").await.unwrap();
    assert_eq!(writer.finish().await.unwrap(), 5);
    let meta = fs.metadata("/new.txt").await.unwrap();
    assert_eq!(meta.content_type.as_deref(), Some("text/plain"));
}

#[tokio::test]
async fn the_default_backend_rejects_preconditions() {
    let bucket = FakeBucket::default();
    let err = bucket
        .write_with("fake://k", b"x".to_vec(), WriteOptions::new().if_absent())
        .await
        .unwrap_err();
    assert!(matches!(err, VfsError::Unsupported(_)), "{err:?}");
    assert!(matches!(
        bucket
            .write_with("fake://k", b"x".to_vec(), WriteOptions::new().if_match("e"))
            .await,
        Err(VfsError::Unsupported(_))
    ));
    assert!(bucket.is_empty(), "nothing is written on refusal");

    // Hints alone are accepted (and dropped by the default).
    bucket
        .write_with(
            "fake://k",
            b"x".to_vec(),
            WriteOptions::new().content_type("text/plain"),
        )
        .await
        .unwrap();
    assert_eq!(bucket.stored("k").as_deref(), Some(&b"x"[..]));
}

#[tokio::test]
async fn local_fs_rejects_preconditions() {
    let dir = TempDir::new("preconditions");
    let fs = LocalFs::rooted(&dir.0).unwrap();

    let err = fs
        .write_with("a.txt", b"x".to_vec(), WriteOptions::new().if_absent())
        .await
        .unwrap_err();
    assert!(matches!(err, VfsError::Unsupported(_)), "{err:?}");
    let err = fs
        .create_with("a.txt", WriteOptions::new().if_absent())
        .await
        .err()
        .expect("create_with refuses a precondition");
    assert!(matches!(err, VfsError::Unsupported(_)), "{err:?}");
    assert_eq!(dir.file_count(), 0);

    // Plain options (and hints) work.
    fs.write_with(
        "a.txt",
        b"whole".to_vec(),
        WriteOptions::new().content_type("text/plain"),
    )
    .await
    .unwrap();
    assert_eq!(fs.read("a.txt").await.unwrap(), b"whole");
    let mut writer = fs.create_with("b.txt", WriteOptions::new()).await.unwrap();
    writer.write_all(b"streamed").await.unwrap();
    writer.finish().await.unwrap();
    assert_eq!(fs.read("b.txt").await.unwrap(), b"streamed");
    assert_eq!(dir.file_count(), 2);
}

// ---- try_exists, signed URLs, downcasting (V7, V9) -------------------------

#[tokio::test]
async fn try_exists_reports_backend_failures() {
    let bucket = FakeBucket::default();
    bucket.write("fake://here", b"1".to_vec()).await.unwrap();

    bucket.unavailable.store(true, Ordering::SeqCst);
    assert!(!bucket.exists("fake://here").await);
    let err = bucket.try_exists("fake://here").await.unwrap_err();
    assert!(matches!(err, VfsError::Unavailable(_)), "{err:?}");
    assert!(err.is_retryable());

    bucket.unavailable.store(false, Ordering::SeqCst);
    assert!(!bucket.try_exists("fake://missing").await.unwrap());
    assert!(bucket.try_exists("fake://here").await.unwrap());

    // The bundled backends: absent is `Ok(false)`, a bad path is an error.
    let fs = MemoryFs::new();
    assert!(!fs.try_exists("/nope").await.unwrap());
    assert!(fs.try_exists("/").await.unwrap());
    assert!(matches!(
        fs.try_exists("s3://bucket/key").await,
        Err(VfsError::InvalidPath { .. })
    ));
}

#[tokio::test]
async fn signed_urls_default_to_unsupported() {
    let fs = MemoryFs::new();
    fs.write("/a.txt", b"x".to_vec()).await.unwrap();
    let err = fs
        .signed_url("/a.txt", SignedUrlMethod::Get, Duration::from_secs(60))
        .await
        .unwrap_err();
    assert!(matches!(err, VfsError::Unsupported(_)), "{err:?}");

    let bucket = FakeBucket::default();
    let url = bucket
        .signed_url("fake://k", SignedUrlMethod::Put, Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(url, "https://fake.example/k?sig=1");
}

#[test]
fn a_backend_can_be_downcast() {
    let mem: Arc<dyn Vfs> = Arc::new(MemoryFs::new());
    assert!(mem
        .as_any()
        .and_then(|a| a.downcast_ref::<MemoryFs>())
        .is_some());
    assert!(mem
        .as_any()
        .and_then(|a| a.downcast_ref::<LocalFs>())
        .is_none());

    let local: Arc<dyn Vfs> = Arc::new(LocalFs::new());
    assert!(local
        .as_any()
        .and_then(|a| a.downcast_ref::<LocalFs>())
        .is_some());

    let bucket: Arc<dyn Vfs> = Arc::new(FakeBucket::default());
    let concrete = bucket
        .as_any()
        .and_then(|a| a.downcast_ref::<FakeBucket>())
        .expect("FakeBucket opts in");
    assert!(concrete.is_empty());

    // The default opts out.
    assert!(BrokenSource.as_any().is_none());
}

// ---- registry (V6) ---------------------------------------------------------

#[tokio::test]
async fn registry_forwards_the_new_operations() {
    let mem = Arc::new(MemoryFs::new());
    let bucket = Arc::new(FakeBucket::default());
    let mut registry = VfsRegistry::new();
    registry.register(mem.clone());
    registry.register(bucket.clone());

    // read_range and append reach the memory backend.
    registry
        .write("mem://log.txt", b"hello".to_vec())
        .await
        .unwrap();
    registry
        .append("mem://log.txt", b" world".to_vec())
        .await
        .unwrap();
    assert_eq!(mem.read("/log.txt").await.unwrap(), b"hello world");
    assert_eq!(
        registry.read_range("mem://log.txt", 6, 5).await.unwrap(),
        b"world"
    );

    // create + finish reach the bucket's own streaming sink.
    let mut writer = registry.create("fake://up.bin").await.unwrap();
    assert_eq!(bucket.open_uploads.load(Ordering::SeqCst), 1);
    writer.write_all(b"streamed").await.unwrap();
    writer.finish().await.unwrap();
    assert_eq!(bucket.stored("up.bin").as_deref(), Some(&b"streamed"[..]));
    assert_eq!(bucket.open_uploads.load(Ordering::SeqCst), 0);

    // write_with and create_with carry their options.
    registry
        .write_with(
            "mem://typed.txt",
            b"t".to_vec(),
            WriteOptions::new().content_type("text/plain"),
        )
        .await
        .unwrap();
    assert_eq!(
        mem.metadata("/typed.txt")
            .await
            .unwrap()
            .content_type
            .as_deref(),
        Some("text/plain")
    );
    let mut writer = registry
        .create_with("mem://typed.txt", WriteOptions::new().if_absent())
        .await
        .unwrap();
    writer.write_all(b"again").await.unwrap();
    assert!(matches!(
        writer.finish().await,
        Err(VfsError::AlreadyExists(_))
    ));

    // list_page and walk reach the memory backend.
    let page = registry.list_page("mem://", None, 1).await.unwrap();
    assert_eq!(page.entries.len(), 1);
    assert!(page.next.is_some());
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = seen.clone();
    registry
        .walk(
            "mem://",
            Box::new(move |e| {
                sink.lock().unwrap().push(e.path.clone());
                WalkAction::Continue
            }),
        )
        .await
        .unwrap();
    assert_eq!(*seen.lock().unwrap(), ["/log.txt", "/typed.txt"]);

    // try_exists reaches the bucket, and reports routing failures.
    assert!(registry.try_exists("fake://up.bin").await.unwrap());
    assert!(!registry.try_exists("fake://nope").await.unwrap());
    assert!(matches!(
        registry.try_exists("s3://bucket/key").await,
        Err(VfsError::Unsupported(_))
    ));
    assert!(!registry.exists("s3://bucket/key").await);

    // signed_url reaches the right backend.
    assert_eq!(
        registry
            .signed_url(
                "fake://up.bin",
                SignedUrlMethod::Get,
                Duration::from_secs(1)
            )
            .await
            .unwrap(),
        "https://fake.example/up.bin?sig=1"
    );
    assert!(matches!(
        registry
            .signed_url(
                "mem://log.txt",
                SignedUrlMethod::Get,
                Duration::from_secs(1)
            )
            .await,
        Err(VfsError::Unsupported(_))
    ));
}

#[test]
fn resolve_path_shows_the_backend_and_path() {
    let mem: Arc<dyn Vfs> = Arc::new(MemoryFs::new());
    let scratch: Arc<dyn Vfs> = Arc::new(MemoryFs::new());
    let mut registry = VfsRegistry::new();
    registry.register(mem.clone());
    registry.register_scheme("scratch", scratch.clone());

    // The backend's own scheme: the full URL is passed through.
    let (backend, path) = registry.resolve_path("mem://a/b").unwrap();
    assert!(Arc::ptr_eq(backend, &mem));
    assert_eq!(path, "mem://a/b");

    // An alias scheme is removed before the backend sees the path.
    let (backend, path) = registry.resolve_path("scratch://a/b").unwrap();
    assert!(Arc::ptr_eq(backend, &scratch));
    assert_eq!(path, "a/b");

    // Nothing registered, and no default for a plain path.
    assert!(matches!(
        registry.resolve_path("s3://bucket/key"),
        Err(VfsError::Unsupported(_))
    ));
    assert!(matches!(
        registry.resolve_path("/plain"),
        Err(VfsError::Unsupported(_))
    ));
    registry.set_default("mem");
    let (backend, path) = registry.resolve_path("/plain").unwrap();
    assert!(Arc::ptr_eq(backend, &mem));
    assert_eq!(path, "/plain");
}

#[test]
fn strip_scheme_accepts_only_its_own_schemes() {
    assert_eq!(
        strip_scheme("fake://k", &["fake"], "FakeBucket").unwrap(),
        "k"
    );
    assert_eq!(
        strip_scheme("FAKE://k", &["fake"], "FakeBucket").unwrap(),
        "k"
    );
    assert_eq!(strip_scheme("k/v", &["fake"], "FakeBucket").unwrap(), "k/v");
    match strip_scheme("s3://k", &["fake"], "FakeBucket") {
        Err(VfsError::InvalidPath { path, reason }) => {
            assert_eq!(path, "s3://k");
            assert!(reason.contains("s3") && reason.contains("FakeBucket"));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

// ---- list_page contract (V5) -----------------------------------------------

#[tokio::test]
async fn list_page_cursor_round_trips() {
    let fs = MemoryFs::new();
    for name in ["d", "b", "e", "a", "c"] {
        fs.write(&format!("/{name}.txt"), vec![]).await.unwrap();
    }
    let mut seen = Vec::new();
    let mut after = None;
    let mut pages = 0;
    loop {
        let page = fs.list_page("/", after, 2).await.unwrap();
        pages += 1;
        assert!(page.entries.len() <= 2);
        seen.extend(page.entries.into_iter().map(|e| e.name));
        match page.next {
            // Handed back unchanged — never inspected.
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(seen.len(), 5, "every file exactly once: {seen:?}");
    assert_eq!(sorted, ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"]);
}

//! The content-addressed cache.

use std::collections::HashMap;
use std::future::Future;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::OwnedMutexGuard;

use ferroly::hash::Sha256;
use ferroly::http::{Client, Method, Request};

use super::fetch::{fetch_http, Fetched};
#[cfg(unix)]
use super::local::FILE_MODE;
use super::local::{min_opt, unique_name, Lease, READ_BUF};
use super::{
    Digest, Evicted, Layout, LocalStore, ObjectInfo, ObjectStatus, StoreError, StoreLock, Stored,
};

/// Reports download progress: the digest, the bytes on disk so far (a
/// resumed prefix included) and the expected total, when known.
type Progress = dyn Fn(&Digest, u64, Option<u64>) + Send + Sync;

/// The longest one blocking-thread attempt of `remove`, `evict_to` or
/// `verify` waits for the store lock before checking whether its caller is
/// still there (see `Cache::under_lock`).
const LOCK_SLICE: Duration = Duration::from_millis(50);

/// A content-addressed file cache: a directory of files keyed by SHA-256,
/// with async downloads that verify, resume and deduplicate.
///
/// `Cache` is the async layer over a [`LocalStore`] (see
/// [`local`](Cache::local) and [`from_local`](Cache::from_local)): the same
/// directory, the same files, usable from both sides at once. Layout under
/// the root (see [`Layout`]):
///
/// ```text
/// sha256/ab/abcdef0123…   committed objects, Layout::Fanout (the default;
///                         the file name is the full hex digest)
/// <prefix>/sha256/abcd…   committed objects, Layout::Flat { prefix }
/// tmp/                    downloads in progress, one uniquely named file each
/// partial/                interrupted downloads, parked for resumption
/// .lock                   the store lock (see `lock`), created on first use
/// ```
///
/// A committed object always holds exactly the bytes its digest names:
/// content is streamed into a file under `tmp/`, hashed as it arrives, and
/// renamed into the objects directory only after the digest (and the size,
/// when known) matched. A rename within one directory tree is atomic, so
/// readers never see a half-written object.
///
/// - **Hit** — [`get_or_fetch`](Cache::get_or_fetch) returns the existing
///   path without calling the fetch callback.
/// - **Resume** — a download that fails part-way (a dropped connection, a
///   timeout, the future being cancelled) is parked under `partial/`. The
///   next fetch of that digest hashes the parked prefix and asks the source
///   for the rest (`Range: bytes=<n>-`). If the source cannot honour the
///   range it sends the whole content and the cache starts over. A process
///   that dies mid-download (killed, out of memory, crashed) parks nothing:
///   its bytes stay in `tmp/` until [`purge_temp`](Cache::purge_temp)
///   removes them, and the next fetch starts from byte 0.
/// - **Mismatch** — content that hashes to something else, is larger than
///   declared or ends at the wrong size is deleted and reported as
///   [`StoreError::DigestMismatch`] / [`StoreError::SizeMismatch`]. When the
///   failing attempt had resumed a parked prefix, the cache first retries
///   once from byte 0, in case the prefix itself was stale.
/// - **Concurrency** — fetches of one digest through the same `Cache` (or
///   its clones) run one at a time: the second waits and then gets the hit.
///   Separate `Cache` values or processes on one directory may download
///   the same digest at the same time; each writes its own temporary file
///   and the first to finish commits it. Neither can corrupt the other.
/// - **Permissions** — on Unix the directories the cache creates are `0700`
///   and its files `0600` (before the umask), since cached models, layers
///   and objects are often private. An existing root keeps its permissions;
///   [`open`](Cache::open) removes group and other access from the
///   objects, `tmp/` and `partial/` directories it owns, so a cache
///   created by an older version (whose directories were `0755` and objects
///   `0644`) is no longer readable by others either (skipped where this user
///   may not change them).
///
/// - **Deletion** — [`remove`](Cache::remove), [`evict_to`](Cache::evict_to)
///   and the deletion inside [`verify`](Cache::verify) take the
///   cross-process [store lock](Cache::lock) when they have something to
///   delete, so they wait while another holder has it. A call dropped
///   while waiting deletes nothing.
///
/// `Cache` is cheap to clone; clones share their in-process coordination.
#[derive(Clone)]
pub struct Cache {
    local: LocalStore,
    inner: Arc<Inner>,
    stall_timeout: Option<Duration>,
    fetch_timeout: Option<Duration>,
    progress: Option<Arc<Progress>>,
}

struct Inner {
    inflight: Mutex<HashMap<[u8; 32], Slot>>,
}

/// The per-digest lock, with the number of tasks holding or waiting for it
/// (the entry is removed when the last one leaves, cancelled or not).
struct Slot {
    mutex: Arc<tokio::sync::Mutex<()>>,
    users: usize,
}

impl std::fmt::Debug for Cache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cache")
            .field("root", &self.local.root())
            .field("layout", self.local.layout())
            .field("max_object_bytes", &self.local.max_object_bytes())
            .field("stall_timeout", &self.stall_timeout)
            .field("fetch_timeout", &self.fetch_timeout)
            .field("progress", &self.progress.is_some())
            .finish()
    }
}

impl From<LocalStore> for Cache {
    /// [`Cache::from_local`].
    fn from(local: LocalStore) -> Cache {
        Cache::from_local(local)
    }
}

/// A place in the per-digest queue: holding the lock, or waiting for it.
/// Dropping it (released, or cancelled while waiting) wakes the next waiter
/// and removes the map entry once nobody else uses it.
struct Inflight<'a> {
    inner: &'a Inner,
    key: [u8; 32],
    guard: Option<OwnedMutexGuard<()>>,
}

impl Drop for Inflight<'_> {
    fn drop(&mut self) {
        drop(self.guard.take());
        let mut map = self
            .inner
            .inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = map.get_mut(&self.key) {
            slot.users = slot.users.saturating_sub(1);
            if slot.users == 0 {
                map.remove(&self.key);
            }
        }
    }
}

/// Why one download attempt failed, which decides what happens to the
/// bytes on disk.
enum Failure {
    /// The transfer broke off (or never started); what arrived is a valid
    /// prefix as far as we know. Keep it for resumption.
    Interrupted(StoreError),
    /// The content is wrong. Delete it (or retry from scratch once, if the
    /// attempt had resumed a parked prefix).
    Rejected(StoreError),
}

impl Cache {
    /// Opens (creating if needed) a cache rooted at `dir`, with the default
    /// [`Layout::Fanout`].
    pub fn open(dir: impl AsRef<Path>) -> Result<Cache, StoreError> {
        Cache::open_with(dir, Layout::Fanout)
    }

    /// Opens (creating if needed) a cache rooted at `dir` with `layout`
    /// (see [`LocalStore::open_with`]).
    ///
    /// **No migration is performed**: a cache written with one layout looks
    /// empty when opened with another (copy objects across with
    /// [`LocalStore::list`] and [`LocalStore::insert_path`]).
    pub fn open_with(dir: impl AsRef<Path>, layout: Layout) -> Result<Cache, StoreError> {
        Ok(Cache::from_local(LocalStore::open_with(dir, layout)?))
    }

    /// The async cache over an open [`LocalStore`] (sharing its directory,
    /// layout and [object size cap](LocalStore::with_max_object_bytes)),
    /// with no timeouts and no progress callback.
    pub fn from_local(local: LocalStore) -> Cache {
        Cache {
            local,
            inner: Arc::new(Inner {
                inflight: Mutex::new(HashMap::new()),
            }),
            stall_timeout: None,
            fetch_timeout: None,
            progress: None,
        }
    }

    /// The synchronous store under this cache (same directory, same objects).
    pub fn local(&self) -> &LocalStore {
        &self.local
    }

    /// Where committed objects live under the root.
    pub fn layout(&self) -> &Layout {
        self.local.layout()
    }

    /// Caps the size of any one object this cache will download (`None`, the
    /// default, means no cap). A larger download is stopped and deleted as
    /// soon as it passes the cap.
    ///
    /// Without a cap, a source that declares no size can fill the disk: set
    /// one when the sources are not trusted.
    ///
    /// The cap is shared with [`local`](Cache::local), so it also bounds
    /// the synchronous inserts made through it.
    pub fn with_max_object_bytes(mut self, max: Option<u64>) -> Cache {
        self.local = self.local.with_max_object_bytes(max);
        self
    }

    /// Stops a download when the source delivers nothing for `timeout`
    /// (`None`, the default, waits as long as the source does; an HTTP
    /// source is still bounded by its client's read timeout). What arrived
    /// is parked and the error is [`StoreError::Interrupted`], so a retry
    /// resumes. This frees the per-digest lock that other fetches of the
    /// same digest are queued behind.
    pub fn with_stall_timeout(mut self, timeout: Option<Duration>) -> Cache {
        self.stall_timeout = timeout;
        self
    }

    /// Bounds each miss of [`get_or_fetch`](Cache::get_or_fetch) and
    /// [`insert`](Cache::insert) — every attempt, from the first request to
    /// the last byte — by `timeout` (`None`, the default, means no bound).
    /// Time spent queued behind another fetch of the same digest does not
    /// count. A download cut off by it is parked and fails with
    /// [`StoreError::Interrupted`].
    pub fn with_fetch_timeout(mut self, timeout: Option<Duration>) -> Cache {
        self.fetch_timeout = timeout;
        self
    }

    /// Calls `progress(digest, bytes_on_disk, total)` after every piece a
    /// digest-keyed download writes, and once after a resumed prefix has
    /// been re-hashed. `total` is the expected size when the caller or the
    /// source declared one. Keep it quick: it runs on the download's task.
    pub fn with_progress(
        mut self,
        progress: impl Fn(&Digest, u64, Option<u64>) + Send + Sync + 'static,
    ) -> Cache {
        self.progress = Some(Arc::new(progress));
        self
    }

    /// The cache's root directory.
    pub fn root(&self) -> &Path {
        self.local.root()
    }

    /// Where the object for `digest` lives (or would live) in the cache.
    pub fn path_for(&self, digest: &Digest) -> PathBuf {
        self.local.path_for(digest)
    }

    fn partial_path(&self, digest: &Digest) -> PathBuf {
        self.local.partial_path(digest)
    }

    /// The path of the committed object for `digest`, if the cache has it.
    ///
    /// This only checks that the file exists; the content was verified when
    /// it was written. Use [`verify`](Cache::verify) or
    /// [`check`](Cache::check) to re-check it.
    pub fn get(&self, digest: &Digest) -> Option<PathBuf> {
        self.local.get(digest)
    }

    /// Whether the cache holds `digest`.
    pub fn contains(&self, digest: &Digest) -> bool {
        self.local.contains(digest)
    }

    /// The size of a parked, resumable partial download of `digest`, if any.
    pub fn partial_len(&self, digest: &Digest) -> Option<u64> {
        self.local.partial_len(digest)
    }

    /// Runs `f` on the store on a blocking thread (one hop for the whole
    /// operation).
    async fn blocking<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(LocalStore) -> Result<T, StoreError> + Send + 'static,
    {
        let local = self.local.clone();
        tokio::task::spawn_blocking(move || f(local))
            .await
            .map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))?
    }

    /// Runs a destructive operation that needs the store lock, without ever
    /// leaving a blocking thread waiting on the lock for an abandoned call.
    ///
    /// Each attempt is one blocking hop: `op` gets the store and an
    /// `acquire` function, which waits at most [`LOCK_SLICE`] for the lock
    /// and returns `None` if it is still held or this future was dropped
    /// meanwhile (the lock is then released at once). `op` returns
    /// `Ok(None)` to ask for another attempt. When the lock is free the
    /// whole operation is one hop; while another holder has it, each hop
    /// ends after a slice, so a dropped future deletes nothing and holds up
    /// runtime shutdown for at most one slice.
    async fn under_lock<T, F>(&self, op: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: Fn(
                &LocalStore,
                &dyn Fn() -> Result<Option<StoreLock>, StoreError>,
            ) -> Result<Option<T>, StoreError>
            + Send
            + Sync
            + 'static,
    {
        /// Marks the call abandoned when its future is dropped.
        struct Abandon(Arc<AtomicBool>);
        impl Drop for Abandon {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let op = Arc::new(op);
        let abandoned = Arc::new(AtomicBool::new(false));
        let _abandon = Abandon(Arc::clone(&abandoned));
        loop {
            let op = Arc::clone(&op);
            let abandoned = Arc::clone(&abandoned);
            let done = self
                .blocking(move |local| {
                    let acquire = || match local.lock_timeout(LOCK_SLICE) {
                        Ok(lock) if !abandoned.load(Ordering::SeqCst) => Ok(Some(lock)),
                        Ok(_) | Err(StoreError::Locked(_)) => Ok(None),
                        Err(e) => Err(e),
                    };
                    op(&local, &acquire)
                })
                .await?;
            if let Some(value) = done {
                return Ok(value);
            }
        }
    }

    /// Removes the object for `digest` (and any parked partial download).
    /// Returns whether an object was removed.
    ///
    /// Takes the [store lock](Cache::lock) when there is something to
    /// delete, so it waits while another holder (in this process or another)
    /// has it; with nothing to delete it returns `Ok(false)` without the
    /// lock ([`LocalStore::remove`], on a blocking thread). Under a lock you
    /// already hold, use [`StoreLock::remove`].
    ///
    /// **Cancellation**: the wait for the lock happens in short slices, not
    /// on a blocking thread parked until the holder lets go. If this future
    /// is dropped while waiting, nothing is deleted (now or later). Once the
    /// lock is taken the deletion runs to completion even if the future is
    /// dropped.
    pub async fn remove(&self, digest: &Digest) -> Result<bool, StoreError> {
        let digest = *digest;
        self.under_lock(move |local, acquire| {
            if !local.has_removable(&digest) {
                return Ok(Some(false));
            }
            match acquire()? {
                Some(lock) => lock.remove(&digest).map(Some),
                None => Ok(None),
            }
        })
        .await
    }

    /// Re-hashes the stored object for `digest`. A missing object is
    /// `Ok(false)`; a corrupted one (changed on disk since it was committed)
    /// is deleted and also `Ok(false)`.
    ///
    /// The deletion runs under the [store lock](Cache::lock), after checking
    /// again (the object may have been replaced by a good copy meanwhile;
    /// then the answer is `Ok(true)`). Use [`check`](Cache::check) to
    /// re-hash without deleting anything.
    ///
    /// **Cancellation**: as for [`remove`](Cache::remove), the wait for the
    /// lock happens in short slices; if this future is dropped while
    /// waiting, the corrupted object is not deleted.
    pub async fn verify(&self, digest: &Digest) -> Result<bool, StoreError> {
        let digest = *digest;
        self.under_lock(move |local, acquire| match local.check(&digest)? {
            ObjectStatus::Intact { .. } => Ok(Some(true)),
            ObjectStatus::Corrupt { .. } => match acquire()? {
                Some(lock) => {
                    let status = lock.repair(&digest)?;
                    Ok(Some(matches!(status, ObjectStatus::Intact { .. })))
                }
                None => Ok(None),
            },
            _ => Ok(Some(false)),
        })
        .await
    }

    /// Every committed object, sorted by digest ([`LocalStore::list`] on a
    /// blocking thread).
    pub async fn list(&self) -> Result<Vec<ObjectInfo>, StoreError> {
        self.blocking(|local| local.list()).await
    }

    /// Resolves a short digest to the one stored digest that starts with it
    /// ([`LocalStore::resolve_prefix`] on a blocking thread).
    pub async fn resolve_prefix(&self, prefix: &str) -> Result<Digest, StoreError> {
        let prefix = prefix.to_owned();
        self.blocking(move |local| local.resolve_prefix(&prefix))
            .await
    }

    /// [`resolve_prefix`](Cache::resolve_prefix) with another minimum number
    /// of hex digits ([`LocalStore::resolve_prefix_min`] on a blocking
    /// thread).
    pub async fn resolve_prefix_min(
        &self,
        prefix: &str,
        min_hex: usize,
    ) -> Result<Digest, StoreError> {
        let prefix = prefix.to_owned();
        self.blocking(move |local| local.resolve_prefix_min(&prefix, min_hex))
            .await
    }

    /// Re-hashes the object for `digest` without modifying anything
    /// ([`LocalStore::check`] on a blocking thread).
    pub async fn check(&self, digest: &Digest) -> Result<ObjectStatus, StoreError> {
        let digest = *digest;
        self.blocking(move |local| local.check(&digest)).await
    }

    /// Re-hashes every committed object without modifying anything
    /// ([`LocalStore::check_all`] on a blocking thread).
    pub async fn check_all(&self) -> Result<Vec<(Digest, ObjectStatus)>, StoreError> {
        self.blocking(|local| local.check_all()).await
    }

    /// Takes the exclusive, cross-process store lock, waiting (on a blocking
    /// thread) until it is free. See [`LocalStore::lock`] for what it
    /// excludes and the intended registry pattern.
    ///
    /// Do not call [`remove`](Cache::remove), [`evict_to`](Cache::evict_to)
    /// or [`verify`](Cache::verify) on this store while holding it: they
    /// would wait for it. Use the guard's methods instead. If this future is
    /// dropped while waiting, the lock is taken and released again in the
    /// background once it comes free.
    pub async fn lock(&self) -> Result<StoreLock, StoreError> {
        self.blocking(|local| local.lock()).await
    }

    /// Returns the path of `digest`, fetching it first on a miss.
    ///
    /// `fetch` is called with the byte offset to start from (`0`, or the
    /// size of a parked partial download) and returns the content as a
    /// [`Fetched`]. It is not called at all on a hit, and is called at most
    /// twice per miss (the second time from offset `0`, when a resumed
    /// attempt was rejected).
    ///
    /// ```no_run
    /// use ferroly::http::{Client, Method, Request};
    /// use ferroly::store::{fetch_http, Cache, Digest};
    ///
    /// # async fn demo(digest: Digest) -> Result<(), ferroly::store::StoreError> {
    /// let cache = Cache::open("cache")?;
    /// let client = Client::new();
    /// let req = Request::builder(Method::Get, "https://example.com/model.bin")?
    ///     .header("authorization", "Bearer …")
    ///     .build();
    /// let path = cache
    ///     .get_or_fetch(&digest, |offset| fetch_http(&client, &req, offset))
    ///     .await?;
    /// # let _ = path; Ok(())
    /// # }
    /// ```
    pub async fn get_or_fetch<F, Fut>(
        &self,
        digest: &Digest,
        fetch: F,
    ) -> Result<PathBuf, StoreError>
    where
        F: FnMut(u64) -> Fut,
        Fut: Future<Output = Result<Fetched, StoreError>>,
    {
        self.fetch_verified(digest, None, fetch).await
    }

    /// [`get_or_fetch`](Cache::get_or_fetch) for content of a known size
    /// (an OCI descriptor's `size`, an `X-Linked-Size`): a body that runs
    /// past `size` is stopped as soon as it does, and one that ends at any
    /// other size is rejected.
    pub async fn get_or_fetch_sized<F, Fut>(
        &self,
        digest: &Digest,
        size: u64,
        fetch: F,
    ) -> Result<PathBuf, StoreError>
    where
        F: FnMut(u64) -> Fut,
        Fut: Future<Output = Result<Fetched, StoreError>>,
    {
        self.fetch_verified(digest, Some(size), fetch).await
    }

    /// Fetches a plain `GET` URL into the cache (see
    /// [`get_or_fetch`](Cache::get_or_fetch)). For requests that need
    /// headers, build a [`Request`] and use [`fetch_http`].
    pub async fn fetch_url(
        &self,
        client: &Client,
        url: &str,
        digest: &Digest,
    ) -> Result<PathBuf, StoreError> {
        let req = Request::builder(Method::Get, url)?.build();
        self.get_or_fetch(digest, |offset| fetch_http(client, &req, offset))
            .await
    }

    /// Stores content whose digest is not known in advance: streams it into
    /// the cache, hashing as it goes, and commits it under the digest it
    /// turns out to have. (There is nothing to resume or verify against, so
    /// an interrupted insert starts over.)
    pub async fn insert<F, Fut>(&self, fetch: F) -> Result<Stored, StoreError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Fetched, StoreError>>,
    {
        let deadline = self.deadline();
        let mut tmp = TempFile::create(self.tmp_dir(), "insert").await?;
        let mut fetched = self.bounded(deadline, false, fetch()).await?;
        if fetched.start != 0 {
            return Err(StoreError::Protocol(format!(
                "insert source started at byte {} instead of 0",
                fetched.start
            )));
        }
        let limit = min_opt(fetched.total, self.local.max_object_bytes());
        while let Some(chunk) = self
            .bounded(deadline, true, fetched.body.next_chunk())
            .await?
        {
            if let Some(limit) = limit {
                if tmp.len + chunk.len() as u64 > limit {
                    return Err(StoreError::SizeMismatch(format!(
                        "content exceeds {limit} bytes"
                    )));
                }
            }
            tmp.write(&chunk).await?;
        }
        if let Some(total) = fetched.total {
            if tmp.len != total {
                return Err(StoreError::Interrupted(format!(
                    "content ended at {} of {total} bytes",
                    tmp.len
                )));
            }
        }
        tmp.finish().await?;
        let digest = Digest::from(tmp.hasher.clone().finalize());
        let _lock = self.lock_digest(&digest).await;
        let path = self.commit(tmp, &digest).await?;
        Ok(Stored { digest, path })
    }

    /// Stores in-memory content and returns its digest and path.
    pub async fn insert_bytes(&self, data: impl Into<Vec<u8>>) -> Result<Stored, StoreError> {
        let data = data.into();
        self.insert(move || std::future::ready(Ok(Fetched::from_bytes(data))))
            .await
    }

    /// Deletes temporary and parked partial files last modified more than
    /// `older_than` ago — leftovers of processes that died mid-download
    /// (their bytes are never resumed). Returns how many files were removed.
    /// Committed objects are never touched.
    ///
    /// A download running in this process is never removed, whatever its
    /// age. A download running in another process is judged by its
    /// modification time: claiming a parked partial refreshes it, and every
    /// write after that updates it, so use an age well above the longest a
    /// source may stall.
    pub async fn purge_temp(&self, older_than: Duration) -> Result<usize, StoreError> {
        self.blocking(move |local| local.purge_temp(older_than))
            .await
    }

    /// The total size of the committed objects, in bytes.
    pub async fn usage(&self) -> Result<u64, StoreError> {
        self.blocking(|local| local.usage()).await
    }

    /// Deletes committed objects, oldest first (by modification time, which
    /// is when an object was committed unless something touched it since),
    /// until the rest hold at most `max_bytes`. Temporary and partial files
    /// are not counted (see [`purge_temp`](Cache::purge_temp)). An object
    /// that cannot be deleted (open elsewhere, on Windows) is skipped.
    ///
    /// Takes the [store lock](Cache::lock) when it has something to delete,
    /// so it waits while another holder has it, and lists the objects again
    /// under the lock; when the objects already fit it returns without the
    /// lock ([`LocalStore::evict_to`], on a blocking thread). Under a lock
    /// you already hold, use [`StoreLock::evict_to`]. A path the cache
    /// returned earlier stops existing when its object is evicted; open
    /// files keep working on Unix.
    ///
    /// **Cancellation**: as for [`remove`](Cache::remove), the wait for the
    /// lock happens in short slices; if this future is dropped while
    /// waiting, nothing is evicted.
    pub async fn evict_to(&self, max_bytes: u64) -> Result<Evicted, StoreError> {
        self.under_lock(move |local, acquire| {
            if let Some(fits) = local.evict_unneeded(max_bytes)? {
                return Ok(Some(fits));
            }
            match acquire()? {
                Some(lock) => lock.evict_to(max_bytes).map(Some),
                None => Ok(None),
            }
        })
        .await
    }

    fn tmp_dir(&self) -> PathBuf {
        self.local.tmp_dir()
    }

    fn deadline(&self) -> Option<Instant> {
        self.fetch_timeout
            .and_then(|t| Instant::now().checked_add(t))
    }

    /// Runs `fut` within the fetch deadline and, for a body read
    /// (`stall`), the stall timeout.
    async fn bounded<T>(
        &self,
        deadline: Option<Instant>,
        stall: bool,
        fut: impl Future<Output = Result<T, StoreError>>,
    ) -> Result<T, StoreError> {
        let stall = if stall { self.stall_timeout } else { None };
        let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        let limit = match (stall, left) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let Some(limit) = limit else {
            return fut.await;
        };
        match tokio::time::timeout(limit, fut).await {
            Ok(result) => result,
            Err(_) if left.is_some_and(|l| l <= limit) => Err(StoreError::Interrupted(
                "the fetch deadline passed".to_string(),
            )),
            Err(_) => Err(StoreError::Interrupted(format!(
                "the source sent nothing for {limit:?}"
            ))),
        }
    }

    async fn lock_digest(&self, digest: &Digest) -> Inflight<'_> {
        let key = *digest.as_bytes();
        let mutex = {
            let mut map = self
                .inner
                .inflight
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let slot = map.entry(key).or_insert_with(|| Slot {
                mutex: Arc::default(),
                users: 0,
            });
            slot.users += 1;
            slot.mutex.clone()
        };
        // Registered before waiting, so a waiter cancelled here still
        // leaves the queue.
        let mut place = Inflight {
            inner: &self.inner,
            key,
            guard: None,
        };
        place.guard = Some(mutex.lock_owned().await);
        place
    }

    async fn fetch_verified<F, Fut>(
        &self,
        digest: &Digest,
        size: Option<u64>,
        mut fetch: F,
    ) -> Result<PathBuf, StoreError>
    where
        F: FnMut(u64) -> Fut,
        Fut: Future<Output = Result<Fetched, StoreError>>,
    {
        if let Some(path) = self.get(digest) {
            return Ok(path);
        }
        if let (Some(size), Some(max)) = (size, self.local.max_object_bytes()) {
            if size > max {
                return Err(StoreError::SizeMismatch(format!(
                    "{digest} is {size} bytes, over the {max}-byte cap"
                )));
            }
        }
        let _lock = self.lock_digest(digest).await;
        // Another task may have committed it while we waited.
        if let Some(path) = self.get(digest) {
            return Ok(path);
        }

        let deadline = self.deadline();
        let mut tmp = TempFile::create(self.tmp_dir(), &digest.hex()).await?;
        tmp.park_to = Some(self.partial_path(digest));
        tmp.claim_partial().await?;
        if tmp.len > 0 {
            self.report(digest, tmp.len, size);
        }

        let mut may_restart = true;
        loop {
            match self
                .attempt(&mut tmp, digest, size, deadline, &mut fetch)
                .await
            {
                Ok(()) => break,
                Err(Failure::Interrupted(e)) => {
                    tmp.park().await;
                    return Err(e);
                }
                Err(Failure::Rejected(e)) => {
                    // Only when the rejected content still began with the
                    // parked prefix (the source did not start it over).
                    if tmp.prefix > 0 && may_restart {
                        // The parked prefix may be what was wrong.
                        may_restart = false;
                        tmp.reset().await?;
                        continue;
                    }
                    tmp.discard().await;
                    return Err(e);
                }
            }
        }
        tmp.finish().await?;
        self.commit(tmp, digest).await
    }

    fn report(&self, digest: &Digest, len: u64, total: Option<u64>) {
        if let Some(progress) = &self.progress {
            progress(digest, len, total);
        }
    }

    /// One download attempt into `tmp`, continuing from `tmp.len`.
    async fn attempt<F, Fut>(
        &self,
        tmp: &mut TempFile,
        digest: &Digest,
        size: Option<u64>,
        deadline: Option<Instant>,
        fetch: &mut F,
    ) -> Result<(), Failure>
    where
        F: FnMut(u64) -> Fut,
        Fut: Future<Output = Result<Fetched, StoreError>>,
    {
        use Failure::{Interrupted, Rejected};

        let offset = tmp.len;
        let mut fetched = self
            .bounded(deadline, false, fetch(offset))
            .await
            .map_err(Interrupted)?;
        if fetched.start != offset {
            if fetched.start == 0 {
                // The source ignored the range: start over.
                tmp.reset().await.map_err(Interrupted)?;
            } else {
                return Err(Rejected(StoreError::Protocol(format!(
                    "source resumed at byte {} instead of {offset}",
                    fetched.start
                ))));
            }
        }
        if let (Some(total), Some(size)) = (fetched.total, size) {
            if total != size {
                return Err(Rejected(StoreError::SizeMismatch(format!(
                    "source reports {total} bytes, expected {size}"
                ))));
            }
        }
        if let Some(total) = fetched.total {
            if total < tmp.len {
                return Err(Rejected(StoreError::SizeMismatch(format!(
                    "source reports {total} bytes, but {} are already on disk",
                    tmp.len
                ))));
            }
        }
        let expected = size.or(fetched.total);
        let limit = min_opt(min_opt(size, fetched.total), self.local.max_object_bytes());
        loop {
            let chunk = match self
                .bounded(deadline, true, fetched.body.next_chunk())
                .await
            {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(e) => return Err(Interrupted(e)),
            };
            if let Some(limit) = limit {
                if tmp.len + chunk.len() as u64 > limit {
                    return Err(Rejected(StoreError::SizeMismatch(format!(
                        "{digest}: content exceeds {limit} bytes"
                    ))));
                }
            }
            tmp.write(&chunk).await.map_err(Interrupted)?;
            if !chunk.is_empty() {
                self.report(digest, tmp.len, expected);
            }
        }
        if let Some(total) = fetched.total {
            if tmp.len < total {
                return Err(Interrupted(StoreError::Interrupted(format!(
                    "{digest}: transfer ended at {} of {total} bytes",
                    tmp.len
                ))));
            }
        }
        if let Some(size) = size {
            if tmp.len != size {
                return Err(Rejected(StoreError::SizeMismatch(format!(
                    "{digest}: got {} bytes, expected {size}",
                    tmp.len
                ))));
            }
        }
        let actual = Digest::from(tmp.hasher.clone().finalize());
        if !actual.ct_eq(digest) {
            return Err(Rejected(StoreError::DigestMismatch {
                expected: digest.to_string(),
                actual: actual.to_string(),
            }));
        }
        Ok(())
    }

    /// Moves a verified temporary file to its final place, with the rules
    /// of [`LocalStore`]'s commit (shared with the synchronous inserts), on
    /// a blocking thread.
    async fn commit(&self, mut tmp: TempFile, digest: &Digest) -> Result<PathBuf, StoreError> {
        let src = tmp.path.clone();
        // Keeps `purge_temp` off the file even if this future is cancelled
        // while the blocking commit runs.
        let lease = Lease::new(&src);
        let digest = *digest;
        // From here the blocking commit owns the file: a cancelled caller
        // must not park or delete it under it.
        tmp.committed = true;
        let result = self
            .blocking(move |local| {
                let _lease = lease;
                local.commit_file(&src, &digest)
            })
            .await;
        if result.is_err() && tmp.path.exists() {
            // Failed before the rename: let `Drop` park or delete it.
            tmp.committed = false;
        }
        result
    }
}

async fn remove_if_exists(path: &Path) -> std::io::Result<bool> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

async fn hash_reader(file: &mut tokio::fs::File, hasher: &mut Sha256) -> std::io::Result<u64> {
    let mut buf = vec![0u8; READ_BUF];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            return Ok(total);
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
}

/// Sets a file's modification time to now.
fn touch(path: &Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .set_modified(SystemTime::now())
}

/// A download in progress: a uniquely named file under `tmp/` and the hash
/// of what has been written to it.
///
/// Dropped without being committed (an error, or the fetch future being
/// cancelled), it is parked for resumption when it has a `park_to` path and
/// holds data, and deleted otherwise.
struct TempFile {
    path: PathBuf,
    file: Option<tokio::fs::File>,
    len: u64,
    hasher: Sha256,
    park_to: Option<PathBuf>,
    /// The file is a parked partial this fetch claimed (and has not reset):
    /// it holds resumable bytes even before `len` says so.
    claimed: bool,
    /// How many of the file's bytes came from the claimed partial (0 once
    /// the download starts over).
    prefix: u64,
    committed: bool,
    /// Released after `Drop` has parked or deleted the file.
    _lease: Lease,
}

impl TempFile {
    /// Creates a new, empty file with a unique name (`create_new`, so two
    /// fetches can never share one).
    async fn create(dir: PathBuf, prefix: &str) -> Result<TempFile, StoreError> {
        let mut attempts = 0;
        loop {
            let path = dir.join(unique_name(prefix));
            let lease = Lease::new(&path);
            let mut options = tokio::fs::OpenOptions::new();
            options.read(true).write(true).create_new(true);
            #[cfg(unix)]
            options.mode(FILE_MODE);
            match options.open(&path).await {
                Ok(file) => {
                    return Ok(TempFile {
                        path,
                        file: Some(file),
                        len: 0,
                        hasher: Sha256::new(),
                        park_to: None,
                        claimed: false,
                        prefix: 0,
                        committed: false,
                        _lease: lease,
                    })
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists && attempts < 16 => attempts += 1,
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Takes over a parked partial download, if there is one: it is renamed
    /// onto this file (so exactly one fetch can claim it) and its content
    /// hashed, ready to be appended to.
    async fn claim_partial(&mut self) -> Result<(), StoreError> {
        let Some(parked) = self.park_to.clone() else {
            return Ok(());
        };
        // Refresh its modification time first: once it is under `tmp/`, a
        // `purge_temp` in another process judges it by that time.
        match touch(&parked) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        // Close our handle first: Windows cannot replace an open file.
        drop(self.file.take());
        // A plain (blocking) rename: one system call, and no await between
        // the claim and `claimed`, so a cancellation always parks it again.
        match std::fs::rename(&parked, &self.path) {
            Ok(()) => self.claimed = true,
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let mut file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .await?;
        // Record the size first, so a cancellation while hashing parks the
        // claimed prefix again instead of deleting it.
        self.len = file.metadata().await?.len();
        let mut hasher = Sha256::new();
        self.len = hash_reader(&mut file, &mut hasher).await?;
        self.hasher = hasher;
        self.prefix = self.len;
        file.seek(std::io::SeekFrom::Start(self.len)).await?;
        self.file = Some(file);
        Ok(())
    }

    fn file(&mut self) -> std::io::Result<&mut tokio::fs::File> {
        self.file
            .as_mut()
            .ok_or_else(|| std::io::Error::other("temporary file is closed"))
    }

    async fn write(&mut self, chunk: &[u8]) -> Result<(), StoreError> {
        self.file()?.write_all(chunk).await?;
        self.hasher.update(chunk);
        self.len += chunk.len() as u64;
        Ok(())
    }

    /// Empties the file, to start the download over.
    async fn reset(&mut self) -> Result<(), StoreError> {
        let file = self.file()?;
        file.flush().await?;
        file.set_len(0).await?;
        file.seek(std::io::SeekFrom::Start(0)).await?;
        self.len = 0;
        self.prefix = 0;
        self.claimed = false;
        self.hasher = Sha256::new();
        Ok(())
    }

    /// Flushes and syncs the content, and closes the file (ready to rename).
    async fn finish(&mut self) -> Result<(), StoreError> {
        let file = self.file()?;
        file.flush().await?;
        file.sync_all().await?;
        drop(self.file.take());
        Ok(())
    }

    /// Closes the file and moves it to `partial/` for a later resume (or
    /// deletes it when it is empty or a longer partial is already parked).
    async fn park(&mut self) {
        if let Some(mut file) = self.file.take() {
            let _ = file.flush().await;
        }
        if let Some(parked) = self.park_to.clone() {
            let longer = tokio::fs::metadata(&parked)
                .await
                .map(|m| m.len() >= self.len)
                .unwrap_or(false);
            if self.len > 0 && !longer && tokio::fs::rename(&self.path, &parked).await.is_ok() {
                self.committed = true;
                return;
            }
        }
        self.discard().await;
    }

    /// Closes and deletes the file.
    async fn discard(&mut self) {
        drop(self.file.take());
        let _ = remove_if_exists(&self.path).await;
        self.committed = true;
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        // Cancelled mid-download: keep what arrived when it can be resumed.
        drop(self.file.take());
        if let Some(parked) = &self.park_to {
            if (self.len > 0 || self.claimed) && std::fs::rename(&self.path, parked).is_ok() {
                return;
            }
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::store::local::{OBJECTS, PARTIAL, TMP};

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ferroly-cache-unit-{tag}-{}-{}",
            std::process::id(),
            unique_name("d")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn inflight_len(cache: &Cache) -> usize {
        cache.inner.inflight.lock().unwrap().len()
    }

    #[tokio::test]
    async fn cancelled_waiter_leaves_no_inflight_entry() {
        // ST-9: a waiter cancelled after the holder released used to leave
        // its map entry behind for good.
        let dir = scratch("waiter");
        let cache = Cache::open(&dir).unwrap();
        let digest = Digest::of(b"queued");
        let holder = cache.lock_digest(&digest).await;
        let waiter_cache = cache.clone();
        let waiter = tokio::spawn(async move {
            let _place = waiter_cache.lock_digest(&digest).await;
            std::future::pending::<()>().await;
        });
        // Let the waiter queue up behind the holder.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(inflight_len(&cache), 1);
        // Release first, then cancel the waiter before it runs again.
        drop(holder);
        waiter.abort();
        let _ = waiter.await;
        assert_eq!(inflight_len(&cache), 0);

        // A waiter cancelled while the holder still holds.
        let holder = cache.lock_digest(&digest).await;
        let waiter_cache = cache.clone();
        let waiter = tokio::spawn(async move {
            let _place = waiter_cache.lock_digest(&digest).await;
        });
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        waiter.abort();
        let _ = waiter.await;
        assert_eq!(inflight_len(&cache), 1);
        drop(holder);
        assert_eq!(inflight_len(&cache), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_claimed_partial_is_parked_again_when_dropped() {
        // ST-10: dropped right after the claim (before its length is known),
        // the claimed bytes go back to `partial/` instead of being deleted.
        let dir = scratch("claim");
        let cache = Cache::open(&dir).unwrap();
        let digest = Digest::of(b"whatever");
        std::fs::write(cache.partial_path(&digest), b"prefix").unwrap();
        let mut tmp = TempFile::create(cache.tmp_dir(), &digest.hex())
            .await
            .unwrap();
        tmp.park_to = Some(cache.partial_path(&digest));
        drop(tmp.file.take());
        std::fs::rename(cache.partial_path(&digest), &tmp.path).unwrap();
        tmp.claimed = true;
        assert_eq!(tmp.len, 0);
        drop(tmp);
        assert_eq!(cache.partial_len(&digest), Some(6));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn files_and_directories_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("perm");
        let cache = Cache::open(&dir).unwrap();
        let stored = cache
            .insert_bytes(b"secret weights".to_vec())
            .await
            .unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        for sub in [OBJECTS, TMP, PARTIAL] {
            assert_eq!(mode(&dir.join(sub)) & 0o077, 0, "{sub}");
        }
        assert_eq!(mode(stored.path.parent().unwrap()) & 0o077, 0);
        assert_eq!(mode(&stored.path) & 0o077, 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// SEC-12: a cache whose directories were created open (by an older
    /// version) is tightened when it is opened.
    #[cfg(unix)]
    #[test]
    fn opening_an_old_cache_makes_its_directories_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("perm-old");
        let set = |p: &Path, m: u32| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap()
        };
        for sub in [OBJECTS, TMP, PARTIAL] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
            set(&dir.join(sub), 0o755);
        }
        set(&dir, 0o755);
        Cache::open(&dir).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        for sub in [OBJECTS, TMP, PARTIAL] {
            assert_eq!(mode(&dir.join(sub)), 0o700, "{sub}");
        }
        // The root is the caller's; it keeps its permissions.
        assert_eq!(mode(&dir), 0o755);
        let _ = std::fs::remove_dir_all(dir);
    }
}

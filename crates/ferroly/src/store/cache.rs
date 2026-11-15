//! The content-addressed cache.

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsString;
use std::future::Future;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::OwnedMutexGuard;

use ferroly::hash::Sha256;
use ferroly::http::{Client, Method, Request};

use super::fetch::{fetch_http, Fetched};
use super::{Digest, StoreError};

/// Directory holding the committed objects, per algorithm.
const OBJECTS: &str = "sha256";
/// Directory holding in-progress downloads (one unique file per fetch).
const TMP: &str = "tmp";
/// Directory holding interrupted downloads waiting to be resumed.
const PARTIAL: &str = "partial";

/// Read buffer used when hashing a file already on disk.
const READ_BUF: usize = 64 * 1024;

/// Permissions of the directories the cache creates (Unix).
#[cfg(unix)]
const DIR_MODE: u32 = 0o700;
/// Permissions of the files the cache writes (Unix).
#[cfg(unix)]
const FILE_MODE: u32 = 0o600;

/// Temporary files a fetch in this process is writing, by file name (the
/// names are unique per process). [`Cache::purge_temp`] never deletes one of
/// these, however old it looks: a resumed partial keeps the modification
/// time it was parked with until it is written to.
static LIVE_TEMP: Mutex<BTreeSet<OsString>> = Mutex::new(BTreeSet::new());

/// Reports download progress: the digest, the bytes on disk so far (a
/// resumed prefix included) and the expected total, when known.
type Progress = dyn Fn(&Digest, u64, Option<u64>) + Send + Sync;

/// A file committed to the cache: its digest and where it is on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Stored {
    /// The SHA-256 of the file's content.
    pub digest: Digest,
    /// The file's path inside the cache.
    pub path: PathBuf,
}

/// What [`Cache::evict_to`] removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Evicted {
    /// Objects deleted.
    pub removed: usize,
    /// Bytes those objects held.
    pub freed: u64,
    /// Bytes the remaining objects hold.
    pub remaining: u64,
}

/// A content-addressed file cache: a directory of files keyed by SHA-256.
///
/// Layout under the root:
///
/// ```text
/// sha256/ab/abcdef0123…   committed objects (the file name is the full hex digest)
/// tmp/                    downloads in progress, one uniquely named file each
/// partial/                interrupted downloads, parked for resumption
/// ```
///
/// A committed object always holds exactly the bytes its digest names:
/// content is streamed into a file under `tmp/`, hashed as it arrives, and
/// renamed into `sha256/` only after the digest (and the size, when known)
/// matched. A rename within one directory tree is atomic, so readers never
/// see a half-written object.
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
///   `sha256/`, `tmp/` and `partial/` directories it owns, so a cache
///   created by an older version (whose directories were `0755` and objects
///   `0644`) is no longer readable by others either (skipped where this user
///   may not change them).
///
/// `Cache` is cheap to clone; clones share their in-process coordination.
#[derive(Clone)]
pub struct Cache {
    inner: Arc<Inner>,
    max_object_bytes: Option<u64>,
    stall_timeout: Option<Duration>,
    fetch_timeout: Option<Duration>,
    progress: Option<Arc<Progress>>,
}

struct Inner {
    root: PathBuf,
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
            .field("root", &self.inner.root)
            .field("max_object_bytes", &self.max_object_bytes)
            .field("stall_timeout", &self.stall_timeout)
            .field("fetch_timeout", &self.fetch_timeout)
            .field("progress", &self.progress.is_some())
            .finish()
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

/// Creates `dir` and its missing parents, private to the owner on Unix.
fn create_dirs(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(DIR_MODE);
    }
    builder.create(dir)
}

/// Tightens one of the cache's own directories to `0700` on Unix, for a
/// cache created by an older version (or under a lax umask) whose
/// directories are readable by others. A symlink is left alone (never chmod
/// through it), and so is a directory this user may not change (a shared
/// cache owned by someone else).
fn make_private(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::symlink_metadata(dir)?;
        if !meta.is_dir() || meta.permissions().mode() & 0o077 == 0 {
            return Ok(());
        }
        let mode = meta.permissions().mode() & !0o077;
        match std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)) {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Ok(()),
            other => other,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

async fn create_dirs_async(dir: &Path) -> std::io::Result<()> {
    let mut builder = tokio::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(DIR_MODE);
    builder.create(dir).await
}

impl Cache {
    /// Opens (creating if needed) a cache rooted at `dir`.
    pub fn open(dir: impl AsRef<Path>) -> Result<Cache, StoreError> {
        let root = dir.as_ref().to_path_buf();
        for sub in [OBJECTS, TMP, PARTIAL] {
            let dir = root.join(sub);
            create_dirs(&dir)?;
            make_private(&dir)?;
        }
        Ok(Cache {
            inner: Arc::new(Inner {
                root,
                inflight: Mutex::new(HashMap::new()),
            }),
            max_object_bytes: None,
            stall_timeout: None,
            fetch_timeout: None,
            progress: None,
        })
    }

    /// Caps the size of any one object this cache will download (`None`, the
    /// default, means no cap). A larger download is stopped and deleted as
    /// soon as it passes the cap.
    ///
    /// Without a cap, a source that declares no size can fill the disk: set
    /// one when the sources are not trusted.
    pub fn with_max_object_bytes(mut self, max: Option<u64>) -> Cache {
        self.max_object_bytes = max;
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
        &self.inner.root
    }

    /// Where the object for `digest` lives (or would live) in the cache.
    pub fn path_for(&self, digest: &Digest) -> PathBuf {
        let hex = digest.hex();
        self.inner.root.join(OBJECTS).join(&hex[..2]).join(hex)
    }

    fn partial_path(&self, digest: &Digest) -> PathBuf {
        self.inner
            .root
            .join(PARTIAL)
            .join(format!("{}-{}", OBJECTS, digest.hex()))
    }

    /// The path of the committed object for `digest`, if the cache has it.
    ///
    /// This only checks that the file exists; the content was verified when
    /// it was written. Use [`verify`](Cache::verify) to re-check it.
    pub fn get(&self, digest: &Digest) -> Option<PathBuf> {
        let path = self.path_for(digest);
        match std::fs::metadata(&path) {
            Ok(m) if m.is_file() => Some(path),
            _ => None,
        }
    }

    /// Whether the cache holds `digest`.
    pub fn contains(&self, digest: &Digest) -> bool {
        self.get(digest).is_some()
    }

    /// The size of a parked, resumable partial download of `digest`, if any.
    pub fn partial_len(&self, digest: &Digest) -> Option<u64> {
        std::fs::metadata(self.partial_path(digest))
            .ok()
            .map(|m| m.len())
    }

    /// Removes the object for `digest` (and any parked partial download).
    /// Returns whether an object was removed.
    pub async fn remove(&self, digest: &Digest) -> Result<bool, StoreError> {
        let _ = remove_if_exists(&self.partial_path(digest)).await;
        Ok(remove_if_exists(&self.path_for(digest)).await?)
    }

    /// Re-hashes the stored object for `digest`. A missing object is
    /// `Ok(false)`; a corrupted one (changed on disk since it was committed)
    /// is deleted and also `Ok(false)`.
    pub async fn verify(&self, digest: &Digest) -> Result<bool, StoreError> {
        let path = self.path_for(digest);
        let mut file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let mut hasher = Sha256::new();
        hash_reader(&mut file, &mut hasher).await?;
        drop(file);
        if Digest::from(hasher.finalize()).ct_eq(digest) {
            Ok(true)
        } else {
            remove_if_exists(&path).await?;
            Ok(false)
        }
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
        let limit = min_opt(fetched.total, self.max_object_bytes);
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
        let _lock = self.lock(&digest).await;
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
        let now = SystemTime::now();
        let mut removed = 0;
        for sub in [TMP, PARTIAL] {
            let mut dir = tokio::fs::read_dir(self.inner.root.join(sub)).await?;
            while let Some(entry) = dir.next_entry().await? {
                if sub == TMP && is_live(&entry.file_name()) {
                    continue;
                }
                let Ok(meta) = entry.metadata().await else {
                    continue;
                };
                let age = meta
                    .modified()
                    .ok()
                    .and_then(|m| now.duration_since(m).ok())
                    .unwrap_or_default();
                if meta.is_file() && age > older_than && remove_if_exists(&entry.path()).await? {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    /// The total size of the committed objects, in bytes.
    pub async fn usage(&self) -> Result<u64, StoreError> {
        Ok(self.objects().await?.iter().map(|o| o.1).sum())
    }

    /// Deletes committed objects, oldest first (by modification time, which
    /// is when an object was committed unless something touched it since),
    /// until the rest hold at most `max_bytes`. Temporary and partial files
    /// are not counted (see [`purge_temp`](Cache::purge_temp)). An object
    /// that cannot be deleted (open elsewhere, on Windows) is skipped.
    ///
    /// A path the cache returned earlier stops existing when its object is
    /// evicted; open files keep working on Unix.
    pub async fn evict_to(&self, max_bytes: u64) -> Result<Evicted, StoreError> {
        let mut objects = self.objects().await?;
        objects.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.cmp(&b.0)));
        let mut evicted = Evicted {
            remaining: objects.iter().map(|o| o.1).sum(),
            ..Evicted::default()
        };
        for (path, len, _) in objects {
            if evicted.remaining <= max_bytes {
                break;
            }
            if let Ok(true) = remove_if_exists(&path).await {
                evicted.removed += 1;
                evicted.freed += len;
                evicted.remaining -= len;
            }
        }
        Ok(evicted)
    }

    /// Every committed object: its path, size and modification time.
    async fn objects(&self) -> Result<Vec<(PathBuf, u64, SystemTime)>, StoreError> {
        let mut out = Vec::new();
        let mut fans = tokio::fs::read_dir(self.inner.root.join(OBJECTS)).await?;
        while let Some(fan) = fans.next_entry().await? {
            if !fan.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let mut files = tokio::fs::read_dir(fan.path()).await?;
            while let Some(file) = files.next_entry().await? {
                let Ok(meta) = file.metadata().await else {
                    continue;
                };
                if meta.is_file() {
                    let modified = meta.modified().unwrap_or(UNIX_EPOCH);
                    out.push((file.path(), meta.len(), modified));
                }
            }
        }
        Ok(out)
    }

    fn tmp_dir(&self) -> PathBuf {
        self.inner.root.join(TMP)
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

    async fn lock(&self, digest: &Digest) -> Inflight<'_> {
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
        if let (Some(size), Some(max)) = (size, self.max_object_bytes) {
            if size > max {
                return Err(StoreError::SizeMismatch(format!(
                    "{digest} is {size} bytes, over the {max}-byte cap"
                )));
            }
        }
        let _lock = self.lock(digest).await;
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
        let limit = min_opt(min_opt(size, fetched.total), self.max_object_bytes);
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

    /// Moves a verified temporary file to its final place.
    async fn commit(&self, mut tmp: TempFile, digest: &Digest) -> Result<PathBuf, StoreError> {
        let dest = self.path_for(digest);
        if let Some(parent) = dest.parent() {
            create_dirs_async(parent).await?;
        }
        if self.get(digest).is_some() {
            // Another process got there first with the same bytes.
            tmp.discard().await;
            return Ok(dest);
        }
        match tokio::fs::rename(&tmp.path, &dest).await {
            Ok(()) => {
                tmp.committed = true;
                if let Some(parent) = dest.parent() {
                    sync_dir(parent).await;
                }
                Ok(dest)
            }
            Err(e) => {
                tmp.discard().await;
                if self.get(digest).is_some() {
                    Ok(dest)
                } else {
                    Err(e.into())
                }
            }
        }
    }
}

/// Makes a rename into `dir` durable (Unix; best effort: a failure can lose
/// the entry in a crash, never corrupt it).
async fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = tokio::fs::File::open(dir).await {
        let _ = d.sync_all().await;
    }
    #[cfg(not(unix))]
    let _ = dir;
}

fn min_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, None) => a,
        (None, b) => b,
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

fn is_live(name: &std::ffi::OsStr) -> bool {
    LIVE_TEMP
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(name)
}

/// A name no other fetch — in this process or another — is using: the
/// process id, a process-wide counter and the current time. (Not random:
/// `hash::random` is not available on every platform.)
fn unique_name(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{prefix}.{}.{n}.{nanos}.tmp", std::process::id())
}

/// Marks a temporary file as in use by this process (see [`LIVE_TEMP`]).
struct Lease(OsString);

impl Lease {
    fn new(path: &Path) -> Lease {
        let name = path.file_name().unwrap_or_default().to_os_string();
        LIVE_TEMP
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.clone());
        Lease(name)
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        LIVE_TEMP
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
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

    #[test]
    fn unique_names_differ() {
        let a = unique_name("x");
        let b = unique_name("x");
        assert_ne!(a, b);
        assert!(a.starts_with("x.") && a.ends_with(".tmp"));
    }

    #[test]
    fn min_opt_combines() {
        assert_eq!(min_opt(Some(3), Some(2)), Some(2));
        assert_eq!(min_opt(None, Some(2)), Some(2));
        assert_eq!(min_opt(Some(3), None), Some(3));
        assert_eq!(min_opt(None, None), None);
    }

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
        let holder = cache.lock(&digest).await;
        let waiter_cache = cache.clone();
        let waiter = tokio::spawn(async move {
            let _place = waiter_cache.lock(&digest).await;
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
        let holder = cache.lock(&digest).await;
        let waiter_cache = cache.clone();
        let waiter = tokio::spawn(async move {
            let _place = waiter_cache.lock(&digest).await;
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

//! The synchronous, local content-addressed store (feature `store-local`).

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ferroly::fsutils::{sync_dir, FileLock};
use ferroly::hash::Sha256;

use super::{Digest, Layout, StoreError};

/// Directory holding the committed objects, per algorithm.
pub(crate) const OBJECTS: &str = "sha256";
/// Directory holding in-progress writes (one unique file each).
pub(crate) const TMP: &str = "tmp";
/// Directory holding interrupted downloads waiting to be resumed.
pub(crate) const PARTIAL: &str = "partial";
/// The store lock file, in the root (never deleted).
pub(crate) const LOCK: &str = ".lock";

/// Read buffer used when hashing or copying a file.
pub(crate) const READ_BUF: usize = 64 * 1024;

/// Permissions of the directories the store creates (Unix).
#[cfg(unix)]
const DIR_MODE: u32 = 0o700;
/// Permissions of the files the store writes (Unix).
#[cfg(unix)]
pub(crate) const FILE_MODE: u32 = 0o600;

/// The fewest hex digits [`LocalStore::resolve_prefix`] accepts in a short
/// digest (`sha256:<12 hex>`, as `docker images` prints them).
pub const MIN_PREFIX_HEX: usize = 12;

/// How many candidates a [`StoreError::AmbiguousPrefix`] lists.
const MAX_CANDIDATES: usize = 8;

/// Temporary files a write in this process is using, by file name (the
/// names are unique per process), with the number of leases on each.
/// `purge_temp` never deletes one of these, however old it looks: a resumed
/// partial keeps the modification time it was parked with until it is
/// written to. Shared by the synchronous inserts and the async downloads.
static LIVE_TEMP: Mutex<BTreeMap<OsString, usize>> = Mutex::new(BTreeMap::new());

/// A file committed to the store: its digest and where it is on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Stored {
    /// The SHA-256 of the file's content.
    pub digest: Digest,
    /// The file's path inside the store.
    pub path: PathBuf,
}

/// What an eviction ([`LocalStore::evict_to`], and the same method on the
/// async cache) removed.
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

/// One committed object, as [`LocalStore::list`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ObjectInfo {
    /// The object's digest (taken from its file name; not re-hashed).
    pub digest: Digest,
    /// Where the object is on disk.
    pub path: PathBuf,
    /// Its size in bytes.
    pub size: u64,
    /// Its modification time: when it was committed, unless something
    /// touched it since (the Unix epoch where the platform has none).
    pub modified: SystemTime,
}

/// What re-hashing a stored object found ([`LocalStore::check`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ObjectStatus {
    /// Present and hashes to its digest.
    Intact {
        /// The object's size in bytes.
        size: u64,
    },
    /// No object for this digest (or something other than a regular file
    /// at its path).
    Missing,
    /// Present but hashes to something else: it changed on disk since it
    /// was committed.
    Corrupt {
        /// What the content hashes to now.
        actual: Digest,
        /// The object's size in bytes.
        size: u64,
    },
}

/// A local content-addressed store: a directory of files keyed by SHA-256,
/// with a synchronous (blocking) API and no async runtime or HTTP stack.
///
/// Layout under the root (see [`Layout`] for where the objects go):
///
/// ```text
/// sha256/ab/abcdef0123…   committed objects, Layout::Fanout (the default)
/// <prefix>/sha256/abcd…   committed objects, Layout::Flat { prefix }
/// tmp/                    writes in progress, one uniquely named file each
/// partial/                interrupted downloads of the async cache, parked for resumption
/// .lock                   the store lock (see `lock`), created on first use
/// ```
///
/// A committed object always holds exactly the bytes its digest names:
/// content is written to a uniquely named file under `tmp/`, hashed as it
/// is written, `fsync`ed, and only then renamed into place (and, on Unix,
/// the directory synced). A rename within the root is atomic, so readers
/// never see a half-written object, and concurrent writers of the same
/// digest (threads or processes) never corrupt each other: the first rename
/// wins and the others discard their copy.
///
/// On Unix the directories the store creates are `0700` and its files
/// `0600` (before the umask). An existing root keeps its permissions;
/// [`open`](LocalStore::open) removes group and other access from the
/// directories it owns (the objects directory, `tmp/`, `partial/`, and for
/// a flat layout `<prefix>`), skipping those this user may not change.
///
/// This is the core under the async `Cache` (feature `store`), which adds
/// downloads with resume, timeouts and progress; both can share one
/// directory, in one process or several.
///
/// ```
/// use ferroly::store::{Digest, Layout, LocalStore, ObjectStatus};
///
/// # let dir = std::env::temp_dir().join(format!("ferroly-doc-local-{}", std::process::id()));
/// let store = LocalStore::open_with(&dir, Layout::flat("blobs")?)?;
/// let stored = store.insert_bytes(b"hello")?;
/// assert_eq!(stored.digest, Digest::of(b"hello"));
/// assert!(stored.path.ends_with(format!("blobs/sha256/{}", stored.digest.hex())));
///
/// let short = &stored.digest.to_string()[..7 + 12]; // "sha256:" + 12 hex digits
/// assert_eq!(store.resolve_prefix(short)?, stored.digest);
/// assert_eq!(store.check(&stored.digest)?, ObjectStatus::Intact { size: 5 });
/// assert_eq!(store.list()?.len(), 1);
/// # std::fs::remove_dir_all(&dir).unwrap();
/// # Ok::<(), ferroly::store::StoreError>(())
/// ```
///
/// `LocalStore` is cheap to clone (an `Arc` inside).
#[derive(Clone)]
pub struct LocalStore {
    inner: Arc<Inner>,
    max_object_bytes: Option<u64>,
}

struct Inner {
    root: PathBuf,
    layout: Layout,
}

impl std::fmt::Debug for LocalStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalStore")
            .field("root", &self.inner.root)
            .field("layout", &self.inner.layout)
            .field("max_object_bytes", &self.max_object_bytes)
            .finish()
    }
}

/// An exclusive, cross-process lock on a whole store: an OS file lock on
/// `<root>/.lock` (see [`LocalStore::lock`]). Released when dropped, by
/// [`release`](StoreLock::release), or when the process ends.
///
/// It owns a clone of its store, so it is `'static` and `Send` (it can be
/// returned from a blocking thread).
#[derive(Debug)]
#[must_use = "the store lock is released as soon as the guard is dropped"]
pub struct StoreLock {
    store: LocalStore,
    file: FileLock,
}

/// Creates `dir` and its missing parents, private to the owner on Unix.
pub(crate) fn create_dirs(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(DIR_MODE);
    }
    builder.create(dir)
}

/// Tightens one of the store's own directories to `0700` on Unix, for a
/// store created by an older version (or under a lax umask) whose
/// directories are readable by others. A symlink is left alone (never chmod
/// through it), and so is a directory this user may not change (a shared
/// store owned by someone else).
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
            Err(e) if e.kind() == ErrorKind::PermissionDenied => Ok(()),
            other => other,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// The smaller of two optional limits.
pub(crate) fn min_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

fn remove_if_exists(path: &Path) -> std::io::Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Lists `dir`; a missing directory is `None`.
fn read_dir_opt(dir: &Path) -> std::io::Result<Option<std::fs::ReadDir>> {
    match std::fs::read_dir(dir) {
        Ok(rd) => Ok(Some(rd)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The digest an object file name stands for: exactly 64 lower-case hex
/// digits, and (in a fan-out directory) starting with the directory's name.
fn object_name(name: &OsStr, fan: Option<&str>) -> Option<Digest> {
    let name = name.to_str()?;
    if name.len() != 64 || !name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    if fan.is_some_and(|fan| &name[..2] != fan) {
        return None;
    }
    Digest::from_hex(name).ok()
}

/// A fan-out directory's name, when `entry` is one (a real directory, not a
/// link to one).
fn fan_name(entry: &std::fs::DirEntry) -> Option<String> {
    if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
        return None;
    }
    entry.file_name().to_str().map(str::to_owned)
}

/// Validates a short digest and returns its lower-case hex digits.
fn parse_prefix(text: &str, min_hex: usize) -> Result<String, StoreError> {
    let bad = |why: String| {
        // Never echo an unbounded string back.
        let shown: String = text.chars().take(80).collect();
        StoreError::InvalidDigest(format!("{why}, got {shown:?}"))
    };
    let hex = match text.split_once(':') {
        Some((algo, rest)) if algo.eq_ignore_ascii_case(Digest::ALGORITHM) => rest,
        Some(_) => return Err(bad("expected sha256:<hex digest prefix>".into())),
        None => text,
    };
    if hex.is_empty() || hex.len() > 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad(format!(
            "expected a sha256 digest prefix of {min_hex} to 64 hex digits"
        )));
    }
    if hex.len() < min_hex {
        return Err(bad(format!(
            "a digest prefix needs at least {min_hex} hex digits"
        )));
    }
    Ok(hex.to_ascii_lowercase())
}

/// Whether a write in this process holds a [`Lease`] on the temporary file
/// `name` (see [`LIVE_TEMP`]).
pub(crate) fn is_live(name: &OsStr) -> bool {
    LIVE_TEMP
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(name)
}

/// A name no other write — in this process or another — is using: the
/// process id, a process-wide counter and the current time. (Not random:
/// `hash::random` is not available on every platform.)
pub(crate) fn unique_name(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{prefix}.{}.{n}.{nanos}.tmp", std::process::id())
}

/// Marks a temporary file as in use by this process (see [`LIVE_TEMP`]).
/// Several leases on one name are counted.
pub(crate) struct Lease(OsString);

impl Lease {
    pub(crate) fn new(path: &Path) -> Lease {
        let name = path.file_name().unwrap_or_default().to_os_string();
        *LIVE_TEMP
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(name.clone())
            .or_insert(0) += 1;
        Lease(name)
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut live = LIVE_TEMP.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = live.get_mut(&self.0) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                live.remove(&self.0);
            }
        }
    }
}

/// A synchronous write in progress: a uniquely named file under `tmp/` and
/// the hash of what has been written to it. Deleted when dropped without
/// being committed.
struct SyncTemp {
    path: PathBuf,
    file: Option<File>,
    len: u64,
    hasher: Sha256,
    committed: bool,
    /// Released after `Drop` has deleted the file.
    _lease: Lease,
}

impl SyncTemp {
    /// Creates a new, empty file with a unique name (`create_new`, so two
    /// writers can never share one).
    fn create(dir: &Path, prefix: &str) -> Result<SyncTemp, StoreError> {
        let mut attempts = 0;
        loop {
            let path = dir.join(unique_name(prefix));
            let lease = Lease::new(&path);
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(FILE_MODE);
            }
            match options.open(&path) {
                Ok(file) => {
                    return Ok(SyncTemp {
                        path,
                        file: Some(file),
                        len: 0,
                        hasher: Sha256::new(),
                        committed: false,
                        _lease: lease,
                    })
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists && attempts < 16 => attempts += 1,
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Copies `reader` in, hashing it; fails as soon as the content passes
    /// `limit`.
    fn fill(
        &mut self,
        reader: &mut dyn Read,
        limit: Option<u64>,
        what: Option<&Digest>,
    ) -> Result<(), StoreError> {
        let mut buf = vec![0u8; READ_BUF];
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            if let Some(limit) = limit {
                if self.len + n as u64 > limit {
                    let what = what.map(|d| format!("{d}: ")).unwrap_or_default();
                    return Err(StoreError::SizeMismatch(format!(
                        "{what}content exceeds {limit} bytes"
                    )));
                }
            }
            let file = self
                .file
                .as_mut()
                .ok_or_else(|| std::io::Error::other("temporary file is closed"))?;
            file.write_all(&buf[..n])?;
            self.hasher.update(&buf[..n]);
            self.len += n as u64;
        }
    }

    fn digest(&self) -> Digest {
        Digest::from(self.hasher.clone().finalize())
    }

    /// Flushes and syncs the content, and closes the file (ready to rename).
    fn finish(&mut self) -> Result<(), StoreError> {
        if let Some(mut file) = self.file.take() {
            file.flush()?;
            file.sync_all()?;
        }
        Ok(())
    }
}

impl Drop for SyncTemp {
    fn drop(&mut self) {
        if !self.committed {
            drop(self.file.take());
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Hashes everything `file` holds from its current position.
fn hash_file(file: &mut File, hasher: &mut Sha256) -> std::io::Result<u64> {
    let mut buf = vec![0u8; READ_BUF];
    let mut total = 0u64;
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        hasher.update(&buf[..n]);
        total += n as u64;
    }
}

impl LocalStore {
    /// Opens (creating if needed) a store rooted at `dir`, with the default
    /// [`Layout::Fanout`].
    pub fn open(dir: impl AsRef<Path>) -> Result<LocalStore, StoreError> {
        LocalStore::open_with(dir, Layout::Fanout)
    }

    /// Opens (creating if needed) a store rooted at `dir` with `layout`.
    ///
    /// A [`Layout::Flat`] built directly (not through [`Layout::flat`]) is
    /// validated here, with the same rules. **No migration is performed**:
    /// a store written with one layout looks empty when opened with another
    /// (copy objects across with [`list`](LocalStore::list) and
    /// [`insert_path`](LocalStore::insert_path)).
    pub fn open_with(dir: impl AsRef<Path>, layout: Layout) -> Result<LocalStore, StoreError> {
        let layout = layout.validated()?;
        let root = dir.as_ref().to_path_buf();
        for sub in [
            layout.objects_dir(),
            PathBuf::from(TMP),
            PathBuf::from(PARTIAL),
        ] {
            let dir = root.join(sub);
            create_dirs(&dir)?;
            make_private(&dir)?;
        }
        if let Layout::Flat { prefix } = &layout {
            make_private(&root.join(prefix))?;
        }
        Ok(LocalStore {
            inner: Arc::new(Inner { root, layout }),
            max_object_bytes: None,
        })
    }

    /// Caps the size of any one object this store will take in (`None`, the
    /// default, means no cap). An insert that passes the cap is stopped and
    /// deleted as soon as it does ([`StoreError::SizeMismatch`]).
    pub fn with_max_object_bytes(mut self, max: Option<u64>) -> LocalStore {
        self.max_object_bytes = max;
        self
    }

    /// The cap set by [`with_max_object_bytes`](LocalStore::with_max_object_bytes).
    pub fn max_object_bytes(&self) -> Option<u64> {
        self.max_object_bytes
    }

    /// The store's root directory.
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// Where committed objects live under the root.
    pub fn layout(&self) -> &Layout {
        &self.inner.layout
    }

    /// Where the object for `digest` lives (or would live) in the store.
    pub fn path_for(&self, digest: &Digest) -> PathBuf {
        self.inner.root.join(self.inner.layout.object_path(digest))
    }

    /// Where an interrupted download of `digest` is parked.
    pub(crate) fn partial_path(&self, digest: &Digest) -> PathBuf {
        self.inner
            .root
            .join(PARTIAL)
            .join(format!("{}-{}", OBJECTS, digest.hex()))
    }

    pub(crate) fn tmp_dir(&self) -> PathBuf {
        self.inner.root.join(TMP)
    }

    fn objects_dir(&self) -> PathBuf {
        self.inner.root.join(self.inner.layout.objects_dir())
    }

    /// The path of the committed object for `digest`, if the store has it.
    ///
    /// This only checks that a regular file exists; the content was verified
    /// when it was written. Use [`check`](LocalStore::check) to re-hash it.
    pub fn get(&self, digest: &Digest) -> Option<PathBuf> {
        let path = self.path_for(digest);
        match std::fs::metadata(&path) {
            Ok(m) if m.is_file() => Some(path),
            _ => None,
        }
    }

    /// Whether the store holds `digest`.
    pub fn contains(&self, digest: &Digest) -> bool {
        self.get(digest).is_some()
    }

    /// The size of a parked, resumable partial download of `digest`, if any.
    pub fn partial_len(&self, digest: &Digest) -> Option<u64> {
        std::fs::metadata(self.partial_path(digest))
            .ok()
            .map(|m| m.len())
    }

    /// Every committed object, sorted by digest.
    ///
    /// Only real objects are listed: entries that are not regular files
    /// (directories, symbolic links), whose name is not exactly 64
    /// lower-case hex digits (a stray `README`, an upper-case copy) or, in
    /// the fan-out layout, that sit in a fan directory other than their
    /// first two digits are skipped. A missing objects directory is an empty
    /// list. Nothing is re-hashed (see [`check_all`](LocalStore::check_all)).
    pub fn list(&self) -> Result<Vec<ObjectInfo>, StoreError> {
        let objects = self.objects_dir();
        let mut out = Vec::new();
        match &self.inner.layout {
            Layout::Fanout => {
                let Some(fans) = read_dir_opt(&objects)? else {
                    return Ok(out);
                };
                for fan in fans {
                    let fan = fan?;
                    if let Some(name) = fan_name(&fan) {
                        list_dir(&fan.path(), Some(&name), &mut out)?;
                    }
                }
            }
            Layout::Flat { .. } => list_dir(&objects, None, &mut out)?,
        }
        out.sort_by(|a, b| a.digest.as_bytes().cmp(b.digest.as_bytes()));
        Ok(out)
    }

    /// Resolves a short digest — `sha256:<hex prefix>` (algorithm name in
    /// any case) or a bare hex prefix, in either case — to the one stored
    /// digest that starts with it. At least [`MIN_PREFIX_HEX`] hex digits are
    /// required (see [`resolve_prefix_min`](LocalStore::resolve_prefix_min)).
    ///
    /// - malformed, too short or longer than 64 digits:
    ///   [`StoreError::InvalidDigest`];
    /// - no stored object matches (or, for all 64 digits, the object is not
    ///   stored): [`StoreError::NotFound`];
    /// - several match: [`StoreError::AmbiguousPrefix`], with the first few
    ///   candidates.
    ///
    /// Only the objects [`list`](LocalStore::list) reports are candidates
    /// (a symbolic link or a stray file at an object or fan-directory path
    /// is ignored, as there). Only file names are compared; nothing is
    /// hashed. In the fan-out layout only one fan directory is read.
    pub fn resolve_prefix(&self, prefix: &str) -> Result<Digest, StoreError> {
        self.resolve_prefix_min(prefix, MIN_PREFIX_HEX)
    }

    /// [`resolve_prefix`](LocalStore::resolve_prefix) with another minimum
    /// number of hex digits (`min_hex`, clamped to `1..=64`).
    pub fn resolve_prefix_min(&self, prefix: &str, min_hex: usize) -> Result<Digest, StoreError> {
        let hex = parse_prefix(prefix, min_hex.clamp(1, 64))?;
        if hex.len() == 64 {
            let digest = Digest::from_hex(&hex)?;
            return if self.is_listed(&digest) {
                Ok(digest)
            } else {
                Err(StoreError::NotFound(digest.to_string()))
            };
        }
        let mut found = self.matching(&hex)?;
        found.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        let shown = format!("{}:{hex}", Digest::ALGORITHM);
        match found.len() {
            0 => Err(StoreError::NotFound(shown)),
            1 => Ok(found[0]),
            matches => {
                found.truncate(MAX_CANDIDATES);
                Err(StoreError::AmbiguousPrefix {
                    prefix: shown,
                    candidates: found,
                    matches,
                })
            }
        }
    }

    /// Whether `digest` is an object [`list`](LocalStore::list) reports: a
    /// regular file (not a link) at its path, and in the fan-out layout in
    /// a real fan directory (not a link to one).
    fn is_listed(&self, digest: &Digest) -> bool {
        let path = self.path_for(digest);
        let in_real_dir = match &self.inner.layout {
            Layout::Fanout => path
                .parent()
                .is_some_and(|fan| std::fs::symlink_metadata(fan).is_ok_and(|m| m.is_dir())),
            Layout::Flat { .. } => true,
        };
        in_real_dir && std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file())
    }

    /// The stored digests whose hex form starts with `hex` (lower case).
    /// Only objects [`list`](LocalStore::list) would report are matched.
    fn matching(&self, hex: &str) -> Result<Vec<Digest>, StoreError> {
        let objects = self.objects_dir();
        let mut dirs: Vec<(PathBuf, Option<String>)> = Vec::new();
        match &self.inner.layout {
            Layout::Fanout if hex.len() >= 2 => {
                // Only a real fan directory, as `list` (a stray file or a
                // link at its name holds no objects).
                let fan = objects.join(&hex[..2]);
                match std::fs::symlink_metadata(&fan) {
                    Ok(meta) if meta.is_dir() => dirs.push((fan, Some(hex[..2].to_owned()))),
                    Ok(_) => {}
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
            Layout::Fanout => {
                if let Some(fans) = read_dir_opt(&objects)? {
                    for fan in fans {
                        let fan = fan?;
                        if let Some(name) = fan_name(&fan).filter(|n| n.starts_with(hex)) {
                            dirs.push((fan.path(), Some(name)));
                        }
                    }
                }
            }
            Layout::Flat { .. } => dirs.push((objects, None)),
        }
        let mut found = Vec::new();
        for (dir, fan) in dirs {
            let Some(entries) = read_dir_opt(&dir)? else {
                continue;
            };
            for entry in entries {
                let entry = entry?;
                let Some(digest) = object_name(&entry.file_name(), fan.as_deref()) else {
                    continue;
                };
                let is_file = entry.file_type().map(|t| t.is_file()).unwrap_or(false);
                if is_file && digest.hex().starts_with(hex) {
                    found.push(digest);
                }
            }
        }
        Ok(found)
    }

    /// Re-hashes the object for `digest` and reports what it found. **Never
    /// modifies the store**: a corrupted object is reported, not deleted
    /// (see [`StoreLock::repair`] for that).
    ///
    /// A missing object, or anything but a regular file at its path, is
    /// [`ObjectStatus::Missing`]; other I/O errors are returned.
    pub fn check(&self, digest: &Digest) -> Result<ObjectStatus, StoreError> {
        let path = self.path_for(digest);
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => return Ok(ObjectStatus::Missing),
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(ObjectStatus::Missing),
            Err(e) => return Err(e.into()),
        }
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(ObjectStatus::Missing),
            Err(e) => return Err(e.into()),
        };
        let mut hasher = Sha256::new();
        let size = hash_file(&mut file, &mut hasher)?;
        let actual = Digest::from(hasher.finalize());
        Ok(if actual.ct_eq(digest) {
            ObjectStatus::Intact { size }
        } else {
            ObjectStatus::Corrupt { actual, size }
        })
    }

    /// [`check`](LocalStore::check) for every [listed](LocalStore::list)
    /// object, sorted by digest. Never modifies the store. Stops at (and
    /// returns) the first I/O error; an object removed during the walk is
    /// reported [`Missing`](ObjectStatus::Missing).
    pub fn check_all(&self) -> Result<Vec<(Digest, ObjectStatus)>, StoreError> {
        self.list()?
            .into_iter()
            .map(|o| Ok((o.digest, self.check(&o.digest)?)))
            .collect()
    }

    /// Streams `reader` into the store, hashing as it goes, and commits it
    /// under the digest it turns out to have. Inserting content that is
    /// already stored is harmless and returns the same [`Stored`].
    pub fn insert_reader(&self, reader: impl Read) -> Result<Stored, StoreError> {
        let mut reader = reader;
        let mut tmp = SyncTemp::create(&self.tmp_dir(), "insert")?;
        tmp.fill(&mut reader, self.max_object_bytes, None)?;
        tmp.finish()?;
        let digest = tmp.digest();
        let path = self.commit_temp(&mut tmp, &digest)?;
        Ok(Stored { digest, path })
    }

    /// [`insert_reader`](LocalStore::insert_reader) for content that must
    /// hash to `expected`: anything else fails with
    /// [`StoreError::DigestMismatch`] and nothing is kept. When `size` is
    /// given, content that runs past it is stopped as soon as it does, and
    /// content that ends at any other size is rejected
    /// ([`StoreError::SizeMismatch`]). Returns the object's path.
    ///
    /// The reader is always consumed and verified, even when `expected` is
    /// already stored (check [`get`](LocalStore::get) first to skip that).
    pub fn insert_verified(
        &self,
        reader: impl Read,
        expected: &Digest,
        size: Option<u64>,
    ) -> Result<PathBuf, StoreError> {
        if let (Some(size), Some(max)) = (size, self.max_object_bytes) {
            if size > max {
                return Err(StoreError::SizeMismatch(format!(
                    "{expected} is {size} bytes, over the {max}-byte cap"
                )));
            }
        }
        let mut reader = reader;
        let mut tmp = SyncTemp::create(&self.tmp_dir(), &expected.hex())?;
        let limit = min_opt(size, self.max_object_bytes);
        tmp.fill(&mut reader, limit, Some(expected))?;
        if let Some(size) = size {
            if tmp.len != size {
                return Err(StoreError::SizeMismatch(format!(
                    "{expected}: got {} bytes, expected {size}",
                    tmp.len
                )));
            }
        }
        let actual = tmp.digest();
        if !actual.ct_eq(expected) {
            return Err(StoreError::DigestMismatch {
                expected: expected.to_string(),
                actual: actual.to_string(),
            });
        }
        tmp.finish()?;
        self.commit_temp(&mut tmp, expected)
    }

    /// Stores in-memory content and returns its digest and path.
    pub fn insert_bytes(&self, data: &[u8]) -> Result<Stored, StoreError> {
        self.insert_reader(data)
    }

    /// Copies the local file `src` into the store, hashing while copying.
    /// The source is never moved, linked or modified.
    pub fn insert_path(&self, src: impl AsRef<Path>) -> Result<Stored, StoreError> {
        self.insert_reader(File::open(src)?)
    }

    fn commit_temp(&self, tmp: &mut SyncTemp, digest: &Digest) -> Result<PathBuf, StoreError> {
        let path = self.commit_file(&tmp.path, digest)?;
        tmp.committed = true;
        Ok(path)
    }

    /// Moves a verified, synced and closed temporary file to its place as
    /// the object for `digest`. The one implementation of the commit rules,
    /// for the synchronous inserts and the async downloads alike:
    ///
    /// - the object's directory is created if missing;
    /// - if the object is already there (another writer got there first
    ///   with the same bytes), the temporary file is deleted;
    /// - otherwise it is renamed into place and, on Unix, the directory is
    ///   synced (best effort);
    /// - if the rename fails, the temporary file is deleted, and the error
    ///   is returned unless the object appeared meanwhile.
    ///
    /// On success `tmp` no longer exists. On an error before the rename
    /// (creating the directory) it is left in place for the caller.
    pub(crate) fn commit_file(&self, tmp: &Path, digest: &Digest) -> Result<PathBuf, StoreError> {
        let dest = self.path_for(digest);
        if let Some(parent) = dest.parent() {
            create_dirs(parent)?;
        }
        if self.get(digest).is_some() {
            let _ = std::fs::remove_file(tmp);
            return Ok(dest);
        }
        match std::fs::rename(tmp, &dest) {
            Ok(()) => {
                if let Some(parent) = dest.parent() {
                    sync_dir(parent);
                }
                Ok(dest)
            }
            Err(e) => {
                let _ = std::fs::remove_file(tmp);
                if self.get(digest).is_some() {
                    Ok(dest)
                } else {
                    Err(e.into())
                }
            }
        }
    }

    /// Removes the object for `digest` (and any parked partial download).
    /// Returns whether an object was removed.
    ///
    /// Takes the [store lock](LocalStore::lock) for the deletion, so it
    /// **waits** while another holder (in this process or another) has it.
    /// When there is nothing to delete (neither the object nor a partial
    /// exists) it returns `Ok(false)` without taking the lock, so it works
    /// on a store it may not write to, as before the lock existed. Under a
    /// lock you already hold, use [`StoreLock::remove`].
    pub fn remove(&self, digest: &Digest) -> Result<bool, StoreError> {
        if !self.has_removable(digest) {
            return Ok(false);
        }
        self.lock()?.remove(digest)
    }

    /// Whether [`remove`](LocalStore::remove) would find anything to delete:
    /// an entry (of any kind) at the object or partial path. An error other
    /// than "not found" counts as an entry, so the removal reports it.
    pub(crate) fn has_removable(&self, digest: &Digest) -> bool {
        let present = |path: PathBuf| match std::fs::symlink_metadata(path) {
            Ok(_) => true,
            Err(e) => e.kind() != ErrorKind::NotFound,
        };
        present(self.path_for(digest)) || present(self.partial_path(digest))
    }

    fn remove_unlocked(&self, digest: &Digest) -> Result<bool, StoreError> {
        let _ = remove_if_exists(&self.partial_path(digest));
        Ok(remove_if_exists(&self.path_for(digest))?)
    }

    /// The total size of the committed objects, in bytes.
    pub fn usage(&self) -> Result<u64, StoreError> {
        Ok(self.list()?.iter().map(|o| o.size).sum())
    }

    /// Deletes committed objects, oldest first (by modification time, which
    /// is when an object was committed unless something touched it since),
    /// until the rest hold at most `max_bytes`. Temporary and partial files
    /// are not counted (see [`purge_temp`](LocalStore::purge_temp)). An
    /// object that cannot be deleted (open elsewhere, on Windows) is skipped.
    ///
    /// Takes the [store lock](LocalStore::lock) when it has something to
    /// delete, so it **waits** while another holder has it, and lists the
    /// objects again under the lock. When the objects already fit in
    /// `max_bytes` it returns without taking the lock. Under a lock you
    /// already hold, use [`StoreLock::evict_to`]. A path the store returned
    /// earlier stops existing when its object is evicted; open files keep
    /// working on Unix.
    pub fn evict_to(&self, max_bytes: u64) -> Result<Evicted, StoreError> {
        if let Some(fits) = self.evict_unneeded(max_bytes)? {
            return Ok(fits);
        }
        self.lock()?.evict_to(max_bytes)
    }

    /// `Some` (nothing evicted) when the objects already fit in
    /// `max_bytes`, so [`evict_to`](LocalStore::evict_to) need not take the
    /// lock.
    pub(crate) fn evict_unneeded(&self, max_bytes: u64) -> Result<Option<Evicted>, StoreError> {
        let remaining = self.usage()?;
        Ok((remaining <= max_bytes).then(|| Evicted {
            remaining,
            ..Evicted::default()
        }))
    }

    fn evict_unlocked(&self, max_bytes: u64) -> Result<Evicted, StoreError> {
        let mut objects = self.list()?;
        objects.sort_by(|a, b| {
            a.modified
                .cmp(&b.modified)
                .then_with(|| a.digest.as_bytes().cmp(b.digest.as_bytes()))
        });
        let mut evicted = Evicted {
            remaining: objects.iter().map(|o| o.size).sum(),
            ..Evicted::default()
        };
        for object in objects {
            if evicted.remaining <= max_bytes {
                break;
            }
            if let Ok(true) = remove_if_exists(&object.path) {
                evicted.removed += 1;
                evicted.freed += object.size;
                evicted.remaining -= object.size;
            }
        }
        Ok(evicted)
    }

    /// Deletes temporary and parked partial files last modified more than
    /// `older_than` ago — leftovers of processes that died mid-write (their
    /// bytes are never resumed). Returns how many files were removed.
    /// Committed objects and the lock file are never touched.
    ///
    /// A write running in this process (synchronous or async) is never
    /// removed, whatever its age. One running in another process is judged
    /// by its modification time: every write updates it, so use an age well
    /// above the longest a source may stall.
    pub fn purge_temp(&self, older_than: Duration) -> Result<usize, StoreError> {
        let now = SystemTime::now();
        let mut removed = 0;
        for sub in [TMP, PARTIAL] {
            for entry in std::fs::read_dir(self.inner.root.join(sub))? {
                let entry = entry?;
                if sub == TMP && is_live(&entry.file_name()) {
                    continue;
                }
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                let age = meta
                    .modified()
                    .ok()
                    .and_then(|m| now.duration_since(m).ok())
                    .unwrap_or_default();
                if meta.is_file() && age > older_than && remove_if_exists(&entry.path())? {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    fn lock_path(&self) -> PathBuf {
        self.inner.root.join(LOCK)
    }

    /// Takes the exclusive, cross-process store lock, **blocking the
    /// calling thread** until it is free.
    ///
    /// The lock is an OS file lock (`fsutils::FileLock`) on `<root>/.lock`,
    /// which is created on first use and never deleted. The kernel releases
    /// it when the holder exits or crashes, so a dead holder never blocks
    /// anyone. It only excludes other lock holders:
    ///
    /// - [`remove`](LocalStore::remove), [`evict_to`](LocalStore::evict_to)
    ///   (and the async cache's `remove`, `evict_to` and the deletion inside
    ///   `verify`) take it themselves when they have something to delete,
    ///   so they wait for a holder;
    /// - inserts, commits, reads, `list`, `check` and `purge_temp` never take
    ///   it (a commit is an idempotent atomic rename), so holding the lock
    ///   across an insert and your own metadata write cannot deadlock.
    ///
    /// **Not reentrant**: calling `LocalStore::remove` or `evict_to` while
    /// this thread holds a `StoreLock` on the same store waits forever. Use
    /// the guard's own [`StoreLock::remove`], [`StoreLock::evict_to`] and
    /// [`StoreLock::repair`] instead.
    ///
    /// The intended pattern for a registry keeping its own name → digest
    /// references next to the blobs:
    ///
    /// - **pull**: fetch and insert the blob *without* the lock (downloads
    ///   can be long); then `lock()`; if `!contains(digest)` (a concurrent
    ///   garbage collection removed it), insert it again; write the
    ///   reference; drop the lock.
    /// - **rm / gc**: `lock()`; read the references; [`StoreLock::remove`]
    ///   every digest no reference names; drop the lock.
    ///
    /// ```
    /// use ferroly::store::LocalStore;
    ///
    /// # let dir = std::env::temp_dir().join(format!("ferroly-doc-lock-{}", std::process::id()));
    /// let store = LocalStore::open(&dir)?;
    /// let blob = store.insert_bytes(b"weights")?; // no lock needed to insert
    /// {
    ///     let lock = store.lock()?;
    ///     // ... read the references; nothing names `blob` any more ...
    ///     assert!(lock.remove(&blob.digest)?);
    /// } // released here
    /// # std::fs::remove_dir_all(&dir).unwrap();
    /// # Ok::<(), ferroly::store::StoreError>(())
    /// ```
    ///
    /// Where `FileLock` is unsupported (platforms other than Linux, Android,
    /// the Apple systems, the BSDs, illumos and Windows) every acquire fails
    /// with [`StoreError::Io`] of kind `Unsupported`, and so do `remove` and
    /// `evict_to` whenever they have something to delete. On network filesystems the guarantee is the server's;
    /// on Linux NFS mounts a second acquire from the same process succeeds,
    /// so take the lock at most once per process there.
    pub fn lock(&self) -> Result<StoreLock, StoreError> {
        let file = FileLock::acquire_wait(self.lock_path())?;
        Ok(StoreLock {
            store: self.clone(),
            file,
        })
    }

    /// Takes the store lock if it is free; `Ok(None)` if another holder (in
    /// this process or another) has it. See [`lock`](LocalStore::lock).
    pub fn try_lock(&self) -> Result<Option<StoreLock>, StoreError> {
        Ok(
            FileLock::try_acquire(self.lock_path())?.map(|file| StoreLock {
                store: self.clone(),
                file,
            }),
        )
    }

    /// Takes the store lock, waiting at most `timeout` (blocking the calling
    /// thread); fails with [`StoreError::Locked`] if it is still held then.
    /// See [`lock`](LocalStore::lock).
    pub fn lock_timeout(&self, timeout: Duration) -> Result<StoreLock, StoreError> {
        let file = FileLock::acquire_timeout(self.lock_path(), timeout)?;
        Ok(StoreLock {
            store: self.clone(),
            file,
        })
    }
}

/// Collects the objects directly in `dir` (one fan directory, or the flat
/// objects directory).
fn list_dir(dir: &Path, fan: Option<&str>, out: &mut Vec<ObjectInfo>) -> Result<(), StoreError> {
    // A fan directory removed since it was listed is simply empty.
    let Some(entries) = read_dir_opt(dir)? else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let Some(digest) = object_name(&entry.file_name(), fan) else {
            continue;
        };
        // Vanished since `read_dir` (a concurrent remove): skip it.
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_file() {
            out.push(ObjectInfo {
                digest,
                path: entry.path(),
                size: meta.len(),
                modified: meta.modified().unwrap_or(UNIX_EPOCH),
            });
        }
    }
    Ok(())
}

impl StoreLock {
    /// The store this lock is held on.
    pub fn store(&self) -> &LocalStore {
        &self.store
    }

    /// [`LocalStore::remove`] under this lock (no second acquire).
    pub fn remove(&self, digest: &Digest) -> Result<bool, StoreError> {
        self.store.remove_unlocked(digest)
    }

    /// [`LocalStore::evict_to`] under this lock (no second acquire).
    pub fn evict_to(&self, max_bytes: u64) -> Result<Evicted, StoreError> {
        self.store.evict_unlocked(max_bytes)
    }

    /// [`LocalStore::check`], then deletes the object if it is
    /// [`Corrupt`](ObjectStatus::Corrupt). Returns what the check found.
    pub fn repair(&self, digest: &Digest) -> Result<ObjectStatus, StoreError> {
        let status = self.store.check(digest)?;
        if let ObjectStatus::Corrupt { .. } = status {
            remove_if_exists(&self.store.path_for(digest))?;
        }
        Ok(status)
    }

    /// Releases the lock now, reporting any error (dropping the guard
    /// ignores errors). The lock file stays in place.
    pub fn release(self) -> Result<(), StoreError> {
        Ok(self.file.release()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ferroly-local-unit-{tag}-{}", unique_name("d")));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

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

    #[test]
    fn leases_are_counted() {
        let path = Path::new("lease-count-test.tmp");
        let name = OsStr::new("lease-count-test.tmp");
        let a = Lease::new(path);
        let b = Lease::new(path);
        assert!(is_live(name));
        drop(a);
        assert!(is_live(name), "a second lease keeps it live");
        drop(b);
        assert!(!is_live(name));
    }

    #[test]
    fn object_names_are_strict() {
        let d = Digest::of(b"x");
        let hex = d.hex();
        assert_eq!(object_name(OsStr::new(&hex), None), Some(d));
        assert_eq!(object_name(OsStr::new(&hex), Some(&hex[..2])), Some(d));
        assert_eq!(object_name(OsStr::new(&hex), Some("zz")), None);
        assert_eq!(object_name(OsStr::new(&hex.to_uppercase()), None), None);
        assert_eq!(object_name(OsStr::new(&hex[..63]), None), None);
        assert_eq!(object_name(OsStr::new("README"), None), None);
        assert_eq!(object_name(OsStr::new(&format!("{hex}.tmp")), None), None);
    }

    #[test]
    fn prefixes_are_parsed() {
        let hex = "abcdef012345";
        assert_eq!(parse_prefix(hex, 12).unwrap(), hex);
        assert_eq!(parse_prefix("SHA256:ABCDEF012345", 12).unwrap(), hex);
        assert!(parse_prefix("abcdef01234", 12).is_err());
        assert!(parse_prefix("md5:abcdef012345", 12).is_err());
        assert!(parse_prefix("sha256:", 1).is_err());
        assert!(parse_prefix(&"a".repeat(65), 12).is_err());
        let err = parse_prefix("abc", 12).unwrap_err().to_string();
        assert!(err.contains("12"), "{err}");
    }

    /// The commit rules, once, for both the synchronous and async writers.
    #[test]
    fn commit_rules() {
        let dir = scratch("commit");
        let store = LocalStore::open(&dir).unwrap();
        let data = b"commit me";
        let digest = Digest::of(data);

        // Renamed into place; the fan directory is created on demand.
        let tmp = store.tmp_dir().join("a.tmp");
        std::fs::write(&tmp, data).unwrap();
        let path = store.commit_file(&tmp, &digest).unwrap();
        assert_eq!(path, store.path_for(&digest));
        assert_eq!(std::fs::read(&path).unwrap(), data);
        assert!(!tmp.exists());

        // Already present: the second copy is discarded, the first kept.
        let tmp = store.tmp_dir().join("b.tmp");
        std::fs::write(&tmp, data).unwrap();
        assert_eq!(store.commit_file(&tmp, &digest).unwrap(), path);
        assert!(!tmp.exists());

        // A rename that fails (the source is gone) is an error when the
        // object did not appear.
        let other = Digest::of(b"other");
        let missing = store.tmp_dir().join("missing.tmp");
        assert!(store.commit_file(&missing, &other).is_err());
        assert!(!store.contains(&other));

        // The objects directory removed by hand is recreated.
        std::fs::remove_dir_all(dir.join(OBJECTS)).unwrap();
        let tmp = store.tmp_dir().join("c.tmp");
        std::fs::write(&tmp, data).unwrap();
        assert_eq!(store.commit_file(&tmp, &digest).unwrap(), path);
        assert!(store.contains(&digest));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_insert_leaves_no_temporary() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("broken source"))
            }
        }
        let dir = scratch("broken");
        let store = LocalStore::open(&dir).unwrap();
        assert!(matches!(
            store.insert_reader(Broken),
            Err(StoreError::Io(_))
        ));
        assert_eq!(std::fs::read_dir(store.tmp_dir()).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// SEC-12: a store whose directories were created open (by an older
    /// version) is tightened when it is opened; with a flat layout that
    /// includes `<prefix>`.
    #[cfg(unix)]
    #[test]
    fn opening_an_old_flat_store_makes_its_directories_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("perm-old-flat");
        let set = |p: &Path, m: u32| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap()
        };
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        for sub in ["blobs/sha256", TMP, PARTIAL] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
            set(&dir.join(sub), 0o755);
        }
        set(&dir.join("blobs"), 0o755);
        set(&dir, 0o755);
        LocalStore::open_with(&dir, Layout::flat("blobs").unwrap()).unwrap();
        for sub in ["blobs", "blobs/sha256", TMP, PARTIAL] {
            assert_eq!(mode(&dir.join(sub)), 0o700, "{sub}");
        }
        assert_eq!(mode(&dir), 0o755);
        let _ = std::fs::remove_dir_all(dir);
    }
}

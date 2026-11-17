//! A cheaply clonable handle to the current configuration snapshot.

use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, SystemTime};

use ferroly::codec::Decode;

use super::{Config, ConfigError};

type Loader = dyn Fn() -> Result<Config, ConfigError> + Send + Sync;

/// A shared, swappable [`Config`].
///
/// Clones of a `SharedConfig` all point at the same state. Readers take a
/// [`snapshot`](SharedConfig::snapshot) — an `Arc<Config>` that stays valid and
/// unchanged for as long as they hold it — while [`reload`](SharedConfig::reload)
/// or [`replace`](SharedConfig::replace) swap in a new configuration for
/// everyone who asks afterwards.
///
/// Reloading happens when *you* call `reload` (from an admin endpoint, say),
/// or automatically: [`watch_files`](SharedConfig::watch_files) polls files
/// for changes, and `reload_on` (feature `lifecycle`) follows the lifecycle
/// reload handle (`trigger_reload`, `SIGHUP`).
///
/// ```
/// use ferroly::codec::Value;
/// use ferroly::config::{Config, SharedConfig};
///
/// let shared = SharedConfig::with_loader(|| {
///     Ok(Config::builder()
///         .merge_value(Value::Object(vec![("port".into(), Value::Int(8080))]))
///         .build())
/// })
/// .unwrap();
///
/// let snapshot = shared.snapshot();
/// assert_eq!(snapshot.get::<u16>("port").unwrap(), 8080);
///
/// shared.reload().unwrap(); // re-runs the loader and swaps the result in
/// assert_eq!(shared.generation(), 1);
/// ```
#[derive(Clone)]
pub struct SharedConfig {
    inner: Arc<Inner>,
}

struct Inner {
    current: RwLock<Arc<Config>>,
    loader: Option<Box<Loader>>,
    generation: AtomicUsize,
    /// Held around the loader and the swap, so two reloads (watcher, signal,
    /// manual) never overlap and the one that read last installs last.
    reloading: Mutex<()>,
    /// Wall-clock time just before the loader last started a successful
    /// load; `None` without a loader.
    loaded_at: Mutex<Option<SystemTime>>,
}

impl SharedConfig {
    /// Wraps an already built configuration. Without a loader,
    /// [`reload`](SharedConfig::reload) has nothing to run and leaves the
    /// configuration as it is; use [`replace`](SharedConfig::replace) to swap.
    pub fn new(config: Config) -> Self {
        Self::from_parts(config, None)
    }

    /// Builds the initial configuration by running `loader`, and keeps the
    /// loader so [`reload`](SharedConfig::reload) can run it again.
    pub fn with_loader<F>(loader: F) -> Result<Self, ConfigError>
    where
        F: Fn() -> Result<Config, ConfigError> + Send + Sync + 'static,
    {
        let started = SystemTime::now();
        let config = loader()?;
        let shared = Self::from_parts(config, Some(Box::new(loader)));
        shared.set_loaded_at(started);
        Ok(shared)
    }

    fn from_parts(config: Config, loader: Option<Box<Loader>>) -> Self {
        Self {
            inner: Arc::new(Inner {
                current: RwLock::new(Arc::new(config)),
                loader,
                generation: AtomicUsize::new(0),
                reloading: Mutex::new(()),
                loaded_at: Mutex::new(None),
            }),
        }
    }

    fn set_loaded_at(&self, started: SystemTime) {
        *self
            .inner
            .loaded_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(started);
    }

    fn loaded_at(&self) -> Option<SystemTime> {
        *self
            .inner
            .loaded_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The current configuration. The returned `Arc` is a stable snapshot: a
    /// later reload does not change it.
    pub fn snapshot(&self) -> Arc<Config> {
        // A panic while the lock was held cannot leave the `Arc` half-written,
        // so a poisoned lock is still safe to read.
        Arc::clone(
            &self
                .inner
                .current
                .read()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    /// Binds a single dot-pathed key of the current snapshot; see
    /// [`Config::get`].
    pub fn get<T: Decode>(&self, key: &str) -> Result<T, ConfigError> {
        self.snapshot().get(key)
    }

    /// Binds the whole current snapshot; see [`Config::extract`].
    pub fn extract<T: Decode>(&self) -> Result<T, ConfigError> {
        self.snapshot().extract()
    }

    /// Swaps in `config`, returning the snapshot it replaced.
    pub fn replace(&self, config: Config) -> Arc<Config> {
        self.swap(Arc::new(config))
    }

    /// Runs the loader again and swaps the result in, returning the new
    /// snapshot. If the loader fails the error is returned and the current
    /// configuration stays in place, so a broken edit never takes a running
    /// service down. Without a loader this returns the current snapshot.
    ///
    /// Reloads never overlap: a reload that starts while another one runs
    /// waits for it, so the configuration installed last is always the one
    /// read last. Readers are not blocked meanwhile.
    pub fn reload(&self) -> Result<Arc<Config>, ConfigError> {
        let Some(loader) = &self.inner.loader else {
            return Ok(self.snapshot());
        };
        // A panicking loader leaves nothing half-done behind this lock.
        let _reloading = self
            .inner
            .reloading
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // The loader runs outside the snapshot lock: readers are never
        // blocked on file or environment access.
        let started = SystemTime::now();
        let config = Arc::new(loader()?);
        self.set_loaded_at(started);
        self.swap(Arc::clone(&config));
        Ok(config)
    }

    fn swap(&self, config: Arc<Config>) -> Arc<Config> {
        let mut guard = self
            .inner
            .current
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let previous = std::mem::replace(&mut *guard, config);
        self.inner.generation.fetch_add(1, Ordering::SeqCst);
        previous
    }

    /// How many times the configuration has been swapped (by
    /// [`reload`](SharedConfig::reload) or [`replace`](SharedConfig::replace)).
    /// Compare against a remembered value to notice a change cheaply.
    pub fn generation(&self) -> usize {
        self.inner.generation.load(Ordering::SeqCst)
    }
}

impl SharedConfig {
    /// Polls `paths` every `interval` on a background thread and
    /// [`reload`](SharedConfig::reload)s when any of them changes — opt-in hot
    /// reload for configuration files.
    ///
    /// A path counts as changed when it appears, disappears, or its size,
    /// modification time or content differs from what was last loaded (the
    /// content of a file up to 1 MiB is hashed too, so an edit within the
    /// filesystem's timestamp granularity, or a Kubernetes ConfigMap's
    /// symlink swap, is still seen; a larger file is compared by size and
    /// modification time only). List the files the loader reads; the loader
    /// itself decides what a reload does.
    ///
    /// **A change must settle first**: the watcher reloads only once the
    /// files look the same on two polls in a row, so a file caught half
    /// written (an editor or `cp` that truncates, then writes) is not loaded
    /// while it is still being written. A change that leaves a watched file
    /// empty or missing must stay that way for at least one second (and five
    /// polls) before it reloads, because an empty file is a valid, empty
    /// layer: the moment between a rewrite's truncation and its write is not
    /// loaded as "no settings". A writer slower than that can still be
    /// caught mid-write, so replace files atomically (write a temporary
    /// file, then rename it over the old one) where you can.
    ///
    /// A change made after the configuration was loaded but before
    /// `watch_files` is called is noticed when it gave a watched file a
    /// modification time no older than the load: the first poll then
    /// reloads. (A file deleted in that window is noticed with the next
    /// change.)
    ///
    /// `on_reload` is called on the watcher thread after every reload the
    /// watcher triggers, with the new snapshot or the loader's error — on an
    /// error the previous configuration stays in place, exactly as with
    /// `reload`. The watcher stops when the returned [`ConfigWatcher`] is
    /// dropped or [`stop`](ConfigWatcher::stop)ped. The interval is at least
    /// 10 ms. Without a loader there is nothing to reload and the watcher only
    /// reports.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Io`] when the watcher thread cannot be started — hot
    /// reload is then off.
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use ferroly::config::{Config, SharedConfig};
    ///
    /// let shared = SharedConfig::with_loader(|| {
    ///     Ok(Config::builder().merge_file("config/app.yaml")?.build())
    /// })?;
    /// let _watcher = shared.watch_files(["config/app.yaml"], Duration::from_secs(2), |result| {
    ///     if let Err(e) = result {
    ///         eprintln!("config reload failed, keeping the old one: {e}");
    ///     }
    /// })?;
    /// # Ok::<(), ferroly::config::ConfigError>(())
    /// ```
    pub fn watch_files<I, P, F>(
        &self,
        paths: I,
        interval: Duration,
        on_reload: F,
    ) -> Result<ConfigWatcher, ConfigError>
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
        F: FnMut(Result<Arc<Config>, ConfigError>) + Send + 'static,
    {
        let paths: Vec<PathBuf> = paths.into_iter().map(Into::into).collect();
        let interval = interval.max(Duration::from_millis(10));
        let shared = self.clone();
        let (stop, stopped) = mpsc::channel::<()>();
        let mut on_reload = on_reload;
        let current = fingerprints(&paths);
        let mut state = if self
            .loaded_at()
            .is_some_and(|loaded| modified_since(&current, loaded))
        {
            Debounce::stale(current)
        } else {
            Debounce::new(current)
        }
        .with_empty_polls(Debounce::empty_polls_for(interval));
        let thread = std::thread::Builder::new()
            .name("ferroly-config-watch".into())
            .spawn(move || {
                // A message or a hang-up both mean stop.
                while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(interval) {
                    if state.poll(fingerprints(&paths)) {
                        on_reload(shared.reload());
                    }
                }
            })
            .map_err(|e| ConfigError::Io(format!("cannot start the config file watcher: {e}")))?;
        Ok(ConfigWatcher {
            stop: Some(stop),
            thread: Some(thread),
        })
    }

    /// Reloads whenever `signal` changes — tie it to the lifecycle reload
    /// handle with
    /// [`ComponentManager::subscribe_reload`](ferroly::lifecycle::ComponentManager::subscribe_reload),
    /// which fires on [`trigger_reload`](ferroly::lifecycle::ComponentManager::trigger_reload)
    /// and, with [`ManagerOptions::reload_on_sighup`](ferroly::lifecycle::ManagerOptions::reload_on_sighup),
    /// on `SIGHUP`. Needs the `lifecycle` feature and a Tokio runtime.
    ///
    /// The loader runs on Tokio's blocking pool. `on_reload` receives the new
    /// snapshot or the loader's error (the previous configuration stays in
    /// place; a panicking loader is reported as [`ConfigError::Io`]). The task
    /// ends when every sender of `signal` is gone — when the manager is
    /// dropped — or when the returned handle is aborted.
    ///
    /// ```no_run
    /// use ferroly::config::{Config, SharedConfig};
    /// use ferroly::lifecycle::ComponentManager;
    ///
    /// # async fn ex() -> Result<(), ferroly::config::ConfigError> {
    /// let mgr = ComponentManager::new();
    /// let shared = SharedConfig::with_loader(|| {
    ///     Ok(Config::builder().merge_file("config/app.yaml")?.build())
    /// })?;
    /// shared.reload_on(mgr.subscribe_reload(), |result| {
    ///     if let Err(e) = result {
    ///         eprintln!("config reload failed, keeping the old one: {e}");
    ///     }
    /// });
    /// mgr.trigger_reload(); // or SIGHUP with `reload_on_sighup`
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(feature = "lifecycle")]
    pub fn reload_on<F>(
        &self,
        mut signal: tokio::sync::watch::Receiver<u64>,
        mut on_reload: F,
    ) -> tokio::task::JoinHandle<()>
    where
        F: FnMut(Result<Arc<Config>, ConfigError>) + Send + 'static,
    {
        let shared = self.clone();
        // Only changes after this call count.
        signal.borrow_and_update();
        tokio::spawn(async move {
            while signal.changed().await.is_ok() {
                let target = shared.clone();
                let result = match tokio::task::spawn_blocking(move || target.reload()).await {
                    Ok(result) => result,
                    Err(_) => Err(ConfigError::Io("the configuration loader panicked".into())),
                };
                on_reload(result);
            }
        })
    }
}

/// Largest file whose content [`SharedConfig::watch_files`] hashes on every
/// poll; a larger one is compared by size and modification time only.
const HASH_LIMIT: u64 = 1024 * 1024;

/// What [`SharedConfig::watch_files`] compares between polls, per path:
/// `None` when the file cannot be read, else its size, modification time and
/// a hash of its content (`None` above [`HASH_LIMIT`]).
type Fingerprint = Option<(u64, Option<SystemTime>, Option<u64>)>;

fn fingerprints(paths: &[PathBuf]) -> Vec<Fingerprint> {
    paths.iter().map(|path| fingerprint(path)).collect()
}

fn fingerprint(path: &Path) -> Fingerprint {
    // One handle for metadata and content, so both describe the same file
    // even while a symlink is being swapped.
    let file = std::fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    let hash = if meta.len() <= HASH_LIMIT {
        let mut bytes = Vec::new();
        // Bounded even when the file grows, or reports a length of 0.
        file.take(HASH_LIMIT + 1).read_to_end(&mut bytes).ok()?;
        let mut hasher = DefaultHasher::new();
        bytes.hash(&mut hasher);
        Some(hasher.finish())
    } else {
        None
    };
    Some((meta.len(), meta.modified().ok(), hash))
}

/// Whether any of `current` was modified at or after `loaded` — then the
/// loaded configuration may predate it.
/// Whether a watched file is empty or cannot be read: the state a non-atomic
/// rewrite passes through.
fn has_empty_or_missing(prints: &[Fingerprint]) -> bool {
    prints
        .iter()
        .any(|print| !matches!(print, Some((len, _, _)) if *len > 0))
}

fn modified_since(current: &[Fingerprint], loaded: SystemTime) -> bool {
    current
        .iter()
        .flatten()
        .any(|(_, modified, _)| modified.is_some_and(|m| m >= loaded))
}

/// How long a change that leaves a watched file empty or missing must stay
/// put before it reloads: far longer than the gap between the truncation and
/// the write of a non-atomic rewrite, so that gap is never loaded as an empty
/// layer.
const EMPTY_SETTLE: Duration = Duration::from_secs(1);

/// The fewest polls [`EMPTY_SETTLE`] is made of, however long the interval.
const EMPTY_SETTLE_MIN_POLLS: u32 = 5;

/// The watcher's reload decision: reload when the files differ from what was
/// last loaded **and** look the same as on the previous poll, so a change is
/// only picked up once it has settled for one interval. A change that leaves
/// a watched file empty or missing must look the same for `empty_polls`
/// polls in a row instead.
#[derive(Debug)]
struct Debounce {
    /// What the running configuration was loaded from; `None` when unknown
    /// (any settled state then reloads).
    loaded: Option<Vec<Fingerprint>>,
    /// The previous poll, when it differed from `loaded`.
    pending: Option<Vec<Fingerprint>>,
    /// How many polls in a row have repeated `pending`.
    repeats: u32,
    /// Repeats needed when a watched file is empty or missing (at least 1).
    empty_polls: u32,
}

impl Debounce {
    /// The files currently look like `loaded`.
    fn new(loaded: Vec<Fingerprint>) -> Self {
        Debounce {
            loaded: Some(loaded),
            pending: None,
            repeats: 0,
            empty_polls: 1,
        }
    }

    /// The files look like `seen`, which may be newer than the running
    /// configuration: the next poll that still sees `seen` reloads.
    fn stale(seen: Vec<Fingerprint>) -> Self {
        Debounce {
            loaded: None,
            pending: Some(seen),
            repeats: 0,
            empty_polls: 1,
        }
    }

    /// Requires `polls` repeated polls (at least 1) before a state with an
    /// empty or missing watched file reloads.
    fn with_empty_polls(mut self, polls: u32) -> Self {
        self.empty_polls = polls.max(1);
        self
    }

    /// The number of polls that make up [`EMPTY_SETTLE`] at `interval`.
    fn empty_polls_for(interval: Duration) -> u32 {
        let per_poll = interval.as_millis().max(1);
        let polls = EMPTY_SETTLE.as_millis().div_ceil(per_poll);
        u32::try_from(polls)
            .unwrap_or(u32::MAX)
            .max(EMPTY_SETTLE_MIN_POLLS)
    }

    /// Feeds one poll; `true` means reload now. The fingerprint is taken
    /// before the loader runs, so an edit during the load is seen next time.
    fn poll(&mut self, now: Vec<Fingerprint>) -> bool {
        if self.loaded.as_ref() == Some(&now) {
            self.pending = None;
            self.repeats = 0;
            return false;
        }
        if self.pending.as_ref() == Some(&now) {
            self.repeats += 1;
            let needed = if has_empty_or_missing(&now) {
                self.empty_polls
            } else {
                1
            };
            if self.repeats < needed {
                return false;
            }
            self.pending = None;
            self.repeats = 0;
            self.loaded = Some(now);
            return true;
        }
        self.pending = Some(now);
        self.repeats = 0;
        false
    }
}

/// The background poller started by [`SharedConfig::watch_files`]. Dropping
/// it stops the polling (without waiting for the thread);
/// [`stop`](ConfigWatcher::stop) also waits for an in-progress reload.
#[derive(Debug)]
pub struct ConfigWatcher {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ConfigWatcher {
    /// Stops polling and waits for the watcher thread to finish.
    pub fn stop(mut self) {
        self.stop.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        // Hanging up wakes the thread at once.
        self.stop.take();
    }
}

impl fmt::Debug for SharedConfig {
    /// Redacted like [`Config`]'s own `Debug`: key names only, never values.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedConfig")
            .field("config", &*self.snapshot())
            .field("generation", &self.generation())
            .field("reloadable", &self.inner.loader.is_some())
            .finish()
    }
}

impl From<Config> for SharedConfig {
    fn from(config: Config) -> Self {
        Self::new(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn print(n: u64) -> Vec<Fingerprint> {
        vec![Some((n, None, Some(n)))]
    }

    #[test]
    fn a_change_reloads_only_once_it_has_settled() {
        let mut state = Debounce::new(print(1));
        assert!(!state.poll(print(1)), "nothing changed");
        // Caught half written, then complete on the next poll: still moving.
        assert!(!state.poll(print(2)));
        assert!(!state.poll(print(3)));
        // The same twice in a row: reload, once.
        assert!(state.poll(print(3)));
        assert!(!state.poll(print(3)));
        // A change that reverts before settling never reloads.
        assert!(!state.poll(print(4)));
        assert!(!state.poll(print(3)));
        assert!(!state.poll(print(3)));
    }

    #[test]
    fn an_empty_or_missing_file_must_settle_longer() {
        let empty = || vec![Some((0, None, Some(0)))];
        let mut state = Debounce::new(print(1)).with_empty_polls(3);
        // Truncated, then rewritten before the longer settle is up: the
        // empty state is never loaded.
        assert!(!state.poll(empty()));
        assert!(!state.poll(empty()));
        assert!(!state.poll(empty()));
        assert!(!state.poll(print(2)));
        assert!(state.poll(print(2)), "a non-empty change settles as usual");
        // Emptied on purpose: reloads once it has stayed empty long enough.
        assert!(!state.poll(empty()));
        assert!(!state.poll(empty()));
        assert!(!state.poll(empty()));
        assert!(state.poll(empty()));
        assert!(!state.poll(empty()));
        // A missing file is treated the same way.
        assert!(!state.poll(vec![None]));
        assert!(!state.poll(vec![None]));
        assert!(!state.poll(vec![None]));
        assert!(state.poll(vec![None]));
    }

    #[test]
    fn the_empty_settle_spans_a_second_and_at_least_five_polls() {
        assert_eq!(Debounce::empty_polls_for(Duration::from_millis(20)), 50);
        assert_eq!(Debounce::empty_polls_for(Duration::from_millis(10)), 100);
        assert_eq!(Debounce::empty_polls_for(Duration::from_millis(300)), 5);
        assert_eq!(Debounce::empty_polls_for(Duration::from_secs(5)), 5);
        assert_eq!(Debounce::empty_polls_for(Duration::ZERO), 1000);
    }

    #[test]
    fn a_stale_baseline_reloads_on_the_first_settled_poll() {
        let mut state = Debounce::stale(print(5));
        assert!(state.poll(print(5)));
        assert!(!state.poll(print(5)));
    }

    #[test]
    fn large_files_are_not_hashed() {
        let dir = std::env::temp_dir().join(format!("ferroly-shared-fp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let small = dir.join("small");
        let large = dir.join("large");
        std::fs::write(&small, b"port: 1\n").unwrap();
        std::fs::write(&large, vec![b'x'; HASH_LIMIT as usize + 1]).unwrap();
        let prints = fingerprints(&[small, large, dir.join("missing")]);
        assert!(matches!(prints[0], Some((8, _, Some(_)))));
        assert!(matches!(prints[1], Some((n, _, None)) if n == HASH_LIMIT + 1));
        assert_eq!(prints[2], None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn modification_since_the_load_marks_the_baseline_stale() {
        let now = SystemTime::now();
        let before = now - Duration::from_secs(5);
        let after = now + Duration::from_secs(5);
        assert!(!modified_since(&[Some((1, Some(before), None)), None], now));
        assert!(modified_since(&[Some((1, Some(after), None))], now));
        assert!(!modified_since(&[Some((1, None, None))], now));
    }
}

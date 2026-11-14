//! A cheaply clonable handle to the current configuration snapshot.

use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

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
/// There is no file watching: reloading happens when *you* call `reload`, for
/// example from a `SIGHUP` handler or an admin endpoint.
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
        let config = loader()?;
        Ok(Self::from_parts(config, Some(Box::new(loader))))
    }

    fn from_parts(config: Config, loader: Option<Box<Loader>>) -> Self {
        Self {
            inner: Arc::new(Inner {
                current: RwLock::new(Arc::new(config)),
                loader,
                generation: AtomicUsize::new(0),
            }),
        }
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
        let mut guard = self
            .inner
            .current
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let previous = std::mem::replace(&mut *guard, Arc::new(config));
        self.inner.generation.fetch_add(1, Ordering::SeqCst);
        previous
    }

    /// Runs the loader again and swaps the result in, returning the new
    /// snapshot. If the loader fails the error is returned and the current
    /// configuration stays in place, so a broken edit never takes a running
    /// service down. Without a loader this returns the current snapshot.
    pub fn reload(&self) -> Result<Arc<Config>, ConfigError> {
        if let Some(loader) = &self.inner.loader {
            // The loader runs outside the lock: readers are never blocked on
            // file or environment access.
            let config = loader()?;
            self.replace(config);
        }
        Ok(self.snapshot())
    }

    /// How many times the configuration has been swapped (by
    /// [`reload`](SharedConfig::reload) or [`replace`](SharedConfig::replace)).
    /// Compare against a remembered value to notice a change cheaply.
    pub fn generation(&self) -> usize {
        self.inner.generation.load(Ordering::SeqCst)
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

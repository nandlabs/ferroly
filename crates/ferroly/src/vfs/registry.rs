//! [`VfsRegistry`] — dispatches paths to backends by URL scheme.

use std::collections::HashMap;
use std::sync::Arc;

use super::path::split_scheme;
use super::{copy_between, Entry, LocalFs, Metadata, Vfile, Vfs, VfsError};

/// Routes a path to the [`Vfs`] backend registered for its URL scheme.
///
/// Each backend announces the schemes it handles through [`Vfs::schemes`];
/// [`register`](VfsRegistry::register) records them, and
/// [`resolve`](VfsRegistry::resolve) picks the backend for a URL such as
/// `mem://cache/x`. Paths with no scheme go to the *default* backend, if one
/// has been chosen. Schemes are matched ignoring ASCII case.
///
/// The convenience methods (`read`, `write`, …) resolve and forward; and
/// [`copy`](VfsRegistry::copy) / [`rename`](VfsRegistry::rename) work
/// **across** backends by streaming.
///
/// ```
/// use std::sync::Arc;
/// use ferroly::vfs::{MemoryFs, Vfs, VfsRegistry};
///
/// # async fn demo() -> Result<(), ferroly::vfs::VfsError> {
/// let mut registry = VfsRegistry::new();
/// registry.register(Arc::new(MemoryFs::new()));
/// // A second, separate tree under a scheme of our choosing.
/// registry.register_scheme("scratch", Arc::new(MemoryFs::new()));
///
/// registry.write("mem://greeting.txt", b"hello".to_vec()).await?;
/// registry.copy("mem://greeting.txt", "scratch://copy.txt").await?;
/// assert_eq!(registry.read("scratch://copy.txt").await?, b"hello");
/// assert!(registry.read("s3://bucket/key").await.is_err()); // nothing registered
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Default)]
pub struct VfsRegistry {
    backends: HashMap<String, Arc<dyn Vfs>>,
    default_scheme: Option<String>,
}

impl VfsRegistry {
    /// Creates an empty registry: no backends, no default.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a registry with `local` registered for `file://` URLs and as
    /// the default for paths without a scheme.
    ///
    /// Pass a [`LocalFs::rooted`] backend to keep scheme-less paths confined;
    /// an unconfined [`LocalFs::new`] gives them the run of the disk.
    pub fn with_local(local: LocalFs) -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(local));
        registry.set_default("file");
        registry
    }

    /// Registers `backend` for every scheme it reports from
    /// [`Vfs::schemes`], replacing any backend already registered for one of
    /// them.
    pub fn register(&mut self, backend: Arc<dyn Vfs>) -> &mut Self {
        let schemes: Vec<String> = backend
            .schemes()
            .iter()
            .map(|s| s.to_ascii_lowercase())
            .collect();
        for scheme in schemes {
            self.backends.insert(scheme, backend.clone());
        }
        self
    }

    /// Registers `backend` under an explicit `scheme`, whatever
    /// [`Vfs::schemes`] says — for mounting two instances of one backend side
    /// by side.
    ///
    /// The backend still parses the paths it is given: one that rejects
    /// foreign schemes (as the bundled ones do) will be handed the path with
    /// the registry's scheme **removed**, so `scratch://a/b` reaches it as
    /// `a/b`.
    pub fn register_scheme(&mut self, scheme: &str, backend: Arc<dyn Vfs>) -> &mut Self {
        self.backends.insert(scheme.to_ascii_lowercase(), backend);
        self
    }

    /// Chooses which registered scheme handles paths that have no scheme.
    pub fn set_default(&mut self, scheme: &str) -> &mut Self {
        self.default_scheme = Some(scheme.to_ascii_lowercase());
        self
    }

    /// The backend registered for `scheme`, if any.
    pub fn get(&self, scheme: &str) -> Option<&Arc<dyn Vfs>> {
        self.backends.get(&scheme.to_ascii_lowercase())
    }

    /// The registered schemes, sorted.
    pub fn schemes(&self) -> Vec<&str> {
        let mut schemes: Vec<&str> = self.backends.keys().map(String::as_str).collect();
        schemes.sort_unstable();
        schemes
    }

    /// The backend that handles `url`, by its scheme (or the default for a
    /// plain path). Fails with [`VfsError::Unsupported`] if nothing is
    /// registered for it.
    pub fn resolve(&self, url: &str) -> Result<&Arc<dyn Vfs>, VfsError> {
        self.route(url).map(|(backend, _)| backend)
    }

    /// Resolves `url` to its backend and the path to hand that backend.
    ///
    /// The path is passed through unchanged when the backend itself claims
    /// the scheme; when the scheme is only an alias given to
    /// [`register_scheme`](VfsRegistry::register_scheme), it is stripped.
    fn route<'a, 'u>(&'a self, url: &'u str) -> Result<(&'a Arc<dyn Vfs>, &'u str), VfsError> {
        match split_scheme(url) {
            (Some(scheme), rest) => {
                let key = scheme.to_ascii_lowercase();
                let backend = self.backends.get(&key).ok_or_else(|| {
                    VfsError::Unsupported(format!(
                        "no backend registered for scheme `{scheme}`: {url}"
                    ))
                })?;
                let native = backend
                    .schemes()
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(scheme));
                Ok((backend, if native { url } else { rest }))
            }
            (None, _) => {
                let backend = self
                    .default_scheme
                    .as_ref()
                    .and_then(|scheme| self.backends.get(scheme))
                    .ok_or_else(|| {
                        VfsError::Unsupported(format!(
                            "no default backend for a path without a scheme: {url}"
                        ))
                    })?;
                Ok((backend, url))
            }
        }
    }

    /// Reads a whole file. See [`Vfs::read`].
    pub async fn read(&self, url: &str) -> Result<Vec<u8>, VfsError> {
        let (backend, path) = self.route(url)?;
        backend.read(path).await
    }

    /// Writes a whole file. See [`Vfs::write`].
    pub async fn write(&self, url: &str, data: Vec<u8>) -> Result<(), VfsError> {
        let (backend, path) = self.route(url)?;
        backend.write(path, data).await
    }

    /// Opens a file as a streaming reader. See [`Vfs::open`].
    pub async fn open(&self, url: &str) -> Result<Vfile, VfsError> {
        let (backend, path) = self.route(url)?;
        backend.open(path).await
    }

    /// Whether the path exists; `false` if no backend handles it. See
    /// [`Vfs::exists`].
    pub async fn exists(&self, url: &str) -> bool {
        match self.route(url) {
            Ok((backend, path)) => backend.exists(path).await,
            Err(_) => false,
        }
    }

    /// Metadata for the path. See [`Vfs::metadata`].
    pub async fn metadata(&self, url: &str) -> Result<Metadata, VfsError> {
        let (backend, path) = self.route(url)?;
        backend.metadata(path).await
    }

    /// Lists one directory level. See [`Vfs::list`].
    ///
    /// Entry paths are the backend's own: for a backend mounted under an
    /// alias scheme they come back without that scheme.
    pub async fn list(&self, url: &str) -> Result<Vec<Entry>, VfsError> {
        let (backend, path) = self.route(url)?;
        backend.list(path).await
    }

    /// Creates a directory and all missing parents. See [`Vfs::mkdir_all`].
    pub async fn mkdir_all(&self, url: &str) -> Result<(), VfsError> {
        let (backend, path) = self.route(url)?;
        backend.mkdir_all(path).await
    }

    /// Deletes a file, or a directory and its contents. See [`Vfs::delete`].
    pub async fn delete(&self, url: &str) -> Result<(), VfsError> {
        let (backend, path) = self.route(url)?;
        backend.delete(path).await
    }

    /// Copies one file, within a backend or from one backend to another.
    ///
    /// When both URLs resolve to the same backend this is that backend's own
    /// [`Vfs::copy`]; otherwise the file is streamed with [`copy_between`].
    pub async fn copy(&self, src: &str, dst: &str) -> Result<(), VfsError> {
        let (from, from_path) = self.route(src)?;
        let (to, to_path) = self.route(dst)?;
        if Arc::ptr_eq(from, to) {
            from.copy(from_path, to_path).await
        } else {
            copy_between(from.as_ref(), from_path, to.as_ref(), to_path)
                .await
                .map(|_| ())
        }
    }

    /// Moves one path, within a backend or from one backend to another.
    ///
    /// Within a backend this is [`Vfs::rename`]. Across backends it is a
    /// streamed copy followed by a delete of the source — which is **not
    /// atomic** (a failure in between leaves both copies) and handles a
    /// single file only, not a directory tree.
    pub async fn rename(&self, src: &str, dst: &str) -> Result<(), VfsError> {
        let (from, from_path) = self.route(src)?;
        let (to, to_path) = self.route(dst)?;
        if Arc::ptr_eq(from, to) {
            return from.rename(from_path, to_path).await;
        }
        copy_between(from.as_ref(), from_path, to.as_ref(), to_path).await?;
        from.delete(from_path).await
    }
}

impl std::fmt::Debug for VfsRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VfsRegistry")
            .field("schemes", &self.schemes())
            .field("default_scheme", &self.default_scheme)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::testutil::TempRoot;
    use crate::vfs::MemoryFs;

    #[tokio::test]
    async fn routes_by_scheme_and_default() {
        let mem = Arc::new(MemoryFs::new());
        let mut registry = VfsRegistry::new();
        registry.register(mem.clone());
        assert_eq!(registry.schemes(), ["mem"]);
        assert!(registry.get("MEM").is_some());
        assert!(registry.get("file").is_none());

        registry.write("mem://a.txt", b"1".to_vec()).await.unwrap();
        assert_eq!(registry.read("MEM://a.txt").await.unwrap(), b"1");
        assert_eq!(mem.read("/a.txt").await.unwrap(), b"1");
        assert!(registry.exists("mem://a.txt").await);

        // No default yet: scheme-less paths are refused, not guessed at.
        assert!(matches!(
            registry.read("/a.txt").await,
            Err(VfsError::Unsupported(_))
        ));
        assert!(!registry.exists("/a.txt").await);
        registry.set_default("mem");
        assert_eq!(registry.read("/a.txt").await.unwrap(), b"1");

        // Unregistered scheme.
        let err = registry.read("s3://bucket/key").await.unwrap_err();
        assert!(matches!(&err, VfsError::Unsupported(m) if m.contains("s3")));
        assert!(registry.resolve("s3://bucket/key").is_err());
        assert!(!registry.exists("s3://bucket/key").await);
    }

    #[tokio::test]
    async fn alias_schemes_mount_separate_instances() {
        let mut registry = VfsRegistry::new();
        let cache = Arc::new(MemoryFs::new());
        let archive = Arc::new(MemoryFs::new());
        registry.register_scheme("cache", cache.clone());
        registry.register_scheme("archive", archive.clone());
        assert_eq!(registry.schemes(), ["archive", "cache"]);

        registry.mkdir_all("cache://tmp").await.unwrap();
        registry
            .write("cache://tmp/x.bin", vec![7; 10])
            .await
            .unwrap();
        assert!(cache.exists("/tmp/x.bin").await);
        assert!(!archive.exists("/tmp/x.bin").await);
        assert_eq!(
            registry.metadata("cache://tmp/x.bin").await.unwrap().len,
            10
        );
        assert_eq!(registry.list("cache://tmp").await.unwrap().len(), 1);

        // Cross-backend move: streamed copy, then the source is removed.
        registry
            .rename("cache://tmp/x.bin", "archive://x.bin")
            .await
            .unwrap();
        assert!(!cache.exists("/tmp/x.bin").await);
        assert_eq!(archive.read("/x.bin").await.unwrap(), vec![7; 10]);

        // Same-backend copy and rename use the backend's own operations.
        registry
            .copy("archive://x.bin", "archive://y.bin")
            .await
            .unwrap();
        registry
            .rename("archive://y.bin", "archive://z.bin")
            .await
            .unwrap();
        assert!(archive.exists("/z.bin").await);
        assert!(!archive.exists("/y.bin").await);
        registry.delete("archive://z.bin").await.unwrap();
        assert!(!registry.exists("archive://z.bin").await);
    }

    #[tokio::test]
    async fn copies_between_memory_and_local() {
        let temp = TempRoot::new("registry");
        let mut registry = VfsRegistry::with_local(LocalFs::rooted(temp.path()).unwrap());
        registry.register(Arc::new(MemoryFs::new()));
        assert_eq!(registry.schemes(), ["file", "mem"]);

        // Larger than one streaming chunk.
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        registry
            .write("mem://payload.bin", payload.clone())
            .await
            .unwrap();
        registry
            .copy("mem://payload.bin", "payload.bin")
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(temp.path().join("payload.bin")).unwrap(),
            payload
        );

        registry
            .copy("payload.bin", "mem://back.bin")
            .await
            .unwrap();
        assert_eq!(registry.read("mem://back.bin").await.unwrap(), payload);

        let mut opened = registry.open("payload.bin").await.unwrap();
        assert_eq!(opened.metadata().len, payload.len() as u64);
        let mut head = [0u8; 4];
        tokio::io::AsyncReadExt::read_exact(&mut opened, &mut head)
            .await
            .unwrap();
        assert_eq!(head, payload[..4]);

        // A missing source leaves nothing at the destination.
        assert!(registry.copy("mem://nope", "nope.bin").await.is_err());
        assert!(!temp.path().join("nope.bin").exists());
        // The confined default still rejects escapes.
        assert!(matches!(
            registry.read("../outside").await,
            Err(VfsError::InvalidPath { .. })
        ));
    }
}

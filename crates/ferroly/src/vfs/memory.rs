//! [`MemoryFs`] — an in-memory backend.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

use super::path::strip_scheme;
use super::{BoxFuture, Entry, Metadata, Vfile, Vfs, VfsError, WalkAction, WalkFn};

/// A [`Vfs`] that keeps everything in memory — a stand-in for [`LocalFs`] in
/// tests, and a scratch filesystem that leaves nothing behind.
///
/// It behaves like the local backend where it matters for code written against
/// the trait: a file's parent directory must exist before the file is written,
/// reading a directory is [`VfsError::IsDir`], listing a file is
/// [`VfsError::NotDir`], writes replace a file in one step, and so on.
///
/// Paths are `/`-separated and always resolved from the root: `"a/b"`,
/// `"/a/b"` and `"mem://a/b"` are the same file. `.` components are ignored
/// and `..` is rejected. Entries report absolute paths (`/a/b`).
///
/// Cloning yields another handle to the **same** tree. All data lives until
/// the last handle is dropped; nothing is ever persisted.
///
/// ```
/// use ferroly::vfs::{MemoryFs, Vfs};
///
/// # async fn demo() -> Result<(), ferroly::vfs::VfsError> {
/// let fs = MemoryFs::new();
/// fs.mkdir_all("/reports/2024").await?;
/// fs.write("/reports/2024/q1.csv", b"a,b\n".to_vec()).await?;
/// assert_eq!(fs.read("mem://reports/2024/q1.csv").await?, b"a,b\n");
/// assert_eq!(fs.list("/reports").await?.len(), 1);
/// # Ok(())
/// # }
/// ```
///
/// [`LocalFs`]: super::LocalFs
#[derive(Debug, Clone, Default)]
pub struct MemoryFs {
    tree: Arc<Mutex<Tree>>,
}

#[derive(Debug, Clone)]
enum Node {
    File {
        data: Arc<Vec<u8>>,
        created: SystemTime,
        modified: SystemTime,
    },
    Dir {
        created: SystemTime,
    },
}

impl Node {
    fn is_dir(&self) -> bool {
        matches!(self, Node::Dir { .. })
    }

    fn metadata(&self) -> Metadata {
        match self {
            Node::File {
                data,
                created,
                modified,
            } => Metadata::file(data.len() as u64)
                .with_created(*created)
                .with_modified(*modified),
            Node::Dir { created } => Metadata::dir()
                .with_created(*created)
                .with_modified(*created),
        }
    }
}

/// Every node except the root, keyed by its normalized absolute path
/// (`/a/b`). The root directory `/` always exists and is not stored.
#[derive(Debug, Default)]
struct Tree {
    nodes: BTreeMap<String, Node>,
}

const ROOT: &str = "/";

/// The parent of a normalized key (`/a/b` → `/a`, `/a` → `/`).
fn parent_of(key: &str) -> &str {
    match key.rfind('/') {
        Some(0) | None => ROOT,
        Some(at) => &key[..at],
    }
}

/// The final component of a normalized key.
fn name_of(key: &str) -> &str {
    key.rsplit('/').next().unwrap_or(key)
}

/// The prefix shared by everything strictly inside directory `key`.
fn prefix_of(key: &str) -> String {
    if key == ROOT {
        ROOT.to_string()
    } else {
        format!("{key}/")
    }
}

/// Normalizes an address to a key: `/`-joined components under the root.
fn normalize(addr: &str) -> Result<String, VfsError> {
    let local = strip_scheme(addr, &["mem"], "MemoryFs")?;
    let mut key = String::new();
    for part in local.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                return Err(VfsError::InvalidPath {
                    path: addr.to_string(),
                    reason: "`..` is not allowed".to_string(),
                })
            }
            part => {
                key.push('/');
                key.push_str(part);
            }
        }
    }
    if key.is_empty() {
        key.push('/');
    }
    Ok(key)
}

impl Tree {
    fn is_dir(&self, key: &str) -> bool {
        key == ROOT || self.nodes.get(key).is_some_and(Node::is_dir)
    }

    fn exists(&self, key: &str) -> bool {
        key == ROOT || self.nodes.contains_key(key)
    }

    /// The error for a key that is absent: [`VfsError::NotDir`] if some
    /// parent is a file (the path can never resolve), else `NotFound`.
    fn missing(&self, key: &str) -> VfsError {
        let mut ancestor = parent_of(key);
        while ancestor != ROOT {
            match self.nodes.get(ancestor) {
                Some(Node::File { .. }) => return VfsError::NotDir(key.to_string()),
                Some(Node::Dir { .. }) => break,
                None => ancestor = parent_of(ancestor),
            }
        }
        VfsError::NotFound(key.to_string())
    }

    /// The bytes of the file at `key`.
    fn file(&self, key: &str) -> Result<&Arc<Vec<u8>>, VfsError> {
        if key == ROOT {
            return Err(VfsError::IsDir(key.to_string()));
        }
        match self.nodes.get(key) {
            Some(Node::File { data, .. }) => Ok(data),
            Some(Node::Dir { .. }) => Err(VfsError::IsDir(key.to_string())),
            None => Err(self.missing(key)),
        }
    }

    /// Checks that `key` can hold a file: not the root, not a directory, and
    /// its parent is an existing directory.
    fn check_file_slot(&self, key: &str) -> Result<(), VfsError> {
        if self.is_dir(key) {
            return Err(VfsError::IsDir(key.to_string()));
        }
        let parent = parent_of(key);
        if self.is_dir(parent) {
            Ok(())
        } else if self.nodes.contains_key(parent) {
            Err(VfsError::NotDir(key.to_string()))
        } else {
            Err(self.missing(key))
        }
    }

    /// Stores `data` at `key`, keeping the creation time of a replaced file.
    fn put(&mut self, key: &str, data: Arc<Vec<u8>>) -> Result<(), VfsError> {
        self.check_file_slot(key)?;
        let now = SystemTime::now();
        let created = match self.nodes.get(key) {
            Some(Node::File { created, .. }) => *created,
            _ => now,
        };
        self.nodes.insert(
            key.to_string(),
            Node::File {
                data,
                created,
                modified: now,
            },
        );
        Ok(())
    }

    /// The direct children of directory `key`, in name order.
    fn children(&self, key: &str) -> Result<Vec<Entry>, VfsError> {
        if !self.is_dir(key) {
            return Err(if self.nodes.contains_key(key) {
                VfsError::NotDir(key.to_string())
            } else {
                self.missing(key)
            });
        }
        let prefix = prefix_of(key);
        Ok(self
            .nodes
            .range(prefix.clone()..)
            .take_while(|(k, _)| k.starts_with(&prefix))
            .filter(|(k, _)| !k[prefix.len()..].contains('/'))
            .map(|(k, node)| entry_of(k, node))
            .collect())
    }

    /// Every key strictly inside directory `key`.
    fn descendants(&self, key: &str) -> Vec<String> {
        let prefix = prefix_of(key);
        self.nodes
            .range(prefix.clone()..)
            .take_while(|(k, _)| k.starts_with(&prefix))
            .map(|(k, _)| k.clone())
            .collect()
    }
}

fn entry_of(key: &str, node: &Node) -> Entry {
    Entry {
        path: key.to_string(),
        name: name_of(key).to_string(),
        is_dir: node.is_dir(),
        len: node.metadata().len,
    }
}

impl MemoryFs {
    /// Creates an empty in-memory filesystem (just the root directory).
    pub fn new() -> Self {
        Self::default()
    }

    /// Locks the tree. A poisoned lock is recovered: every mutation below
    /// validates first and then applies a single map update, so the tree is
    /// consistent even if a holder panicked.
    fn tree(&self) -> MutexGuard<'_, Tree> {
        self.tree.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Runs `f` on the normalized key, as a ready future.
    fn with<'a, T: Send + 'a>(
        &'a self,
        path: &str,
        f: impl FnOnce(&mut Tree, &str) -> Result<T, VfsError> + Send + 'a,
    ) -> BoxFuture<'a, Result<T, VfsError>> {
        let key = normalize(path);
        Box::pin(async move {
            let key = key?;
            f(&mut self.tree(), &key)
        })
    }
}

impl Vfs for MemoryFs {
    fn schemes(&self) -> &[&str] {
        &["mem"]
    }

    fn read(&self, path: &str) -> BoxFuture<'_, Result<Vec<u8>, VfsError>> {
        self.with(path, |tree, key| Ok(tree.file(key)?.as_ref().clone()))
    }

    fn read_range(
        &self,
        path: &str,
        offset: u64,
        len: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, VfsError>> {
        self.with(path, move |tree, key| {
            let data = tree.file(key)?;
            // Clamp to what exists; `offset`/`len` may be anything.
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(data.len());
            let want = usize::try_from(len).unwrap_or(usize::MAX);
            let end = start.saturating_add(want).min(data.len());
            Ok(data[start..end].to_vec())
        })
    }

    fn write(&self, path: &str, data: Vec<u8>) -> BoxFuture<'_, Result<(), VfsError>> {
        self.with(path, move |tree, key| tree.put(key, Arc::new(data)))
    }

    fn open(&self, path: &str) -> BoxFuture<'_, Result<Vfile, VfsError>> {
        self.with(path, |tree, key| {
            let data = tree.file(key)?.as_ref().clone();
            let meta = tree
                .nodes
                .get(key)
                .map(Node::metadata)
                .unwrap_or_else(|| Metadata::file(data.len() as u64));
            Ok(Vfile::from_bytes(data, meta))
        })
    }

    fn delete(&self, path: &str) -> BoxFuture<'_, Result<(), VfsError>> {
        let shown = path.to_string();
        self.with(path, move |tree, key| {
            if key == ROOT {
                return Err(VfsError::InvalidPath {
                    path: shown,
                    reason: "the root itself cannot be deleted".to_string(),
                });
            }
            if tree.nodes.remove(key).is_none() {
                return Err(tree.missing(key));
            }
            for inner in tree.descendants(key) {
                tree.nodes.remove(&inner);
            }
            Ok(())
        })
    }

    fn exists(&self, path: &str) -> BoxFuture<'_, bool> {
        let key = normalize(path);
        Box::pin(async move { key.is_ok_and(|key| self.tree().exists(&key)) })
    }

    fn metadata(&self, path: &str) -> BoxFuture<'_, Result<Metadata, VfsError>> {
        self.with(path, |tree, key| {
            if key == ROOT {
                return Ok(Metadata::dir());
            }
            match tree.nodes.get(key) {
                Some(node) => Ok(node.metadata()),
                None => Err(tree.missing(key)),
            }
        })
    }

    fn list(&self, path: &str) -> BoxFuture<'_, Result<Vec<Entry>, VfsError>> {
        self.with(path, |tree, key| tree.children(key))
    }

    fn mkdir_all(&self, path: &str) -> BoxFuture<'_, Result<(), VfsError>> {
        self.with(path, |tree, key| {
            // Validate the whole chain before creating anything.
            let mut prefixes = Vec::new();
            let mut at = key;
            while at != ROOT {
                match tree.nodes.get(at) {
                    Some(Node::File { .. }) => return Err(VfsError::NotDir(key.to_string())),
                    Some(Node::Dir { .. }) => break,
                    None => prefixes.push(at.to_string()),
                }
                at = parent_of(at);
            }
            let created = SystemTime::now();
            for prefix in prefixes {
                tree.nodes.insert(prefix, Node::Dir { created });
            }
            Ok(())
        })
    }

    fn copy(&self, src: &str, dst: &str) -> BoxFuture<'_, Result<(), VfsError>> {
        let dst = normalize(dst);
        self.with(src, move |tree, from| {
            let to = dst?;
            let data = tree.file(from)?.clone();
            tree.put(&to, data)
        })
    }

    fn rename(&self, src: &str, dst: &str) -> BoxFuture<'_, Result<(), VfsError>> {
        let shown = src.to_string();
        let dst = normalize(dst);
        self.with(src, move |tree, from| {
            let to = dst?;
            if from == ROOT || to == ROOT {
                return Err(VfsError::InvalidPath {
                    path: shown,
                    reason: "the root itself cannot be renamed or replaced".to_string(),
                });
            }
            let Some(node) = tree.nodes.get(from).cloned() else {
                return Err(tree.missing(from));
            };
            if from == to {
                return Ok(());
            }
            let invalid = |reason: &str| VfsError::InvalidPath {
                path: to.clone(),
                reason: reason.to_string(),
            };
            if node.is_dir() {
                if to.starts_with(&prefix_of(from)) {
                    return Err(invalid("cannot move a directory into itself"));
                }
                // The destination's parent must be a directory, and the
                // destination itself absent or an empty directory.
                let parent = parent_of(&to);
                if !tree.is_dir(parent) {
                    return Err(if tree.nodes.contains_key(parent) {
                        VfsError::NotDir(to.clone())
                    } else {
                        tree.missing(&to)
                    });
                }
                match tree.nodes.get(&to) {
                    Some(Node::File { .. }) => return Err(VfsError::NotDir(to.clone())),
                    Some(Node::Dir { .. }) if !tree.descendants(&to).is_empty() => {
                        return Err(invalid("destination directory is not empty"))
                    }
                    _ => {}
                }
                let old_prefix = prefix_of(from);
                let new_prefix = prefix_of(&to);
                for inner in tree.descendants(from) {
                    if let Some(moved) = tree.nodes.remove(&inner) {
                        let renamed = format!("{new_prefix}{}", &inner[old_prefix.len()..]);
                        tree.nodes.insert(renamed, moved);
                    }
                }
            } else {
                tree.check_file_slot(&to)?;
            }
            tree.nodes.remove(from);
            tree.nodes.insert(to, node);
            Ok(())
        })
    }

    fn walk(&self, path: &str, mut f: WalkFn) -> BoxFuture<'_, Result<(), VfsError>> {
        // Snapshot under the lock, then call back without it, so the
        // callback may use this filesystem itself.
        let snapshot = self.with(path, |tree, key| {
            tree.children(key)?; // validates that `key` is a directory
            let prefix = prefix_of(key);
            Ok(tree
                .nodes
                .range(prefix.clone()..)
                .take_while(|(k, _)| k.starts_with(&prefix))
                .map(|(k, node)| entry_of(k, node))
                .collect::<Vec<Entry>>())
        });
        Box::pin(async move {
            // Keys are ordered so that a directory comes before everything
            // inside it, which is all pruning needs.
            let mut skipped: Vec<String> = Vec::new();
            for entry in snapshot.await? {
                if skipped.iter().any(|dir| entry.path.starts_with(dir)) {
                    continue;
                }
                match f(&entry) {
                    WalkAction::Stop => return Ok(()),
                    WalkAction::SkipDir if entry.is_dir => skipped.push(prefix_of(&entry.path)),
                    WalkAction::SkipDir | WalkAction::Continue => {}
                }
            }
            Ok(())
        })
    }

    fn append(&self, path: &str, data: Vec<u8>) -> BoxFuture<'_, Result<(), VfsError>> {
        self.with(path, move |tree, key| {
            let mut contents = match tree.nodes.get(key) {
                Some(Node::File { data, .. }) => data.as_ref().clone(),
                _ => Vec::new(),
            };
            contents.extend_from_slice(&data);
            tree.put(key, Arc::new(contents))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    async fn seeded() -> MemoryFs {
        let fs = MemoryFs::new();
        fs.mkdir_all("/docs/sub").await.unwrap();
        fs.write("/docs/a.txt", b"alpha".to_vec()).await.unwrap();
        fs.write("/docs/sub/b.txt", b"beta".to_vec()).await.unwrap();
        fs.write("/top.txt", b"top".to_vec()).await.unwrap();
        fs
    }

    #[test]
    fn normalizes_paths() {
        assert_eq!(normalize("/a/b").unwrap(), "/a/b");
        assert_eq!(normalize("a/b/").unwrap(), "/a/b");
        assert_eq!(normalize("mem://a//./b").unwrap(), "/a/b");
        assert_eq!(normalize("").unwrap(), "/");
        assert_eq!(normalize("mem://").unwrap(), "/");
        assert!(matches!(
            normalize("/a/../b"),
            Err(VfsError::InvalidPath { .. })
        ));
        assert!(matches!(
            normalize("s3://bucket/key"),
            Err(VfsError::InvalidPath { .. })
        ));
    }

    #[tokio::test]
    async fn read_write_and_metadata() {
        let fs = seeded().await;
        assert_eq!(fs.schemes(), ["mem"]);
        assert_eq!(fs.read("docs/a.txt").await.unwrap(), b"alpha");
        assert_eq!(fs.read("mem://docs/a.txt").await.unwrap(), b"alpha");
        assert!(fs.exists("/docs").await);
        assert!(fs.exists("/").await);
        assert!(!fs.exists("/nope").await);
        assert!(!fs.exists("s3://bucket/x").await);

        let meta = fs.metadata("/docs/a.txt").await.unwrap();
        assert!(meta.is_file());
        assert_eq!(meta.len, 5);
        assert!(meta.modified.is_some() && meta.created.is_some());
        assert!(fs.metadata("/docs").await.unwrap().is_dir);
        assert!(fs.metadata("/").await.unwrap().is_dir);

        // Replacing keeps the creation time and updates the size.
        fs.write("/docs/a.txt", b"alpha-2".to_vec()).await.unwrap();
        let again = fs.metadata("/docs/a.txt").await.unwrap();
        assert_eq!(again.len, 7);
        assert_eq!(again.created, meta.created);

        let file = fs.open("/docs/a.txt").await.unwrap();
        assert_eq!(file.metadata().len, 7);
        assert_eq!(file.read_all_string().await.unwrap(), "alpha-2");
    }

    #[tokio::test]
    async fn read_range_is_clamped() {
        let fs = seeded().await;
        assert_eq!(fs.read_range("/docs/a.txt", 1, 3).await.unwrap(), b"lph");
        assert_eq!(
            fs.read_range("/docs/a.txt", 3, u64::MAX).await.unwrap(),
            b"ha"
        );
        assert_eq!(fs.read_range("/docs/a.txt", 5, 10).await.unwrap(), b"");
        assert_eq!(
            fs.read_range("/docs/a.txt", u64::MAX, u64::MAX)
                .await
                .unwrap(),
            b""
        );
        assert_eq!(fs.read_range("/docs/a.txt", 0, 0).await.unwrap(), b"");
    }

    #[tokio::test]
    async fn errors_match_the_local_backend() {
        let fs = seeded().await;
        assert!(matches!(fs.read("/nope").await, Err(VfsError::NotFound(_))));
        assert!(matches!(fs.read("/docs").await, Err(VfsError::IsDir(_))));
        assert!(matches!(fs.read("/").await, Err(VfsError::IsDir(_))));
        assert!(matches!(fs.open("/docs").await, Err(VfsError::IsDir(_))));
        assert!(matches!(
            fs.list("/top.txt").await,
            Err(VfsError::NotDir(_))
        ));
        assert!(matches!(
            fs.read("/top.txt/inner").await,
            Err(VfsError::NotDir(_))
        ));
        // A file's parent must exist...
        assert!(matches!(
            fs.write("/missing/x.txt", vec![]).await,
            Err(VfsError::NotFound(_))
        ));
        // ...and be a directory.
        assert!(matches!(
            fs.write("/top.txt/x.txt", vec![]).await,
            Err(VfsError::NotDir(_))
        ));
        assert!(matches!(
            fs.write("/docs", vec![]).await,
            Err(VfsError::IsDir(_))
        ));
        assert!(matches!(
            fs.mkdir_all("/top.txt/dir").await,
            Err(VfsError::NotDir(_))
        ));
        assert!(matches!(
            fs.delete("/nope").await,
            Err(VfsError::NotFound(_))
        ));
        assert!(matches!(
            fs.delete("/").await,
            Err(VfsError::InvalidPath { .. })
        ));
        assert!(matches!(
            fs.read("s3://bucket/key").await,
            Err(VfsError::InvalidPath { .. })
        ));
    }

    #[tokio::test]
    async fn list_copy_rename_delete() {
        let fs = seeded().await;
        let names: Vec<String> = fs
            .list("/docs")
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["a.txt", "sub"]);
        let root: Vec<String> = fs
            .list("/")
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.path)
            .collect();
        assert_eq!(root, ["/docs", "/top.txt"]);

        fs.copy("/docs/a.txt", "/docs/a-copy.txt").await.unwrap();
        assert_eq!(fs.read("/docs/a-copy.txt").await.unwrap(), b"alpha");
        assert!(matches!(
            fs.copy("/docs", "/x").await,
            Err(VfsError::IsDir(_))
        ));

        // Rename a file over another file.
        fs.rename("/docs/a-copy.txt", "/top.txt").await.unwrap();
        assert_eq!(fs.read("/top.txt").await.unwrap(), b"alpha");
        assert!(!fs.exists("/docs/a-copy.txt").await);

        // Rename a directory: its contents move with it.
        fs.rename("/docs", "/archive").await.unwrap();
        assert!(!fs.exists("/docs").await);
        assert_eq!(fs.read("/archive/sub/b.txt").await.unwrap(), b"beta");
        assert!(matches!(
            fs.rename("/archive", "/archive/sub/inner").await,
            Err(VfsError::InvalidPath { .. })
        ));
        assert!(matches!(
            fs.rename("/nope", "/x").await,
            Err(VfsError::NotFound(_))
        ));

        // Delete a directory recursively.
        fs.delete("/archive").await.unwrap();
        assert!(!fs.exists("/archive/sub/b.txt").await);
        assert_eq!(fs.list("/").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn sibling_with_shared_prefix_is_not_a_child() {
        // `/a.txt` and `/a-b` sort next to `/a`, but are not inside it.
        let fs = MemoryFs::new();
        fs.mkdir_all("/a").await.unwrap();
        fs.write("/a/inner.txt", b"1".to_vec()).await.unwrap();
        fs.write("/a.txt", b"2".to_vec()).await.unwrap();
        fs.write("/a-b", b"3".to_vec()).await.unwrap();
        assert_eq!(fs.list("/a").await.unwrap().len(), 1);
        fs.delete("/a").await.unwrap();
        assert!(fs.exists("/a.txt").await);
        assert!(fs.exists("/a-b").await);
    }

    #[tokio::test]
    async fn walk_honors_skip_and_stop() {
        let fs = seeded().await;
        let seen = Arc::new(StdMutex::new(Vec::<String>::new()));

        let sink = seen.clone();
        fs.walk(
            "/",
            Box::new(move |e| {
                sink.lock().unwrap().push(e.path.clone());
                WalkAction::Continue
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            [
                "/docs",
                "/docs/a.txt",
                "/docs/sub",
                "/docs/sub/b.txt",
                "/top.txt"
            ]
        );

        seen.lock().unwrap().clear();
        let sink = seen.clone();
        fs.walk(
            "/",
            Box::new(move |e| {
                sink.lock().unwrap().push(e.path.clone());
                if e.name == "sub" {
                    WalkAction::SkipDir
                } else {
                    WalkAction::Continue
                }
            }),
        )
        .await
        .unwrap();
        assert!(!seen
            .lock()
            .unwrap()
            .contains(&"/docs/sub/b.txt".to_string()));
        assert!(seen.lock().unwrap().contains(&"/top.txt".to_string()));

        seen.lock().unwrap().clear();
        let sink = seen.clone();
        fs.walk(
            "/docs",
            Box::new(move |e| {
                sink.lock().unwrap().push(e.path.clone());
                WalkAction::Stop
            }),
        )
        .await
        .unwrap();
        assert_eq!(seen.lock().unwrap().len(), 1);

        assert!(matches!(
            fs.walk("/top.txt", Box::new(|_| WalkAction::Continue))
                .await,
            Err(VfsError::NotDir(_))
        ));
    }

    #[tokio::test]
    async fn append_streaming_write_and_paging() {
        let fs = MemoryFs::new();
        fs.append("/log.txt", b"one\n".to_vec()).await.unwrap();
        fs.append("/log.txt", b"two\n".to_vec()).await.unwrap();
        assert_eq!(fs.read("/log.txt").await.unwrap(), b"one\ntwo\n");
        assert!(matches!(
            fs.append("/missing/log.txt", vec![1]).await,
            Err(VfsError::NotFound(_))
        ));

        // The default streaming writer: nothing is visible before `finish`.
        let mut writer = fs.create("/big.bin").await.unwrap();
        writer.write_all(b"part one, ").await.unwrap();
        writer.write_all(b"part two").await.unwrap();
        assert_eq!(writer.bytes_written(), 18);
        assert!(!fs.exists("/big.bin").await);
        assert_eq!(writer.finish().await.unwrap(), 18);
        assert_eq!(fs.read("/big.bin").await.unwrap(), b"part one, part two");

        // Dropped without finishing: nothing is written.
        let mut abandoned = fs.create("/never.bin").await.unwrap();
        abandoned.write_all(b"x").await.unwrap();
        drop(abandoned);
        assert!(!fs.exists("/never.bin").await);

        // Default paging.
        for name in ["c", "a", "e", "b", "d"] {
            fs.write(&format!("/{name}.dat"), vec![]).await.unwrap();
        }
        let mut names = Vec::new();
        let mut after = None;
        loop {
            let page = fs.list_page("/", after.clone(), 3).await.unwrap();
            assert!(page.entries.len() <= 3);
            names.extend(page.entries.into_iter().map(|e| e.name));
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        assert_eq!(
            names,
            ["a.dat", "b.dat", "big.bin", "c.dat", "d.dat", "e.dat", "log.txt"]
        );
    }

    #[tokio::test]
    async fn clones_share_one_tree_and_survive_a_poisoned_lock() {
        let fs = MemoryFs::new();
        let other = fs.clone();
        fs.write("/shared.txt", b"x".to_vec()).await.unwrap();
        assert_eq!(other.read("/shared.txt").await.unwrap(), b"x");

        // Poison the lock by panicking while holding it.
        let poisoner = fs.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.tree.lock().unwrap();
            panic!("poison the lock");
        })
        .join();
        assert!(fs.tree.is_poisoned());
        assert_eq!(fs.read("/shared.txt").await.unwrap(), b"x");
        fs.write("/after.txt", b"y".to_vec()).await.unwrap();
        assert!(other.exists("/after.txt").await);
    }
}

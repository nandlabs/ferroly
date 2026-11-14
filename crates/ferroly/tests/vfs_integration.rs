#![cfg(feature = "vfs")]
//! `ferroly::vfs` through its public API: code written against the `Vfs`
//! trait runs unchanged on the local and in-memory backends, and the
//! cross-backend helpers tie them together.

use std::path::PathBuf;
use std::sync::Arc;

use ferroly::vfs::{
    copy_between, glob, glob_match, scheme_of, LocalFs, MemoryFs, Metadata, Vfile, Vfs, VfsError,
    VfsRegistry, WalkAction,
};

/// A unique temp directory, removed on drop.
struct TempRoot(PathBuf);

impl TempRoot {
    fn new(tag: &str) -> TempRoot {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ferroly-vfs-it-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        TempRoot(path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A small workload written only against `dyn Vfs`.
async fn exercise(fs: &dyn Vfs) {
    fs.mkdir_all("data/raw").await.unwrap();
    fs.write("data/raw/a.csv", b"a,b\n1,2\n".to_vec())
        .await
        .unwrap();
    fs.append("data/raw/a.csv", b"3,4\n".to_vec())
        .await
        .unwrap();
    fs.write("data/readme.md", b"# data".to_vec())
        .await
        .unwrap();

    assert_eq!(fs.read("data/raw/a.csv").await.unwrap(), b"a,b\n1,2\n3,4\n");
    assert_eq!(
        fs.read_range("data/raw/a.csv", 4, u64::MAX).await.unwrap(),
        b"1,2\n3,4\n"
    );
    assert_eq!(
        fs.read_range("data/raw/a.csv", 1_000, 10).await.unwrap(),
        b""
    );

    let meta = fs.metadata("data/raw/a.csv").await.unwrap();
    assert!(meta.is_file());
    assert_eq!(meta.len, 12);
    assert!(meta.modified.is_some());
    assert!(fs.metadata("data").await.unwrap().is_dir);

    // Streaming write: invisible until finished.
    let mut writer = fs.create("data/raw/b.csv").await.unwrap();
    writer.write_all(b"x,y\n").await.unwrap();
    writer.write_all(b"5,6\n").await.unwrap();
    assert!(!fs.exists("data/raw/b.csv").await);
    assert_eq!(writer.finish().await.unwrap(), 8);
    assert_eq!(fs.read("data/raw/b.csv").await.unwrap(), b"x,y\n5,6\n");

    // Typed errors, the same on every backend.
    assert!(matches!(
        fs.read("data/missing.csv").await,
        Err(VfsError::NotFound(_))
    ));
    assert!(matches!(fs.read("data").await, Err(VfsError::IsDir(_))));
    assert!(matches!(
        fs.list("data/readme.md").await,
        Err(VfsError::NotDir(_))
    ));
    assert!(matches!(
        fs.write("data/readme.md/x", vec![]).await,
        Err(VfsError::NotDir(_))
    ));
    assert!(matches!(
        fs.read("data/../data/readme.md").await,
        Err(VfsError::InvalidPath { .. })
    ));
    assert!(matches!(
        fs.read("s3://bucket/key").await,
        Err(VfsError::InvalidPath { .. })
    ));

    // Paging and globbing.
    let mut names = Vec::new();
    let mut after = None;
    loop {
        let page = fs.list_page("data/raw", after, 1).await.unwrap();
        names.extend(page.entries.into_iter().map(|e| e.name));
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert_eq!(names, ["a.csv", "b.csv"]);

    let csvs = glob(fs, "data", "**/*.csv").await.unwrap();
    let csv_names: Vec<&str> = csvs.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(csv_names, ["a.csv", "b.csv"]);
    // Entry paths feed straight back into the same backend.
    assert_eq!(fs.read(&csvs[1].path).await.unwrap(), b"x,y\n5,6\n");

    // One directory level, streamed through `walk`.
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    fs.walk(
        "data",
        Box::new(move |entry| {
            sink.lock().unwrap().push(entry.name.clone());
            WalkAction::SkipDir
        }),
    )
    .await
    .unwrap();
    let mut level = seen.lock().unwrap().clone();
    level.sort();
    assert_eq!(level, ["raw", "readme.md"]);

    fs.copy("data/readme.md", "data/raw/readme-copy.md")
        .await
        .unwrap();
    fs.rename("data/raw/readme-copy.md", "data/moved.md")
        .await
        .unwrap();
    assert_eq!(fs.read("data/moved.md").await.unwrap(), b"# data");
    fs.delete("data/raw").await.unwrap();
    assert!(!fs.exists("data/raw/a.csv").await);
    assert!(fs.exists("data/moved.md").await);
}

#[tokio::test]
async fn the_same_code_runs_on_memory_and_rooted_local() {
    exercise(&MemoryFs::new()).await;

    let temp = TempRoot::new("same-code");
    let local = LocalFs::rooted(&temp.0).unwrap();
    exercise(&local).await;
    // What is left on disk is exactly what the workload left, and no
    // temporary files.
    let mut left: Vec<String> = std::fs::read_dir(temp.0.join("data"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["moved.md", "readme.md"]);
}

#[tokio::test]
async fn registry_copies_across_backends() {
    let temp = TempRoot::new("registry");
    let memory = Arc::new(MemoryFs::new());
    let mut registry = VfsRegistry::with_local(LocalFs::rooted(&temp.0).unwrap());
    registry.register(memory.clone());
    assert_eq!(registry.schemes(), ["file", "mem"]);
    assert_eq!(scheme_of("mem://x"), Some("mem"));

    let payload: Vec<u8> = (0..150_000u32).map(|i| (i % 241) as u8).collect();
    registry
        .write("mem://upload.bin", payload.clone())
        .await
        .unwrap();
    registry
        .copy("mem://upload.bin", "stored.bin")
        .await
        .unwrap();
    assert_eq!(std::fs::read(temp.0.join("stored.bin")).unwrap(), payload);
    registry
        .rename("stored.bin", "mem://roundtrip.bin")
        .await
        .unwrap();
    assert!(!temp.0.join("stored.bin").exists());
    assert_eq!(memory.read("/roundtrip.bin").await.unwrap(), payload);

    // The free function does the same without a registry.
    let local = LocalFs::rooted(&temp.0).unwrap();
    let copied = copy_between(memory.as_ref(), "/roundtrip.bin", &local, "direct.bin")
        .await
        .unwrap();
    assert_eq!(copied, payload.len() as u64);

    assert!(matches!(
        registry.read("s3://bucket/key").await,
        Err(VfsError::Unsupported(_))
    ));
    assert!(matches!(
        registry.read("../escape").await,
        Err(VfsError::InvalidPath { .. })
    ));
}

#[cfg(feature = "hash")]
#[tokio::test]
async fn checksum_is_the_same_on_every_backend() {
    use ferroly::vfs::checksum_sha256;

    let temp = TempRoot::new("checksum");
    let local = LocalFs::rooted(&temp.0).unwrap();
    let memory = MemoryFs::new();
    let data: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 256) as u8).collect();
    local.write("blob.bin", data.clone()).await.unwrap();
    memory.write("/blob.bin", data.clone()).await.unwrap();

    let expected = ferroly::hash::sha256(&data);
    assert_eq!(checksum_sha256(&local, "blob.bin").await.unwrap(), expected);
    assert_eq!(
        checksum_sha256(&memory, "/blob.bin").await.unwrap(),
        expected
    );
    // The empty file has the well-known empty digest.
    memory.write("/empty", Vec::new()).await.unwrap();
    assert_eq!(
        checksum_sha256(&memory, "/empty").await.unwrap().to_hex(),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert!(matches!(
        checksum_sha256(&memory, "/missing").await,
        Err(VfsError::NotFound(_))
    ));
}

/// A minimal external backend: the trait's defaults (`append`, `create`,
/// `list_page`) work on top of the required methods alone.
struct OneFile(std::sync::Mutex<Vec<u8>>);

impl Vfs for OneFile {
    fn schemes(&self) -> &[&str] {
        &["one"]
    }
    fn read(&self, _path: &str) -> ferroly::vfs::BoxFuture<'_, Result<Vec<u8>, VfsError>> {
        Box::pin(async move { Ok(self.0.lock().unwrap().clone()) })
    }
    fn read_range(
        &self,
        _path: &str,
        offset: u64,
        len: u64,
    ) -> ferroly::vfs::BoxFuture<'_, Result<Vec<u8>, VfsError>> {
        Box::pin(async move {
            let data = self.0.lock().unwrap();
            let start = (offset as usize).min(data.len());
            let end = start.saturating_add(len as usize).min(data.len());
            Ok(data[start..end].to_vec())
        })
    }
    fn write(
        &self,
        _path: &str,
        data: Vec<u8>,
    ) -> ferroly::vfs::BoxFuture<'_, Result<(), VfsError>> {
        Box::pin(async move {
            *self.0.lock().unwrap() = data;
            Ok(())
        })
    }
    fn open(&self, _path: &str) -> ferroly::vfs::BoxFuture<'_, Result<Vfile, VfsError>> {
        Box::pin(async move {
            let data = self.0.lock().unwrap().clone();
            let meta = Metadata::file(data.len() as u64);
            Ok(Vfile::from_bytes(data, meta))
        })
    }
    fn delete(&self, _path: &str) -> ferroly::vfs::BoxFuture<'_, Result<(), VfsError>> {
        Box::pin(async { Err(VfsError::Unsupported("delete".into())) })
    }
    fn exists(&self, _path: &str) -> ferroly::vfs::BoxFuture<'_, bool> {
        Box::pin(async { true })
    }
    fn metadata(&self, _path: &str) -> ferroly::vfs::BoxFuture<'_, Result<Metadata, VfsError>> {
        Box::pin(async move { Ok(Metadata::file(self.0.lock().unwrap().len() as u64)) })
    }
    fn list(
        &self,
        _path: &str,
    ) -> ferroly::vfs::BoxFuture<'_, Result<Vec<ferroly::vfs::Entry>, VfsError>> {
        Box::pin(async move {
            Ok(["b", "a", "c"]
                .into_iter()
                .map(|name| ferroly::vfs::Entry {
                    path: name.to_string(),
                    name: name.to_string(),
                    is_dir: false,
                    len: 0,
                })
                .collect())
        })
    }
    fn mkdir_all(&self, _path: &str) -> ferroly::vfs::BoxFuture<'_, Result<(), VfsError>> {
        Box::pin(async { Ok(()) })
    }
    fn copy(&self, _src: &str, _dst: &str) -> ferroly::vfs::BoxFuture<'_, Result<(), VfsError>> {
        Box::pin(async { Ok(()) })
    }
    fn rename(&self, _src: &str, _dst: &str) -> ferroly::vfs::BoxFuture<'_, Result<(), VfsError>> {
        Box::pin(async { Ok(()) })
    }
    fn walk(
        &self,
        _path: &str,
        _f: ferroly::vfs::WalkFn,
    ) -> ferroly::vfs::BoxFuture<'_, Result<(), VfsError>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn an_external_backend_gets_working_defaults() {
    let backend: Arc<dyn Vfs> = Arc::new(OneFile(std::sync::Mutex::new(b"start".to_vec())));

    backend.append("f", b"+more".to_vec()).await.unwrap();
    assert_eq!(backend.read("f").await.unwrap(), b"start+more");

    let mut writer = backend.create("f").await.unwrap();
    writer.write_all(b"streamed ").await.unwrap();
    writer.write_all(b"contents").await.unwrap();
    assert_eq!(backend.read("f").await.unwrap(), b"start+more", "not yet");
    writer.finish().await.unwrap();
    assert_eq!(backend.read("f").await.unwrap(), b"streamed contents");

    let page = backend.list_page("", None, 2).await.unwrap();
    let names: Vec<&str> = page.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["a", "b"]);
    assert_eq!(page.next.as_deref(), Some("b"));

    // It plugs into the registry and the cross-backend copy.
    let mut registry = VfsRegistry::new();
    registry.register(backend.clone());
    registry.register(Arc::new(MemoryFs::new()));
    registry.copy("one://f", "mem://copy").await.unwrap();
    assert_eq!(
        registry.read("mem://copy").await.unwrap(),
        b"streamed contents"
    );
    assert!(glob_match("**/*.rs", "src/lib.rs"));
}

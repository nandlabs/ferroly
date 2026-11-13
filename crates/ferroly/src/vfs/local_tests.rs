//! Tests for [`LocalFs`], unconfined and rooted.

use std::sync::{Arc, Mutex};

use super::*;
use crate::vfs::testutil::TempRoot;

/// The names in `dir`, sorted — including any stray temporary files.
fn names_on_disk(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn sorted_names(entries: Vec<Entry>) -> Vec<String> {
    let mut names: Vec<String> = entries.into_iter().map(|e| e.name).collect();
    names.sort();
    names
}

// ---- unconfined -------------------------------------------------------------

#[tokio::test]
async fn write_read_range_list_walk_delete() {
    let fs = LocalFs::new();
    let temp = TempRoot::new("main");
    let base = temp.join("work");
    fs.mkdir_all(&base).await.unwrap();

    // write + read
    let file = format!("{base}/a.txt");
    fs.write(&file, b"hello world".to_vec()).await.unwrap();
    assert_eq!(fs.read(&file).await.unwrap(), b"hello world");
    assert!(fs.exists(&file).await);
    assert_eq!(fs.metadata(&file).await.unwrap().len, 11);

    // range read
    assert_eq!(fs.read_range(&file, 6, 5).await.unwrap(), b"world");

    // open + streaming read
    let vf = fs.open(&file).await.unwrap();
    assert!(!vf.metadata().is_dir);
    assert_eq!(vf.read_all_string().await.unwrap(), "hello world");

    // subdir + list
    let sub = format!("{base}/sub");
    fs.mkdir_all(&sub).await.unwrap();
    fs.write(&format!("{sub}/b.txt"), b"x".to_vec())
        .await
        .unwrap();
    let entries = fs.list(&base).await.unwrap();
    assert_eq!(sorted_names(entries), ["a.txt", "sub"]);

    // walk with SkipDir: should not descend into `sub`.
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let s2 = seen.clone();
    fs.walk(
        &base,
        Box::new(move |e: &Entry| {
            s2.lock().unwrap().push(e.name.clone());
            if e.is_dir {
                WalkAction::SkipDir
            } else {
                WalkAction::Continue
            }
        }),
    )
    .await
    .unwrap();
    let names = seen.lock().unwrap().clone();
    assert!(names.contains(&"a.txt".to_string()));
    assert!(names.contains(&"sub".to_string()));
    assert!(!names.contains(&"b.txt".to_string())); // skipped

    // rename + copy
    fs.rename(&file, &format!("{base}/a2.txt")).await.unwrap();
    assert!(!fs.exists(&file).await);
    fs.copy(&format!("{base}/a2.txt"), &format!("{base}/a3.txt"))
        .await
        .unwrap();
    assert!(fs.exists(&format!("{base}/a3.txt")).await);

    // scheme-prefixed paths work too
    fs.write(&format!("file://{base}/c.txt"), b"z".to_vec())
        .await
        .unwrap();
    assert_eq!(fs.read(&format!("{base}/c.txt")).await.unwrap(), b"z");
    assert_eq!(
        fs.read(&format!("FILE://{base}/c.txt")).await.unwrap(),
        b"z"
    );

    // delete the tree
    fs.delete(&base).await.unwrap();
    assert!(!fs.exists(&base).await);
}

#[tokio::test]
async fn not_found_maps_to_sentinel() {
    let fs = LocalFs::new();
    let err = fs.read("/no/such/ferroly/path/xyz").await.unwrap_err();
    assert!(matches!(err, VfsError::NotFound(_)));
}

#[cfg(unix)]
#[tokio::test]
async fn file_url_with_localhost_authority() {
    let fs = LocalFs::new();
    let temp = TempRoot::new("localhost");
    let file = temp.join("a.txt");
    fs.write(&format!("file://localhost{file}"), b"hi".to_vec())
        .await
        .unwrap();
    assert_eq!(fs.read(&format!("file://{file}")).await.unwrap(), b"hi");
}

/// I9: `read_range` must never size its buffer by the caller's `len`.
#[tokio::test]
async fn read_range_is_bounded_by_the_file_not_the_request() {
    let fs = LocalFs::new();
    let temp = TempRoot::new("range");
    let file = temp.join("small.txt");
    fs.write(&file, b"0123456789".to_vec()).await.unwrap();

    // Previously: "capacity overflow" panic.
    assert_eq!(
        fs.read_range(&file, 0, u64::MAX).await.unwrap(),
        b"0123456789"
    );
    // Previously: zero-filled a terabyte before reading 6 bytes.
    let tail = fs.read_range(&file, 4, 1 << 40).await.unwrap();
    assert_eq!(tail, b"456789");
    assert!(tail.capacity() < 1024, "buffer sized by the file");

    assert_eq!(fs.read_range(&file, 2, 3).await.unwrap(), b"234");
    assert_eq!(fs.read_range(&file, 0, 0).await.unwrap(), b"");
    // At and beyond the end: empty, not an error.
    assert_eq!(fs.read_range(&file, 10, 5).await.unwrap(), b"");
    assert_eq!(fs.read_range(&file, 11, 5).await.unwrap(), b"");
    assert_eq!(fs.read_range(&file, u64::MAX, u64::MAX).await.unwrap(), b"");

    assert!(matches!(
        fs.read_range(&temp.join("missing"), 0, 1).await,
        Err(VfsError::NotFound(_))
    ));
    assert!(matches!(
        fs.read_range(&temp.join(""), 0, 1).await,
        Err(VfsError::IsDir(_))
    ));
}

#[tokio::test]
async fn read_range_spans_a_larger_file() {
    let fs = LocalFs::new();
    let temp = TempRoot::new("range-big");
    let file = temp.join("big.bin");
    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    fs.write(&file, data.clone()).await.unwrap();
    assert_eq!(
        fs.read_range(&file, 100_000, 150_000).await.unwrap(),
        data[100_000..250_000]
    );
    assert_eq!(
        fs.read_range(&file, 299_990, 1_000).await.unwrap(),
        data[299_990..]
    );
}

/// Recommendation 9: a foreign scheme is an error, never a local path.
#[tokio::test]
async fn foreign_schemes_are_rejected() {
    let fs = LocalFs::new();
    for url in [
        "s3://bucket/key",
        "gs://bucket/key",
        "http://host/x",
        "mem://x",
    ] {
        assert!(
            matches!(fs.read(url).await, Err(VfsError::InvalidPath { .. })),
            "read {url}"
        );
        assert!(
            matches!(
                fs.write(url, b"x".to_vec()).await,
                Err(VfsError::InvalidPath { .. })
            ),
            "write {url}"
        );
        assert!(matches!(
            fs.mkdir_all(url).await,
            Err(VfsError::InvalidPath { .. })
        ));
        assert!(matches!(
            fs.list(url).await,
            Err(VfsError::InvalidPath { .. })
        ));
        assert!(matches!(
            fs.copy(url, "x").await,
            Err(VfsError::InvalidPath { .. })
        ));
        assert!(!fs.exists(url).await);
    }
    // Nothing named after the scheme was created in the working directory.
    assert!(!Path::new("s3:").exists());
    assert!(!Path::new("bucket").exists());

    let err = fs.read("s3://bucket/key").await.unwrap_err();
    let text = err.to_string();
    assert!(text.contains("s3://bucket/key") && text.contains("LocalFs"));
}

#[tokio::test]
async fn directory_and_file_mismatches_are_typed() {
    let fs = LocalFs::new();
    let temp = TempRoot::new("typed");
    let dir = temp.join("dir");
    let file = temp.join("file.txt");
    fs.mkdir_all(&dir).await.unwrap();
    fs.write(&file, b"x".to_vec()).await.unwrap();

    // A directory where a file is expected.
    assert!(matches!(fs.read(&dir).await, Err(VfsError::IsDir(_))));
    assert!(matches!(fs.open(&dir).await, Err(VfsError::IsDir(_))));
    assert!(matches!(
        fs.write(&dir, b"x".to_vec()).await,
        Err(VfsError::IsDir(_))
    ));
    assert!(matches!(
        fs.append(&dir, b"x".to_vec()).await,
        Err(VfsError::IsDir(_))
    ));
    assert!(matches!(fs.create(&dir).await, Err(VfsError::IsDir(_))));
    assert!(matches!(
        fs.copy(&dir, &temp.join("copy")).await,
        Err(VfsError::IsDir(_))
    ));

    // A file where a directory is expected.
    assert!(matches!(fs.list(&file).await, Err(VfsError::NotDir(_))));
    assert!(matches!(
        fs.list_page(&file, None, 10).await,
        Err(VfsError::NotDir(_))
    ));
    assert!(matches!(
        fs.walk(&file, Box::new(|_| WalkAction::Continue)).await,
        Err(VfsError::NotDir(_))
    ));
    assert!(matches!(
        fs.mkdir_all(&file).await,
        Err(VfsError::NotDir(_))
    ));

    // A file used as a parent directory.
    let under_file = format!("{file}/inner.txt");
    assert!(matches!(
        fs.read(&under_file).await,
        Err(VfsError::NotDir(_))
    ));
    assert!(matches!(
        fs.write(&under_file, b"x".to_vec()).await,
        Err(VfsError::NotDir(_))
    ));
    assert!(matches!(
        fs.mkdir_all(&format!("{file}/sub")).await,
        Err(VfsError::NotDir(_))
    ));

    // A genuinely missing parent is still NotFound.
    assert!(matches!(
        fs.write(&temp.join("missing/x.txt"), b"x".to_vec()).await,
        Err(VfsError::NotFound(_))
    ));
    assert!(matches!(
        fs.list(&temp.join("missing")).await,
        Err(VfsError::NotFound(_))
    ));
    // The failed writes left no temporary files behind.
    assert_eq!(names_on_disk(temp.path()), ["dir", "file.txt"]);
}

#[tokio::test]
async fn write_is_atomic_and_leaves_no_temporaries() {
    let fs = LocalFs::new();
    let temp = TempRoot::new("atomic");
    let file = temp.join("state.bin");
    fs.write(&file, vec![b'a'; 64 * 1024]).await.unwrap();

    // One writer flips the contents back and forth while readers watch:
    // every read must see one whole version.
    let writer = {
        let (fs, file) = (fs.clone(), file.clone());
        tokio::spawn(async move {
            for round in 0..40u8 {
                let fill = if round % 2 == 0 { b'b' } else { b'a' };
                let result = fs.write(&file, vec![fill; 64 * 1024]).await;
                // On Windows a rename can be refused while a reader has the
                // destination open; such a write fails cleanly, which is
                // within the contract being tested.
                #[cfg(not(windows))]
                result.unwrap();
                #[cfg(windows)]
                let _ = result;
            }
        })
    };
    let mut reads = 0;
    while !writer.is_finished() {
        // A read can lose a race with the rename on Windows; whole-or-nothing
        // is what is being checked.
        if let Ok(data) = fs.read(&file).await {
            assert_eq!(data.len(), 64 * 1024, "partial file observed");
            assert!(data.iter().all(|&b| b == data[0]), "mixed contents");
            reads += 1;
        }
        tokio::task::yield_now().await;
    }
    writer.await.unwrap();
    assert!(reads > 0);
    assert_eq!(names_on_disk(temp.path()), ["state.bin"]);

    // Replacing with shorter and with empty contents.
    fs.write(&file, b"short".to_vec()).await.unwrap();
    assert_eq!(fs.read(&file).await.unwrap(), b"short");
    fs.write(&file, Vec::new()).await.unwrap();
    assert_eq!(fs.read(&file).await.unwrap(), b"");
}

#[cfg(unix)]
#[tokio::test]
async fn write_keeps_permissions_and_refuses_read_only_files() {
    use std::os::unix::fs::PermissionsExt;
    let fs = LocalFs::new();
    let temp = TempRoot::new("modes");
    let file = temp.join("script.sh");
    fs.write(&file, b"v1".to_vec()).await.unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o750)).unwrap();
    fs.write(&file, b"v2".to_vec()).await.unwrap();
    let meta = fs.metadata(&file).await.unwrap();
    assert_eq!(meta.mode, Some(0o750));
    assert!(!meta.readonly);

    // A rename would happily replace a read-only file; the backend refuses.
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o440)).unwrap();
    assert!(fs.metadata(&file).await.unwrap().readonly);
    assert!(matches!(
        fs.write(&file, b"v3".to_vec()).await,
        Err(VfsError::Permission(_))
    ));
    assert!(matches!(
        fs.create(&file).await,
        Err(VfsError::Permission(_))
    ));
    assert_eq!(std::fs::read(&file).unwrap(), b"v2");
    assert_eq!(names_on_disk(temp.path()), ["script.sh"]);
}

#[cfg(unix)]
#[tokio::test]
async fn write_follows_a_symlink_to_its_target() {
    let fs = LocalFs::new();
    let temp = TempRoot::new("link-write");
    let target = temp.join("target.txt");
    let link = temp.join("link.txt");
    std::fs::write(&target, b"old").unwrap();
    std::os::unix::fs::symlink("target.txt", &link).unwrap();

    fs.write(&link, b"new".to_vec()).await.unwrap();
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(std::fs::read(&target).unwrap(), b"new");

    let meta = fs.metadata(&link).await.unwrap();
    assert!(meta.is_symlink && meta.is_file());
    assert_eq!(meta.len, 3);
    assert!(!fs.metadata(&target).await.unwrap().is_symlink);

    // A dangling link is written through, like a plain create would.
    let dangling = temp.join("dangling.txt");
    std::os::unix::fs::symlink("made-later.txt", &dangling).unwrap();
    fs.write(&dangling, b"made".to_vec()).await.unwrap();
    assert_eq!(
        std::fs::read(temp.path().join("made-later.txt")).unwrap(),
        b"made"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn delete_removes_a_link_not_its_target() {
    let fs = LocalFs::new();
    let temp = TempRoot::new("link-delete");
    let real_dir = temp.join("real");
    std::fs::create_dir(&real_dir).unwrap();
    std::fs::write(temp.path().join("real/keep.txt"), b"keep").unwrap();
    let link = temp.join("link");
    std::os::unix::fs::symlink(&real_dir, &link).unwrap();

    fs.delete(&link).await.unwrap();
    assert!(std::fs::symlink_metadata(&link).is_err());
    assert_eq!(
        std::fs::read(temp.path().join("real/keep.txt")).unwrap(),
        b"keep"
    );
}

#[tokio::test]
async fn append_creates_then_extends() {
    let fs = LocalFs::new();
    let temp = TempRoot::new("append");
    let file = temp.join("log.txt");
    fs.append(&file, b"one\n".to_vec()).await.unwrap();
    fs.append(&file, b"two\n".to_vec()).await.unwrap();
    fs.append(&file, Vec::new()).await.unwrap();
    assert_eq!(fs.read(&file).await.unwrap(), b"one\ntwo\n");
    assert!(matches!(
        fs.append(&temp.join("missing/log.txt"), b"x".to_vec())
            .await,
        Err(VfsError::NotFound(_))
    ));
}

#[tokio::test]
async fn streaming_write_is_published_on_finish() {
    let fs = LocalFs::new();
    let temp = TempRoot::new("stream");
    let file = temp.join("out.bin");
    fs.write(&file, b"previous".to_vec()).await.unwrap();

    let mut writer = fs.create(&file).await.unwrap();
    let chunk = vec![7u8; 10_000];
    for _ in 0..30 {
        writer.write_all(&chunk).await.unwrap();
    }
    assert_eq!(writer.bytes_written(), 300_000);
    // Not visible yet: the old contents are still there.
    assert_eq!(fs.read(&file).await.unwrap(), b"previous");
    assert_eq!(writer.finish().await.unwrap(), 300_000);
    let data = fs.read(&file).await.unwrap();
    assert_eq!(data.len(), 300_000);
    assert!(data.iter().all(|&b| b == 7));
    assert_eq!(names_on_disk(temp.path()), ["out.bin"]);

    // Dropped without finishing: the draft is discarded.
    let mut abandoned = fs.create(&file).await.unwrap();
    abandoned.write_all(b"never published").await.unwrap();
    assert_eq!(names_on_disk(temp.path()).len(), 2);
    drop(abandoned);
    assert_eq!(names_on_disk(temp.path()), ["out.bin"]);
    assert_eq!(fs.read(&file).await.unwrap().len(), 300_000);

    // A new file through the streaming path.
    let fresh = temp.join("fresh.bin");
    let mut writer = fs.create(&fresh).await.unwrap();
    writer.write_all(b"abc").await.unwrap();
    assert!(!fs.exists(&fresh).await);
    writer.finish().await.unwrap();
    assert_eq!(fs.read(&fresh).await.unwrap(), b"abc");

    assert!(matches!(
        fs.create(&temp.join("missing/x.bin")).await,
        Err(VfsError::NotFound(_))
    ));
}

#[tokio::test]
async fn metadata_reports_times_and_kind() {
    let fs = LocalFs::new();
    let temp = TempRoot::new("meta");
    let file = temp.join("a.txt");
    let before = std::time::SystemTime::now() - std::time::Duration::from_secs(5);
    fs.write(&file, b"12345".to_vec()).await.unwrap();

    let meta = fs.metadata(&file).await.unwrap();
    assert!(meta.is_file() && !meta.is_dir && !meta.is_symlink && !meta.readonly);
    assert_eq!(meta.len, 5);
    let modified = meta.modified.expect("modification time");
    assert!(modified >= before);
    #[cfg(unix)]
    assert!(meta.mode.is_some());

    let dir = fs.metadata(&temp.join("")).await.unwrap();
    assert!(dir.is_dir && !dir.is_file());
    assert_eq!(dir.len, 0);

    assert!(matches!(
        fs.metadata(&temp.join("missing")).await,
        Err(VfsError::NotFound(_))
    ));
    // The open handle carries the same information.
    let opened = fs.open(&file).await.unwrap();
    assert_eq!(opened.metadata().len, 5);
    assert_eq!(opened.metadata().modified, Some(modified));
}

#[tokio::test]
async fn list_page_visits_everything_once_in_name_order() {
    let fs = LocalFs::new();
    let temp = TempRoot::new("page");
    let dir = temp.join("");
    let mut expected = Vec::new();
    for i in 0..25 {
        let name = format!("f{i:02}.txt");
        fs.write(&temp.join(&name), vec![b'x'; i]).await.unwrap();
        expected.push(name);
    }
    fs.mkdir_all(&temp.join("zdir")).await.unwrap();
    expected.push("zdir".to_string());

    let mut seen = Vec::new();
    let mut after = None;
    let mut pages = 0;
    loop {
        let page = fs.list_page(&dir, after.clone(), 10).await.unwrap();
        assert!(page.entries.len() <= 10);
        pages += 1;
        seen.extend(page.entries.iter().map(|e| e.name.clone()));
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, expected);
    assert_eq!(pages, 3);

    // Sizes and kinds are filled in for the page.
    let first = fs.list_page(&dir, None, 3).await.unwrap();
    assert_eq!(first.entries[2].len, 2);
    assert!(!first.entries[0].is_dir);
    assert_eq!(first.next.as_deref(), Some("f02.txt"));
    let last = fs
        .list_page(&dir, Some("f24.txt".to_string()), 5)
        .await
        .unwrap();
    assert_eq!(last.entries.len(), 1);
    assert!(last.entries[0].is_dir);
    assert_eq!(last.next, None);
    // Exactly filling the last page does not promise another.
    let exact = fs
        .list_page(&dir, Some("f23.txt".to_string()), 2)
        .await
        .unwrap();
    assert_eq!(exact.entries.len(), 2);
    assert_eq!(exact.next, None);
}

// ---- rooted -------------------------------------------------------------------

#[tokio::test]
async fn rooted_requires_an_existing_directory() {
    let temp = TempRoot::new("rooted-new");
    assert!(matches!(
        LocalFs::rooted(temp.path().join("missing")),
        Err(VfsError::NotFound(_))
    ));
    std::fs::write(temp.path().join("file.txt"), b"x").unwrap();
    assert!(matches!(
        LocalFs::rooted(temp.path().join("file.txt")),
        Err(VfsError::NotDir(_))
    ));
    let fs = LocalFs::rooted(temp.path()).unwrap();
    assert_eq!(
        fs.root().unwrap(),
        std::fs::canonicalize(temp.path()).unwrap()
    );
    assert!(LocalFs::new().root().is_none());
}

#[tokio::test]
async fn rooted_operations_use_relative_paths() {
    let temp = TempRoot::new("rooted-ops");
    let fs = LocalFs::rooted(temp.path()).unwrap();

    fs.mkdir_all("docs/sub").await.unwrap();
    fs.write("docs/a.txt", b"alpha".to_vec()).await.unwrap();
    fs.write("./docs/sub/b.txt", b"beta".to_vec())
        .await
        .unwrap();
    fs.write("file://top.txt", b"top".to_vec()).await.unwrap();
    assert_eq!(std::fs::read(temp.path().join("top.txt")).unwrap(), b"top");
    assert_eq!(fs.read("docs/a.txt").await.unwrap(), b"alpha");
    assert_eq!(fs.read("docs//sub/./b.txt").await.unwrap(), b"beta");
    assert_eq!(fs.read_range("docs/a.txt", 1, 3).await.unwrap(), b"lph");
    assert!(fs.exists("docs/sub").await);
    assert!(fs.exists("").await && fs.exists(".").await);
    assert!(fs.metadata(".").await.unwrap().is_dir);

    // Entry paths are root-relative, `/`-separated, and usable as-is.
    let mut entries = fs.list("docs").await.unwrap();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, ["docs/a.txt", "docs/sub"]);
    assert_eq!(fs.read(&entries[0].path).await.unwrap(), b"alpha");
    assert_eq!(
        sorted_names(fs.list("").await.unwrap()),
        ["docs", "top.txt"]
    );
    let top: Vec<String> = fs
        .list(".")
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.path)
        .collect();
    assert!(top.contains(&"docs".to_string()) && top.contains(&"top.txt".to_string()));

    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = seen.clone();
    fs.walk(
        "",
        Box::new(move |e| {
            sink.lock().unwrap().push(e.path.clone());
            WalkAction::Continue
        }),
    )
    .await
    .unwrap();
    let mut walked = seen.lock().unwrap().clone();
    walked.sort();
    assert_eq!(
        walked,
        [
            "docs",
            "docs/a.txt",
            "docs/sub",
            "docs/sub/b.txt",
            "top.txt"
        ]
    );
    for path in &walked {
        assert!(fs.exists(path).await, "{path} feeds back");
    }

    let page = fs.list_page("docs", None, 1).await.unwrap();
    assert_eq!(page.entries[0].path, "docs/a.txt");
    assert_eq!(page.next.as_deref(), Some("a.txt"));

    fs.copy("docs/a.txt", "docs/sub/a-copy.txt").await.unwrap();
    fs.rename("docs/sub/a-copy.txt", "moved.txt").await.unwrap();
    assert_eq!(
        std::fs::read(temp.path().join("moved.txt")).unwrap(),
        b"alpha"
    );
    fs.append("moved.txt", b"!".to_vec()).await.unwrap();
    let mut writer = fs.create("docs/stream.bin").await.unwrap();
    writer.write_all(b"streamed").await.unwrap();
    writer.finish().await.unwrap();
    assert_eq!(fs.read("docs/stream.bin").await.unwrap(), b"streamed");

    // Errors show the caller's relative path, not the absolute one.
    match fs.read("docs/missing.txt").await {
        Err(VfsError::NotFound(path)) => assert_eq!(path, "docs/missing.txt"),
        other => panic!("unexpected: {other:?}"),
    }

    fs.delete("docs").await.unwrap();
    assert!(!temp.path().join("docs").exists());
    assert!(fs.exists("moved.txt").await);
}

#[tokio::test]
async fn rooted_rejects_traversal_and_absolute_paths() {
    let temp = TempRoot::new("rooted-escape");
    let inner = temp.path().join("jail");
    std::fs::create_dir(&inner).unwrap();
    std::fs::write(temp.path().join("secret.txt"), b"secret").unwrap();
    let fs = LocalFs::rooted(&inner).unwrap();
    fs.mkdir_all("a").await.unwrap();

    let secret_abs = temp.join("secret.txt");
    let escapes = [
        "../secret.txt",
        "a/../../secret.txt",
        "a/../a", // `..` is refused even when it would stay inside
        "..",
        "/etc/passwd",
        "/",
        secret_abs.as_str(),
        "file:///etc/passwd",
        "file://../secret.txt",
    ];
    for path in escapes {
        let invalid = |r: Result<(), VfsError>| matches!(r, Err(VfsError::InvalidPath { .. }));
        assert!(invalid(fs.read(path).await.map(|_| ())), "read {path}");
        assert!(
            invalid(fs.read_range(path, 0, 1).await.map(|_| ())),
            "read_range {path}"
        );
        assert!(invalid(fs.open(path).await.map(|_| ())), "open {path}");
        assert!(
            invalid(fs.write(path, b"pwned".to_vec()).await),
            "write {path}"
        );
        assert!(
            invalid(fs.append(path, b"pwned".to_vec()).await),
            "append {path}"
        );
        assert!(invalid(fs.create(path).await.map(|_| ())), "create {path}");
        assert!(invalid(fs.delete(path).await), "delete {path}");
        assert!(invalid(fs.metadata(path).await.map(|_| ())), "meta {path}");
        assert!(invalid(fs.list(path).await.map(|_| ())), "list {path}");
        assert!(
            invalid(fs.list_page(path, None, 5).await.map(|_| ())),
            "list_page {path}"
        );
        assert!(invalid(fs.mkdir_all(path).await), "mkdir_all {path}");
        assert!(invalid(fs.copy(path, "x").await), "copy from {path}");
        assert!(invalid(fs.copy("a", path).await), "copy to {path}");
        assert!(invalid(fs.rename(path, "x").await), "rename from {path}");
        assert!(invalid(fs.rename("a", path).await), "rename to {path}");
        assert!(
            invalid(fs.walk(path, Box::new(|_| WalkAction::Continue)).await),
            "walk {path}"
        );
        assert!(!fs.exists(path).await, "exists {path}");
    }
    assert_eq!(
        std::fs::read(temp.path().join("secret.txt")).unwrap(),
        b"secret"
    );
    assert_eq!(names_on_disk(temp.path()), ["jail", "secret.txt"]);
    assert_eq!(names_on_disk(&inner), ["a"]);
    assert!(matches!(
        fs.read("s3://bucket/key").await,
        Err(VfsError::InvalidPath { .. })
    ));
}

#[tokio::test]
async fn rooted_protects_the_root_itself() {
    let temp = TempRoot::new("rooted-self");
    let fs = LocalFs::rooted(temp.path()).unwrap();
    fs.write("a.txt", b"x".to_vec()).await.unwrap();
    for root in ["", ".", "./"] {
        assert!(matches!(
            fs.delete(root).await,
            Err(VfsError::InvalidPath { .. })
        ));
        assert!(matches!(
            fs.rename(root, "elsewhere").await,
            Err(VfsError::InvalidPath { .. })
        ));
        assert!(matches!(
            fs.rename("a.txt", root).await,
            Err(VfsError::InvalidPath { .. })
        ));
        assert!(matches!(
            fs.write(root, b"x".to_vec()).await,
            Err(VfsError::IsDir(_))
        ));
    }
    assert!(temp.path().join("a.txt").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn rooted_rejects_symlinks_that_escape() {
    use std::os::unix::fs::symlink;
    let temp = TempRoot::new("rooted-links");
    let jail = temp.path().join("jail");
    let outside = temp.path().join("outside");
    std::fs::create_dir(&jail).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
    std::fs::create_dir(jail.join("real")).unwrap();
    std::fs::write(jail.join("real/ok.txt"), b"ok").unwrap();

    symlink(&outside, jail.join("dir-out")).unwrap(); // dir link, absolute
    symlink("../outside/secret.txt", jail.join("file-out")).unwrap(); // file link, relative
    symlink("../outside/not-yet.txt", jail.join("dangling-out")).unwrap();
    symlink("real", jail.join("dir-in")).unwrap(); // stays inside
    symlink("real/ok.txt", jail.join("file-in")).unwrap();
    symlink("nowhere", jail.join("dangling-in")).unwrap();

    let fs = LocalFs::rooted(&jail).unwrap();
    let invalid = |r: Result<(), VfsError>| matches!(r, Err(VfsError::InvalidPath { .. }));

    // Through a directory link that leaves the root.
    assert!(invalid(fs.read("dir-out/secret.txt").await.map(|_| ())));
    assert!(invalid(fs.list("dir-out").await.map(|_| ())));
    assert!(invalid(fs.write("dir-out/new.txt", b"x".to_vec()).await));
    assert!(invalid(fs.mkdir_all("dir-out/sub/deeper").await));
    assert!(invalid(
        fs.append("dir-out/secret.txt", b"x".to_vec()).await
    ));
    assert!(invalid(fs.create("dir-out/new.txt").await.map(|_| ())));
    assert!(invalid(fs.copy("real/ok.txt", "dir-out/copied.txt").await));
    assert!(invalid(fs.rename("real/ok.txt", "dir-out/moved.txt").await));
    assert!(invalid(fs.delete("dir-out/secret.txt").await));
    assert!(invalid(
        fs.walk("dir-out", Box::new(|_| WalkAction::Continue)).await
    ));
    assert!(!fs.exists("dir-out/secret.txt").await);

    // A file link that leaves the root.
    assert!(invalid(fs.read("file-out").await.map(|_| ())));
    assert!(invalid(fs.open("file-out").await.map(|_| ())));
    assert!(invalid(fs.write("file-out", b"pwned".to_vec()).await));
    assert!(invalid(fs.append("file-out", b"pwned".to_vec()).await));
    assert!(invalid(fs.metadata("file-out").await.map(|_| ())));
    assert!(invalid(fs.copy("file-out", "stolen.txt").await));

    // A dangling link must not be written through to create its target.
    assert!(invalid(fs.write("dangling-out", b"x".to_vec()).await));
    assert!(invalid(fs.create("dangling-out").await.map(|_| ())));
    assert!(invalid(fs.write("dangling-in", b"x".to_vec()).await));

    // Nothing outside changed or appeared.
    assert_eq!(
        std::fs::read(outside.join("secret.txt")).unwrap(),
        b"secret"
    );
    assert_eq!(names_on_disk(&outside), ["secret.txt"]);
    assert!(!jail.join("stolen.txt").exists());

    // Links that stay inside the root work.
    assert_eq!(fs.read("dir-in/ok.txt").await.unwrap(), b"ok");
    assert_eq!(fs.read("file-in").await.unwrap(), b"ok");
    assert_eq!(fs.list("dir-in").await.unwrap().len(), 1);
    fs.write("file-in", b"changed".to_vec()).await.unwrap();
    assert_eq!(std::fs::read(jail.join("real/ok.txt")).unwrap(), b"changed");
    let meta = fs.metadata("file-in").await.unwrap();
    assert!(meta.is_symlink && meta.is_file());

    // A walk reports links as plain entries and never descends through them.
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = seen.clone();
    fs.walk(
        "",
        Box::new(move |e| {
            sink.lock().unwrap().push(e.path.clone());
            WalkAction::Continue
        }),
    )
    .await
    .unwrap();
    let walked = seen.lock().unwrap().clone();
    assert!(walked.contains(&"dir-out".to_string()));
    assert!(walked.contains(&"real/ok.txt".to_string()));
    assert!(!walked.iter().any(|p| p.contains("secret")));
    assert!(!walked.iter().any(|p| p.starts_with("dir-in/")));

    // Deleting or renaming an escaping link acts on the link, not its target.
    fs.rename("dir-out", "dir-out-renamed").await.unwrap();
    fs.delete("dir-out-renamed").await.unwrap();
    fs.delete("file-out").await.unwrap();
    fs.delete("dangling-out").await.unwrap();
    assert!(std::fs::symlink_metadata(jail.join("file-out")).is_err());
    assert_eq!(
        std::fs::read(outside.join("secret.txt")).unwrap(),
        b"secret"
    );
    assert_eq!(names_on_disk(&outside), ["secret.txt"]);
}

#[cfg(unix)]
#[tokio::test]
async fn rooted_follows_a_symlinked_root() {
    // The root itself may be reached through a link; it is canonicalized
    // once, at construction.
    let temp = TempRoot::new("rooted-linked-root");
    let real = temp.path().join("real-root");
    std::fs::create_dir(&real).unwrap();
    let link = temp.path().join("root-link");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let fs = LocalFs::rooted(&link).unwrap();
    assert_eq!(fs.root().unwrap(), std::fs::canonicalize(&real).unwrap());
    fs.write("a.txt", b"x".to_vec()).await.unwrap();
    assert_eq!(std::fs::read(real.join("a.txt")).unwrap(), b"x");
}

#![cfg(feature = "store-local")]
//! The synchronous local store (`store-local`): layouts, listing, short
//! digests, non-destructive checks, inserts, housekeeping and the
//! cross-process store lock. Plain `#[test]`s: no async runtime involved.

mod store_common;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use ferroly::store::{Digest, Layout, LocalStore, ObjectStatus, StoreError, MIN_PREFIX_HEX};
use store_common::{content, files_in, Scratch};

/// Five distinct blobs.
fn blobs() -> Vec<Vec<u8>> {
    (0..5).map(|i| content(100 + i * 37)).collect()
}

fn layouts() -> Vec<Layout> {
    vec![Layout::Fanout, Layout::flat("blobs").unwrap()]
}

fn sorted_digests(data: &[Vec<u8>]) -> Vec<Digest> {
    let mut digests: Vec<Digest> = data.iter().map(|d| Digest::of(d)).collect();
    digests.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    digests
}

/// Writes a file named `hex` straight into the objects tree (content need
/// not match: for tests that only look at names).
fn plant(store: &LocalStore, hex: &str, data: &[u8]) -> PathBuf {
    let digest = Digest::from_hex(hex).unwrap();
    let path = store.path_for(&digest);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, data).unwrap();
    path
}

// ---------------------------------------------------------------- (a) layout

#[test]
fn layout_fanout_is_the_default() {
    let dir = Scratch::new("local-default");
    let a = LocalStore::open(dir.path()).unwrap();
    let b = LocalStore::open_with(dir.path(), Layout::Fanout).unwrap();
    assert_eq!(a.layout(), &Layout::Fanout);
    let d = Digest::of(b"x");
    let hex = d.hex();
    assert_eq!(a.path_for(&d), b.path_for(&d));
    assert!(a
        .path_for(&d)
        .ends_with(Path::new("sha256").join(&hex[..2]).join(&hex)));
    assert_eq!(a.root(), dir.path());
}

#[test]
fn layout_flat_paths() {
    let dir = Scratch::new("local-flat");
    let store = LocalStore::open_with(dir.path(), Layout::flat("blobs").unwrap()).unwrap();
    let d = Digest::of(b"flat");
    let expected = dir.path().join("blobs").join("sha256").join(d.hex());
    assert_eq!(store.path_for(&d), expected);
    let stored = store.insert_bytes(b"flat").unwrap();
    assert_eq!(stored.digest, d);
    assert_eq!(stored.path, expected);
    assert_eq!(std::fs::read(&expected).unwrap(), b"flat");
    assert_eq!(store.get(&d), Some(expected));
    assert!(store.contains(&d));
}

#[test]
fn layout_flat_rejects_bad_prefixes() {
    for bad in ["", "/abs", "../x", "a/../../b", "tmp", "partial", ".lock"] {
        let err = Layout::flat(bad).unwrap_err();
        assert!(matches!(err, StoreError::Config(_)), "{bad:?}: {err:?}");
    }
    // A `Flat` built directly is checked by `open_with`.
    let dir = Scratch::new("local-flat-bad");
    let raw = Layout::Flat {
        prefix: PathBuf::from("../escape"),
    };
    let err = LocalStore::open_with(dir.path(), raw).unwrap_err();
    assert!(matches!(err, StoreError::Config(_)), "{err:?}");
    assert!(!dir.path().parent().unwrap().join("escape").exists());
}

#[test]
fn layouts_do_not_see_each_other() {
    let dir = Scratch::new("local-two-layouts");
    let fan = LocalStore::open(dir.path()).unwrap();
    let flat = LocalStore::open_with(dir.path(), Layout::flat("blobs").unwrap()).unwrap();
    let a = fan.insert_bytes(b"in fanout").unwrap();
    let b = flat.insert_bytes(b"in flat").unwrap();
    assert!(!flat.contains(&a.digest));
    assert!(!fan.contains(&b.digest));
    assert_eq!(fan.list().unwrap().len(), 1);
    assert_eq!(flat.list().unwrap().len(), 1);
}

#[cfg(unix)]
#[test]
fn flat_layout_directories_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Scratch::new("local-flat-perm");
    LocalStore::open_with(dir.path(), Layout::flat("blobs").unwrap()).unwrap();
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    for sub in ["blobs", "blobs/sha256", "tmp", "partial"] {
        assert_eq!(mode(&dir.path().join(sub)) & 0o077, 0, "{sub}");
    }
}

// ------------------------------------------------------ (b) list and prefixes

#[test]
fn list_is_sorted_and_complete() {
    for layout in layouts() {
        let dir = Scratch::new("local-list");
        let store = LocalStore::open_with(dir.path(), layout.clone()).unwrap();
        let data = blobs();
        for d in &data {
            store.insert_bytes(d).unwrap();
        }
        let listed = store.list().unwrap();
        let digests: Vec<Digest> = listed.iter().map(|o| o.digest).collect();
        assert_eq!(digests, sorted_digests(&data), "{layout:?}");
        for info in &listed {
            let blob = data.iter().find(|d| Digest::of(d) == info.digest).unwrap();
            assert_eq!(info.size, blob.len() as u64);
            assert_eq!(info.path, store.path_for(&info.digest));
        }
        let total: u64 = data.iter().map(|d| d.len() as u64).sum();
        assert_eq!(store.usage().unwrap(), total);
    }
}

#[test]
fn list_skips_foreign_entries() {
    for layout in layouts() {
        let dir = Scratch::new("local-foreign");
        let store = LocalStore::open_with(dir.path(), layout.clone()).unwrap();
        let data = blobs();
        for d in &data {
            store.insert_bytes(d).unwrap();
        }
        let real = Digest::of(&data[0]);
        let real_path = store.path_for(&real);
        let objects_dir = match layout {
            Layout::Fanout => dir.path().join("sha256"),
            _ => dir.path().join("blobs").join("sha256"),
        };
        // Next to a real object: a README, an upper-case name, a 63-digit
        // name and a directory with an object's name. (The upper-case name
        // is not the real object's: on a case-insensitive filesystem that
        // would overwrite it.)
        let beside = real_path.parent().unwrap();
        std::fs::write(beside.join("README"), b"not an object").unwrap();
        let mut upper = Digest::of(b"upper").hex().to_uppercase();
        upper.replace_range(..2, &real.hex()[..2]);
        std::fs::write(beside.join(upper), b"upper").unwrap();
        std::fs::write(beside.join(&Digest::of(b"63").hex()[..63]), b"short").unwrap();
        std::fs::create_dir_all(beside.join(Digest::of(b"a dir").hex())).unwrap();
        std::fs::write(objects_dir.join("README"), b"top-level stray").unwrap();
        if layout == Layout::Fanout {
            // A correctly named object in the wrong fan directory.
            let stray = Digest::of(b"misplaced").hex();
            let wrong = if &stray[..2] == "00" { "01" } else { "00" };
            std::fs::create_dir_all(objects_dir.join(wrong)).unwrap();
            std::fs::write(objects_dir.join(wrong).join(&stray), b"misplaced").unwrap();
        }
        let listed: Vec<Digest> = store.list().unwrap().iter().map(|o| o.digest).collect();
        assert_eq!(listed, sorted_digests(&data), "{layout:?}");
        let total: u64 = data.iter().map(|d| d.len() as u64).sum();
        assert_eq!(store.usage().unwrap(), total, "{layout:?}");
    }
}

#[test]
fn list_empty_and_missing_dir() {
    for layout in layouts() {
        let dir = Scratch::new("local-empty");
        let store = LocalStore::open_with(dir.path(), layout.clone()).unwrap();
        assert!(store.list().unwrap().is_empty());
        let objects = match layout {
            Layout::Fanout => dir.path().join("sha256"),
            _ => dir.path().join("blobs"),
        };
        std::fs::remove_dir_all(objects).unwrap();
        assert_eq!(store.list().unwrap(), vec![]);
        assert_eq!(store.usage().unwrap(), 0);
        assert!(store.check_all().unwrap().is_empty());
        // Inserting recreates what is missing.
        let stored = store.insert_bytes(b"back").unwrap();
        assert!(store.contains(&stored.digest));
    }
}

#[test]
fn resolve_unique_prefix() {
    for layout in layouts() {
        let dir = Scratch::new("local-resolve");
        let store = LocalStore::open_with(dir.path(), layout).unwrap();
        for d in blobs() {
            store.insert_bytes(&d).unwrap();
        }
        let target = Digest::of(&blobs()[2]);
        let hex = target.hex();
        for query in [
            hex[..12].to_string(),
            hex[..20].to_string(),
            format!("sha256:{}", &hex[..12]),
            format!("SHA256:{}", &hex[..20]),
            hex[..12].to_uppercase(),
            format!("sha256:{}", hex[..16].to_uppercase()),
        ] {
            assert_eq!(store.resolve_prefix(&query).unwrap(), target, "{query}");
        }
    }
}

#[test]
fn resolve_full_digest() {
    let dir = Scratch::new("local-resolve-full");
    let store = LocalStore::open(dir.path()).unwrap();
    let stored = store.insert_bytes(b"full").unwrap();
    assert_eq!(
        store.resolve_prefix(&stored.digest.to_string()).unwrap(),
        stored.digest
    );
    assert_eq!(
        store.resolve_prefix(&stored.digest.hex()).unwrap(),
        stored.digest
    );
    let absent = Digest::of(b"absent");
    let err = store.resolve_prefix(&absent.to_string()).unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)), "{err:?}");
    assert!(err.is_not_found());
}

#[test]
fn resolve_ignores_a_stray_file_at_a_fan_name() {
    let dir = Scratch::new("local-resolve-stray");
    let store = LocalStore::open(dir.path()).unwrap();
    // A regular file where the `ab` fan directory would be.
    std::fs::write(dir.path().join("sha256").join("ab"), b"stray").unwrap();
    assert!(store.list().unwrap().is_empty());
    for query in [
        "ab0123456789".to_string(),
        "ab".to_string() + &"0".repeat(20),
        "ab".to_string() + &"0".repeat(62),
    ] {
        let err = store.resolve_prefix(&query).unwrap_err();
        assert!(err.is_not_found(), "{query}: {err:?}");
    }
}

/// Symbolic links are not objects for `list`, so not for `resolve_prefix`
/// either: neither a linked fan directory nor a link at an object's path.
#[cfg(unix)]
#[test]
fn resolve_ignores_links_as_list_does() {
    use std::os::unix::fs::symlink;
    let dir = Scratch::new("local-resolve-links");
    let store = LocalStore::open(dir.path()).unwrap();
    let objects = dir.path().join("sha256");

    // A fan directory that is a link to a directory holding an object.
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let via_fan = Digest::of(b"via a linked fan");
    std::fs::write(outside.join(via_fan.hex()), b"via a linked fan").unwrap();
    symlink(&outside, objects.join(&via_fan.hex()[..2])).unwrap();

    // A link at an object's path, in a real fan directory.
    let via_file = (0..)
        .map(|i| Digest::of(format!("linked object {i}").as_bytes()))
        .find(|d| d.hex()[..2] != via_fan.hex()[..2])
        .unwrap();
    let target = dir.path().join("outside-file");
    std::fs::write(&target, b"linked object").unwrap();
    let path = store.path_for(&via_file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    symlink(&target, &path).unwrap();

    assert!(store.list().unwrap().is_empty());
    for digest in [via_fan, via_file] {
        let hex = digest.hex();
        for query in [hex[..12].to_string(), hex.clone(), digest.to_string()] {
            let err = store.resolve_prefix(&query).unwrap_err();
            assert!(err.is_not_found(), "{query}: {err:?}");
        }
    }
}

#[test]
fn resolve_too_short_or_bad() {
    let dir = Scratch::new("local-resolve-bad");
    let store = LocalStore::open(dir.path()).unwrap();
    let hex = store.insert_bytes(b"bad").unwrap().digest.hex();
    let long = "a".repeat(10_000);
    let xyz = format!("xyz{}", &hex[3..20]);
    let too_long = format!("{hex}0");
    let bad_algo = format!("md5:{}", &hex[..12]);
    for bad in [
        &hex[..11],
        "",
        xyz.as_str(),
        too_long.as_str(),
        bad_algo.as_str(),
        "sha256:",
        long.as_str(),
    ] {
        let err = store.resolve_prefix(bad).unwrap_err();
        assert!(
            matches!(err, StoreError::InvalidDigest(_)),
            "{bad:?}: {err:?}"
        );
        assert!(err.to_string().len() < 300, "error echoes unbounded input");
    }
    let err = store.resolve_prefix(&hex[..11]).unwrap_err().to_string();
    assert!(err.contains(&MIN_PREFIX_HEX.to_string()), "{err}");
}

#[test]
fn resolve_not_found() {
    for layout in layouts() {
        let dir = Scratch::new("local-resolve-none");
        let store = LocalStore::open_with(dir.path(), layout).unwrap();
        let present = store.insert_bytes(b"present").unwrap().digest.hex();
        // Same fan directory, different twelfth digit.
        let mut query = present[..12].to_string();
        let last = if query.ends_with('0') { "1" } else { "0" };
        query.replace_range(11..12, last);
        let err = store.resolve_prefix(&query).unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)), "{err:?}");
        assert!(err.is_not_found());
    }
}

#[test]
fn resolve_ambiguous() {
    for layout in layouts() {
        let dir = Scratch::new("local-resolve-ambiguous");
        let store = LocalStore::open_with(dir.path(), layout.clone()).unwrap();
        let prefix = "abcdef012345";
        let name = |i: usize| format!("{prefix}{i:052x}");
        plant(&store, &name(0x70), b"seventy");
        plant(&store, &name(3), b"three");
        match store.resolve_prefix(prefix).unwrap_err() {
            StoreError::AmbiguousPrefix {
                prefix: shown,
                candidates,
                matches,
            } => {
                assert_eq!(shown, format!("sha256:{prefix}"));
                assert_eq!(matches, 2);
                assert_eq!(
                    candidates,
                    vec![
                        Digest::from_hex(&name(3)).unwrap(),
                        Digest::from_hex(&name(0x70)).unwrap()
                    ],
                    "sorted"
                );
            }
            other => panic!("{layout:?}: {other:?}"),
        }
        // A longer prefix tells them apart.
        let longer = &name(3)[..63];
        assert_eq!(
            store.resolve_prefix(longer).unwrap(),
            Digest::from_hex(&name(3)).unwrap()
        );

        for i in 10..18 {
            plant(&store, &name(i), b"more");
        }
        match store.resolve_prefix(prefix).unwrap_err() {
            StoreError::AmbiguousPrefix {
                candidates,
                matches,
                ..
            } => {
                assert_eq!(matches, 10);
                assert_eq!(candidates.len(), 8);
                let mut sorted = candidates.clone();
                sorted.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
                assert_eq!(candidates, sorted);
                assert_eq!(candidates[0], Digest::from_hex(&name(3)).unwrap());
            }
            other => panic!("{layout:?}: {other:?}"),
        }
    }
}

#[test]
fn resolve_prefix_min() {
    for layout in layouts() {
        let dir = Scratch::new("local-resolve-min");
        let store = LocalStore::open_with(dir.path(), layout).unwrap();
        let only = store.insert_bytes(b"only one").unwrap().digest;
        let hex = only.hex();
        assert_eq!(store.resolve_prefix_min(&hex[..4], 4).unwrap(), only);
        assert!(matches!(
            store.resolve_prefix_min(&hex[..3], 4),
            Err(StoreError::InvalidDigest(_))
        ));
        // 0 is clamped to 1: a single digit resolves, an empty one does not.
        assert_eq!(store.resolve_prefix_min(&hex[..1], 0).unwrap(), only);
        assert!(matches!(
            store.resolve_prefix_min("", 0),
            Err(StoreError::InvalidDigest(_))
        ));
        // Above 64 is clamped to 64: only the full digest is accepted.
        assert_eq!(store.resolve_prefix_min(&hex, 1000).unwrap(), only);
        assert!(store.resolve_prefix_min(&hex[..63], 1000).is_err());
    }
}

// -------------------------------------------------------------- (c) check

#[test]
fn check_reports_without_deleting() {
    for layout in layouts() {
        let dir = Scratch::new("local-check");
        let store = LocalStore::open_with(dir.path(), layout).unwrap();
        let stored = store.insert_bytes(b"pristine").unwrap();
        assert_eq!(
            store.check(&stored.digest).unwrap(),
            ObjectStatus::Intact { size: 8 }
        );

        std::fs::write(&stored.path, b"tampered!").unwrap();
        assert_eq!(
            store.check(&stored.digest).unwrap(),
            ObjectStatus::Corrupt {
                actual: Digest::of(b"tampered!"),
                size: 9
            }
        );
        // Reported, not deleted.
        assert_eq!(std::fs::read(&stored.path).unwrap(), b"tampered!");
        assert!(store.contains(&stored.digest));

        let absent = Digest::of(b"absent");
        assert_eq!(store.check(&absent).unwrap(), ObjectStatus::Missing);

        // A directory where the object would be is missing, as for `get`.
        let odd = Digest::of(b"a directory");
        std::fs::create_dir_all(store.path_for(&odd)).unwrap();
        assert_eq!(store.check(&odd).unwrap(), ObjectStatus::Missing);
        assert_eq!(store.get(&odd), None);
    }
}

#[test]
fn check_all_reports_each() {
    let dir = Scratch::new("local-check-all");
    let store = LocalStore::open(dir.path()).unwrap();
    let data = [b"one".to_vec(), b"two".to_vec(), b"three".to_vec()];
    let stored: Vec<_> = data
        .iter()
        .map(|d| store.insert_bytes(d).unwrap())
        .collect();
    std::fs::write(&stored[1].path, b"TWO").unwrap();

    let report = store.check_all().unwrap();
    let digests: Vec<Digest> = report.iter().map(|r| r.0).collect();
    assert_eq!(digests, sorted_digests(&data));
    for (digest, status) in &report {
        if *digest == stored[1].digest {
            assert_eq!(
                *status,
                ObjectStatus::Corrupt {
                    actual: Digest::of(b"TWO"),
                    size: 3
                }
            );
        } else {
            assert!(matches!(status, ObjectStatus::Intact { .. }), "{status:?}");
        }
    }
    // Nothing deleted.
    assert_eq!(store.list().unwrap().len(), 3);
    assert_eq!(std::fs::read(&stored[1].path).unwrap(), b"TWO");
}

#[test]
fn repair_deletes_only_corrupt() {
    let dir = Scratch::new("local-repair");
    let store = LocalStore::open(dir.path()).unwrap();
    let good = store.insert_bytes(b"good").unwrap();
    let bad = store.insert_bytes(b"bad").unwrap();
    std::fs::write(&bad.path, b"BAD").unwrap();

    let lock = store.lock().unwrap();
    assert_eq!(
        lock.repair(&good.digest).unwrap(),
        ObjectStatus::Intact { size: 4 }
    );
    assert!(good.path.exists());
    assert_eq!(
        lock.repair(&bad.digest).unwrap(),
        ObjectStatus::Corrupt {
            actual: Digest::of(b"BAD"),
            size: 3
        }
    );
    assert!(!bad.path.exists());
    assert_eq!(lock.repair(&bad.digest).unwrap(), ObjectStatus::Missing);
    lock.release().unwrap();
}

// ------------------------------------------------------------ (d) sync core

#[test]
fn local_insert_reader_and_bytes() {
    for layout in layouts() {
        let dir = Scratch::new("local-insert");
        let store = LocalStore::open_with(dir.path(), layout).unwrap();
        let stored = store.insert_reader(&b"hello"[..]).unwrap();
        assert_eq!(stored.digest, Digest::of(b"hello"));
        assert_eq!(std::fs::read(&stored.path).unwrap(), b"hello");
        // Again: a no-op returning the same thing.
        assert_eq!(store.insert_reader(&b"hello"[..]).unwrap(), stored);
        assert_eq!(store.insert_bytes(b"hello").unwrap(), stored);
        assert_eq!(files_in(&dir.path().join("tmp")), 0);

        // Larger than one read buffer.
        let big = content(200_000);
        let stored = store.insert_bytes(&big).unwrap();
        assert_eq!(stored.digest, Digest::of(&big));
        assert_eq!(std::fs::read(&stored.path).unwrap(), big);
        assert_eq!(files_in(&dir.path().join("tmp")), 0);
    }
}

#[test]
fn local_insert_verified_mismatch() {
    let dir = Scratch::new("local-verified");
    let store = LocalStore::open(dir.path()).unwrap();
    let data = content(5_000);
    let digest = Digest::of(&data);

    let wrong = Digest::of(b"something else");
    let err = store.insert_verified(&data[..], &wrong, None).unwrap_err();
    assert!(matches!(err, StoreError::DigestMismatch { .. }), "{err:?}");
    assert!(store.list().unwrap().is_empty());
    assert_eq!(files_in(&dir.path().join("tmp")), 0);

    for size in [4_999, 5_001, 10] {
        let err = store
            .insert_verified(&data[..], &digest, Some(size))
            .unwrap_err();
        assert!(
            matches!(err, StoreError::SizeMismatch(_)),
            "{size}: {err:?}"
        );
        assert!(!store.contains(&digest));
        assert_eq!(files_in(&dir.path().join("tmp")), 0);
    }

    let path = store
        .insert_verified(&data[..], &digest, Some(5_000))
        .unwrap();
    assert_eq!(path, store.path_for(&digest));
    assert_eq!(std::fs::read(&path).unwrap(), data);
    assert_eq!(
        store.insert_verified(&data[..], &digest, None).unwrap(),
        path
    );
    assert_eq!(files_in(&dir.path().join("tmp")), 0);
}

#[test]
fn local_max_object_bytes() {
    let dir = Scratch::new("local-cap");
    let store = LocalStore::open(dir.path())
        .unwrap()
        .with_max_object_bytes(Some(1_000));
    assert_eq!(store.max_object_bytes(), Some(1_000));
    let big = content(150_000);
    let err = store.insert_reader(&big[..]).unwrap_err();
    assert!(matches!(err, StoreError::SizeMismatch(_)), "{err:?}");
    assert_eq!(files_in(&dir.path().join("tmp")), 0);
    assert!(store.list().unwrap().is_empty());

    // A declared size over the cap is refused before reading anything.
    let err = store
        .insert_verified(&big[..], &Digest::of(&big), Some(big.len() as u64))
        .unwrap_err();
    assert!(matches!(err, StoreError::SizeMismatch(_)), "{err:?}");
    let err = store
        .insert_verified(&big[..], &Digest::of(&big), None)
        .unwrap_err();
    assert!(matches!(err, StoreError::SizeMismatch(_)), "{err:?}");
    assert_eq!(files_in(&dir.path().join("tmp")), 0);

    // At the cap is fine.
    let ok = content(1_000);
    assert!(store.insert_bytes(&ok).is_ok());
}

#[test]
fn local_insert_path_copies() {
    let dir = Scratch::new("local-insert-path");
    let store = LocalStore::open(dir.path()).unwrap();
    let src = dir.path().join("source.bin");
    let data = content(70_000);
    std::fs::write(&src, &data).unwrap();
    let before = std::fs::metadata(&src).unwrap();

    let stored = store.insert_path(&src).unwrap();
    assert_eq!(stored.digest, Digest::of(&data));
    assert_eq!(std::fs::read(&stored.path).unwrap(), data);
    // The source is untouched (not moved, not linked).
    assert_eq!(std::fs::read(&src).unwrap(), data);
    let after = std::fs::metadata(&src).unwrap();
    assert_eq!(before.len(), after.len());
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    std::fs::write(&src, b"changed").unwrap();
    assert_eq!(std::fs::read(&stored.path).unwrap(), data);

    let err = store.insert_path(dir.path().join("missing")).unwrap_err();
    assert!(matches!(err, StoreError::Io(_)), "{err:?}");
}

/// Yields one chunk, announces it, then blocks until told to finish.
struct Gated {
    started: Option<mpsc::Sender<()>>,
    go: mpsc::Receiver<()>,
    sent: bool,
}

impl Read for Gated {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if !self.sent {
            self.sent = true;
            buf[0] = b'x';
            return Ok(1);
        }
        if let Some(started) = self.started.take() {
            let _ = started.send(());
            let _ = self.go.recv();
        }
        Ok(0)
    }
}

#[test]
fn purge_temp_respects_live_sync_writes() {
    let dir = Scratch::new("local-purge-live");
    let store = LocalStore::open(dir.path()).unwrap();
    let leftover = dir.path().join("tmp").join("leftover.tmp");
    std::fs::write(&leftover, b"old").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&leftover)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
    let (started_tx, started_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel();
    let writer = {
        let store = store.clone();
        std::thread::spawn(move || {
            store.insert_reader(Gated {
                started: Some(started_tx),
                go: go_rx,
                sent: false,
            })
        })
    };
    started_rx.recv().unwrap();
    assert_eq!(files_in(&dir.path().join("tmp")), 2);
    // Only the leftover goes; the live write stays, however young the cut.
    assert_eq!(store.purge_temp(Duration::ZERO).unwrap(), 1);
    assert_eq!(files_in(&dir.path().join("tmp")), 1);
    go_tx.send(()).unwrap();
    let stored = writer.join().unwrap().unwrap();
    assert_eq!(stored.digest, Digest::of(b"x"));
    assert_eq!(files_in(&dir.path().join("tmp")), 0);
}

#[cfg(unix)]
#[test]
fn local_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Scratch::new("local-perm");
    let store = LocalStore::open(dir.path()).unwrap();
    let stored = store.insert_bytes(b"secret weights").unwrap();
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    for sub in ["sha256", "tmp", "partial"] {
        assert_eq!(mode(&dir.path().join(sub)) & 0o077, 0, "{sub}");
    }
    assert_eq!(mode(stored.path.parent().unwrap()) & 0o077, 0);
    assert_eq!(mode(&stored.path) & 0o077, 0);
}

#[test]
fn remove_and_evict() {
    let dir = Scratch::new("local-remove");
    let store = LocalStore::open(dir.path()).unwrap();
    let data = blobs();
    for d in &data {
        store.insert_bytes(d).unwrap();
    }
    let first = Digest::of(&data[0]);
    assert!(store.remove(&first).unwrap());
    assert!(!store.remove(&first).unwrap());
    assert!(!store.contains(&first));
    let evicted = store.evict_to(0).unwrap();
    assert_eq!(evicted.removed, 4);
    assert_eq!(evicted.remaining, 0);
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn nothing_to_delete_takes_no_lock() {
    let dir = Scratch::new("local-no-lock");
    let store = LocalStore::open(dir.path()).unwrap();
    let a = store.insert_bytes(b"alpha").unwrap();
    let absent = Digest::of(b"absent");
    assert!(!store.remove(&absent).unwrap());
    let fits = store.evict_to(u64::MAX).unwrap();
    assert_eq!((fits.removed, fits.freed, fits.remaining), (0, 0, 5));
    assert!(
        !dir.path().join(".lock").exists(),
        "the lock file was created with nothing to delete"
    );

    // With the lock held elsewhere, they still return at once.
    let held = store.lock().unwrap();
    let (tx, rx) = mpsc::channel();
    let other = store.clone();
    std::thread::spawn(move || {
        let removed = other.remove(&absent).unwrap();
        let evicted = other.evict_to(u64::MAX).unwrap().removed;
        tx.send((removed, evicted)).unwrap();
    });
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        (false, 0)
    );
    drop(held);
    assert!(a.path.exists());
}

/// On a store root this user may not write to, `remove` of an absent object
/// and an `evict_to` with nothing to do succeed (as in 0.6) instead of
/// failing to create the lock file.
#[cfg(unix)]
#[test]
fn read_only_root_allows_no_op_deletions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Scratch::new("local-read-only");
    let store = LocalStore::open(dir.path()).unwrap();
    store.insert_bytes(b"alpha").unwrap();
    let set_mode =
        |mode| std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode));
    set_mode(0o555).unwrap();
    if std::fs::write(dir.path().join("probe"), b"x").is_ok() {
        // Running as root, which ignores the mode: nothing to test.
        set_mode(0o755).unwrap();
        return;
    }
    let removed = store.remove(&Digest::of(b"absent"));
    let evicted = store.evict_to(u64::MAX);
    let locked = store.lock();
    set_mode(0o755).unwrap();
    assert!(!removed.unwrap());
    assert_eq!(evicted.unwrap().remaining, 5);
    // Only an operation that needs the lock fails there.
    assert!(matches!(locked, Err(StoreError::Io(_))), "{locked:?}");
}

// ------------------------------------------------------------------ (e) lock

#[test]
fn try_lock_excludes() {
    let dir = Scratch::new("local-lock");
    let store = LocalStore::open(dir.path()).unwrap();
    let held = store.lock().unwrap();
    assert_eq!(held.store().root(), store.root());
    let other = store.clone();
    let (free, timed_out) = std::thread::spawn(move || {
        let free = other.try_lock().unwrap().is_none();
        let timed_out = other.lock_timeout(Duration::from_millis(50));
        (free, timed_out)
    })
    .join()
    .unwrap();
    assert!(free, "try_lock must see the lock held");
    assert!(
        matches!(timed_out, Err(StoreError::Locked(_))),
        "{timed_out:?}"
    );
    drop(held);
    let again = store.try_lock().unwrap();
    assert!(again.is_some());
    drop(again);
    assert!(store.lock_timeout(Duration::from_millis(50)).is_ok());
}

#[test]
fn lock_file_is_never_listed_or_purged() {
    let dir = Scratch::new("local-lock-file");
    let store = LocalStore::open(dir.path()).unwrap();
    store.insert_bytes(b"object").unwrap();
    let lock = store.lock().unwrap();
    let lock_path = dir.path().join(".lock");
    assert!(lock_path.is_file());
    assert_eq!(store.list().unwrap().len(), 1);
    assert_eq!(store.purge_temp(Duration::ZERO).unwrap(), 0);
    drop(lock);
    assert_eq!(store.purge_temp(Duration::ZERO).unwrap(), 0);
    store.evict_to(0).unwrap();
    assert!(lock_path.is_file(), "the lock file is never deleted");
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn remove_waits_for_the_holder() {
    let dir = Scratch::new("local-lock-wait");
    let store = LocalStore::open(dir.path()).unwrap();
    let stored = store.insert_bytes(b"in use").unwrap();
    let held = store.lock().unwrap();

    let (tx, rx) = mpsc::channel();
    let remover = {
        let store = store.clone();
        let digest = stored.digest;
        std::thread::spawn(move || {
            let _ = tx.send(store.remove(&digest));
        })
    };
    assert!(
        rx.recv_timeout(Duration::from_millis(100)).is_err(),
        "remove returned while the lock was held"
    );
    assert!(stored.path.exists());
    drop(held);
    let removed = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(removed.unwrap());
    assert!(!stored.path.exists());
    remover.join().unwrap();
}

#[test]
fn guard_methods_do_not_deadlock() {
    let dir = Scratch::new("local-lock-guard");
    let store = LocalStore::open(dir.path()).unwrap();
    let lock = store.lock().unwrap();
    // Inserts never take the lock.
    let a = store.insert_bytes(b"a").unwrap();
    let b = lock.store().insert_bytes(b"bb").unwrap();
    assert!(lock.remove(&a.digest).unwrap());
    assert!(!store.contains(&a.digest));
    let evicted = lock.evict_to(0).unwrap();
    assert_eq!(evicted.removed, 1);
    assert_eq!(evicted.freed, 2);
    assert!(!store.contains(&b.digest));
    lock.release().unwrap();
    // Released: the self-locking forms work again.
    assert!(!store.remove(&a.digest).unwrap());
}

/// Set in the child process of `lock_is_released_when_the_holder_dies`.
#[cfg(unix)]
const CHILD_ENV: &str = "FERROLY_STORE_LOCK_CHILD_DIR";

/// The file the child creates once it holds the lock (outside the store's
/// own entries). A marker file, not a line on stdout: libtest's own output
/// (`test child_holds_the_store_lock ... ` with one test thread) shares that
/// line.
#[cfg(unix)]
const CHILD_READY: &str = "child-ready";

/// In the child: take the store lock, announce it, and wait to be killed. In
/// a normal run (no environment variable) it does nothing.
#[cfg(unix)]
#[test]
fn child_holds_the_store_lock() {
    let Some(dir) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let store = LocalStore::open(&dir).unwrap();
    let lock = store.lock().unwrap();
    std::fs::write(dir.join(CHILD_READY), b"locked").unwrap();
    // The parent kills it within moments; this only bounds a stray child.
    std::thread::sleep(Duration::from_secs(20));
    drop(lock);
}

#[cfg(unix)]
#[test]
fn lock_is_released_when_the_holder_dies() {
    use std::process::{Command, Stdio};
    use std::time::Instant;

    let dir = Scratch::new("local-lock-xproc");
    let store = LocalStore::open(dir.path()).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["child_holds_the_store_lock", "--exact", "--nocapture"])
        .env(CHILD_ENV, dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let ready = dir.path().join(CHILD_READY);
    let deadline = Instant::now() + Duration::from_secs(15);
    while !ready.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("the child exited ({status}) before taking the lock");
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the child did not take the lock in time");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(store.try_lock().unwrap().is_none(), "held by the child");
    child.kill().unwrap();
    child.wait().unwrap();
    let lock = store
        .lock_timeout(Duration::from_secs(10))
        .expect("a dead holder must not block the store lock");
    drop(lock);
}

// ---------------------------------------------------------------- errors

#[test]
fn fs_errors_convert() {
    use ferroly::fsutils::FsError;
    let locked: StoreError = FsError::Locked("x/.lock".into()).into();
    assert!(matches!(locked, StoreError::Locked(ref p) if p == "x/.lock"));
    let io: StoreError = FsError::Io(std::io::Error::other("boom")).into();
    assert!(matches!(io, StoreError::Io(_)));
    let other: StoreError = FsError::Undetermined("y".into()).into();
    assert!(matches!(other, StoreError::Io(_)));
}

#[cfg(feature = "errutils")]
#[test]
fn new_errors_classify() {
    use ferroly::errutils::{Classify, ErrorKind};
    let kind = |e: StoreError| ferroly::Error::from(e).kind();
    assert_eq!(
        kind(StoreError::NotFound("sha256:ab".into())),
        ErrorKind::NotFound
    );
    assert_eq!(
        kind(StoreError::AmbiguousPrefix {
            prefix: "sha256:ab".into(),
            candidates: vec![],
            matches: 2
        }),
        ErrorKind::InvalidInput
    );
    assert_eq!(
        kind(StoreError::Locked(".lock".into())),
        ErrorKind::Conflict
    );
}

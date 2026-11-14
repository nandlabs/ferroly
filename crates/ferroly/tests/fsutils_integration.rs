#![cfg(feature = "fsutils")]
//! `ferroly::fsutils` through its public, safe API.

use std::io::Write;

use ferroly::fsutils::{
    detect_content_type, lookup_content_type, sniff, write_atomic, AtomicFile, FsError, LockFile,
    Mmap, TempDir, TempFile, SNIFF_LEN,
};

#[test]
fn content_type_by_extension_and_by_content_can_disagree() {
    let dir = TempDir::new("ferroly-fsutils-it").unwrap();

    // A PNG saved under a misleading name: the extension says text, the
    // bytes say image. Both answers are hints.
    let path = dir.path().join("notes.txt");
    std::fs::write(&path, b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR").unwrap();
    assert_eq!(
        lookup_content_type("notes.txt").as_deref(),
        Some("text/plain")
    );
    assert_eq!(detect_content_type(&path).unwrap(), "image/png");

    // Text formats are recognized from their content too.
    let json = dir.path().join("payload");
    std::fs::write(&json, b"  {\"ok\": true}\n").unwrap();
    assert_eq!(detect_content_type(&json).unwrap(), "application/json");
    let html = dir.path().join("page");
    std::fs::write(&html, b"<!doctype html><title>x</title>").unwrap();
    assert_eq!(detect_content_type(&html).unwrap(), "text/html");

    // A header longer than the sniff window is handled.
    let big = dir.path().join("big.bin");
    std::fs::write(&big, vec![0u8; SNIFF_LEN * 4]).unwrap();
    assert!(matches!(
        detect_content_type(&big),
        Err(FsError::Undetermined(_))
    ));
    assert_eq!(sniff(b"\x28\xb5\x2f\xfd"), Some("application/zstd"));
    assert_eq!(
        lookup_content_type("Report.DOCX").as_deref(),
        Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document")
    );
}

#[test]
fn atomic_write_under_a_lock_then_read_back() {
    let dir = TempDir::new("ferroly-fsutils-it").unwrap();
    let state = dir.path().join("state.json");
    let lock_path = dir.path().join("state.lock");

    {
        let _lock = LockFile::acquire(&lock_path).unwrap();
        // A second writer backs off instead of interleaving.
        assert!(matches!(
            LockFile::acquire(&lock_path),
            Err(FsError::Locked(_))
        ));
        write_atomic(&state, b"{\"generation\":1}").unwrap();

        let mut draft = AtomicFile::create(&state).unwrap();
        draft.write_all(b"{\"generation\":").unwrap();
        draft.write_all(b"2}").unwrap();
        // Readers still see generation 1 until the commit.
        assert_eq!(std::fs::read(&state).unwrap(), b"{\"generation\":1}");
        draft.commit().unwrap();
    }
    assert!(!lock_path.exists(), "lock released on drop");

    // The safe view is a private copy of the bytes.
    let view = Mmap::open(&state).unwrap();
    assert_eq!(&view[..], b"{\"generation\":2}");
    assert!(!view.is_mapped());
    write_atomic(&state, b"{}").unwrap();
    assert_eq!(view.as_bytes(), b"{\"generation\":2}");
    assert_eq!(view.len(), 16);

    // Only the state file remains.
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["state.json"]);
}

#[test]
fn temp_guards_clean_up() {
    let outer = TempDir::new("ferroly-fsutils-it").unwrap();
    let outer_path = outer.path().to_path_buf();
    let inner = TempDir::new_in(outer.path(), "inner").unwrap();
    let mut file = TempFile::with_suffix_in(inner.path(), "report", ".csv").unwrap();
    file.as_file_mut().write_all(b"a,b\n").unwrap();
    assert_eq!(
        lookup_content_type(&file.path().to_string_lossy()).as_deref(),
        Some("text/csv")
    );

    let kept = outer.path().join("report.csv");
    file.persist(&kept).unwrap();
    assert_eq!(std::fs::read(&kept).unwrap(), b"a,b\n");

    drop(inner);
    assert_eq!(std::fs::read_dir(&outer_path).unwrap().count(), 1);
    drop(outer);
    assert!(!outer_path.exists());
}

#[test]
fn errors_convert_into_the_crate_error() {
    fn load(path: &std::path::Path) -> Result<Mmap, ferroly::Error> {
        Ok(Mmap::open(path)?)
    }
    let dir = TempDir::new("ferroly-fsutils-it").unwrap();
    let err = load(&dir.path().join("missing")).unwrap_err();
    assert!(matches!(err, ferroly::Error::Fs(FsError::Io(_))));
}

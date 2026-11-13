//! Atomic file replacement: write to a temporary sibling, flush it to disk,
//! then rename it over the destination.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::temp::{create_new_private, unique_token};
use super::FsError;

/// Writes `data` to `path` so that readers see either the old contents or the
/// new ones, never a partial file.
///
/// Shorthand for [`AtomicFile::create`], one `write_all`, and
/// [`AtomicFile::commit`]; see [`AtomicFile`] for the exact guarantees.
///
/// ```
/// use ferroly::fsutils::{write_atomic, TempDir};
///
/// let dir = TempDir::new("my-app")?;
/// let path = dir.path().join("state.json");
/// write_atomic(&path, b"{\"version\":1}")?;
/// write_atomic(&path, b"{\"version\":2}")?; // replaces in one step
/// assert_eq!(std::fs::read(&path)?, b"{\"version\":2}");
/// # Ok::<(), ferroly::fsutils::FsError>(())
/// ```
pub fn write_atomic(path: impl AsRef<Path>, data: impl AsRef<[u8]>) -> Result<(), FsError> {
    let mut file = AtomicFile::create(path)?;
    file.write_all(data.as_ref())?;
    file.commit()
}

/// A file being written that only appears at its destination on
/// [`commit`](AtomicFile::commit).
///
/// Bytes go to a uniquely named temporary file in the **same directory** as
/// the destination (a rename is only atomic within one filesystem). `commit`
/// flushes that file to disk (`fsync`) and renames it over the destination;
/// on Unix the containing directory is then flushed too, so the rename itself
/// survives a crash. Dropping the writer without committing deletes the
/// temporary file and leaves the destination untouched.
///
/// What this does and does not give you:
///
/// * A crash or error mid-write never leaves a truncated destination.
/// * Concurrent readers see the old file or the new file, complete.
/// * The destination gets a **new inode**: existing hard links keep the old
///   contents, and anything holding the old file open keeps reading it. On
///   Unix, if the destination already exists its permission bits are copied
///   to the new file (ownership and extended attributes are not); a new
///   destination gets the usual mode of a created file.
/// * If the destination is a symbolic link to an existing file, the link is
///   followed and its target is replaced (the link itself is kept).
/// * Two writers committing at once do not corrupt the file, but the last
///   rename wins — use a [`LockFile`](super::LockFile) to coordinate them.
/// * The destination's directory must be writable, and a stray temporary file
///   (`.<name>.<unique>.tmp`) can be left behind if the process is killed.
#[derive(Debug)]
pub struct AtomicFile {
    /// `None` once committed.
    file: Option<File>,
    tmp: PathBuf,
    dest: PathBuf,
}

impl AtomicFile {
    /// Starts an atomic write to `dest`. The destination's parent directory
    /// must exist.
    pub fn create(dest: impl AsRef<Path>) -> Result<AtomicFile, FsError> {
        let dest = dest.as_ref();
        // Follow a symlink so the rename replaces its target, not the link.
        let dest = match std::fs::symlink_metadata(dest) {
            Ok(meta) if meta.file_type().is_symlink() => std::fs::canonicalize(dest)?,
            _ => dest.to_path_buf(),
        };
        let name = dest.file_name().ok_or_else(|| {
            FsError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "atomic write needs a destination with a file name",
            ))
        })?;
        let dir = parent_dir(&dest);
        // An existing destination's permission bits are put on the draft
        // before any data is written, so the new contents are never more
        // exposed than the file they replace. A brand new destination gets
        // the ordinary mode of a created file.
        let existing = std::fs::metadata(&dest).ok().map(|m| m.permissions());
        let mut last = None;
        for _ in 0..16 {
            let tmp = dir.join(format!(
                ".{}.{}.tmp",
                name.to_string_lossy(),
                unique_token()
            ));
            let opened = if existing.is_some() {
                create_new_private(&tmp)
            } else {
                File::options().write(true).create_new(true).open(&tmp)
            };
            match opened {
                Ok(file) => {
                    let draft = AtomicFile {
                        file: Some(file),
                        tmp,
                        dest,
                    };
                    // If this fails, `draft` drops and removes the file.
                    #[cfg(unix)]
                    if let Some(permissions) = existing {
                        std::fs::set_permissions(&draft.tmp, permissions)?;
                    }
                    return Ok(draft);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
                Err(e) => return Err(FsError::Io(e)),
            }
        }
        Err(FsError::Io(last.unwrap_or_else(|| {
            std::io::Error::other("could not create a unique temporary file")
        })))
    }

    /// The destination path (after following a symlink, if it was one).
    pub fn path(&self) -> &Path {
        &self.dest
    }

    /// Flushes the data to disk and renames it over the destination.
    ///
    /// On error the temporary file is removed and the destination keeps its
    /// previous contents.
    pub fn commit(mut self) -> Result<(), FsError> {
        let file = self
            .file
            .take()
            .expect("AtomicFile holds its file until commit");
        let result = publish(file, &self.tmp, &self.dest);
        if result.is_err() {
            let _ = std::fs::remove_file(&self.tmp);
        }
        result.map_err(FsError::Io)
    }
}

/// Flushes `file`, closes it, and renames `tmp` over `dest`.
fn publish(file: File, tmp: &Path, dest: &Path) -> std::io::Result<()> {
    file.sync_all()?;
    // Closed before the rename, which Windows requires of the source file.
    drop(file);
    std::fs::rename(tmp, dest)?;
    sync_dir(&parent_dir(dest));
    Ok(())
}

impl Write for AtomicFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.file.as_mut() {
            Some(file) => file.write(buf),
            None => Err(std::io::Error::other("atomic file already committed")),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

impl Drop for AtomicFile {
    fn drop(&mut self) {
        // Still holding the file means `commit` never ran: discard the draft.
        if self.file.take().is_some() {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

/// The directory containing `path` (`.` for a bare file name).
fn parent_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Flushes a directory entry change to disk. Best-effort: not every
/// filesystem supports it, and the data itself is already safe.
#[cfg(unix)]
fn sync_dir(dir: &Path) {
    if let Ok(handle) = File::open(dir) {
        let _ = handle.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutils::TempDir;

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn write_atomic_creates_and_replaces() {
        let dir = TempDir::new("ferroly-atomic").unwrap();
        let path = dir.path().join("state.json");
        write_atomic(&path, b"one").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"one");
        write_atomic(&path, b"two, longer").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two, longer");
        write_atomic(&path, b"").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        // No temporary files left behind.
        assert_eq!(entries(dir.path()), vec!["state.json".to_string()]);
    }

    #[test]
    fn uncommitted_write_leaves_destination_untouched() {
        let dir = TempDir::new("ferroly-atomic-drop").unwrap();
        let path = dir.path().join("keep.txt");
        std::fs::write(&path, b"original").unwrap();
        {
            let mut draft = AtomicFile::create(&path).unwrap();
            draft.write_all(b"half-written").unwrap();
            // While the draft is open the destination is unchanged.
            assert_eq!(std::fs::read(&path).unwrap(), b"original");
            assert_eq!(entries(dir.path()).len(), 2);
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        assert_eq!(entries(dir.path()), vec!["keep.txt".to_string()]);
    }

    #[test]
    fn streaming_commit_writes_everything() {
        let dir = TempDir::new("ferroly-atomic-stream").unwrap();
        let path = dir.path().join("big.bin");
        let mut draft = AtomicFile::create(&path).unwrap();
        assert_eq!(draft.path(), path);
        for chunk in 0..100u8 {
            draft.write_all(&[chunk; 1000]).unwrap();
        }
        draft.flush().unwrap();
        assert!(!path.exists());
        draft.commit().unwrap();
        let data = std::fs::read(&path).unwrap();
        assert_eq!(data.len(), 100_000);
        assert_eq!(data[99_999], 99);
    }

    #[test]
    fn missing_parent_directory_is_an_error() {
        let dir = TempDir::new("ferroly-atomic-missing").unwrap();
        let path = dir.path().join("no-such-dir").join("f.txt");
        assert!(write_atomic(&path, b"x").is_err());
        assert!(entries(dir.path()).is_empty());
    }

    #[test]
    fn concurrent_writers_never_produce_a_mixed_file() {
        let dir = TempDir::new("ferroly-atomic-race").unwrap();
        let path = dir.path().join("shared.bin");
        write_atomic(&path, vec![b'0'; 4096]).unwrap();
        let handles: Vec<_> = (0..4u8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        // On Windows a rename can lose a race with another
                        // rename or an open handle; only successful commits
                        // need to be whole.
                        let _ = write_atomic(&path, vec![b'a' + i; 4096]);
                    }
                })
            })
            .collect();
        for _ in 0..50 {
            if let Ok(data) = std::fs::read(&path) {
                assert_eq!(data.len(), 4096);
                assert!(data.iter().all(|&b| b == data[0]), "mixed contents");
            }
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let data = std::fs::read(&path).unwrap();
        assert!(data.iter().all(|&b| b == data[0]));
        assert_eq!(entries(dir.path()), vec!["shared.bin".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn keeps_existing_permissions_and_uses_default_for_new_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("ferroly-atomic-mode").unwrap();
        let path = dir.path().join("script.sh");
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o750)).unwrap();
        write_atomic(&path, b"#!/bin/sh\necho hi\n").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o750);

        // A new file gets the same mode an ordinary create would.
        let plain = dir.path().join("plain-create.txt");
        std::fs::write(&plain, b"x").unwrap();
        let expected = std::fs::metadata(&plain).unwrap().permissions().mode() & 0o777;
        let fresh = dir.path().join("fresh.txt");
        write_atomic(&fresh, b"x").unwrap();
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, expected);
    }

    #[cfg(unix)]
    #[test]
    fn follows_a_symlink_and_replaces_its_target() {
        let dir = TempDir::new("ferroly-atomic-link").unwrap();
        let target = dir.path().join("target.txt");
        let link = dir.path().join("link.txt");
        std::fs::write(&target, b"old").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        write_atomic(&link, b"new").unwrap();
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
    }
}

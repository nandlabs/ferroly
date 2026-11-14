//! A simple advisory lock file.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::FsError;

/// An advisory lock held by the existence of a file.
///
/// Acquiring creates the file with create-new semantics — the one filesystem
/// operation that atomically fails if the name is taken — and writes the
/// holder's process id into it. Dropping the guard (or calling
/// [`release`](LockFile::release)) deletes the file.
///
/// Know the limits before relying on it:
///
/// * **Advisory.** Nothing stops a process that ignores the lock file from
///   touching whatever it protects; every participant must use the same path.
/// * **Not crash-safe.** If the holder is killed, the file stays and the lock
///   looks held forever. Stale locks are never broken automatically (a pid can
///   be reused, so "is that process alive" is not a safe test); inspect
///   [`LockFile::owner`] and call [`LockFile::break_lock`] deliberately.
/// * **Not reentrant**, and not tied to a thread: a second `acquire` from the
///   same process fails like any other.
/// * Create-new is atomic on local filesystems; on some network filesystems
///   (older NFS) it is not.
///
/// ```
/// use ferroly::fsutils::{LockFile, TempDir};
///
/// let dir = TempDir::new("my-app")?;
/// let path = dir.path().join("job.lock");
/// let lock = LockFile::acquire(&path)?;
/// assert!(LockFile::try_acquire(&path)?.is_none()); // already held
/// drop(lock);
/// assert!(LockFile::try_acquire(&path)?.is_some());
/// # Ok::<(), ferroly::fsutils::FsError>(())
/// ```
#[derive(Debug)]
pub struct LockFile {
    path: PathBuf,
    held: bool,
}

impl LockFile {
    /// Takes the lock, or returns `Ok(None)` if another holder has it.
    pub fn try_acquire(path: impl AsRef<Path>) -> Result<Option<LockFile>, FsError> {
        let path = path.as_ref();
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        match options.open(path) {
            Ok(mut file) => {
                let guard = LockFile {
                    path: path.to_path_buf(),
                    held: true,
                };
                // If recording the owner fails, `guard` drops and unlocks.
                writeln!(file, "{}", std::process::id())?;
                file.sync_all()?;
                Ok(Some(guard))
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(e) => Err(FsError::Io(e)),
        }
    }

    /// Takes the lock, or fails with [`FsError::Locked`] if it is held.
    pub fn acquire(path: impl AsRef<Path>) -> Result<LockFile, FsError> {
        let path = path.as_ref();
        LockFile::try_acquire(path)?.ok_or_else(|| FsError::Locked(path.display().to_string()))
    }

    /// Takes the lock, retrying until `timeout` has elapsed; fails with
    /// [`FsError::Locked`] if it is still held then.
    ///
    /// This **blocks the calling thread** between attempts (polling with a
    /// short, growing sleep). From async code, run it on a blocking thread.
    pub fn acquire_timeout(path: impl AsRef<Path>, timeout: Duration) -> Result<LockFile, FsError> {
        let path = path.as_ref();
        let deadline = Instant::now() + timeout;
        let mut pause = Duration::from_millis(2);
        loop {
            if let Some(lock) = LockFile::try_acquire(path)? {
                return Ok(lock);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(FsError::Locked(path.display().to_string()));
            }
            std::thread::sleep(pause.min(deadline - now));
            pause = (pause * 2).min(Duration::from_millis(100));
        }
    }

    /// Reads the process id recorded in the lock file at `path`.
    ///
    /// Returns `Ok(None)` if no lock file exists, or if it exists but holds no
    /// readable id (for example, it was observed between creation and the
    /// write of the id).
    pub fn owner(path: impl AsRef<Path>) -> Result<Option<u32>, FsError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(text.trim().parse().ok()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(FsError::Io(e)),
        }
    }

    /// Forcibly removes the lock file at `path`, whoever holds it. Returns
    /// whether a file was removed.
    ///
    /// Only for recovering from a holder that is known to be gone: breaking a
    /// live lock lets two holders proceed at once.
    pub fn break_lock(path: impl AsRef<Path>) -> Result<bool, FsError> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(FsError::Io(e)),
        }
    }

    /// The lock file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Releases the lock now, reporting any error (dropping the guard ignores
    /// errors).
    pub fn release(mut self) -> Result<(), FsError> {
        self.held = false;
        std::fs::remove_file(&self.path).map_err(FsError::Io)
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        if self.held {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutils::TempDir;

    #[test]
    fn acquire_is_exclusive_and_released_on_drop() {
        let dir = TempDir::new("ferroly-lock").unwrap();
        let path = dir.path().join("a.lock");
        let lock = LockFile::acquire(&path).unwrap();
        assert_eq!(lock.path(), path);
        assert!(path.exists());
        assert!(LockFile::try_acquire(&path).unwrap().is_none());
        assert!(matches!(LockFile::acquire(&path), Err(FsError::Locked(_))));
        assert_eq!(LockFile::owner(&path).unwrap(), Some(std::process::id()));
        drop(lock);
        assert!(!path.exists());
        assert_eq!(LockFile::owner(&path).unwrap(), None);
        LockFile::acquire(&path).unwrap().release().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn failed_acquire_does_not_remove_the_holders_file() {
        let dir = TempDir::new("ferroly-lock-keep").unwrap();
        let path = dir.path().join("a.lock");
        let _lock = LockFile::acquire(&path).unwrap();
        for _ in 0..3 {
            assert!(LockFile::acquire(&path).is_err());
        }
        assert!(path.exists(), "a losing attempt must not unlock");
    }

    #[test]
    fn acquire_timeout_waits_for_release() {
        let dir = TempDir::new("ferroly-lock-wait").unwrap();
        let path = dir.path().join("a.lock");
        let lock = LockFile::acquire(&path).unwrap();

        // Still held: times out.
        let err = LockFile::acquire_timeout(&path, Duration::from_millis(30));
        assert!(matches!(err, Err(FsError::Locked(_))));

        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(lock);
        });
        let lock = LockFile::acquire_timeout(&path, Duration::from_secs(10)).unwrap();
        releaser.join().unwrap();
        assert!(path.exists());
        drop(lock);
    }

    #[test]
    fn only_one_thread_wins() {
        let dir = TempDir::new("ferroly-lock-race").unwrap();
        let path = dir.path().join("a.lock");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    LockFile::try_acquire(&path).unwrap()
                })
            })
            .collect();
        // Keep every guard alive until all threads have tried.
        let guards: Vec<Option<LockFile>> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(guards.iter().filter(|g| g.is_some()).count(), 1);
    }

    #[test]
    fn break_lock_removes_a_stale_file() {
        let dir = TempDir::new("ferroly-lock-break").unwrap();
        let path = dir.path().join("a.lock");
        // A lock left behind by a holder that never released it.
        std::mem::forget(LockFile::acquire(&path).unwrap());
        assert!(LockFile::try_acquire(&path).unwrap().is_none());
        assert!(LockFile::break_lock(&path).unwrap());
        assert!(!LockFile::break_lock(&path).unwrap());
        assert!(LockFile::try_acquire(&path).unwrap().is_some());
    }

    #[test]
    fn missing_directory_is_an_io_error_not_locked() {
        let dir = TempDir::new("ferroly-lock-missing").unwrap();
        let path = dir.path().join("nope").join("a.lock");
        assert!(matches!(LockFile::acquire(&path), Err(FsError::Io(_))));
    }
}

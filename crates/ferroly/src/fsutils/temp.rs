//! Unique temporary files and directories that clean up after themselves.

use std::collections::hash_map::RandomState;
use std::fs::File;
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::FsError;

/// How many candidate names are tried before giving up.
const MAX_ATTEMPTS: usize = 64;

/// Returns a 16-hex-digit token that is unpredictable and unique enough for a
/// temporary name.
///
/// `RandomState` is seeded from OS randomness, so the token is not guessable
/// from the pid and clock alone; the counter and clock make repeats within a
/// process vanishingly unlikely. Callers still create with create-new
/// semantics and retry, so a collision is never silently reused.
pub(crate) fn unique_token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    hasher.write_u32(std::process::id());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    hasher.write_u128(nanos);
    format!("{:016x}", hasher.finish())
}

/// Rejects name fragments that could step outside the target directory.
fn check_fragment(what: &str, fragment: &str) -> Result<(), FsError> {
    if fragment.contains(['/', '\\', '\0']) || fragment == ".." {
        return Err(FsError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("temporary name {what} must not contain path separators: {fragment:?}"),
        )));
    }
    Ok(())
}

/// Opens `path` for writing, failing if it already exists. On Unix the file is
/// created with mode `0600` so other users cannot read it.
pub(crate) fn create_new_private(path: &Path) -> std::io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// A directory builder that, on Unix, creates with mode `0700`.
#[cfg(unix)]
fn private_dir_builder() -> std::fs::DirBuilder {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    builder
}

/// A directory builder (no mode to restrict off Unix).
#[cfg(not(unix))]
fn private_dir_builder() -> std::fs::DirBuilder {
    std::fs::DirBuilder::new()
}

/// A uniquely named directory that is removed, with everything in it, when the
/// guard is dropped.
///
/// ```
/// use ferroly::fsutils::TempDir;
///
/// let dir = TempDir::new("my-app")?;
/// std::fs::write(dir.path().join("scratch.txt"), b"hello")?;
/// let kept = dir.path().to_path_buf();
/// drop(dir);
/// assert!(!kept.exists());
/// # Ok::<(), ferroly::fsutils::FsError>(())
/// ```
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
    armed: bool,
}

impl TempDir {
    /// Creates a new directory named `<prefix>-<unique>` under
    /// [`std::env::temp_dir`].
    pub fn new(prefix: &str) -> Result<TempDir, FsError> {
        TempDir::new_in(std::env::temp_dir(), prefix)
    }

    /// Creates a new directory named `<prefix>-<unique>` under `parent`, which
    /// must already exist. On Unix the directory is created with mode `0700`.
    pub fn new_in(parent: impl AsRef<Path>, prefix: &str) -> Result<TempDir, FsError> {
        check_fragment("prefix", prefix)?;
        let parent = parent.as_ref();
        let mut last = None;
        for _ in 0..MAX_ATTEMPTS {
            let path = parent.join(format!("{prefix}-{}", unique_token()));
            match private_dir_builder().create(&path) {
                Ok(()) => return Ok(TempDir { path, armed: true }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
                Err(e) => return Err(FsError::Io(e)),
            }
        }
        Err(FsError::Io(last.unwrap_or_else(|| {
            std::io::Error::other("could not create a unique temporary directory")
        })))
    }

    /// The directory's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Disarms the guard and returns the path; the directory is left in place.
    pub fn keep(mut self) -> PathBuf {
        self.armed = false;
        std::mem::take(&mut self.path)
    }

    /// Removes the directory now, reporting any error (dropping the guard
    /// ignores errors).
    pub fn close(mut self) -> Result<(), FsError> {
        self.armed = false;
        std::fs::remove_dir_all(&self.path).map_err(FsError::Io)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

/// A uniquely named, open file that is deleted when the guard is dropped.
///
/// The file is created with create-new semantics (an existing file is never
/// reused or truncated) and, on Unix, mode `0600`.
///
/// ```
/// use ferroly::fsutils::TempFile;
/// use std::io::Write;
///
/// let mut tmp = TempFile::new("my-app")?;
/// tmp.as_file_mut().write_all(b"scratch")?;
/// assert_eq!(std::fs::read(tmp.path())?, b"scratch");
/// # Ok::<(), ferroly::fsutils::FsError>(())
/// ```
#[derive(Debug)]
pub struct TempFile {
    path: PathBuf,
    file: File,
    armed: bool,
}

impl TempFile {
    /// Creates `<prefix>-<unique>` under [`std::env::temp_dir`].
    pub fn new(prefix: &str) -> Result<TempFile, FsError> {
        TempFile::with_suffix_in(std::env::temp_dir(), prefix, "")
    }

    /// Creates `<prefix>-<unique>` under `dir`, which must already exist.
    pub fn new_in(dir: impl AsRef<Path>, prefix: &str) -> Result<TempFile, FsError> {
        TempFile::with_suffix_in(dir, prefix, "")
    }

    /// Creates `<prefix>-<unique><suffix>` under `dir` — use a suffix such as
    /// `".json"` when a consumer picks a format from the extension.
    pub fn with_suffix_in(
        dir: impl AsRef<Path>,
        prefix: &str,
        suffix: &str,
    ) -> Result<TempFile, FsError> {
        check_fragment("prefix", prefix)?;
        check_fragment("suffix", suffix)?;
        let dir = dir.as_ref();
        let mut last = None;
        for _ in 0..MAX_ATTEMPTS {
            let path = dir.join(format!("{prefix}-{}{suffix}", unique_token()));
            match create_new_private(&path) {
                Ok(file) => {
                    return Ok(TempFile {
                        path,
                        file,
                        armed: true,
                    })
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
                Err(e) => return Err(FsError::Io(e)),
            }
        }
        Err(FsError::Io(last.unwrap_or_else(|| {
            std::io::Error::other("could not create a unique temporary file")
        })))
    }

    /// The file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The open file handle (read + write).
    pub fn as_file(&self) -> &File {
        &self.file
    }

    /// The open file handle, mutably (for `Write` / `Seek`).
    pub fn as_file_mut(&mut self) -> &mut File {
        &mut self.file
    }

    /// Disarms the guard and returns the path; the file is left in place.
    pub fn keep(mut self) -> PathBuf {
        self.armed = false;
        std::mem::take(&mut self.path)
    }

    /// Flushes the file to disk and renames it to `dest`, replacing any
    /// existing file there. `dest` must be on the same filesystem (a rename
    /// cannot cross devices); on failure the temporary file is removed.
    pub fn persist(mut self, dest: impl AsRef<Path>) -> Result<(), FsError> {
        self.file.sync_all()?;
        std::fs::rename(&self.path, dest.as_ref())?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};

    #[test]
    fn tokens_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..2_000 {
            assert!(seen.insert(unique_token()));
        }
    }

    #[test]
    fn temp_dir_is_unique_and_removed_on_drop() {
        let a = TempDir::new("ferroly-temp").unwrap();
        let b = TempDir::new("ferroly-temp").unwrap();
        assert_ne!(a.path(), b.path());
        assert!(a.path().is_dir());
        std::fs::create_dir(a.path().join("nested")).unwrap();
        std::fs::write(a.path().join("nested").join("f.txt"), b"x").unwrap();
        let path = a.path().to_path_buf();
        drop(a);
        assert!(!path.exists());
        b.close().unwrap();
    }

    #[test]
    fn temp_dir_keep_leaves_it_in_place() {
        let dir = TempDir::new("ferroly-temp-keep").unwrap();
        let path = dir.keep();
        assert!(path.is_dir());
        std::fs::remove_dir_all(&path).unwrap();
    }

    #[test]
    fn temp_dir_new_in_requires_existing_parent_and_safe_prefix() {
        let parent = TempDir::new("ferroly-temp-parent").unwrap();
        let child = TempDir::new_in(parent.path(), "child").unwrap();
        assert!(child.path().starts_with(parent.path()));
        assert!(TempDir::new_in(parent.path().join("missing"), "x").is_err());
        assert!(TempDir::new_in(parent.path(), "../escape").is_err());
        assert!(TempDir::new_in(parent.path(), "a/b").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn temp_entries_are_private_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("ferroly-temp-mode").unwrap();
        let mode = std::fs::metadata(dir.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        let file = TempFile::new_in(dir.path(), "f").unwrap();
        let mode = std::fs::metadata(file.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn temp_file_reads_writes_and_is_removed_on_drop() {
        let dir = TempDir::new("ferroly-temp-file").unwrap();
        let mut tmp = TempFile::with_suffix_in(dir.path(), "data", ".json").unwrap();
        assert!(tmp.path().to_string_lossy().ends_with(".json"));
        tmp.as_file_mut().write_all(b"{\"a\":1}").unwrap();
        tmp.as_file_mut().seek(SeekFrom::Start(0)).unwrap();
        let mut back = String::new();
        tmp.as_file().read_to_string(&mut back).unwrap();
        assert_eq!(back, "{\"a\":1}");
        let path = tmp.path().to_path_buf();
        drop(tmp);
        assert!(!path.exists());
    }

    #[test]
    fn temp_file_persist_and_keep() {
        let dir = TempDir::new("ferroly-temp-persist").unwrap();
        let mut tmp = TempFile::new_in(dir.path(), "p").unwrap();
        tmp.as_file_mut().write_all(b"final").unwrap();
        let tmp_path = tmp.path().to_path_buf();
        let dest = dir.path().join("final.txt");
        std::fs::write(&dest, b"old").unwrap();
        tmp.persist(&dest).unwrap();
        assert!(!tmp_path.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"final");

        let kept = TempFile::new_in(dir.path(), "k").unwrap().keep();
        assert!(kept.is_file());
    }
}

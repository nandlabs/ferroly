//! An operating-system file lock: `flock` on Unix, `LockFileEx` on Windows.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::FsError;

/// An exclusive lock held by the operating system on an open file —
/// `flock(LOCK_EX)` on Unix, `LockFileEx` on Windows.
///
/// Unlike [`LockFile`](super::LockFile), this lock belongs to the open file,
/// not to the file's existence: **when the holder exits or crashes, the
/// kernel releases it**, so a dead holder never blocks the next one and there
/// is nothing to break by hand. Prefer it whenever every participant runs on
/// the same machine and a supported platform.
///
/// - The lock file is created if missing and **never deleted**. Deleting it
///   on release would let a waiter that already opened the old file lock it
///   while a newcomer creates and locks a new one: two holders. An empty,
///   unlocked file left behind is harmless.
/// - **Unlinking or replacing the file while it is held breaks exclusion**:
///   the holder keeps its lock on the old, nameless file, and the next
///   acquirer creates a new file and locks that. An acquirer that opened the
///   old file before it went away notices once it gets the lock and retries
///   on the new one, but a holder is never told. Keep the file where cleaners
///   (such as a temporary-directory reaper) leave it alone.
/// - The holder's process id is written into the file after locking, for
///   [`owner`](FileLock::owner) to report; it is information, never the lock.
///   It is written only when the file is empty or holds nothing but a decimal
///   number (an earlier holder's id). **Any other content is left untouched**
///   and `owner` reports `None`, so locking an existing data file does not
///   destroy it.
/// - The last path component **must not be a symbolic link** (on Windows,
///   any reparse point): acquiring through one fails with an I/O error of
///   kind `InvalidInput`, so a link planted at the lock path cannot redirect
///   the lock or the id onto another file. Directories higher up the path
///   may be links.
/// - **Advisory**: processes that do not take the lock are not stopped.
/// - **Not reentrant**: a second acquire of the same path, even from the same
///   process or thread, fails like any other while the first guard is alive
///   (but see network filesystems below).
/// - Released by dropping the guard, by [`release`](FileLock::release), or by
///   the process ending. A child started with `std::process::Command` does not
///   inherit it.
/// - Supported on Linux, Android, macOS, iOS, tvOS, watchOS, FreeBSD,
///   DragonFly, NetBSD, OpenBSD, illumos and Windows. Elsewhere every acquire
///   fails with an I/O error of kind `Unsupported`.
/// - On network filesystems the guarantee is the server's; SMB shares honour
///   `LockFileEx`. **On Linux NFS mounts** the kernel emulates `flock` with
///   POSIX byte-range locks, which belong to the process rather than the open
///   file: there a second acquire in the same process *succeeds*, and closing
///   *any* descriptor the process has on the file — including the one
///   [`owner`](FileLock::owner) opens — releases the lock. On NFS, take the
///   lock at most once per process and do not call `owner` while holding it.
///
/// On Windows the locked range is one byte far beyond the end of the file
/// (offset 2⁶³ − 2), because Windows byte-range locks are mandatory: locking
/// the content would stop other processes from reading the owner's id.
///
/// ```
/// use ferroly::fsutils::{FileLock, TempDir};
///
/// let dir = TempDir::new("my-app")?;
/// let path = dir.path().join("job.lock");
/// let lock = FileLock::acquire(&path)?;
/// assert!(FileLock::try_acquire(&path)?.is_none()); // already held
/// assert_eq!(FileLock::owner(&path)?, Some(std::process::id()));
/// drop(lock);
/// assert!(FileLock::try_acquire(&path)?.is_some());
/// # Ok::<(), ferroly::fsutils::FsError>(())
/// ```
#[derive(Debug)]
pub struct FileLock {
    file: Option<File>,
    path: PathBuf,
}

/// The longest content still taken for a recorded process id (a `u32` takes
/// at most 10 digits; the rest is room for whitespace).
const MAX_ID_LEN: usize = 32;

/// How many times an acquire reopens the path after the file it locked turned
/// out to have been unlinked or replaced, before giving up.
const MAX_REOPENS: usize = 64;

impl FileLock {
    /// Takes the lock, or returns `Ok(None)` if another holder has it.
    pub fn try_acquire(path: impl AsRef<Path>) -> Result<Option<FileLock>, FsError> {
        FileLock::lock_at(path.as_ref(), false)
    }

    /// Takes the lock, or fails with [`FsError::Locked`] if it is held.
    pub fn acquire(path: impl AsRef<Path>) -> Result<FileLock, FsError> {
        let path = path.as_ref();
        FileLock::try_acquire(path)?.ok_or_else(|| FsError::Locked(path.display().to_string()))
    }

    /// Takes the lock, retrying until `timeout` has elapsed; fails with
    /// [`FsError::Locked`] if it is still held then. A `timeout` too large
    /// to add to the current time (such as [`Duration::MAX`]) waits forever.
    ///
    /// This **blocks the calling thread** between attempts (polling with a
    /// short, growing sleep). From async code, run it on a blocking thread.
    pub fn acquire_timeout(path: impl AsRef<Path>, timeout: Duration) -> Result<FileLock, FsError> {
        let path = path.as_ref();
        let deadline = Instant::now().checked_add(timeout);
        let mut pause = Duration::from_millis(2);
        loop {
            if let Some(lock) = FileLock::try_acquire(path)? {
                return Ok(lock);
            }
            let now = Instant::now();
            let nap = match deadline {
                Some(deadline) if now >= deadline => {
                    return Err(FsError::Locked(path.display().to_string()))
                }
                Some(deadline) => pause.min(deadline - now),
                None => pause,
            };
            std::thread::sleep(nap);
            pause = (pause * 2).min(Duration::from_millis(100));
        }
    }

    /// Takes the lock, waiting in the kernel for as long as another holder
    /// has it. **Blocks the calling thread**, with no timeout; see
    /// [`acquire_timeout`](FileLock::acquire_timeout) for a bounded wait.
    pub fn acquire_wait(path: impl AsRef<Path>) -> Result<FileLock, FsError> {
        let path = path.as_ref();
        FileLock::lock_at(path, true)?.ok_or_else(|| FsError::Locked(path.display().to_string()))
    }

    /// Reads the process id the last holder recorded in the file at `path`.
    ///
    /// The id says who took the lock most recently, not whether it is still
    /// held (the file keeps it after release); use
    /// [`try_acquire`](FileLock::try_acquire) to find out. `Ok(None)` if the
    /// file does not exist or holds anything but an id.
    pub fn owner(path: impl AsRef<Path>) -> Result<Option<u32>, FsError> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(FsError::Io(e)),
        };
        let head = read_head(&file)?;
        Ok(id_text(&head).and_then(|text| text.parse().ok()))
    }

    /// The lock file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Releases the lock now, reporting any error (dropping the guard ignores
    /// errors). The file stays in place.
    pub fn release(mut self) -> Result<(), FsError> {
        match self.file.take() {
            Some(file) => sys::unlock(&file).map_err(FsError::Io),
            None => Ok(()),
        }
    }

    /// Opens and locks the file at `path`, and keeps the lock only once the
    /// locked file is still the one at `path`: an acquirer that opened a file
    /// which was then unlinked or replaced would otherwise hold a lock nobody
    /// else can see. `Ok(None)` when `wait` is off and another holder has it.
    fn lock_at(path: &Path, wait: bool) -> Result<Option<FileLock>, FsError> {
        for _ in 0..MAX_REOPENS {
            let file = open(path)?;
            if !sys::lock(&file, wait)? {
                return Ok(None);
            }
            if sys::same_file(&file, path)? {
                let mut guard = FileLock {
                    file: Some(file),
                    path: path.to_path_buf(),
                };
                // If recording the owner fails, `guard` drops and unlocks.
                guard.record_owner()?;
                return Ok(Some(guard));
            }
            let _ = sys::unlock(&file);
            if is_symlink(path) {
                // Only reached where the platform has no `O_NOFOLLOW` below.
                return Err(link_refused(path));
            }
        }
        Err(FsError::Io(io::Error::other(format!(
            "lock file {} kept being replaced while acquiring it",
            path.display()
        ))))
    }

    fn record_owner(&mut self) -> Result<(), FsError> {
        if let Some(file) = self.file.as_mut() {
            if id_text(&read_head(file)?).is_none() {
                // Not ours to overwrite: the path names a file with content.
                return Ok(());
            }
            let id = format!("{}\n", std::process::id());
            file.seek(SeekFrom::Start(0))?;
            file.write_all(id.as_bytes())?;
            file.set_len(id.len() as u64)?;
            file.flush()?;
        }
        Ok(())
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            // Closing the file releases the lock too; unlocking first makes the
            // release immediate on Windows, where closing may be deferred.
            let _ = sys::unlock(&file);
        }
    }
}

/// Opens (creating if needed, never truncating) the lock file, refusing a
/// symbolic link (on Windows, any reparse point) as the last component.
fn open(path: &Path) -> Result<File, FsError> {
    match sys::open_options().open(path) {
        Ok(file) if sys::is_reparse_point(&file)? => Err(link_refused(path)),
        Ok(file) => Ok(file),
        // `O_NOFOLLOW` fails with `ELOOP`, `EMLINK` or `EFTYPE` depending on
        // the system; name the cause instead.
        Err(_) if is_symlink(path) => Err(link_refused(path)),
        Err(e) => Err(FsError::Io(e)),
    }
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
}

fn link_refused(path: &Path) -> FsError {
    FsError::Io(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "refusing to lock {}: it is a symbolic link or reparse point",
            path.display()
        ),
    ))
}

/// The start of the file: enough to tell a recorded id from other content.
fn read_head(mut file: &File) -> io::Result<Vec<u8>> {
    let mut head = Vec::with_capacity(MAX_ID_LEN + 1);
    file.seek(SeekFrom::Start(0))?;
    file.take(MAX_ID_LEN as u64 + 1).read_to_end(&mut head)?;
    Ok(head)
}

/// The trimmed text when `head` is empty or holds just a decimal number —
/// the only content `FileLock` may overwrite — and `None` otherwise.
fn id_text(head: &[u8]) -> Option<&str> {
    if head.len() > MAX_ID_LEN {
        return None;
    }
    let text = std::str::from_utf8(head).ok()?.trim();
    text.bytes().all(|b| b.is_ascii_digit()).then_some(text)
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "illumos",
))]
mod sys {
    //! `flock(2)`. `LOCK_EX = 2`, `LOCK_NB = 4` and `LOCK_UN = 8` have these
    //! values on every target listed above, and none renames the symbol.

    use core::ffi::c_int;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::Path;

    const LOCK_EX: c_int = 2;
    const LOCK_NB: c_int = 4;
    const LOCK_UN: c_int = 8;

    /// `O_NOFOLLOW`, which differs between systems and, on Linux, between
    /// architectures (checked against the kernel's `fcntl.h` headers and the
    /// `libc` crate). Where it is unknown it is 0 and `lock_at` still refuses
    /// a link, after opening its target but before writing to it.
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd",
    ))]
    const O_NOFOLLOW: c_int = 0x100;
    #[cfg(all(
        any(target_os = "linux", target_os = "android"),
        any(
            target_arch = "arm",
            target_arch = "aarch64",
            target_arch = "powerpc",
            target_arch = "powerpc64",
            target_arch = "m68k",
        )
    ))]
    const O_NOFOLLOW: c_int = 0x8000; // 0o100000
    #[cfg(any(
        target_os = "illumos",
        all(
            any(target_os = "linux", target_os = "android"),
            any(
                target_arch = "x86",
                target_arch = "x86_64",
                target_arch = "mips",
                target_arch = "mips64",
                target_arch = "mips32r6",
                target_arch = "mips64r6",
                target_arch = "riscv32",
                target_arch = "riscv64",
                target_arch = "s390x",
                target_arch = "sparc",
                target_arch = "sparc64",
                target_arch = "loongarch64",
                target_arch = "csky",
                target_arch = "hexagon",
            )
        )
    ))]
    const O_NOFOLLOW: c_int = 0x20000; // 0o400000
    #[cfg(all(
        any(target_os = "linux", target_os = "android"),
        not(any(
            target_arch = "arm",
            target_arch = "aarch64",
            target_arch = "powerpc",
            target_arch = "powerpc64",
            target_arch = "m68k",
            target_arch = "x86",
            target_arch = "x86_64",
            target_arch = "mips",
            target_arch = "mips64",
            target_arch = "mips32r6",
            target_arch = "mips64r6",
            target_arch = "riscv32",
            target_arch = "riscv64",
            target_arch = "s390x",
            target_arch = "sparc",
            target_arch = "sparc64",
            target_arch = "loongarch64",
            target_arch = "csky",
            target_arch = "hexagon",
        ))
    ))]
    const O_NOFOLLOW: c_int = 0;

    extern "C" {
        fn flock(fd: c_int, operation: c_int) -> c_int;
    }

    fn call(file: &File, operation: c_int) -> io::Result<()> {
        loop {
            // SAFETY: `flock` takes a descriptor and an integer and touches no
            // memory of ours; the descriptor stays open for the call because
            // `file` is borrowed.
            if unsafe { flock(file.as_raw_fd(), operation) } == 0 {
                return Ok(());
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }

    /// Read-write, create, no truncation, and no following a symbolic link
    /// in the last component.
    pub(super) fn open_options() -> OpenOptions {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .custom_flags(O_NOFOLLOW);
        options
    }

    /// `O_NOFOLLOW` already refused a link at open.
    pub(super) fn is_reparse_point(_file: &File) -> io::Result<bool> {
        Ok(false)
    }

    /// Whether `file` is still the file at `path` (same device and inode).
    pub(super) fn same_file(file: &File, path: &Path) -> io::Result<bool> {
        let held = file.metadata()?;
        match std::fs::symlink_metadata(path) {
            Ok(at) => Ok(at.dev() == held.dev() && at.ino() == held.ino()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Locks `file` exclusively. `Ok(false)` when `wait` is off and another
    /// holder has it.
    pub(super) fn lock(file: &File, wait: bool) -> io::Result<bool> {
        let operation = if wait { LOCK_EX } else { LOCK_EX | LOCK_NB };
        match call(file, operation) {
            Ok(()) => Ok(true),
            Err(e) if !wait && e.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub(super) fn unlock(file: &File) -> io::Result<()> {
        call(file, LOCK_UN)
    }
}

#[cfg(windows)]
mod sys {
    //! `LockFileEx` / `UnlockFileEx` / `GetFileInformationByHandle` from
    //! `kernel32`.

    use core::ffi::c_void;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    const LOCKFILE_FAIL_IMMEDIATELY: u32 = 0x1;
    const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x2;
    const ERROR_LOCK_VIOLATION: i32 = 33;
    /// Opens a reparse point itself instead of what it points to.
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    /// The locked byte: offset 2⁶³ − 2, far beyond any real content.
    const OFFSET_LOW: u32 = 0xFFFF_FFFE;
    const OFFSET_HIGH: u32 = 0x7FFF_FFFF;

    /// `OVERLAPPED`; the `Offset`/`OffsetHigh` pair is the union member used,
    /// which has the union's size and alignment on 32 and 64 bits alike.
    #[repr(C)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        event: *mut c_void,
    }

    impl Overlapped {
        fn at_lock_byte() -> Self {
            Overlapped {
                internal: 0,
                internal_high: 0,
                offset: OFFSET_LOW,
                offset_high: OFFSET_HIGH,
                event: std::ptr::null_mut(),
            }
        }
    }

    /// `BY_HANDLE_FILE_INFORMATION`: thirteen `DWORD`s (each `FILETIME` is a
    /// pair of them), so 52 bytes with 4-byte alignment on every target.
    #[repr(C)]
    #[derive(Default)]
    struct FileInformation {
        file_attributes: u32,
        _creation_time: [u32; 2],
        _last_access_time: [u32; 2],
        _last_write_time: [u32; 2],
        volume_serial_number: u32,
        _file_size_high: u32,
        _file_size_low: u32,
        _number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LockFileEx(
            file: *mut c_void,
            flags: u32,
            reserved: u32,
            bytes_low: u32,
            bytes_high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
        fn UnlockFileEx(
            file: *mut c_void,
            reserved: u32,
            bytes_low: u32,
            bytes_high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
        fn GetFileInformationByHandle(file: *mut c_void, info: *mut FileInformation) -> i32;
    }

    fn information(file: &File) -> io::Result<FileInformation> {
        let mut info = FileInformation::default();
        // SAFETY: the handle is open for the duration of the call (`file` is
        // borrowed), and `info` is a live, writable `BY_HANDLE_FILE_INFORMATION`
        // of the right size that the call only writes into.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0 {
            Ok(info)
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Read-write, create, no truncation; a reparse point (a symbolic link or
    /// junction) is opened itself, never followed, so it can be refused.
    pub(super) fn open_options() -> OpenOptions {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        options
    }

    pub(super) fn is_reparse_point(file: &File) -> io::Result<bool> {
        Ok(information(file)?.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0)
    }

    /// Whether `file` is still the file at `path` (same volume and file
    /// index).
    pub(super) fn same_file(file: &File, path: &Path) -> io::Result<bool> {
        let held = information(file)?;
        let at = match OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
        {
            Ok(at) => information(&at)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        Ok(at.volume_serial_number == held.volume_serial_number
            && at.file_index_high == held.file_index_high
            && at.file_index_low == held.file_index_low)
    }

    /// Locks `file` exclusively. `Ok(false)` when `wait` is off and another
    /// holder has it.
    pub(super) fn lock(file: &File, wait: bool) -> io::Result<bool> {
        let mut flags = LOCKFILE_EXCLUSIVE_LOCK;
        if !wait {
            flags |= LOCKFILE_FAIL_IMMEDIATELY;
        }
        let mut overlapped = Overlapped::at_lock_byte();
        // SAFETY: the handle is open for the duration of the call (`file` is
        // borrowed) and was opened for synchronous I/O, so the call completes
        // before returning and `overlapped` outlives it.
        let ok = unsafe { LockFileEx(file.as_raw_handle(), flags, 0, 1, 0, &mut overlapped) };
        if ok != 0 {
            return Ok(true);
        }
        let err = io::Error::last_os_error();
        if !wait && err.raw_os_error() == Some(ERROR_LOCK_VIOLATION) {
            return Ok(false);
        }
        Err(err)
    }

    pub(super) fn unlock(file: &File) -> io::Result<()> {
        let mut overlapped = Overlapped::at_lock_byte();
        // SAFETY: as for `LockFileEx`.
        let ok = unsafe { UnlockFileEx(file.as_raw_handle(), 0, 1, 0, &mut overlapped) };
        if ok != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "illumos",
)))]
mod sys {
    //! No OS file lock on this target.

    use std::fs::{File, OpenOptions};
    use std::io;
    use std::path::Path;

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "FileLock is not supported on this platform; use LockFile",
        )
    }

    pub(super) fn open_options() -> OpenOptions {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        options
    }

    pub(super) fn is_reparse_point(_file: &File) -> io::Result<bool> {
        Ok(false)
    }

    pub(super) fn same_file(_file: &File, _path: &Path) -> io::Result<bool> {
        Err(unsupported())
    }

    pub(super) fn lock(_file: &File, _wait: bool) -> io::Result<bool> {
        Err(unsupported())
    }

    pub(super) fn unlock(_file: &File) -> io::Result<()> {
        Err(unsupported())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutils::TempDir;

    #[test]
    fn acquire_is_exclusive_and_the_file_stays_after_release() {
        let dir = TempDir::new("ferroly-oslock").unwrap();
        let path = dir.path().join("a.lock");
        let lock = FileLock::acquire(&path).unwrap();
        assert_eq!(lock.path(), path);
        assert!(FileLock::try_acquire(&path).unwrap().is_none());
        assert!(matches!(FileLock::acquire(&path), Err(FsError::Locked(_))));
        assert_eq!(FileLock::owner(&path).unwrap(), Some(std::process::id()));
        drop(lock);
        assert!(path.exists(), "the lock file is never deleted");
        let again = FileLock::acquire(&path).unwrap();
        again.release().unwrap();
        assert!(FileLock::try_acquire(&path).unwrap().is_some());
    }

    #[test]
    fn acquire_timeout_and_wait_see_a_release_from_another_thread() {
        let dir = TempDir::new("ferroly-oslock-wait").unwrap();
        let path = dir.path().join("a.lock");
        let lock = FileLock::acquire(&path).unwrap();
        let err = FileLock::acquire_timeout(&path, Duration::from_millis(30));
        assert!(matches!(err, Err(FsError::Locked(_))));

        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(lock);
        });
        let lock = FileLock::acquire_wait(&path).unwrap();
        releaser.join().unwrap();
        assert!(FileLock::try_acquire(&path).unwrap().is_none());
        drop(lock);
    }

    #[test]
    fn acquire_timeout_with_an_unrepresentable_deadline_waits_instead_of_panicking() {
        let dir = TempDir::new("ferroly-oslock-max").unwrap();
        let path = dir.path().join("a.lock");
        // Free: taken at once.
        drop(FileLock::acquire_timeout(&path, Duration::MAX).unwrap());

        // Held: waits until the holder lets go.
        let lock = FileLock::acquire(&path).unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(lock);
        });
        let lock = FileLock::acquire_timeout(&path, Duration::MAX).unwrap();
        releaser.join().unwrap();
        assert!(FileLock::try_acquire(&path).unwrap().is_none());
        drop(lock);
    }

    #[test]
    fn only_one_thread_wins() {
        let dir = TempDir::new("ferroly-oslock-race").unwrap();
        let path = dir.path().join("a.lock");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    FileLock::try_acquire(&path).unwrap()
                })
            })
            .collect();
        let guards: Vec<Option<FileLock>> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(guards.iter().filter(|g| g.is_some()).count(), 1);
    }

    #[test]
    fn a_leftover_file_from_a_dead_holder_does_not_block() {
        let dir = TempDir::new("ferroly-oslock-stale").unwrap();
        let path = dir.path().join("a.lock");
        // What a crashed holder leaves: the file with its id, and no lock.
        std::fs::write(&path, "4294967295\n").unwrap();
        let lock = FileLock::try_acquire(&path).unwrap().expect("not held");
        assert_eq!(FileLock::owner(&path).unwrap(), Some(std::process::id()));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{}\n", std::process::id()),
            "a longer old id leaves no trailing digits"
        );
        drop(lock);
    }

    #[test]
    fn locking_a_file_with_content_leaves_the_content_alone() {
        let dir = TempDir::new("ferroly-oslock-data").unwrap();
        for content in [
            &b"precious user data"[..],
            &[0xff, 0xfe, 0x00, 0x01][..],
            &b"12 apples\n"[..],
            &[b'7'; MAX_ID_LEN + 1][..],
        ] {
            let path = dir.path().join("data.db");
            std::fs::write(&path, content).unwrap();
            let lock = FileLock::acquire(&path).unwrap();
            assert!(FileLock::try_acquire(&path).unwrap().is_none());
            assert_eq!(std::fs::read(&path).unwrap(), content);
            assert_eq!(FileLock::owner(&path).unwrap(), None);
            drop(lock);
            assert_eq!(std::fs::read(&path).unwrap(), content);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_at_the_lock_path_is_refused_and_its_target_untouched() {
        let dir = TempDir::new("ferroly-oslock-link").unwrap();
        let victim = dir.path().join("victim.txt");
        std::fs::write(&victim, "").unwrap();
        let link = dir.path().join("app.lock");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        let err = FileLock::acquire(&link).unwrap_err();
        assert!(
            matches!(&err, FsError::Io(e) if e.kind() == io::ErrorKind::InvalidInput),
            "{err:?}"
        );
        assert!(FileLock::try_acquire(&link).is_err());
        assert!(FileLock::acquire_wait(&link).is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"", "target not written");

        // A dangling link does not get its target created.
        let missing = dir.path().join("missing.txt");
        let dangling = dir.path().join("dangling.lock");
        std::os::unix::fs::symlink(&missing, &dangling).unwrap();
        assert!(FileLock::acquire(&dangling).is_err());
        assert!(!missing.exists(), "target not created");
    }

    /// A waiter that opened the file before it was unlinked must not come out
    /// holding a lock on the orphan while a newcomer holds the new file.
    #[cfg(unix)]
    #[test]
    fn a_waiter_on_an_unlinked_file_retries_on_the_new_one() {
        let dir = TempDir::new("ferroly-oslock-unlink").unwrap();
        let path = dir.path().join("a.lock");
        let first = FileLock::acquire(&path).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let waiter_path = path.clone();
        let waiter = std::thread::spawn(move || {
            tx.send(FileLock::acquire_wait(&waiter_path).unwrap())
                .unwrap();
        });
        // Let the waiter open the original file and block on it.
        std::thread::sleep(Duration::from_millis(150));

        std::fs::remove_file(&path).unwrap();
        let newcomer = FileLock::acquire(&path).unwrap();
        drop(first);
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "the waiter must not hold the orphaned file while the newcomer holds the path"
        );

        drop(newcomer);
        let waiter_lock = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        waiter.join().unwrap();
        assert!(FileLock::try_acquire(&path).unwrap().is_none());
        drop(waiter_lock);
    }

    #[test]
    fn missing_directory_is_an_io_error_not_locked() {
        let dir = TempDir::new("ferroly-oslock-missing").unwrap();
        let path = dir.path().join("nope").join("a.lock");
        assert!(matches!(FileLock::acquire(&path), Err(FsError::Io(_))));
    }
}

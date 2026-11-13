//! Read-only file views: [`Mmap`].
//!
//! This is the crate's one `unsafe` region (the parent declares the module with
//! `#[allow(unsafe_code)]`; the workspace lint denies `unsafe` everywhere else).
//! It wraps the POSIX `mmap`/`munmap` FFI, hand-declared because the crate takes
//! no `libc` dependency.
//!
//! # What is and is not safe
//!
//! * [`Mmap::open`] is **safe**: it reads the file into memory. Nothing outside
//!   this process can affect the bytes afterwards.
//! * [`Mmap::map`] is **`unsafe`**: it creates a real `PROT_READ | MAP_PRIVATE`
//!   mapping and hands out `&[u8]` over it. A private mapping protects the
//!   *file* from this process, not this process from the file: until a page is
//!   copied (which a read-only mapping never does), it still reflects the file,
//!   so another process writing to the file changes bytes behind a `&[u8]`, and
//!   truncating the file makes touching the lost pages raise `SIGBUS`. The
//!   compiler cannot rule either out, so the caller must.
//!
//! # Targets
//!
//! The FFI declarations below are only compiled where the declared signature
//! (`off_t` is `i64`, `size_t` is `usize`, `int` is `i32`) and the constants
//! (`PROT_READ = 1`, `MAP_PRIVATE = 2`, `MAP_FAILED = (void *) -1`) are known to
//! be right: 64-bit Linux (glibc or musl), 64-bit Android, macOS, iOS, FreeBSD,
//! DragonFly, NetBSD and OpenBSD. Everywhere else — Windows, every 32-bit
//! target, other Unixes — [`Mmap::map`] reads the file into memory instead, and
//! [`Mmap::is_mapped`] reports which happened.

use std::path::Path;

use super::FsError;

/// The real mapping, compiled only for targets whose ABI was checked.
///
/// The values were checked against the `libc` crate's per-target definitions
/// (0.2.191): `off_t` is `i64` for every BSD-family target
/// (`unix/bsd/mod.rs`), for 64-bit glibc (`linux/gnu/b64/mod.rs`) and for musl
/// (`linux/musl/mod.rs`), and `c_long` — 64 bits on LP64 — for Android;
/// `PROT_READ = 1`, `MAP_PRIVATE = 0x0002` and `MAP_FAILED = !0` are defined
/// once for all of `linux_like`, once for Apple, once for `freebsdlike`
/// (FreeBSD, DragonFly) and once for `netbsdlike` (NetBSD, OpenBSD); and none
/// of these targets renames the `mmap`/`munmap` symbols at 64 bits. The
/// 64-bit requirement also excludes 32-bit x86 macOS, the one Apple target
/// with a different symbol name. Only macOS has been exercised by this
/// crate's own test runs; the others rest on that cross-check.
#[cfg(all(
    target_pointer_width = "64",
    any(
        all(target_os = "linux", any(target_env = "gnu", target_env = "musl")),
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd",
    )
))]
mod sys {
    use core::ffi::{c_int, c_void};
    use std::os::fd::AsRawFd;
    use std::ptr::NonNull;

    /// `off_t` on every target this module is compiled for.
    type OffT = i64;

    extern "C" {
        fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            offset: OffT,
        ) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> c_int;
    }

    const PROT_READ: c_int = 0x1;
    const MAP_PRIVATE: c_int = 0x2;
    /// `mmap` returns `(void *) -1` on failure.
    const MAP_FAILED: usize = usize::MAX;

    /// Whether this target gets a real mapping.
    pub(super) const SUPPORTED: bool = true;

    /// An owned, read-only, private mapping of `len > 0` bytes.
    pub(super) struct Mapping {
        ptr: NonNull<u8>,
        len: usize,
    }

    impl Mapping {
        /// Maps the first `len` bytes of `file`.
        ///
        /// # Safety
        ///
        /// For as long as the returned `Mapping` lives, the file's first `len`
        /// bytes must not be modified and the file must not be truncated below
        /// `len`, by this or any other process. `len` must be non-zero and at
        /// most `isize::MAX`.
        pub(super) unsafe fn map(file: &std::fs::File, len: usize) -> std::io::Result<Mapping> {
            debug_assert!(len != 0 && len <= isize::MAX as usize);
            // SAFETY: the arguments form a valid call — a null `addr` lets the
            // kernel choose the address, `len` is non-zero (the caller's
            // contract; zero would be `EINVAL`), `fd` is open for reading for
            // the duration of the call because `file` is borrowed, and offset 0
            // is page-aligned. The call creates a fresh mapping and cannot
            // disturb any existing one, since `MAP_FIXED` is not passed.
            let ptr = unsafe {
                mmap(
                    std::ptr::null_mut(),
                    len,
                    PROT_READ,
                    MAP_PRIVATE,
                    file.as_raw_fd(),
                    0,
                )
            };
            if ptr as usize == MAP_FAILED {
                return Err(std::io::Error::last_os_error());
            }
            // A successful `mmap` without `MAP_FIXED` never returns null, but a
            // null here must not become a dangling slice.
            match NonNull::new(ptr.cast::<u8>()) {
                Some(ptr) => Ok(Mapping { ptr, len }),
                None => Err(std::io::Error::other("mmap returned a null mapping")),
            }
        }

        pub(super) fn as_slice(&self) -> &[u8] {
            // SAFETY: `ptr` is the start of a live `PROT_READ` mapping of
            // exactly `len` bytes (non-null, page-aligned, `len <= isize::MAX`)
            // that stays mapped until `Drop`, so the range is readable for the
            // borrow's lifetime. That the bytes are also *unchanging and backed
            // by the file* for that long is not something the mapping
            // guarantees — `MAP_PRIVATE` does not snapshot the file — it is the
            // contract the caller of `Mapping::map` accepted.
            unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
        }
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            // SAFETY: `ptr`/`len` are exactly the address and length a
            // successful `mmap` returned, the mapping has not been unmapped
            // before (`Mapping` is not `Clone` and this runs once), and no
            // borrow of the slice can outlive `self`. The file descriptor was
            // closed long ago; a mapping holds its own reference to the file.
            unsafe {
                munmap(self.ptr.as_ptr().cast::<c_void>(), self.len);
            }
        }
    }

    // SAFETY: `Mapping` exclusively owns its address range, exposes it only as
    // shared `&[u8]`, and never writes to it, so moving it to another thread
    // (`Send`) or reading it from several (`Sync`) introduces no data race
    // within this process; `munmap` may be called from any thread. Changes
    // made to the file from elsewhere are excluded by the `Mapping::map`
    // contract rather than by anything here.
    unsafe impl Send for Mapping {}
    // SAFETY: see `Send` above.
    unsafe impl Sync for Mapping {}
}

/// Targets without a checked `mmap` ABI: no mapping type can be constructed,
/// and [`Mmap::map`] reads the file into memory.
#[cfg(not(all(
    target_pointer_width = "64",
    any(
        all(target_os = "linux", any(target_env = "gnu", target_env = "musl")),
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd",
    )
)))]
mod sys {
    /// Whether this target gets a real mapping.
    pub(super) const SUPPORTED: bool = false;

    /// Uninhabited: there is no mapping on this target.
    pub(super) enum Mapping {}

    impl Mapping {
        /// Never called (`SUPPORTED` is false); present so both `sys` modules
        /// expose the same interface.
        ///
        /// # Safety
        ///
        /// None required: this never maps anything.
        pub(super) unsafe fn map(_file: &std::fs::File, _len: usize) -> std::io::Result<Mapping> {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "memory mapping is not supported on this target",
            ))
        }

        pub(super) fn as_slice(&self) -> &[u8] {
            match *self {}
        }
    }
}

enum Repr {
    /// The file's bytes, read into memory.
    Owned(Vec<u8>),
    /// A live OS mapping (only constructed on supported targets).
    Mapped(sys::Mapping),
}

/// A read-only view of a whole file's bytes.
///
/// Dereferences to `&[u8]`. There are two ways to get one:
///
/// | Constructor | Safe? | Backing |
/// |---|---|---|
/// | [`Mmap::open`] | yes | the file is read into memory |
/// | [`Mmap::map`] | **`unsafe`** | a real OS mapping, paged in lazily ([where supported](Mmap::map_supported)) |
///
/// A view is `Send + Sync` and releases its memory (or mapping) on drop.
pub struct Mmap {
    repr: Repr,
}

impl Mmap {
    /// Reads `path` fully into memory and returns a view over the bytes.
    ///
    /// This is the safe constructor: once it returns, nothing another process
    /// does to the file can affect the view. It costs a full read and as much
    /// memory as the file is long — for a lazily paged, zero-copy view of a
    /// large file use the `unsafe` [`Mmap::map`] and uphold its contract.
    ///
    /// An empty file yields an empty view.
    pub fn open(path: impl AsRef<Path>) -> Result<Mmap, FsError> {
        Ok(Mmap {
            repr: Repr::Owned(std::fs::read(path)?),
        })
    }

    /// Memory-maps `path` read-only (`PROT_READ | MAP_PRIVATE`), with pages
    /// faulted in by the OS on first access.
    ///
    /// On targets where the mapping ABI has not been checked
    /// ([`Mmap::map_supported`] is `false`: Windows, 32-bit targets, other
    /// Unixes) this reads the file into memory exactly like [`Mmap::open`]; an
    /// empty file is likewise returned as an empty in-memory view everywhere,
    /// because a zero-length mapping is invalid. [`Mmap::is_mapped`] reports
    /// which happened.
    ///
    /// Fails if `path` cannot be opened, is not a regular file, or is too
    /// large to address on this target.
    ///
    /// # Safety
    ///
    /// The mapping is a window onto the file, not a copy of it. `MAP_PRIVATE`
    /// keeps this process's writes out of the file; it does **not** freeze the
    /// file's contents. The caller must therefore guarantee that, from this
    /// call until the returned `Mmap` is dropped, **no process (including this
    /// one) modifies the mapped bytes or truncates the file**:
    ///
    /// * a write to the file would change bytes behind a live `&[u8]`, which
    ///   is undefined behavior;
    /// * a truncation makes any access to the pages past the new end raise
    ///   `SIGBUS`, killing the process.
    ///
    /// Typical ways to uphold this: map files your process created with
    /// restrictive permissions and never rewrites in place, or immutable
    /// artifacts that are only ever replaced by rename (an [`AtomicFile`]
    /// replacement gives the path a new inode and leaves an existing mapping
    /// untouched). If you cannot guarantee it, use [`Mmap::open`].
    ///
    /// [`AtomicFile`]: super::AtomicFile
    pub unsafe fn map(path: impl AsRef<Path>) -> Result<Mmap, FsError> {
        let path = path.as_ref();
        if !sys::SUPPORTED {
            return Mmap::open(path);
        }
        let file = std::fs::File::open(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(invalid_input("not a regular file; cannot be memory-mapped"));
        }
        // `as usize` would silently truncate a >4 GiB length on a 32-bit
        // target; a slice may also not exceed `isize::MAX` bytes.
        let len = usize::try_from(meta.len())
            .ok()
            .filter(|len| *len <= isize::MAX as usize)
            .ok_or_else(|| invalid_input("file is too large to map on this target"))?;
        if len == 0 {
            // `mmap` rejects a zero length; an empty view needs no mapping.
            return Ok(Mmap {
                repr: Repr::Owned(Vec::new()),
            });
        }
        // SAFETY: `len` is non-zero and at most `isize::MAX` (checked above).
        // That the file is neither modified nor truncated while mapped is this
        // function's own safety contract, passed on unchanged from our caller.
        let mapping = unsafe { sys::Mapping::map(&file, len) }?;
        // `file` is dropped (closed) here; the mapping keeps the file alive.
        Ok(Mmap {
            repr: Repr::Mapped(mapping),
        })
    }

    /// Whether [`Mmap::map`] creates a real OS mapping on this target (as
    /// opposed to falling back to an in-memory read).
    pub const fn map_supported() -> bool {
        sys::SUPPORTED
    }

    /// Whether this view is a live OS mapping (`true`) or an in-memory copy
    /// (`false`).
    pub fn is_mapped(&self) -> bool {
        matches!(self.repr, Repr::Mapped(_))
    }

    /// The file's bytes.
    pub fn as_bytes(&self) -> &[u8] {
        match &self.repr {
            Repr::Owned(data) => data,
            Repr::Mapped(mapping) => mapping.as_slice(),
        }
    }

    /// The length of the view in bytes.
    pub fn len(&self) -> usize {
        self.as_bytes().len()
    }

    /// Whether the view is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn invalid_input(msg: &'static str) -> FsError {
    FsError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, msg))
}

impl std::ops::Deref for Mmap {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl AsRef<[u8]> for Mmap {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl std::fmt::Debug for Mmap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mmap")
            .field("len", &self.len())
            .field("mapped", &self.is_mapped())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutils::TempDir;

    fn sample(len: usize) -> Vec<u8> {
        (0u8..=255).cycle().take(len).collect()
    }

    #[test]
    fn open_reads_file_contents_safely() {
        let dir = TempDir::new("ferroly-mmap-open").unwrap();
        let path = dir.path().join("data.bin");
        let data = sample(10_000);
        std::fs::write(&path, &data).unwrap();

        let view = Mmap::open(&path).unwrap();
        assert!(!view.is_mapped(), "the safe constructor never maps");
        assert_eq!(view.len(), data.len());
        assert!(!view.is_empty());
        assert_eq!(&view[..], &data[..]); // Deref<Target = [u8]>
        assert_eq!(view.as_bytes(), view.as_ref());
    }

    #[test]
    fn open_is_unaffected_by_later_truncation() {
        // The I1 scenario: with the safe constructor, truncating or rewriting
        // the file afterwards must not change (or fault) the view.
        let dir = TempDir::new("ferroly-mmap-trunc").unwrap();
        let path = dir.path().join("data.bin");
        let data = sample(64 * 1024);
        std::fs::write(&path, &data).unwrap();

        let view = Mmap::open(&path).unwrap();
        std::fs::write(&path, b"").unwrap(); // truncate to zero
        assert_eq!(&view[..], &data[..]);
        std::fs::write(&path, vec![0xAA; 64 * 1024]).unwrap(); // rewrite
        assert_eq!(&view[..], &data[..]);
    }

    #[test]
    fn open_empty_and_missing() {
        let dir = TempDir::new("ferroly-mmap-empty").unwrap();
        let path = dir.path().join("empty.bin");
        std::fs::write(&path, b"").unwrap();
        let view = Mmap::open(&path).unwrap();
        assert!(view.is_empty());
        assert_eq!(&view[..], b"");
        assert!(Mmap::open(dir.path().join("missing.bin")).is_err());
    }

    #[test]
    fn map_reads_file_contents() {
        let dir = TempDir::new("ferroly-mmap-map").unwrap();
        let path = dir.path().join("data.bin");
        // Several pages plus a partial one.
        let data = sample(3 * 4096 + 123);
        std::fs::write(&path, &data).unwrap();

        // SAFETY: the file lives in a private temp directory created by this
        // test, and nothing modifies or truncates it while `view` is alive.
        let view = unsafe { Mmap::map(&path) }.unwrap();
        assert_eq!(view.is_mapped(), Mmap::map_supported());
        assert_eq!(view.len(), data.len());
        assert_eq!(&view[..], &data[..]);
        assert_eq!(view.as_bytes().last(), data.last());
    }

    #[test]
    fn map_empty_file_is_an_empty_unmapped_view() {
        let dir = TempDir::new("ferroly-mmap-map-empty").unwrap();
        let path = dir.path().join("empty.bin");
        std::fs::write(&path, b"").unwrap();
        // SAFETY: private temp file, untouched while the view is alive.
        let view = unsafe { Mmap::map(&path) }.unwrap();
        assert!(view.is_empty());
        assert!(!view.is_mapped());
        assert_eq!(&view[..], b"");
    }

    #[test]
    fn map_rejects_missing_files_and_directories() {
        let dir = TempDir::new("ferroly-mmap-map-bad").unwrap();
        // SAFETY: nothing is mapped — both calls fail before `mmap`.
        let missing = unsafe { Mmap::map(dir.path().join("missing.bin")) };
        assert!(missing.is_err());
        // SAFETY: as above.
        let directory = unsafe { Mmap::map(dir.path()) };
        assert!(directory.is_err());
    }

    #[test]
    fn mapping_outlives_the_path_being_replaced() {
        // Replacing a file by rename gives the path a new inode; an existing
        // mapping keeps the old one, which is why rename-only replacement is
        // a sound way to uphold the `map` contract.
        let dir = TempDir::new("ferroly-mmap-rename").unwrap();
        let path = dir.path().join("data.bin");
        let data = sample(2 * 4096);
        std::fs::write(&path, &data).unwrap();

        // SAFETY: the original file is never written or truncated; the path
        // is only re-pointed at a new file by an atomic rename.
        let view = unsafe { Mmap::map(&path) }.unwrap();
        // On Windows the fallback holds no handle, so the rename succeeds
        // there too.
        crate::fsutils::write_atomic(&path, b"replacement").unwrap();
        assert_eq!(&view[..], &data[..]);
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
    }

    #[test]
    fn view_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Mmap>();

        let dir = TempDir::new("ferroly-mmap-threads").unwrap();
        let path = dir.path().join("data.bin");
        let data = sample(4096 * 2);
        std::fs::write(&path, &data).unwrap();
        // SAFETY: private temp file, untouched while the view is alive.
        let view = std::sync::Arc::new(unsafe { Mmap::map(&path) }.unwrap());
        let sums: Vec<u64> = (0..4)
            .map(|_| {
                let view = view.clone();
                std::thread::spawn(move || view.iter().map(|&b| b as u64).sum::<u64>())
            })
            .map(|h| h.join().unwrap())
            .collect();
        let expected: u64 = data.iter().map(|&b| b as u64).sum();
        assert!(sums.iter().all(|&s| s == expected));
    }
}

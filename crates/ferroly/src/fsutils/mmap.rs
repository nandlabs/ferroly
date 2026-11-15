//! Read-only file views: [`Mmap`].
//!
//! This is one of the crate's four `unsafe` regions (the others are the OS
//! file lock in `fsutils/oslock.rs`, the Windows CSPRNG call in
//! `hash/random/windows.rs` and the process-metrics FFI in `metrics`; the parent declares the module with
//! `#[allow(unsafe_code)]`; the workspace lint denies `unsafe` everywhere else).
//! It wraps the POSIX `mmap`/`munmap` FFI and, on Windows, the `kernel32`
//! `CreateFileMappingW` / `MapViewOfFile` / `UnmapViewOfFile` / `CloseHandle`
//! FFI, all hand-declared because the crate takes no `libc` or `windows-sys`
//! dependency.
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
//!   compiler cannot rule either out, so the caller must. On Windows the view
//!   is a `PAGE_READONLY` / `FILE_MAP_READ` mapping with the same property:
//!   it stays coherent with writes made through other handles. Windows
//!   refuses to truncate a file on a local volume while a view of it is
//!   mapped, but not a file on a network share (truncated on the server or by
//!   another client) or on removable media that goes away; there, touching a
//!   lost page raises `EXCEPTION_IN_PAGE_ERROR`, which kills the process as
//!   `SIGBUS` does.
//!
//! # Targets
//!
//! The FFI declarations below are only compiled where the declared signature
//! (`off_t` is `i64`, `size_t` is `usize`, `int` is `i32`) and the constants
//! (`PROT_READ = 1`, `MAP_PRIVATE = 2`, `MAP_FAILED = (void *) -1`) are known to
//! be right: 64-bit Linux (glibc or musl), 64-bit Android, macOS, iOS, FreeBSD,
//! DragonFly, NetBSD and OpenBSD. The Windows declarations are compiled for
//! 64-bit Windows (`-msvc` and `-gnu`). Everywhere else — every 32-bit target,
//! other Unixes — [`Mmap::map`] reads the file into memory instead, and
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

    /// Opens `path` for reading.
    pub(super) fn open(path: &std::path::Path) -> std::io::Result<std::fs::File> {
        std::fs::File::open(path)
    }

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

/// The real mapping on 64-bit Windows: `CreateFileMappingW` +
/// `MapViewOfFile` from `kernel32`.
///
/// The declarations match the Win32 prototypes in `memoryapi.h` and
/// `handleapi.h` (Windows SDK; the MinGW-w64 headers agree):
///
/// ```c
/// HANDLE CreateFileMappingW(
///     HANDLE                hFile,
///     LPSECURITY_ATTRIBUTES lpFileMappingAttributes, // pointer, may be NULL
///     DWORD                 flProtect,               // 32-bit unsigned
///     DWORD                 dwMaximumSizeHigh,
///     DWORD                 dwMaximumSizeLow,
///     LPCWSTR               lpName);                 // const WCHAR *, may be NULL
/// LPVOID MapViewOfFile(
///     HANDLE hFileMappingObject,
///     DWORD  dwDesiredAccess,
///     DWORD  dwFileOffsetHigh,
///     DWORD  dwFileOffsetLow,
///     SIZE_T dwNumberOfBytesToMap);                  // pointer-sized unsigned
/// BOOL UnmapViewOfFile(LPCVOID lpBaseAddress);       // BOOL is a 32-bit int
/// BOOL CloseHandle(HANDLE hObject);
/// ```
///
/// All four are `WINAPI`, which is what `extern "system"` means (`__stdcall`
/// on 32-bit x86, the platform C convention on x86-64 and AArch64). `HANDLE`
/// is `void *`; `CreateFileMappingW` and `MapViewOfFile` return NULL on
/// failure (never `INVALID_HANDLE_VALUE`) and set the thread's last error,
/// which `std::io::Error::last_os_error` reads (`GetLastError`). The
/// constants are `PAGE_READONLY = 0x02` (`winnt.h`) and
/// `FILE_MAP_READ = SECTION_MAP_READ = 0x0004` (`memoryapi.h`). `kernel32` is
/// an import library of every Windows SDK and of MinGW-w64, so both the
/// `-msvc` and `-gnu` targets link it. Restricted to 64 bits so that any file
/// length the API can map fits in `usize`; the CI runner (`windows-latest`)
/// is x86-64.
///
/// The file itself is opened by `std`, which calls `CreateFileW` (the wide
/// API, so any Unicode path works, with `std`'s `\\?\` handling of long
/// paths), with an explicit share mode that lets other handles — in this or
/// another process — keep the file open for reading, writing or deletion.
#[cfg(all(windows, target_pointer_width = "64"))]
mod sys {
    use core::ffi::c_void;
    use std::fs::File;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use std::ptr::NonNull;

    /// `HANDLE`.
    type Handle = *mut c_void;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileMappingW(
            file: Handle,
            attributes: *mut c_void,
            protect: u32,
            maximum_size_high: u32,
            maximum_size_low: u32,
            name: *const u16,
        ) -> Handle;
        fn MapViewOfFile(
            mapping: Handle,
            desired_access: u32,
            offset_high: u32,
            offset_low: u32,
            bytes_to_map: usize,
        ) -> *mut c_void;
        fn UnmapViewOfFile(base_address: *const c_void) -> i32;
        fn CloseHandle(object: Handle) -> i32;
    }

    /// `PAGE_READONLY` (`winnt.h`).
    const PAGE_READONLY: u32 = 0x02;
    /// `FILE_MAP_READ` (`memoryapi.h`; equal to `SECTION_MAP_READ`).
    const FILE_MAP_READ: u32 = 0x0004;
    /// `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE` (`winnt.h`):
    /// opening the file to map it never conflicts with another holder of the
    /// file, whatever access that holder has.
    const SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;

    /// Whether this target gets a real mapping.
    pub(super) const SUPPORTED: bool = true;

    /// Opens `path` read-only for mapping, sharing it with every other
    /// opener (readers, writers and deleters alike).
    pub(super) fn open(path: &Path) -> std::io::Result<File> {
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(SHARE_ALL)
            .open(path)
    }

    /// An owned, read-only view of `len > 0` bytes and the file-mapping
    /// object behind it.
    pub(super) struct Mapping {
        view: NonNull<u8>,
        len: usize,
        mapping: Handle,
    }

    impl Mapping {
        /// Maps the first `len` bytes of `file`.
        ///
        /// # Safety
        ///
        /// For as long as the returned `Mapping` lives, the file's first `len`
        /// bytes must not be modified, by this or any other process, and the
        /// file must not be truncated below `len` bytes. `len` must be
        /// non-zero, at most `isize::MAX` and at most the file's length.
        /// (Windows refuses to truncate a file on a local volume while a view
        /// of it is mapped, but a file on a network share can still be
        /// truncated by the server or another client, and removable media can
        /// go away; a lost page then raises `EXCEPTION_IN_PAGE_ERROR`.)
        pub(super) unsafe fn map(file: &File, len: usize) -> std::io::Result<Mapping> {
            debug_assert!(len != 0 && len <= isize::MAX as usize);
            // SAFETY: `file` is borrowed, so its handle is open for the
            // duration of the call, with `GENERIC_READ` access — exactly what
            // `PAGE_READONLY` requires. Null security attributes and a null
            // name are documented as allowed (default security, an unnamed
            // object), and a maximum size of 0/0 means "the file's current
            // size". The call creates a new kernel object and touches no
            // memory of ours.
            let mapping = unsafe {
                CreateFileMappingW(
                    file.as_raw_handle(),
                    std::ptr::null_mut(),
                    PAGE_READONLY,
                    0,
                    0,
                    std::ptr::null(),
                )
            };
            if mapping.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: `mapping` is the live file-mapping handle created just
            // above with `PAGE_READONLY`, which permits a `FILE_MAP_READ`
            // view. Offset 0 satisfies the allocation-granularity alignment
            // rule, and `len` (non-zero) does not exceed the mapping's size,
            // the file's length (the caller's contract). The system chooses
            // the address, so no existing memory is affected.
            let view = unsafe { MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, len) };
            match NonNull::new(view.cast::<u8>()) {
                Some(view) => Ok(Mapping { view, len, mapping }),
                None => {
                    let err = std::io::Error::last_os_error();
                    // SAFETY: `mapping` is a live handle owned only by this
                    // function; it is closed exactly once, here, on the error
                    // path.
                    unsafe {
                        CloseHandle(mapping);
                    }
                    Err(err)
                }
            }
        }

        pub(super) fn as_slice(&self) -> &[u8] {
            // SAFETY: `view` is the base of a live `FILE_MAP_READ` view of
            // exactly `len` bytes (non-null, allocation-granularity aligned,
            // `len <= isize::MAX`) that stays mapped until `Drop`, so the
            // range is readable for the borrow's lifetime. That the bytes are
            // also unchanging for that long is the contract the caller of
            // `Mapping::map` accepted: the view stays coherent with the file,
            // so a write through another handle would show through it.
            unsafe { std::slice::from_raw_parts(self.view.as_ptr(), self.len) }
        }
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            // SAFETY: `view` is exactly the base address `MapViewOfFile`
            // returned and `mapping` the handle `CreateFileMappingW` returned;
            // neither has been released before (`Mapping` is not `Clone` and
            // this runs once), and no borrow of the slice can outlive `self`.
            // The view is unmapped before its mapping object is closed. The
            // file handle was closed long ago: the mapping object keeps its
            // own reference to the file.
            unsafe {
                UnmapViewOfFile(self.view.as_ptr().cast_const().cast::<c_void>());
                CloseHandle(self.mapping);
            }
        }
    }

    // SAFETY: `Mapping` exclusively owns its view and its mapping handle,
    // exposes the view only as shared `&[u8]`, and never writes to it, so
    // moving it to another thread (`Send`) or reading it from several
    // (`Sync`) introduces no data race within this process. Kernel handles
    // and views belong to the process, not to a thread: `UnmapViewOfFile` and
    // `CloseHandle` may be called from any thread. Changes made to the file
    // from elsewhere are excluded by the `Mapping::map` contract rather than
    // by anything here.
    unsafe impl Send for Mapping {}
    // SAFETY: see `Send` above.
    unsafe impl Sync for Mapping {}

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Win32 type names, as `windows.h` defines them on 64-bit targets.
        type Dword = u32;
        type Bool = i32;
        type SizeT = usize;
        type Lpcwstr = *const u16;
        type Lpvoid = *mut c_void;
        type Lpcvoid = *const c_void;

        /// Pins each declaration to the signature transcribed from its
        /// prototype (the aliases above): a change to a parameter or return
        /// type stops this from compiling. It guards against edits; it does
        /// not check the transcription itself, which was done by hand
        /// against the SDK headers (the prototypes are quoted on `sys`).
        #[test]
        fn declarations_are_pinned_to_their_transcribed_signatures() {
            let _: unsafe extern "system" fn(
                Handle,
                Lpvoid, // LPSECURITY_ATTRIBUTES
                Dword,
                Dword,
                Dword,
                Lpcwstr,
            ) -> Handle = CreateFileMappingW;
            let _: unsafe extern "system" fn(Handle, Dword, Dword, Dword, SizeT) -> Lpvoid =
                MapViewOfFile;
            let _: unsafe extern "system" fn(Lpcvoid) -> Bool = UnmapViewOfFile;
            let _: unsafe extern "system" fn(Handle) -> Bool = CloseHandle;
            assert_eq!(std::mem::size_of::<Handle>(), 8);
            assert_eq!(std::mem::size_of::<SizeT>(), 8);
            assert_eq!(PAGE_READONLY, 0x02);
            assert_eq!(FILE_MAP_READ, 0x04);
            assert_eq!(SHARE_ALL, 0x07);
        }
    }
}

/// Targets without a checked mapping ABI: no mapping type can be constructed,
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
        windows,
    )
)))]
mod sys {
    /// Whether this target gets a real mapping.
    pub(super) const SUPPORTED: bool = false;

    /// Opens `path` for reading.
    pub(super) fn open(path: &std::path::Path) -> std::io::Result<std::fs::File> {
        std::fs::File::open(path)
    }

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

    /// Memory-maps `path` read-only (`PROT_READ | MAP_PRIVATE` on Unix,
    /// `PAGE_READONLY` + `FILE_MAP_READ` on Windows), with pages faulted in by
    /// the OS on first access and shared with the OS page cache.
    ///
    /// On Windows the file is opened with a share mode that admits every other
    /// opener, so a file another process holds open for writing can still be
    /// mapped. While the view lives, Windows refuses to truncate a file on a
    /// local volume and may refuse to delete or replace (rename over) it;
    /// those operations succeed again once the view is dropped. A file on a
    /// network share or removable media has no such protection (see the
    /// safety section).
    ///
    /// On targets where the mapping ABI has not been checked
    /// ([`Mmap::map_supported`] is `false`: 32-bit targets, other Unixes)
    /// this reads the file into memory exactly like [`Mmap::open`]; an
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
    ///   `SIGBUS` (on Windows, `EXCEPTION_IN_PAGE_ERROR`), killing the
    ///   process. Windows refuses the truncation itself only on a local
    ///   volume: a file on a network share can be truncated by the server or
    ///   another client, and removable media can disappear, while it is
    ///   mapped.
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
        let file = sys::open(path)?;
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
            // `mmap` rejects a zero length and `CreateFileMappingW` an empty
            // file; an empty view needs no mapping.
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
    /// opposed to falling back to an in-memory read): `true` on 64-bit Linux
    /// (glibc, musl), Android, macOS, iOS, FreeBSD, DragonFly, NetBSD,
    /// OpenBSD and Windows; `false` everywhere else.
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
        let replaced = crate::fsutils::write_atomic(&path, b"replacement");
        if cfg!(windows) && view.is_mapped() {
            // Windows may refuse to replace a file that has a mapped view
            // (depending on the rename semantics the filesystem offers); the
            // view is intact either way, and once it is dropped the
            // replacement goes through.
            assert_eq!(&view[..], &data[..]);
            drop(view);
            if replaced.is_err() {
                crate::fsutils::write_atomic(&path, b"replacement").unwrap();
            }
        } else {
            replaced.unwrap();
            assert_eq!(&view[..], &data[..]);
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
    }

    #[test]
    fn map_supported_matches_the_target() {
        let expected = cfg!(all(
            target_pointer_width = "64",
            any(
                all(
                    target_os = "linux",
                    any(target_env = "gnu", target_env = "musl")
                ),
                target_os = "android",
                target_os = "macos",
                target_os = "ios",
                target_os = "freebsd",
                target_os = "dragonfly",
                target_os = "netbsd",
                target_os = "openbsd",
                windows,
            )
        ));
        assert_eq!(Mmap::map_supported(), expected);
        // The CI runners (Linux, macOS and Windows, all 64-bit) map for real.
        if cfg!(all(
            target_pointer_width = "64",
            any(target_os = "linux", target_os = "macos", windows)
        )) {
            assert!(Mmap::map_supported());
        }
    }

    #[test]
    fn map_non_ascii_path() {
        let dir = TempDir::new("ferroly-mmap-unicode").unwrap();
        let path = dir.path().join("donn\u{e9}es-\u{65e5}\u{672c}-\u{fc}.bin");
        let data = sample(4096 + 7);
        std::fs::write(&path, &data).unwrap();
        // SAFETY: private temp file, untouched while the view is alive.
        let view = unsafe { Mmap::map(&path) }.unwrap();
        assert_eq!(view.is_mapped(), Mmap::map_supported());
        assert_eq!(&view[..], &data[..]);
    }

    #[test]
    fn map_while_another_handle_holds_the_file_open_for_writing() {
        let dir = TempDir::new("ferroly-mmap-shared").unwrap();
        let path = dir.path().join("data.bin");
        let data = sample(2 * 4096 + 1);
        // A writer that keeps its handle open for the whole test, as another
        // process still holding a download would.
        let mut writer = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut writer, &data).unwrap();
        std::io::Write::flush(&mut writer).unwrap();
        // SAFETY: the writer has finished writing and does not write again
        // while the view is alive.
        let view = unsafe { Mmap::map(&path) }.unwrap();
        assert_eq!(view.is_mapped(), Mmap::map_supported());
        assert_eq!(&view[..], &data[..]);
        drop(view);
        drop(writer);
    }

    /// The strictest writer Windows lets coexist with a reader: it denies
    /// other writers and deleters (`FILE_SHARE_READ` only). Mapping still
    /// succeeds because the mapping opens for reading only and itself shares
    /// read, write and delete access.
    #[cfg(windows)]
    #[test]
    fn map_while_a_writer_shares_only_reads() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;

        let dir = TempDir::new("ferroly-mmap-share-read").unwrap();
        let path = dir.path().join("data.bin");
        let data = sample(3 * 4096);
        let mut writer = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .share_mode(FILE_SHARE_READ)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut writer, &data).unwrap();
        std::io::Write::flush(&mut writer).unwrap();
        // SAFETY: the writer does not write again while the view is alive.
        let view = unsafe { Mmap::map(&path) }.unwrap();
        assert_eq!(view.is_mapped(), Mmap::map_supported());
        assert_eq!(&view[..], &data[..]);
        drop(view);
        drop(writer);
    }

    /// A 1 GiB sparse file maps as a real view, and reading a few pages
    /// from its start, middle and end works. The file is extended with
    /// `set_len` and written only at its start, so the rest is a hole (Unix)
    /// or past the valid data length (NTFS) and costs neither disk writes nor
    /// reads. This shows the view is a mapping (`is_mapped`), not a copy; it
    /// does not measure resident memory, and 1 GiB is not larger than the
    /// RAM of the machines it runs on.
    #[test]
    fn map_large_sparse_file_and_read_a_few_pages() {
        if !Mmap::map_supported() {
            return; // the fallback would read the whole file into memory
        }
        const LEN: u64 = 1 << 30;
        let dir = TempDir::new("ferroly-mmap-large").unwrap();
        let path = dir.path().join("large.bin");
        {
            use std::io::Write;
            let mut file = std::fs::File::create(&path).unwrap();
            file.write_all(b"start").unwrap();
            file.set_len(LEN).unwrap();
        }
        // SAFETY: private temp file, untouched while the view is alive.
        let view = unsafe { Mmap::map(&path) }.unwrap();
        assert!(view.is_mapped());
        assert_eq!(view.len() as u64, LEN);
        assert_eq!(&view[..5], b"start");
        let mid = (LEN / 2) as usize;
        assert_eq!(view[mid], 0);
        assert_eq!(view[view.len() - 1], 0);
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

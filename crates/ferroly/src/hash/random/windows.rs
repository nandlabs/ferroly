//! The Windows system CSPRNG, `BCryptGenRandom`.
//!
//! This is one of the crate's four `unsafe` regions (the parent declares the
//! module with `#[allow(unsafe_code)]`; the workspace lint denies `unsafe`
//! everywhere else). It holds one hand-written FFI declaration, because the
//! crate takes no `windows-sys` / `getrandom` dependency, and one call site.
//!
//! The declaration matches the Win32 prototype in `bcrypt.h`:
//!
//! ```c
//! NTSTATUS WINAPI BCryptGenRandom(
//!     BCRYPT_ALG_HANDLE hAlgorithm, // PVOID
//!     PUCHAR            pbBuffer,
//!     ULONG             cbBuffer,   // 32-bit unsigned
//!     ULONG             dwFlags);
//! ```
//!
//! `WINAPI` is `__stdcall` on 32-bit x86 and the platform C convention
//! elsewhere, which is exactly what Rust's `extern "system"` means. `NTSTATUS`
//! is a 32-bit signed `LONG`; success values are non-negative (`NT_SUCCESS`).
//! With `BCRYPT_USE_SYSTEM_PREFERRED_RNG` the algorithm handle must be null
//! and the system-preferred RNG is used (Windows Vista and later), so there is
//! no handle to open or close. `bcrypt` is an import library of every Windows
//! SDK and of MinGW-w64, so both the `-msvc` and `-gnu` targets link it.

use std::ffi::c_void;
use std::io;

use super::RandomError;

/// `BCRYPT_USE_SYSTEM_PREFERRED_RNG` from `bcrypt.h`.
const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;

#[link(name = "bcrypt")]
extern "system" {
    fn BCryptGenRandom(algorithm: *mut c_void, buffer: *mut u8, buffer_len: u32, flags: u32)
        -> i32;
}

/// Fills `buf` from the system-preferred RNG, in chunks of at most
/// `u32::MAX` bytes (the API takes a 32-bit length).
pub(super) fn fill(buf: &mut [u8]) -> Result<(), RandomError> {
    for chunk in buf.chunks_mut(u32::MAX as usize) {
        // `chunks_mut` bounds every chunk by `u32::MAX`, so this cannot
        // truncate.
        let len = chunk.len() as u32;
        // SAFETY: `chunk` is a live, exclusively borrowed buffer of exactly
        // `len` writable bytes; BCryptGenRandom writes at most `len` bytes to
        // it and keeps no pointer after returning. The null algorithm handle
        // is the documented requirement of BCRYPT_USE_SYSTEM_PREFERRED_RNG.
        let status = unsafe {
            BCryptGenRandom(
                std::ptr::null_mut(),
                chunk.as_mut_ptr(),
                len,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        if status < 0 {
            return Err(RandomError::Io(io::Error::other(format!(
                "BCryptGenRandom failed (NTSTATUS {:#010x})",
                status as u32
            ))));
        }
    }
    Ok(())
}

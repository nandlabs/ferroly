//! Cryptographically secure random bytes from the operating system.
//!
//! ```no_run
//! use ferroly::hash::random;
//!
//! # fn main() -> Result<(), random::RandomError> {
//! let mut key = [0u8; 32];
//! random::fill(&mut key)?;                 // raw key material
//! let session_id = random::token_hex(16)?; // 32 hex characters
//! let api_token = random::token_base64url(32)?; // 43 url-safe characters
//! # Ok(())
//! # }
//! ```
//!
//! # Platform support
//!
//! ferroly allows neither extra dependencies nor `unsafe`, which rules out the
//! `getrandom(2)` / `BCryptGenRandom` system calls. The only remaining portable
//! route to the kernel CSPRNG is the device file:
//!
//! | Platform | Source | Result |
//! |---|---|---|
//! | Unix (Linux, macOS, the BSDs, …) | `/dev/urandom` | random bytes |
//! | Everything else (Windows, wasm, …) | none | [`RandomError::Unsupported`] |
//!
//! There is deliberately **no fallback** to a weaker source (time, addresses,
//! a userspace PRNG): an unsupported platform is an explicit error, never
//! silently predictable output.
//!
//! Limits of the Unix path:
//!
//! - `/dev/urandom` must exist and be a character device. In a `chroot`,
//!   container or sandbox without it, every call fails with
//!   [`RandomError::Io`].
//! - It consumes a file descriptor (opened once, kept for the process
//!   lifetime), so it can fail under descriptor exhaustion on first use.
//! - On Linux, `/dev/urandom` does not block before the kernel pool has been
//!   initialised. That matters only very early at boot (initramfs, first boot
//!   of an embedded device); do not generate long-lived keys there.

use std::fmt;

use super::{base64, hex, Secret};

/// Why random bytes could not be produced.
#[derive(Debug)]
#[non_exhaustive]
pub enum RandomError {
    /// This platform has no OS randomness source reachable without `unsafe`
    /// or a dependency (anything that is not Unix).
    Unsupported,
    /// The OS source exists but could not be opened or read.
    Io(std::io::Error),
}

impl fmt::Display for RandomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RandomError::Unsupported => {
                f.write_str("secure randomness is not supported on this platform")
            }
            RandomError::Io(e) => write!(f, "reading the OS randomness source failed: {e}"),
        }
    }
}

impl std::error::Error for RandomError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RandomError::Unsupported => None,
            RandomError::Io(e) => Some(e),
        }
    }
}

impl From<std::io::Error> for RandomError {
    fn from(e: std::io::Error) -> Self {
        RandomError::Io(e)
    }
}

/// Whether [`fill`] can work on this platform (it may still fail at runtime
/// if `/dev/urandom` is missing).
pub const fn is_supported() -> bool {
    cfg!(unix)
}

/// Fills `buf` with cryptographically secure random bytes from the OS.
///
/// On error the contents of `buf` are unspecified and must not be used.
pub fn fill(buf: &mut [u8]) -> Result<(), RandomError> {
    os_fill(buf)
}

#[cfg(unix)]
fn os_fill(buf: &mut [u8]) -> Result<(), RandomError> {
    use std::fs::File;
    use std::io::Read;
    use std::os::unix::fs::FileTypeExt;
    use std::sync::OnceLock;

    static DEVICE: OnceLock<File> = OnceLock::new();

    let device = match DEVICE.get() {
        Some(device) => device,
        None => {
            let file = File::open("/dev/urandom")?;
            // A regular file planted at that path would hand out fixed bytes.
            if !file.metadata()?.file_type().is_char_device() {
                return Err(RandomError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "/dev/urandom is not a character device",
                )));
            }
            DEVICE.get_or_init(|| file)
        }
    };
    let mut reader: &File = device;
    reader.read_exact(buf)?;
    Ok(())
}

#[cfg(not(unix))]
fn os_fill(_buf: &mut [u8]) -> Result<(), RandomError> {
    Err(RandomError::Unsupported)
}

/// Returns `len` random bytes.
pub fn bytes(len: usize) -> Result<Vec<u8>, RandomError> {
    let mut out = vec![0u8; len];
    fill(&mut out)?;
    Ok(out)
}

/// Returns a random fixed-size array, e.g. `let key: [u8; 32] = array()?;`.
pub fn array<const N: usize>() -> Result<[u8; N], RandomError> {
    let mut out = [0u8; N];
    fill(&mut out)?;
    Ok(out)
}

/// Returns a uniformly random `u64`.
pub fn u64() -> Result<u64, RandomError> {
    Ok(u64::from_le_bytes(array()?))
}

/// Returns `len` random bytes wrapped in a redacting [`Secret`].
pub fn secret(len: usize) -> Result<Secret, RandomError> {
    Ok(Secret::new(bytes(len)?))
}

/// A random token of `byte_len` random bytes as lowercase hex
/// (`2 * byte_len` characters). 16 bytes (128 bits) is a sound minimum for
/// unguessable identifiers.
pub fn token_hex(byte_len: usize) -> Result<String, RandomError> {
    Ok(hex::encode(&bytes(byte_len)?))
}

/// A random token of `byte_len` random bytes as unpadded URL-safe base64
/// (`ceil(byte_len * 4 / 3)` characters) — safe in URLs, headers and cookies.
pub fn token_base64url(byte_len: usize) -> Result<String, RandomError> {
    Ok(base64::encode_url_no_pad(&bytes(byte_len)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn fill_produces_distinct_non_constant_output() {
        let mut a = [0u8; 64];
        let mut b = [0u8; 64];
        fill(&mut a).unwrap();
        fill(&mut b).unwrap();
        assert_ne!(a, b);
        assert_ne!(a, [0u8; 64]);
        // 64 random bytes are all-equal with probability 2^-504.
        assert!(a.iter().any(|&x| x != a[0]));
    }

    #[cfg(unix)]
    #[test]
    fn helpers_have_the_documented_shapes() {
        assert!(is_supported());
        assert!(fill(&mut []).is_ok());
        assert_eq!(bytes(0).unwrap().len(), 0);
        assert_eq!(bytes(33).unwrap().len(), 33);
        let key: [u8; 32] = array().unwrap();
        assert_ne!(key, [0u8; 32]);
        assert_ne!(u64().unwrap(), u64().unwrap());
        assert_eq!(secret(24).unwrap().len(), 24);

        let hex_token = token_hex(16).unwrap();
        assert_eq!(hex_token.len(), 32);
        assert!(hex_token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert_ne!(hex_token, token_hex(16).unwrap());

        let url_token = token_base64url(32).unwrap();
        assert_eq!(url_token.len(), 43);
        assert!(url_token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        assert_eq!(base64::decode_url_no_pad(&url_token).unwrap().len(), 32);
    }

    #[cfg(unix)]
    #[test]
    fn large_requests_are_filled_completely() {
        // Larger than any single read the kernel is obliged to satisfy.
        let big = bytes(1 << 20).unwrap();
        let zeros = big.iter().filter(|&&b| b == 0).count();
        // Expect ~4096 zero bytes; a short read would leave a long zero tail.
        assert!(zeros < 8192, "{zeros} zero bytes in 1 MiB");
        assert!(big[big.len() - 4096..].iter().any(|&b| b != 0));
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_callers_get_independent_bytes() {
        let handles: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(|| array::<32>().unwrap()))
            .collect();
        let mut seen = std::collections::HashSet::new();
        for handle in handles {
            assert!(seen.insert(handle.join().unwrap()));
        }
    }

    #[cfg(not(unix))]
    #[test]
    fn unsupported_platforms_fail_explicitly() {
        assert!(!is_supported());
        assert!(matches!(fill(&mut [0u8; 8]), Err(RandomError::Unsupported)));
        assert!(matches!(token_hex(8), Err(RandomError::Unsupported)));
    }
}

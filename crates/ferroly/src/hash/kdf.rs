//! Key derivation: HKDF (RFC 5869) and PBKDF2 (RFC 8018 §5.2).
//!
//! - **HKDF** turns *already strong* key material (a Diffie–Hellman output, a
//!   random master key) into one or more independent keys. It is fast by
//!   design and must **not** be used on passwords.
//! - **PBKDF2** stretches a low-entropy secret by iterating an HMAC. It is the
//!   only password-based KDF here; it is not memory-hard, so for storing user
//!   passwords prefer a dedicated algorithm (Argon2id, scrypt, bcrypt), none
//!   of which ferroly implements.
//!
//! ```
//! use ferroly::hash::kdf;
//!
//! // Two independent 32-byte keys from one master secret.
//! let (mut enc, mut mac) = ([0u8; 32], [0u8; 32]);
//! kdf::hkdf_sha256(b"salt", b"master key material", b"enc", &mut enc).unwrap();
//! kdf::hkdf_sha256(b"salt", b"master key material", b"mac", &mut mac).unwrap();
//! assert_ne!(enc, mac);
//!
//! // A key from a passphrase (use a random per-secret salt and a high count).
//! let mut key = [0u8; 32];
//! kdf::pbkdf2_hmac_sha256(b"correct horse", b"random-salt", 10_000, &mut key).unwrap();
//! ```

use std::fmt;

use super::{HashAlgorithm, Hmac, Sha256};

/// A key-derivation parameter error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum KdfError {
    /// HKDF can produce at most `255 * hash_len` bytes of output.
    OutputTooLong,
    /// PBKDF2 needs an iteration count of at least 1.
    ZeroIterations,
}

impl fmt::Display for KdfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KdfError::OutputTooLong => f.write_str("requested HKDF output is too long"),
            KdfError::ZeroIterations => f.write_str("PBKDF2 iteration count must be at least 1"),
        }
    }
}

impl std::error::Error for KdfError {}

/// HKDF-Extract: concentrates `ikm` (input key material) into a fixed-length
/// pseudorandom key. An empty `salt` is equivalent to the RFC's default of
/// `hash_len` zero bytes.
pub fn hkdf_extract<H: HashAlgorithm>(salt: &[u8], ikm: &[u8]) -> H::Output {
    let mut mac = Hmac::<H>::new(salt);
    mac.update(ikm);
    mac.finalize()
}

/// HKDF-Expand: fills `okm` with output key material derived from the
/// pseudorandom key `prk` (normally an [`hkdf_extract`] result) and the
/// context string `info`.
///
/// Fails with [`KdfError::OutputTooLong`] if `okm` is longer than
/// `255 * hash_len` bytes.
pub fn hkdf_expand<H: HashAlgorithm>(
    prk: &[u8],
    info: &[u8],
    okm: &mut [u8],
) -> Result<(), KdfError> {
    if okm.len() > 255 * H::OUTPUT_LEN {
        return Err(KdfError::OutputTooLong);
    }
    let keyed = Hmac::<H>::new(prk);
    let mut previous: Option<H::Output> = None;
    for (index, chunk) in okm.chunks_mut(H::OUTPUT_LEN).enumerate() {
        let mut mac = keyed.clone();
        if let Some(prev) = &previous {
            mac.update(prev.as_ref());
        }
        mac.update(info);
        mac.update(&[(index + 1) as u8]);
        let block = mac.finalize();
        chunk.copy_from_slice(&block.as_ref()[..chunk.len()]);
        previous = Some(block);
    }
    Ok(())
}

/// HKDF (extract-then-expand) with the hash `H`.
pub fn hkdf<H: HashAlgorithm>(
    salt: &[u8],
    ikm: &[u8],
    info: &[u8],
    okm: &mut [u8],
) -> Result<(), KdfError> {
    let prk = hkdf_extract::<H>(salt, ikm);
    hkdf_expand::<H>(prk.as_ref(), info, okm)
}

/// HKDF-SHA256 (RFC 5869): derives `okm.len()` bytes (at most 8160) from the
/// high-entropy `ikm`, an optional `salt` and a context string `info`.
pub fn hkdf_sha256(salt: &[u8], ikm: &[u8], info: &[u8], okm: &mut [u8]) -> Result<(), KdfError> {
    hkdf::<Sha256>(salt, ikm, info, okm)
}

/// PBKDF2 with HMAC over the hash `H`: fills `out` with a key derived from
/// `password` and `salt` using `iterations` rounds.
pub fn pbkdf2_hmac<H: HashAlgorithm>(
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    out: &mut [u8],
) -> Result<(), KdfError> {
    if iterations == 0 {
        return Err(KdfError::ZeroIterations);
    }
    // Key the HMAC once; every round starts from a clone of this state.
    let keyed = Hmac::<H>::new(password);
    for (index, chunk) in out.chunks_mut(H::OUTPUT_LEN).enumerate() {
        let block_number = (index as u32).wrapping_add(1);
        let mut mac = keyed.clone();
        mac.update(salt);
        mac.update(&block_number.to_be_bytes());
        let mut u = mac.finalize();
        let mut acc = u.as_ref().to_vec();
        for _ in 1..iterations {
            let mut mac = keyed.clone();
            mac.update(u.as_ref());
            u = mac.finalize();
            for (a, b) in acc.iter_mut().zip(u.as_ref()) {
                *a ^= b;
            }
        }
        chunk.copy_from_slice(&acc[..chunk.len()]);
    }
    Ok(())
}

/// PBKDF2-HMAC-SHA256.
///
/// Choose the iteration count for your hardware and threat model; as a
/// reference point OWASP's 2023 guidance for PBKDF2-HMAC-SHA256 is 600,000.
/// The cost is linear in `iterations` and this implementation has no hardware
/// acceleration, so benchmark before picking a number. Always use a unique
/// random salt of at least 16 bytes per secret (see
/// [`random`](super::random)).
pub fn pbkdf2_hmac_sha256(
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    out: &mut [u8],
) -> Result<(), KdfError> {
    pbkdf2_hmac::<Sha256>(password, salt, iterations, out)
}

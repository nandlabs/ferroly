//! Dependency-free hashing, MACs, key derivation, encodings and randomness.
//!
//! Public building blocks for content-addressing, integrity checks, request
//! signing and token handling:
//!
//! | Area | Items |
//! |---|---|
//! | Cryptographic hashes | [`Sha256`], [`Sha384`], [`Sha512`] (and legacy [`Sha1`]) |
//! | Checksums (**not** cryptographic) | [`Md5`], [`Crc32`] |
//! | Message authentication | [`Hmac`] ([`HmacSha256`], [`HmacSha384`], [`HmacSha512`], [`HmacSha1`]) |
//! | Key derivation | [`kdf`]: HKDF, PBKDF2 |
//! | Encodings | [`base64`], [`hex`] |
//! | Comparison | [`ct_eq`], [`Digest::ct_eq`] |
//! | Secrets | [`Secret`] (redacting, wiped on drop) |
//! | Randomness | [`random`] (OS CSPRNG; Unix only) |
//!
//! Every hasher is **incremental** — feed bytes with `update` as they arrive
//! (a download, a large file read) and call `finalize` once, without buffering
//! the whole input in memory. One-shot functions ([`sha256`], [`sha512`],
//! [`hmac_sha256`], …) cover the common case.
//!
//! ```
//! use ferroly::hash::Sha256;
//!
//! let mut h = Sha256::new();
//! h.update(b"hello, ");
//! h.update(b"world");
//! let digest = h.finalize();
//! assert_eq!(digest.to_hex().len(), 64);
//! // One-shot form:
//! assert_eq!(ferroly::hash::sha256(b"abc").to_hex().len(), 64);
//! ```
//!
//! # Hand-rolled, not FIPS-validated
//!
//! All algorithms are implemented in-house in safe Rust (FIPS 180-4 SHA-2,
//! FIPS 180-1 SHA-1, RFC 1321 MD5, RFC 2104 HMAC, RFC 5869 HKDF, RFC 8018
//! PBKDF2, RFC 4648 base64) with no external crypto dependency. They are
//! checked against the published test vectors, but they are **not**
//! FIPS 140 validated, have had no independent audit, use no hardware
//! acceleration, and — apart from the explicit constant-time comparisons —
//! make no side-channel guarantees. If your environment mandates validated
//! cryptography, do not use this module for that purpose.

#![deny(missing_docs)]

use std::fmt;

pub mod base64;
mod block;
mod crc32;
pub mod hex;
mod hmac;
pub mod kdf;
mod md5;
pub mod random;
mod secret;
mod sha512;

pub use crc32::{crc32, Crc32};
pub use hmac::{
    hmac, hmac_sha1, hmac_sha256, hmac_sha384, hmac_sha512, Hmac, HmacMd5, HmacSha1, HmacSha256,
    HmacSha384, HmacSha512,
};
pub use md5::{md5, Md5};
pub use secret::Secret;
pub use sha512::{sha384, sha512, Sha384, Sha512};

/// Compares two byte slices in constant time with respect to their contents.
///
/// Use this — never `==` — to compare a MAC, signature, token or password
/// hash against an untrusted value: `==` returns at the first differing byte,
/// which lets an attacker recover the expected value byte by byte from
/// response timing.
///
/// Slices of different lengths compare unequal immediately, so the *length*
/// of the inputs is not hidden (it rarely is secret; hash both sides first if
/// it is).
///
/// ```
/// use ferroly::hash::{ct_eq, hmac_sha256};
///
/// let expected = hmac_sha256(b"key", b"body");
/// let received: &[u8] = expected.as_bytes(); // e.g. decoded from a header
/// assert!(ct_eq(expected.as_bytes(), received));
/// assert!(!ct_eq(b"abc", b"abd"));
/// assert!(!ct_eq(b"abc", b"abcd"));
/// ```
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    // `black_box` keeps the optimiser from turning the fold into an early exit.
    std::hint::black_box(diff) == 0
}

/// A hash function usable with the generic [`Hmac`] and [`kdf`] constructions.
///
/// Implemented by [`Sha256`], [`Sha384`], [`Sha512`], [`Sha1`] and [`Md5`].
/// The inherent `new` / `update` / `finalize` methods on those types do the
/// same thing without importing this trait.
pub trait HashAlgorithm: Clone {
    /// The internal block size in bytes (64 for SHA-256, 128 for SHA-512).
    const BLOCK_LEN: usize;
    /// The digest size in bytes. Must not exceed [`BLOCK_LEN`](Self::BLOCK_LEN).
    const OUTPUT_LEN: usize;
    /// The digest type, a [`Digest`] of [`OUTPUT_LEN`](Self::OUTPUT_LEN) bytes.
    type Output: AsRef<[u8]>;

    /// Creates a fresh hasher.
    fn new() -> Self;
    /// Absorbs more input.
    fn update(&mut self, data: &[u8]);
    /// Finishes the hash and returns the digest.
    fn finalize(self) -> Self::Output;
}

/// A fixed-size hash output.
///
/// Wraps the raw digest bytes and renders as lowercase hex via
/// [`Display`](std::fmt::Display) / [`to_hex`](Digest::to_hex). Comparison is
/// byte-exact.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Digest<const N: usize>([u8; N]);

impl<const N: usize> Digest<N> {
    /// Wraps raw digest bytes.
    pub const fn from_bytes(bytes: [u8; N]) -> Self {
        Digest(bytes)
    }

    /// The raw digest bytes.
    pub const fn as_bytes(&self) -> &[u8; N] {
        &self.0
    }

    /// Consumes the digest, returning the raw bytes.
    pub const fn into_bytes(self) -> [u8; N] {
        self.0
    }

    /// The digest as a lowercase hex string (`2 * N` characters).
    pub fn to_hex(&self) -> String {
        hex::encode(&self.0)
    }

    /// Parses a digest from its hex form (either case, exactly `2 * N`
    /// characters) — e.g. an expected checksum from a manifest.
    ///
    /// ```
    /// use ferroly::hash::{sha256, Digest};
    ///
    /// let expected: Digest<32> = Digest::from_hex(
    ///     "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    /// ).unwrap();
    /// assert!(sha256(b"abc").ct_eq(&expected));
    /// ```
    pub fn from_hex(text: &str) -> Result<Self, hex::DecodeError> {
        let mut bytes = [0u8; N];
        hex::decode_to_slice(text, &mut bytes)?;
        Ok(Digest(bytes))
    }

    /// Constant-time equality with another digest, resistant to timing attacks
    /// (does not short-circuit on the first differing byte).
    pub fn ct_eq(&self, other: &Digest<N>) -> bool {
        ct_eq(&self.0, &other.0)
    }
}

impl<const N: usize> fmt::Display for Digest<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl<const N: usize> fmt::Debug for Digest<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({})", self.to_hex())
    }
}

impl<const N: usize> AsRef<[u8]> for Digest<N> {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

// ---- SHA-256 -------------------------------------------------------------

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const SHA256_H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

const BLOCK: usize = 64;

/// An incremental SHA-256 hasher (FIPS 180-4).
#[derive(Clone)]
pub struct Sha256 {
    h: [u32; 8],
    buf: [u8; BLOCK],
    buf_len: usize,
    total: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    /// Creates a fresh hasher.
    pub fn new() -> Self {
        Sha256 {
            h: SHA256_H0,
            buf: [0; BLOCK],
            buf_len: 0,
            total: 0,
        }
    }

    /// Absorbs more input. May be called any number of times.
    pub fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let need = BLOCK - self.buf_len;
            let take = need.min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == BLOCK {
                let block = self.buf;
                sha256_compress(&mut self.h, &block);
                self.buf_len = 0;
            }
        }
        while data.len() >= BLOCK {
            let mut block = [0u8; BLOCK];
            block.copy_from_slice(&data[..BLOCK]);
            sha256_compress(&mut self.h, &block);
            data = &data[BLOCK..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    /// Finishes the hash and returns the 32-byte digest.
    pub fn finalize(mut self) -> Digest<32> {
        let bit_len = self.total.wrapping_mul(8);
        let i = self.buf_len;
        self.buf[i] = 0x80;
        if i + 1 > BLOCK - 8 {
            self.buf[i + 1..].fill(0);
            let block = self.buf;
            sha256_compress(&mut self.h, &block);
            self.buf = [0; BLOCK];
        } else {
            self.buf[i + 1..].fill(0);
        }
        self.buf[BLOCK - 8..].copy_from_slice(&bit_len.to_be_bytes());
        let block = self.buf;
        sha256_compress(&mut self.h, &block);

        let mut out = [0u8; 32];
        for (i, word) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        Digest(out)
    }
}

impl HashAlgorithm for Sha256 {
    const BLOCK_LEN: usize = BLOCK;
    const OUTPUT_LEN: usize = 32;
    type Output = Digest<32>;

    fn new() -> Self {
        Sha256::new()
    }
    fn update(&mut self, data: &[u8]) {
        Sha256::update(self, data);
    }
    fn finalize(self) -> Digest<32> {
        Sha256::finalize(self)
    }
}

fn sha256_compress(h: &mut [u32; 8], block: &[u8; BLOCK]) {
    let mut w = [0u32; 64];
    for (i, word) in w.iter_mut().enumerate().take(16) {
        let o = i * 4;
        *word = u32::from_be_bytes([block[o], block[o + 1], block[o + 2], block[o + 3]]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = *h;
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let t1 = hh
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(SHA256_K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        hh = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    for (hv, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
        *hv = hv.wrapping_add(v);
    }
}

/// Computes the SHA-256 digest of `data` in one call.
pub fn sha256(data: &[u8]) -> Digest<32> {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize()
}

// ---- SHA-1 ---------------------------------------------------------------

/// An incremental SHA-1 hasher (FIPS 180-1).
///
/// SHA-1 is cryptographically broken for collision resistance; it is provided
/// only for legacy interop (e.g. the WebSocket handshake accept key). Prefer
/// [`Sha256`] for anything security-sensitive.
#[derive(Clone)]
pub struct Sha1 {
    h: [u32; 5],
    buf: [u8; BLOCK],
    buf_len: usize,
    total: u64,
}

impl Default for Sha1 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha1 {
    /// Creates a fresh hasher.
    pub fn new() -> Self {
        Sha1 {
            h: [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0],
            buf: [0; BLOCK],
            buf_len: 0,
            total: 0,
        }
    }

    /// Absorbs more input.
    pub fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let need = BLOCK - self.buf_len;
            let take = need.min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == BLOCK {
                let block = self.buf;
                sha1_compress(&mut self.h, &block);
                self.buf_len = 0;
            }
        }
        while data.len() >= BLOCK {
            let mut block = [0u8; BLOCK];
            block.copy_from_slice(&data[..BLOCK]);
            sha1_compress(&mut self.h, &block);
            data = &data[BLOCK..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    /// Finishes the hash and returns the 20-byte digest.
    pub fn finalize(mut self) -> Digest<20> {
        let bit_len = self.total.wrapping_mul(8);
        let i = self.buf_len;
        self.buf[i] = 0x80;
        if i + 1 > BLOCK - 8 {
            self.buf[i + 1..].fill(0);
            let block = self.buf;
            sha1_compress(&mut self.h, &block);
            self.buf = [0; BLOCK];
        } else {
            self.buf[i + 1..].fill(0);
        }
        self.buf[BLOCK - 8..].copy_from_slice(&bit_len.to_be_bytes());
        let block = self.buf;
        sha1_compress(&mut self.h, &block);

        let mut out = [0u8; 20];
        for (i, word) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        Digest(out)
    }
}

impl HashAlgorithm for Sha1 {
    const BLOCK_LEN: usize = BLOCK;
    const OUTPUT_LEN: usize = 20;
    type Output = Digest<20>;

    fn new() -> Self {
        Sha1::new()
    }
    fn update(&mut self, data: &[u8]) {
        Sha1::update(self, data);
    }
    fn finalize(self) -> Digest<20> {
        Sha1::finalize(self)
    }
}

fn sha1_compress(h: &mut [u32; 5], block: &[u8; BLOCK]) {
    let mut w = [0u32; 80];
    for (i, word) in w.iter_mut().enumerate().take(16) {
        let o = i * 4;
        *word = u32::from_be_bytes([block[o], block[o + 1], block[o + 2], block[o + 3]]);
    }
    for i in 16..80 {
        w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
    }
    let [mut a, mut b, mut c, mut d, mut e] = *h;
    for (i, &word) in w.iter().enumerate() {
        let (f, k) = match i {
            0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
            20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
            40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
            _ => (b ^ c ^ d, 0xCA62C1D6),
        };
        let t = a
            .rotate_left(5)
            .wrapping_add(f)
            .wrapping_add(e)
            .wrapping_add(k)
            .wrapping_add(word);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = t;
    }
    for (hv, v) in h.iter_mut().zip([a, b, c, d, e]) {
        *hv = hv.wrapping_add(v);
    }
}

/// Computes the SHA-1 digest of `data` in one call.
pub fn sha1(data: &[u8]) -> Digest<20> {
    let mut h = Sha1::new();
    h.update(data);
    h.finalize()
}

#[cfg(test)]
mod tests;

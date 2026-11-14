//! MD5 (RFC 1321) — a legacy, **non-cryptographic** checksum.

use super::block::BlockBuffer;
use super::{Digest, HashAlgorithm};

const MD5_T: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

const MD5_S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

const BLOCK: usize = 64;

/// An incremental MD5 hasher (RFC 1321).
///
/// # Not a cryptographic hash
///
/// MD5 is **broken**: collisions can be produced in seconds and chosen-prefix
/// collisions are practical. It is provided only as a *checksum* for interop
/// with systems that mandate it — `Content-MD5` headers, S3-style ETags,
/// legacy file manifests. Never use it for signatures, password storage,
/// deduplication of untrusted input or any integrity check an attacker could
/// influence; use [`Sha256`](super::Sha256) there.
#[derive(Clone)]
pub struct Md5 {
    h: [u32; 4],
    buf: BlockBuffer<BLOCK>,
    total: u64,
}

impl Default for Md5 {
    fn default() -> Self {
        Self::new()
    }
}

impl Md5 {
    /// Creates a fresh hasher.
    pub fn new() -> Self {
        Md5 {
            h: [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476],
            buf: BlockBuffer::new(),
            total: 0,
        }
    }

    /// Absorbs more input. May be called any number of times.
    pub fn update(&mut self, data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        let h = &mut self.h;
        self.buf.update(data, |block| compress(h, block));
    }

    /// Finishes the hash and returns the 16-byte digest.
    pub fn finalize(mut self) -> Digest<16> {
        // MD5 appends the bit length little-endian.
        let bit_len = self.total.wrapping_mul(8).to_le_bytes();
        let h = &mut self.h;
        self.buf.finish(&bit_len, |block| compress(h, block));
        let mut out = [0u8; 16];
        for (chunk, word) in out.chunks_exact_mut(4).zip(self.h) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        Digest::from_bytes(out)
    }
}

impl HashAlgorithm for Md5 {
    const BLOCK_LEN: usize = BLOCK;
    const OUTPUT_LEN: usize = 16;
    type Output = Digest<16>;

    fn new() -> Self {
        Md5::new()
    }
    fn update(&mut self, data: &[u8]) {
        Md5::update(self, data);
    }
    fn finalize(self) -> Digest<16> {
        Md5::finalize(self)
    }
}

fn compress(h: &mut [u32; 4], block: &[u8; BLOCK]) {
    let mut m = [0u32; 16];
    for (word, bytes) in m.iter_mut().zip(block.chunks_exact(4)) {
        *word = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    }
    let [mut a, mut b, mut c, mut d] = *h;
    for i in 0..64 {
        let (f, g) = match i / 16 {
            0 => ((b & c) | ((!b) & d), i),
            1 => ((d & b) | ((!d) & c), (5 * i + 1) % 16),
            2 => (b ^ c ^ d, (3 * i + 5) % 16),
            _ => (c ^ (b | (!d)), (7 * i) % 16),
        };
        let sum = f.wrapping_add(a).wrapping_add(MD5_T[i]).wrapping_add(m[g]);
        a = d;
        d = c;
        c = b;
        b = b.wrapping_add(sum.rotate_left(MD5_S[i]));
    }
    for (hv, v) in h.iter_mut().zip([a, b, c, d]) {
        *hv = hv.wrapping_add(v);
    }
}

/// Computes the MD5 checksum of `data` in one call.
///
/// MD5 is **not** collision resistant — see [`Md5`] for when it is acceptable.
pub fn md5(data: &[u8]) -> Digest<16> {
    let mut h = Md5::new();
    h.update(data);
    h.finalize()
}

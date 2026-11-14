//! SHA-512 and SHA-384 (FIPS 180-4).

use super::block::BlockBuffer;
use super::{Digest, HashAlgorithm};

const SHA512_K: [u64; 80] = [
    0x428a2f98d728ae22,
    0x7137449123ef65cd,
    0xb5c0fbcfec4d3b2f,
    0xe9b5dba58189dbbc,
    0x3956c25bf348b538,
    0x59f111f1b605d019,
    0x923f82a4af194f9b,
    0xab1c5ed5da6d8118,
    0xd807aa98a3030242,
    0x12835b0145706fbe,
    0x243185be4ee4b28c,
    0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f,
    0x80deb1fe3b1696b1,
    0x9bdc06a725c71235,
    0xc19bf174cf692694,
    0xe49b69c19ef14ad2,
    0xefbe4786384f25e3,
    0x0fc19dc68b8cd5b5,
    0x240ca1cc77ac9c65,
    0x2de92c6f592b0275,
    0x4a7484aa6ea6e483,
    0x5cb0a9dcbd41fbd4,
    0x76f988da831153b5,
    0x983e5152ee66dfab,
    0xa831c66d2db43210,
    0xb00327c898fb213f,
    0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2,
    0xd5a79147930aa725,
    0x06ca6351e003826f,
    0x142929670a0e6e70,
    0x27b70a8546d22ffc,
    0x2e1b21385c26c926,
    0x4d2c6dfc5ac42aed,
    0x53380d139d95b3df,
    0x650a73548baf63de,
    0x766a0abb3c77b2a8,
    0x81c2c92e47edaee6,
    0x92722c851482353b,
    0xa2bfe8a14cf10364,
    0xa81a664bbc423001,
    0xc24b8b70d0f89791,
    0xc76c51a30654be30,
    0xd192e819d6ef5218,
    0xd69906245565a910,
    0xf40e35855771202a,
    0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8,
    0x1e376c085141ab53,
    0x2748774cdf8eeb99,
    0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63,
    0x4ed8aa4ae3418acb,
    0x5b9cca4f7763e373,
    0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc,
    0x78a5636f43172f60,
    0x84c87814a1f0ab72,
    0x8cc702081a6439ec,
    0x90befffa23631e28,
    0xa4506cebde82bde9,
    0xbef9a3f7b2c67915,
    0xc67178f2e372532b,
    0xca273eceea26619c,
    0xd186b8c721c0c207,
    0xeada7dd6cde0eb1e,
    0xf57d4f7fee6ed178,
    0x06f067aa72176fba,
    0x0a637dc5a2c898a6,
    0x113f9804bef90dae,
    0x1b710b35131c471b,
    0x28db77f523047d84,
    0x32caab7b40c72493,
    0x3c9ebe0a15c9bebc,
    0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6,
    0x597f299cfc657e2a,
    0x5fcb6fab3ad6faec,
    0x6c44198c4a475817,
];

const SHA512_H0: [u64; 8] = [
    0x6a09e667f3bcc908,
    0xbb67ae8584caa73b,
    0x3c6ef372fe94f82b,
    0xa54ff53a5f1d36f1,
    0x510e527fade682d1,
    0x9b05688c2b3e6c1f,
    0x1f83d9abfb41bd6b,
    0x5be0cd19137e2179,
];

const SHA384_H0: [u64; 8] = [
    0xcbbb9d5dc1059ed8,
    0x629a292a367cd507,
    0x9159015a3070dd17,
    0x152fecd8f70e5939,
    0x67332667ffc00b31,
    0x8eb44a8768581511,
    0xdb0c2e0d64f98fa7,
    0x47b5481dbefa4fa4,
];

const BLOCK: usize = 128;

/// The shared SHA-512 family engine; the variants differ only in their
/// initial state and how much of the final state they output.
#[derive(Clone)]
struct Engine {
    h: [u64; 8],
    buf: BlockBuffer<BLOCK>,
    total: u128,
}

impl Engine {
    fn new(h: [u64; 8]) -> Self {
        Engine {
            h,
            buf: BlockBuffer::new(),
            total: 0,
        }
    }

    fn update(&mut self, data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u128);
        let h = &mut self.h;
        self.buf.update(data, |block| compress(h, block));
    }

    fn finish(mut self) -> [u64; 8] {
        let bit_len = self.total.wrapping_mul(8).to_be_bytes();
        let h = &mut self.h;
        self.buf.finish(&bit_len, |block| compress(h, block));
        self.h
    }
}

fn compress(h: &mut [u64; 8], block: &[u8; BLOCK]) {
    let mut w = [0u64; 80];
    for (word, bytes) in w.iter_mut().zip(block.chunks_exact(8)) {
        let mut be = [0u8; 8];
        be.copy_from_slice(bytes);
        *word = u64::from_be_bytes(be);
    }
    for i in 16..80 {
        let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
        let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = *h;
    for i in 0..80 {
        let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
        let ch = (e & f) ^ ((!e) & g);
        let t1 = hh
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(SHA512_K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
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

fn state_bytes<const N: usize>(state: [u64; 8]) -> Digest<N> {
    let mut out = [0u8; N];
    for (chunk, word) in out.chunks_exact_mut(8).zip(state) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    Digest::from_bytes(out)
}

/// An incremental SHA-512 hasher (FIPS 180-4).
#[derive(Clone)]
pub struct Sha512(Engine);

impl Default for Sha512 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha512 {
    /// Creates a fresh hasher.
    pub fn new() -> Self {
        Sha512(Engine::new(SHA512_H0))
    }

    /// Absorbs more input. May be called any number of times.
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    /// Finishes the hash and returns the 64-byte digest.
    pub fn finalize(self) -> Digest<64> {
        state_bytes(self.0.finish())
    }
}

impl HashAlgorithm for Sha512 {
    const BLOCK_LEN: usize = BLOCK;
    const OUTPUT_LEN: usize = 64;
    type Output = Digest<64>;

    fn new() -> Self {
        Sha512::new()
    }
    fn update(&mut self, data: &[u8]) {
        Sha512::update(self, data);
    }
    fn finalize(self) -> Digest<64> {
        Sha512::finalize(self)
    }
}

/// An incremental SHA-384 hasher (FIPS 180-4): SHA-512 with a different
/// initial state, truncated to 48 bytes.
#[derive(Clone)]
pub struct Sha384(Engine);

impl Default for Sha384 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha384 {
    /// Creates a fresh hasher.
    pub fn new() -> Self {
        Sha384(Engine::new(SHA384_H0))
    }

    /// Absorbs more input. May be called any number of times.
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    /// Finishes the hash and returns the 48-byte digest.
    pub fn finalize(self) -> Digest<48> {
        state_bytes(self.0.finish())
    }
}

impl HashAlgorithm for Sha384 {
    const BLOCK_LEN: usize = BLOCK;
    const OUTPUT_LEN: usize = 48;
    type Output = Digest<48>;

    fn new() -> Self {
        Sha384::new()
    }
    fn update(&mut self, data: &[u8]) {
        Sha384::update(self, data);
    }
    fn finalize(self) -> Digest<48> {
        Sha384::finalize(self)
    }
}

/// Computes the SHA-512 digest of `data` in one call.
pub fn sha512(data: &[u8]) -> Digest<64> {
    let mut h = Sha512::new();
    h.update(data);
    h.finalize()
}

/// Computes the SHA-384 digest of `data` in one call.
pub fn sha384(data: &[u8]) -> Digest<48> {
    let mut h = Sha384::new();
    h.update(data);
    h.finalize()
}

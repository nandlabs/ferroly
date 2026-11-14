//! CRC-32 (IEEE 802.3) — an error-detecting checksum, not a hash.

/// The reflected IEEE 802.3 polynomial (`0x04C11DB7` bit-reversed).
const POLY: u32 = 0xedb8_8320;

const fn make_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

const TABLE: [u32; 256] = make_table();

/// An incremental CRC-32 checksum: the IEEE 802.3 / ISO-HDLC variant used by
/// zlib, gzip, PNG and zip (`crc32()` in most languages).
///
/// # Not a cryptographic hash
///
/// A CRC detects *accidental* corruption (bit flips, truncation). It offers no
/// protection against deliberate modification: anyone can craft data with a
/// chosen CRC. Use [`Sha256`](super::Sha256) or an HMAC when an adversary is
/// in the picture.
///
/// ```
/// use ferroly::hash::{crc32, Crc32};
///
/// let mut crc = Crc32::new();
/// crc.update(b"1234");
/// crc.update(b"56789");
/// assert_eq!(crc.finalize(), 0xcbf4_3926);
/// assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
/// ```
#[derive(Debug, Clone)]
pub struct Crc32 {
    state: u32,
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32 {
    /// Creates a fresh checksum.
    pub fn new() -> Self {
        Crc32 { state: !0 }
    }

    /// Absorbs more input. May be called any number of times.
    pub fn update(&mut self, data: &[u8]) {
        let mut crc = self.state;
        for &byte in data {
            crc = TABLE[((crc ^ byte as u32) & 0xff) as usize] ^ (crc >> 8);
        }
        self.state = crc;
    }

    /// The checksum of everything absorbed so far. Does not consume the
    /// state, so it can be read at any point of a stream.
    pub fn finalize(&self) -> u32 {
        !self.state
    }
}

/// Computes the CRC-32 (IEEE) checksum of `data` in one call.
///
/// A CRC is **not** a cryptographic hash — see [`Crc32`].
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = Crc32::new();
    crc.update(data);
    crc.finalize()
}

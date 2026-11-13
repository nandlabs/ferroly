//! HMAC (RFC 2104), generic over the hash function.

use super::{ct_eq, HashAlgorithm, Md5, Sha1, Sha256, Sha384, Sha512};

/// An incremental HMAC (RFC 2104) keyed hasher over any [`HashAlgorithm`].
///
/// Use the aliases — [`HmacSha256`], [`HmacSha384`], [`HmacSha512`],
/// [`HmacSha1`] — or name the hash yourself (`Hmac::<Sha512>::new(key)`).
///
/// ```
/// use ferroly::hash::{hmac_sha512, HmacSha512};
///
/// let mut mac = HmacSha512::new(b"key");
/// mac.update(b"mess");
/// mac.update(b"age");
/// let tag = mac.finalize();
/// assert_eq!(tag, hmac_sha512(b"key", b"message"));
///
/// // Verifying a received tag: constant-time, never `==` on raw bytes.
/// let mut check = HmacSha512::new(b"key");
/// check.update(b"message");
/// assert!(check.verify(tag.as_bytes()));
/// ```
///
/// Keys longer than the hash's block size are hashed first; shorter keys are
/// zero-padded. An empty key is accepted (RFC 2104 permits it) but provides no
/// authentication — callers that take keys from configuration should reject
/// empty and short keys themselves. RFC 2104 recommends a key at least as long
/// as the hash output.
///
/// The value holds the keyed inner and outer hash states, so cloning a keyed
/// `Hmac` and feeding the clones is the cheap way to MAC many messages under
/// one key.
#[derive(Clone)]
pub struct Hmac<H: HashAlgorithm> {
    inner: H,
    outer: H,
}

/// HMAC-SHA256.
pub type HmacSha256 = Hmac<Sha256>;
/// HMAC-SHA384.
pub type HmacSha384 = Hmac<Sha384>;
/// HMAC-SHA512.
pub type HmacSha512 = Hmac<Sha512>;
/// HMAC-SHA1 — legacy interop only (TOTP/HOTP, old request-signing schemes).
/// HMAC-SHA1 is not affected by SHA-1's collision attacks, but prefer
/// [`HmacSha256`] for new designs.
pub type HmacSha1 = Hmac<Sha1>;
/// HMAC-MD5 — legacy interop only; do not use in new designs.
pub type HmacMd5 = Hmac<Md5>;

impl<H: HashAlgorithm> Hmac<H> {
    /// Creates a keyed hasher.
    pub fn new(key: &[u8]) -> Self {
        let mut block = vec![0u8; H::BLOCK_LEN];
        if key.len() > H::BLOCK_LEN {
            let mut h = H::new();
            h.update(key);
            let digest = h.finalize();
            let digest = digest.as_ref();
            block[..digest.len()].copy_from_slice(digest);
        } else {
            block[..key.len()].copy_from_slice(key);
        }

        for b in block.iter_mut() {
            *b ^= 0x36;
        }
        let mut inner = H::new();
        inner.update(&block);

        for b in block.iter_mut() {
            *b ^= 0x36 ^ 0x5c;
        }
        let mut outer = H::new();
        outer.update(&block);

        // Best-effort wipe of the padded key block.
        for b in block.iter_mut() {
            *b = 0;
        }
        std::hint::black_box(&block);

        Hmac { inner, outer }
    }

    /// Absorbs more message input.
    pub fn update(&mut self, data: &[u8]) {
        self.inner.update(data);
    }

    /// Finishes the MAC and returns the tag.
    pub fn finalize(self) -> H::Output {
        let inner = self.inner.finalize();
        let mut outer = self.outer;
        outer.update(inner.as_ref());
        outer.finalize()
    }

    /// Finishes the MAC and compares it with `tag` in constant time.
    ///
    /// Returns `false` if `tag` has the wrong length. Truncated tags are not
    /// accepted — compare a prefix yourself with [`ct_eq`] if a protocol
    /// requires truncation.
    pub fn verify(self, tag: &[u8]) -> bool {
        ct_eq(self.finalize().as_ref(), tag)
    }
}

/// Computes an HMAC of `msg` under `key` with the hash `H` in one call.
pub fn hmac<H: HashAlgorithm>(key: &[u8], msg: &[u8]) -> H::Output {
    let mut h = Hmac::<H>::new(key);
    h.update(msg);
    h.finalize()
}

/// Computes HMAC-SHA256 of `msg` under `key` in one call.
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> super::Digest<32> {
    hmac::<Sha256>(key, msg)
}

/// Computes HMAC-SHA384 of `msg` under `key` in one call.
pub fn hmac_sha384(key: &[u8], msg: &[u8]) -> super::Digest<48> {
    hmac::<Sha384>(key, msg)
}

/// Computes HMAC-SHA512 of `msg` under `key` in one call.
pub fn hmac_sha512(key: &[u8], msg: &[u8]) -> super::Digest<64> {
    hmac::<Sha512>(key, msg)
}

/// Computes HMAC-SHA1 of `msg` under `key` in one call (legacy interop).
pub fn hmac_sha1(key: &[u8], msg: &[u8]) -> super::Digest<20> {
    hmac::<Sha1>(key, msg)
}

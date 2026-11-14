//! Randomness for masking keys and the handshake nonce.
//!
//! Every connection gets its **own** generator ([`MaskGen`]); nothing is
//! shared between connections, so traffic to one server reveals nothing about
//! the masks used toward another.
//!
//! - On platforms where [`ferroly::hash::random`] works (Unix), the generator
//!   is seeded with 256 bits from the operating system and the handshake
//!   nonce is 16 separate OS-random bytes.
//! - Elsewhere (or if the OS source fails) the seed is a SHA-256 digest over
//!   mixed process entropy: wall-clock and monotonic time, stack, heap and
//!   static addresses, the process id, a counter and several
//!   `std::collections::hash_map::RandomState` outputs (which the standard
//!   library keys from OS randomness). This is unpredictable to a remote
//!   peer but is **not** a certified cryptographic source.
//!
//! In both cases mask keys are `SHA-256(tag ‖ seed ‖ counter)` output, so
//! observing keys does not reveal the seed or other keys, and the nonce is
//! derived under a different tag from a different seed, so it reveals
//! nothing about the mask stream.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ferroly::hash::{random, Sha256};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn anchor() -> Instant {
    static ANCHOR: OnceLock<Instant> = OnceLock::new();
    *ANCHOR.get_or_init(Instant::now)
}

/// A 32-byte seed from mixed process entropy, for platforms without an OS
/// randomness source. Two calls never return the same seed (the counter
/// alone guarantees that).
pub(crate) fn fallback_seed(domain: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"ferroly-ws-seed");
    h.update(domain);
    h.update(&COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    if let Ok(d) = SystemTime::now().duration_since(UNIX_EPOCH) {
        h.update(&d.as_nanos().to_le_bytes());
    }
    h.update(&anchor().elapsed().as_nanos().to_le_bytes());
    // Address-space layout: a stack slot, a fresh heap allocation and a
    // static.
    let local = 0u8;
    h.update(&(&local as *const u8 as usize).to_le_bytes());
    let heap = Box::new(0u8);
    h.update(&(&*heap as *const u8 as usize).to_le_bytes());
    h.update(&(&COUNTER as *const AtomicU64 as usize).to_le_bytes());
    h.update(&std::process::id().to_le_bytes());
    // `RandomState` is keyed by the standard library from OS randomness, so
    // each of these is a keyed hash an outside observer cannot predict.
    for round in 0..4u64 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(round);
        std::thread::current().id().hash(&mut hasher);
        h.update(&hasher.finish().to_le_bytes());
    }
    h.finalize().into_bytes()
}

fn seed(domain: &[u8]) -> [u8; 32] {
    match random::array::<32>() {
        Ok(seed) => seed,
        Err(_) => fallback_seed(domain),
    }
}

/// A per-connection generator of frame masking keys.
pub(crate) struct MaskGen {
    seed: [u8; 32],
    counter: u64,
    block: [u8; 32],
    used: usize,
}

impl MaskGen {
    /// A generator with a fresh seed.
    pub(crate) fn new() -> Self {
        Self::from_seed(seed(b"mask"))
    }

    pub(crate) fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            seed,
            counter: 0,
            block: [0; 32],
            used: 32,
        }
    }

    /// The next 4-byte masking key.
    pub(crate) fn next_key(&mut self) -> [u8; 4] {
        if self.used + 4 > self.block.len() {
            let mut h = Sha256::new();
            h.update(b"ferroly-ws-mask");
            h.update(&self.seed);
            h.update(&self.counter.to_le_bytes());
            self.block = h.finalize().into_bytes();
            self.counter = self.counter.wrapping_add(1);
            self.used = 0;
        }
        let k = &self.block[self.used..self.used + 4];
        self.used += 4;
        [k[0], k[1], k[2], k[3]]
    }
}

/// The 16-byte `Sec-WebSocket-Key` nonce. Never derived from a [`MaskGen`].
pub(crate) fn nonce() -> [u8; 16] {
    if let Ok(nonce) = random::array::<16>() {
        return nonce;
    }
    nonce_from_seed(fallback_seed(b"nonce"))
}

fn nonce_from_seed(seed: [u8; 32]) -> [u8; 16] {
    let mut h = Sha256::new();
    h.update(b"ferroly-ws-nonce");
    h.update(&seed);
    let digest = h.finalize().into_bytes();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

/// A uniformly distributed value in `[0, 1)`, for reconnect jitter.
pub(crate) fn unit() -> f64 {
    let s = seed(b"jitter");
    let v = u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]);
    (v >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generators_are_independent() {
        let mut a = MaskGen::new();
        let mut b = MaskGen::new();
        let ka: Vec<[u8; 4]> = (0..32).map(|_| a.next_key()).collect();
        let kb: Vec<[u8; 4]> = (0..32).map(|_| b.next_key()).collect();
        assert_ne!(ka, kb);
        // Keys within one stream are not a repeating pattern.
        assert_ne!(ka[0], ka[1]);
        assert_ne!(ka[7], ka[8]); // across a block boundary
    }

    #[test]
    fn fallback_seeds_differ_and_drive_distinct_streams() {
        let s1 = fallback_seed(b"mask");
        let s2 = fallback_seed(b"mask");
        assert_ne!(s1, s2);
        let mut a = MaskGen::from_seed(s1);
        let mut b = MaskGen::from_seed(s2);
        assert_ne!(a.next_key(), b.next_key());
    }

    #[test]
    fn nonce_does_not_expose_the_mask_stream() {
        // Even if a nonce and a mask generator were built from the *same*
        // seed, the nonce bytes are not a prefix of the key stream.
        let seed = fallback_seed(b"x");
        let nonce = nonce_from_seed(seed);
        let mut gen = MaskGen::from_seed(seed);
        let mut stream = Vec::new();
        for _ in 0..4 {
            stream.extend_from_slice(&gen.next_key());
        }
        assert_ne!(&nonce[..], &stream[..]);
        assert_ne!(nonce, super::nonce());
    }

    #[test]
    fn unit_is_in_range() {
        for _ in 0..100 {
            let u = unit();
            assert!((0.0..1.0).contains(&u));
        }
    }
}

//! A tiny non-cryptographic PRNG for WebSocket masking keys and the handshake
//! nonce. Per RFC 6455 these must be unpredictable-ish but need not be secure.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static STATE: AtomicU64 = AtomicU64::new(0);

fn next() -> u64 {
    let mut x = STATE.load(Ordering::Relaxed);
    if x == 0 {
        // Seed from the clock plus the counter's own address for entropy.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9e3779b9);
        x = nanos ^ (&STATE as *const _ as u64) ^ 0xda3e39cb94b95bdb;
    }
    // xorshift64*
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    STATE.store(x, Ordering::Relaxed);
    x.wrapping_mul(0x2545F4914F6CDD1D)
}

/// Fills a byte slice with pseudo-random bytes.
pub(crate) fn fill(buf: &mut [u8]) {
    let mut i = 0;
    while i < buf.len() {
        let bytes = next().to_le_bytes();
        for b in bytes {
            if i >= buf.len() {
                break;
            }
            buf[i] = b;
            i += 1;
        }
    }
}

//! A tiny pseudo-random generator for retry jitter.
//!
//! **Not for security.** The output is predictable to anyone who can observe
//! a few values; it exists only to spread retries from many clients apart in
//! time. Never use it for tokens, keys, nonces or anything an attacker should
//! not be able to guess — use `ferroly::hash::random` for those.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// SplitMix64 increment (the 64-bit golden ratio).
const GAMMA: u64 = 0x9e37_79b9_7f4a_7c15;

/// Shared generator state; `0` means "not seeded yet".
static STATE: AtomicU64 = AtomicU64::new(0);

/// Gathers a seed from the wall clock, the address of a stack variable
/// (address-space layout) and the standard library's per-process hash keys.
fn seed() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let marker = 0u8;
    let address = &marker as *const u8 as usize as u64;
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(nanos);
    hasher.write_u64(address);
    let mixed = hasher.finish() ^ nanos.rotate_left(32) ^ address;
    // Never 0, which is the "unseeded" marker.
    mixed | 1
}

fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The next pseudo-random `u64` (SplitMix64 over a shared atomic counter).
pub(crate) fn next_u64() -> u64 {
    if STATE.load(Ordering::Relaxed) == 0 {
        // Losing this race just means another thread seeded first.
        let _ = STATE.compare_exchange(0, seed(), Ordering::Relaxed, Ordering::Relaxed);
    }
    mix(STATE
        .fetch_add(GAMMA, Ordering::Relaxed)
        .wrapping_add(GAMMA))
}

/// A pseudo-random fraction in `[0, 1)`.
pub(crate) fn fraction() -> f64 {
    (next_u64() >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractions_are_in_range_and_spread_out() {
        let samples: Vec<f64> = (0..10_000).map(|_| fraction()).collect();
        assert!(samples.iter().all(|f| (0.0..1.0).contains(f)));
        let mean = samples.iter().sum::<f64>() / samples.len() as f64;
        assert!((0.45..0.55).contains(&mean), "mean {mean}");
        // Each tenth of the range is hit a plausible number of times.
        for decile in 0..10 {
            let lo = decile as f64 / 10.0;
            let count = samples.iter().filter(|&&f| f >= lo && f < lo + 0.1).count();
            assert!((700..1300).contains(&count), "decile {decile}: {count}");
        }
    }

    #[test]
    fn consecutive_values_differ() {
        let a = next_u64();
        let b = next_u64();
        let c = next_u64();
        assert!(a != b && b != c && a != c);
    }

    #[test]
    fn concurrent_callers_do_not_repeat_each_other() {
        let handles: Vec<_> = (0..4)
            .map(|_| std::thread::spawn(|| (0..1000).map(|_| next_u64()).collect::<Vec<_>>()))
            .collect();
        let mut all = std::collections::HashSet::new();
        for handle in handles {
            for value in handle.join().unwrap() {
                assert!(all.insert(value));
            }
        }
    }
}

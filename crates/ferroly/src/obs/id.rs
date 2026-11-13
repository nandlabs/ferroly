//! Trace and span identifiers, and the per-thread generator that mints them.

use std::cell::Cell;
use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Renders bytes as lowercase hex.
fn write_hex(f: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    let mut buf = [0u8; 32];
    for (i, &b) in bytes.iter().enumerate() {
        buf[i * 2] = HEX[(b >> 4) as usize];
        buf[i * 2 + 1] = HEX[(b & 0x0f) as usize];
    }
    // Hex digits are ASCII, so this cannot fail.
    f.write_str(std::str::from_utf8(&buf[..bytes.len() * 2]).unwrap_or(""))
}

/// Decodes strictly lowercase hex of exactly `out.len() * 2` characters.
fn parse_hex(text: &str, out: &mut [u8]) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != out.len() * 2 {
        return false;
    }
    let nibble = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    for (i, slot) in out.iter_mut().enumerate() {
        match (nibble(bytes[i * 2]), nibble(bytes[i * 2 + 1])) {
            (Some(hi), Some(lo)) => *slot = (hi << 4) | lo,
            _ => return false,
        }
    }
    true
}

/// A 16-byte trace identifier, shared by every span of one trace. Never
/// all-zero. Displays as 32 lowercase hex characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TraceId([u8; 16]);

impl TraceId {
    /// A fresh random trace id.
    pub fn random() -> TraceId {
        loop {
            let mut bytes = [0u8; 16];
            bytes[..8].copy_from_slice(&next_u64().to_be_bytes());
            bytes[8..].copy_from_slice(&next_u64().to_be_bytes());
            if bytes != [0u8; 16] {
                return TraceId(bytes);
            }
        }
    }

    /// Wraps raw bytes; `None` for the all-zero (invalid) id.
    pub fn from_bytes(bytes: [u8; 16]) -> Option<TraceId> {
        (bytes != [0u8; 16]).then_some(TraceId(bytes))
    }

    /// Parses exactly 32 lowercase hex characters; `None` for anything else,
    /// including uppercase digits and the all-zero id.
    pub fn from_hex(text: &str) -> Option<TraceId> {
        let mut bytes = [0u8; 16];
        if parse_hex(text, &mut bytes) {
            TraceId::from_bytes(bytes)
        } else {
            None
        }
    }

    /// The raw bytes.
    pub fn to_bytes(self) -> [u8; 16] {
        self.0
    }
}

impl fmt::Display for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(f, &self.0)
    }
}

impl fmt::Debug for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TraceId({self})")
    }
}

/// An 8-byte span identifier. Never all-zero. Displays as 16 lowercase hex
/// characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SpanId([u8; 8]);

impl SpanId {
    /// A fresh random span id.
    pub fn random() -> SpanId {
        loop {
            let n = next_u64();
            if n != 0 {
                return SpanId(n.to_be_bytes());
            }
        }
    }

    /// Wraps raw bytes; `None` for the all-zero (invalid) id.
    pub fn from_bytes(bytes: [u8; 8]) -> Option<SpanId> {
        (bytes != [0u8; 8]).then_some(SpanId(bytes))
    }

    /// Parses exactly 16 lowercase hex characters; `None` for anything else,
    /// including uppercase digits and the all-zero id.
    pub fn from_hex(text: &str) -> Option<SpanId> {
        let mut bytes = [0u8; 8];
        if parse_hex(text, &mut bytes) {
            SpanId::from_bytes(bytes)
        } else {
            None
        }
    }

    /// The raw bytes.
    pub fn to_bytes(self) -> [u8; 8] {
        self.0
    }
}

impl fmt::Display for SpanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(f, &self.0)
    }
}

impl fmt::Debug for SpanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SpanId({self})")
    }
}

// ---- generator ---------------------------------------------------------------
//
// Each thread owns a xoshiro256** generator. Its 256-bit state is expanded
// with splitmix64 from two SipHash digests keyed by `RandomState` (whose keys
// the standard library draws from the operating system's entropy source) over
// the wall clock, a monotonic clock reading, the process id, the thread id, a
// stack address, a heap address and a process-wide counter. Ids are therefore
// neither sequential nor repeated across threads, processes or restarts. They
// are identifiers, not secrets: do not use them as tokens.

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn seed_state() -> [u64; 4] {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let wall_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let local = 0u8;
    let stack_addr = &local as *const u8 as usize;
    let boxed = Box::new(0u8);
    let heap_addr = &*boxed as *const u8 as usize;

    // Two independently keyed digests give 128 bits of seed material.
    let digest = |salt: u64| {
        let mut h = RandomState::new().build_hasher();
        salt.hash(&mut h);
        wall_nanos.hash(&mut h);
        Instant::now().hash(&mut h);
        std::process::id().hash(&mut h);
        std::thread::current().id().hash(&mut h);
        stack_addr.hash(&mut h);
        heap_addr.hash(&mut h);
        COUNTER.fetch_add(1, Ordering::Relaxed).hash(&mut h);
        h.finish()
    };
    let (mut a, mut b) = (digest(0x5eed), digest(0xfeed));
    let mut state = [
        splitmix64(&mut a),
        splitmix64(&mut b),
        splitmix64(&mut a),
        splitmix64(&mut b),
    ];
    if state == [0; 4] {
        // xoshiro must not start from the all-zero state.
        state[0] = 0x9e37_79b9_7f4a_7c15;
    }
    state
}

thread_local! {
    static RNG: Cell<[u64; 4]> = Cell::new(seed_state());
}

/// The next output of this thread's xoshiro256** generator.
fn next_u64() -> u64 {
    RNG.with(|cell| {
        let mut s = cell.get();
        let result = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        cell.set(s);
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn hex_round_trip_is_strict() {
        let t = TraceId::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").unwrap();
        assert_eq!(t.to_string(), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(TraceId::from_bytes(t.to_bytes()), Some(t));
        let s = SpanId::from_hex("00f067aa0ba902b7").unwrap();
        assert_eq!(s.to_string(), "00f067aa0ba902b7");
        assert_eq!(format!("{s:?}"), "SpanId(00f067aa0ba902b7)");

        for bad in [
            "",
            "4bf92f3577b34da6a3ce929d0e0e473",   // short
            "4bf92f3577b34da6a3ce929d0e0e47360", // long
            "4BF92F3577B34DA6A3CE929D0E0E4736",  // uppercase
            "4bf92f3577b34da6a3ce929d0e0e473g",  // not hex
            "00000000000000000000000000000000",  // all-zero
        ] {
            assert!(TraceId::from_hex(bad).is_none(), "{bad}");
        }
        assert!(SpanId::from_hex("0000000000000000").is_none());
        assert!(SpanId::from_hex("00F067AA0BA902B7").is_none());
        assert!(TraceId::from_bytes([0; 16]).is_none());
        assert!(SpanId::from_bytes([0; 8]).is_none());
    }

    #[test]
    fn ids_are_unique_and_not_sequential() {
        let mut spans = HashSet::new();
        let mut traces = HashSet::new();
        let mut prev = u64::from_be_bytes(SpanId::random().to_bytes());
        let mut small_steps = 0;
        for _ in 0..10_000 {
            let s = SpanId::random();
            assert!(spans.insert(s), "span id repeated");
            assert!(traces.insert(TraceId::random()), "trace id repeated");
            let n = u64::from_be_bytes(s.to_bytes());
            if n.abs_diff(prev) < 1 << 32 {
                small_steps += 1;
            }
            prev = n;
        }
        assert_eq!(small_steps, 0, "ids must not be a counter");
    }

    #[test]
    fn threads_do_not_share_a_sequence() {
        let handles: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(|| (0..1000).map(|_| SpanId::random()).collect::<Vec<_>>()))
            .collect();
        let mut all = HashSet::new();
        for h in handles {
            for id in h.join().unwrap() {
                assert!(all.insert(id), "span id repeated across threads");
            }
        }
        assert_eq!(all.len(), 8000);
    }
}

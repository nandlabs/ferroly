//! Opt-in rate limiting of repeated records.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Above this many tracked keys the limiter forgets windows that have ended
/// (and, if that is not enough, everything) so its memory stays bounded.
const MAX_KEYS: usize = 4096;

struct Window {
    start: Instant,
    emitted: u32,
    suppressed: u64,
}

/// A per-key rate limiter: at most `max` records per `interval` for each key.
///
/// A [`Logger`](super::Logger) configured with
/// [`sample`](super::Logger::sample) keys records by level, target and message
/// text (not by field values), so a hot loop logging the same message is
/// capped while distinct messages are unaffected. The type is public so the
/// same policy can be applied elsewhere, and so it can be driven with explicit
/// instants via [`check_at`](Self::check_at).
///
/// ```
/// use std::time::{Duration, Instant};
/// use ferroly::log::Sampler;
///
/// let s = Sampler::new(2, Duration::from_secs(60));
/// let t0 = Instant::now();
/// assert_eq!(s.check_at("disk full", t0), Some(0));
/// assert_eq!(s.check_at("disk full", t0), Some(0));
/// assert_eq!(s.check_at("disk full", t0), None); // third in the window
/// // The next window reports how many were suppressed in the last one.
/// assert_eq!(s.check_at("disk full", t0 + Duration::from_secs(61)), Some(1));
/// ```
pub struct Sampler {
    max: u32,
    interval: Duration,
    windows: Mutex<HashMap<u64, Window>>,
    suppressed: AtomicU64,
}

impl std::fmt::Debug for Sampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sampler")
            .field("max", &self.max)
            .field("interval", &self.interval)
            .field("suppressed", &self.suppressed())
            .finish()
    }
}

impl Sampler {
    /// Allows at most `max` records per `interval` for each key. A `max` of 0
    /// is treated as 1 (use a level filter to silence a logger entirely).
    pub fn new(max: u32, interval: Duration) -> Self {
        Sampler {
            max: max.max(1),
            interval,
            windows: Mutex::new(HashMap::new()),
            suppressed: AtomicU64::new(0),
        }
    }

    /// Checks `key` against the clock. See [`check_at`](Self::check_at).
    pub fn check(&self, key: &str) -> Option<u64> {
        self.check_at(key, Instant::now())
    }

    /// Decides whether a record with `key` arriving at `now` may be emitted.
    ///
    /// `None` means suppress it. `Some(n)` means emit it; `n` is the number of
    /// records with this key that were suppressed in the key's previous window
    /// (reported once, on the first record of the new window).
    pub fn check_at(&self, key: &str, now: Instant) -> Option<u64> {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        self.check_hashed(hasher.finish(), now)
    }

    /// The same decision for a pre-hashed key.
    pub(super) fn check_hashed(&self, key: u64, now: Instant) -> Option<u64> {
        let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        if windows.len() >= MAX_KEYS && !windows.contains_key(&key) {
            let interval = self.interval;
            windows.retain(|_, w| now.saturating_duration_since(w.start) < interval);
            if windows.len() >= MAX_KEYS {
                windows.clear();
            }
        }
        let window = windows.entry(key).or_insert(Window {
            start: now,
            emitted: 0,
            suppressed: 0,
        });
        let mut carried = 0;
        if now.saturating_duration_since(window.start) >= self.interval {
            carried = window.suppressed;
            *window = Window {
                start: now,
                emitted: 0,
                suppressed: 0,
            };
        }
        if window.emitted < self.max {
            window.emitted += 1;
            Some(carried)
        } else {
            window.suppressed += 1;
            self.suppressed.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    /// The total number of records this sampler has suppressed.
    pub fn suppressed(&self) -> u64 {
        self.suppressed.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_are_per_key_and_roll_over() {
        let s = Sampler::new(2, Duration::from_secs(10));
        let t0 = Instant::now();
        assert_eq!(s.check_at("a", t0), Some(0));
        assert_eq!(s.check_at("a", t0 + Duration::from_secs(1)), Some(0));
        assert_eq!(s.check_at("a", t0 + Duration::from_secs(2)), None);
        assert_eq!(s.check_at("a", t0 + Duration::from_secs(9)), None);
        // Another key has its own budget.
        assert_eq!(s.check_at("b", t0 + Duration::from_secs(9)), Some(0));
        assert_eq!(s.suppressed(), 2);
        // A new window opens and reports the two suppressed records once.
        assert_eq!(s.check_at("a", t0 + Duration::from_secs(10)), Some(2));
        assert_eq!(s.check_at("a", t0 + Duration::from_secs(11)), Some(0));
        assert_eq!(s.check_at("a", t0 + Duration::from_secs(12)), None);
    }

    #[test]
    fn tracked_keys_stay_bounded() {
        let s = Sampler::new(1, Duration::from_secs(1));
        let t0 = Instant::now();
        for i in 0..(MAX_KEYS as u64 * 3) {
            assert!(s.check_hashed(i, t0).is_some());
            assert!(s.windows.lock().unwrap().len() <= MAX_KEYS);
        }
    }
}

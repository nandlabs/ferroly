//! `jti` replay detection: the [`ReplayCache`] hook of
//! [`Validation::replay_cache`](super::Validation::replay_cache) and a
//! bounded in-memory implementation.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use ferroly::hash::Sha256;

/// Remembers the identifiers (`jti`) of accepted tokens so a token can be
/// used only once.
///
/// [`Validation::replay_cache`](super::Validation::replay_cache) calls
/// [`check_and_record`](Self::check_and_record) as the very last validation
/// step, after the signature and every other claim check passed.
///
/// Implement it over a shared store when several processes accept the same
/// tokens — for example Redis `SET key 1 NX EXAT expires_at`, or an insert
/// into a table with a unique index — so a token replayed against another
/// instance is caught too. The check and the insert must be **one atomic
/// operation**: two concurrent calls for the same token must not both see it
/// as new.
pub trait ReplayCache: Send + Sync {
    /// Records the token identified by `issuer` (its `iss` claim, if any) and
    /// `jti` as used until `expires_at` (Unix seconds), and returns `true` if
    /// it had **not** been recorded before — the token may be accepted — or
    /// `false` if it is a replay.
    ///
    /// `now` is the validation clock (Unix seconds; it honours
    /// [`Validation::current_time`](super::Validation::current_time)), so an
    /// implementation can forget entries whose `expires_at` has passed.
    /// [`Validation`](super::Validation) always passes a bounded
    /// `expires_at` (the first second the token is no longer accepted); it
    /// is `u64::MAX` only for an `exp` beyond the `u64` range.
    fn check_and_record(&self, issuer: Option<&str>, jti: &str, expires_at: u64, now: u64) -> bool;
}

/// A bounded, in-process [`ReplayCache`].
///
/// - Each entry is a SHA-256 of the issuer and `jti` (32 bytes, whatever the
///   claim lengths) plus its expiry, so memory is bounded by the capacity.
/// - Entries are dropped once their token has expired (`expires_at <= now`).
/// - When the cache is full of unexpired entries, by default the one that
///   **expires soonest** is evicted to make room (fail open). That token could
///   then be replayed for the rest of its (shortest remaining) lifetime, so
///   size the capacity above the number of tokens you accept within one token
///   lifetime. With [`fail_closed(true)`](Self::fail_closed) a full cache
///   refuses new tokens instead, until entries expire.
/// - [`Validation`](super::Validation) never records a token without a
///   bounded lifetime (it rejects one with neither `exp` nor a checked
///   maximum age), so entries cannot pile up that would never expire.
/// - State is per process and lost on restart; see [`ReplayCache`] for a
///   shared store.
///
/// ```
/// use ferroly::auth::{MemoryReplayCache, ReplayCache};
///
/// let cache = MemoryReplayCache::new(2);
/// assert!(cache.check_and_record(Some("iss"), "a", 100, 10));
/// assert!(!cache.check_and_record(Some("iss"), "a", 100, 10)); // replay
/// assert!(cache.check_and_record(Some("other"), "a", 100, 10)); // another issuer
/// assert!(cache.check_and_record(Some("iss"), "a", 200, 150)); // expired, so new again
/// ```
pub struct MemoryReplayCache {
    capacity: usize,
    fail_closed: bool,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Key digest -> expiry.
    entries: HashMap<[u8; 32], u64>,
    /// `(expiry, key digest)`, soonest first.
    by_expiry: BTreeSet<(u64, [u8; 32])>,
}

impl State {
    fn prune(&mut self, now: u64) {
        while let Some(&(expiry, key)) = self.by_expiry.iter().next() {
            if expiry > now {
                break;
            }
            self.by_expiry.remove(&(expiry, key));
            self.entries.remove(&key);
        }
    }

    /// Evicts the entry that expires soonest; `false` if there was none.
    fn evict_soonest(&mut self) -> bool {
        match self.by_expiry.iter().next() {
            Some(&(expiry, key)) => {
                self.by_expiry.remove(&(expiry, key));
                self.entries.remove(&key);
                true
            }
            None => false,
        }
    }
}

impl MemoryReplayCache {
    /// A cache that remembers at most `capacity` tokens (at least 1).
    pub fn new(capacity: usize) -> Self {
        MemoryReplayCache {
            capacity: capacity.max(1),
            fail_closed: false,
            state: Mutex::new(State::default()),
        }
    }

    /// What happens when the cache is full of unexpired entries and a new
    /// token arrives.
    ///
    /// - `false` (the default, fail open): the entry that expires soonest is
    ///   evicted and the new token is accepted. Availability is kept, but
    ///   whoever can present `capacity` fresh valid tokens can push a captured
    ///   one out and replay it.
    /// - `true` (fail closed): the new token is refused —
    ///   [`check_and_record`](ReplayCache::check_and_record) returns `false`,
    ///   which validation reports as [`JwtError::Replayed`](super::JwtError::Replayed)
    ///   — until an entry expires. No replay gets through, at the cost of
    ///   rejecting valid tokens while the cache is full.
    ///
    /// ```
    /// use ferroly::auth::{MemoryReplayCache, ReplayCache};
    ///
    /// let cache = MemoryReplayCache::new(1).fail_closed(true);
    /// assert!(cache.check_and_record(None, "a", 100, 10));
    /// assert!(!cache.check_and_record(None, "b", 100, 10)); // full: refused
    /// assert!(cache.check_and_record(None, "b", 200, 100)); // "a" expired
    /// ```
    pub fn fail_closed(mut self, enabled: bool) -> Self {
        self.fail_closed = enabled;
        self
    }

    /// The maximum number of remembered tokens.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// The number of tokens currently remembered (expired ones included until
    /// the next call prunes them).
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// Whether no token is remembered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // The state is consistent after every statement, so a panic while it
        // was held leaves nothing half-done.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn key(issuer: Option<&str>, jti: &str) -> [u8; 32] {
        let mut h = Sha256::new();
        // Length-prefixed, so ("ab", "c") and ("a", "bc") differ, and an
        // absent issuer differs from an empty one.
        match issuer {
            Some(iss) => {
                h.update(&[1]);
                h.update(&(iss.len() as u64).to_be_bytes());
                h.update(iss.as_bytes());
            }
            None => h.update(&[0]),
        }
        h.update(jti.as_bytes());
        h.finalize().into_bytes()
    }
}

impl ReplayCache for MemoryReplayCache {
    fn check_and_record(&self, issuer: Option<&str>, jti: &str, expires_at: u64, now: u64) -> bool {
        let key = Self::key(issuer, jti);
        let mut state = self.lock();
        state.prune(now);
        if state.entries.contains_key(&key) {
            return false;
        }
        if expires_at <= now {
            // Already expired: nothing to remember (validation rejects such
            // tokens before asking, but stay correct for direct callers).
            return true;
        }
        if state.entries.len() >= self.capacity && self.fail_closed {
            return false;
        }
        while state.entries.len() >= self.capacity && state.evict_soonest() {}
        state.entries.insert(key, expires_at);
        state.by_expiry.insert((expires_at, key));
        true
    }
}

impl std::fmt::Debug for MemoryReplayCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryReplayCache")
            .field("capacity", &self.capacity)
            .field("fail_closed", &self.fail_closed)
            .field("len", &self.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_accepted_once_per_issuer() {
        let cache = MemoryReplayCache::new(8);
        assert!(cache.check_and_record(Some("a"), "1", 100, 0));
        assert!(!cache.check_and_record(Some("a"), "1", 100, 50));
        assert!(cache.check_and_record(Some("b"), "1", 100, 50));
        assert!(cache.check_and_record(None, "1", 100, 50));
        assert!(!cache.check_and_record(None, "1", 100, 50));
        // Length prefixing keeps the pair unambiguous.
        assert!(cache.check_and_record(Some("ab"), "c", 100, 50));
        assert!(cache.check_and_record(Some("a"), "bc", 100, 50));
        assert!(cache.check_and_record(Some(""), "1", 100, 50));
        assert_eq!(cache.len(), 6);
    }

    #[test]
    fn expired_entries_are_forgotten() {
        let cache = MemoryReplayCache::new(8);
        assert!(cache.check_and_record(None, "x", 100, 0));
        assert!(!cache.check_and_record(None, "x", 100, 99));
        // At `expires_at` the token is no longer acceptable, so the entry goes.
        assert!(cache.check_and_record(None, "y", 300, 100));
        assert_eq!(cache.len(), 1);
        assert!(cache.check_and_record(None, "x", 200, 100));
        // A token that has already expired is not stored.
        assert!(cache.check_and_record(None, "z", 50, 100));
        assert!(cache.check_and_record(None, "z", 50, 100));
    }

    #[test]
    fn a_full_cache_evicts_the_entry_that_expires_soonest() {
        let cache = MemoryReplayCache::new(3);
        assert_eq!(cache.capacity(), 3);
        assert!(cache.check_and_record(None, "late", 900, 0));
        assert!(cache.check_and_record(None, "soon", 100, 0));
        assert!(cache.check_and_record(None, "mid", 500, 0));
        assert!(cache.check_and_record(None, "new", 700, 0));
        assert_eq!(cache.len(), 3);
        // "soon" was evicted; the others are still remembered.
        assert!(!cache.check_and_record(None, "late", 900, 0));
        assert!(!cache.check_and_record(None, "mid", 500, 0));
        assert!(!cache.check_and_record(None, "new", 700, 0));
        assert!(cache.check_and_record(None, "soon", 100, 0));
        assert_eq!(MemoryReplayCache::new(0).capacity(), 1);
    }

    /// SEC-8: a fail-closed cache full of entries that never expire refuses
    /// new tokens instead of evicting the last one it protects.
    #[test]
    fn a_fail_closed_cache_refuses_new_tokens_when_full() {
        // Fail open, the degenerate case: entries that never expire fill the
        // cache, so each finite entry is the soonest to expire and is evicted
        // by the next one, and the first token can be replayed.
        let open = MemoryReplayCache::new(3);
        assert!(open.check_and_record(None, "forever-1", u64::MAX, 0));
        assert!(open.check_and_record(None, "forever-2", u64::MAX, 0));
        assert!(open.check_and_record(None, "first", 500, 0));
        assert!(open.check_and_record(None, "second", 500, 0));
        assert!(open.check_and_record(None, "first", 500, 0), "fail open");

        let closed = MemoryReplayCache::new(3).fail_closed(true);
        assert!(closed.check_and_record(None, "forever-1", u64::MAX, 0));
        assert!(closed.check_and_record(None, "forever-2", u64::MAX, 0));
        assert!(closed.check_and_record(None, "first", 500, 0));
        // Full: the second token is refused, the first stays protected.
        assert!(!closed.check_and_record(None, "second", 500, 0));
        assert!(!closed.check_and_record(None, "first", 500, 0));
        assert_eq!(closed.len(), 3);
        // Once "first" has expired there is room again.
        assert!(closed.check_and_record(None, "second", 900, 500));
        assert!(!closed.check_and_record(None, "second", 900, 600));
    }

    #[test]
    fn concurrent_replays_are_accepted_exactly_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let cache = Arc::new(MemoryReplayCache::new(1024));
        let accepted = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let cache = cache.clone();
                let accepted = accepted.clone();
                std::thread::spawn(move || {
                    for i in 0..100 {
                        if cache.check_and_record(Some("iss"), &i.to_string(), 1000, 0) {
                            accepted.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(accepted.load(Ordering::Relaxed), 100);
    }
}

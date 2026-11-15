//! A small idle-connection pool for the HTTP client, keyed by target host.
//!
//! Keeps kept-alive connections around after a response body is fully drained so
//! the next request to the same `host:port` (and TLS-ness, and proxy) reuses
//! the existing TCP + TLS session instead of paying a fresh handshake.
//!
//! Idle connections expire after a configurable time, are capped per host, and
//! are checked for liveness when taken: one the server has already closed (or
//! that has unexpected bytes waiting) is discarded instead of being reused.

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use tokio::io::{AsyncBufRead, BufReader};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use super::transport::Conn;

/// Identifies which connections are interchangeable.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PoolKey {
    /// Target host (bracket-free for IPv6).
    pub(crate) host: String,
    /// Target port.
    pub(crate) port: u16,
    /// Whether the connection to the target is TLS.
    pub(crate) tls: bool,
    /// The proxy the connection goes through, if any.
    pub(crate) proxy: Option<String>,
}

struct Idle {
    reader: BufReader<Conn>,
    since: Instant,
}

/// A shared pool of idle, reusable connections.
pub(crate) struct Pool {
    idle: Mutex<HashMap<PoolKey, VecDeque<Idle>>>,
    /// Maximum idle connections retained per key; extras are closed.
    max_idle_per_host: usize,
    /// How long a connection may sit idle before it is closed.
    idle_timeout: Option<Duration>,
    /// Maximum connections per key in use at once (being opened or carrying
    /// an exchange); `None` is unlimited.
    max_active_per_host: Option<usize>,
    /// One semaphore per key with connections in use or requests waiting.
    active: Mutex<HashMap<PoolKey, Arc<Semaphore>>>,
}

/// Past this many keys, semaphores nobody holds or waits on are dropped.
const ACTIVE_SWEEP_KEYS: usize = 64;

impl Pool {
    /// Creates an empty shared pool.
    pub(crate) fn new(
        max_idle_per_host: usize,
        idle_timeout: Option<Duration>,
        max_active_per_host: Option<usize>,
    ) -> Arc<Self> {
        Arc::new(Self {
            idle: Mutex::new(HashMap::new()),
            max_idle_per_host,
            idle_timeout,
            max_active_per_host: max_active_per_host.map(|n| n.clamp(1, Semaphore::MAX_PERMITS)),
            active: Mutex::new(HashMap::new()),
        })
    }

    /// Waits for a slot to use a connection to `key` — `None` when the number
    /// of active connections is not limited. The slot is held for as long as
    /// the connection carries the exchange: drop the permit when the
    /// connection goes back to the pool or is closed. Waiters are served in
    /// order; the caller bounds the wait.
    pub(crate) async fn acquire(&self, key: &PoolKey) -> Option<OwnedSemaphorePermit> {
        let max = self.max_active_per_host?;
        let semaphore = {
            let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            if active.len() >= ACTIVE_SWEEP_KEYS {
                // Only the map holds these: no permit is out and nobody waits.
                active.retain(|_, s| Arc::strong_count(s) > 1);
            }
            active
                .entry(key.clone())
                .or_insert_with(|| Arc::new(Semaphore::new(max)))
                .clone()
        };
        semaphore.acquire_owned().await.ok()
    }

    /// The number of keys with an active-connection semaphore.
    #[cfg(test)]
    pub(crate) fn active_keys(&self) -> usize {
        self.active.lock().unwrap().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PoolKey, VecDeque<Idle>>> {
        self.idle.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn expired(&self, since: Instant, now: Instant) -> bool {
        match self.idle_timeout {
            Some(limit) => now.saturating_duration_since(since) >= limit,
            None => false,
        }
    }

    /// Removes and returns the most recently used idle connection for `key`
    /// that has not expired and is still open, if any. Stale ones found on the
    /// way are closed.
    pub(crate) fn take(&self, key: &PoolKey) -> Option<BufReader<Conn>> {
        loop {
            let mut candidate = {
                let mut idle = self.lock();
                let queue = idle.get_mut(key)?;
                let candidate = queue.pop_back();
                if queue.is_empty() {
                    idle.remove(key);
                }
                candidate?
            };
            if self.expired(candidate.since, Instant::now()) {
                continue;
            }
            if still_usable(&mut candidate.reader) {
                return Some(candidate.reader);
            }
        }
    }

    /// Returns a drained, still-open connection to the pool for reuse, closing
    /// the oldest one for the key if the per-host cap is exceeded and evicting
    /// every expired connection.
    pub(crate) fn put(&self, key: PoolKey, reader: BufReader<Conn>) {
        if self.max_idle_per_host == 0 {
            return;
        }
        let now = Instant::now();
        let mut idle = self.lock();
        if self.idle_timeout.is_some() {
            idle.retain(|_, queue| {
                queue.retain(|c| !self.expired(c.since, now));
                !queue.is_empty()
            });
        }
        let queue = idle.entry(key).or_default();
        queue.push_back(Idle { reader, since: now });
        while queue.len() > self.max_idle_per_host {
            queue.pop_front();
        }
    }

    /// The number of idle connections currently held.
    #[cfg(test)]
    pub(crate) fn idle_count(&self) -> usize {
        self.lock().values().map(VecDeque::len).sum()
    }
}

struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

/// Whether an idle connection can carry another request: a read must be
/// *pending*. If it is ready, the peer has closed the connection, the
/// transport failed, or unsolicited bytes are waiting — all reasons to discard
/// it rather than risk sending a request into it.
fn still_usable(reader: &mut BufReader<Conn>) -> bool {
    let waker = Waker::from(Arc::new(NoopWake));
    let mut cx = Context::from_waker(&waker);
    matches!(Pin::new(reader).poll_fill_buf(&mut cx), Poll::Pending)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    fn key() -> PoolKey {
        PoolKey {
            host: "h".into(),
            port: 80,
            tls: false,
            proxy: None,
        }
    }

    /// An idle connection whose peer end is returned so the test controls it.
    fn pair() -> (BufReader<Conn>, tokio::io::DuplexStream) {
        let (a, b) = tokio::io::duplex(64);
        (BufReader::new(Box::new(a) as Conn), b)
    }

    #[tokio::test]
    async fn caps_idle_connections_per_host() {
        let pool = Pool::new(2, None, None);
        let mut peers = Vec::new();
        for _ in 0..5 {
            let (conn, peer) = pair();
            pool.put(key(), conn);
            peers.push(peer);
        }
        assert_eq!(pool.idle_count(), 2);
        assert!(pool.take(&key()).is_some());
        assert!(pool.take(&key()).is_some());
        assert!(pool.take(&key()).is_none());

        let disabled = Pool::new(0, None, None);
        let (conn, _peer) = pair();
        disabled.put(key(), conn);
        assert_eq!(disabled.idle_count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn expires_idle_connections() {
        let pool = Pool::new(8, Some(Duration::from_secs(30)), None);
        let (conn, _peer1) = pair();
        pool.put(key(), conn);
        tokio::time::advance(Duration::from_secs(20)).await;
        let (conn, _peer2) = pair();
        pool.put(key(), conn);
        assert_eq!(pool.idle_count(), 2);
        tokio::time::advance(Duration::from_secs(15)).await;
        // The first has now been idle 35s, the second 15s: putting a third
        // sweeps the expired one.
        let mut other = key();
        other.host = "other".into();
        let (conn, _peer3) = pair();
        pool.put(other, conn);
        assert_eq!(pool.idle_count(), 2);
        assert!(pool.take(&key()).is_some());
        assert!(pool.take(&key()).is_none());

        // An expired connection is also skipped when taken.
        let (conn, _peer4) = pair();
        pool.put(key(), conn);
        tokio::time::advance(Duration::from_secs(31)).await;
        assert!(pool.take(&key()).is_none());
    }

    #[tokio::test]
    async fn discards_closed_or_dirty_connections() {
        let pool = Pool::new(8, None, None);
        // Peer closed while idle.
        let (conn, peer) = pair();
        pool.put(key(), conn);
        drop(peer);
        assert!(pool.take(&key()).is_none());

        // Peer sent unsolicited bytes while idle.
        let (conn, mut peer) = pair();
        pool.put(key(), conn);
        peer.write_all(b"HTTP/1.1 408 Request Timeout\r\n\r\n")
            .await
            .unwrap();
        assert!(pool.take(&key()).is_none());

        // A quiet, open connection is handed back.
        let (conn, _peer) = pair();
        pool.put(key(), conn);
        assert!(pool.take(&key()).is_some());
    }

    #[tokio::test]
    async fn active_connections_are_capped_per_host_and_queued() {
        let unlimited = Pool::new(8, None, None);
        assert!(unlimited.acquire(&key()).await.is_none());

        let pool = Pool::new(8, None, Some(2));
        let a = pool.acquire(&key()).await.expect("a slot");
        let _b = pool.acquire(&key()).await.expect("a slot");
        // Another host has its own slots.
        let mut other = key();
        other.host = "other".into();
        let _c = pool.acquire(&other).await.expect("a slot");
        // A third request to the full host waits until a slot is released.
        let waiting = {
            let pool = pool.clone();
            tokio::spawn(async move { pool.acquire(&key()).await.is_some() })
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        drop(a);
        assert!(waiting.await.unwrap());

        // Semaphores nobody uses are swept once there are many keys.
        drop((_b, _c));
        for i in 0..ACTIVE_SWEEP_KEYS + 5 {
            let mut k = key();
            k.host = format!("h{i}");
            drop(pool.acquire(&k).await);
        }
        assert!(
            pool.active_keys() <= ACTIVE_SWEEP_KEYS + 1,
            "{}",
            pool.active_keys()
        );
    }
}

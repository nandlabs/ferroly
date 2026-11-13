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
}

impl Pool {
    /// Creates an empty shared pool.
    pub(crate) fn new(max_idle_per_host: usize, idle_timeout: Option<Duration>) -> Arc<Self> {
        Arc::new(Self {
            idle: Mutex::new(HashMap::new()),
            max_idle_per_host,
            idle_timeout,
        })
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
        let pool = Pool::new(2, None);
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

        let disabled = Pool::new(0, None);
        let (conn, _peer) = pair();
        disabled.put(key(), conn);
        assert_eq!(disabled.idle_count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn expires_idle_connections() {
        let pool = Pool::new(8, Some(Duration::from_secs(30)));
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
        let pool = Pool::new(8, None);
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
}

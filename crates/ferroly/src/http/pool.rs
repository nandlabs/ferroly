//! A small idle-connection pool for the HTTP client, keyed by target host.
//!
//! Keeps kept-alive connections around after a response body is fully drained so
//! the next request to the same `host:port` (and TLS-ness) reuses the existing
//! TCP + TLS session instead of paying a fresh handshake.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use tokio::io::BufReader;

use super::transport::Conn;

/// Pool key: `(host, port, is_tls)`.
pub(crate) type PoolKey = (String, u16, bool);

/// Maximum idle connections retained per host key; extras are dropped (closed).
const MAX_IDLE_PER_HOST: usize = 32;

/// A shared pool of idle, reusable connections.
pub(crate) struct Pool {
    idle: Mutex<HashMap<PoolKey, VecDeque<BufReader<Conn>>>>,
}

impl Pool {
    /// Creates an empty shared pool.
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            idle: Mutex::new(HashMap::new()),
        })
    }

    /// Removes and returns an idle connection for `key`, if any.
    pub(crate) fn take(&self, key: &PoolKey) -> Option<BufReader<Conn>> {
        let mut idle = self.idle.lock().unwrap();
        idle.get_mut(key).and_then(|q| q.pop_front())
    }

    /// Returns a drained, still-open connection to the pool for reuse (dropping
    /// it if the per-host idle cap is already reached).
    pub(crate) fn put(&self, key: PoolKey, reader: BufReader<Conn>) {
        let mut idle = self.idle.lock().unwrap();
        let q = idle.entry(key).or_default();
        if q.len() < MAX_IDLE_PER_HOST {
            q.push_back(reader);
        }
    }
}

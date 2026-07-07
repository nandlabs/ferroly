//! Async runtime surface — the `tokio` primitives Ferroly already builds on,
//! re-exported so consumers don't add their own `tokio` dependency.
//!
//! Ferroly depends on `tokio` internally; this module exposes a curated slice of
//! it (task spawning, channels, synchronization, time, TCP) under one roof.
//! Enabling the `rt` feature is enough to spawn tasks and use channels without
//! declaring `tokio` yourself.
//!
//! ```no_run
//! use ferroly::rt::{self, sync::mpsc, time::Duration};
//!
//! # async fn demo() {
//! let (tx, mut rx) = mpsc::channel::<u32>(8);
//! let worker = rt::spawn(async move {
//!     tx.send(42).await.ok();
//! });
//! rt::select! {
//!     msg = rx.recv() => { let _ = msg; }
//!     _ = rt::time::sleep(Duration::from_secs(1)) => {}
//! }
//! worker.await.ok();
//! # }
//! ```
//!
//! Need something not re-exported here? Enable `tokio` directly — this module is
//! a convenience surface, not a wall.

#![deny(missing_docs)]

// ---- tasks ---------------------------------------------------------------

#[doc(no_inline)]
pub use tokio::task::{spawn, spawn_blocking, yield_now, JoinError, JoinHandle, JoinSet};

// ---- macros --------------------------------------------------------------

#[doc(no_inline)]
pub use tokio::{join, pin, select, try_join};

// ---- time ----------------------------------------------------------------

/// Time utilities: [`sleep`](time::sleep), [`timeout`](time::timeout),
/// [`interval`](time::interval), and the [`Duration`](time::Duration) /
/// [`Instant`](time::Instant) types.
pub mod time {
    #[doc(no_inline)]
    pub use tokio::time::{
        interval, interval_at, sleep, sleep_until, timeout, timeout_at, Duration, Instant,
        Interval, Sleep, Timeout,
    };
}

// ---- synchronization -----------------------------------------------------

/// Synchronization primitives: channels ([`mpsc`](sync::mpsc),
/// [`oneshot`](sync::oneshot), [`broadcast`](sync::broadcast),
/// [`watch`](sync::watch)) and locks ([`Mutex`](sync::Mutex),
/// [`RwLock`](sync::RwLock), [`Semaphore`](sync::Semaphore),
/// [`Notify`](sync::Notify)).
pub mod sync {
    #[doc(no_inline)]
    pub use tokio::sync::{
        broadcast, mpsc, oneshot, watch, Mutex, MutexGuard, Notify, OwnedSemaphorePermit, RwLock,
        RwLockReadGuard, RwLockWriteGuard, Semaphore, SemaphorePermit,
    };
}

// ---- networking ----------------------------------------------------------

/// Async TCP types the HTTP stack sits on: [`TcpListener`](net::TcpListener) and
/// [`TcpStream`](net::TcpStream).
pub mod net {
    #[doc(no_inline)]
    pub use tokio::net::{TcpListener, TcpStream};
}

// ---- io ------------------------------------------------------------------

/// Async I/O traits and helpers, including the `AsyncReadExt` / `AsyncWriteExt`
/// extension traits.
pub mod io {
    #[doc(no_inline)]
    pub use tokio::io::{
        copy, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
        BufWriter,
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    #[tokio::test]
    async fn spawn_and_channel_roundtrip() {
        let (tx, mut rx) = sync::mpsc::channel::<u32>(4);
        let h = spawn(async move {
            tx.send(7).await.ok();
        });
        assert_eq!(rx.recv().await, Some(7));
        h.await.unwrap();
    }

    #[tokio::test]
    async fn timeout_and_select() {
        let slept = time::timeout(
            Duration::from_millis(5),
            time::sleep(Duration::from_secs(60)),
        )
        .await
        .is_err();
        assert!(slept, "the 60s sleep should have timed out");

        let (_tx, mut rx) = sync::mpsc::channel::<()>(1);
        let hit_default = select! {
            _ = rx.recv() => false,
            _ = time::sleep(Duration::from_millis(1)) => true,
        };
        assert!(hit_default);
    }
}

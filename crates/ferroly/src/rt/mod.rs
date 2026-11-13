//! Async runtime surface — the `tokio` primitives Ferroly already builds on,
//! re-exported so most consumers need no `tokio` dependency of their own, plus
//! a few shutdown helpers of Ferroly's own.
//!
//! Ferroly depends on `tokio` internally; this module exposes a curated slice of
//! it — task spawning, channels, synchronization, time, networking, files,
//! child processes, signals and runtime construction — under one roof, and adds
//! [`CancellationToken`], [`TaskTracker`] and [`shutdown_signal`].
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
//! # What works without a direct `tokio` dependency
//!
//! With only `ferroly = { features = ["rt"] }` in `Cargo.toml`:
//!
//! | Item | Works as-is? |
//! |---|---|
//! | Every function, type and module re-exported here | yes |
//! | `select!`, `join!`, `try_join!`, `pin!`, `task_local!` | yes |
//! | [`async_test!`] and [`async_main!`] | yes |
//! | `#[ferroly::rt::test]` / `#[ferroly::rt::main]` **bare** | **no** — the attribute expands to `::tokio::…` paths |
//! | `#[ferroly::rt::test(crate = "ferroly::rt::__tokio")]` (and the same for `main`) | yes |
//!
//! The two attribute macros are tokio's own, re-exported unchanged; a re-export
//! cannot change the crate path a procedural macro emits. See
//! [Entry points and async tests](#entry-points-and-async-tests) below.
//!
//! # Entry points and async tests
//!
//! The declarative forms need nothing but `ferroly`:
//!
//! ```no_run
//! ferroly::rt::async_main! {
//!     async fn main() {
//!         ferroly::rt::time::sleep(ferroly::rt::time::Duration::from_millis(1)).await;
//!     }
//! }
//! ```
//!
//! ```
//! ferroly::rt::async_test! {
//!     async fn adds() {
//!         assert_eq!(ferroly::rt::spawn(async { 1 + 1 }).await.unwrap(), 2);
//!     }
//! }
//! # fn main() {}
//! ```
//!
//! The attribute forms accept every tokio option (`flavor`, `worker_threads`,
//! `start_paused`, …) but must be told where tokio lives:
//!
//! ```no_run
//! #[ferroly::rt::main(crate = "ferroly::rt::__tokio")]
//! async fn main() {}
//! ```
//!
//! ```
//! #[ferroly::rt::test(crate = "ferroly::rt::__tokio")]
//! async fn it_works() {}
//! # fn main() {}
//! ```
//!
//! Or build the runtime by hand through [`runtime::Builder`]:
//!
//! ```no_run
//! ferroly::rt::runtime::Builder::new_multi_thread()
//!     .enable_all()
//!     .build()
//!     .expect("runtime")
//!     .block_on(async { /* serve */ });
//! ```
//!
//! # Not re-exported
//!
//! `time::pause` / `time::advance` / `time::resume` (the paused test clock)
//! exist only when tokio's `test-util` feature is on, which Ferroly does not
//! enable for normal builds; a test suite that wants them needs its own
//! `tokio` dev-dependency with `test-util`. Anything else missing here is
//! likewise one direct `tokio` dependency away — this module is a convenience
//! surface, not a wall.

#![deny(missing_docs)]

mod cancel;
mod shutdown;
mod tracker;

pub use cancel::{CancellationToken, DropGuard};
pub use shutdown::{shutdown_signal, ShutdownSignal};
pub use tracker::{TaskTracker, TrackerTimeout};

/// The `tokio` crate itself, for the one place a path to it must be spelled
/// out: the `crate = "…"` argument of the [`macro@test`] and [`macro@main`]
/// attributes (`#[ferroly::rt::test(crate = "ferroly::rt::__tokio")]`).
///
/// This path is a supported part of the API for that purpose. Reaching through
/// it for anything else works, but ties your code to whichever tokio version
/// and feature set Ferroly happens to build with — prefer the curated
/// re-exports, or depend on `tokio` directly.
#[doc(hidden)]
pub use tokio as __tokio;

// ---- tasks ---------------------------------------------------------------

#[doc(no_inline)]
pub use tokio::task::{
    block_in_place, spawn, spawn_blocking, spawn_local, yield_now, AbortHandle, JoinError,
    JoinHandle, JoinSet, LocalKey, LocalSet,
};

// ---- macros --------------------------------------------------------------

#[doc(no_inline)]
pub use tokio::{join, pin, select, task_local, try_join};

// ---- time ----------------------------------------------------------------

/// Time utilities: [`sleep`](time::sleep), [`timeout`](time::timeout),
/// [`interval`](time::interval), and the [`Duration`](time::Duration) /
/// [`Instant`](time::Instant) types.
///
/// The paused test clock (`pause` / `advance` / `resume`) is not re-exported:
/// it needs tokio's `test-util` feature, which normal builds do not enable.
pub mod time {
    #[doc(no_inline)]
    pub use tokio::time::{
        error::Elapsed, interval, interval_at, sleep, sleep_until, timeout, timeout_at, Duration,
        Instant, Interval, MissedTickBehavior, Sleep, Timeout,
    };

    /// The paused test clock, available with the `rt-test-util` feature
    /// (enable it under `[dev-dependencies]`): freeze time with [`pause`],
    /// move it with [`advance`], and return to the real clock with [`resume`].
    #[cfg(feature = "rt-test-util")]
    #[doc(no_inline)]
    pub use tokio::time::{advance, pause, resume};
}

// ---- synchronization -----------------------------------------------------

/// Synchronization primitives: channels ([`mpsc`](sync::mpsc),
/// [`oneshot`](sync::oneshot), [`broadcast`](sync::broadcast),
/// [`watch`](sync::watch)), locks ([`Mutex`](sync::Mutex),
/// [`RwLock`](sync::RwLock), [`Semaphore`](sync::Semaphore)) and coordination
/// ([`Notify`](sync::Notify), [`OnceCell`](sync::OnceCell),
/// [`Barrier`](sync::Barrier)).
pub mod sync {
    #[doc(no_inline)]
    pub use tokio::sync::{
        broadcast, mpsc, oneshot, watch, AcquireError, Barrier, BarrierWaitResult, Mutex,
        MutexGuard, Notify, OnceCell, OwnedMutexGuard, OwnedRwLockReadGuard, OwnedRwLockWriteGuard,
        OwnedSemaphorePermit, RwLock, RwLockReadGuard, RwLockWriteGuard, Semaphore,
        SemaphorePermit, SetError, TryAcquireError, TryLockError,
    };
}

// ---- networking ----------------------------------------------------------

/// Async sockets: TCP ([`TcpListener`](net::TcpListener),
/// [`TcpStream`](net::TcpStream), [`TcpSocket`](net::TcpSocket)), UDP
/// ([`UdpSocket`](net::UdpSocket)), name resolution
/// ([`lookup_host`](net::lookup_host)) and — on Unix — Unix-domain sockets.
pub mod net {
    #[doc(no_inline)]
    pub use tokio::net::{lookup_host, TcpListener, TcpSocket, TcpStream, UdpSocket};

    /// Unix-domain sockets (Unix only).
    #[cfg(unix)]
    #[doc(no_inline)]
    pub use tokio::net::{UnixDatagram, UnixListener, UnixStream};
}

// ---- io ------------------------------------------------------------------

/// Async I/O traits and helpers, including the `AsyncReadExt` / `AsyncWriteExt`
/// extension traits, [`split`](io::split) and the in-memory
/// [`duplex`](io::duplex) pipe.
pub mod io {
    #[doc(no_inline)]
    pub use tokio::io::{
        copy, copy_bidirectional, duplex, empty, sink, split, stderr, stdin, stdout, AsyncBufRead,
        AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, AsyncWrite,
        AsyncWriteExt, BufReader, BufWriter, DuplexStream, ReadBuf, ReadHalf, WriteHalf,
    };
}

// ---- files -----------------------------------------------------------------

/// Async file operations, run on the blocking thread pool.
///
/// For path-confined or backend-agnostic file access prefer `ferroly::vfs`
/// (the `vfs` feature); this is the raw surface.
pub mod fs {
    #[doc(no_inline)]
    pub use tokio::fs::{
        canonicalize, copy, create_dir, create_dir_all, hard_link, metadata, read, read_dir,
        read_link, read_to_string, remove_dir, remove_dir_all, remove_file, rename,
        set_permissions, symlink_metadata, try_exists, write, DirEntry, File, OpenOptions, ReadDir,
    };
}

// ---- child processes -------------------------------------------------------

/// Spawn and await child processes without blocking the runtime.
pub mod process {
    #[doc(no_inline)]
    pub use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
}

// ---- signals --------------------------------------------------------------

/// Async signal handling for graceful shutdown.
///
/// On Unix, [`signal`](signal::unix::signal) registers a listener for a
/// [`SignalKind`](signal::unix::SignalKind) (e.g. SIGINT/SIGTERM); on any
/// platform, [`ctrl_c`](signal::ctrl_c) resolves on Ctrl-C. For the common
/// "Ctrl-C or SIGTERM" case use [`shutdown_signal`].
pub mod signal {
    #[doc(no_inline)]
    pub use tokio::signal::ctrl_c;

    /// Unix-specific signal kinds and listeners (SIGINT, SIGTERM, …).
    #[cfg(unix)]
    pub mod unix {
        #[doc(no_inline)]
        pub use tokio::signal::unix::{signal, Signal, SignalKind};
    }
}

// ---- runtime construction ---------------------------------------------------

/// Build and drive an async runtime.
///
/// The rest of `ferroly::rt` re-exports primitives for use *inside* an
/// already-running reactor; this module is for binaries that *own* the reactor —
/// a `main` that builds a multi-thread runtime and `block_on`s a root future —
/// and for code that needs a [`Handle`](runtime::Handle) to a running one.
pub mod runtime {
    #[doc(no_inline)]
    pub use tokio::runtime::{Builder, EnterGuard, Handle, Runtime, TryCurrentError};
}

// ---- entry points and tests ---------------------------------------------------

/// tokio's `#[test]` attribute for async tests, re-exported unchanged.
///
/// **A bare `#[ferroly::rt::test]` only compiles in a crate that also depends
/// on `tokio`**: the attribute expands to `::tokio::…` paths, and a re-export
/// cannot change that. In a crate that depends only on `ferroly`, either point
/// it at Ferroly's copy of tokio with the `crate` argument —
///
/// ```
/// #[ferroly::rt::test(crate = "ferroly::rt::__tokio")]
/// async fn it_works() { /* … */ }
/// # fn main() {}
/// ```
///
/// — or use the declarative [`async_test!`], which needs no argument. (If you
/// renamed the `ferroly` dependency, use your name in the `crate` path.)
#[doc(no_inline)]
pub use tokio::test;

/// tokio's `#[main]` attribute, re-exported unchanged.
///
/// Like [`macro@test`], the bare form only compiles next to a direct `tokio`
/// dependency. Without one, pass the `crate` argument —
///
/// ```no_run
/// #[ferroly::rt::main(crate = "ferroly::rt::__tokio")]
/// async fn main() { /* … */ }
/// ```
///
/// — or use the declarative [`async_main!`].
#[doc(no_inline)]
pub use tokio::main;

/// Builds a current-thread runtime and runs `future` on it. Support code for
/// [`async_test!`]; not part of the public API.
#[doc(hidden)]
pub fn __block_on_current_thread<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build the async test runtime")
        .block_on(future)
}

/// Builds a multi-thread runtime and runs `future` on it. Support code for
/// [`async_main!`]; not part of the public API.
#[doc(hidden)]
pub fn __block_on_multi_thread<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build the async runtime")
        .block_on(future)
}

/// Declares one or more async tests — the dependency-free counterpart of
/// `#[tokio::test]`.
///
/// Each `async fn` becomes a regular `#[test]` that runs its body on a fresh
/// current-thread runtime with I/O and time enabled (tokio's default for
/// tests). Attributes such as `#[ignore]` and `#[should_panic]`, and a return
/// type (e.g. `-> Result<(), E>` for `?`), are carried over. The functions
/// take no arguments.
///
/// It expands to paths inside `ferroly`, so it works in a crate with no
/// `tokio` dependency. For tokio's other test options (multi-thread flavor,
/// `start_paused`, …) use the attribute form described on [`macro@test`].
///
/// ```
/// ferroly::rt::async_test! {
///     async fn channel_roundtrip() {
///         let (tx, mut rx) = ferroly::rt::sync::mpsc::channel(1);
///         tx.send(7u8).await.unwrap();
///         assert_eq!(rx.recv().await, Some(7));
///     }
///
///     #[should_panic(expected = "boom")]
///     async fn panics() {
///         panic!("boom");
///     }
///
///     async fn with_result() -> Result<(), std::num::ParseIntError> {
///         let n: u8 = "7".parse()?;
///         assert_eq!(n, 7);
///         Ok(())
///     }
/// }
/// # fn main() {}
/// ```
#[doc(hidden)]
#[macro_export]
macro_rules! __ferroly_rt_async_test {
    ($(
        $(#[$meta:meta])*
        $vis:vis async fn $name:ident() $(-> $ret:ty)? $body:block
    )+) => {
        $(
            #[::core::prelude::v1::test]
            $(#[$meta])*
            $vis fn $name() $(-> $ret)? {
                $crate::rt::__block_on_current_thread(async move $body)
            }
        )+
    };
}

/// Declares an async `main` — the dependency-free counterpart of
/// `#[tokio::main]`.
///
/// The body runs on a multi-thread runtime with I/O and time enabled (tokio's
/// default for `main`). A return type is carried over, so `main` can return a
/// `Result`.
///
/// It expands to paths inside `ferroly`, so it works in a crate with no
/// `tokio` dependency. For tokio's other options (`flavor`, `worker_threads`,
/// …) use the attribute form described on [`macro@main`], or
/// [`runtime::Builder`] directly.
///
/// ```no_run
/// ferroly::rt::async_main! {
///     async fn main() -> std::io::Result<()> {
///         let listener = ferroly::rt::net::TcpListener::bind("127.0.0.1:0").await?;
///         println!("listening on {}", listener.local_addr()?);
///         Ok(())
///     }
/// }
/// ```
#[doc(hidden)]
#[macro_export]
macro_rules! __ferroly_rt_async_main {
    (
        $(#[$meta:meta])*
        $vis:vis async fn main() $(-> $ret:ty)? $body:block
    ) => {
        $(#[$meta])*
        $vis fn main() $(-> $ret)? {
            $crate::rt::__block_on_multi_thread(async move $body)
        }
    };
}

#[doc(inline)]
pub use __ferroly_rt_async_main as async_main;
#[doc(inline)]
pub use __ferroly_rt_async_test as async_test;

#[cfg(test)]
mod tests {
    use super::{select, spawn, sync, time};
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

    #[tokio::test]
    async fn ctrl_c_future_is_constructible() {
        // We can't deliver Ctrl-C in a test, but the future must exist and be
        // constructible through the re-exported path.
        let _fut = super::signal::ctrl_c();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_signal_listener_registers() {
        use super::signal::unix::{signal, SignalKind};
        // Registration is the part that can fail at the API level; we assert the
        // listener constructs without delivering a signal (dependency-free).
        let _sig = signal(SignalKind::user_defined1()).expect("register SIGUSR1");
    }

    #[test]
    fn runtime_builder_constructs() {
        let rt = super::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async { 1 + 1 });
    }

    // The declarative test macro, exercised through its public path.
    super::async_test! {
        async fn async_test_macro_runs_the_body() {
            let value = spawn(async { 20 + 1 }).await.unwrap();
            assert_eq!(value * 2, 42);
        }

        #[should_panic(expected = "expected failure")]
        async fn async_test_macro_keeps_attributes() {
            time::sleep(Duration::from_millis(1)).await;
            panic!("expected failure");
        }

        async fn async_test_macro_supports_results() -> Result<(), std::num::ParseIntError> {
            let n: u32 = "42".parse()?;
            assert_eq!(n, 42);
            Ok(())
        }
    }

    // The attribute with an explicit crate path — the documented form for a
    // crate without its own tokio dependency.
    #[super::test(crate = "crate::rt::__tokio")]
    async fn attribute_with_crate_path_runs() {
        assert_eq!(spawn(async { 3 }).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn broadened_reexports_are_usable() {
        use super::io::{AsyncReadExt, AsyncWriteExt};

        // io::duplex + io::split
        let (a, mut b) = super::io::duplex(64);
        let (mut read_half, mut write_half) = super::io::split(a);
        write_half.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        b.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        b.write_all(b"pong").await.unwrap();
        read_half.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");

        // sync::OnceCell + Barrier
        static CELL: sync::OnceCell<u32> = sync::OnceCell::const_new();
        assert_eq!(*CELL.get_or_init(|| async { 5 }).await, 5);
        let barrier = std::sync::Arc::new(sync::Barrier::new(2));
        let other = spawn({
            let barrier = barrier.clone();
            async move { barrier.wait().await.is_leader() }
        });
        let leader_here = barrier.wait().await.is_leader();
        assert!(leader_here ^ other.await.unwrap());

        // runtime::Handle + AbortHandle
        let handle = super::runtime::Handle::current();
        let task = handle.spawn(std::future::pending::<()>());
        let abort: super::AbortHandle = task.abort_handle();
        abort.abort();
        assert!(task.await.unwrap_err().is_cancelled());

        // time::MissedTickBehavior
        let mut ticker = time::interval(Duration::from_millis(1));
        ticker.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        ticker.tick().await;

        // net::UdpSocket
        let socket = super::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        socket.send_to(b"hi", addr).await.unwrap();
        let mut datagram = [0u8; 8];
        let (n, _) = socket.recv_from(&mut datagram).await.unwrap();
        assert_eq!(&datagram[..n], b"hi");
    }

    super::task_local! {
        static REQUEST_ID: u32;
    }

    #[tokio::test]
    async fn task_local_and_local_set_work() {
        let seen = REQUEST_ID.scope(9, async { REQUEST_ID.get() }).await;
        assert_eq!(seen, 9);

        let local = super::LocalSet::new();
        let value = local
            .run_until(async {
                // `Rc` is not `Send`: this only compiles with `spawn_local`.
                let rc = std::rc::Rc::new(4);
                super::spawn_local(async move { *rc + 1 }).await.unwrap()
            })
            .await;
        assert_eq!(value, 5);
    }
}
